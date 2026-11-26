//! The checks of the session teardown.
//!
//! A peer closes the session with a disconnection frame, and it closes the
//! socket right after it. The engine has to treat both as the end of a
//! transfer that worked, not as a failure: the file was delivered, and a
//! noisy error log buries the failures that matter.
//!
//! These checks drive the real handlers over a real socket pair, with the
//! real encryption, so the frame the peer sends is the frame the engine has
//! to understand.

use hmac::Mac;
use prost::Message;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, oneshot};

use crate::channel::ChannelDirection;
use crate::errors::AppError;
use crate::hdl::inbound::InboundRequest;
use crate::hdl::outbound::OutboundRequest;
use crate::hdl::{OutboundPayload, State};
use crate::location_nearby_connections::{
    offline_frame, v1_frame, DisconnectionFrame, OfflineFrame, V1Frame,
};
use crate::securegcm::{DeviceToDeviceMessage, GcmMetadata, Type};
use crate::securemessage::{EncScheme, Header, HeaderAndBody, SecureMessage, SigScheme};
use crate::utils::RemoteDeviceInfo;

/// The keys of a session that was set up by hand.
///
/// The engine derives them from the key exchange, but a check that wants to
/// push a frame into a live handler does not care how they came about: it
/// needs a pair of keys that agree.
struct SessionKeys {
    /// The key the peer encrypts with.
    peer_encrypt: Vec<u8>,
    /// The key the peer signs with.
    peer_hmac: Vec<u8>,
    /// The key the engine decrypts with.
    engine_decrypt: Vec<u8>,
    /// The key the engine verifies with.
    engine_hmac: Vec<u8>,
}

impl SessionKeys {
    fn new() -> Self {
        Self {
            peer_encrypt: vec![0x11; 32],
            peer_hmac: vec![0x22; 32],
            engine_decrypt: vec![0x11; 32],
            engine_hmac: vec![0x22; 32],
        }
    }

    /// Encrypts a frame the way a peer does, so the engine can read it.
    fn encrypt(&self, sequence_number: i32, payload: &[u8]) -> Vec<u8> {
        let d2d = DeviceToDeviceMessage {
            sequence_number: Some(sequence_number),
            message: Some(payload.to_vec()),
        };

        let msg_data = d2d.encode_to_vec();
        let iv: Vec<u8> = (0..16u8).collect();

        let mut cipher = libaes::Cipher::new_256(self.peer_encrypt[..32].try_into().unwrap());
        cipher.set_auto_padding(true);
        let encrypted = cipher.cbc_encrypt(&iv, &msg_data);

        let hb = HeaderAndBody {
            body: encrypted,
            header: Header {
                encryption_scheme: EncScheme::Aes256Cbc.into(),
                signature_scheme: SigScheme::HmacSha256.into(),
                iv: Some(iv),
                public_metadata: Some(
                    GcmMetadata {
                        r#type: Type::DeviceToDeviceMessage.into(),
                        version: Some(1),
                    }
                    .encode_to_vec(),
                ),
                ..Default::default()
            },
        };

        let mut hmac = hmac::Hmac::<sha2::Sha256>::new_from_slice(&self.peer_hmac).unwrap();
        hmac.update(&hb.encode_to_vec());
        let signature = hmac.finalize().into_bytes().to_vec();

        let smsg = SecureMessage {
            header_and_body: hb.encode_to_vec(),
            signature,
        };

        smsg.encode_to_vec()
    }
}

/// Builds the disconnection frame a peer sends when it closes the session.
fn disconnection_frame() -> Vec<u8> {
    let frame = OfflineFrame {
        version: Some(offline_frame::Version::V1.into()),
        v1: Some(V1Frame {
            r#type: Some(v1_frame::FrameType::Disconnection.into()),
            disconnection: Some(DisconnectionFrame {
                request_safe_to_disconnect: Some(false),
                ack_safe_to_disconnect: Some(false),
            }),
            ..Default::default()
        }),
    };

    frame.encode_to_vec()
}

/// Writes a length-prefixed frame, the way the protocol frames its messages.
async fn write_frame(socket: &mut TcpStream, data: &[u8]) {
    let length = (data.len() as u32).to_be_bytes();
    socket.write_all(&length).await.unwrap();
    socket.write_all(data).await.unwrap();
    socket.flush().await.unwrap();
}

/// A peer that sends one frame and then holds the socket open.
async fn peer_that_sends(frame: Vec<u8>) -> (TcpStream, oneshot::Sender<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let (close_tx, close_rx) = oneshot::channel();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        write_frame(&mut socket, &frame).await;

        // The peer holds the socket until the check is done, so the check
        // sees the frame and not an end of file.
        let _ = close_rx.await;
    });

    let socket = TcpStream::connect(addr).await.unwrap();
    (socket, close_tx)
}

/// A disconnection frame ends the outbound session without an error.
///
/// The engine used to log the frame as unhandled and let the socket end as a
/// failure, so a transfer that had worked was reported as a broken one.
#[tokio::test]
async fn a_disconnection_frame_ends_the_outbound_session_without_an_error() {
    let keys = SessionKeys::new();
    let (socket, close) = peer_that_sends(keys.encrypt(1, &disconnection_frame())).await;

    let (sender, mut messages) = broadcast::channel(16);
    let mut handler = OutboundRequest::new(
        [0, 0, 0, 0],
        socket,
        "peer".to_owned(),
        sender.clone(),
        OutboundPayload::Files(vec![]),
        RemoteDeviceInfo {
            name: "peer".to_owned(),
            device_type: crate::DeviceType::Unknown,
        },
    );

    handler.state.decrypt_key = Some(keys.engine_decrypt.clone());
    handler.state.recv_hmac_key = Some(keys.engine_hmac.clone());
    handler.state.encryption_done = true;
    handler.state.state = State::SendingFiles;

    let result = handler.handle().await;
    let _ = close.send(());

    // The frame was understood, so the handler ended on its own terms
    // instead of failing on the socket.
    let error = result.expect_err("the disconnection frame must end the handler");
    assert!(
        error.downcast_ref::<AppError>().is_some(),
        "the disconnection must not be an error, got: {error:?}"
    );

    // The session is reported as disconnected: the peer hung up and the
    // transfer did not complete on our side.
    let mut saw_disconnected = false;
    while let Ok(message) = messages.try_recv() {
        if message.direction != ChannelDirection::LibToFront {
            continue;
        }
        if message.state == Some(State::Disconnected) {
            saw_disconnected = true;
        }
    }
    assert!(
        saw_disconnected,
        "the disconnection must be reported as a disconnected session"
    );
}

/// A disconnection frame ends the inbound session without an error.
#[tokio::test]
async fn a_disconnection_frame_ends_the_inbound_session_without_an_error() {
    let keys = SessionKeys::new();
    let (socket, close) = peer_that_sends(keys.encrypt(1, &disconnection_frame())).await;

    let (sender, mut messages) = broadcast::channel(16);
    let mut handler = InboundRequest::new(socket, "peer".to_owned(), sender.clone());

    handler.state.decrypt_key = Some(keys.engine_decrypt.clone());
    handler.state.recv_hmac_key = Some(keys.engine_hmac.clone());
    handler.state.encryption_done = true;
    handler.state.state = State::ReceivingFiles;

    let result = handler.handle().await;
    let _ = close.send(());

    let error = result.expect_err("the disconnection frame must end the handler");
    assert!(
        error.downcast_ref::<AppError>().is_some(),
        "the disconnection must not be an error, got: {error:?}"
    );

    let mut saw_disconnected = false;
    while let Ok(message) = messages.try_recv() {
        if message.direction != ChannelDirection::LibToFront {
            continue;
        }
        if message.state == Some(State::Disconnected) {
            saw_disconnected = true;
        }
    }
    assert!(
        saw_disconnected,
        "the disconnection must be reported as a disconnected session"
    );
}

/// A transfer that already finished stays finished when the peer hangs up.
///
/// The peer sends the disconnection right after the last byte, so the frame
/// arrives after the transfer is over. Downgrading it to a disconnection
/// would throw away a transfer that worked.
#[tokio::test]
async fn a_disconnection_after_a_finished_transfer_keeps_it_finished() {
    let keys = SessionKeys::new();
    let (socket, close) = peer_that_sends(keys.encrypt(1, &disconnection_frame())).await;

    let (sender, mut messages) = broadcast::channel(16);
    let mut handler = OutboundRequest::new(
        [0, 0, 0, 0],
        socket,
        "peer".to_owned(),
        sender.clone(),
        OutboundPayload::Files(vec![]),
        RemoteDeviceInfo {
            name: "peer".to_owned(),
            device_type: crate::DeviceType::Unknown,
        },
    );

    handler.state.decrypt_key = Some(keys.engine_decrypt.clone());
    handler.state.recv_hmac_key = Some(keys.engine_hmac.clone());
    handler.state.encryption_done = true;
    handler.state.state = State::Finished;

    let result = handler.handle().await;
    let _ = close.send(());

    assert!(
        result.is_err(),
        "the disconnection frame must end the handler"
    );

    let mut saw_disconnected = false;
    while let Ok(message) = messages.try_recv() {
        if message.direction != ChannelDirection::LibToFront {
            continue;
        }
        if message.state == Some(State::Disconnected) {
            saw_disconnected = true;
        }
    }
    assert!(
        !saw_disconnected,
        "a finished transfer must not be downgraded to a disconnection"
    );
}

/// The state of a session that is over is the state of a session that worked.
///
/// The manager uses this to decide whether a socket that ended is worth
/// reporting as an error.
#[test]
fn a_session_that_is_over_is_not_an_error() {
    use crate::manager::is_terminal;

    assert!(is_terminal(&State::Finished));
    assert!(is_terminal(&State::Cancelled));
    assert!(is_terminal(&State::Rejected));
    assert!(is_terminal(&State::Disconnected));

    assert!(!is_terminal(&State::Initial));
    assert!(!is_terminal(&State::SendingFiles));
    assert!(!is_terminal(&State::ReceivingFiles));
    assert!(!is_terminal(&State::WaitingForUserConsent));
}
