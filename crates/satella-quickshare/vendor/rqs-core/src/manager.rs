use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::broadcast::Sender;
use tokio::sync::mpsc::Receiver;
use tokio_util::sync::CancellationToken;
use ts_rs::TS;

use crate::channel::{ChannelDirection, ChannelMessage};
use crate::errors::AppError;
use crate::hdl::{InboundRequest, OutboundPayload, OutboundRequest, State};
use crate::outcome::{OutcomeRegistry, SendOutcome};
use crate::utils::RemoteDeviceInfo;

const INNER_NAME: &str = "TcpServer";

/// Whether a session state means the session is over.
///
/// A peer that hangs up after a finished or a cancelled transfer did what it
/// was supposed to do. The socket ending there is the normal end of a
/// transfer, and reporting it as a failure would bury a real error in noise.
pub(crate) fn is_terminal(state: &State) -> bool {
    matches!(
        state,
        State::Finished | State::Cancelled | State::Rejected | State::Disconnected
    )
}

#[derive(Debug, Deserialize, Serialize, TS)]
#[ts(export)]
pub struct SendInfo {
    pub id: String,
    pub name: String,
    pub addr: String,
    pub ob: OutboundPayload,
}

pub struct TcpServer {
    endpoint_id: [u8; 4],
    tcp_listener: TcpListener,
    sender: Sender<ChannelMessage>,
    connect_receiver: Receiver<SendInfo>,
    /// The receipts of the outgoing transfers.
    outcomes: Arc<OutcomeRegistry>,
    /// How long the engine waits for a peer to answer the handshake and the
    /// consent.
    consent_timeout: Duration,
}

impl TcpServer {
    pub fn new(
        endpoint_id: [u8; 4],
        tcp_listener: TcpListener,
        sender: Sender<ChannelMessage>,
        connect_receiver: Receiver<SendInfo>,
        outcomes: Arc<OutcomeRegistry>,
        consent_timeout: Duration,
    ) -> Result<Self, anyhow::Error> {
        Ok(Self {
            endpoint_id,
            tcp_listener,
            sender,
            connect_receiver,
            outcomes,
            consent_timeout,
        })
    }

    pub async fn run(&mut self, ctk: CancellationToken) -> Result<(), anyhow::Error> {
        info!("{INNER_NAME}: service starting");

        loop {
            let cctk = ctk.clone();

            tokio::select! {
                _ = ctk.cancelled() => {
                    info!("{INNER_NAME}: tracker cancelled, breaking");
                    break;
                }
                Some(i) = self.connect_receiver.recv() => {
                    info!("{INNER_NAME}: connect_receiver: got {:?}", i);
                    if let Err(e) = self.connect(cctk, i).await {
                        error!("{INNER_NAME}: error sending: {}", e.to_string());
                    }
                }
                r = self.tcp_listener.accept() => {
                    match r {
                        Ok((socket, remote_addr)) => {
                            info!("{INNER_NAME}: new client: {remote_addr}");
                            let esender = self.sender.clone();
                            let csender = self.sender.clone();

                            tokio::spawn(async move {
                                let mut ir = InboundRequest::new(socket, remote_addr.to_string(), csender);

                                loop {
                                    match ir.handle().await {
                                        Ok(_) => {},
                                        Err(e) => match e.downcast_ref() {
                                            Some(AppError::NotAnError) => break,
                                            Some(AppError::TimedOut) => {
                                                // The peer went quiet while the
                                                // consent was outstanding.
                                                error!("{INNER_NAME}: the peer did not answer in time ({:?})", ir.state.state);
                                                break;
                                            }
                                            None => {
                                                if ir.state.state == State::Initial {
                                                    break;
                                                }

                                                if ir.state.state != State::Finished {
                                                    let _ = esender.send(ChannelMessage {
                                                        id: remote_addr.to_string(),
                                                        direction: ChannelDirection::LibToFront,
                                                        state: Some(State::Disconnected),
                                                        ..Default::default()
                                                    });
                                                }

                                                if is_terminal(&ir.state.state) {
                                                    // The session was over and the peer
                                                    // hung up. The socket ending here
                                                    // is the expected end of a
                                                    // transfer, not a failure.
                                                    debug!("{INNER_NAME}: the peer closed the connection ({:?})", ir.state.state);
                                                } else {
                                                    error!("{INNER_NAME}: error while handling client: {e} ({:?})", ir.state.state);
                                                }
                                                break;
                                            }
                                        },
                                    }
                                }
                            });
                        },
                        Err(err) => {
                            error!("{INNER_NAME}: error accepting: {}", err);
                            break;
                        }
                    }
                }
            }
        }

        Ok(())
    }

    /// To be called inside a separate task if we want to handle concurrency
    pub async fn connect(&self, ctk: CancellationToken, si: SendInfo) -> Result<(), anyhow::Error> {
        debug!("{INNER_NAME}: Connecting to: {}", si.addr);

        // The id is the key of the receipt, so it is kept before the request
        // takes it.
        let transfer_id = si.id.clone();

        let socket = match TcpStream::connect(si.addr.clone()).await {
            Ok(socket) => socket,
            Err(err) => {
                // The peer could not be reached at all. The caller is waiting
                // for a receipt, so it has to get one.
                self.outcomes
                    .complete(&transfer_id, SendOutcome::Failed(err.to_string()));
                return Err(err.into());
            }
        };

        let mut or = OutboundRequest::new(
            self.endpoint_id,
            socket,
            si.id,
            self.sender.clone(),
            si.ob,
            RemoteDeviceInfo {
                device_type: crate::DeviceType::Unknown,
                name: si.name,
            },
        );
        or.consent_timeout = self.consent_timeout;

        // The outcome of the session, if it ended badly. It is what the
        // caller is told.
        let mut failure: Option<SendOutcome> = None;

        // Send connection request
        if let Err(err) = or.send_connection_request().await {
            failure = Some(SendOutcome::Failed(err.to_string()));
        }

        // Send UKEY init
        if failure.is_none() {
            if let Err(err) = or.send_ukey2_client_init().await {
                failure = Some(SendOutcome::Failed(err.to_string()));
            }
        }

        while failure.is_none() {
            tokio::select! {
                _ = ctk.cancelled() => {
                    info!("{INNER_NAME}: tracker cancelled, breaking");
                    failure = Some(SendOutcome::Failed("the session was cancelled".to_owned()));
                    break;
                },
                r = or.handle() => {
                    if let Err(e) = r {
                        match e.downcast_ref() {
                            Some(AppError::NotAnError) => break,
                            Some(AppError::TimedOut) => {
                                // The peer went quiet. The caller has to be
                                // told, because a quiet peer is not a peer
                                // that agreed.
                                error!("{INNER_NAME}: the peer did not answer in time ({:?})", or.state.state);
                                failure = Some(SendOutcome::TimedOut);
                                break;
                            }
                            None => {
                                if or.state.state == State::Initial {
                                    break;
                                }

                                if or.state.state != State::Finished && or.state.state != State::Cancelled {
                                    let _ = self.sender.clone().send(ChannelMessage {
                                        id: si.addr,
                                        direction: ChannelDirection::LibToFront,
                                        state: Some(State::Disconnected),
                                        ..Default::default()
                                    });
                                }

                                if is_terminal(&or.state.state) {
                                    // The session was over and the peer hung up.
                                    // The socket ending here is the expected end
                                    // of a transfer, not a failure.
                                    debug!("{INNER_NAME}: the peer closed the connection ({:?})", or.state.state);
                                } else {
                                    error!("{INNER_NAME}: error while handling client: {e} ({:?})", or.state.state);
                                    failure = Some(SendOutcome::Failed(e.to_string()));
                                }
                                break;
                            }
                        }
                    }
                }
            }
        }

        // The caller waits for this receipt, so it is always delivered: a
        // transfer that failed must not look like one that worked.
        let outcome = match failure {
            Some(outcome) => outcome,
            None => match or.state.state {
                State::Finished => SendOutcome::Completed,
                State::Rejected => SendOutcome::Rejected,
                State::Cancelled => SendOutcome::Failed("the transfer was cancelled".to_owned()),
                _ => SendOutcome::Failed(format!(
                    "the session ended before the transfer was over ({:?})",
                    or.state.state
                )),
            },
        };
        self.outcomes.complete(&transfer_id, outcome);

        Ok(())
    }
}
