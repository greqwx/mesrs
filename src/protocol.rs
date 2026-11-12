// Copyright (c) 2026 Greg

//! wire format and the channel types shared by the ui and the network thread

use std::net::Ipv4Addr;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// bumped when the packet layout changes, packets with another version are dropped
pub const PROTO: u8 = 1;
/// first port of the range used by group codes, see [`group_for`]
pub const BASE_PORT: u16 = 25100;

/// one udp datagram, everything is plain json
#[derive(Serialize, Deserialize)]
pub struct Packet {
    pub proto: u8,
    pub peer: String,
    pub nick: String,
    /// hello, msg, action, nick, topic or bye
    pub kind: String,
    /// message id, empty for everything except chat and actions
    pub id: String,
    #[serde(default)]
    pub text: String,
    /// group topic, carried by hello so newcomers learn it
    #[serde(default)]
    pub topic: String,
}

/// builds an outgoing packet
pub fn packet(peer: &str, nick: &str, kind: &str, id: &str, text: &str) -> Packet {
    Packet {
        proto: PROTO,
        peer: peer.to_string(),
        nick: nick.to_string(),
        kind: kind.to_string(),
        id: id.to_string(),
        text: text.to_string(),
        topic: String::new(),
    }
}

/// what the ui asks the network thread to do
pub enum OutCmd {
    Chat(String),
    Action(String),
    Nick(String),
    Topic(String),
    /// switch to another group, the network thread reopens the socket
    Join(u32),
}

/// a chat message or action that arrived from someone else
pub struct PeerMessage {
    pub peer: String,
    pub nick: String,
    pub id: String,
    pub text: String,
    pub action: bool,
}

/// what the network thread reports back to the ui
pub enum NetEvent {
    Message(PeerMessage),
    /// id and nickname of everyone in the group, sorted by nickname
    Peers(Vec<(String, String)>),
    /// shared topic, empty means none
    Topic(String),
    Joined {
        code: u32,
        port: u16,
    },
    /// a message got no acknowledgement from these peers before the deadline
    Undelivered {
        to: Vec<String>,
        text: String,
    },
    /// join, leave, rename and fallback notices
    Notice(String),
    Error(String),
}

/// maps a 6 digit code to a multicast group and port
///
/// the low 16 bits pick the last two octets, the rest picks the port offset,
/// so all million codes are distinct and stay inside 239.255.0.0/16
pub fn group_for(code: u32) -> (Ipv4Addr, u16) {
    let low = code % 65_536;
    let addr = Ipv4Addr::new(239, 255, (low % 256) as u8, (low / 256) as u8);
    (addr, BASE_PORT + (code / 65_536) as u16)
}

/// the code must be exactly 6 ascii digits, leading zeros are fine
pub fn parse_code(raw: &str) -> Option<u32> {
    if raw.len() != 6 || !raw.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    raw.parse().ok()
}

/// random id without pulling in a random number crate
///
/// the hasher from `RandomState` is seeded by the os, mixing in the clock and
/// the pid keeps ids unique between processes started at the same moment
pub fn random_id() -> String {
    use std::hash::{BuildHasher, Hasher};

    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    hasher.write_u128(nanos);
    hasher.write_u32(std::process::id());
    format!("{:016x}", hasher.finish())
}
