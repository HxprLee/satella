//! Cattysend Core Library
//!
//! Core implementation library for the Mutual Transfer Alliance protocol, fully compatible with CatShare (Android)
//!
//! # Modules
//!
//! - **ble**: BLE scanning, advertising, GATT client/server
//! - **crypto**: ECDH key exchange and AES-CTR encryption
//! - **wifi**: WiFi P2P hotspot creation and connection
//! - **transfer**: HTTP/WebSocket file transfer
//!
//! # Usage Example
//!
//! ## Sending Files
//!
//! ```ignore
//! use cattysend_core::{BleScanner, BleClient, WiFiP2pSender, TransferServer};
//!
//! // 1. Scan for receiver devices
//! let scanner = BleScanner::new().await?;
//! let devices = scanner.scan(Duration::from_secs(5)).await?;
//!
//! // 2. Create a WiFi P2P hotspot and start the transfer server
//! let sender = WiFiP2pSender::new("wlan0");
//! let p2p_info = sender.create_group(8443).await?;
//!
//! // 3. Connect to the receiver and send the P2P info
//! let ble_client = BleClient::new().await?;
//! ble_client.connect_and_handshake(&device.address, &p2p_info, "sender_id").await?;
//!
//! // 4. Wait for the receiver to connect and transfer files
//! ```
//!
//! ## Receiving Files
//!
//! ```ignore
//! use cattysend_core::{GattServer, WiFiP2pReceiver, ReceiverClient};
//!
//! // 1. Start the GATT Server and wait for connections
//! let server = GattServer::new("02:00:00:00:00:00", "MyDevice")?;
//! let handle = server.start().await?;
//!
//! // 2. Wait for the P2P info
//! let p2p_event = p2p_rx.recv().await?;
//!
//! // 3. Connect to the sender's hotspot
//! let receiver = WiFiP2pReceiver::new("wlan0");
//! let ip = receiver.connect(&p2p_event.p2p_info).await?;
//!
//! // 4. Receive files
//! let client = ReceiverClient::new(&host_ip, p2p_info.port, output_dir);
//! client.start(&callback).await?;
//! ```

pub mod ble;
pub mod config;
pub mod crypto;
pub mod logging;
pub mod transfer;
pub mod wifi;
pub mod workflow;

// Config re-exports
pub use config::{AppSettings, BrandId};

// Logging re-exports
pub use logging::{LogEntry, LogLevel};

// BLE re-exports
pub use ble::{
    ADV_SERVICE_UUID, BleClient, BleScanner, ChannelScanCallback, DeviceInfo, DiscoveredDevice,
    GattServer, GattServerHandle, MAIN_SERVICE_UUID, P2P_CHAR_UUID, SERVICE_UUID, STATUS_CHAR_UUID,
    ScanCallback,
};

// Crypto re-exports
pub use crypto::{BleSecurity, BleSecurityPersistent, SessionCipher};

// WiFi re-exports
pub use wifi::{P2pConfig, P2pInfo, WiFiP2pReceiver, WiFiP2pSender};

// Transfer re-exports
pub use transfer::{
    FileEntry, ReceiverCallback, ReceiverClient, SendRequest, TransferServer, TransferTask,
    WsMessage,
};

// Workflow re-exports
pub use workflow::{
    ReceiveEvent, ReceiveOptions, ReceiveProgressCallback, ReceiveRequest, Receiver, SendEvent,
    SendOptions, SendProgressCallback, Sender, SimpleReceiveCallback, SimpleSendCallback,
};
