//! The UI state machine.
//!
//! [`App`] owns the `ClientState` and everything on screen. It reacts to
//! client events, key presses, and the results of earlier requests, and
//! returns [`Effect`]s (requests to the core) for the runtime to carry out.
//! It does no I/O itself, so the whole interaction model is unit-testable.
//!
//! Rules carried over from the Python client:
//!
//! - A disconnect before the session opened is fatal on the first
//!   connection (exit with the reason). After that it enters a visible
//!   disconnected state: history stays on screen, the input is disabled,
//!   and Ctrl+R reconnects over the same state.
//! - The active buffer is picked once at startup (where the activity is)
//!   and afterwards only changes on the user's request, or when the active
//!   buffer is removed.
//! - Messages for other buffers mark them in the sidebar ("highlight" for
//!   highlights and queries, sticky until visited); the user's own echoed
//!   lines don't.
//! - Read markers and read state round-trip through the core.

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::app::format::{DisplaySettings, ordered_buffer_ids, pick_default_buffer};
use crate::app::log_view::LogView;
use crate::client::{ClientState, IrcMessage};
use crate::protocol::types::{BufferId, BufferInfo, BufferType, MessageFlags, MsgId};
use crate::sync::events::ClientEvent;
use crate::util::text::sanitize_and_truncate;

/// Longest disconnect reason or notice shown. A hostile core can send an
/// arbitrarily long error, and escaping control bytes can quadruple it.
pub const MAX_REASON_LEN: usize = 400;
const HISTORY_LIMIT: usize = 100;
pub const DEFAULT_PLACEHOLDER: &str = "Type a message and press Enter…";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Info,
    Warning,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Toast {
    pub text: String,
    pub severity: Severity,
    pub shown_at: Instant,
}

impl Toast {
    fn lifetime(&self) -> Duration {
        match self.severity {
            Severity::Error => Duration::from_secs(10),
            _ => Duration::from_secs(5),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    /// Typing in the input bar.
    Input,
    /// Normal mode: keys navigate the current channel and the channel list.
    Log,
}

/// Unread level of a non-active buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Activity {
    Message,
    Highlight,
}

/// A request to the core, carried out by the runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    SendInput {
        buffer: BufferInfo,
        text: String,
    },
    RequestBacklog(BufferId),
    /// Fetch history older than `before` (the oldest message we have).
    RequestOlderBacklog {
        buffer: BufferId,
        before: MsgId,
    },
    SetLastSeen(BufferId, MsgId),
    SetMarkerLine(BufferId, MsgId),
    /// Replace the client with a fresh connection over the same state.
    Reconnect,
}

/// How the app wants to exit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Exit {
    pub code: i32,
    /// Printed after the terminal is restored.
    pub message: Option<String>,
}

/// The single-line input with per-session history.
#[derive(Debug, Clone)]
pub struct InputBar {
    pub value: String,
    /// Cursor position in chars.
    pub cursor: usize,
    pub disabled: bool,
    pub placeholder: String,
    history: Vec<String>,
    /// `None`: editing the live prompt; otherwise the history entry shown.
    history_index: Option<usize>,
    /// The draft stashed when history browsing started.
    history_stash: String,
}

impl Default for InputBar {
    fn default() -> Self {
        Self {
            value: String::new(),
            cursor: 0,
            disabled: false,
            placeholder: DEFAULT_PLACEHOLDER.into(),
            history: Vec::new(),
            history_index: None,
            history_stash: String::new(),
        }
    }
}

impl InputBar {
    pub fn set_value(&mut self, value: impl Into<String>) {
        self.value = value.into();
        self.cursor = self.value.chars().count();
    }

    fn byte_index(&self, char_index: usize) -> usize {
        self.value
            .char_indices()
            .nth(char_index)
            .map_or(self.value.len(), |(i, _)| i)
    }

    pub fn insert(&mut self, text: &str) {
        let at = self.byte_index(self.cursor);
        self.value.insert_str(at, text);
        self.cursor += text.chars().count();
    }

    fn delete_range(&mut self, start: usize, end: usize) {
        let (a, b) = (self.byte_index(start), self.byte_index(end));
        self.value.replace_range(a..b, "");
        self.cursor = start;
    }

    fn backspace(&mut self) {
        if self.cursor > 0 {
            self.delete_range(self.cursor - 1, self.cursor);
        }
    }

    fn delete(&mut self) {
        if self.cursor < self.value.chars().count() {
            self.delete_range(self.cursor, self.cursor + 1);
        }
    }

    fn delete_word_back(&mut self) {
        let chars: Vec<char> = self.value.chars().collect();
        let mut start = self.cursor;
        while start > 0 && chars[start - 1] == ' ' {
            start -= 1;
        }
        while start > 0 && chars[start - 1] != ' ' {
            start -= 1;
        }
        self.delete_range(start, self.cursor);
    }

    fn remember(&mut self, text: &str) {
        if self.history.last().map(String::as_str) != Some(text) {
            self.history.push(text.to_string());
            if self.history.len() > HISTORY_LIMIT {
                let excess = self.history.len() - HISTORY_LIMIT;
                self.history.drain(..excess);
            }
        }
        self.history_index = None;
        self.history_stash.clear();
    }

    pub fn history_prev(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let index = match self.history_index {
            None => {
                self.history_stash = self.value.clone();
                self.history.len() - 1
            }
            Some(i) => i.saturating_sub(1),
        };
        self.history_index = Some(index);
        let entry = self.history[index].clone();
        self.set_value(entry);
    }

    pub fn history_next(&mut self) {
        let Some(index) = self.history_index else {
            return;
        };
        if index + 1 >= self.history.len() {
            self.history_index = None;
            let stash = std::mem::take(&mut self.history_stash);
            self.set_value(stash);
        } else {
            self.history_index = Some(index + 1);
            let entry = self.history[index + 1].clone();
            self.set_value(entry);
        }
    }
}

pub struct App {
    pub state: ClientState,
    pub active_buffer_id: Option<BufferId>,
    /// A mid-session disconnect happened; cleared by a reconnect.
    pub connection_lost: bool,
    /// Every user-facing notice, sanitized. Toasts disappear; this doesn't.
    pub notices: Vec<String>,
    pub toasts: VecDeque<Toast>,
    pub buffer_activity: HashMap<BufferId, Activity>,
    pub input: InputBar,
    pub log: LogView,
    pub focus: Focus,
    pub show_help: bool,
    pub exit: Option<Exit>,
    /// Connected to a core (false for the demo).
    live: bool,
    reconnectable: bool,
    /// Input text stashed at disconnect, restored on reconnect.
    pending_input: Option<String>,
    /// Backlog requests in flight. The core answers them in the order they
    /// were sent, so a reply belongs to the initial request if one is
    /// pending for that buffer, and to the older-history request otherwise.
    initial_pending: HashSet<BufferId>,
    pub(crate) older_pending: HashSet<BufferId>,
    /// Buffers whose history the core has no more of.
    history_start: HashSet<BufferId>,
    // Bridge state for the current connection attempt.
    ever_active: bool,
    session_opened: bool,
    presession_fatal: bool,
}

impl App {
    /// `live`: a client is attached. `reconnectable`: Ctrl+R can build a
    /// replacement client.
    pub fn new(state: ClientState, live: bool, reconnectable: bool) -> Self {
        let mut app = Self {
            state,
            active_buffer_id: None,
            connection_lost: false,
            notices: Vec::new(),
            toasts: VecDeque::new(),
            buffer_activity: HashMap::new(),
            input: InputBar::default(),
            log: LogView::default(),
            focus: Focus::Input,
            show_help: false,
            exit: None,
            live,
            reconnectable,
            pending_input: None,
            initial_pending: HashSet::new(),
            older_pending: HashSet::new(),
            history_start: HashSet::new(),
            ever_active: false,
            session_opened: false,
            presession_fatal: true,
        };
        if !live {
            // The demo has no events to pick a buffer; show content now.
            if let Some(id) = pick_default_buffer(&app.state) {
                app.active_buffer_id = Some(id);
                app.active_updated(Some(id));
            }
        }
        app
    }

    /// Apply display preferences (from the config file).
    pub fn set_display(&mut self, settings: DisplaySettings) {
        self.log.settings = settings;
    }

    pub fn is_live(&self) -> bool {
        self.live
    }

    // -- notices ---------------------------------------------------------------

    /// Record a notice and show it as a toast. Notices can embed untrusted
    /// core text, so they are sanitized and length-bounded.
    pub fn notify(&mut self, message: &str, severity: Severity) {
        let safe = sanitize_and_truncate(message, MAX_REASON_LEN);
        self.notices.push(safe.clone());
        self.toasts.push_back(Toast {
            text: safe,
            severity,
            shown_at: Instant::now(),
        });
        while self.toasts.len() > 4 {
            self.toasts.pop_front();
        }
    }

    /// Drop expired toasts. Returns whether anything changed.
    pub fn tick(&mut self, now: Instant) -> bool {
        let before = self.toasts.len();
        self.toasts
            .retain(|t| now.saturating_duration_since(t.shown_at) < t.lifetime());
        before != self.toasts.len()
    }

    // -- client events (the bridge) -------------------------------------------

    pub fn on_client_event(&mut self, event: &ClientEvent) -> Vec<Effect> {
        match event {
            ClientEvent::SessionOpened { .. } => {
                self.session_opened = true;
                if let Some(active) = self.active_buffer_id {
                    // Reconnect: keep the user's buffer and refresh it, which
                    // re-requests the backlog to fill the gap since the drop.
                    self.ever_active = true;
                    self.active_updated(Some(active))
                } else {
                    self.maybe_pick_default()
                }
            }
            ClientEvent::BufferAdded { .. } | ClientEvent::BufferRenamed { .. } => {
                self.maybe_pick_default()
            }
            ClientEvent::BufferRemoved { buffer_id } => {
                self.buffer_activity.remove(buffer_id);
                self.initial_pending.remove(buffer_id);
                self.older_pending.remove(buffer_id);
                self.history_start.remove(buffer_id);
                if self.active_buffer_id == Some(*buffer_id) {
                    let replacement = pick_default_buffer(&self.state);
                    self.active_buffer_id = replacement;
                    self.active_updated(replacement)
                } else {
                    self.maybe_pick_default()
                }
            }
            ClientEvent::MessageReceived(message) => self.handle_message(message),
            ClientEvent::BacklogReceived { buffer_id, count } => {
                self.older_backlog_arrived(*buffer_id, *count);
                if Some(*buffer_id) == self.active_buffer_id && *count > 0 {
                    self.active_updated(Some(*buffer_id))
                } else {
                    Vec::new()
                }
            }
            ClientEvent::Disconnected { reason, .. } => {
                let fatal = !self.session_opened && self.presession_fatal;
                self.session_ended(reason, fatal);
                Vec::new()
            }
            ClientEvent::NetworkAdded { .. }
            | ClientEvent::NetworkUpdated { .. }
            | ClientEvent::NetworkRemoved { .. }
            | ClientEvent::IdentityAdded { .. } => Vec::new(),
        }
    }

    fn maybe_pick_default(&mut self) -> Vec<Effect> {
        if self.active_buffer_id.is_some() {
            self.ever_active = true;
            return Vec::new();
        }
        let Some(id) = pick_default_buffer(&self.state) else {
            return Vec::new();
        };
        self.active_buffer_id = Some(id);
        self.ever_active = true;
        self.active_updated(Some(id))
    }

    fn handle_message(&mut self, message: &IrcMessage) -> Vec<Effect> {
        let buffer = message.buffer_id;
        let mut effects = Vec::new();
        // Cold start: land where the activity is. Once a buffer has been
        // active, a stray message never moves the user.
        if self.active_buffer_id.is_none() && !self.ever_active {
            self.active_buffer_id = Some(buffer);
            self.ever_active = true;
            effects.extend(self.active_updated(Some(buffer)));
        }
        if Some(buffer) != self.active_buffer_id
            && !message.flags.contains(MessageFlags::SELF)
            && self.log.settings.shows(message)
        {
            let highlight = message.flags.contains(MessageFlags::HIGHLIGHT)
                || self
                    .state
                    .buffers
                    .get(&buffer)
                    .is_some_and(|info| info.kind == BufferType::Query);
            self.mark_activity(buffer, highlight);
        }
        effects
    }

    fn mark_activity(&mut self, buffer: BufferId, highlight: bool) {
        if Some(buffer) == self.active_buffer_id {
            return;
        }
        let level = if highlight {
            Activity::Highlight
        } else {
            Activity::Message
        };
        let entry = self.buffer_activity.entry(buffer).or_insert(level);
        if *entry != Activity::Highlight {
            *entry = level;
        }
    }

    /// The active buffer was set or refreshed: point the log at it, sync
    /// the sidebar cursor, consume its unread mark, and fetch its history
    /// once per session.
    fn active_updated(&mut self, buffer: Option<BufferId>) -> Vec<Effect> {
        self.log.set_buffer(buffer);
        let mut effects = Vec::new();
        if let Some(id) = buffer {
            self.buffer_activity.remove(&id);
            // No history requests once the socket is gone: they'd fail and
            // spam notices on every post-drop buffer switch.
            if self.live && !self.connection_lost && self.state.claim_backlog(id) {
                self.initial_pending.insert(id);
                effects.push(Effect::RequestBacklog(id));
            }
        }
        effects
    }

    /// User-driven switch. Reports the buffer being left as read up to its
    /// newest message, which clears its unread state in other clients.
    pub fn select_buffer(&mut self, buffer: BufferId) -> Vec<Effect> {
        if Some(buffer) == self.active_buffer_id {
            return Vec::new();
        }
        let previous = self.active_buffer_id.replace(buffer);
        let mut effects = self.active_updated(Some(buffer));
        if let Some(prev) = previous
            && self.live
            && !self.connection_lost
            && let Some(last) = self.state.messages_for_buffer(prev).last()
        {
            effects.push(Effect::SetLastSeen(prev, last.msg_id));
        }
        effects
    }

    pub fn cycle_buffer(&mut self, delta: isize) -> Vec<Effect> {
        let ordered = ordered_buffer_ids(&self.state);
        if ordered.is_empty() {
            return Vec::new();
        }
        let target = match self
            .active_buffer_id
            .and_then(|a| ordered.iter().position(|id| *id == a))
        {
            None => ordered[0],
            Some(i) => ordered[(i as isize + delta).rem_euclid(ordered.len() as isize) as usize],
        };
        self.select_buffer(target)
    }

    fn session_ended(&mut self, reason: &str, fatal: bool) {
        let safe = sanitize_and_truncate(reason, MAX_REASON_LEN);
        tracing::warn!("session ended: {safe}");
        if !self.live {
            return;
        }
        if fatal {
            self.exit = Some(Exit {
                code: 1,
                message: Some(format!("quasseltui: {safe}")),
            });
            return;
        }
        self.enter_disconnected(&safe);
    }

    /// Latch the disconnected state and make it visible. Typed text is
    /// stashed so the placeholder shows, and restored on reconnect.
    fn enter_disconnected(&mut self, reason: &str) {
        if self.connection_lost {
            return;
        }
        self.connection_lost = true;
        self.notify(&format!("Disconnected: {reason}"), Severity::Error);
        self.input.disabled = true;
        if !self.input.value.is_empty() {
            self.pending_input = Some(std::mem::take(&mut self.input.value));
            self.input.cursor = 0;
        }
        self.input.placeholder =
            format!("Disconnected: {reason} — Ctrl+R to reconnect, Ctrl+Q to quit");
        if self.focus == Focus::Input {
            self.focus = Focus::Log;
        }
    }

    /// Ctrl+R. Only acts when the connection is actually lost: tearing
    /// down a healthy session would be a destructive misclick.
    pub fn reconnect(&mut self) -> Vec<Effect> {
        if !self.live || !self.reconnectable {
            return Vec::new();
        }
        if !self.connection_lost {
            self.notify("Already connected", Severity::Info);
            return Vec::new();
        }
        self.connection_lost = false;
        // Requests on the old connection will never be answered.
        self.initial_pending.clear();
        self.older_pending.clear();
        self.input.disabled = false;
        self.input.placeholder = DEFAULT_PLACEHOLDER.into();
        if let Some(text) = self.pending_input.take()
            && self.input.value.is_empty()
        {
            self.input.set_value(text);
        }
        self.focus = Focus::Input;
        self.notify("Reconnecting…", Severity::Info);
        // A still-down core must re-enter the disconnected state rather
        // than exit and destroy the scrollback.
        self.presession_fatal = false;
        self.session_opened = false;
        self.ever_active = false;
        vec![Effect::Reconnect]
    }

    // -- input ------------------------------------------------------------------

    fn submit(&mut self) -> Vec<Effect> {
        if self.input.disabled {
            return Vec::new();
        }
        let text = self.input.value.clone();
        if text.is_empty() {
            return self.marker_to_latest();
        }
        // A stray space+Enter must neither send a blank-looking line nor
        // move the marker.
        self.input.set_value("");
        if text.trim().is_empty() {
            return Vec::new();
        }
        self.input.remember(&text);
        self.line_submitted(text)
    }

    fn line_submitted(&mut self, text: String) -> Vec<Effect> {
        if !self.live {
            return Vec::new();
        }
        let Some(active) = self.active_buffer_id else {
            self.restore_input(text);
            self.notify(
                "No active buffer yet — still connecting; try again in a moment",
                Severity::Warning,
            );
            return Vec::new();
        };
        match self.state.buffers.get(&active) {
            Some(info) => vec![Effect::SendInput {
                buffer: info.clone(),
                text,
            }],
            None => {
                self.send_failed(text, &format!("cannot send to unknown buffer {active}"));
                Vec::new()
            }
        }
    }

    /// A send failed: give the text back and say why.
    pub fn send_failed(&mut self, text: String, error: &str) {
        tracing::warn!("send_input failed: {error}");
        self.restore_input(text);
        self.notify(&format!("Message not sent: {error}"), Severity::Warning);
    }

    /// Put text back in the input unless the user already typed something
    /// new. While disconnected it's stashed for the reconnect instead.
    fn restore_input(&mut self, text: String) {
        if self.connection_lost {
            self.pending_input.get_or_insert(text);
            return;
        }
        if self.input.value.is_empty() {
            self.input.set_value(text);
        }
    }

    /// Route a backlog reply to the request it answers (see
    /// `initial_pending`). An older-history reply with nothing new means the
    /// core has no more.
    fn older_backlog_arrived(&mut self, buffer: BufferId, count: usize) {
        if self.initial_pending.remove(&buffer) {
            return;
        }
        if self.older_pending.remove(&buffer) && count == 0 {
            self.history_start.insert(buffer);
        }
    }

    /// After a move upward: at the top of what we have, fetch the next page
    /// of older history from the core.
    fn fetch_older_if_at_top(&mut self) -> Vec<Effect> {
        let Some(buffer) = self.log.buffer else {
            return Vec::new();
        };
        if !self.live
            || self.connection_lost
            || self.older_pending.contains(&buffer)
            || self.history_start.contains(&buffer)
            || self.log.top_line(&self.state) != 0
        {
            return Vec::new();
        }
        let messages = self.state.messages_for_buffer(buffer);
        // An empty buffer is the initial backlog request's job.
        let Some(oldest) = messages.first().map(|m| m.msg_id) else {
            return Vec::new();
        };
        // At the retention cap, older messages would be trimmed right away.
        let cap = self.state.max_messages_per_buffer;
        if cap > 0 && messages.len() >= cap {
            return Vec::new();
        }
        self.older_pending.insert(buffer);
        vec![Effect::RequestOlderBacklog {
            buffer,
            before: oldest,
        }]
    }

    pub fn older_backlog_failed(&mut self, buffer: BufferId, error: &str) {
        tracing::warn!("older backlog request failed for buffer {buffer}: {error}");
        self.older_pending.remove(&buffer);
        self.notify(
            &format!("Could not load older history: {error}"),
            Severity::Warning,
        );
    }

    /// What the top of the active buffer's history looks like, for the view.
    pub fn history_status(&mut self) -> Option<&'static str> {
        let buffer = self.log.buffer?;
        if self.older_pending.contains(&buffer) {
            return Some("loading older messages…");
        }
        if self.log.top_line(&self.state) != 0 {
            return None;
        }
        let cap = self.state.max_messages_per_buffer;
        if self.history_start.contains(&buffer) {
            Some("start of history")
        } else if cap > 0 && self.state.messages_for_buffer(buffer).len() >= cap {
            Some("history limit reached")
        } else {
            None
        }
    }

    pub fn backlog_failed(&mut self, buffer: BufferId, error: &str) {
        tracing::warn!("backlog request failed for buffer {buffer}: {error}");
        self.state.release_backlog(buffer);
        self.initial_pending.remove(&buffer);
        self.notify(
            &format!("Could not load history: {error}"),
            Severity::Warning,
        );
    }

    /// Empty Enter: move the read marker to the newest message. It
    /// overwrites the old marker with no undo, so the move is announced.
    fn marker_to_latest(&mut self) -> Vec<Effect> {
        let Some(active) = self.active_buffer_id else {
            return Vec::new();
        };
        let settings = self.log.settings;
        let Some(last) = self
            .state
            .messages_for_buffer(active)
            .iter()
            .rev()
            .find(|m| settings.shows(m))
            .map(|m| m.msg_id)
        else {
            return Vec::new();
        };
        let effects = self.place_marker(active, last);
        let place = self
            .state
            .buffers
            .get(&active)
            .filter(|info| !info.name.is_empty())
            .map_or_else(|| format!("buffer {active}"), |info| info.name.clone());
        self.notify(
            &format!("Read marker moved to latest in {place}"),
            Severity::Info,
        );
        effects
    }

    fn place_marker(&mut self, buffer: BufferId, msg_id: MsgId) -> Vec<Effect> {
        self.state.read_markers.insert(buffer, msg_id);
        if self.live && !self.connection_lost {
            vec![Effect::SetMarkerLine(buffer, msg_id)]
        } else {
            Vec::new()
        }
    }

    // -- mouse ------------------------------------------------------------------

    /// A click on a sidebar buffer.
    pub fn click_buffer(&mut self, buffer: BufferId) -> Vec<Effect> {
        self.select_buffer(buffer)
    }

    /// A click on scrollback line `line` (from the top of the view). It
    /// moves the cursor but never the marker; that stays on Enter.
    pub fn click_log(&mut self, line: usize) {
        self.focus = Focus::Log;
        self.log.click(&self.state, line);
    }

    pub fn scroll_log(&mut self, lines: isize) -> Vec<Effect> {
        self.log.scroll_lines(&self.state, lines);
        if lines < 0 {
            self.fetch_older_if_at_top()
        } else {
            Vec::new()
        }
    }

    // -- keys -------------------------------------------------------------------

    pub fn on_paste(&mut self, text: &str) {
        if self.focus == Focus::Input && !self.input.disabled {
            let flat: String = text
                .chars()
                .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
                .collect();
            self.input.insert(&flat);
        }
    }

    pub fn on_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);

        if self.show_help {
            if matches!(
                key.code,
                KeyCode::Esc | KeyCode::F(1) | KeyCode::Char('?') | KeyCode::Enter
            ) || (ctrl && key.code == KeyCode::Char('q'))
            {
                self.show_help = false;
                if ctrl {
                    self.exit = Some(Exit {
                        code: 0,
                        message: None,
                    });
                }
            }
            return Vec::new();
        }

        // Global bindings work regardless of focus.
        match key.code {
            KeyCode::Char('q') if ctrl => {
                self.exit = Some(Exit {
                    code: 0,
                    message: None,
                });
                return Vec::new();
            }
            KeyCode::Char('c') if ctrl => {
                self.notify("Press Ctrl+Q to quit", Severity::Info);
                return Vec::new();
            }
            KeyCode::Char('r') if ctrl => return self.reconnect(),
            KeyCode::Up if alt => return self.cycle_buffer(-1),
            KeyCode::Down if alt => return self.cycle_buffer(1),
            KeyCode::Char('p') if ctrl => return self.cycle_buffer(-1),
            KeyCode::Char('n') if ctrl => return self.cycle_buffer(1),
            KeyCode::F(1) => {
                self.show_help = true;
                return Vec::new();
            }
            KeyCode::Tab | KeyCode::BackTab => {
                self.toggle_focus();
                return Vec::new();
            }
            KeyCode::PageUp => {
                self.log.page(&self.state, -1);
                return self.fetch_older_if_at_top();
            }
            KeyCode::PageDown => {
                self.log.page(&self.state, 1);
                return Vec::new();
            }
            _ => {}
        }

        match self.focus {
            Focus::Input => self.input_key(key, ctrl),
            Focus::Log => self.normal_key(key),
        }
    }

    /// Tab: switch between typing and normal mode.
    fn toggle_focus(&mut self) {
        let next = match self.focus {
            Focus::Input => Focus::Log,
            Focus::Log => Focus::Input,
        };
        self.set_focus(next);
    }

    pub fn set_focus(&mut self, focus: Focus) {
        if focus == Focus::Input && self.input.disabled {
            return;
        }
        self.focus = focus;
        self.focused();
    }

    fn focused(&mut self) {
        if self.focus == Focus::Log {
            self.log.on_focus(&self.state);
        }
    }

    fn input_key(&mut self, key: KeyEvent, ctrl: bool) -> Vec<Effect> {
        if self.input.disabled {
            return Vec::new();
        }
        let input = &mut self.input;
        match key.code {
            KeyCode::Enter => return self.submit(),
            KeyCode::Up => input.history_prev(),
            KeyCode::Down => input.history_next(),
            KeyCode::Left => input.cursor = input.cursor.saturating_sub(1),
            KeyCode::Right => input.cursor = (input.cursor + 1).min(input.value.chars().count()),
            KeyCode::Home => input.cursor = 0,
            KeyCode::End => input.cursor = input.value.chars().count(),
            KeyCode::Backspace => input.backspace(),
            KeyCode::Delete => input.delete(),
            // Leave typing for the scrollback, like vim's normal mode.
            KeyCode::Esc => self.set_focus(Focus::Log),
            KeyCode::Char('a') if ctrl => input.cursor = 0,
            KeyCode::Char('e') if ctrl => input.cursor = input.value.chars().count(),
            KeyCode::Char('u') if ctrl => {
                let cursor = input.cursor;
                input.delete_range(0, cursor);
            }
            KeyCode::Char('k') if ctrl => {
                let (cursor, len) = (input.cursor, input.value.chars().count());
                input.delete_range(cursor, len);
            }
            KeyCode::Char('w') if ctrl => input.delete_word_back(),
            KeyCode::Char('h') if ctrl => input.backspace(),
            KeyCode::Char(c) if !ctrl => input.insert(&c.to_string()),
            _ => {}
        }
        Vec::new()
    }

    /// Normal-mode keys, aerc-style: lowercase navigates within the
    /// current channel, uppercase through the channel list.
    fn normal_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        let half_page = (self.log.height as isize / 2).max(1);
        let upward = !shift
            && match key.code {
                KeyCode::Char('u' | 'y') => ctrl,
                KeyCode::Up | KeyCode::Char('k' | 'g') | KeyCode::Home => !ctrl,
                _ => false,
            };
        match key.code {
            KeyCode::Char('d') if ctrl => self.log.scroll_lines(&self.state, half_page),
            KeyCode::Char('u') if ctrl => self.log.scroll_lines(&self.state, -half_page),
            KeyCode::Char('e') if ctrl => self.log.scroll_lines(&self.state, 1),
            KeyCode::Char('y') if ctrl => self.log.scroll_lines(&self.state, -1),
            _ if ctrl => {}
            KeyCode::Char('K') => return self.cycle_buffer(-1),
            KeyCode::Char('J') => return self.cycle_buffer(1),
            KeyCode::Up if shift => return self.cycle_buffer(-1),
            KeyCode::Down if shift => return self.cycle_buffer(1),
            KeyCode::Up | KeyCode::Char('k') => self.log.move_highlight(&self.state, -1),
            KeyCode::Down | KeyCode::Char('j') => self.log.move_highlight(&self.state, 1),
            KeyCode::Home | KeyCode::Char('g') => self.log.highlight_first(&self.state),
            KeyCode::End | KeyCode::Char('G') => self.log.highlight_last(&self.state),
            KeyCode::Char('?') => self.show_help = true,
            KeyCode::Esc | KeyCode::Char('i' | 'a') => self.set_focus(Focus::Input),
            KeyCode::Enter => {
                if let (Some(buffer), Some(msg)) = (self.log.buffer, self.log.highlighted) {
                    return self.place_marker(buffer, msg);
                }
            }
            _ => {}
        }
        if upward {
            return self.fetch_older_if_at_top();
        }
        Vec::new()
    }
}

#[cfg(test)]
mod tests;
