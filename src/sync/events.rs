//! Client-facing events: the stable contract consumed by the UI and other
//! embedders. Each one describes something a user cares about ("a buffer
//! appeared", "a message arrived"), not every internal state mutation.
//! `Disconnected` is always the last event of a session.

use std::sync::Arc;

use chrono::{DateTime, Utc};

use crate::protocol::error::Error;
use crate::protocol::features::Features;
use crate::protocol::messages::SessionInit;
use crate::protocol::types::{
    BufferId, BufferType, IdentityId, Message, MessageFlags, MessageType, MsgId, NetworkId,
};
use crate::sync::network::NetworkConnectionState;
use crate::util::text::truncate_bytes;

/// One IRC message as the UI sees it: the subset of `Message` worth
/// rendering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IrcMessage {
    pub msg_id: MsgId,
    pub buffer_id: BufferId,
    pub network_id: NetworkId,
    pub timestamp: DateTime<Utc>,
    pub kind: MessageType,
    pub flags: MessageFlags,
    pub sender: String,
    pub sender_prefixes: String,
    pub contents: String,
}

/// Longest message body kept. IRC lines are at most 512 bytes and Quassel
/// splits longer input, but one decoded string may be 16 MiB; history is
/// capped in lines, so each line needs a size bound too.
pub const MAX_CONTENTS_BYTES: usize = 8 * 1024;
/// Longest sender (`nick!user@host`) and prefix string kept.
pub const MAX_SENDER_BYTES: usize = 1024;

impl From<&Message> for IrcMessage {
    fn from(raw: &Message) -> Self {
        Self {
            msg_id: raw.msg_id,
            buffer_id: raw.buffer_info.buffer_id,
            network_id: raw.buffer_info.network_id,
            timestamp: raw.timestamp,
            kind: raw.kind,
            flags: raw.flags,
            sender: truncate_bytes(&raw.sender, MAX_SENDER_BYTES),
            sender_prefixes: truncate_bytes(&raw.sender_prefixes, MAX_SENDER_BYTES),
            contents: truncate_bytes(&raw.contents, MAX_CONTENTS_BYTES),
        }
    }
}

/// A network property that changed, with its new value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetworkChange {
    Name(String),
    CurrentServer(String),
    MyNick(String),
    ConnectionState(NetworkConnectionState),
    Connected(bool),
}

#[derive(Debug, Clone)]
pub enum ClientEvent {
    /// The handshake is done. A burst of `NetworkAdded`/`BufferAdded`/
    /// `IdentityAdded` follows as the session is walked.
    SessionOpened {
        session: Arc<SessionInit>,
        peer_features: Features,
    },
    NetworkAdded {
        network_id: NetworkId,
        name: String,
    },
    NetworkUpdated {
        network_id: NetworkId,
        change: NetworkChange,
    },
    NetworkRemoved {
        network_id: NetworkId,
    },
    BufferAdded {
        buffer_id: BufferId,
        network_id: NetworkId,
        name: String,
        kind: BufferType,
    },
    BufferRenamed {
        buffer_id: BufferId,
        name: String,
    },
    BufferRemoved {
        buffer_id: BufferId,
    },
    MessageReceived(IrcMessage),
    /// A backlog reply was merged; `count` new messages kept (after the
    /// retention cap). Emitted for every
    /// reply that maps to a live buffer, including empty ones, so the UI
    /// knows the request completed.
    BacklogReceived {
        buffer_id: BufferId,
        count: usize,
    },
    IdentityAdded {
        identity_id: IdentityId,
        name: String,
    },
    /// The core's unread state for a buffer: the `Message::Types` bits and
    /// the number of highlights since its last-seen message. All zero means
    /// the buffer was read (possibly by another client). Only cores with
    /// `BufferActivitySync` (0.13+) send these.
    BufferActivity {
        buffer_id: BufferId,
        types: u32,
        highlights: i64,
    },
    /// Terminal. `error` is set when an error ended the session.
    Disconnected {
        reason: String,
        error: Option<Arc<Error>>,
    },
}

impl ClientEvent {
    /// Variant name, for event counting in `dump-state`.
    pub fn name(&self) -> &'static str {
        match self {
            Self::SessionOpened { .. } => "SessionOpened",
            Self::NetworkAdded { .. } => "NetworkAdded",
            Self::NetworkUpdated { .. } => "NetworkUpdated",
            Self::NetworkRemoved { .. } => "NetworkRemoved",
            Self::BufferAdded { .. } => "BufferAdded",
            Self::BufferRenamed { .. } => "BufferRenamed",
            Self::BufferRemoved { .. } => "BufferRemoved",
            Self::MessageReceived(_) => "MessageReceived",
            Self::BacklogReceived { .. } => "BacklogReceived",
            Self::IdentityAdded { .. } => "IdentityAdded",
            Self::BufferActivity { .. } => "BufferActivity",
            Self::Disconnected { .. } => "Disconnected",
        }
    }
}
