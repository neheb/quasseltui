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

    // Esc leaves typing; j/k and the arrows move within the channel
    // without typing anything.
    type_text(&mut app, "draft");
    app.on_key(key(KeyCode::Esc));
    assert_eq!(app.focus, Focus::Log);
    assert_eq!(app.log.highlighted, Some(MsgId(40)));
    app.on_key(key(KeyCode::Char('k')));
    app.on_key(key(KeyCode::Up));
    app.on_key(key(KeyCode::Char('j')));
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

    // J/K and Shift+arrows switch channels immediately.
    app.on_key(shift('J'));
    assert_eq!(app.active_buffer_id, Some(BufferId(11)));
    app.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::SHIFT));
    assert_eq!(app.active_buffer_id, Some(BufferId(12)));
    app.on_key(shift('K'));
    app.on_key(KeyEvent::new(KeyCode::Up, KeyModifiers::SHIFT));
    assert_eq!(app.active_buffer_id, Some(BufferId(10)));

    // i (or Esc, or Tab) goes back to typing.
    app.on_key(key(KeyCode::Char('i')));
    assert_eq!(app.focus, Focus::Input);
    type_text(&mut app, "!");
    assert_eq!(app.input.value, "draft!");
    app.on_key(key(KeyCode::Tab));
    assert_eq!(app.focus, Focus::Log);
    app.on_key(key(KeyCode::Tab));
    assert_eq!(app.focus, Focus::Input);
}

#[test]
fn letters_type_while_typing() {
    let mut app = live_app();
    app.on_client_event(&opened());
    type_text(&mut app, "jkJK");
    assert_eq!(app.input.value, "jkJK");
    assert_eq!(app.active_buffer_id, Some(BufferId(10)));
}

#[test]
fn clicking_a_buffer_switches_without_changing_mode() {
    let mut app = live_app();
    app.on_client_event(&opened());
    app.click_buffer(BufferId(12));
    assert_eq!(app.active_buffer_id, Some(BufferId(12)));
    assert_eq!(app.focus, Focus::Input);
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
    });
    app.on_client_event(&opened());
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
    }
}

/// Simulate a backlog reply: merge messages, then tell the app.
fn backlog_reply(app: &mut App, buffer: i32, ids: std::ops::RangeInclusive<i64>) -> Vec<Effect> {
    let list = app.state.messages.entry(BufferId(buffer)).or_default();
    let before = list.len();
    for id in ids {
        if !list.iter().any(|m| m.msg_id == MsgId(id)) {
            list.push(message(id, buffer, MessageFlags::NONE));
        }
    }
    list.sort_by_key(|m| m.msg_id);
    let count = list.len() - before;
    app.on_client_event(&ClientEvent::BacklogReceived {
        buffer_id: BufferId(buffer),
        count,
    })
}

#[test]
fn scrolling_past_the_top_fetches_older_history() {
    let mut app = live_app();
    app.on_client_event(&opened());
    backlog_reply(&mut app, 10, 100..=110);
    app.log.set_viewport(80, 5);
    app.set_focus(Focus::Log);

    // Moving up within what's loaded fetches nothing.
    assert!(app.on_key(key(KeyCode::Char('k'))).is_empty());
    // Reaching the top asks for messages before the oldest one, once.
    assert_eq!(app.on_key(key(KeyCode::Char('g'))), [older(10, 100)]);
    assert!(app.on_key(key(KeyCode::Char('k'))).is_empty());
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
    assert!(app.older_pending.contains(&BufferId(10)));

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
    // Fits on screen, so the view is at the top: k asks for older.
    assert_eq!(app.on_key(key(KeyCode::Char('k'))), [older(10, 50)]);
    // The initial backlog lands first with nothing new: still pending.
    backlog_reply(&mut app, 10, 50..=50);
    assert!(app.older_pending.contains(&BufferId(10)));
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
