//! Workflow module
//!
//! Provides a high-level API that wraps the complete send/receive flow

pub mod receiver;
pub mod sender;

pub use receiver::{
    ReceiveEvent, ReceiveOptions, ReceiveProgressCallback, ReceiveRequest, Receiver,
    SimpleReceiveCallback,
};
pub use sender::{SendEvent, SendOptions, SendProgressCallback, Sender, SimpleSendCallback};
