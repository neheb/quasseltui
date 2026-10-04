//! State-machine tests against a fake core over an in-memory pipe.
//!
//! The fake core reads the client's 8-byte probe, replies, then plays back
//! a prepared byte sequence. It records everything the client writes after
//! the probe so tests can inspect the frames that went out.

use std::sync::atomic::AtomicBool;

use super::*;
use crate::protocol::features::LEGACY_SENDER_PREFIXES;
use crate::protocol::framing::encode_frame;
use crate::protocol::handshake::decode_handshake_payload;
use crate::protocol::messages::CLIENT_LOGIN_REJECT;
use crate::protocol::testing::*;
use crate::qt::variant::Variant;

async fn run(opts: ConnectionOptions, fake: &Fake) -> (Vec<ProtocolEvent>, ConnectionHandle) {
    let conn = QuasselConnection::with_connector(opts, fake.connector.clone());
    let (handle, mut rx) = conn.start();
    let mut events = Vec::new();
    while let Some(event) = rx.recv().await {
        events.push(event);
    }
    (events, handle)
}

fn reason(event: &ProtocolEvent) -> &str {
    match event {
        ProtocolEvent::Disconnected { reason, .. } => reason,
        other => panic!("expected Disconnected, got {other:?}"),
    }
}

fn sync_msg(name: &str) -> SyncMessage {
    SyncMessage {
        class_name: b"Network".to_vec(),
        object_name: "1".into(),
        slot_name: b"setNetworkName".to_vec(),
        params: vec![name.into()],
    }
}

fn ready_features(event: &ProtocolEvent) -> Features {
    match event {
        ProtocolEvent::SessionReady { peer_features, .. } => *peer_features,
        other => panic!("expected SessionReady, got {other:?}"),
    }
}

#[tokio::test]
async fn session_ready_is_first_and_disconnected_last() {
    let fake = fake_core(inbound(base_init_ack(), login_ack(), &[]), false, false);
    let (events, handle) = run(options(false), &fake).await;
    assert_eq!(ready_features(&events[0]), modern());
    assert!(reason(events.last().unwrap()).contains("frame read error"));
    assert_eq!(handle.state(), ConnState::Closed);
    assert_eq!(handle.peer_features(), modern());

    // The client sent ClientInit with our features, then ClientLogin.
    let frames = split_frames(&fake.written.await.unwrap());
    assert_eq!(frames.len(), 2);
    let init = decode_handshake_payload(&frames[0]).unwrap();
    assert_eq!(init["MsgType"], Variant::String("ClientInit".into()));
    assert_eq!(init["Features"], Variant::UInt(LEGACY_SENDER_PREFIXES));
    assert_eq!(
        init["FeatureList"],
        Variant::StringList(vec![
            "LongTime".into(),
            "SenderPrefixes".into(),
            "RichMessages".into(),
            "LongMessageId".into()
        ])
    );
    let login = decode_handshake_payload(&frames[1]).unwrap();
    assert_eq!(login["User"], Variant::String("u".into()));
}

#[tokio::test]
async fn feature_negotiation_tiers() {
    let cases: Vec<(Vec<String>, u32, Features)> = vec![
        // Intersection, not union.
        (vec!["LongTime".into()], 0, Features::LONG_TIME),
        // Tier 1 with a binary-only supplement.
        (
            vec!["LongTime".into()],
            LEGACY_SENDER_PREFIXES,
            Features::LONG_TIME | Features::SENDER_PREFIXES,
        ),
        // Tier 2: ExtendedFeatures with an empty list means everything offered.
        (
            vec![],
            LEGACY_EXTENDED_FEATURES | LEGACY_SENDER_PREFIXES,
            Features::client_default(),
        ),
        // Tier 3: binary bitmask only.
        (vec![], LEGACY_SENDER_PREFIXES, Features::SENDER_PREFIXES),
    ];
    for (list, bits, expected) in cases {
        let mut ack = base_init_ack();
        ack.insert("FeatureList".into(), Variant::StringList(list));
        ack.insert("CoreFeatures".into(), Variant::UInt(bits));
        let fake = fake_core(inbound(ack, login_ack(), &[]), false, false);
        let (events, _) = run(options(false), &fake).await;
        assert_eq!(ready_features(&events[0]), expected);
    }
}

#[tokio::test]
async fn tls_declined_aborts_before_client_init() {
    let fake = fake_core(inbound(base_init_ack(), login_ack(), &[]), false, false);
    let (events, handle) = run(options(true), &fake).await;
    assert_eq!(events.len(), 1);
    let text = reason(&events[0]).to_lowercase();
    assert!(text.contains("tls") && text.contains("plaintext"), "{text}");
    assert!(text.contains("--no-tls"));
    assert!(!text.contains("tls=false"));
    assert_eq!(handle.state(), ConnState::Closed);
    // Nothing was written after the probe: no ClientInit, no credentials.
    assert!(fake.written.await.unwrap().is_empty());
}

#[tokio::test]
async fn tls_enabled_triggers_upgrade() {
    let fake = fake_core(inbound(base_init_ack(), login_ack(), &[]), true, false);
    let (events, _) = run(options(true), &fake).await;
    assert!(matches!(events[0], ProtocolEvent::SessionReady { .. }));
    assert!(fake.tls_called.load(Ordering::SeqCst));
}

#[tokio::test]
async fn login_reject_surfaces_as_auth_disconnect() {
    let reject = map(vec![
        ("MsgType", CLIENT_LOGIN_REJECT.into()),
        ("Error", "bad password".into()),
    ]);
    let fake = fake_core(inbound(base_init_ack(), reject, &[]), false, false);
    let (events, _) = run(options(false), &fake).await;
    assert_eq!(events.len(), 1);
    let text = reason(&events[0]);
    assert!(
        text.contains("auth") && text.contains("bad password"),
        "{text}"
    );
    let ProtocolEvent::Disconnected { error, .. } = &events[0] else {
        unreachable!()
    };
    assert!(error.as_ref().unwrap().is_auth());
}

#[tokio::test]
async fn unconfigured_core_is_rejected() {
    let mut ack = base_init_ack();
    ack.insert("Configured".into(), Variant::Bool(false));
    let fake = fake_core(inbound(ack, login_ack(), &[]), false, false);
    let (events, _) = run(options(false), &fake).await;
    assert!(reason(&events[0]).contains("not configured"));
}

#[tokio::test]
async fn heartbeat_is_answered_before_the_event() {
    let ts = QDateTime {
        julian_day: 2_461_145,
        ms_of_day: 45_296_000,
        is_utc: true,
    };
    let fake = fake_core(
        inbound(
            base_init_ack(),
            login_ack(),
            &[framed_sp(&SignalProxyMessage::HeartBeat(ts))],
        ),
        false,
        false,
    );
    let (events, _) = run(options(false), &fake).await;
    let beats: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, ProtocolEvent::HeartBeat(t) if *t == ts))
        .collect();
    assert_eq!(beats.len(), 1);
    let frames = split_frames(&fake.written.await.unwrap());
    assert_eq!(frames.len(), 3);
    assert_eq!(
        decode_signalproxy_payload(&frames[2], modern()).unwrap(),
        SignalProxyMessage::HeartBeatReply(ts)
    );
}

#[tokio::test]
async fn sync_and_rpc_events_arrive_in_order() {
    let rpc = RpcCall {
        signal_name: b"2test()".to_vec(),
        params: vec![],
    };
    let fake = fake_core(
        inbound(
            base_init_ack(),
            login_ack(),
            &[
                framed_sp(&SignalProxyMessage::Sync(sync_msg("freenode"))),
                framed_sp(&SignalProxyMessage::RpcCall(rpc.clone())),
            ],
        ),
        false,
        false,
    );
    let (events, _) = run(options(false), &fake).await;
    assert_eq!(events.len(), 4);
    assert!(matches!(&events[1], ProtocolEvent::Sync(m) if *m == sync_msg("freenode")));
    assert!(matches!(&events[2], ProtocolEvent::Rpc(m) if *m == rpc));
    assert!(matches!(events[3], ProtocolEvent::Disconnected { .. }));
}

/// A stream that accepts a fixed number of writes and then fails every
/// write: models a peer that resets mid-session.
struct FailingWrites<S> {
    inner: S,
    writes_left: usize,
}

impl<S: AsyncRead + Unpin> AsyncRead for FailingWrites<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for FailingWrites<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        if self.writes_left == 0 {
            return std::task::Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "broken pipe",
            )));
        }
        self.writes_left -= 1;
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

#[tokio::test]
async fn heartbeat_reply_failure_is_terminal() {
    let ts = QDateTime::now_utc();
    let fake = fake_core(
        inbound(
            base_init_ack(),
            login_ack(),
            &[framed_sp(&SignalProxyMessage::HeartBeat(ts))],
        ),
        false,
        true,
    );
    let inner = fake.connector.stream.lock().unwrap().take().unwrap();
    // Probe, ClientInit and ClientLogin are one write_all each (a duplex
    // pipe accepts each in one call); the heartbeat reply is the fourth.
    let failing: BoxedStream = Box::new(FailingWrites {
        inner,
        writes_left: 3,
    });
    let connector = Arc::new(FakeConnector {
        stream: Mutex::new(Some(failing)),
        tls_called: Arc::new(AtomicBool::new(false)),
    });
    let (handle, mut rx) = QuasselConnection::with_connector(options(false), connector).start();
    let mut events = Vec::new();
    while let Some(event) = rx.recv().await {
        events.push(event);
    }
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, ProtocolEvent::HeartBeat(_)))
    );
    let last = reason(events.last().unwrap());
    assert!(last.contains("broken pipe"), "{last}");
    assert_eq!(handle.state(), ConnState::Closed);
}

#[tokio::test]
async fn dead_connection_is_detected_by_liveness_watchdog() {
    let fake = fake_core(inbound(base_init_ack(), login_ack(), &[]), false, true);
    let mut opts = options(false);
    opts.liveness_timeout = Duration::from_millis(50);
    let (events, handle) = run(opts, &fake).await;
    assert!(matches!(events[0], ProtocolEvent::SessionReady { .. }));
    assert!(reason(events.last().unwrap()).contains("no data from core"));
    assert_eq!(handle.state(), ConnState::Closed);
}

#[tokio::test]
async fn silent_peer_fails_the_handshake_with_a_timeout() {
    let fake = fake_core(Vec::new(), false, true);
    let mut opts = options(false);
    opts.handshake_timeout = Some(Duration::from_millis(50));
    let (events, handle) = run(opts, &fake).await;
    assert_eq!(events.len(), 1);
    let text = reason(&events[0]);
    assert!(
        text.contains("handshake") && text.contains("stalled"),
        "{text}"
    );
    assert!(text.contains("handshake_init"), "{text}");
    assert_eq!(handle.state(), ConnState::Closed);
}

fn bad_variant_frame() -> Vec<u8> {
    encode_frame(b"\x00\x00\x00\x01\x00\x00\x00\xff\x00")
}

#[tokio::test]
async fn undecodable_frames_are_skipped() {
    let fake = fake_core(
        inbound(
            base_init_ack(),
            login_ack(),
            &[
                bad_variant_frame(),
                encode_frame(b"\x00\x00\x00\x00"),
                encode_frame(b"poison"),
                framed_sp(&SignalProxyMessage::Sync(sync_msg("libera"))),
            ],
        ),
        false,
        false,
    );
    let (events, _) = run(options(false), &fake).await;
    let syncs = events
        .iter()
        .filter(|e| matches!(e, ProtocolEvent::Sync(_)))
        .count();
    assert_eq!(syncs, 1);
    let disconnects: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, ProtocolEvent::Disconnected { .. }))
        .collect();
    assert_eq!(disconnects.len(), 1);
    assert!(!reason(disconnects[0]).contains("undecodable"));
}

#[tokio::test]
async fn systematic_decode_failure_escalates() {
    let mut frames = vec![bad_variant_frame(); 20];
    frames.push(framed_sp(&SignalProxyMessage::HeartBeat(
        QDateTime::now_utc(),
    )));
    let fake = fake_core(inbound(base_init_ack(), login_ack(), &frames), false, false);
    let (events, _) = run(options(false), &fake).await;
    assert!(reason(events.last().unwrap()).contains("undecodable"));
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, ProtocolEvent::HeartBeat(_)))
    );
}

#[tokio::test]
async fn occasional_decode_failures_do_not_escalate() {
    let good = framed_sp(&SignalProxyMessage::Sync(sync_msg("x")));
    let mut frames = Vec::new();
    for _ in 0..4 {
        frames.extend(vec![bad_variant_frame(); 3]);
        frames.push(good.clone());
    }
    let fake = fake_core(inbound(base_init_ack(), login_ack(), &frames), false, false);
    let (events, _) = run(options(false), &fake).await;
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, ProtocolEvent::Sync(_)))
            .count(),
        4
    );
    assert!(!reason(events.last().unwrap()).contains("undecodable"));
}

#[test]
fn handshake_window_has_a_generous_floor() {
    let mut opts = options(false);
    opts.connect_timeout = Duration::from_secs(5);
    assert!(opts.effective_handshake_timeout() >= Duration::from_secs(60));
    opts.connect_timeout = Duration::from_secs(120);
    assert_eq!(opts.effective_handshake_timeout(), Duration::from_secs(120));
    opts.handshake_timeout = Some(Duration::from_secs(7));
    assert_eq!(opts.effective_handshake_timeout(), Duration::from_secs(7));
}

#[tokio::test]
async fn slow_payload_gets_a_larger_window() {
    let mut data = inbound(base_init_ack(), login_ack(), &[]);
    data.extend_from_slice(&100u32.to_be_bytes());
    let fake = fake_core(data, false, true);
    let mut opts = options(false);
    opts.liveness_timeout = Duration::from_millis(20);
    let start = std::time::Instant::now();
    let (events, handle) = run(opts, &fake).await;
    let elapsed = start.elapsed();
    assert!(reason(events.last().unwrap()).contains("no data"));
    assert!(
        elapsed >= Duration::from_millis(150) && elapsed < Duration::from_secs(5),
        "{elapsed:?}"
    );
    assert_eq!(handle.state(), ConnState::Closed);
}

#[tokio::test]
async fn sends_are_written_and_close_ends_the_stream() {
    let fake = fake_core(inbound(base_init_ack(), login_ack(), &[]), false, true);
    let conn = QuasselConnection::with_connector(options(false), fake.connector.clone());
    let (handle, mut rx) = conn.start();
    assert!(matches!(
        rx.recv().await,
        Some(ProtocolEvent::SessionReady { .. })
    ));

    let request = SignalProxyMessage::InitRequest(InitRequest {
        class_name: b"BufferSyncer".to_vec(),
        object_name: String::new(),
    });
    handle.send(request.clone()).await.unwrap();
    handle.close();
    let last = rx.recv().await.unwrap();
    assert_eq!(reason(&last), "client closed");
    assert!(rx.recv().await.is_none());
    assert_eq!(handle.state(), ConnState::Closed);

    let frames = split_frames(&fake.written.await.unwrap());
    assert_eq!(
        decode_signalproxy_payload(&frames[2], modern()).unwrap(),
        request
    );
    let err = handle
        .send(SignalProxyMessage::HeartBeat(QDateTime::now_utc()))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("CLOSED"), "{err}");
}

#[tokio::test]
async fn send_before_connected_is_rejected() {
    let fake = fake_core(Vec::new(), false, true);
    let conn = QuasselConnection::with_connector(options(false), fake.connector.clone());
    let (handle, _rx) = conn.start();
    let err = handle
        .send_nowait(SignalProxyMessage::HeartBeat(QDateTime::now_utc()))
        .unwrap_err();
    assert!(err.to_string().contains("cannot send"), "{err}");
    handle.close();
}
