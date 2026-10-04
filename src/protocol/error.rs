//! Error type shared by the protocol, sync, and client layers.
//!
//! The variants mirror the failure classes callers need to tell apart:
//! the CLI maps `Auth` and `Transport` to distinct exit codes, and a
//! reconnect policy must never retry on `Auth`.

use crate::qt::datastream::DecodeError;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A binary payload was malformed.
    #[error("{0}")]
    Decode(#[from] DecodeError),
    /// The pre-framing probe handshake failed.
    #[error("{0}")]
    Probe(String),
    /// A handshake message was malformed or the core rejected us.
    #[error("{0}")]
    Handshake(String),
    /// The core rejected our credentials. Never retried.
    #[error("{0}")]
    Auth(String),
    /// Connect, TLS upgrade, or low-level I/O failure.
    #[error("{0}")]
    Transport(String),
    /// The peer closed the connection mid-stream.
    #[error("{0}")]
    ConnectionClosed(String),
    /// A peer-supplied frame length exceeds the configured cap.
    #[error("{0}")]
    FrameTooLarge(String),
    /// A SignalProxy frame doesn't match any known shape.
    #[error("{0}")]
    SignalProxy(String),
    #[error("{0}")]
    Io(#[from] std::io::Error),
    /// Anything else, with a human-readable description.
    #[error("{0}")]
    Other(String),
}

impl Error {
    pub fn is_auth(&self) -> bool {
        matches!(self, Self::Auth(_))
    }

    pub fn is_transport(&self) -> bool {
        matches!(self, Self::Transport(_))
    }
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
