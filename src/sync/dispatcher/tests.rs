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
) -> SessionInit {
    SessionInit {
        identities,
        network_ids: network_ids.iter().map(|n| NetworkId(*n)).collect(),
        buffer_infos: buffers,
    }
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
    Dispatcher::new().seed_from_session(
        &mut state,
        &SessionInit::default(),
        Features::empty(),
        &mut out,
    );
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
fn sync_for_unseen_network_creates_it() {
    let mut h = Harness::new(&[], vec![]);
    h.sync(b"Network", "4", b"setMyNick", vec!["me".into()]);
    assert_eq!(h.names(), ["NetworkAdded", "NetworkUpdated"]);
    assert_eq!(h.state.networks[&NetworkId(4)].my_nick, "me");
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
    assert_eq!(h.dispatcher.user("1/seanr").unwrap().real_name, "Sean");
    assert_eq!(h.dispatcher.channel("1/#python").unwrap().topic, "hi");
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
    let alice = h.dispatcher.user("3/alice").unwrap();
    assert_eq!(
        (alice.user.as_str(), alice.host.as_str(), alice.away),
        ("al", "h1", false)
    );
    assert!(h.dispatcher.user("3/bob").unwrap().away);
    let chan = h.dispatcher.channel("3/#chan").unwrap();
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
    h.sync(b"BufferSyncer", "", b"removeBuffer", vec![bid(10)]);
    assert!(matches!(
        &h.take()[..],
        [ClientEvent::BufferRemoved {
            buffer_id: BufferId(10)
        }]
    ));
    assert!(!h.state.buffers.contains_key(&BufferId(10)));
    assert!(!h.state.messages.contains_key(&BufferId(10)));
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
    let mut fresh = Dispatcher::new();
    let s = session(&[1], vec![info], vec![]);
    fresh.seed_from_session(&mut h.state, &s, Features::empty(), &mut Vec::new());
    assert!(h.state.backlog_requested.is_empty());
    assert_eq!(h.state.messages[&BufferId(10)][0].contents, "kept");
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
    assert!(h.dispatcher.user("1/alice").is_none());
    let bob = h.dispatcher.user("1/bob").unwrap();
    assert_eq!((bob.nick.as_str(), bob.user.as_str()), ("bob", "al"));
    let chan = h.dispatcher.channel("1/#chan").unwrap();
    assert_eq!(
        chan.user_modes.iter().collect::<Vec<_>>(),
        [(&"bob".to_string(), &"@".to_string())]
    );
    let net = &h.state.networks[&NetworkId(1)];
    assert!(net.users.contains("bob") && !net.users.contains("alice"));

    h.sync(b"IrcUser", "1/bob", b"setAway", vec![Variant::Bool(true)]);
    let bob = h.dispatcher.user("1/bob").unwrap();
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
    let chan = h.dispatcher.channel("1/#chan").unwrap();
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
        h.dispatcher
            .channel("1/#chan")
            .unwrap()
            .user_modes
            .is_empty()
    );
    assert!(
        h.dispatcher
            .channel("1/#other")
            .unwrap()
            .user_modes
            .contains_key("alice")
    );

    h.sync(b"IrcUser", "1/alice", b"quit", vec![]);
    assert!(
        h.dispatcher
            .channel("1/#other")
            .unwrap()
            .user_modes
            .is_empty()
    );
    assert!(h.dispatcher.user("1/alice").is_none());
    assert!(!h.state.networks[&NetworkId(1)].users.contains("alice"));
}

#[test]
fn own_part_tears_down_the_channel() {
    let mut h = setup_user_in_channel();
    h.sync(b"Network", "1", b"setMyNick", vec!["alice".into()]);
    h.sync(b"Network", "1", b"addIrcChannel", vec!["#chan".into()]);
    h.sync(b"IrcUser", "1/alice", b"partChannel", vec!["#chan".into()]);
    assert!(h.dispatcher.channel("1/#chan").is_none());
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
    assert!(h.dispatcher.user("1/alice").is_none());
    assert!(h.dispatcher.channel("1/#chan").is_none());
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
    assert!(h.dispatcher.user("2/zed").is_none());
    assert!(h.state.buffers.contains_key(&BufferId(11)));
    h.rpc(NETWORK_REMOVED_SIGNAL, vec!["junk".into()]);
    assert!(h.take().is_empty());
}

#[test]
fn marker_lines_seed_without_clobbering_local_markers() {
    let mut h = Harness::new(&[1], vec![]);
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
