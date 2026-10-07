use chrono::{TimeZone, Utc};

use super::*;
use crate::protocol::types::{BufferInfo, BufferType, MessageFlags, MessageType};
use crate::qt::variant::VariantMap;

fn map(entries: Vec<(&str, Variant)>) -> VariantMap {
    entries
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect()
}

fn buffer(id: i32, net: i32, kind: BufferType, name: &str) -> BufferInfo {
    BufferInfo {
        buffer_id: BufferId(id),
        network_id: NetworkId(net),
        kind,
        group_id: 0,
        name: name.into(),
    }
}

fn session(
    network_ids: &[i32],
    buffers: Vec<BufferInfo>,
    identities: Vec<VariantMap>,
) -> Arc<SessionInit> {
    Arc::new(SessionInit {
        identities,
        network_ids: network_ids.iter().map(|n| NetworkId(*n)).collect(),
        buffer_infos: buffers,
    })
}

struct Harness {
    state: ClientState,
    dispatcher: Dispatcher,
    events: Vec<ClientEvent>,
}

impl Harness {
    fn new(network_ids: &[i32], buffers: Vec<BufferInfo>) -> Self {
        let mut h = Self {
            state: ClientState::new(0),
            dispatcher: Dispatcher::new(),
            events: Vec::new(),
        };
        let s = session(network_ids, buffers, vec![]);
        h.dispatcher
            .seed_from_session(&mut h.state, &s, Features::empty(), &mut h.events);
        h.events.clear();
        h
    }

    fn sync(&mut self, class: &[u8], object: &str, slot: &[u8], params: Vec<Variant>) {
        let msg = SyncMessage {
            class_name: class.to_vec(),
            object_name: object.into(),
            slot_name: slot.to_vec(),
            params,
        };
        self.dispatcher
            .handle_sync(&mut self.state, &msg, &mut self.events);
    }

    fn init(&mut self, class: &[u8], object: &str, data: VariantMap) {
        let msg = InitData {
            class_name: class.to_vec(),
            object_name: object.into(),
            init_data: data,
        };
        self.dispatcher
            .handle_init_data(&mut self.state, &msg, &mut self.events);
    }

    fn rpc(&mut self, signal: &[u8], params: Vec<Variant>) {
        let msg = RpcCall {
            signal_name: signal.to_vec(),
            params,
        };
        self.dispatcher
            .handle_rpc(&mut self.state, &msg, &mut self.events);
    }

    fn take(&mut self) -> Vec<ClientEvent> {
        std::mem::take(&mut self.events)
    }

    fn names(&mut self) -> Vec<&'static str> {
        self.take().iter().map(ClientEvent::name).collect()
    }
}

fn message(id: i64, info: &BufferInfo, contents: &str) -> Message {
    Message {
        msg_id: MsgId(id),
        timestamp: Utc.with_ymd_and_hms(2026, 4, 14, 12, 0, 0).unwrap(),
        kind: MessageType::Plain,
        flags: MessageFlags::NONE,
        buffer_info: info.clone(),
        sender: "nick!u@h".into(),
        sender_prefixes: String::new(),
        real_name: String::new(),
        avatar_url: String::new(),
        contents: contents.into(),
    }
}

fn msg_variant(m: Message) -> Variant {
    Variant::User(UserValue::Message(Box::new(m)))
}

fn bid(v: i32) -> Variant {
    Variant::User(UserValue::BufferId(BufferId(v)))
}

fn mid(v: i64) -> Variant {
    Variant::User(UserValue::MsgId(MsgId(v)))
}

#[test]
fn empty_session_still_emits_session_opened() {
    let mut state = ClientState::default();
    let mut out = Vec::new();
    Dispatcher::new().seed_from_session(&mut state, &Arc::default(), Features::empty(), &mut out);
    assert_eq!(out.len(), 1);
    assert!(matches!(out[0], ClientEvent::SessionOpened { .. }));
    assert!(state.buffer_syncer.is_some());
}

#[test]
fn seed_networks_buffers_identities() {
    let mut state = ClientState::default();
    let mut out = Vec::new();
    let identities = vec![
        map(vec![
            (
                "identityId",
                Variant::User(UserValue::IdentityId(IdentityId(1))),
            ),
            ("identityName", "default".into()),
            ("nicks", Variant::StringList(vec!["sean".into()])),
        ]),
        map(vec![
            ("identityId", Variant::Int(2)),
            ("identityName", "alt".into()),
        ]),
        map(vec![("identityName", "no id".into())]),
    ];
    let s = session(
        &[1, 2],
        vec![buffer(10, 1, BufferType::Channel, "#python")],
        identities,
    );
    Dispatcher::new().seed_from_session(&mut state, &s, Features::LONG_TIME, &mut out);
    let names: Vec<_> = out.iter().map(ClientEvent::name).collect();
    assert_eq!(
        names,
        [
            "SessionOpened",
            "NetworkAdded",
            "NetworkAdded",
            "BufferAdded",
            "IdentityAdded",
            "IdentityAdded"
        ]
    );
    assert_eq!(state.networks.len(), 2);
    assert_eq!(state.identities[&IdentityId(1)].nicks, ["sean"]);
    assert_eq!(state.identities[&IdentityId(2)].identity_name, "alt");
    assert_eq!(state.buffers[&BufferId(10)].name, "#python");
    assert!(state.messages[&BufferId(10)].is_empty());
    assert_eq!(state.peer_features, Features::LONG_TIME);
}

#[test]
fn set_network_name_emits_network_updated() {
    let mut h = Harness::new(&[1], vec![]);
    h.sync(b"Network", "1", b"setNetworkName", vec!["Libera".into()]);
    let events = h.take();
    assert!(matches!(
        &events[..],
        [ClientEvent::NetworkUpdated { network_id: NetworkId(1), change: NetworkChange::Name(n) }] if n == "Libera"
    ));
    assert_eq!(h.state.networks[&NetworkId(1)].network_name, "Libera");
    // Non-tracked slots mutate without an event.
    h.sync(b"Network", "1", b"setLatency", vec![Variant::Int(5)]);
    assert!(h.take().is_empty());
}

#[test]
fn unknown_class_is_dropped() {
    let mut h = Harness::new(&[], vec![]);
    h.sync(b"Mystery", "x", b"doThing", vec![]);
    h.init(b"Mystery", "x", VariantMap::new());
    assert!(h.take().is_empty());
}

#[test]
fn objects_on_unknown_networks_are_ignored() {
    let mut h = Harness::new(&[1], vec![]);
    h.sync(b"Network", "4", b"setMyNick", vec!["me".into()]);
    h.init(b"Network", "5", map(vec![("networkName", "x".into())]));
    h.sync(b"IrcUser", "4/bob", b"setAway", vec![Variant::Bool(true)]);
    h.sync(b"IrcChannel", "4/#c", b"setTopic", vec!["t".into()]);
    h.init(b"IrcUser", "nope", VariantMap::new());
    assert!(h.take().is_empty());
    assert_eq!(h.state.networks.len(), 1);
    assert!(!h.state.users.contains_key("4/bob"));
    assert!(!h.state.channels.contains_key("4/#c"));
    // On a known network they are created.
    h.sync(b"IrcUser", "1/bob", b"setAway", vec![Variant::Bool(true)]);
    assert!(h.state.users.get("1/bob").unwrap().away);
}

#[test]
fn buffer_syncer_ignores_unknown_buffers() {
    let mut h = Harness::new(&[1], vec![buffer(10, 1, BufferType::Channel, "#a")]);
    h.init(
        b"BufferSyncer",
        "",
        map(vec![(
            "LastSeenMsg",
            Variant::List(vec![bid(10), mid(1), bid(99), mid(2)]),
        )]),
    );
    h.sync(b"BufferSyncer", "", b"setMarkerLine", vec![bid(98), mid(3)]);
    h.sync(
        b"BufferSyncer",
        "",
        b"setBufferActivity",
        vec![bid(97), Variant::Int(1)],
    );
    let syncer = h.state.buffer_syncer.as_ref().unwrap();
    assert_eq!(syncer.last_seen_by_buffer.keys().collect::<Vec<_>>(), [&10]);
    assert!(syncer.marker_lines_by_buffer.is_empty());
    assert!(syncer.activities.is_empty());
    assert!(h.take().is_empty());
}

#[test]
fn channel_rename_updates_name_and_network_list() {
    let mut h = Harness::new(&[1], vec![]);
    h.sync(b"Network", "1", b"addIrcChannel", vec!["#old".into()]);
    h.sync(b"IrcChannel", "1/#old", b"setTopic", vec!["t".into()]);
    h.rpc(
        OBJECT_RENAMED_SIGNAL,
        vec![
            Variant::ByteArray(b"IrcChannel".to_vec()),
            "1/#new".into(),
            "1/#old".into(),
        ],
    );
    let chan = h.state.channels.get("1/#new").unwrap();
    assert_eq!(chan.name, "#new");
    assert_eq!(chan.topic, "t");
    let net = &h.state.networks[&NetworkId(1)];
    assert!(net.channels.contains("#new"));
    assert!(!net.channels.contains("#old"));
}

#[test]
fn network_init_data_emits_update_and_creates_children() {
    let mut h = Harness::new(&[1], vec![]);
    h.init(
        b"Network",
        "1",
        map(vec![
            ("networkName", "Libera".into()),
            (
                "IrcUsersAndChannels",
                Variant::Map(map(vec![
                    (
                        "Users",
                        Variant::Map(map(vec![(
                            "seanr",
                            Variant::Map(map(vec![
                                ("nick", "seanr".into()),
                                ("realName", "Sean".into()),
                            ])),
                        )])),
                    ),
                    (
                        "Channels",
                        Variant::Map(map(vec![(
                            "#python",
                            Variant::Map(map(vec![
                                ("name", "#python".into()),
                                ("topic", "hi".into()),
                            ])),
                        )])),
                    ),
                ])),
            ),
        ]),
    );
    let events = h.take();
    assert!(matches!(
        &events[..],
        [ClientEvent::NetworkUpdated { change: NetworkChange::Name(n), .. }] if n == "Libera"
    ));
    assert_eq!(h.state.users.get("1/seanr").unwrap().real_name, "Sean");
    assert_eq!(h.state.channels.get("1/#python").unwrap().topic, "hi");
}

#[test]
fn struct_of_arrays_seed() {
    let mut h = Harness::new(&[3], vec![]);
    let mut modes = VariantMap::new();
    modes.insert("alice".into(), "@".into());
    modes.insert("bob".into(), "".into());
    h.init(
        b"Network",
        "3",
        map(vec![
            ("networkName", "libera".into()),
            (
                "IrcUsersAndChannels",
                Variant::Map(map(vec![
                    (
                        "Users",
                        Variant::Map(map(vec![
                            ("nick", Variant::List(vec!["alice".into(), "bob".into()])),
                            ("user", Variant::List(vec!["al".into(), "bo".into()])),
                            ("host", Variant::List(vec!["h1".into(), "h2".into()])),
                            (
                                "away",
                                Variant::List(vec![Variant::Bool(false), Variant::Bool(true)]),
                            ),
                        ])),
                    ),
                    (
                        "Channels",
                        Variant::Map(map(vec![
                            ("name", Variant::List(vec!["#chan".into()])),
                            ("topic", Variant::List(vec!["greetings".into()])),
                            ("UserModes", Variant::List(vec![Variant::Map(modes)])),
                        ])),
                    ),
                ])),
            ),
        ]),
    );
    let alice = h.state.users.get("3/alice").unwrap();
    assert_eq!(
        (alice.user.as_str(), alice.host.as_str(), alice.away),
        ("al", "h1", false)
    );
    assert!(h.state.users.get("3/bob").unwrap().away);
    let chan = h.state.channels.get("3/#chan").unwrap();
    assert_eq!(chan.topic, "greetings");
    assert_eq!(chan.user_modes.len(), 2);
    assert_eq!(chan.user_modes["alice"], "@");
    let net = &h.state.networks[&NetworkId(3)];
    assert_eq!(net.users.iter().collect::<Vec<_>>(), ["alice", "bob"]);
    assert_eq!(net.channels.iter().collect::<Vec<_>>(), ["#chan"]);
}

#[test]
fn identity_init_data_re_emits_identity_added() {
    let mut h = Harness::new(&[], vec![]);
    h.init(b"Identity", "7", map(vec![("identityName", "work".into())]));
    let events = h.take();
    assert!(matches!(
        &events[..],
        [ClientEvent::IdentityAdded { identity_id: IdentityId(7), name }] if name == "work"
    ));
    assert_eq!(h.state.identities[&IdentityId(7)].identity_name, "work");
}

#[test]
fn display_msg_emits_message_received() {
    let info = buffer(10, 1, BufferType::Channel, "#python");
    let mut h = Harness::new(&[1], vec![info.clone()]);
    h.rpc(
        DISPLAY_MSG_SIGNAL,
        vec![msg_variant(message(1, &info, "hello"))],
    );
    let events = h.take();
    assert!(matches!(&events[..], [ClientEvent::MessageReceived(m)] if m.contents == "hello"));
    assert_eq!(h.state.messages[&BufferId(10)].len(), 1);
}

#[test]
fn malformed_and_other_rpcs_are_dropped() {
    let mut h = Harness::new(&[1], vec![]);
    h.rpc(b"2somethingElse()", vec![]);
    h.rpc(DISPLAY_MSG_SIGNAL, vec![]);
    h.rpc(DISPLAY_MSG_SIGNAL, vec![Variant::Int(5)]);
    assert!(h.take().is_empty());
}

#[test]
fn display_msg_for_unknown_buffer_emits_buffer_added_first() {
    let mut h = Harness::new(&[1], vec![]);
    let info = buffer(55, 1, BufferType::Query, "friend");
    h.rpc(
        DISPLAY_MSG_SIGNAL,
        vec![msg_variant(message(1, &info, "hi"))],
    );
    assert_eq!(h.names(), ["BufferAdded", "MessageReceived"]);
    assert_eq!(h.state.buffers[&BufferId(55)].name, "friend");
    h.rpc(
        DISPLAY_MSG_SIGNAL,
        vec![msg_variant(message(2, &info, "again"))],
    );
    assert_eq!(h.names(), ["MessageReceived"]);
}

#[test]
fn live_messages_stay_sorted_and_unique() {
    let info = buffer(10, 1, BufferType::Channel, "#a");
    let mut h = Harness::new(&[1], vec![info.clone()]);
    for id in [1, 3, 3, 2] {
        h.rpc(
            DISPLAY_MSG_SIGNAL,
            vec![msg_variant(message(id, &info, "x"))],
        );
    }
    // The repeat is dropped without an event; the late one is placed.
    assert_eq!(h.take().len(), 3);
    let ids: Vec<i64> = h.state.messages[&BufferId(10)]
        .iter()
        .map(|m| m.msg_id.0)
        .collect();
    assert_eq!(ids, [1, 2, 3]);
}

#[test]
fn retention_cap_drops_oldest() {
    let info = buffer(10, 1, BufferType::Channel, "#busy");
    let mut h = Harness::new(&[1], vec![info.clone()]);
    h.state.max_messages_per_buffer = 3;
    for i in 1..=5 {
        h.rpc(
            DISPLAY_MSG_SIGNAL,
            vec![msg_variant(message(i, &info, "x"))],
        );
    }
    let ids: Vec<i64> = h.state.messages[&BufferId(10)]
        .iter()
        .map(|m| m.msg_id.0)
        .collect();
    assert_eq!(ids, [3, 4, 5]);
}

#[test]
fn buffer_syncer_lifecycle() {
    let mut h = Harness::new(
        &[1],
        vec![
            buffer(10, 1, BufferType::Channel, "#a"),
            buffer(11, 1, BufferType::Channel, "#b"),
            buffer(12, 1, BufferType::Channel, "#c"),
            buffer(13, 1, BufferType::Channel, "#d"),
        ],
    );
    h.state.read_markers.insert(BufferId(10), MsgId(5));
    assert!(h.state.claim_backlog(BufferId(10)));
    h.sync(b"BufferSyncer", "", b"removeBuffer", vec![bid(10)]);
    assert!(matches!(
        &h.take()[..],
        [ClientEvent::BufferRemoved {
            buffer_id: BufferId(10)
        }]
    ));
    assert!(!h.state.buffers.contains_key(&BufferId(10)));
    assert!(!h.state.messages.contains_key(&BufferId(10)));
    assert!(!h.state.read_markers.contains_key(&BufferId(10)));
    assert!(!h.state.backlog_requested.contains(&BufferId(10)));
    // A repeated removal is a no-op.
    h.sync(b"BufferSyncer", "", b"removeBuffer", vec![bid(10)]);
    assert!(h.take().is_empty());

    h.sync(
        b"BufferSyncer",
        "",
        b"renameBuffer",
        vec![bid(11), "#renamed".into()],
    );
    assert!(matches!(
        &h.take()[..],
        [ClientEvent::BufferRenamed { buffer_id: BufferId(11), name }] if name == "#renamed"
    ));
    assert_eq!(h.state.buffers[&BufferId(11)].name, "#renamed");
    assert_eq!(h.state.buffers[&BufferId(11)].kind, BufferType::Channel);
    // Renaming a buffer we don't have is ignored.
    h.sync(
        b"BufferSyncer",
        "",
        b"renameBuffer",
        vec![bid(99), "#ghost".into()],
    );
    assert!(h.take().is_empty());
    assert!(!h.state.buffers.contains_key(&BufferId(99)));

    h.sync(
        b"BufferSyncer",
        "",
        b"mergeBuffersPermanently",
        vec![bid(12), bid(13)],
    );
    assert!(matches!(
        &h.take()[..],
        [ClientEvent::BufferRemoved {
            buffer_id: BufferId(13)
        }]
    ));
    assert!(h.state.buffers.contains_key(&BufferId(12)));
}

fn backlog_params(buffer: Variant, messages: Variant) -> Vec<Variant> {
    vec![
        buffer,
        mid(-1),
        mid(-1),
        Variant::Int(100),
        Variant::Int(0),
        messages,
    ]
}

#[test]
fn backlog_count_is_what_survives_the_cap() {
    let info = buffer(10, 1, BufferType::Channel, "#busy");
    let mut h = Harness::new(&[1], vec![info.clone()]);
    h.state.max_messages_per_buffer = 3;
    for i in 10..=11 {
        h.rpc(
            DISPLAY_MSG_SIGNAL,
            vec![msg_variant(message(i, &info, "live"))],
        );
    }
    h.take();
    // Three older messages arrive; only one fits.
    h.sync(
        b"BacklogManager",
        "",
        b"receiveBacklog",
        backlog_params(
            bid(10),
            Variant::List(
                (1..=3)
                    .map(|i| msg_variant(message(i, &info, "old")))
                    .collect(),
            ),
        ),
    );
    assert!(matches!(
        &h.take()[..],
        [ClientEvent::BacklogReceived {
            buffer_id: BufferId(10),
            count: 1
        }]
    ));
    let ids: Vec<i64> = h.state.messages[&BufferId(10)]
        .iter()
        .map(|m| m.msg_id.0)
        .collect();
    assert_eq!(ids, [3, 10, 11]);
}

#[test]
fn backlog_merge_dedupes_and_sorts() {
    let info = buffer(10, 1, BufferType::Channel, "#python");
    let other = buffer(11, 1, BufferType::Channel, "#other");
    let mut h = Harness::new(&[1], vec![info.clone(), other.clone()]);
    h.rpc(
        DISPLAY_MSG_SIGNAL,
        vec![msg_variant(message(5, &info, "live"))],
    );
    h.take();
    h.sync(
        b"BacklogManager",
        "",
        b"receiveBacklog",
        backlog_params(
            bid(10),
            Variant::List(vec![
                msg_variant(message(3, &info, "old")),
                msg_variant(message(5, &info, "dup of live")),
                msg_variant(message(3, &info, "dup in batch")),
                msg_variant(message(4, &info, "older")),
                msg_variant(message(9, &other, "wrong buffer")),
                Variant::Int(7),
            ]),
        ),
    );
    assert!(matches!(
        &h.take()[..],
        [ClientEvent::BacklogReceived {
            buffer_id: BufferId(10),
            count: 2
        }]
    ));
    let msgs = &h.state.messages[&BufferId(10)];
    let ids: Vec<i64> = msgs.iter().map(|m| m.msg_id.0).collect();
    assert_eq!(ids, [3, 4, 5]);
    assert_eq!(msgs[2].contents, "live");
    assert!(h.state.messages[&BufferId(11)].is_empty());
}

#[test]
fn backlog_edge_cases() {
    let info = buffer(10, 1, BufferType::Channel, "#python");
    let mut h = Harness::new(&[1], vec![info.clone()]);

    // Empty reply still completes.
    h.sync(
        b"BacklogManager",
        "",
        b"receiveBacklog",
        backlog_params(bid(10), Variant::List(vec![])),
    );
    assert!(matches!(
        &h.take()[..],
        [ClientEvent::BacklogReceived { count: 0, .. }]
    ));

    // A malformed payload still completes with zero.
    h.sync(
        b"BacklogManager",
        "",
        b"receiveBacklog",
        backlog_params(bid(10), "junk".into()),
    );
    assert!(matches!(
        &h.take()[..],
        [ClientEvent::BacklogReceived { count: 0, .. }]
    ));

    // A malformed buffer id is dropped without an event.
    h.sync(
        b"BacklogManager",
        "",
        b"receiveBacklog",
        backlog_params(Variant::Map(VariantMap::new()), Variant::List(vec![])),
    );
    assert!(h.take().is_empty());

    // A reply for a removed buffer is dropped and clears the latch.
    h.state.claim_backlog(BufferId(99));
    let gone = buffer(99, 1, BufferType::Channel, "#gone");
    h.sync(
        b"BacklogManager",
        "",
        b"receiveBacklog",
        backlog_params(
            bid(99),
            Variant::List(vec![msg_variant(message(1, &gone, "x"))]),
        ),
    );
    assert!(h.take().is_empty());
    assert!(!h.state.buffers.contains_key(&BufferId(99)));
    assert!(!h.state.backlog_requested.contains(&BufferId(99)));
}

#[test]
fn reseed_clears_latches_and_keeps_history() {
    let info = buffer(10, 1, BufferType::Channel, "#python");
    let mut h = Harness::new(&[1], vec![info.clone()]);
    h.rpc(
        DISPLAY_MSG_SIGNAL,
        vec![msg_variant(message(1, &info, "kept"))],
    );
    h.state.claim_backlog(BufferId(10));
    h.sync(b"IrcChannel", "1/#python", b"setTopic", vec!["old".into()]);
    h.sync(b"IrcUser", "1/ghost", b"setAway", vec![Variant::Bool(true)]);
    assert!(!h.state.channels.is_empty() && !h.state.users.is_empty());
    let mut fresh = Dispatcher::new();
    let s = session(&[1], vec![info], vec![]);
    fresh.seed_from_session(&mut h.state, &s, Features::empty(), &mut Vec::new());
    assert!(h.state.backlog_requested.is_empty());
    assert_eq!(h.state.messages[&BufferId(10)][0].contents, "kept");
    // Rosters belong to a session; the new one re-sends them.
    assert!(h.state.channels.is_empty() && h.state.users.is_empty());
}

fn setup_user_in_channel() -> Harness {
    let mut h = Harness::new(&[1], vec![]);
    h.sync(b"IrcUser", "1/alice", b"setUser", vec!["al".into()]);
    h.sync(b"Network", "1", b"addIrcUser", vec!["alice!al@host".into()]);
    h.sync(
        b"IrcChannel",
        "1/#chan",
        b"joinIrcUsers",
        vec![
            Variant::List(vec!["alice".into()]),
            Variant::List(vec!["@".into()]),
        ],
    );
    h.take();
    h
}

#[test]
fn nick_change_rekeys_object_and_rosters() {
    let mut h = setup_user_in_channel();
    h.rpc(
        OBJECT_RENAMED_SIGNAL,
        vec![
            Variant::ByteArray(b"IrcUser".to_vec()),
            "1/bob".into(),
            "1/alice".into(),
        ],
    );
    assert!(!h.state.users.contains_key("1/alice"));
    let bob = h.state.users.get("1/bob").unwrap();
    assert_eq!((bob.nick.as_str(), bob.user.as_str()), ("bob", "al"));
    let chan = h.state.channels.get("1/#chan").unwrap();
    assert_eq!(
        chan.user_modes.iter().collect::<Vec<_>>(),
        [(&"bob".to_string(), &"@".to_string())]
    );
    let net = &h.state.networks[&NetworkId(1)];
    assert!(net.users.contains("bob") && !net.users.contains("alice"));

    h.sync(b"IrcUser", "1/bob", b"setAway", vec![Variant::Bool(true)]);
    let bob = h.state.users.get("1/bob").unwrap();
    assert!(bob.away);
    assert_eq!(bob.user, "al");
}

#[test]
fn rename_for_uninstantiated_user_still_rekeys_rosters() {
    let mut h = Harness::new(&[1], vec![]);
    h.sync(b"Network", "1", b"addIrcUser", vec!["carol!c@h".into()]);
    h.sync(
        b"IrcChannel",
        "1/#chan",
        b"joinIrcUsers",
        vec![
            Variant::List(vec!["carol".into()]),
            Variant::List(vec!["+".into()]),
        ],
    );
    h.rpc(
        OBJECT_RENAMED_SIGNAL,
        vec!["IrcUser".into(), "1/dave".into(), "1/carol".into()],
    );
    let chan = h.state.channels.get("1/#chan").unwrap();
    assert_eq!(chan.user_modes.get("dave").map(String::as_str), Some("+"));
    assert!(!chan.user_modes.contains_key("carol"));
    assert!(h.state.networks[&NetworkId(1)].users.contains("dave"));
}

#[test]
fn part_and_quit_cascade() {
    let mut h = setup_user_in_channel();
    h.sync(
        b"IrcChannel",
        "1/#other",
        b"joinIrcUsers",
        vec![Variant::List(vec!["alice".into()]), Variant::List(vec![])],
    );
    h.sync(b"IrcUser", "1/alice", b"partChannel", vec!["#chan".into()]);
    assert!(
        h.state
            .channels
            .get("1/#chan")
            .unwrap()
            .user_modes
            .is_empty()
    );
    assert!(
        h.state
            .channels
            .get("1/#other")
            .unwrap()
            .user_modes
            .contains_key("alice")
    );

    h.sync(b"IrcUser", "1/alice", b"quit", vec![]);
    assert!(
        h.state
            .channels
            .get("1/#other")
            .unwrap()
            .user_modes
            .is_empty()
    );
    assert!(!h.state.users.contains_key("1/alice"));
    assert!(!h.state.networks[&NetworkId(1)].users.contains("alice"));
}

#[test]
fn own_part_tears_down_the_channel() {
    let mut h = setup_user_in_channel();
    h.sync(b"Network", "1", b"setMyNick", vec!["alice".into()]);
    h.sync(b"Network", "1", b"addIrcChannel", vec!["#chan".into()]);
    h.sync(b"IrcUser", "1/alice", b"partChannel", vec!["#chan".into()]);
    assert!(!h.state.channels.contains_key("1/#chan"));
    assert!(!h.state.networks[&NetworkId(1)].channels.contains("#chan"));
}

#[test]
fn network_disconnect_clears_rosters() {
    let mut h = setup_user_in_channel();
    h.sync(b"Network", "1", b"setConnected", vec![Variant::Bool(false)]);
    let events = h.take();
    assert!(matches!(
        &events[..],
        [ClientEvent::NetworkUpdated {
            change: NetworkChange::Connected(false),
            ..
        }]
    ));
    assert!(!h.state.users.contains_key("1/alice"));
    assert!(!h.state.channels.contains_key("1/#chan"));
    assert!(h.state.networks[&NetworkId(1)].users.is_empty());
}

#[test]
fn network_lifecycle_rpcs() {
    let info = buffer(10, 2, BufferType::Channel, "#gone");
    let keep = buffer(11, 1, BufferType::Channel, "#keep");
    let mut h = Harness::new(&[1], vec![keep]);
    h.rpc(
        NETWORK_CREATED_SIGNAL,
        vec![Variant::User(UserValue::NetworkId(NetworkId(2)))],
    );
    assert!(matches!(
        &h.take()[..],
        [ClientEvent::NetworkAdded {
            network_id: NetworkId(2),
            ..
        }]
    ));
    h.rpc(NETWORK_CREATED_SIGNAL, vec![Variant::Int(2)]);
    assert!(h.take().is_empty());

    h.rpc(
        DISPLAY_MSG_SIGNAL,
        vec![msg_variant(message(1, &info, "x"))],
    );
    h.sync(b"IrcUser", "2/zed", b"setUser", vec!["z".into()]);
    h.state.read_markers.insert(BufferId(10), MsgId(1));
    h.take();
    h.rpc(NETWORK_REMOVED_SIGNAL, vec![Variant::Int(2)]);
    assert_eq!(h.names(), ["BufferRemoved", "NetworkRemoved"]);
    assert!(!h.state.networks.contains_key(&NetworkId(2)));
    assert!(!h.state.buffers.contains_key(&BufferId(10)));
    assert!(!h.state.read_markers.contains_key(&BufferId(10)));
    assert!(!h.state.users.contains_key("2/zed"));
    assert!(h.state.buffers.contains_key(&BufferId(11)));
    h.rpc(NETWORK_REMOVED_SIGNAL, vec!["junk".into()]);
    assert!(h.take().is_empty());
}

#[test]
fn marker_lines_seed_without_clobbering_local_markers() {
    let mut h = Harness::new(
        &[1],
        (1..=3)
            .map(|i| buffer(i, 1, BufferType::Channel, &format!("#{i}")))
            .collect(),
    );
    h.state.read_markers.insert(BufferId(2), MsgId(222));
    let mut markers = VariantMap::new();
    markers.insert("1".into(), mid(100));
    markers.insert("2".into(), mid(200));
    markers.insert("3".into(), mid(-1));
    h.init(
        b"BufferSyncer",
        "",
        map(vec![("MarkerLines", Variant::Map(markers))]),
    );
    assert_eq!(h.state.read_markers[&BufferId(1)], MsgId(100));
    assert_eq!(h.state.read_markers[&BufferId(2)], MsgId(222));
    assert!(!h.state.read_markers.contains_key(&BufferId(3)));
}

#[test]
fn buffer_activity_reported_on_init_and_sync() {
    let mut h = Harness::new(
        &[1],
        vec![
            buffer(1, 1, BufferType::Channel, "#a"),
            buffer(2, 1, BufferType::Channel, "#b"),
        ],
    );
    h.init(
        b"BufferSyncer",
        "",
        map(vec![
            (
                "Activities",
                Variant::List(vec![bid(1), Variant::Int(0x1), bid(2), Variant::Int(0x20)]),
            ),
            (
                "HighlightCounts",
                Variant::List(vec![bid(1), Variant::Int(3)]),
            ),
        ]),
    );
    let events = h.take();
    assert!(matches!(
        events.as_slice(),
        [
            ClientEvent::BufferActivity {
                buffer_id: BufferId(1),
                types: 0x1,
                highlights: 3
            },
            ClientEvent::BufferActivity {
                buffer_id: BufferId(2),
                types: 0x20,
                highlights: 0
            },
        ]
    ));
    // Another client read buffer 1.
    h.sync(
        b"BufferSyncer",
        "",
        b"setBufferActivity",
        vec![bid(1), Variant::Int(0)],
    );
    h.sync(
        b"BufferSyncer",
        "",
        b"setHighlightCount",
        vec![bid(1), Variant::Int(0)],
    );
    let events = h.take();
    assert!(matches!(
        events.as_slice(),
        [
            ClientEvent::BufferActivity {
                buffer_id: BufferId(1),
                types: 0,
                highlights: 3
            },
            ClientEvent::BufferActivity {
                buffer_id: BufferId(1),
                types: 0,
                highlights: 0
            },
        ]
    ));
}

#[test]
fn a_late_message_trimmed_by_the_cap_is_not_announced() {
    let info = buffer(10, 1, BufferType::Channel, "#python");
    let mut h = Harness::new(&[1], vec![info.clone()]);
    h.state.max_messages_per_buffer = 3;
    for id in 10..=12 {
        h.rpc(
            DISPLAY_MSG_SIGNAL,
            vec![msg_variant(message(id, &info, "x"))],
        );
    }
    h.take();
    // Older than everything kept: stored and trimmed at once.
    h.rpc(
        DISPLAY_MSG_SIGNAL,
        vec![msg_variant(message(5, &info, "late"))],
    );
    assert!(h.take().is_empty());
    let ids: Vec<_> = h.state.messages[&BufferId(10)]
        .iter()
        .map(|m| m.msg_id.0)
        .collect();
    assert_eq!(ids, [10, 11, 12]);
    // A late one that fits in the middle still counts.
    h.state.max_messages_per_buffer = 4;
    h.rpc(
        DISPLAY_MSG_SIGNAL,
        vec![msg_variant(message(11, &info, "dup"))],
    );
    assert!(h.take().is_empty(), "a repeat is dropped");
}

#[test]
fn seeding_respects_the_buffer_cap() {
    // A SessionInit built in code skips from_map's cap; seeding has its own.
    let buffers = (0..MAX_BUFFERS as i32 + 2)
        .map(|i| buffer(i, 1, BufferType::Channel, "#c"))
        .collect();
    let h = Harness::new(&[1], buffers);
    assert_eq!(h.state.buffers.len(), MAX_BUFFERS);
}

#[test]
fn reseed_drops_what_the_core_deleted_meanwhile() {
    let kept = buffer(10, 1, BufferType::Channel, "#kept");
    let gone = buffer(11, 1, BufferType::Channel, "#gone");
    let on_gone_net = buffer(20, 2, BufferType::Channel, "#elsewhere");
    let mut h = Harness::new(&[1, 2], vec![kept.clone(), gone.clone(), on_gone_net]);
    h.rpc(
        DISPLAY_MSG_SIGNAL,
        vec![msg_variant(message(1, &gone, "x"))],
    );
    h.init(b"Identity", "7", VariantMap::new());
    assert!(h.state.identities.contains_key(&IdentityId(7)));
    h.take();

    // Reconnect: network 2, buffer 11 and identity 7 were deleted.
    let s = session(&[1], vec![kept], vec![]);
    let mut events = Vec::new();
    Dispatcher::new().seed_from_session(&mut h.state, &s, Features::empty(), &mut events);
    assert_eq!(h.state.buffers.keys().collect::<Vec<_>>(), [&BufferId(10)]);
    assert!(!h.state.messages.contains_key(&BufferId(11)));
    assert!(!h.state.networks.contains_key(&NetworkId(2)));
    assert!(h.state.identities.is_empty());
    // The UI hears about it before the session opens.
    let names: Vec<_> = events.iter().map(ClientEvent::name).collect();
    let opened = names.iter().position(|n| *n == "SessionOpened").unwrap();
    let removed: Vec<_> = names[..opened]
        .iter()
        .filter(|n| n.ends_with("Removed"))
        .collect();
    assert_eq!(
        removed,
        [&"BufferRemoved", &"NetworkRemoved", &"BufferRemoved"]
    );
}

#[test]
fn channel_memberships_share_one_budget() {
    let mut h = Harness::new(&[1], vec![]);
    // One huge channel uses up all but one membership.
    let mut big = IrcChannel::new("1/#big");
    big.user_modes = (0..MAX_MEMBERSHIPS - 1)
        .map(|i| (format!("n{i}"), String::new()))
        .collect();
    h.state.channels.insert(big.object_name.clone(), big);
    h.state.recount_memberships();
    let join = |nicks: &[&str]| {
        vec![
            Variant::StringList(nicks.iter().map(|n| (*n).into()).collect()),
            Variant::Null,
        ]
    };
    h.sync(
        b"IrcChannel",
        "1/#small",
        b"joinIrcUsers",
        join(&["a", "b", "c"]),
    );
    assert_eq!(h.state.channels["1/#small"].user_modes.len(), 1);
    // Leaving frees room again.
    h.sync(b"IrcChannel", "1/#big", b"part", vec!["n0".into()]);
    h.sync(b"IrcChannel", "1/#small", b"joinIrcUsers", join(&["b"]));
    assert_eq!(h.state.channels["1/#small"].user_modes.len(), 2);
}

#[test]
fn markers_moved_elsewhere_move_here() {
    let info = buffer(10, 1, BufferType::Channel, "#python");
    let mut h = Harness::new(&[1], vec![info]);
    let marker = |bid: i32, mid: i64| {
        vec![
            Variant::User(UserValue::BufferId(BufferId(bid))),
            Variant::User(UserValue::MsgId(MsgId(mid))),
        ]
    };
    h.sync(b"BufferSyncer", "", b"setMarkerLine", marker(10, 42));
    assert_eq!(h.state.read_markers[&BufferId(10)], MsgId(42));
    // Backwards too: the core is the authority.
    h.sync(b"BufferSyncer", "", b"setMarkerLine", marker(10, 7));
    assert_eq!(h.state.read_markers[&BufferId(10)], MsgId(7));
    h.sync(b"BufferSyncer", "", b"setMarkerLine", marker(10, -1));
    assert!(!h.state.read_markers.contains_key(&BufferId(10)));
    // Unknown buffers are ignored.
    h.sync(b"BufferSyncer", "", b"setMarkerLine", marker(99, 5));
    assert!(!h.state.read_markers.contains_key(&BufferId(99)));
}

#[test]
fn messages_dont_create_buffers_on_unknown_networks() {
    let mut h = Harness::new(&[1], vec![]);
    let stray = buffer(50, 9, BufferType::Query, "stranger");
    h.rpc(
        DISPLAY_MSG_SIGNAL,
        vec![msg_variant(message(1, &stray, "hi"))],
    );
    assert!(h.take().is_empty());
    assert!(!h.state.buffers.contains_key(&BufferId(50)));
}

#[test]
fn oversized_message_fields_are_cut() {
    let info = buffer(10, 1, BufferType::Channel, "#python");
    let mut h = Harness::new(&[1], vec![info.clone()]);
    let mut big = message(1, &info, &"x".repeat(1 << 20));
    big.sender = "n".repeat(1 << 16);
    h.rpc(DISPLAY_MSG_SIGNAL, vec![msg_variant(big)]);
    let kept = &h.state.messages[&BufferId(10)][0];
    assert!(kept.contents.len() <= crate::sync::events::MAX_CONTENTS_BYTES);
    assert!(kept.sender.len() <= crate::sync::events::MAX_SENDER_BYTES);
    assert!(kept.contents.ends_with('…'));
}

#[test]
fn the_membership_count_stays_in_step() {
    let mut h = Harness::new(&[1], vec![]);
    let check = |h: &mut Harness| {
        let running = h.state.memberships;
        h.state.recount_memberships();
        assert_eq!(running, h.state.memberships);
    };
    let join = |nicks: &[&str]| {
        vec![
            Variant::StringList(nicks.iter().map(|n| (*n).into()).collect()),
            Variant::Null,
        ]
    };
    h.state.networks.get_mut(&NetworkId(1)).unwrap().my_nick = "me".into();
    h.sync(
        b"IrcChannel",
        "1/#a",
        b"joinIrcUsers",
        join(&["me", "alice", "bob"]),
    );
    h.sync(
        b"IrcChannel",
        "1/#b",
        b"joinIrcUsers",
        join(&["me", "alice", "carol"]),
    );
    check(&mut h);
    // A part, a quit, and a nick change onto a nick already in #b.
    h.sync(b"IrcUser", "1/bob", b"partChannel", vec!["#a".into()]);
    h.sync(b"IrcUser", "1/carol", b"quit", vec![]);
    check(&mut h);
    let renamed = |old: &str, new: &str| {
        vec![
            Variant::ByteArray(b"IrcUser".to_vec()),
            new.into(),
            old.into(),
        ]
    };
    h.sync(b"IrcChannel", "1/#b", b"joinIrcUsers", join(&["dave"]));
    h.rpc(OBJECT_RENAMED_SIGNAL, renamed("1/dave", "1/alice"));
    check(&mut h);
    // A snapshot replaces a roster; our own part tears a channel down.
    let mut modes = VariantMap::new();
    modes.insert("x".into(), Variant::Null);
    h.init(
        b"IrcChannel",
        "1/#a",
        map(vec![("UserModes", Variant::Map(modes))]),
    );
    check(&mut h);
    h.sync(b"IrcUser", "1/me", b"partChannel", vec!["#b".into()]);
    check(&mut h);
    assert_eq!(h.state.memberships, 1);
}

#[test]
fn networks_and_identities_are_capped() {
    let mut h = Harness::new(&[], vec![]);
    for i in 0..MAX_NETWORKS as i32 + 5 {
        h.rpc(NETWORK_CREATED_SIGNAL, vec![Variant::Int(i)]);
    }
    assert_eq!(h.state.networks.len(), MAX_NETWORKS);
    for i in 0..MAX_IDENTITIES + 5 {
        h.init(b"Identity", &i.to_string(), VariantMap::new());
    }
    assert_eq!(h.state.identities.len(), MAX_IDENTITIES);
    // Existing ones still update at the cap.
    h.init(b"Identity", "0", map(vec![("identityName", "home".into())]));
    assert_eq!(h.state.identities[&IdentityId(0)].identity_name, "home");
}
