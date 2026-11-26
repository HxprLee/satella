//! HTTP/HTTPS transfer server
//!
//! File transfer service compatible with CatShare.
//!
//! # Features
//!
//! - HTTPS WebSocket for negotiation and status synchronization
//! - HTTPS GET /download for ZIP file downloads
//!
//! # Protocol
//!
//! Uses the custom text protocol `type:id:name?payload`

use log::{debug, error, info, warn};

use crate::transfer::protocol::WsMessage;
use axum::{
    Router,
    extract::{Query, State},
    http::StatusCode,
    response::IntoResponse,
    routing::get,
};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::fs::File;
use tokio::io::AsyncReadExt;
use tokio::net::TcpListener;
use tokio::sync::{Mutex, broadcast};
use tokio_tungstenite::tungstenite::Message;

#[derive(Deserialize)]
pub struct DownloadQuery {
    #[serde(rename = "taskId")]
    pub task_id: String,
}

/// Transfer task
#[derive(Debug, Clone)]
pub struct TransferTask {
    pub task_id: String,
    pub files: Vec<FileEntry>,
    pub sender_id: String,
    pub sender_name: String,
}

#[derive(Debug, Clone)]
pub struct FileEntry {
    pub path: PathBuf,
    pub name: String,
    pub size: u64,
    pub mime_type: String,
}

/// Transfer status
#[derive(Debug, Clone)]
pub enum TransferStatus {
    Pending,
    Accepted,
    Rejected(String),
    Transferring { progress: f64 },
    Completed,
    Failed(String),
}

/// Server state
pub struct TransferServerState {
    pub task: TransferTask,
    pub status_tx: broadcast::Sender<TransferStatus>,
}

/// Transfer server
pub struct TransferServer {
    port: u16,
    state: Arc<Mutex<TransferServerState>>,
}

impl TransferServer {
    pub fn new(task: TransferTask) -> Self {
        let (status_tx, _) = broadcast::channel(16);

        Self {
            port: 0, // use a random port
            state: Arc::new(Mutex::new(TransferServerState { task, status_tx })),
        }
    }

    /// Returns the assigned port
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Subscribes to transfer status updates
    pub fn subscribe_status(&self) -> broadcast::Receiver<TransferStatus> {
        let state = self.state.blocking_lock();
        state.status_tx.subscribe()
    }

    /// Subscribes to transfer status updates asynchronously
    pub async fn subscribe_status_async(&self) -> broadcast::Receiver<TransferStatus> {
        let state = self.state.lock().await;
        state.status_tx.subscribe()
    }

    /// Starts the server (HTTP version, for tests)
    pub async fn start(&mut self) -> anyhow::Result<u16> {
        let state = self.state.clone();

        let app = Router::new()
            .route("/download", get(download_handler))
            .with_state(state);

        let listener = TcpListener::bind("0.0.0.0:0").await?;
        let port = listener.local_addr()?.port();
        self.port = port;

        info!("Transfer server listening on port {}", port);

        tokio::spawn(async move {
            if let Err(e) = axum::serve(listener, app).await {
                error!("Server error: {}", e);
            }
        });

        Ok(port)
    }

    /// Starts the WebSocket + HTTP server
    pub async fn start_with_websocket(&mut self) -> anyhow::Result<u16> {
        let state = self.state.clone();
        let state_for_ws = self.state.clone();

        // HTTP server
        let app = Router::new()
            .route("/download", get(download_handler))
            .with_state(state);

        let http_listener = TcpListener::bind("0.0.0.0:0").await?;
        let port = http_listener.local_addr()?.port();
        self.port = port;

        // Start the HTTP server
        tokio::spawn(async move {
            if let Err(e) = axum::serve(http_listener, app).await {
                error!("HTTP Server error: {}", e);
            }
        });

        // WebSocket server (different path on the same port)
        // Note: production should merge this into a single server
        let ws_listener = TcpListener::bind(format!("0.0.0.0:{}", port + 1)).await?;
        let ws_port = ws_listener.local_addr()?.port();

        tokio::spawn(async move {
            while let Ok((stream, _)) = ws_listener.accept().await {
                let state = state_for_ws.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_websocket_connection(stream, state).await {
                        error!("WebSocket error: {}", e);
                    }
                });
            }
        });

        info!(
            "Transfer server started: HTTP={}, WebSocket={}",
            port, ws_port
        );

        Ok(port)
    }

    /// Starts the transfer server with TLS
    ///
    /// The receiver connects over `wss://` and `https://`, so the sender has
    /// to provide TLS. The certificate is self-signed: the receiver does not
    /// validate it and only validates the content.
    pub async fn start_tls(&mut self) -> anyhow::Result<u16> {
        let state = self.state.clone();

        let app = Router::new()
            .route("/download", get(download_handler))
            .route("/websocket", get(websocket_upgrade))
            .with_state(state);

        let tls = tls_config()?;
        // `axum-server` takes a blocking listener and converts it itself.
        let listener = std::net::TcpListener::bind("0.0.0.0:0")?;
        listener.set_nonblocking(true)?;
        let port = listener.local_addr()?.port();
        self.port = port;

        info!("Transfer server listening on port {} (TLS)", port);

        let server = axum_server::from_tcp_rustls(
            listener,
            axum_server::tls_rustls::RustlsConfig::from_config(Arc::new(tls)),
        );
        tokio::spawn(async move {
            if let Err(e) = server.serve(app.into_make_service()).await {
                error!("TLS transfer server error: {}", e);
            }
        });

        Ok(port)
    }
}

/// Builds a TLS configuration with a fresh self-signed certificate.
///
/// The receiver connects with `danger_accept_invalid_certs`, so the
/// certificate only has to be well formed.
fn tls_config() -> anyhow::Result<rustls::ServerConfig> {
    let _ = rustls::crypto::ring::default_provider().install_default();

    let certified = rcgen::generate_simple_self_signed(vec!["satella.local".to_owned()])?;
    let cert_der = certified.cert.der().clone();
    let key_der = certified.signing_key.serialize_der();
    let certs = vec![rustls::pki_types::CertificateDer::from(cert_der)];
    let key = rustls::pki_types::PrivateKeyDer::Pkcs8(rustls::pki_types::PrivatePkcs8KeyDer::from(
        key_der,
    ));

    Ok(rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)?)
}

/// Upgrades an HTTP request to a WebSocket and runs the protocol on it.
async fn websocket_upgrade(
    ws: axum::extract::ws::WebSocketUpgrade,
    State(state): State<Arc<Mutex<TransferServerState>>>,
) -> axum::response::Response {
    ws.on_upgrade(move |socket| handle_websocket_socket(socket, state))
}

/// Runs the protocol on an upgraded WebSocket.
async fn handle_websocket_socket(
    socket: axum::extract::ws::WebSocket,
    state: Arc<Mutex<TransferServerState>>,
) {
    use axum::extract::ws::Message;
    use futures_util::{SinkExt, StreamExt};

    let (mut write, mut read) = socket.split();

    let mut msg_id: u32 = 0;

    // Send version negotiation
    let ver_msg = WsMessage::version_negotiation(msg_id);
    if write
        .send(Message::Text(ver_msg.to_string()))
        .await
        .is_err()
    {
        return;
    }

    while let Some(Ok(msg)) = read.next().await {
        let text = match msg {
            Message::Text(text) => text.to_string(),
            Message::Close(_) => break,
            _ => continue,
        };

        let Some(ws_msg) = WsMessage::parse(&text) else {
            warn!("Invalid WebSocket message: {}", text);
            continue;
        };

        debug!(
            "WS received: type={}, name={}",
            ws_msg.msg_type, ws_msg.name
        );

        match ws_msg.msg_type.as_str() {
            "ack" => {
                if ws_msg.name == "versionNegotiation" {
                    // Version negotiation complete; send the transfer request
                    msg_id += 1;
                    let task = {
                        let s = state.lock().await;
                        s.task.clone()
                    };

                    let total_size: u64 = task.files.iter().map(|f| f.size).sum();
                    let file_name = task
                        .files
                        .first()
                        .map(|f| f.name.clone())
                        .unwrap_or_default();

                    let send_req = WsMessage::action(
                        msg_id,
                        "sendRequest",
                        Some(serde_json::json!({
                            "taskId": task.task_id,
                            "id": task.task_id,
                            "senderId": task.sender_id,
                            "senderName": task.sender_name,
                            "fileName": file_name,
                            "mimeType": task.files.first().map(|f| &f.mime_type).unwrap_or(&"application/octet-stream".to_string()),
                            "fileCount": task.files.len(),
                            "totalSize": total_size
                        })),
                    );
                    if write
                        .send(Message::Text(send_req.to_string()))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            }
            "action" => {
                // Send the ACK
                let ack = WsMessage::ack(ws_msg.id, &ws_msg.name, None);
                if write.send(Message::Text(ack.to_string())).await.is_err() {
                    break;
                }

                if ws_msg.name == "status"
                    && let Some(payload) = &ws_msg.payload
                {
                    let status_type = payload.get("type").and_then(|v| v.as_i64()).unwrap_or(0);
                    if status_type == 1 {
                        // Transfer complete
                        info!("Transfer completed successfully");
                        let _ = state.lock().await.status_tx.send(TransferStatus::Completed);
                        break;
                    } else if status_type == 3 {
                        // User refused
                        info!("Transfer rejected by receiver");
                        let reason = payload
                            .get("reason")
                            .and_then(|v| v.as_str())
                            .unwrap_or("rejected");
                        let _ = state
                            .lock()
                            .await
                            .status_tx
                            .send(TransferStatus::Rejected(reason.to_string()));
                        break;
                    }
                }
            }
            _ => {}
        }
    }
}

/// Handles a WebSocket connection
async fn handle_websocket_connection(
    stream: tokio::net::TcpStream,
    state: Arc<Mutex<TransferServerState>>,
) -> anyhow::Result<()> {
    let ws_stream = tokio_tungstenite::accept_async(stream).await?;
    let (mut write, mut read) = ws_stream.split();

    let mut msg_id: u32 = 0;

    // Send version negotiation
    let ver_msg = WsMessage::version_negotiation(msg_id);
    write.send(Message::Text(ver_msg.to_string())).await?;

    // Handle messages
    while let Some(msg) = read.next().await {
        let msg = match msg {
            Ok(Message::Text(text)) => text.to_string(),
            Ok(Message::Close(_)) => break,
            Err(e) => {
                error!("WebSocket read error: {}", e);
                break;
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

        match ws_msg.msg_type.as_str() {
            "ack" => {
                if ws_msg.name == "versionNegotiation" {
                    // Version negotiation complete; send the transfer request
                    msg_id += 1;
                    let task = {
                        let s = state.lock().await;
                        s.task.clone()
                    };

                    let total_size: u64 = task.files.iter().map(|f| f.size).sum();
                    let file_name = task
                        .files
                        .first()
                        .map(|f| f.name.clone())
                        .unwrap_or_default();

                    let send_req = WsMessage::action(
                        msg_id,
                        "sendRequest",
                        Some(serde_json::json!({
                            "taskId": task.task_id,
                            "id": task.task_id,
                            "senderId": task.sender_id,
                            "senderName": task.sender_name,
                            "fileName": file_name,
                            "mimeType": task.files.first().map(|f| &f.mime_type).unwrap_or(&"application/octet-stream".to_string()),
                            "fileCount": task.files.len(),
                            "totalSize": total_size
                        })),
                    );
                    write.send(Message::Text(send_req.to_string())).await?;
                }
            }
            "action" => {
                // Send the ACK
                let ack = WsMessage::ack(ws_msg.id, &ws_msg.name, None);
                write.send(Message::Text(ack.to_string())).await?;

                if ws_msg.name == "status"
                    && let Some(payload) = &ws_msg.payload
                {
                    let status_type = payload.get("type").and_then(|v| v.as_i64()).unwrap_or(0);
                    if status_type == 1 {
                        // Transfer complete
                        info!("Transfer completed successfully");
                        let _ = state.lock().await.status_tx.send(TransferStatus::Completed);
                        break;
                    } else if status_type == 3 {
                        // User refused
                        info!("Transfer rejected by receiver");
                        let reason = payload
                            .get("reason")
                            .and_then(|v| v.as_str())
                            .unwrap_or("rejected");
                        let _ = state
                            .lock()
                            .await
                            .status_tx
                            .send(TransferStatus::Rejected(reason.to_string()));
                        break;
                    }
                }
            }
            _ => {}
        }
    }

    Ok(())
}

/// File download handler
async fn download_handler(
    Query(query): Query<DownloadQuery>,
    State(state): State<Arc<Mutex<TransferServerState>>>,
) -> impl IntoResponse {
    let task = {
        let s = state.lock().await;
        if s.task.task_id != query.task_id {
            return (StatusCode::NOT_FOUND, "Task not found").into_response();
        }
        s.task.clone()
    };

    info!("Download request for task_id={}", task.task_id);

    // Create the ZIP file
    match create_zip_response(&task.files).await {
        Ok(data) => {
            let headers = [
                ("Content-Type", "application/zip"),
                ("Content-Disposition", "attachment; filename=\"files.zip\""),
            ];
            (headers, data).into_response()
        }
        Err(e) => {
            error!("Failed to create ZIP: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, "Failed to create ZIP").into_response()
        }
    }
}

async fn create_zip_response(files: &[FileEntry]) -> anyhow::Result<Vec<u8>> {
    let mut buffer = Vec::new();

    {
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buffer));
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);

        for (i, file) in files.iter().enumerate() {
            let entry_name = format!("{}/{}", i, file.name);
            zip.start_file(&entry_name, options)?;

            let mut f = File::open(&file.path).await?;
            let mut contents = Vec::new();
            f.read_to_end(&mut contents).await?;
            zip.write_all(&contents)?;
        }

        zip.finish()?;
    }

    Ok(buffer)
}
