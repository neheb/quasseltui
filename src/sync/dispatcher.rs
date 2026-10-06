//! Routes inbound SignalProxy traffic to syncable objects, mutates
//! `ClientState`, and produces `ClientEvent`s.
//!
//! Every `Sync`/`InitData` frame addresses an object by `(className,
//! objectName)`. Networks must be known (from the session or
//! `networkCreated`); IRC users and channels are created on first sight on
//! a known network, up to [`MAX_ROSTER_OBJECTS`]; unknown classes are
//! dropped. The core is untrusted input, so nothing it names grows state
//! without bound. Networks, identities and the BufferSyncer
//! live in `ClientState` (the UI reads them); IRC users and channels live
//! here, since only the dispatcher needs them.
//!
//! Live IRC messages arrive as the top-level RPC `2displayMsg(Message)`,
//! not as a Sync, and are intercepted here.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::protocol::features::Features;
use crate::protocol::messages::SessionInit;
use crate::protocol::signalproxy::{InitData, RpcCall, SyncMessage};
use crate::protocol::types::{BufferId, IdentityId, Message, MsgId, NetworkId};
use crate::protocol::usertypes::UserValue;
use crate::qt::variant::Variant;
use crate::sync::backlog_manager::{self, BacklogManager};
use crate::sync::buffer_syncer::{self, BufferSyncer};
use crate::sync::events::{ClientEvent, IrcMessage, NetworkChange};
use crate::sync::identity::{self, Identity};
use crate::sync::irc_channel::{self, IrcChannel};
use crate::sync::irc_user::{self, IrcUser};
use crate::sync::network::{self, Network};
use crate::sync::state::{ClientState, MAX_BUFFERS};

/// The core announces a new IRC message.
pub const DISPLAY_MSG_SIGNAL: &[u8] = b"2displayMsg(Message)";
/// SignalProxy's object rename broadcast: `[className, newName, oldName]`.
/// Quassel re-addresses IrcUser objects this way on every nick change.
pub const OBJECT_RENAMED_SIGNAL: &[u8] = b"__objectRenamed__";
pub const NETWORK_CREATED_SIGNAL: &[u8] = b"2networkCreated(NetworkId)";
pub const NETWORK_REMOVED_SIGNAL: &[u8] = b"2networkRemoved(NetworkId)";

/// Most IrcUser plus IrcChannel objects kept across all networks. Far
/// beyond a real session (a user in dozens of large channels sees tens of
/// thousands of nicks); past it, new ones are ignored.
pub const MAX_ROSTER_OBJECTS: usize = 200_000;

/// BufferSyncer slots that store a value per buffer id.
const PER_BUFFER_SLOTS: [&[u8]; 4] = [
    b"setLastSeenMsg",
    b"setMarkerLine",
    b"setBufferActivity",
    b"setHighlightCount",
];

#[derive(Debug, Default)]
pub struct Dispatcher {
    /// IrcChannel objects by object name (`"<netId>/<channel>"`).
    channels: BTreeMap<String, IrcChannel>,
    /// IrcUser objects by object name (`"<netId>/<nick>"`).
    users: BTreeMap<String, IrcUser>,
    backlog: BacklogManager,
}

fn i32_id(value: i64) -> Option<i32> {
    i32::try_from(value).ok()
}

fn as_network_id(value: &Variant) -> Option<NetworkId> {
    value.coerce_i64().and_then(i32_id).map(NetworkId)
}

impl Dispatcher {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn channel(&self, object_name: &str) -> Option<&IrcChannel> {
        self.channels.get(object_name)
    }

    pub fn user(&self, object_name: &str) -> Option<&IrcUser> {
        self.users.get(object_name)
    }

    pub fn channels(&self) -> impl Iterator<Item = &IrcChannel> {
        self.channels.values()
    }

    pub fn users(&self) -> impl Iterator<Item = &IrcUser> {
        self.users.values()
    }

    // -- session seeding -----------------------------------------------------

    /// Populate state from a fresh `SessionInit`.
    ///
    /// Network *state* (name, nick, ...) arrives later via InitData; the
    /// networks created here start empty. On reconnect `state` is reused,
    /// so per-session latches are reset: clearing `backlog_requested` makes
    /// the next buffer switch re-request history, and msg_id dedup in the
    /// backlog merge makes that fill the gap safely.
    pub fn seed_from_session(
        &mut self,
        state: &mut ClientState,
        session: &SessionInit,
        peer_features: Features,
        out: &mut Vec<ClientEvent>,
    ) {
        state.session = Some(session.clone());
        state.peer_features = peer_features;
        state.backlog_requested.clear();
        out.push(ClientEvent::SessionOpened {
            session: Arc::new(session.clone()),
            peer_features,
        });

        for nid in &session.network_ids {
            state.networks.insert(*nid, Network::new(nid.to_string()));
            out.push(ClientEvent::NetworkAdded {
                network_id: *nid,
                name: String::new(),
            });
        }

        for buf in &session.buffer_infos {
            state.buffers.insert(buf.buffer_id, buf.clone());
            state.messages.entry(buf.buffer_id).or_default();
            out.push(ClientEvent::BufferAdded {
                buffer_id: buf.buffer_id,
                network_id: buf.network_id,
                name: buf.name.clone(),
                kind: buf.kind,
            });
        }

        // Real cores wrap identityId in the IdentityId user type; plain
        // ints are accepted too.
        for raw in &session.identities {
            let id_value = raw
                .get("identityId")
                .filter(|v| v.truthy())
                .or_else(|| raw.get("IdentityId"));
            let Some(id) = id_value.and_then(|v| match v {
                Variant::User(UserValue::IdentityId(id)) => Some(id.0),
                other => other.as_i64().and_then(i32_id),
            }) else {
                continue;
            };
            let mut identity = Identity::new(id.to_string());
            identity.apply_init_data(raw);
            let name = identity.identity_name.clone();
            state.identities.insert(IdentityId(id), identity);
            out.push(ClientEvent::IdentityAdded {
                identity_id: IdentityId(id),
                name,
            });
        }

        state.buffer_syncer = Some(BufferSyncer::new());
        self.backlog = BacklogManager::new();
    }

    // -- object lookup ---------------------------------------------------------

    /// A network we know of. Sync and InitData for any other id are
    /// dropped: networks come from the session or `networkCreated`, and
    /// creating them from arbitrary object names would let the core grow
    /// state (and our InitRequests) without bound.
    fn known_network<'s>(state: &'s mut ClientState, object_name: &str) -> Option<&'s mut Network> {
        let nid = NetworkId(object_name.parse().ok()?);
        let network = state.networks.get_mut(&nid);
        if network.is_none() {
            tracing::debug!("ignoring object for unknown network {object_name:?}");
        }
        network
    }

    /// Whether a new roster object may be created for `object_name`
    /// (`"<netId>/<name>"`).
    fn may_create(&self, state: &ClientState, object_name: &str) -> bool {
        let (network_id, name) = crate::sync::split_object_name(object_name);
        if name.is_empty() || !state.networks.contains_key(&NetworkId(network_id)) {
            tracing::debug!("ignoring roster object {object_name:?}: unknown network");
            return false;
        }
        if self.users.len() + self.channels.len() >= MAX_ROSTER_OBJECTS {
            tracing::warn!("roster object cap reached; ignoring {object_name:?}");
            return false;
        }
        true
    }

    fn identity_entry<'s>(state: &'s mut ClientState, object_name: &str) -> &'s mut Identity {
        let iid = IdentityId(object_name.parse().unwrap_or(-1));
        state
            .identities
            .entry(iid)
            .or_insert_with(|| Identity::new(object_name))
    }

    fn channel_entry(&mut self, state: &ClientState, object_name: &str) -> Option<&mut IrcChannel> {
        if !self.channels.contains_key(object_name) && !self.may_create(state, object_name) {
            return None;
        }
        Some(
            self.channels
                .entry(object_name.to_string())
                .or_insert_with(|| IrcChannel::new(object_name)),
        )
    }

    fn user_entry(&mut self, state: &ClientState, object_name: &str) -> Option<&mut IrcUser> {
        if !self.users.contains_key(object_name) && !self.may_create(state, object_name) {
            return None;
        }
        Some(
            self.users
                .entry(object_name.to_string())
                .or_insert_with(|| IrcUser::new(object_name)),
        )
    }

    // -- Sync ------------------------------------------------------------------

    pub fn handle_sync(
        &mut self,
        state: &mut ClientState,
        msg: &SyncMessage,
        out: &mut Vec<ClientEvent>,
    ) {
        let slot = msg.slot_name.as_slice();
        let params = msg.params.as_slice();
        match msg.class_name.as_slice() {
            network::CLASS_NAME => {
                let Some(network) = Self::known_network(state, &msg.object_name) else {
                    return;
                };
                network.handle_sync(slot, params);
                let change = match slot {
                    b"setNetworkName" => NetworkChange::Name(network.network_name.clone()),
                    b"setCurrentServer" => {
                        NetworkChange::CurrentServer(network.current_server.clone())
                    }
                    b"setMyNick" => NetworkChange::MyNick(network.my_nick.clone()),
                    b"setConnectionState" => {
                        NetworkChange::ConnectionState(network.connection_state)
                    }
                    b"setConnected" => NetworkChange::Connected(network.is_connected),
                    _ => return,
                };
                let network_id = NetworkId(network.network_id());
                if matches!(change, NetworkChange::Connected(false)) {
                    // Like the C++ client's removeChansAndUsers(): an IRC-side
                    // disconnect invalidates the roster, or stale members
                    // merge with the fresh seed on every reconnect.
                    self.clear_network_rosters(state, network_id);
                }
                out.push(ClientEvent::NetworkUpdated { network_id, change });
            }
            irc_channel::CLASS_NAME => {
                if let Some(channel) = self.channel_entry(state, &msg.object_name) {
                    channel.handle_sync(slot, params);
                }
            }
            irc_user::CLASS_NAME => {
                let Some(user) = self.user_entry(state, &msg.object_name) else {
                    return;
                };
                user.handle_sync(slot, params);
                let (key, nick, network_id) =
                    (user.object_name.clone(), user.nick.clone(), user.network_id);
                // Real cores sync parts/kicks as IrcUser::partChannel and
                // quits as IrcUser::quit (IrcChannel::part is not a sync
                // method), so the channel rosters only stay right if the
                // removal is fanned out here.
                match slot {
                    b"partChannel" if !params.is_empty() => {
                        let channel = params[0].coerce_string().unwrap_or_default();
                        self.cascade_user_part(state, network_id, &nick, &channel);
                    }
                    b"quit" => self.cascade_user_quit(state, &key, network_id, &nick),
                    _ => {}
                }
            }
            identity::CLASS_NAME => {
                Self::identity_entry(state, &msg.object_name).handle_sync(slot, params)
            }
            buffer_syncer::CLASS_NAME => {
                // Per-buffer state is only kept for buffers we know of.
                if PER_BUFFER_SLOTS.contains(&slot)
                    && !params
                        .first()
                        .and_then(Variant::coerce_i64)
                        .and_then(i32_id)
                        .is_some_and(|bid| state.buffers.contains_key(&BufferId(bid)))
                {
                    tracing::debug!(
                        "ignoring BufferSyncer::{} for unknown buffer",
                        String::from_utf8_lossy(slot)
                    );
                    return;
                }
                state
                    .buffer_syncer
                    .get_or_insert_with(BufferSyncer::new)
                    .handle_sync(slot, params);
                Self::drain_buffer_syncer(state, out);
            }
            backlog_manager::CLASS_NAME => {
                self.backlog.handle_sync(slot, params);
                if slot == b"receiveBacklog" {
                    self.merge_backlog(state, out);
                }
            }
            other => tracing::debug!(
                "ignoring Sync for unknown class {:?}::{:?}",
                String::from_utf8_lossy(other),
                msg.object_name
            ),
        }
    }

    fn cascade_user_part(
        &mut self,
        state: &mut ClientState,
        network_id: i32,
        nick: &str,
        channel: &str,
    ) {
        let key = format!("{network_id}/{channel}");
        let Some(chan) = self.channels.get_mut(&key) else {
            return;
        };
        chan.user_modes.remove(nick);
        // Our own part means the core stops syncing the channel: tear it
        // down, or the stale roster merges ghosts into the seed on rejoin.
        if let Some(network) = state.networks.get_mut(&NetworkId(network_id))
            && !network.my_nick.is_empty()
            && nick == network.my_nick
        {
            chan.user_modes.clear();
            network.channels.remove(&chan.name);
            self.channels.remove(&key);
        }
    }

    /// Remove a quitting user from every roster on the network and from the
    /// registry. The channel-side rosters are authoritative; the user's own
    /// channel set is already cleared by the quit slot.
    fn cascade_user_quit(
        &mut self,
        state: &mut ClientState,
        key: &str,
        network_id: i32,
        nick: &str,
    ) {
        for chan in self.channels.values_mut() {
            if chan.network_id == network_id {
                chan.user_modes.remove(nick);
            }
        }
        if let Some(network) = state.networks.get_mut(&NetworkId(network_id)) {
            network.users.remove(nick);
        }
        self.users.remove(key);
    }

    fn clear_network_rosters(&mut self, state: &mut ClientState, network_id: NetworkId) {
        self.users.retain(|_, u| u.network_id != network_id.0);
        self.channels.retain(|_, c| c.network_id != network_id.0);
        if let Some(network) = state.networks.get_mut(&network_id) {
            network.users.clear();
            network.channels.clear();
        }
    }

    /// Apply pending BufferSyncer removals and renames, and report activity
    /// changes. Drains everything after any BufferSyncer slot, so a rename
    /// followed by a remove emits both, in order.
    fn drain_buffer_syncer(state: &mut ClientState, out: &mut Vec<ClientEvent>) {
        let Some(syncer) = state.buffer_syncer.as_mut() else {
            return;
        };
        let removed = std::mem::take(&mut syncer.removed_buffers);
        let renamed = std::mem::take(&mut syncer.renamed_buffers);
        let activity: Vec<(BufferId, u32, i64)> = std::mem::take(&mut syncer.activity_changed)
            .into_iter()
            .filter_map(|bid| {
                let types = syncer.activities.get(&bid).copied().unwrap_or(0);
                let highlights = syncer.highlight_counts.get(&bid).copied().unwrap_or(0);
                Some((BufferId(i32_id(bid)?), types, highlights))
            })
            .collect();
        for bid in removed {
            let Some(buffer_id) = i32_id(bid).map(BufferId) else {
                continue;
            };
            // A repeated removal (the core broadcasts to every client) is a
            // no-op rather than a double event.
            if state.buffers.remove(&buffer_id).is_some() {
                state.messages.remove(&buffer_id);
                state.backlog_requested.remove(&buffer_id);
                state.read_markers.remove(&buffer_id);
                out.push(ClientEvent::BufferRemoved { buffer_id });
            }
        }
        for (bid, name) in renamed {
            let Some(buffer_id) = i32_id(bid).map(BufferId) else {
                continue;
            };
            if let Some(info) = state.buffers.get_mut(&buffer_id) {
                info.name = name.clone();
            }
            out.push(ClientEvent::BufferRenamed { buffer_id, name });
        }
        for (buffer_id, types, highlights) in activity {
            out.push(ClientEvent::BufferActivity {
                buffer_id,
                types,
                highlights,
            });
        }
    }

    /// Merge a backlog reply into state.
    ///
    /// The buffer id comes from the slot parameter, not the payload, and
    /// messages for other buffers are dropped. A reply for a removed buffer
    /// is discarded (a late reply must not resurrect it) and its latch
    /// cleared. Every reply for a live buffer emits exactly one
    /// `BacklogReceived`, even an empty one.
    fn merge_backlog(&mut self, state: &mut ClientState, out: &mut Vec<ClientEvent>) {
        let raw_messages = std::mem::take(&mut self.backlog.last_received);
        let Some(raw_id) = self.backlog.last_buffer_id.take() else {
            return;
        };
        let Some(buffer_id) = raw_id.coerce_i64().and_then(i32_id).map(BufferId) else {
            tracing::warn!("receiveBacklog with malformed buffer_id {raw_id:?}");
            return;
        };
        if !state.buffers.contains_key(&buffer_id) {
            state.backlog_requested.remove(&buffer_id);
            tracing::debug!("dropping backlog for removed buffer {buffer_id}");
            return;
        }
        let existing = state.messages.entry(buffer_id).or_default();
        let mut seen: std::collections::HashSet<MsgId> =
            existing.iter().map(|m| m.msg_id).collect();
        let mut added = std::collections::HashSet::new();
        for raw in &raw_messages {
            if raw.buffer_info.buffer_id != buffer_id || !seen.insert(raw.msg_id) {
                continue;
            }
            existing.push(IrcMessage::from(raw));
            added.insert(raw.msg_id);
        }
        if !added.is_empty() {
            existing.sort_by_key(|m| m.msg_id);
        }
        state.enforce_cap(buffer_id);
        // Count only what survived the cap.
        let count = if added.is_empty() {
            0
        } else {
            state
                .messages_for_buffer(buffer_id)
                .iter()
                .filter(|m| added.contains(&m.msg_id))
                .count()
        };
        out.push(ClientEvent::BacklogReceived { buffer_id, count });
    }

    // -- InitData --------------------------------------------------------------

    pub fn handle_init_data(
        &mut self,
        state: &mut ClientState,
        msg: &InitData,
        out: &mut Vec<ClientEvent>,
    ) {
        match msg.class_name.as_slice() {
            network::CLASS_NAME => {
                let Some(network) = Self::known_network(state, &msg.object_name) else {
                    return;
                };
                network.apply_init_data(&msg.init_data);
                let network_id = NetworkId(network.network_id());
                let name = network.network_name.clone();
                // Moved out: the seeds are only needed to materialize the
                // roster, and keeping them would hold a second copy of it.
                let users_seed = std::mem::take(&mut network.users_seed);
                let channels_seed = std::mem::take(&mut network.channels_seed);
                // Materialize the roster. Objects that a Sync created before
                // this InitData are re-seeded in place.
                for (nick, fields) in &users_seed {
                    if let Some(user) = self.user_entry(state, &format!("{}/{nick}", network_id.0))
                    {
                        user.apply_init_data(fields);
                    }
                }
                for (channel, fields) in &channels_seed {
                    if let Some(chan) =
                        self.channel_entry(state, &format!("{}/{channel}", network_id.0))
                    {
                        chan.apply_init_data(fields);
                    }
                }
                // The name was usually unknown at session time.
                out.push(ClientEvent::NetworkUpdated {
                    network_id,
                    change: NetworkChange::Name(name),
                });
            }
            identity::CLASS_NAME => {
                let identity = Self::identity_entry(state, &msg.object_name);
                identity.apply_init_data(&msg.init_data);
                // Re-emitted on every (re-)init; consumers dedupe by id.
                out.push(ClientEvent::IdentityAdded {
                    identity_id: IdentityId(identity.identity_id),
                    name: identity.identity_name.clone(),
                });
            }
            buffer_syncer::CLASS_NAME => {
                let syncer = state.buffer_syncer.get_or_insert_with(BufferSyncer::new);
                syncer.apply_init_data(&msg.init_data);
                // Per-buffer state is only kept for buffers we know of.
                let buffers = &state.buffers;
                syncer.retain_buffers(|bid| {
                    i32_id(bid).is_some_and(|bid| buffers.contains_key(&BufferId(bid)))
                });
                // Adopt the core's persisted marker lines, without
                // clobbering a marker placed in this session. -1 means none.
                let markers: Vec<(i64, i64)> = syncer
                    .marker_lines_by_buffer
                    .iter()
                    .map(|(b, m)| (*b, *m))
                    .collect();
                for (bid, mid) in markers {
                    if mid < 0 {
                        continue;
                    }
                    if let Some(bid) = i32_id(bid) {
                        state
                            .read_markers
                            .entry(BufferId(bid))
                            .or_insert(MsgId(mid));
                    }
                }
                Self::drain_buffer_syncer(state, out);
            }
            irc_channel::CLASS_NAME => {
                if let Some(channel) = self.channel_entry(state, &msg.object_name) {
                    channel.apply_init_data(&msg.init_data);
                }
            }
            irc_user::CLASS_NAME => {
                if let Some(user) = self.user_entry(state, &msg.object_name) {
                    user.apply_init_data(&msg.init_data);
                }
            }
            other => tracing::debug!(
                "ignoring InitData for unknown class {:?}::{:?}",
                String::from_utf8_lossy(other),
                msg.object_name
            ),
        }
    }

    // -- RpcCall ---------------------------------------------------------------

    /// Handle the top-level signals we care about; everything else (e.g.
    /// client-to-core signals echoed back) is dropped.
    pub fn handle_rpc(
        &mut self,
        state: &mut ClientState,
        msg: &RpcCall,
        out: &mut Vec<ClientEvent>,
    ) {
        match msg.signal_name.as_slice() {
            DISPLAY_MSG_SIGNAL => match msg.params.first() {
                Some(Variant::User(UserValue::Message(raw))) => {
                    Self::store_message(state, raw, out)
                }
                Some(other) => {
                    tracing::warn!("displayMsg expected Message, got {}", other.type_name())
                }
                None => tracing::warn!("displayMsg with no payload"),
            },
            OBJECT_RENAMED_SIGNAL => self.handle_object_renamed(state, &msg.params),
            NETWORK_CREATED_SIGNAL => {
                let Some(nid) = msg.params.first().and_then(as_network_id) else {
                    tracing::warn!("networkCreated with malformed params: {:?}", msg.params);
                    return;
                };
                if state.networks.contains_key(&nid) {
                    return;
                }
                // Name and state arrive via the InitData the client requests
                // when it sees this NetworkAdded.
                state.networks.insert(nid, Network::new(nid.to_string()));
                out.push(ClientEvent::NetworkAdded {
                    network_id: nid,
                    name: String::new(),
                });
            }
            NETWORK_REMOVED_SIGNAL => {
                let Some(nid) = msg.params.first().and_then(as_network_id) else {
                    tracing::warn!("networkRemoved with malformed params: {:?}", msg.params);
                    return;
                };
                self.remove_network(state, nid, out);
            }
            other => tracing::debug!(
                "ignoring RpcCall {:?} with {} params",
                String::from_utf8_lossy(other),
                msg.params.len()
            ),
        }
    }

    /// Drop a network removed elsewhere, with its objects and buffers.
    fn remove_network(
        &mut self,
        state: &mut ClientState,
        nid: NetworkId,
        out: &mut Vec<ClientEvent>,
    ) {
        if state.networks.remove(&nid).is_none() {
            return;
        }
        self.users.retain(|_, u| u.network_id != nid.0);
        self.channels.retain(|_, c| c.network_id != nid.0);
        let doomed: Vec<BufferId> = state
            .buffers
            .values()
            .filter(|info| info.network_id == nid)
            .map(|info| info.buffer_id)
            .collect();
        for buffer_id in doomed {
            state.buffers.remove(&buffer_id);
            state.messages.remove(&buffer_id);
            state.backlog_requested.remove(&buffer_id);
            state.read_markers.remove(&buffer_id);
            if let Some(syncer) = state.buffer_syncer.as_mut() {
                syncer.drop_buffer(i64::from(buffer_id.0));
            }
            out.push(ClientEvent::BufferRemoved { buffer_id });
        }
        out.push(ClientEvent::NetworkRemoved { network_id: nid });
    }

    /// Re-key an object after `__objectRenamed__`. Without this a renamed
    /// user strands under its old key and later updates go astray.
    fn handle_object_renamed(&mut self, state: &mut ClientState, params: &[Variant]) {
        let [class, new, old, ..] = params else {
            tracing::warn!(
                "__objectRenamed__ with {} params (expected 3)",
                params.len()
            );
            return;
        };
        let class_name = match class {
            Variant::ByteArray(b) => b.clone(),
            Variant::String(s) => s.as_bytes().to_vec(),
            _ => {
                tracing::warn!("__objectRenamed__ with malformed params: {params:?}");
                return;
            }
        };
        let (Some(new_name), Some(old_name)) = (new.coerce_string(), old.coerce_string()) else {
            tracing::warn!("__objectRenamed__ with malformed params: {params:?}");
            return;
        };
        match class_name.as_slice() {
            irc_user::CLASS_NAME => match self.users.remove(&old_name) {
                Some(mut user) => {
                    let old_nick = user.nick.clone();
                    user.rename(&new_name);
                    let (network_id, new_nick) = (user.network_id, user.nick.clone());
                    self.users.insert(new_name, user);
                    self.rekey_rosters(state, network_id, &old_nick, &new_nick);
                }
                // A nick can sit in rosters without an IrcUser object ever
                // having been created; the rosters still need the re-key.
                None => {
                    let (old_net, old_nick) = crate::sync::split_object_name(&old_name);
                    let (new_net, new_nick) = crate::sync::split_object_name(&new_name);
                    if old_name.contains('/')
                        && new_name.contains('/')
                        && old_net == new_net
                        && old_net != -1
                        && !old_nick.is_empty()
                        && !new_nick.is_empty()
                    {
                        self.rekey_rosters(state, old_net, &old_nick, &new_nick);
                    }
                }
            },
            irc_channel::CLASS_NAME => {
                if let Some(mut channel) = self.channels.remove(&old_name) {
                    let old_channel = channel.name.clone();
                    channel.rename(&new_name);
                    // Keep the network's channel list (which drives "joined"
                    // in the sidebar) in step with the object.
                    if let Some(network) = state.networks.get_mut(&NetworkId(channel.network_id))
                        && network.channels.remove(&old_channel)
                    {
                        network.channels.insert(channel.name.clone());
                    }
                    self.channels.insert(new_name, channel);
                }
            }
            other => tracing::debug!(
                "__objectRenamed__ for unhandled {:?}::{old_name:?}",
                String::from_utf8_lossy(other)
            ),
        }
    }

    fn rekey_rosters(
        &mut self,
        state: &mut ClientState,
        network_id: i32,
        old_nick: &str,
        new_nick: &str,
    ) {
        if old_nick == new_nick {
            return;
        }
        if let Some(network) = state.networks.get_mut(&NetworkId(network_id))
            && network.users.remove(old_nick)
        {
            network.users.insert(new_nick.to_string());
        }
        for chan in self.channels.values_mut() {
            if chan.network_id == network_id
                && let Some(mode) = chan.user_modes.remove(old_nick)
            {
                chan.user_modes.insert(new_nick.to_string(), mode);
            }
        }
    }

    /// Store a live message and emit it, enforcing the retention cap.
    ///
    /// A buffer created mid-session (incoming query, /join) has no wire
    /// signal of its own; its first message carries the new BufferInfo.
    /// `BufferAdded` is emitted before `MessageReceived` so the UI has the
    /// buffer before the message is routed to it.
    fn store_message(state: &mut ClientState, raw: &Message, out: &mut Vec<ClientEvent>) {
        let info = &raw.buffer_info;
        let buffer_count = state.buffers.len();
        if let std::collections::btree_map::Entry::Vacant(slot) =
            state.buffers.entry(info.buffer_id)
        {
            if buffer_count >= MAX_BUFFERS {
                tracing::warn!(
                    "buffer cap reached; dropping message for new buffer {}",
                    info.buffer_id
                );
                return;
            }
            slot.insert(info.clone());
            out.push(ClientEvent::BufferAdded {
                buffer_id: info.buffer_id,
                network_id: info.network_id,
                name: info.name.clone(),
                kind: info.kind,
            });
        }
        let message = IrcMessage::from(raw);
        let list = state.messages.entry(info.buffer_id).or_default();
        // Keep the buffer sorted and duplicate-free, as backlog merges do:
        // the common case appends; a repeat is dropped; a late one is
        // inserted in place.
        match list.last() {
            Some(last) if last.msg_id >= message.msg_id => {
                match list.binary_search_by_key(&message.msg_id, |m| m.msg_id) {
                    Ok(_) => return,
                    Err(at) => list.insert(at, message.clone()),
                }
            }
            _ => list.push(message.clone()),
        }
        state.enforce_cap(info.buffer_id);
        out.push(ClientEvent::MessageReceived(message));
    }
}

#[cfg(test)]
mod tests;
