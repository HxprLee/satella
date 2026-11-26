//! The checks of the outcome of an outgoing transfer.
//!
//! A send is only over when the peer says so. A peer that refuses the
//! transfer, and a peer that never answers, both have to reach the caller as
//! a failure: a send that answers before the peer agreed makes a refused
//! transfer look like a successful one.
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
use crate::hdl::outbound::OutboundRequest;
use crate::hdl::{OutboundPayload, State};
use crate::outcome::{OutcomeRegistry, SendOutcome};
use crate::securegcm::{DeviceToDeviceMessage, GcmMetadata, Type};
use crate::securemessage::{EncScheme, Header, HeaderAndBody, SecureMessage, SigScheme};
use crate::sharing_nearby::{connection_response_frame, ConnectionResponseFrame, Frame, V1Frame};
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

/// Builds the payload transfer that carries the consent frame.
///
/// The peer sends its consent as a byte payload: the frame itself first, and
/// then an empty chunk that marks the end of the payload. The engine only
/// looks at the payload once that last chunk arrived.
fn consent_payload(consent: &[u8]) -> Vec<Vec<u8>> {
    let payload_id = 4242i64;
    let total_size = consent.len() as i64;

    let mut frames = Vec::new();

    for (offset, flags, body) in [(0i64, 0i32, consent.to_vec()), (total_size, 1, vec![])] {
        let transfer = crate::location_nearby_connections::PayloadTransferFrame {
            packet_type: Some(crate::location_nearby_connections::payload_transfer_frame::PacketType::Data.into()),
            payload_chunk: Some(crate::location_nearby_connections::payload_transfer_frame::PayloadChunk {
                offset: Some(offset),
                flags: Some(flags),
                body: Some(body),
                ..Default::default()
            }),
            payload_header: Some(crate::location_nearby_connections::payload_transfer_frame::PayloadHeader {
                id: Some(payload_id),
                r#type: Some(crate::location_nearby_connections::payload_transfer_frame::payload_header::PayloadType::Bytes.into()),
                total_size: Some(total_size),
                is_sensitive: Some(false),
                ..Default::default()
            }),
            ..Default::default()
        };

        let wrapper = crate::location_nearby_connections::OfflineFrame {
            version: Some(crate::location_nearby_connections::offline_frame::Version::V1.into()),
            v1: Some(crate::location_nearby_connections::V1Frame {
                r#type: Some(
                    crate::location_nearby_connections::v1_frame::FrameType::PayloadTransfer.into(),
                ),
                payload_transfer: Some(transfer),
                ..Default::default()
            }),
        };

        frames.push(wrapper.encode_to_vec());
    }

    frames
}

/// Builds the consent frame a peer sends when it refuses a transfer.
fn rejection_frame() -> Vec<u8> {
    let frame = Frame {
        version: Some(crate::sharing_nearby::frame::Version::V1.into()),
        v1: Some(V1Frame {
            r#type: Some(crate::sharing_nearby::v1_frame::FrameType::Response.into()),
            connection_response: Some(ConnectionResponseFrame {
                status: Some(connection_response_frame::Status::Reject.into()),
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

/// A peer that sends the given frames and then holds the socket open.
async fn peer_that_sends(frames: Vec<Vec<u8>>) -> (TcpStream, oneshot::Sender<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let (close_tx, close_rx) = oneshot::channel();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        for frame in frames {
            write_frame(&mut socket, &frame).await;
        }

        // The peer holds the socket until the check is done, so the check
        // sees the frame and not an end of file.
        let _ = close_rx.await;
    });

    let socket = TcpStream::connect(addr).await.unwrap();
    (socket, close_tx)
}

/// Builds an outbound handler that is ready to read a consent frame.
fn outbound_handler(
    socket: TcpStream,
    keys: &SessionKeys,
    sender: broadcast::Sender<crate::channel::ChannelMessage>,
) -> OutboundRequest {
    let mut handler = OutboundRequest::new(
        [0, 0, 0, 0],
        socket,
        "peer".to_owned(),
        sender,
        OutboundPayload::Files(vec![]),
        RemoteDeviceInfo {
            name: "peer".to_owned(),
            device_type: crate::DeviceType::Unknown,
        },
    );

    handler.state.decrypt_key = Some(keys.engine_decrypt.clone());
    handler.state.recv_hmac_key = Some(keys.engine_hmac.clone());
    handler.state.encrypt_key = Some(keys.peer_encrypt.clone());
    handler.state.send_hmac_key = Some(keys.peer_hmac.clone());
    handler.state.encryption_done = true;
    handler.state.state = State::SentIntroduction;

    handler
}

/// A peer that refuses the transfer ends the session as a rejection.
///
/// The refusal used to be recorded as a disconnection, so a caller could not
/// tell a peer that said no from a peer that hung up.
#[tokio::test]
async fn a_refused_transfer_is_a_rejection() {
    let keys = SessionKeys::new();

    // The peer sends the consent as a byte payload: the frame itself and the
    // chunk that marks the end of the payload. The sequence number of the
    // secure message counts from one, so every frame carries its own.
    let frames: Vec<Vec<u8>> = consent_payload(&rejection_frame())
        .into_iter()
        .enumerate()
        .map(|(index, frame)| keys.encrypt(index as i32 + 1, &frame))
        .collect();

    let (socket, close) = peer_that_sends(frames).await;

    let (sender, mut messages) = broadcast::channel(16);
    let mut handler = outbound_handler(socket, &keys, sender.clone());

    // The handler reads one frame per call, so it is driven until it ends.
    let mut result = Ok(());
    for _ in 0..4 {
        result = handler.handle().await;
        if result.is_err() {
            break;
        }
    }
    let _ = close.send(());

    // The frame was understood, so the handler ended on its own terms.
    let error = result.expect_err("the refusal must end the handler");
    assert!(
        error.downcast_ref::<AppError>().is_some(),
        "the refusal must not be an error, got: {error:?}"
    );

    // The session is reported as rejected, not as disconnected: the peer
    // said no, which is not the same as the peer hanging up.
    let mut saw_rejected = false;
    let mut saw_disconnected = false;
    while let Ok(message) = messages.try_recv() {
        if message.direction != ChannelDirection::LibToFront {
            continue;
        }
        if message.state == Some(State::Rejected) {
            saw_rejected = true;
        }
        if message.state == Some(State::Disconnected) {
            saw_disconnected = true;
        }
    }
    assert!(
        saw_rejected,
        "the refusal must be reported as a rejected session"
    );
    assert!(
        !saw_disconnected,
        "a refusal must not be reported as a disconnection"
    );
}

/// A peer that never answers ends the session instead of hanging forever.
///
/// The engine used to wait on the socket without a deadline, so a peer that
/// ignored the request kept the session pending forever and the caller never
/// learned anything.
#[tokio::test]
async fn a_peer_that_never_answers_ends_the_session() {
    let keys = SessionKeys::new();

    // The peer accepts the connection and then says nothing at all.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (close_tx, close_rx) = oneshot::channel();
    tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let _ = close_rx.await;
        drop(socket);
    });

    let socket = TcpStream::connect(addr).await.unwrap();
    let (sender, _messages) = broadcast::channel(16);
    let mut handler = outbound_handler(socket, &keys, sender);

    // The wait is short, so the check does not take half a minute.
    handler.consent_timeout = std::time::Duration::from_millis(200);

    // The wait is bounded by the engine, so the check waits for it rather
    // than for a peer that will never answer.
    let result = tokio::time::timeout(std::time::Duration::from_secs(10), handler.handle())
        .await
        .expect("the engine must not wait for a peer that never answers");

    let _ = close_tx.send(());

    let error = result.expect_err("a peer that never answers must end the handler");
    assert!(
        matches!(error.downcast_ref::<AppError>(), Some(AppError::TimedOut)),
        "the session must end as a timeout, got: {error:?}"
    );
}

/// The receipt of a transfer says what happened to it.
///
/// The receipt is what the caller waits for, so it has to name the outcome
/// the peer produced.
#[tokio::test]
async fn the_receipt_names_the_outcome_of_the_transfer() {
    let outcomes = OutcomeRegistry::new();

    let receipt = outcomes.register("transfer");
    outcomes.complete("transfer", SendOutcome::Rejected);
    assert_eq!(
        receipt.await.unwrap(),
        SendOutcome::Rejected,
        "the receipt must say that the peer refused the transfer"
    );

    let receipt = outcomes.register("transfer");
    outcomes.complete("transfer", SendOutcome::TimedOut);
    assert_eq!(
        receipt.await.unwrap(),
        SendOutcome::TimedOut,
        "the receipt must say that the peer did not answer"
    );

    let receipt = outcomes.register("transfer");
    outcomes.complete("transfer", SendOutcome::Completed);
    assert_eq!(
        receipt.await.unwrap(),
        SendOutcome::Completed,
        "the receipt must say that the transfer was over"
    );
}

/// A transfer nobody waits for is not an error.
///
/// The caller may go away before the transfer ends. The engine still finishes
/// the session, and the receipt it finds is simply dropped.
#[tokio::test]
async fn a_transfer_nobody_waits_for_is_not_an_error() {
    let outcomes = OutcomeRegistry::new();

    let receipt = outcomes.register("transfer");
    drop(receipt);

    // The engine answers a transfer whose receipt was dropped.
    outcomes.complete("transfer", SendOutcome::Completed);
}
