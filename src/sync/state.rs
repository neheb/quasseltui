//! `ClientState`: everything the client knows about the session.
//!
//! The UI renders from it and the dispatcher is its only writer. It has a
//! single owner (the UI loop or a headless driver), so there is no locking.
//! On reconnect the same state is handed to the new session so history
//! survives; the dispatcher's re-seed clears the per-session latches.

use std::collections::{BTreeMap, HashMap, HashSet};

use crate::protocol::features::Features;
use crate::protocol::types::{BufferId, BufferInfo, IdentityId, MsgId, NetworkId};
use crate::sync::buffer_syncer::BufferSyncer;
use crate::sync::events::IrcMessage;
use crate::sync::identity::Identity;
use crate::sync::irc_channel::IrcChannel;
use crate::sync::irc_user::IrcUser;
use crate::sync::network::Network;

/// The default per-buffer retention cap. IRC traffic is untrusted input,
/// so history is bounded; 5000 lines is about a day of a busy channel.
pub const DEFAULT_MAX_MESSAGES_PER_BUFFER: usize = 5000;

/// Most buffers kept. A message for a new buffer past this is dropped:
/// with the per-buffer cap, this bounds history memory overall.
pub use crate::protocol::messages::MAX_BUFFERS;

/// Not `Clone`: a copy would duplicate every buffer's history.
#[derive(Debug)]
pub struct ClientState {
    pub peer_features: Features,
    pub networks: BTreeMap<NetworkId, Network>,
    pub buffers: BTreeMap<BufferId, BufferInfo>,
    /// Per-buffer history, oldest first.
    pub messages: BTreeMap<BufferId, Vec<IrcMessage>>,
    pub identities: BTreeMap<IdentityId, Identity>,
    /// IrcChannel objects by object name (`"<netId>/<channel>"`): topic and
    /// members of the channels we're in.
    pub channels: BTreeMap<String, IrcChannel>,
    /// IrcUser objects by object name (`"<netId>/<nick>"`).
    pub users: BTreeMap<String, IrcUser>,
    /// Members across all channels, kept in step by the dispatcher so the
    /// membership cap is checked without walking every channel.
    pub(crate) memberships: usize,
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
            peer_features: Features::empty(),
            networks: BTreeMap::new(),
            buffers: BTreeMap::new(),
            messages: BTreeMap::new(),
            identities: BTreeMap::new(),
            channels: BTreeMap::new(),
            users: BTreeMap::new(),
            memberships: 0,
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

    /// Recount channel members, after a bulk change to the rosters.
    pub(crate) fn recount_memberships(&mut self) {
        self.memberships = self.channels.values().map(|c| c.user_modes.len()).sum();
    }

    /// The channel object behind a channel buffer. Buffer and object names
    /// can differ in case, as IRC channel names are case-insensitive.
    pub fn channel_for_buffer(&self, buffer: &BufferInfo) -> Option<&IrcChannel> {
        let network_id = buffer.network_id.0;
        self.channels
            .get(&format!("{network_id}/{}", buffer.name))
            .or_else(|| {
                self.channels.values().find(|c| {
                    c.network_id == network_id && c.name.eq_ignore_ascii_case(&buffer.name)
                })
            })
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
