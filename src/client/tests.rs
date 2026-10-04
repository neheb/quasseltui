use chrono::{TimeZone, Utc};

use super::*;
use crate::protocol::features::Features;
use crate::protocol::signalproxy::decode_signalproxy_payload;
use crate::protocol::testing::*;
use crate::protocol::types::{BufferType, Message, MessageFlags, MessageType};
use crate::qt::variant::VariantMap;

fn buffer(id: i32, net: i32, name: &str) -> BufferInfo {
    BufferInfo {
        buffer_id: BufferId(id),
        network_id: NetworkId(net),
        kind: BufferType::Channel,
        group_id: 0,
        name: name.into(),
    }
}

fn client_for(fake: &Fake) -> QuasselClient {
    QuasselClient::from_connection(QuasselConnection::with_connector(
        options(false),
        fake.connector.clone(),
    ))
}

async fn drain(client: &mut QuasselClient, state: &mut ClientState) -> Vec<ClientEvent> {
    let mut events = Vec::new();
    while let Some(event) = client.next_event(state).await {
        events.push(event);
    }
    events
}

async fn written(fake: Fake) -> Vec<SignalProxyMessage> {
    // Skip ClientInit and ClientLogin.
    split_frames(&fake.written.await.unwrap())[2..]
        .iter()
        .map(|f| decode_signalproxy_payload(f, modern()).unwrap())
        .collect()
}

fn init_request(class: &[u8], object: &str) -> SignalProxyMessage {
    SignalProxyMessage::InitRequest(InitRequest {
        class_name: class.to_vec(),
        object_name: object.into(),
    })
}

/// Run until the first event satisfying `pred`, keeping everything seen.
async fn until(
    client: &mut QuasselClient,
    state: &mut ClientState,
    pred: impl Fn(&ClientEvent) -> bool,
) -> Vec<ClientEvent> {
    let mut seen = Vec::new();
    while let Some(event) = client.next_event(state).await {
        let done = pred(&event);
        seen.push(event);
        if done {
            break;
        }
    }
    seen
}

#[tokio::test]
async fn session_ready_fans_out_init_requests() {
    let fake = fake_core(
        inbound_with_session(session_with(&[1, 2], &[buffer(10, 1, "#python")]), &[]),
        false,
        true,
    );
    let mut client = client_for(&fake);
    let mut state = ClientState::default();
    let seen = until(&mut client, &mut state, |e| {
        matches!(e, ClientEvent::BufferAdded { .. })
    })
    .await;
    let names: Vec<_> = seen.iter().map(ClientEvent::name).collect();
    assert_eq!(
        names,
        [
            "SessionOpened",
            "NetworkAdded",
            "NetworkAdded",
            "BufferAdded"
        ]
    );
    assert_eq!(state.buffers[&BufferId(10)].name, "#python");

    client.close();
    let rest = drain(&mut client, &mut state).await;
    assert!(matches!(
        rest.last(),
        Some(ClientEvent::Disconnected { .. })
    ));
    assert!(client.next_event(&mut state).await.is_none());
    assert_eq!(
        written(fake).await,
        [
            init_request(b"BufferSyncer", ""),
            init_request(b"Network", "1"),
            init_request(b"Network", "2"),
        ]
    );
}

#[tokio::test]
async fn disconnected_is_terminal() {
    let fake = fake_core(
        inbound_with_session(session_with(&[], &[]), &[]),
        false,
        false,
    );
    let mut client = client_for(&fake);
    let mut state = ClientState::default();
    let events = drain(&mut client, &mut state).await;
    assert!(matches!(
        events.first(),
        Some(ClientEvent::SessionOpened { .. })
    ));
    assert!(matches!(
        events.last(),
        Some(ClientEvent::Disconnected { .. })
    ));
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, ClientEvent::Disconnected { .. }))
            .count(),
        1
    );
}

#[tokio::test]
async fn protocol_events_mutate_state() {
    let info = buffer(10, 1, "#python");
    let msg = Message {
        msg_id: MsgId(7),
        timestamp: Utc.with_ymd_and_hms(2026, 4, 14, 12, 0, 0).unwrap(),
        kind: MessageType::Plain,
        flags: MessageFlags::NONE,
        buffer_info: info.clone(),
        sender: "guido!g@h".into(),
        sender_prefixes: String::new(),
        real_name: String::new(),
        avatar_url: String::new(),
        contents: "hello".into(),
    };
    let mut init = VariantMap::new();
    init.insert("networkName".into(), "Libera".into());
    let frames = vec![
        framed_sp(&SignalProxyMessage::Sync(SyncMessage {
            class_name: b"Network".to_vec(),
            object_name: "1".into(),
            slot_name: b"setMyNick".to_vec(),
            params: vec!["seanr".into()],
        })),
        framed_sp(&SignalProxyMessage::InitData(
            crate::protocol::signalproxy::InitData {
                class_name: b"Network".to_vec(),
                object_name: "1".into(),
                init_data: init,
            },
        )),
        framed_sp(&SignalProxyMessage::RpcCall(RpcCall {
            signal_name: crate::sync::dispatcher::DISPLAY_MSG_SIGNAL.to_vec(),
            params: vec![Variant::User(UserValue::Message(Box::new(msg)))],
        })),
        framed_sp(&SignalProxyMessage::HeartBeat(
            crate::qt::QDateTime::now_utc(),
        )),
    ];
    let fake = fake_core(
        inbound_with_session(session_with(&[1], &[info]), &frames),
        false,
        false,
    );
    let mut client = client_for(&fake);
    let mut state = ClientState::default();
    let events = drain(&mut client, &mut state).await;
    let names: Vec<_> = events.iter().map(ClientEvent::name).collect();
    assert_eq!(
        names,
        [
            "SessionOpened",
            "NetworkAdded",
            "BufferAdded",
            "NetworkUpdated",
            "NetworkUpdated",
            "MessageReceived",
            "Disconnected"
        ]
    );
    let network = &state.networks[&NetworkId(1)];
    assert_eq!(
        (network.my_nick.as_str(), network.network_name.as_str()),
        ("seanr", "Libera")
    );
    assert_eq!(state.messages[&BufferId(10)][0].contents, "hello");
    assert!(state.peer_features.contains(Features::LONG_TIME));
}

#[tokio::test]
async fn network_created_mid_session_gets_one_init_request() {
    let created = |n: i32| {
        framed_sp(&SignalProxyMessage::RpcCall(RpcCall {
            signal_name: crate::sync::dispatcher::NETWORK_CREATED_SIGNAL.to_vec(),
            params: vec![Variant::User(UserValue::NetworkId(NetworkId(n)))],
        }))
    };
    let fake = fake_core(
        inbound_with_session(
            session_with(&[1], &[]),
            &[created(5), created(5), created(1), created(6)],
        ),
        false,
        true,
    );
    let mut client = client_for(&fake);
    let mut state = ClientState::default();
    until(&mut client, &mut state, |e| {
        matches!(
            e,
            ClientEvent::NetworkAdded {
                network_id: NetworkId(6),
                ..
            }
        )
    })
    .await;
    client.close();
    drain(&mut client, &mut state).await;
    assert_eq!(
        written(fake).await,
        [
            init_request(b"BufferSyncer", ""),
            init_request(b"Network", "1"),
            init_request(b"Network", "5"),
            init_request(b"Network", "6"),
        ]
    );
}

#[tokio::test]
async fn requests_are_written_with_the_right_shapes() {
    let info = buffer(10, 1, "#python");
    let fake = fake_core(
        inbound_with_session(session_with(&[], std::slice::from_ref(&info)), &[]),
        false,
        true,
    );
    let mut client = client_for(&fake);
    let mut state = ClientState::default();
    until(&mut client, &mut state, |e| {
        matches!(e, ClientEvent::BufferAdded { .. })
    })
    .await;

    let handle = client.handle();
    handle
        .send_input(info.clone(), "hi there".into())
        .await
        .unwrap();
    handle.request_backlog(BufferId(10), 100).await.unwrap();
    handle
        .request_backlog_before(BufferId(10), MsgId(500), 100)
        .await
        .unwrap();
    handle.set_last_seen(BufferId(10), MsgId(42)).await.unwrap();
    handle
        .set_marker_line(BufferId(10), MsgId(41))
        .await
        .unwrap();
    handle.close();
    drain(&mut client, &mut state).await;

    let sent = written(fake).await;
    assert_eq!(
        sent[1],
        SignalProxyMessage::RpcCall(RpcCall {
            signal_name: SEND_INPUT_SIGNAL.to_vec(),
            params: vec![
                Variant::User(UserValue::BufferInfo(info)),
                "hi there".into()
            ],
        })
    );
    let SignalProxyMessage::Sync(backlog) = &sent[2] else {
        panic!("expected Sync, got {:?}", sent[2]);
    };
    assert_eq!(backlog.class_name, b"BacklogManager");
    assert_eq!(backlog.slot_name, b"requestBacklog");
    assert_eq!(
        backlog.params,
        [
            Variant::User(UserValue::BufferId(BufferId(10))),
            Variant::User(UserValue::MsgId(MsgId(-1))),
            Variant::User(UserValue::MsgId(MsgId(-1))),
            Variant::Int(100),
            Variant::Int(0),
        ]
    );
    let SignalProxyMessage::Sync(older) = &sent[3] else {
        panic!("expected Sync, got {:?}", sent[3]);
    };
    assert_eq!(older.slot_name, b"requestBacklog");
    assert_eq!(
        older.params[1..3],
        [
            Variant::User(UserValue::MsgId(MsgId(-1))),
            Variant::User(UserValue::MsgId(MsgId(500)))
        ]
    );
    let slots: Vec<_> = sent[4..]
        .iter()
        .map(|m| match m {
            SignalProxyMessage::Sync(s) => (s.slot_name.clone(), s.params.clone()),
            other => panic!("unexpected {other:?}"),
        })
        .collect();
    assert_eq!(
        slots,
        [
            (
                b"requestSetLastSeenMsg".to_vec(),
                vec![
                    Variant::User(UserValue::BufferId(BufferId(10))),
                    Variant::User(UserValue::MsgId(MsgId(42)))
                ]
            ),
            (
                b"requestSetMarkerLine".to_vec(),
                vec![
                    Variant::User(UserValue::BufferId(BufferId(10))),
                    Variant::User(UserValue::MsgId(MsgId(41)))
                ]
            ),
        ]
    );
}

#[tokio::test]
async fn requests_after_close_fail_cleanly() {
    let fake = fake_core(
        inbound_with_session(session_with(&[], &[]), &[]),
        false,
        false,
    );
    let mut client = client_for(&fake);
    let mut state = ClientState::default();
    drain(&mut client, &mut state).await;
    let err = client
        .handle()
        .send_input(buffer(1, 1, "#x"), "lost".into())
        .await
        .unwrap_err();
    assert!(err.to_string().contains("CLOSED"), "{err}");
}

#[tokio::test]
async fn reused_state_keeps_history_across_clients() {
    let info = buffer(10, 1, "#python");
    let mut state = ClientState::default();
    state.messages.insert(
        BufferId(10),
        vec![IrcMessage {
            msg_id: MsgId(1),
            buffer_id: BufferId(10),
            network_id: NetworkId(1),
            timestamp: Utc::now(),
            kind: MessageType::Plain,
            flags: MessageFlags::NONE,
            sender: "a".into(),
            sender_prefixes: String::new(),
            contents: "before the drop".into(),
        }],
    );
    state.claim_backlog(BufferId(10));
    let fake = fake_core(
        inbound_with_session(session_with(&[1], &[info]), &[]),
        false,
        false,
    );
    let mut client = client_for(&fake);
    drain(&mut client, &mut state).await;
    assert_eq!(state.messages[&BufferId(10)][0].contents, "before the drop");
    assert!(state.backlog_requested.is_empty());
}
