// The WebRTC transport is not part of the Satella protocol engine: LocalSend
// v2 over the LAN does not need it. The module is kept so the fork stays
// close to upstream, but it is only compiled when it is actually used.

#[cfg(feature = "webrtc")]
pub mod signaling;
#[cfg(feature = "webrtc")]
pub mod webrtc;
