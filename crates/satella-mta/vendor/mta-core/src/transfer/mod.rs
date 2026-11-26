//! File transfer module
//!
//! Includes:
//! - WebSocket protocol implementation (CatShare compatible)
//! - HTTP/HTTPS server (sender)
//! - HTTP/HTTPS client (receiver)

pub mod http_server;
pub mod protocol;
pub mod receiver_client;
pub mod sender_server;
pub mod websocket_handler;

pub use protocol::{SendRequest, WsMessage};
pub use receiver_client::{ReceiverCallback, ReceiverClient};
pub use sender_server::{FileEntry, TransferServer, TransferStatus, TransferTask};

use serde::{Deserialize, Serialize};

/// File information (used for transfer negotiation)
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct FileInfo {
    pub name: String,
    pub size: u64,
    pub modified_time: u64,
    pub mime_type: Option<String>,
}
