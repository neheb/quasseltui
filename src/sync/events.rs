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

impl From<&Message> for IrcMessage {
    fn from(raw: &Message) -> Self {
        Self {
            msg_id: raw.msg_id,
            buffer_id: raw.buffer_info.buffer_id,
            network_id: raw.buffer_info.network_id,
            timestamp: raw.timestamp,
            kind: raw.kind,
            flags: raw.flags,
            sender: raw.sender.clone(),
            sender_prefixes: raw.sender_prefixes.clone(),
            contents: raw.contents.clone(),
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

impl NetworkChange {
    /// Stable tag for the changed field.
    pub fn field_name(&self) -> &'static str {
        match self {
            Self::Name(_) => "network_name",
            Self::CurrentServer(_) => "current_server",
            Self::MyNick(_) => "my_nick",
            Self::ConnectionState(_) => "connection_state",
            Self::Connected(_) => "is_connected",
        }
    }
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
    /// A backlog reply was merged; `count` new messages. Emitted for every
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
            Self::Disconnected { .. } => "Disconnected",
        }
    }
}
