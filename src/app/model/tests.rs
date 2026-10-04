use std::sync::Arc;

use chrono::Utc;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::*;
use crate::protocol::Features;
use crate::protocol::messages::SessionInit;
use crate::protocol::types::{MessageType, NetworkId};
use crate::sync::Network;

fn info(id: i32, net: i32, kind: BufferType, name: &str) -> BufferInfo {
    BufferInfo {
        buffer_id: BufferId(id),
        network_id: NetworkId(net),
        kind,
        group_id: 0,
        name: name.into(),
    }
}

fn message(id: i64, buffer: i32, flags: MessageFlags) -> IrcMessage {
    IrcMessage {
        msg_id: MsgId(id),
        buffer_id: BufferId(buffer),
        network_id: NetworkId(1),
        timestamp: Utc::now(),
        kind: MessageType::Plain,
        flags,
        sender: "nick!u@h".into(),
        sender_prefixes: String::new(),
        contents: format!("message {id}"),
    }
}

/// A state with network 1 holding #a (10), #b (11), and a query (12).
fn state() -> ClientState {
    let mut state = ClientState::new(0);
    state.networks.insert(NetworkId(1), Network::new("1"));
    for b in [
        info(10, 1, BufferType::Channel, "#a"),
        info(11, 1, BufferType::Channel, "#b"),
        info(12, 1, BufferType::Query, "friend"),
    ] {
        state.messages.insert(b.buffer_id, Vec::new());
        state.buffers.insert(b.buffer_id, b);
    }
    state
}

fn live_app() -> App {
    App::new(state(), true, true)
}

fn opened() -> ClientEvent {
    ClientEvent::SessionOpened {
        session: Arc::new(SessionInit::default()),
        peer_features: Features::empty(),
    }
}

fn disconnected(reason: &str) -> ClientEvent {
    ClientEvent::Disconnected {
        reason: reason.into(),
        error: None,
    }
}

/// Deliver a live message: store it like the dispatcher would, then
/// tell the app.
fn deliver(app: &mut App, msg: IrcMessage) -> Vec<Effect> {
    app.state
        .messages
        .entry(msg.buffer_id)
        .or_default()
        .push(msg.clone());
    app.on_client_event(&ClientEvent::MessageReceived(msg))
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn ctrl(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
}

fn alt(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::ALT)
}

fn type_text(app: &mut App, text: &str) {
    for c in text.chars() {
        app.on_key(key(KeyCode::Char(c)));
    }
}

#[test]
fn session_opened_picks_default_and_requests_backlog() {
    let mut app = live_app();
    let effects = app.on_client_event(&opened());
    assert_eq!(app.active_buffer_id, Some(BufferId(10)));
    assert_eq!(effects, [Effect::RequestBacklog(BufferId(10))]);
    assert_eq!(app.tree_cursor, Some(BufferId(10)));
    assert_eq!(app.log.buffer, Some(BufferId(10)));
}

#[test]
fn default_pick_prefers_buffer_with_messages() {
    let mut s = state();
    s.messages
        .get_mut(&BufferId(11))
        .unwrap()
        .push(message(1, 11, MessageFlags::NONE));
    let mut app = App::new(s, true, true);
    app.on_client_event(&opened());
    assert_eq!(app.active_buffer_id, Some(BufferId(11)));
}

#[test]
fn no_buffers_no_pick() {
    let mut app = App::new(ClientState::new(0), true, true);
    assert!(app.on_client_event(&opened()).is_empty());
    assert_eq!(app.active_buffer_id, None);
}

#[test]
fn existing_active_buffer_is_kept_and_refreshed_on_reopen() {
    let mut app = live_app();
    app.on_client_event(&opened());
    app.select_buffer(BufferId(11));
    // A reconnect clears latches (the dispatcher re-seeds) and reopens.
    app.state.backlog_requested.clear();
    let effects = app.on_client_event(&opened());
    assert_eq!(app.active_buffer_id, Some(BufferId(11)));
    assert_eq!(effects, [Effect::RequestBacklog(BufferId(11))]);
}

#[test]
fn cold_start_message_lands_on_its_buffer_and_never_hijacks_later() {
    let mut app = App::new(ClientState::new(0), true, true);
    app.state
        .buffers
        .insert(BufferId(12), info(12, 1, BufferType::Query, "friend"));
    let effects = deliver(&mut app, message(1, 12, MessageFlags::NONE));
    assert_eq!(app.active_buffer_id, Some(BufferId(12)));
    assert_eq!(effects, [Effect::RequestBacklog(BufferId(12))]);

    app.state
        .buffers
        .insert(BufferId(11), info(11, 1, BufferType::Channel, "#b"));
    deliver(&mut app, message(2, 11, MessageFlags::NONE));
    assert_eq!(app.active_buffer_id, Some(BufferId(12)));
}

#[test]
fn activity_marks_for_inactive_buffers() {
    let mut app = live_app();
    app.on_client_event(&opened());
    deliver(&mut app, message(1, 11, MessageFlags::NONE));
    assert_eq!(
        app.buffer_activity.get(&BufferId(11)),
        Some(&Activity::Message)
    );
    // Queries are always attention-worthy.
    deliver(&mut app, message(2, 12, MessageFlags::NONE));
    assert_eq!(
        app.buffer_activity.get(&BufferId(12)),
        Some(&Activity::Highlight)
    );
    // Highlight is sticky.
    deliver(&mut app, message(3, 11, MessageFlags::HIGHLIGHT));
    deliver(&mut app, message(4, 11, MessageFlags::NONE));
    assert_eq!(
        app.buffer_activity.get(&BufferId(11)),
        Some(&Activity::Highlight)
    );
    // Our own echoed lines are not activity; the active buffer never is.
    app.buffer_activity.clear();
    deliver(&mut app, message(5, 11, MessageFlags::SELF));
    deliver(&mut app, message(6, 10, MessageFlags::NONE));
    assert!(app.buffer_activity.is_empty());
    // Visiting consumes it.
    deliver(&mut app, message(7, 11, MessageFlags::NONE));
    app.select_buffer(BufferId(11));
    assert!(!app.buffer_activity.contains_key(&BufferId(11)));
}

#[test]
fn removing_the_active_buffer_repicks() {
    let mut app = live_app();
    app.on_client_event(&opened());
    app.state.buffers.remove(&BufferId(10));
    app.state.messages.remove(&BufferId(10));
    app.on_client_event(&ClientEvent::BufferRemoved {
        buffer_id: BufferId(10),
    });
    assert_eq!(app.active_buffer_id, Some(BufferId(11)));
    assert_eq!(app.log.buffer, Some(BufferId(11)));

    // Removing an inactive buffer leaves the active one alone.
    app.state.buffers.remove(&BufferId(12));
    app.on_client_event(&ClientEvent::BufferRemoved {
        buffer_id: BufferId(12),
    });
    assert_eq!(app.active_buffer_id, Some(BufferId(11)));

    // The last buffer going away clears the pointer and the cursor.
    app.state.buffers.clear();
    app.state.messages.clear();
    app.on_client_event(&ClientEvent::BufferRemoved {
        buffer_id: BufferId(11),
    });
    assert_eq!(app.active_buffer_id, None);
    assert_eq!(app.tree_cursor, None);
    assert_eq!(app.log.buffer, None);
}

#[test]
fn presession_disconnect_is_fatal_with_sanitized_bounded_reason() {
    let mut app = live_app();
    let long = format!("\x1b[31mbad{}", "x".repeat(1000));
    app.on_client_event(&disconnected(&long));
    let exit = app.exit.clone().unwrap();
    assert_eq!(exit.code, 1);
    let message = exit.message.unwrap();
    assert!(message.starts_with("quasseltui: "));
    assert!(!message.contains('\x1b'));
    assert!(message.ends_with("...[truncated]"));
    assert!(message.len() < 500);
}

#[test]
fn midsession_disconnect_disables_input_and_stashes_text() {
    let mut app = live_app();
    app.on_client_event(&opened());
    type_text(&mut app, "half-typed");
    app.on_client_event(&disconnected("core closed connection"));
    assert!(app.exit.is_none());
    assert!(app.connection_lost);
    assert!(app.input.disabled);
    assert!(app.input.value.is_empty());
    assert!(app.input.placeholder.contains("core closed connection"));
    assert!(app.input.placeholder.contains("Ctrl+R"));
    assert!(app.notices.iter().any(|n| n.starts_with("Disconnected:")));
    // A second disconnect doesn't stack notices.
    let count = app.notices.len();
    app.on_client_event(&disconnected("again"));
    assert_eq!(app.notices.len(), count);
    // No history requests once the socket is gone.
    assert!(app.select_buffer(BufferId(11)).is_empty());
}

#[test]
fn reconnect_restores_input_and_relatches_on_failure() {
    let mut app = live_app();
    app.on_client_event(&opened());
    type_text(&mut app, "draft");
    app.on_client_event(&disconnected("gone"));
    assert_eq!(app.reconnect(), [Effect::Reconnect]);
    assert!(!app.connection_lost);
    assert!(!app.input.disabled);
    assert_eq!(app.input.value, "draft");
    assert_eq!(app.input.placeholder, DEFAULT_PLACEHOLDER);

    // The core is still down: back to the disconnected state, no exit.
    app.on_client_event(&disconnected("connection refused"));
    assert!(app.exit.is_none());
    assert!(app.connection_lost);
}

#[test]
fn reconnect_is_a_noop_when_connected_or_unsupported() {
    let mut app = live_app();
    app.on_client_event(&opened());
    assert!(app.reconnect().is_empty());
    assert!(app.notices.iter().any(|n| n == "Already connected"));

    let mut demo = App::new(state(), false, false);
    assert!(demo.on_key(ctrl('r')).is_empty());
    let mut no_factory = App::new(state(), true, false);
    no_factory.on_client_event(&opened());
    no_factory.on_client_event(&disconnected("x"));
    assert!(no_factory.reconnect().is_empty());
}

#[test]
fn input_submit_sends_and_clears() {
    let mut app = live_app();
    app.on_client_event(&opened());
    type_text(&mut app, "hello");
    let effects = app.on_key(key(KeyCode::Enter));
    assert_eq!(
        effects,
        [Effect::SendInput {
            buffer: info(10, 1, BufferType::Channel, "#a"),
            text: "hello".into()
        }]
    );
    assert!(app.input.value.is_empty());
    // A second Enter right away doesn't resend.
    assert!(app.on_key(key(KeyCode::Enter)).is_empty());
}

#[test]
fn send_failure_restores_text_and_notifies() {
    let mut app = live_app();
    app.on_client_event(&opened());
    app.send_failed("lost line".into(), "failed to send input: \x1b[2Jboom");
    assert_eq!(app.input.value, "lost line");
    let notice = app.notices.last().unwrap();
    assert!(notice.starts_with("Message not sent:"));
    assert!(!notice.contains('\x1b'));

    // Text the user already started typing is not overwritten.
    app.input.set_value("new");
    app.send_failed("old".into(), "x");
    assert_eq!(app.input.value, "new");
}

#[test]
fn line_before_active_buffer_is_restored_with_notice() {
    let mut app = App::new(ClientState::new(0), true, true);
    type_text(&mut app, "too early");
    assert!(app.on_key(key(KeyCode::Enter)).is_empty());
    assert_eq!(app.input.value, "too early");
    assert!(app.notices.last().unwrap().contains("No active buffer yet"));
}

#[test]
fn whitespace_only_input_is_not_sent() {
    let mut app = live_app();
    app.on_client_event(&opened());
    type_text(&mut app, "   ");
    assert!(app.on_key(key(KeyCode::Enter)).is_empty());
    assert!(app.input.value.is_empty());
    assert!(app.state.read_markers.is_empty());
}

#[test]
fn demo_mode_never_sends() {
    let mut app = App::new(state(), false, false);
    type_text(&mut app, "hi");
    assert!(app.on_key(key(KeyCode::Enter)).is_empty());
}

#[test]
fn empty_enter_moves_marker_to_latest() {
    let mut app = live_app();
    app.on_client_event(&opened());
    // Nothing to mark yet.
    assert!(app.on_key(key(KeyCode::Enter)).is_empty());
    assert!(app.state.read_markers.is_empty());

    deliver(&mut app, message(5, 10, MessageFlags::NONE));
    deliver(&mut app, message(6, 10, MessageFlags::NONE));
    let effects = app.on_key(key(KeyCode::Enter));
    assert_eq!(effects, [Effect::SetMarkerLine(BufferId(10), MsgId(6))]);
    assert_eq!(app.state.read_markers[&BufferId(10)], MsgId(6));
    assert_eq!(
        app.notices.last().unwrap(),
        "Read marker moved to latest in #a"
    );
}

#[test]
fn enter_in_log_places_marker_per_buffer() {
    let mut app = live_app();
    app.on_client_event(&opened());
    for i in 1..=3 {
        deliver(&mut app, message(i, 10, MessageFlags::NONE));
    }
    app.on_key(key(KeyCode::Tab));
    assert_eq!(app.focus, Focus::Log);
    assert_eq!(app.log.highlighted, Some(MsgId(3)));
    app.on_key(key(KeyCode::Up));
    let effects = app.on_key(key(KeyCode::Enter));
    assert_eq!(effects, [Effect::SetMarkerLine(BufferId(10), MsgId(2))]);
    assert_eq!(app.state.read_markers[&BufferId(10)], MsgId(2));
    // A new placement replaces the old one.
    app.on_key(key(KeyCode::Up));
    app.on_key(key(KeyCode::Enter));
    assert_eq!(app.state.read_markers[&BufferId(10)], MsgId(1));
    assert!(!app.state.read_markers.contains_key(&BufferId(11)));
}

#[test]
fn switching_buffers_reports_last_seen() {
    let mut app = live_app();
    app.on_client_event(&opened());
    deliver(&mut app, message(9, 10, MessageFlags::NONE));
    let effects = app.on_key(alt(KeyCode::Down));
    assert_eq!(app.active_buffer_id, Some(BufferId(11)));
    assert_eq!(
        effects,
        [
            Effect::RequestBacklog(BufferId(11)),
            Effect::SetLastSeen(BufferId(10), MsgId(9))
        ]
    );
    // The backlog latch makes a revisit request nothing new; leaving a
    // buffer still reports it read.
    assert!(app.on_key(alt(KeyCode::Up)).is_empty());
    assert_eq!(
        app.on_key(alt(KeyCode::Down)),
        [Effect::SetLastSeen(BufferId(10), MsgId(9))]
    );
}

#[test]
fn alt_arrows_cycle_in_sidebar_order() {
    let mut app = live_app();
    app.on_client_event(&opened());
    app.on_key(alt(KeyCode::Up));
    assert_eq!(app.active_buffer_id, Some(BufferId(12)), "wraps backward");
    app.on_key(alt(KeyCode::Down));
    assert_eq!(app.active_buffer_id, Some(BufferId(10)), "wraps forward");
    app.on_key(alt(KeyCode::Down));
    assert_eq!(app.active_buffer_id, Some(BufferId(11)));
}

#[test]
fn tree_navigation_selects_buffers() {
    let mut app = live_app();
    app.on_client_event(&opened());
    app.set_focus(Focus::Tree);
    app.on_key(key(KeyCode::Down));
    app.on_key(key(KeyCode::Down));
    assert_eq!(app.tree_cursor, Some(BufferId(12)));
    assert_eq!(app.active_buffer_id, Some(BufferId(10)));
    app.on_key(key(KeyCode::Enter));
    assert_eq!(app.active_buffer_id, Some(BufferId(12)));
}

#[test]
fn backlog_failure_releases_latch_and_notifies() {
    let mut app = live_app();
    app.on_client_event(&opened());
    assert!(app.state.backlog_requested.contains(&BufferId(10)));
    app.backlog_failed(BufferId(10), "boom");
    assert!(!app.state.backlog_requested.contains(&BufferId(10)));
    assert!(
        app.notices
            .last()
            .unwrap()
            .starts_with("Could not load history")
    );
}

#[test]
fn input_history_recall() {
    let mut app = live_app();
    app.on_client_event(&opened());
    for line in ["first", "second"] {
        type_text(&mut app, line);
        app.on_key(key(KeyCode::Enter));
    }
    type_text(&mut app, "draft");
    app.on_key(key(KeyCode::Up));
    assert_eq!(app.input.value, "second");
    app.on_key(key(KeyCode::Up));
    assert_eq!(app.input.value, "first");
    app.on_key(key(KeyCode::Up));
    assert_eq!(app.input.value, "first");
    app.on_key(key(KeyCode::Down));
    assert_eq!(app.input.value, "second");
    app.on_key(key(KeyCode::Down));
    assert_eq!(app.input.value, "draft");
}

#[test]
fn input_editing_keys() {
    let mut app = live_app();
    type_text(&mut app, "hello wörld");
    app.on_key(key(KeyCode::Left));
    app.on_key(key(KeyCode::Backspace));
    assert_eq!(app.input.value, "hello wörd");
    app.on_key(ctrl('a'));
    app.on_key(key(KeyCode::Delete));
    assert_eq!(app.input.value, "ello wörd");
    app.on_key(ctrl('e'));
    app.on_key(ctrl('w'));
    assert_eq!(app.input.value, "ello ");
    app.on_key(ctrl('u'));
    assert_eq!(app.input.value, "");
    app.on_paste("multi\nline");
    assert_eq!(app.input.value, "multi line");
}

#[test]
fn quit_and_help_keys() {
    let mut app = live_app();
    app.on_key(KeyEvent::new(KeyCode::F(1), KeyModifiers::NONE));
    assert!(app.show_help);
    app.on_key(key(KeyCode::Esc));
    assert!(!app.show_help);
    app.on_key(ctrl('c'));
    assert!(app.exit.is_none());
    app.on_key(ctrl('q'));
    assert_eq!(
        app.exit,
        Some(Exit {
            code: 0,
            message: None
        })
    );
}

#[test]
fn toasts_expire() {
    let mut app = live_app();
    app.notify("hi", Severity::Info);
    assert!(!app.tick(Instant::now()));
    assert!(app.tick(Instant::now() + Duration::from_secs(6)));
    assert!(app.toasts.is_empty());
    assert_eq!(app.notices, ["hi"]);
}

#[test]
fn demo_app_shows_content_immediately() {
    let mut s = state();
    s.messages
        .get_mut(&BufferId(11))
        .unwrap()
        .push(message(1, 11, MessageFlags::NONE));
    let app = App::new(s, false, false);
    assert_eq!(app.active_buffer_id, Some(BufferId(11)));
    assert_eq!(app.log.buffer, Some(BufferId(11)));
}
