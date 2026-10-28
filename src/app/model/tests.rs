use std::sync::Arc;

use std::time::{Duration, Instant};

use chrono::Utc;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::*;
use crate::protocol::Features;
use crate::protocol::messages::SessionInit;
use crate::protocol::types::{MessageType, NetworkId};
use crate::sync::{IrcChannel, Network};

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

/// Leave the starting normal mode for typing, with `i`.
fn typing(app: &mut App) {
    app.on_key(key(KeyCode::Char('i')));
    assert_eq!(app.focus, Focus::Input);
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
fn starts_in_the_remembered_buffer_if_the_session_has_it() {
    let mut app = live_app();
    app.set_start_buffer(Some(BufferId(12)));
    assert_eq!(
        app.on_client_event(&opened()),
        [Effect::RequestBacklog(BufferId(12))]
    );
    assert_eq!(app.active_buffer_id, Some(BufferId(12)));

    // A buffer that's gone since falls back to the default.
    let mut app = live_app();
    app.set_start_buffer(Some(BufferId(99)));
    app.on_client_event(&opened());
    assert_eq!(app.active_buffer_id, Some(BufferId(10)));
}

#[test]
fn an_empty_session_lands_on_the_first_buffer_to_appear() {
    // What the dispatcher sends: SessionOpened with no buffers, then
    // BufferAdded ahead of the first message for it.
    let mut app = App::new(ClientState::new(0), true, true);
    assert!(app.on_client_event(&opened()).is_empty());
    assert_eq!(app.active_buffer_id, None);
    app.state
        .buffers
        .insert(BufferId(12), info(12, 1, BufferType::Query, "friend"));
    let effects = app.on_client_event(&ClientEvent::BufferAdded {
        buffer_id: BufferId(12),
        network_id: NetworkId(1),
        name: "friend".into(),
        kind: BufferType::Query,
    });
    assert_eq!(app.active_buffer_id, Some(BufferId(12)));
    assert_eq!(effects, [Effect::RequestBacklog(BufferId(12))]);

    // Later buffers and their messages never move the user.
    app.state
        .buffers
        .insert(BufferId(11), info(11, 1, BufferType::Channel, "#b"));
    app.on_client_event(&ClientEvent::BufferAdded {
        buffer_id: BufferId(11),
        network_id: NetworkId(1),
        name: "#b".into(),
        kind: BufferType::Channel,
    });
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

fn core_activity(buffer: i32, types: u32, highlights: i64) -> ClientEvent {
    ClientEvent::BufferActivity {
        buffer_id: BufferId(buffer),
        types,
        highlights,
    }
}

#[test]
fn core_activity_sets_and_clears_marks() {
    let mut app = live_app();
    app.on_client_event(&opened());
    let active = app.active_buffer_id.unwrap();
    let plain = MessageType::Plain as u32;
    let join = MessageType::Join as u32;
    app.on_client_event(&core_activity(11, plain | join, 0));
    assert_eq!(
        app.buffer_activity.get(&BufferId(11)),
        Some(&Activity::Message)
    );
    app.on_client_event(&core_activity(11, plain, 1));
    assert_eq!(
        app.buffer_activity.get(&BufferId(11)),
        Some(&Activity::Highlight)
    );
    // Never lowered while unread...
    app.on_client_event(&core_activity(11, plain, 0));
    assert_eq!(
        app.buffer_activity.get(&BufferId(11)),
        Some(&Activity::Highlight)
    );
    // ...but cleared once read elsewhere.
    app.on_client_event(&core_activity(11, 0, 0));
    assert!(!app.buffer_activity.contains_key(&BufferId(11)));
    // Queries are always attention-worthy; the active buffer never marks.
    app.on_client_event(&core_activity(12, plain, 0));
    assert_eq!(
        app.buffer_activity.get(&BufferId(12)),
        Some(&Activity::Highlight)
    );
    app.on_client_event(&core_activity(active.0, plain, 1));
    assert!(!app.buffer_activity.contains_key(&active));
}

#[test]
fn events_are_not_activity() {
    let mut app = live_app();
    app.on_client_event(&opened());
    let mut join = message(1, 11, MessageFlags::NONE);
    join.kind = MessageType::Join;
    deliver(&mut app, join);
    let events = MessageType::Join as u32 | MessageType::Quit as u32 | MessageType::Mode as u32;
    app.on_client_event(&core_activity(12, events, 0));
    assert!(app.buffer_activity.is_empty());
    // A notice or action is chat.
    app.on_client_event(&core_activity(11, events | MessageType::Action as u32, 0));
    assert_eq!(
        app.buffer_activity.get(&BufferId(11)),
        Some(&Activity::Message)
    );
    // Only events left since last seen: cleared.
    app.on_client_event(&core_activity(11, events, 0));
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
fn reconnect_restores_input_once_the_session_opens() {
    let mut app = live_app();
    app.on_client_event(&opened());
    typing(&mut app);
    type_text(&mut app, "draft");
    app.on_client_event(&disconnected("gone"));
    assert_eq!(app.reconnect(), [Effect::Reconnect]);
    assert!(!app.connection_lost);
    // Still connecting: no typing yet, and the draft stays stashed.
    assert!(app.input.disabled);
    assert_eq!(app.input.placeholder, "Reconnecting…");
    assert_eq!(app.focus, Focus::Log);

    // The core is still down: back to the disconnected state, no exit.
    app.on_client_event(&disconnected("connection refused"));
    assert!(app.exit.is_none());
    assert!(app.connection_lost);
    assert!(app.input.placeholder.contains("connection refused"));

    app.reconnect();
    app.on_client_event(&opened());
    assert!(!app.input.disabled);
    assert_eq!(app.input.value, "draft");
    assert_eq!(app.input.placeholder, DEFAULT_PLACEHOLDER);
    // Back to typing to finish the draft.
    assert_eq!(app.focus, Focus::Input);
}

#[test]
fn reconnects_on_its_own_with_backoff() {
    let mut app = live_app();
    app.on_client_event(&opened());
    let start = Instant::now();
    app.on_client_event(&disconnected("gone"));
    assert!(app.input.placeholder.contains("retrying in 2s"));
    assert!(app.retry_due(start).is_empty());
    // The countdown moves, and is redrawn.
    assert!(app.tick(start + Duration::from_millis(1100)));
    assert!(app.input.placeholder.contains("retrying in 1s"));
    let after = |secs| start + Duration::from_secs(secs);
    assert_eq!(app.retry_due(after(3)), [Effect::Reconnect]);
    assert!(app.retry_due(after(4)).is_empty(), "one attempt at a time");

    // Each failure doubles the wait, quietly.
    let toasts = app.notices.len();
    app.on_client_event(&disconnected("refused"));
    assert!(app.input.placeholder.contains("retrying in 4s"));
    assert_eq!(app.notices.len(), toasts);
    for expected in ["8s", "16s", "32s", "60s", "60s"] {
        app.retry_due(after(3600));
        app.on_client_event(&disconnected("refused"));
        assert!(app.input.placeholder.contains(expected), "{expected}");
    }

    // A session resets the backoff; Ctrl+R still works while waiting.
    app.retry_due(after(3600));
    app.on_client_event(&opened());
    app.on_client_event(&disconnected("gone again"));
    assert!(app.input.placeholder.contains("retrying in 2s"));
    assert_eq!(app.reconnect(), [Effect::Reconnect]);
    assert!(app.retry_due(after(3600)).is_empty());
}

#[test]
fn no_automatic_retry_when_it_cant_help() {
    for error in [
        Error::Auth("bad password".into()),
        Error::NotConfigured("x".into()),
        Error::TlsDowngrade("x".into()),
        Error::InitRejected("x".into()),
    ] {
        let mut app = live_app();
        app.on_client_event(&opened());
        app.on_client_event(&ClientEvent::Disconnected {
            reason: "rejected".into(),
            error: Some(Arc::new(error)),
        });
        assert!(app.connection_lost);
        assert!(app.input.placeholder.contains("Ctrl+R to reconnect"));
        assert!(app.retry_due(Instant::now() + RETRY_MAX * 2).is_empty());
    }
    // Nor without a way to reconnect.
    let mut no_factory = App::new(state(), true, false);
    no_factory.on_client_event(&opened());
    no_factory.on_client_event(&disconnected("x"));
    assert!(
        no_factory
            .retry_due(Instant::now() + RETRY_MAX * 2)
            .is_empty()
    );
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
    typing(&mut app);
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

    assert!(!notice.contains("Up recalls"));

    // Text the user already started typing is not overwritten; the notice
    // says where the failed line went.
    app.input.set_value("new");
    app.input.remember("old");
    app.send_failed("old".into(), "x");
    assert_eq!(app.input.value, "new");
    assert!(app.notices.last().unwrap().contains("Up recalls it"));
    // A pasted line was never in the history: no false promise.
    app.send_failed("pasted".into(), "x");
    assert!(!app.notices.last().unwrap().contains("Up recalls"));
}

#[test]
fn line_before_active_buffer_is_restored_with_notice() {
    let mut app = App::new(ClientState::new(0), true, true);
    typing(&mut app);
    type_text(&mut app, "too early");
    assert!(app.on_key(key(KeyCode::Enter)).is_empty());
    assert_eq!(app.input.value, "too early");
    assert!(app.notices.last().unwrap().contains("No active buffer yet"));
}

#[test]
fn whitespace_only_input_is_not_sent() {
    let mut app = live_app();
    app.on_client_event(&opened());
    typing(&mut app);
    type_text(&mut app, "   ");
    assert!(app.on_key(key(KeyCode::Enter)).is_empty());
    assert!(app.input.value.is_empty());
    assert!(app.state.read_markers.is_empty());
}

#[test]
fn demo_mode_never_sends() {
    let mut app = App::new(state(), false, false);
    typing(&mut app);
    type_text(&mut app, "hi");
    assert!(app.on_key(key(KeyCode::Enter)).is_empty());
}

#[test]
fn empty_enter_moves_marker_to_latest() {
    let mut app = live_app();
    app.on_client_event(&opened());
    typing(&mut app);
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
    // Entering normal mode puts the cursor on the newest message.
    typing(&mut app);
    app.on_key(key(KeyCode::Esc));
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
    typing(&mut app);
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
    typing(&mut app);
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
    // A single line with a trailing newline stays one line.
    app.on_paste("one line\n");
    assert_eq!(app.input.value, "one line ");
    assert!(app.pending_paste.is_none());
}

#[test]
fn multi_line_paste_waits_for_confirmation() {
    let mut app = live_app();
    app.on_client_event(&opened());
    typing(&mut app);
    type_text(&mut app, "see: ");
    app.on_paste("fn main() {\r\n\n/quit\n}");
    // What was typed joins the first line; blank lines are dropped.
    let paste = app.pending_paste.clone().unwrap();
    assert_eq!(paste.lines, ["see: fn main() {", "/quit", "}"]);
    assert!(app.input.value.is_empty());
    app.on_key(key(KeyCode::Esc));
    // Terminals that paste line breaks as a bare CR get lines too.
    app.on_paste("one\rtwo\r\rthree\r");
    let paste = app.pending_paste.clone().unwrap();
    assert_eq!(paste.lines, ["see: one", "two", "three"]);
    // Other keys do nothing while it waits.
    assert!(app.on_key(key(KeyCode::Char('x'))).is_empty());
    assert!(app.input.value.is_empty());

    // Esc puts the input back as it was.
    app.on_key(key(KeyCode::Esc));
    assert!(app.pending_paste.is_none());
    assert_eq!(app.input.value, "see: ");
    assert_eq!(app.focus, Focus::Input);

    // Enter sends each line, in order; a leading `/` is sent as text.
    app.on_paste("a\n/quit");
    let info = app.state.buffers[&BufferId(10)].clone();
    let send = |text: &str| Effect::SendInput {
        buffer: info.clone(),
        text: text.into(),
    };
    assert_eq!(
        app.on_key(key(KeyCode::Enter)),
        [send("see: a"), send("/say /quit")]
    );
    assert!(app.pending_paste.is_none());
    assert!(app.input.value.is_empty());
}

#[test]
fn a_disconnect_cancels_a_pending_paste() {
    let mut app = live_app();
    app.on_client_event(&opened());
    typing(&mut app);
    type_text(&mut app, "draft");
    app.on_paste("\nline two");
    assert!(app.pending_paste.is_some());
    app.on_client_event(&disconnected("gone"));
    assert!(app.pending_paste.is_none());
    app.reconnect();
    app.on_client_event(&opened());
    assert_eq!(app.input.value, "draft");
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

fn shift(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::SHIFT)
}

#[test]
fn aerc_style_normal_mode() {
    let mut app = live_app();
    app.on_client_event(&opened());
    for i in 1..=40 {
        deliver(&mut app, message(i, 10, MessageFlags::NONE));
    }
    app.log.set_viewport(80, 10);

    // Esc leaves typing; the arrows move within the channel without
    // typing anything.
    typing(&mut app);
    type_text(&mut app, "draft");
    app.on_key(key(KeyCode::Esc));
    assert_eq!(app.focus, Focus::Log);
    assert_eq!(app.log.highlighted, Some(MsgId(40)));
    app.on_key(key(KeyCode::Up));
    app.on_key(key(KeyCode::Up));
    app.on_key(key(KeyCode::Down));
    assert_eq!(app.log.highlighted, Some(MsgId(39)));
    app.on_key(key(KeyCode::Down));
    assert_eq!(app.log.highlighted, Some(MsgId(40)));
    assert_eq!(app.input.value, "draft");

    // g/G jump; Ctrl+U/Ctrl+D scroll half a page and leave the draft alone.
    app.on_key(key(KeyCode::Char('g')));
    assert_eq!(app.log.highlighted, Some(MsgId(1)));
    app.on_key(shift('G'));
    assert_eq!(app.log.highlighted, Some(MsgId(40)));
    app.on_key(ctrl('u'));
    assert!(!app.log.follow_tail);
    app.on_key(ctrl('d'));
    assert!(app.log.follow_tail);
    assert_eq!(app.input.value, "draft");

    // j/k and Shift+arrows switch channels immediately.
    app.on_key(key(KeyCode::Char('j')));
    assert_eq!(app.active_buffer_id, Some(BufferId(11)));
    app.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::SHIFT));
    assert_eq!(app.active_buffer_id, Some(BufferId(12)));
    app.on_key(key(KeyCode::Char('k')));
    app.on_key(KeyEvent::new(KeyCode::Up, KeyModifiers::SHIFT));
    assert_eq!(app.active_buffer_id, Some(BufferId(10)));

    // i (or Esc, or Tab) goes back to typing.
    app.on_key(key(KeyCode::Char('i')));
    assert_eq!(app.focus, Focus::Input);
    type_text(&mut app, "!");
    assert_eq!(app.input.value, "draft!");
    app.on_key(key(KeyCode::Esc));
    assert_eq!(app.focus, Focus::Log);
    app.on_key(key(KeyCode::Tab));
    assert_eq!(app.focus, Focus::Input);
}

#[test]
fn letters_type_while_typing() {
    let mut app = live_app();
    app.on_client_event(&opened());
    typing(&mut app);
    type_text(&mut app, "jkJK");
    assert_eq!(app.input.value, "jkJK");
    assert_eq!(app.active_buffer_id, Some(BufferId(10)));
}

#[test]
fn clicking_a_buffer_switches_without_changing_mode() {
    let mut app = live_app();
    app.on_client_event(&opened());
    typing(&mut app);
    app.click_tree(TreeRow::Buffer(BufferId(12)), Instant::now());
    assert_eq!(app.active_buffer_id, Some(BufferId(12)));
    assert_eq!(app.focus, Focus::Input);
}

/// Two clicks on `row`, `gap` apart.
fn double_click(app: &mut App, row: TreeRow, gap: Duration) -> Vec<Effect> {
    let t = Instant::now();
    let mut effects = app.click_tree(row, t);
    effects.extend(app.click_tree(row, t + gap));
    effects
}

fn connected_app() -> App {
    let mut app = live_app();
    app.on_client_event(&opened());
    let net = app.state.networks.get_mut(&NetworkId(1)).unwrap();
    net.is_connected = true;
    net.network_name = "Libera".into();
    net.channels.insert("#A".into());
    app
}

#[test]
fn double_clicking_a_channel_joins_or_parts() {
    let mut app = connected_app();
    let quick = Duration::from_millis(100);
    // Joined (names match case-insensitively): part.
    let effects = double_click(&mut app, TreeRow::Buffer(BufferId(10)), quick);
    assert_eq!(
        effects,
        [Effect::SendCommand {
            buffer: app.state.buffers[&BufferId(10)].clone(),
            text: "/part #a".into(),
        }]
    );
    // Not joined: switch to it, then join.
    let effects = double_click(&mut app, TreeRow::Buffer(BufferId(11)), quick);
    assert_eq!(app.active_buffer_id, Some(BufferId(11)));
    assert!(matches!(
        effects.last(),
        Some(Effect::SendCommand { text, .. }) if text == "/join #b"
    ));
    // Too slow, or on different rows: no toggle.
    let effects = double_click(
        &mut app,
        TreeRow::Buffer(BufferId(11)),
        Duration::from_secs(1),
    );
    assert!(effects.is_empty());
    let t = Instant::now();
    app.click_tree(TreeRow::Buffer(BufferId(10)), t);
    let effects = app.click_tree(TreeRow::Buffer(BufferId(11)), t);
    assert!(
        !effects
            .iter()
            .any(|e| matches!(e, Effect::SendCommand { .. }))
    );
    // Queries have nothing to toggle.
    let effects = double_click(&mut app, TreeRow::Buffer(BufferId(12)), quick);
    assert!(
        !effects
            .iter()
            .any(|e| matches!(e, Effect::SendCommand { .. }))
    );
}

#[test]
fn double_clicking_a_network_connects_or_disconnects() {
    let mut app = connected_app();
    let quick = Duration::from_millis(100);
    assert_eq!(
        double_click(&mut app, TreeRow::Network(NetworkId(1)), quick),
        [Effect::SetNetworkConnected {
            network: NetworkId(1),
            connect: false,
        }]
    );
    // A third click starts a new pair.
    assert!(
        app.click_tree(TreeRow::Network(NetworkId(1)), Instant::now())
            .is_empty()
    );
    let mut fresh = connected_app();
    fresh
        .state
        .networks
        .get_mut(&NetworkId(1))
        .unwrap()
        .is_connected = false;
    // The status buffer acts for its network.
    fresh
        .state
        .buffers
        .insert(BufferId(9), info(9, 1, BufferType::Status, ""));
    let effects = double_click(&mut fresh, TreeRow::Buffer(BufferId(9)), quick);
    assert_eq!(
        effects.last(),
        Some(&Effect::SetNetworkConnected {
            network: NetworkId(1),
            connect: true,
        })
    );
    // Can't join on a disconnected network.
    let effects = double_click(&mut fresh, TreeRow::Buffer(BufferId(11)), quick);
    assert!(
        !effects
            .iter()
            .any(|e| matches!(e, Effect::SendCommand { .. }))
    );
}

fn join(id: i64, buffer: i32) -> IrcMessage {
    IrcMessage {
        kind: MessageType::Join,
        ..message(id, buffer, MessageFlags::NONE)
    }
}

#[test]
fn hidden_joins_parts_dont_mark_activity_or_take_the_marker() {
    let mut app = live_app();
    app.set_display(crate::app::format::DisplaySettings {
        hide_joins_parts: true,
        ..DisplaySettings::default()
    });
    app.on_client_event(&opened());
    typing(&mut app);
    deliver(&mut app, join(1, 11));
    assert!(app.buffer_activity.is_empty());
    deliver(&mut app, message(2, 11, MessageFlags::NONE));
    assert_eq!(
        app.buffer_activity.get(&BufferId(11)),
        Some(&Activity::Message)
    );

    deliver(&mut app, message(3, 10, MessageFlags::NONE));
    deliver(&mut app, join(4, 10));
    let effects = app.on_key(key(KeyCode::Enter));
    assert_eq!(effects, [Effect::SetMarkerLine(BufferId(10), MsgId(3))]);
}

fn older(buffer: i32, before: i64) -> Effect {
    Effect::RequestOlderBacklog {
        buffer: BufferId(buffer),
        before: MsgId(before),
        limit: DEFAULT_BACKLOG_LIMIT,
    }
}

/// Simulate a backlog reply: merge messages, then tell the app.
fn backlog_reply(app: &mut App, buffer: i32, ids: std::ops::RangeInclusive<i64>) -> Vec<Effect> {
    let received = ids.clone().count();
    let oldest = Some(MsgId(*ids.start()));
    let list = app.state.messages.entry(BufferId(buffer)).or_default();
    let before = list.len();
    for id in ids {
        if !list.iter().any(|m| m.msg_id == MsgId(id)) {
            list.push(message(id, buffer, MessageFlags::NONE));
        }
    }
    list.sort_by_key(|m| m.msg_id);
    let count = list.len() - before;
    app.state.enforce_cap(BufferId(buffer));
    app.on_client_event(&ClientEvent::BacklogReceived {
        buffer_id: BufferId(buffer),
        count,
        received,
        oldest,
    })
}

/// Drop the connection and open a new session over the same state, the
/// way the dispatcher's re-seed would. Returns the session's effects.
fn drop_and_reopen(app: &mut App) -> Vec<Effect> {
    app.on_client_event(&disconnected("gone"));
    assert_eq!(app.reconnect(), [Effect::Reconnect]);
    app.state.backlog_requested.clear();
    app.on_client_event(&opened())
}

fn gap(buffer: i32, after: i64, before: Option<i64>) -> Effect {
    Effect::FillGap {
        buffer: BufferId(buffer),
        after: MsgId(after),
        before: before.map(MsgId),
        limit: DEFAULT_BACKLOG_LIMIT,
    }
}

#[test]
fn a_reconnect_fills_the_messages_missed_meanwhile() {
    let mut app = live_app();
    app.on_client_event(&opened());
    backlog_reply(&mut app, 10, 1..=5);
    app.log.set_viewport(80, 5);
    app.set_focus(Focus::Log);

    // Only what came after the newest message we have is asked for.
    assert_eq!(drop_and_reopen(&mut app), [gap(10, 5, None)]);
    // A full page that doesn't reach back to it: ask for the next page
    // down, under the oldest message just received.
    assert_eq!(
        backlog_reply(&mut app, 10, 301..=400),
        [gap(10, 5, Some(301))]
    );
    // Older history waits until the gap is filled, so replies can't be
    // mistaken for each other.
    app.on_key(key(KeyCode::Char('g')));
    assert!(app.on_key(key(KeyCode::Char('g'))).is_empty());
    assert!(app.older_pending.is_empty());
    // A short page means the core has nothing more in between.
    assert!(backlog_reply(&mut app, 10, 250..=300).is_empty());
    assert!(app.gaps.is_empty());
    assert_eq!(app.on_key(key(KeyCode::Char('g'))), [older(10, 1)]);
}

#[test]
fn a_gap_page_that_reaches_the_old_history_ends_the_fill() {
    let mut app = live_app();
    app.on_client_event(&opened());
    backlog_reply(&mut app, 10, 1..=5);
    assert_eq!(drop_and_reopen(&mut app), [gap(10, 5, None)]);
    // Exactly a page, starting at the newest message we already had.
    assert!(backlog_reply(&mut app, 10, 5..=104).is_empty());
    assert!(app.gaps.is_empty());
}

#[test]
fn other_buffers_fill_their_gap_when_visited() {
    let mut app = live_app();
    app.on_client_event(&opened());
    backlog_reply(&mut app, 10, 1..=5);
    app.select_buffer(BufferId(11));
    backlog_reply(&mut app, 11, 20..=30);
    app.select_buffer(BufferId(10));

    drop_and_reopen(&mut app);
    // Live traffic before the visit doesn't move the gap's lower end.
    deliver(&mut app, message(500, 11, MessageFlags::NONE));
    let effects = app.select_buffer(BufferId(11));
    assert_eq!(effects[0], gap(11, 30, None));
}

#[test]
fn a_failed_gap_fill_is_retried_on_the_next_visit() {
    let mut app = live_app();
    app.on_client_event(&opened());
    backlog_reply(&mut app, 10, 1..=5);
    drop_and_reopen(&mut app);
    backlog_reply(&mut app, 10, 301..=400);
    app.gap_failed(BufferId(10), "boom");
    assert!(app.notices.last().unwrap().contains("boom"));
    assert!(app.gaps.contains_key(&BufferId(10)));

    app.select_buffer(BufferId(11));
    let effects = app.select_buffer(BufferId(10));
    assert_eq!(effects[0], gap(10, 5, None));
}

#[test]
fn a_gap_too_big_for_the_cap_stops_once_the_old_history_is_gone() {
    let mut app = live_app();
    app.on_client_event(&opened());
    backlog_reply(&mut app, 10, 1..=5);
    drop_and_reopen(&mut app);
    // The first page alone pushes everything from before the drop past the
    // cap; the gap went with it.
    app.state.max_messages_per_buffer = 100;
    assert!(backlog_reply(&mut app, 10, 301..=400).is_empty());
    assert!(app.gaps.is_empty());
}

#[test]
fn scrolling_past_the_top_fetches_older_history() {
    let mut app = live_app();
    app.on_client_event(&opened());
    backlog_reply(&mut app, 10, 100..=110);
    app.log.set_viewport(80, 5);
    app.set_focus(Focus::Log);

    // Moving up within what's loaded fetches nothing.
    assert!(app.on_key(key(KeyCode::Up)).is_empty());
    // Reaching the top asks for messages before the oldest one, once.
    assert_eq!(app.on_key(key(KeyCode::Char('g'))), [older(10, 100)]);
    assert!(app.on_key(key(KeyCode::Up)).is_empty());
    assert_eq!(app.history_status(), Some("loading older messages…"));

    // The older page arrives; the reader is still on msg 100 and the next
    // request goes further back.
    backlog_reply(&mut app, 10, 90..=99);
    assert_eq!(app.log.highlighted, Some(MsgId(100)));
    assert_eq!(app.history_status(), None);
    app.on_key(key(KeyCode::Char('g')));
    assert_eq!(app.on_key(key(KeyCode::Up)), Vec::<Effect>::new());
    // msg 90 is now first; PgUp at the top asks for the page before it.
    let effects = app.on_key(key(KeyCode::PageUp));
    assert!(
        effects.is_empty() || effects == [older(10, 90)],
        "{effects:?}"
    );
    app.on_key(key(KeyCode::Char('g')));
    assert!(app.older_pending.contains_key(&BufferId(10)));

    // An empty reply means the core has nothing older.
    backlog_reply(&mut app, 10, 95..=95);
    assert_eq!(app.history_status(), Some("start of history"));
    assert!(app.on_key(key(KeyCode::Char('g'))).is_empty());
    assert!(app.scroll_log(-3).is_empty());
}

#[test]
fn initial_backlog_reply_isnt_mistaken_for_older_history() {
    let mut app = live_app();
    app.on_client_event(&opened());
    deliver(&mut app, message(50, 10, MessageFlags::NONE));
    app.log.set_viewport(80, 5);
    app.set_focus(Focus::Log);
    // Fits on screen, so the view is at the top: Up asks for older.
    assert_eq!(app.on_key(key(KeyCode::Up)), [older(10, 50)]);
    // The initial backlog lands first with nothing new: still pending.
    backlog_reply(&mut app, 10, 50..=50);
    assert!(app.older_pending.contains_key(&BufferId(10)));
    assert_ne!(app.history_status(), Some("start of history"));
    // Then ours.
    backlog_reply(&mut app, 10, 40..=49);
    assert!(app.older_pending.is_empty());
}

#[test]
fn older_history_needs_a_live_connection_and_room() {
    let mut app = live_app();
    app.on_client_event(&opened());
    backlog_reply(&mut app, 10, 1..=3);
    app.set_focus(Focus::Log);
    // Only as much as fits under the cap is requested.
    app.state.max_messages_per_buffer = 5;
    assert_eq!(
        app.on_key(key(KeyCode::Char('g'))),
        [Effect::RequestOlderBacklog {
            buffer: BufferId(10),
            before: MsgId(1),
            limit: 2,
        }]
    );
    app.older_backlog_failed(BufferId(10), "x");
    app.state.max_messages_per_buffer = 3;
    assert!(app.on_key(key(KeyCode::Char('g'))).is_empty());
    assert_eq!(app.history_status(), Some("history limit reached"));

    app.state.max_messages_per_buffer = 0;
    app.on_client_event(&disconnected("gone"));
    assert!(app.on_key(key(KeyCode::Char('g'))).is_empty());

    let mut demo = App::new(state(), false, false);
    demo.state
        .messages
        .get_mut(&BufferId(10))
        .unwrap()
        .push(message(1, 10, MessageFlags::NONE));
    demo.set_focus(Focus::Log);
    assert!(demo.on_key(key(KeyCode::Char('g'))).is_empty());
}

#[test]
fn failed_older_request_can_be_retried() {
    let mut app = live_app();
    app.on_client_event(&opened());
    backlog_reply(&mut app, 10, 5..=6);
    app.set_focus(Focus::Log);
    assert_eq!(app.on_key(key(KeyCode::Char('g'))), [older(10, 5)]);
    app.older_backlog_failed(BufferId(10), "boom");
    assert!(app.notices.last().unwrap().contains("older history"));
    assert_eq!(app.on_key(key(KeyCode::Char('g'))), [older(10, 5)]);
}

#[test]
fn yy_copies_the_highlighted_message() {
    let mut app = live_app();
    app.on_client_event(&opened());
    for i in 1..=3 {
        deliver(&mut app, message(i, 10, MessageFlags::NONE));
    }
    app.set_focus(Focus::Log);
    app.on_key(key(KeyCode::Up));
    let expected = format_message_plain(&app.state.messages[&BufferId(10)][1]);

    // One y does nothing; another key in between cancels it.
    assert!(app.on_key(key(KeyCode::Char('y'))).is_empty());
    assert!(app.on_key(key(KeyCode::Char('G'))).is_empty());
    app.on_key(key(KeyCode::Up));
    assert!(app.on_key(key(KeyCode::Char('y'))).is_empty());
    assert_eq!(
        app.on_key(key(KeyCode::Char('y'))),
        [Effect::Copy(expected)]
    );
    // A third y starts over.
    assert!(app.on_key(key(KeyCode::Char('y'))).is_empty());
    // Global keys and the mouse in between cancel it too.
    app.on_key(key(KeyCode::PageUp));
    assert!(app.on_key(key(KeyCode::Char('y'))).is_empty());
    app.scroll_log(1);
    assert!(app.on_key(key(KeyCode::Char('y'))).is_empty());
    app.click_log(0);
    assert!(app.on_key(key(KeyCode::Char('y'))).is_empty());
}

#[test]
fn q_quits_in_normal_mode_only() {
    let mut app = live_app();
    app.on_client_event(&opened());
    typing(&mut app);
    type_text(&mut app, "q");
    assert_eq!(app.input.value, "q");
    assert!(app.exit.is_none());
    app.on_key(key(KeyCode::Esc));
    app.on_key(key(KeyCode::Char('q')));
    assert_eq!(app.exit.as_ref().map(|e| e.code), Some(0));
}

fn said(id: i64, buffer: i32, nick: &str) -> IrcMessage {
    IrcMessage {
        sender: format!("{nick}!u@h"),
        ..message(id, buffer, MessageFlags::NONE)
    }
}

fn tab(app: &mut App) {
    assert!(app.on_key(key(KeyCode::Tab)).is_empty());
}

#[test]
fn tab_completes_nicks() {
    let mut app = live_app();
    app.on_client_event(&opened());
    app.state.networks.get_mut(&NetworkId(1)).unwrap().my_nick = "me".into();
    // Buffer #a's channel object, named in a different case.
    let mut channel = IrcChannel::new("1/#A");
    for nick in ["alice", "Albert", "alfred", "bob", "me"] {
        channel.user_modes.insert(nick.into(), String::new());
    }
    app.state
        .channels
        .insert(channel.object_name.clone(), channel);
    deliver(&mut app, said(1, 10, "alice"));
    deliver(&mut app, said(2, 10, "Albert"));
    deliver(&mut app, said(3, 10, "me"));
    // Spoke, but has since left: not offered.
    deliver(&mut app, said(4, 10, "alvin"));
    typing(&mut app);

    // At the start of the line: the most recent speaker first, with ": ".
    type_text(&mut app, "al");
    tab(&mut app);
    assert_eq!(app.input.value, "Albert: ");
    // Tab again cycles: earlier speakers, then the rest alphabetically.
    tab(&mut app);
    assert_eq!(app.input.value, "alice: ");
    tab(&mut app);
    assert_eq!(app.input.value, "alfred: ");
    tab(&mut app);
    assert_eq!(app.input.value, "Albert: ");
    // Shift+Tab goes back.
    app.on_key(key(KeyCode::BackTab));
    assert_eq!(app.input.value, "alfred: ");
    assert_eq!(app.focus, Focus::Input);

    // Typing ends the completion; mid-line nicks get a plain space.
    type_text(&mut app, "ask B");
    tab(&mut app);
    assert_eq!(app.input.value, "alfred: ask bob ");
    assert_eq!(app.input.cursor, app.input.value.chars().count());

    // Our own nick and unknown prefixes complete to nothing.
    app.on_key(ctrl('u'));
    type_text(&mut app, "m");
    tab(&mut app);
    assert_eq!(app.input.value, "m");
    app.on_key(ctrl('u'));
    tab(&mut app);
    assert_eq!(app.input.value, "");
}

#[test]
fn tab_completion_without_a_member_list() {
    let mut app = live_app();
    app.on_client_event(&opened());
    deliver(&mut app, said(1, 10, "carol"));
    typing(&mut app);
    // No roster yet (or the demo): recent speakers still complete.
    type_text(&mut app, "hi c");
    tab(&mut app);
    assert_eq!(app.input.value, "hi carol ");
    // A query completes to its peer.
    app.select_buffer(BufferId(12));
    app.on_key(ctrl('u'));
    type_text(&mut app, "fr");
    tab(&mut app);
    assert_eq!(app.input.value, "friend: ");
}

#[test]
fn alerts_for_highlights_and_queries_out_of_sight() {
    let mut app = live_app();
    app.set_display(DisplaySettings {
        notify: true,
        ..DisplaySettings::default()
    });
    app.on_client_event(&opened());
    assert_eq!(app.active_buffer_id, Some(BufferId(10)));
    let alert = |title: &str, body: &str| Effect::Alert {
        title: title.into(),
        body: body.into(),
    };
    let flagged = |id, buffer, flags| IrcMessage {
        sender: "bob!u@h".into(),
        contents: "\x02hey\x02 you".into(),
        ..message(id, buffer, flags)
    };

    // Plain chatter elsewhere only marks the sidebar.
    assert!(deliver(&mut app, flagged(1, 11, MessageFlags::NONE)).is_empty());
    // A highlight elsewhere alerts, without mIRC formatting.
    assert_eq!(
        deliver(&mut app, flagged(2, 11, MessageFlags::HIGHLIGHT)),
        [alert("bob in #b", "hey you")]
    );
    // ...but not again for that buffer right away.
    assert!(deliver(&mut app, flagged(3, 11, MessageFlags::HIGHLIGHT)).is_empty());
    // Any private message counts.
    assert_eq!(
        deliver(&mut app, flagged(4, 12, MessageFlags::NONE)),
        [alert("bob", "hey you")]
    );
    // Our own lines never do.
    let own = MessageFlags::HIGHLIGHT | MessageFlags::SELF;
    app.select_buffer(BufferId(10));
    app.last_alert.clear();
    assert!(deliver(&mut app, flagged(5, 11, own)).is_empty());

    // In the buffer we're reading: only when the terminal is in the
    // background.
    assert!(deliver(&mut app, flagged(6, 10, MessageFlags::HIGHLIGHT)).is_empty());
    app.set_terminal_focus(false);
    assert_eq!(
        deliver(&mut app, flagged(7, 10, MessageFlags::HIGHLIGHT)),
        [alert("bob in #a", "hey you")]
    );

    // Off in the config: nothing.
    app.set_display(DisplaySettings::default());
    app.last_alert.clear();
    assert!(deliver(&mut app, flagged(8, 11, MessageFlags::HIGHLIGHT)).is_empty());
}

#[test]
fn starts_in_normal_mode() {
    let mut app = live_app();
    app.on_client_event(&opened());
    assert_eq!(app.focus, Focus::Log);
    // Keys navigate rather than type.
    deliver(&mut app, message(1, 10, MessageFlags::NONE));
    app.on_key(key(KeyCode::Char('j')));
    assert_eq!(app.active_buffer_id, Some(BufferId(11)));
    assert!(app.input.value.is_empty());
    // Without a draft, a reconnect stays in normal mode.
    app.on_client_event(&disconnected("gone"));
    app.reconnect();
    assert_eq!(app.focus, Focus::Log);
}

#[test]
fn notices_are_capped() {
    let mut app = live_app();
    for i in 0..MAX_NOTICES + 5 {
        app.notify(&format!("n{i}"), Severity::Info);
    }
    assert_eq!(app.notices.len(), MAX_NOTICES);
    assert_eq!(app.notices[0], "n5");
    assert_eq!(app.toasts.len(), 4);
}

#[test]
fn unanswered_history_requests_expire() {
    let mut app = live_app();
    app.on_client_event(&opened());
    backlog_reply(&mut app, 10, 5..=6);
    app.set_focus(Focus::Log);
    assert_eq!(app.on_key(key(KeyCode::Char('g'))), [older(10, 5)]);
    // Not yet.
    assert!(!app.tick(Instant::now() + Duration::from_secs(1)));
    assert_eq!(app.history_status(), Some("loading older messages…"));
    // The reply never comes: give up, say so, and allow a retry.
    assert!(app.tick(Instant::now() + Duration::from_secs(61)));
    assert!(app.older_pending.is_empty());
    assert_ne!(app.history_status(), Some("loading older messages…"));
    assert!(app.notices.last().unwrap().contains("history request"));
    assert_eq!(app.on_key(key(KeyCode::Char('g'))), [older(10, 5)]);
}
