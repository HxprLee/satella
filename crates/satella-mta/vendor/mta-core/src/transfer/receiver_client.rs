//! HTTP/HTTPS receive client
//!
//! File receive client compatible with CatShare.
//!
//! # Features
//!
//! - Connects to the sender's HTTPS WebSocket
//! - Negotiates the version and handles send requests
//! - Downloads the ZIP file and extracts it
//!
//! # Security
//!
//! - Uses HTTPS (certificate validation is skipped because the sender uses a
//!   self-signed certificate)
//! - WebSocket protocol is used for status synchronization

use log::{debug, error, info, warn};

use crate::transfer::protocol::{SendRequest, WsMessage};
use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use std::io::Read;
use std::path::PathBuf;
use tokio::fs::{File, create_dir_all};
use tokio::io::AsyncWriteExt;
use tokio_tungstenite::tungstenite::Message;

/// Receive event callback
#[async_trait]
pub trait ReceiverCallback: Send + Sync {
    /// Called on a send request; returns whether to accept it
    ///
    /// The sender waits for this answer, so an implementation may take its
    /// time, for example by asking the user first.
    async fn on_send_request(&self, request: &SendRequest) -> bool;

    /// Progress update
    fn on_progress(&self, received: u64, total: u64);

    /// Receive complete
    fn on_complete(&self, files: Vec<PathBuf>);

    /// Receive failed
    fn on_error(&self, error: String);
}

/// File receive client
pub struct ReceiverClient {
    host: String,
    port: u16,
    output_dir: PathBuf,
}

impl ReceiverClient {
    pub fn new(host: &str, port: u16, output_dir: PathBuf) -> Self {
        Self {
            host: host.to_string(),
            port,
            output_dir,
        }
    }

    /// Starts receiving
    pub async fn start<C: ReceiverCallback>(&self, callback: &C) -> anyhow::Result<Vec<PathBuf>> {
        // Create the output directory
        create_dir_all(&self.output_dir).await?;

        // Connect the WebSocket (without certificate validation)
        let ws_url = format!("wss://{}:{}/websocket", self.host, self.port);
        info!("Connecting to WebSocket: {}", ws_url);

        // Use a TLS configuration that skips certificate validation
        let connector = native_tls::TlsConnector::builder()
            .danger_accept_invalid_certs(true)
            .build()?;
        let connector = tokio_native_tls::TlsConnector::from(connector);

        // Establish the TCP connection
        let tcp_stream =
            tokio::net::TcpStream::connect(format!("{}:{}", self.host, self.port)).await?;

        // TLS handshake
        let tls_stream = connector.connect(&self.host, tcp_stream).await?;

        // WebSocket handshake
        let (ws_stream, _) = tokio_tungstenite::client_async(&ws_url, tls_stream).await?;

        let (mut write, mut read) = ws_stream.split();

        let mut msg_id: u32 = 0;
        let mut task_id: Option<String> = None;
        let mut total_size: u64 = 0;

        // Message loop
        while let Some(msg) = read.next().await {
            let msg = match msg {
                Ok(Message::Text(text)) => text.to_string(),
                Ok(Message::Close(_)) => break,
                Err(e) => {
                    callback.on_error(format!("WebSocket error: {}", e));
                    return Err(e.into());
                }
                _ => continue,
            };

            let ws_msg = match WsMessage::parse(&msg) {
                Some(m) => m,
                None => {
                    warn!("Invalid WebSocket message: {}", msg);
                    continue;
                }
            };

            debug!(
                "WS received: type={}, name={}",
                ws_msg.msg_type, ws_msg.name
            );

            match ws_msg.name.as_str() {
                "versionNegotiation" => {
                    // Version negotiation
                    let ack = WsMessage::ack(
                        ws_msg.id,
                        "versionNegotiation",
                        Some(serde_json::json!({
                            "version": 1,
                            "threadLimit": 5
                        })),
                    );
                    write.send(Message::Text(ack.to_string())).await?;
                }

                "sendRequest" => {
                    if let Some(payload) = ws_msg.payload {
                        debug!("sendRequest payload: {}", payload);
                        let request: SendRequest = match serde_json::from_value(payload.clone()) {
                            Ok(req) => req,
                            Err(e) => {
                                error!("Failed to parse sendRequest: {}. Payload: {}", e, payload);
                                return Err(anyhow::anyhow!("Protocol error: {}", e));
                            }
                        };
                        total_size = request.total_size;

                        // Get the task ID
                        let req_task_id = request.get_task_id();

                        // Ask the user whether to accept
                        if callback.on_send_request(&request).await {
                            task_id = Some(req_task_id.clone());

                            // Send the ACK
                            let ack = WsMessage::ack(ws_msg.id, "sendRequest", None);
                            write.send(Message::Text(ack.to_string())).await?;

                            // Start downloading
                            break;
                        } else {
                            // Refuse
                            msg_id += 1;
                            let status = WsMessage::status(msg_id, &req_task_id, 3, "user refuse");
                            write.send(Message::Text(status.to_string())).await?;
                            return Err(anyhow::anyhow!("User rejected transfer"));
                        }
                    }
                }

                _ => {
                    // Send the ACK
                    let ack = WsMessage::ack(ws_msg.id, &ws_msg.name, None);
                    write.send(Message::Text(ack.to_string())).await?;
                }
            }
        }

        // Download the file
        let task_id = task_id.ok_or_else(|| anyhow::anyhow!("No task ID received"))?;
        let download_url = format!(
            "https://{}:{}/download?taskId={}",
            self.host, self.port, task_id
        );

        info!("Downloading file from: {}", download_url);

        // Use an HTTP client that skips certificate validation
        let client = reqwest::Client::builder()
            .danger_accept_invalid_certs(true)
            .build()?;

        let response = client.get(&download_url).send().await?;
        let zip_bytes = response.bytes().await?;

        // Extract the ZIP
        let files = self.extract_zip(&zip_bytes, callback, total_size).await?;

        // Send the completion status
        msg_id += 1;
        let status = WsMessage::status(msg_id, &task_id, 1, "ok");
        write.send(Message::Text(status.to_string())).await?;

        callback.on_complete(files.clone());

        Ok(files)
    }

    async fn extract_zip<C: ReceiverCallback>(
        &self,
        data: &[u8],
        callback: &C,
        total_size: u64,
    ) -> anyhow::Result<Vec<PathBuf>> {
        let cursor = std::io::Cursor::new(data);
        let mut archive = zip::ZipArchive::new(cursor)?;

        let mut received: u64 = 0;
        let mut files = Vec::new();

        for i in 0..archive.len() {
            // Read and write (read into memory first to release the zip file
            // handle and avoid holding it across an await)
            let (filename, buffer, is_dir) = {
                let mut file = archive.by_index(i)?;
                let is_dir = file.is_dir();
                let name = file.name().to_string();
                let filename = name.split('/').next_back().unwrap_or(&name).to_string();
                let mut buffer = Vec::new();
                if !is_dir {
                    file.read_to_end(&mut buffer)?;
                }
                (filename, buffer, is_dir)
            };

            if is_dir {
                continue;
            }

            let output_path = self.output_dir.join(filename);
            let mut output_file = File::create(&output_path).await?;
            output_file.write_all(&buffer).await?;

            received += buffer.len() as u64;
            callback.on_progress(received, total_size);

            files.push(output_path);
        }

        Ok(files)
    }
}
