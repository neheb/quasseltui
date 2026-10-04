//! The embeddable client: a connection, a dispatcher, and the requests a
//! UI makes (send input, backlog, read state).
//!
//! `ClientState` is owned by the caller, not the client, so the UI loop
//! can read it between events without locks and a reconnect can hand the
//! same state to a fresh client (history survives; the re-seed merges in).
//!
//! Driving it:
//!
//! - [`QuasselClient::next_event`] for a headless loop, or
//! - [`QuasselClient::recv`] (cancel-safe, so it can sit in a `select!`)
//!   followed by [`QuasselClient::apply`].
//!
//! The event stream always ends with `ClientEvent::Disconnected`.

use std::collections::{HashSet, VecDeque};

use tokio::sync::mpsc;

use crate::protocol::connection::{
    ConnectionHandle, ConnectionOptions, ProtocolEvent, QuasselConnection,
};
use crate::protocol::error::{Error, Result};
use crate::protocol::signalproxy::{InitRequest, RpcCall, SignalProxyMessage, SyncMessage};
use crate::protocol::types::{BufferId, BufferInfo, MsgId, NetworkId};
use crate::protocol::usertypes::UserValue;
use crate::qt::variant::Variant;
use crate::sync::dispatcher::Dispatcher;
use crate::sync::events::ClientEvent;
use crate::sync::{buffer_syncer, network};

pub use crate::sync::events::{IrcMessage, NetworkChange};
pub use crate::sync::state::ClientState;

/// `UserInputHandler::sendInput(BufferInfo, QString)`: the single entry
/// point for everything a user types. The core parses `/commands` itself.
pub const SEND_INPUT_SIGNAL: &[u8] = b"2sendInput(BufferInfo,QString)";
/// How many messages a backlog request asks for.
pub const DEFAULT_BACKLOG_LIMIT: i32 = 100;

pub struct QuasselClient {
    handle: ConnectionHandle,
    events: mpsc::Receiver<ProtocolEvent>,
    dispatcher: Dispatcher,
    /// Networks we've already InitRequested, lazily seeded from the
    /// session (those are covered by the fan-out) so a network created
    /// mid-session gets exactly one request.
    networks_requested: Option<HashSet<NetworkId>>,
    pending: VecDeque<ClientEvent>,
    finished: bool,
}

impl QuasselClient {
    /// Start connecting. Must be called inside a Tokio runtime.
    pub fn connect(options: ConnectionOptions) -> Self {
        Self::from_connection(QuasselConnection::new(options))
    }

    pub fn from_connection(connection: QuasselConnection) -> Self {
        let (handle, events) = connection.start();
        Self {
            handle,
            events,
            dispatcher: Dispatcher::new(),
            networks_requested: None,
            pending: VecDeque::new(),
            finished: false,
        }
    }

    /// A cloneable handle for requests from other tasks.
    pub fn handle(&self) -> ClientHandle {
        ClientHandle {
            connection: self.handle.clone(),
        }
    }

    pub fn dispatcher(&self) -> &Dispatcher {
        &self.dispatcher
    }

    pub fn close(&self) {
        self.handle.close();
    }

    /// The next protocol event, or `None` once the stream has ended.
    /// Cancel-safe.
    pub async fn recv(&mut self) -> Option<ProtocolEvent> {
        if self.finished {
            return None;
        }
        let event = self.events.recv().await;
        if matches!(event, None | Some(ProtocolEvent::Disconnected { .. })) {
            self.finished = true;
        }
        event.or_else(|| {
            // The connection task always sends a terminal event; this only
            // covers a runtime shutting down underneath us.
            Some(ProtocolEvent::Disconnected {
                reason: "connection task ended".into(),
                error: None,
            })
        })
    }

    /// Apply one protocol event to `state` and return the resulting client
    /// events. Also queues the InitRequests the core needs before it sends
    /// most state: the BufferSyncer and every network at session start, and
    /// any network that appears later.
    pub fn apply(&mut self, event: ProtocolEvent, state: &mut ClientState) -> Vec<ClientEvent> {
        let mut out = Vec::new();
        match event {
            ProtocolEvent::SessionReady {
                session,
                peer_features,
                ..
            } => {
                self.dispatcher
                    .seed_from_session(state, &session, peer_features, &mut out);
                self.networks_requested = Some(session.network_ids.iter().copied().collect());
                self.request_init(buffer_syncer::CLASS_NAME, "");
                for nid in &session.network_ids {
                    self.request_init(network::CLASS_NAME, &nid.to_string());
                }
            }
            ProtocolEvent::Sync(msg) => self.dispatcher.handle_sync(state, &msg, &mut out),
            ProtocolEvent::InitData(msg) => self.dispatcher.handle_init_data(state, &msg, &mut out),
            ProtocolEvent::Rpc(msg) => self.dispatcher.handle_rpc(state, &msg, &mut out),
            // We host no syncable objects, so there is nothing to answer
            // with; the connection already answered heartbeats.
            ProtocolEvent::InitRequest(_) | ProtocolEvent::HeartBeat(_) => {}
            ProtocolEvent::Disconnected { reason, error } => {
                self.finished = true;
                out.push(ClientEvent::Disconnected { reason, error });
            }
        }
        for event in &out {
            if let ClientEvent::NetworkAdded { network_id, .. } = event {
                let requested = self.networks_requested.get_or_insert_with(HashSet::new);
                if requested.insert(*network_id) {
                    self.request_init(network::CLASS_NAME, &network_id.to_string());
                }
            }
        }
        out
    }

    /// `recv` + `apply`, buffered: the next client event, or `None` after
    /// `Disconnected` was returned.
    pub async fn next_event(&mut self, state: &mut ClientState) -> Option<ClientEvent> {
        loop {
            if let Some(event) = self.pending.pop_front() {
                return Some(event);
            }
            let event = self.recv().await?;
            let events = self.apply(event, state);
            self.pending.extend(events);
        }
    }

    fn request_init(&self, class_name: &[u8], object_name: &str) {
        let request = SignalProxyMessage::InitRequest(InitRequest {
            class_name: class_name.to_vec(),
            object_name: object_name.to_string(),
        });
        // A failure here means the connection is going away; it reports
        // that itself with a better reason.
        if let Err(e) = self.handle.send_nowait(request) {
            tracing::warn!(
                "InitRequest for {}({object_name:?}) failed: {e}",
                String::from_utf8_lossy(class_name)
            );
        }
    }
}

/// Requests to the core. Cheap to clone and `Send`, so a UI can run each
/// request in its own task.
#[derive(Clone)]
pub struct ClientHandle {
    connection: ConnectionHandle,
}

impl ClientHandle {
    async fn send(&self, message: SignalProxyMessage, what: &str) -> Result<()> {
        self.connection.send(message).await.map_err(|e| match e {
            // State errors are already clear on their own.
            Error::Other(msg) if msg.starts_with("cannot send") => Error::Other(msg),
            other => Error::Other(format!("failed to {what}: {other}")),
        })
    }

    /// Send a chat line or `/command` for a buffer. The core echoes the
    /// result back as an ordinary `displayMsg`.
    pub async fn send_input(&self, buffer: BufferInfo, text: String) -> Result<()> {
        let rpc = SignalProxyMessage::RpcCall(RpcCall {
            signal_name: SEND_INPUT_SIGNAL.to_vec(),
            params: vec![
                Variant::User(UserValue::BufferInfo(buffer)),
                Variant::String(text),
            ],
        });
        self.send(rpc, "send input").await
    }

    /// Ask for up to `limit` historical messages. The reply arrives as a
    /// `BacklogReceived` event. Callers dedupe with
    /// `ClientState::claim_backlog` and release the latch on failure.
    pub async fn request_backlog(&self, buffer_id: BufferId, limit: i32) -> Result<()> {
        let sync = SignalProxyMessage::Sync(SyncMessage {
            class_name: b"BacklogManager".to_vec(),
            object_name: String::new(),
            slot_name: b"requestBacklog".to_vec(),
            params: vec![
                Variant::User(UserValue::BufferId(buffer_id)),
                Variant::User(UserValue::MsgId(MsgId(-1))),
                Variant::User(UserValue::MsgId(MsgId(-1))),
                Variant::Int(limit),
                Variant::Int(0),
            ],
        });
        self.send(sync, "request backlog").await
    }

    /// Mark a buffer read up to `msg_id`. The core broadcasts it, which
    /// clears unread state in every other connected client.
    pub async fn set_last_seen(&self, buffer_id: BufferId, msg_id: MsgId) -> Result<()> {
        self.buffer_syncer_request(b"requestSetLastSeenMsg", buffer_id, msg_id)
            .await
    }

    /// Move the core-persisted marker line, so it survives restarts and
    /// shows up in other clients.
    pub async fn set_marker_line(&self, buffer_id: BufferId, msg_id: MsgId) -> Result<()> {
        self.buffer_syncer_request(b"requestSetMarkerLine", buffer_id, msg_id)
            .await
    }

    async fn buffer_syncer_request(
        &self,
        slot: &[u8],
        buffer_id: BufferId,
        msg_id: MsgId,
    ) -> Result<()> {
        let sync = SignalProxyMessage::Sync(SyncMessage {
            class_name: buffer_syncer::CLASS_NAME.to_vec(),
            object_name: String::new(),
            slot_name: slot.to_vec(),
            params: vec![
                Variant::User(UserValue::BufferId(buffer_id)),
                Variant::User(UserValue::MsgId(msg_id)),
            ],
        });
        self.send(sync, &format!("send {}", String::from_utf8_lossy(slot)))
            .await
    }

    pub fn close(&self) {
        self.connection.close();
    }
}

#[cfg(test)]
mod tests;
