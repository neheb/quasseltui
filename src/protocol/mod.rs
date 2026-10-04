//! The Quassel core protocol: probe, framing, handshake, SignalProxy, and
//! the connection state machine that ties them together.

pub mod connection;
pub mod error;
pub mod features;
pub mod framing;
pub mod handshake;
pub mod messages;
pub mod probe;
pub mod signalproxy;
#[cfg(test)]
pub(crate) mod testing;
pub mod transport;
pub mod types;
pub mod usertypes;

pub use error::{Error, Result};
pub use features::Features;
pub use types::{
    AccountId, BufferId, BufferInfo, BufferType, IdentityId, Message, MessageFlags, MessageType,
    MsgId, NetworkId, UserId,
};
pub use usertypes::UserValue;
