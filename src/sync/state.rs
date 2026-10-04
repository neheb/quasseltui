//! `ClientState`: everything the client knows about the session.
//!
//! The UI renders from it and the dispatcher is its only writer. It has a
//! single owner (the UI loop or a headless driver), so there is no locking.
//! On reconnect the same state is handed to the new session so history
//! survives; the dispatcher's re-seed clears the per-session latches.

use std::collections::{BTreeMap, HashMap, HashSet};

use crate::protocol::features::Features;
use crate::protocol::messages::SessionInit;
use crate::protocol::types::{BufferId, BufferInfo, IdentityId, MsgId, NetworkId};
use crate::sync::buffer_syncer::BufferSyncer;
use crate::sync::events::IrcMessage;
use crate::sync::identity::Identity;
use crate::sync::network::Network;

/// The default per-buffer retention cap. IRC traffic is untrusted input,
/// so history is bounded; 5000 lines is about a day of a busy channel.
pub const DEFAULT_MAX_MESSAGES_PER_BUFFER: usize = 5000;

#[derive(Debug, Clone)]
pub struct ClientState {
    pub session: Option<SessionInit>,
    pub peer_features: Features,
    pub networks: BTreeMap<NetworkId, Network>,
    pub buffers: BTreeMap<BufferId, BufferInfo>,
    /// Per-buffer history, oldest first.
    pub messages: BTreeMap<BufferId, Vec<IrcMessage>>,
    pub identities: BTreeMap<IdentityId, Identity>,
    pub buffer_syncer: Option<BufferSyncer>,
    /// Buffers whose backlog was already requested this session.
    pub backlog_requested: HashSet<BufferId>,
    /// Retention cap; 0 disables it (tests only).
    pub max_messages_per_buffer: usize,
    /// "Read up to here" marker per buffer. Seeded from the core's marker
    /// lines and synced back to it. A marker whose message was trimmed is
    /// kept so it reappears if backlog refills that range.
    pub read_markers: HashMap<BufferId, MsgId>,
}

impl Default for ClientState {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_MESSAGES_PER_BUFFER)
    }
}

impl ClientState {
    pub fn new(max_messages_per_buffer: usize) -> Self {
        Self {
            session: None,
            peer_features: Features::empty(),
            networks: BTreeMap::new(),
            buffers: BTreeMap::new(),
            messages: BTreeMap::new(),
            identities: BTreeMap::new(),
            buffer_syncer: None,
            backlog_requested: HashSet::new(),
            max_messages_per_buffer,
            read_markers: HashMap::new(),
        }
    }

    /// The network a buffer belongs to.
    pub fn network_for_buffer(&self, buffer_id: BufferId) -> Option<&Network> {
        let info = self.buffers.get(&buffer_id)?;
        self.networks.get(&info.network_id)
    }

    pub fn messages_for_buffer(&self, buffer_id: BufferId) -> &[IrcMessage] {
        self.messages.get(&buffer_id).map_or(&[], Vec::as_slice)
    }

    pub fn total_message_count(&self) -> usize {
        self.messages.values().map(Vec::len).sum()
    }

    /// Claim the backlog latch for a buffer. `false` if it was already
    /// requested this session.
    pub fn claim_backlog(&mut self, buffer_id: BufferId) -> bool {
        self.backlog_requested.insert(buffer_id)
    }

    /// Release the latch after a failed request so it can be retried.
    pub fn release_backlog(&mut self, buffer_id: BufferId) {
        self.backlog_requested.remove(&buffer_id);
    }

    /// Drop the oldest messages beyond the retention cap.
    pub(crate) fn enforce_cap(&mut self, buffer_id: BufferId) {
        let cap = self.max_messages_per_buffer;
        if let Some(list) = self.messages.get_mut(&buffer_id)
            && cap > 0
            && list.len() > cap
        {
            list.drain(..list.len() - cap);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::types::BufferType;

    #[test]
    fn defaults() {
        let state = ClientState::default();
        assert!(state.session.is_none());
        assert!(state.networks.is_empty());
        assert_eq!(state.max_messages_per_buffer, 5000);
        assert_eq!(state.total_message_count(), 0);
    }

    #[test]
    fn network_for_buffer() {
        let mut state = ClientState::default();
        state.networks.insert(NetworkId(1), Network::new("1"));
        state.buffers.insert(
            BufferId(10),
            BufferInfo {
                buffer_id: BufferId(10),
                network_id: NetworkId(1),
                kind: BufferType::Channel,
                group_id: 0,
                name: "#x".into(),
            },
        );
        assert_eq!(
            state.network_for_buffer(BufferId(10)).unwrap().network_id(),
            1
        );
        assert!(state.network_for_buffer(BufferId(99)).is_none());
        assert!(state.messages_for_buffer(BufferId(99)).is_empty());
    }

    #[test]
    fn backlog_latch() {
        let mut state = ClientState::default();
        assert!(state.claim_backlog(BufferId(1)));
        assert!(!state.claim_backlog(BufferId(1)));
        state.release_backlog(BufferId(1));
        assert!(state.claim_backlog(BufferId(1)));
    }
}
