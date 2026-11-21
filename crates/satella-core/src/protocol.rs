//! The wire protocols Satella speaks.
//!
//! Every adapter reports the protocol it speaks, so a frontend can badge a
//! peer and a pending receive with the protocol the peer actually used.

use serde::{Deserialize, Serialize};
use std::fmt;

/// A file sharing protocol supported by Satella.
///
/// Adding a protocol means adding a variant here and an adapter crate that
/// implements [`crate::ProtocolAdapter`]. The variant is the single source of
/// truth for how the protocol is named in the public API.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub enum Protocol {
    /// LocalSend protocol v2: HTTP/HTTPS over the LAN with mDNS discovery.
    LocalSend,
    /// Google Quick Share (Nearby Share) over the LAN, mDNS plus Bluetooth assisted discovery.
    QuickShare,
    /// Mutual Transfer Alliance over BLE and Wi-Fi Direct.
    Mta,
}

impl Protocol {
    /// The stable name of the protocol, used in the public API and in logs.
    pub const fn as_str(self) -> &'static str {
        match self {
            Protocol::LocalSend => "localsend",
            Protocol::QuickShare => "quickshare",
            Protocol::Mta => "mta",
        }
    }

    /// The display name of the protocol, used by frontends that show a badge.
    pub const fn display_name(self) -> &'static str {
        match self {
            Protocol::LocalSend => "LocalSend",
            Protocol::QuickShare => "Quick Share",
            Protocol::Mta => "Mutual Transfer Alliance",
        }
    }
}

impl fmt::Display for Protocol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl From<Protocol> for String {
    fn from(protocol: Protocol) -> Self {
        protocol.as_str().to_owned()
    }
}
