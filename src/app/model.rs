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
//! - The active buffer is picked once, when the session opens (the one
//!   active at the last exit, else the first buffer, or the first that
//!   appears if there are none yet), and
//!   afterwards only changes on the user's request, or when the active
//!   buffer is removed.
//! - Chat lines (not joins, parts or other events) for other buffers mark
//!   them in the sidebar ("highlight" for highlights and queries, sticky
//!   until visited); the user's own echoed lines don't. On connect, and whenever another client reads a buffer,
//!   the core's own unread state (0.13+) sets or clears the marks.
//! - Read markers and read state round-trip through the core.

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::app::format::{
    CHAT_TYPES, DisplaySettings, buffer_label, format_message_plain, is_chat, ordered_buffer_ids,
    pick_default_buffer, short_sender,
};
use crate::app::log_view::LogView;
use crate::client::{ClientState, DEFAULT_BACKLOG_LIMIT, IrcMessage};
use crate::protocol::error::Error;
use crate::protocol::types::{BufferId, BufferInfo, BufferType, MessageFlags, MsgId, NetworkId};
use crate::sync::events::ClientEvent;
use crate::util::text::{sanitize_and_truncate, strip_mirc_formatting};

/// Longest disconnect reason or notice shown. A hostile core can send an
/// arbitrarily long error, and escaping control bytes can quadruple it.
pub const MAX_REASON_LEN: usize = 400;
const HISTORY_LIMIT: usize = 100;
pub const MAX_NOTICES: usize = 100;
/// Two clicks on the same sidebar row within this make a double-click.
const DOUBLE_CLICK: Duration = Duration::from_millis(400);
/// A history request with no reply after this is given up on.
const BACKLOG_TIMEOUT: Duration = Duration::from_secs(60);
pub const DEFAULT_PLACEHOLDER: &str = "Type a message and press Enter…";
/// Automatic reconnects back off from this, doubling per failed attempt...
const RETRY_FIRST: Duration = Duration::from_secs(2);
/// ...up to this.
const RETRY_MAX: Duration = Duration::from_secs(60);
/// At most one alert per buffer this often, so a busy query or a flood of
/// highlights doesn't bury the desktop in notifications.
const ALERT_INTERVAL: Duration = Duration::from_secs(30);

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
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Activity {
    Message,
    Highlight,
}

/// A clickable sidebar row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TreeRow {
    Network(NetworkId),
    Buffer(BufferId),
}

/// A request to the core, carried out by the runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    SendInput {
        buffer: BufferInfo,
        text: String,
    },
    RequestBacklog(BufferId),
    /// Fetch up to `limit` messages older than `before` (the oldest
    /// message we have).
    RequestOlderBacklog {
        buffer: BufferId,
        before: MsgId,
        limit: i32,
    },
    /// A `/command` the user triggered by other means than typing it, so
    /// a failure isn't put back in the input bar.
    SendCommand {
        buffer: BufferInfo,
        text: String,
    },
    SetNetworkConnected {
        network: NetworkId,
        connect: bool,
    },
    SetLastSeen(BufferId, MsgId),
    SetMarkerLine(BufferId, MsgId),
    /// Replace the client with a fresh connection over the same state.
    Reconnect,
    /// Put text on the system clipboard (via the terminal).
    Copy(String),
    /// Get the user's attention: ring the bell and raise a desktop
    /// notification. The text is untrusted and not yet sanitized.
    Alert {
        title: String,
        body: String,
    },
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

/// A multi-line paste waiting for the user to confirm sending it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingPaste {
    /// The lines to send, blank ones dropped.
    pub lines: Vec<String>,
    /// The input as it was before the paste, restored on cancel.
    previous: String,
    previous_cursor: usize,
}

/// An in-progress Tab completion: repeated Tabs cycle through `candidates`,
/// replacing what the previous Tab inserted.
#[derive(Debug, Clone)]
struct Completion {
    /// Char index where the completed word starts.
    start: usize,
    /// Chars the current candidate (and its suffix) occupies.
    inserted: usize,
    candidates: Vec<String>,
    index: usize,
}

/// How far back in a buffer to look for recent speakers.
const COMPLETION_SCAN: usize = 1000;

pub struct App {
    pub state: ClientState,
    pub active_buffer_id: Option<BufferId>,
    /// A mid-session disconnect happened; cleared by a reconnect.
    pub connection_lost: bool,
    /// Recent user-facing notices, sanitized, newest last. Toasts
    /// disappear; these are kept up to [`MAX_NOTICES`].
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
    /// Each maps to when it was sent; a reply that never comes (a
    /// malformed one is dropped) expires after [`BACKLOG_TIMEOUT`].
    initial_pending: HashMap<BufferId, Instant>,
    pub(crate) older_pending: HashMap<BufferId, Instant>,
    /// Buffers whose history the core has no more of.
    history_start: HashSet<BufferId>,
    /// The previous sidebar click, for double-click detection.
    last_click: Option<(TreeRow, Instant)>,
    /// The first `y` of a `yy` was pressed in normal mode.
    pending_yank: bool,
    /// While disconnected: why, and when to try again on our own (`None`
    /// when retrying can't help, like a rejected password).
    disconnect_reason: String,
    retry_at: Option<Instant>,
    /// Reconnect attempts since the last session opened.
    retries: u32,
    /// Whether the terminal window has focus, as far as it reports.
    terminal_focused: bool,
    /// When each buffer last raised an alert.
    pub(crate) last_alert: HashMap<BufferId, Instant>,
    /// Where to land when the session opens, if it has this buffer.
    start_buffer: Option<BufferId>,
    /// A multi-line paste awaiting Enter (send) or Esc (cancel).
    pub pending_paste: Option<PendingPaste>,
    completion: Option<Completion>,
    // Bridge state for the current connection attempt.
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
            // Like vim: start in normal mode; `i` to type.
            focus: Focus::Log,
            show_help: false,
            exit: None,
            live,
            reconnectable,
            pending_input: None,
            initial_pending: HashMap::new(),
            older_pending: HashMap::new(),
            history_start: HashSet::new(),
            last_click: None,
            pending_yank: false,
            disconnect_reason: String::new(),
            retry_at: None,
            retries: 0,
            terminal_focused: true,
            last_alert: HashMap::new(),
            start_buffer: None,
            pending_paste: None,
            completion: None,
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
    /// Land in `buffer` when the session opens, instead of the default.
    pub fn set_start_buffer(&mut self, buffer: Option<BufferId>) {
        self.start_buffer = buffer;
    }

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
        tracing::info!("notice: {safe}");
        if self.notices.len() >= MAX_NOTICES {
            self.notices.remove(0);
        }
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
        let expired = self.expire_backlog_requests(now);
        let countdown = self.update_disconnected_placeholder(now);
        let before = self.toasts.len();
        self.toasts
            .retain(|t| now.saturating_duration_since(t.shown_at) < t.lifetime());
        expired || countdown || before != self.toasts.len()
    }

    /// Give up on history requests the core never answered, so the view
    /// doesn't say "loading" forever and the request can be retried.
    fn expire_backlog_requests(&mut self, now: Instant) -> bool {
        let stale = |sent: &Instant| now.saturating_duration_since(*sent) >= BACKLOG_TIMEOUT;
        let initial: Vec<BufferId> = self
            .initial_pending
            .iter()
            .filter(|(_, sent)| stale(sent))
            .map(|(id, _)| *id)
            .collect();
        let older: Vec<BufferId> = self
            .older_pending
            .iter()
            .filter(|(_, sent)| stale(sent))
            .map(|(id, _)| *id)
            .collect();
        for id in &initial {
            self.initial_pending.remove(id);
            self.state.release_backlog(*id);
        }
        for id in &older {
            self.older_pending.remove(id);
        }
        if initial.is_empty() && older.is_empty() {
            return false;
        }
        tracing::warn!("history requests timed out: {initial:?} {older:?}");
        self.notify(
            "The core didn't answer a history request",
            Severity::Warning,
        );
        true
    }

    // -- client events (the bridge) -------------------------------------------

    pub fn on_client_event(&mut self, event: &ClientEvent) -> Vec<Effect> {
        match event {
            ClientEvent::SessionOpened { .. } => {
                self.session_opened = true;
                self.reconnected();
                if let Some(active) = self.active_buffer_id {
                    // Reconnect: keep the user's buffer and refresh it, which
                    // re-requests the backlog to fill the gap since the drop.
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
                self.last_alert.remove(buffer_id);
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
            ClientEvent::Disconnected { reason, error } => {
                let fatal = !self.session_opened && self.presession_fatal;
                // Retrying can't fix these; they need the user.
                let retryable = !matches!(
                    error.as_deref(),
                    Some(
                        Error::Auth(_)
                            | Error::NotConfigured(_)
                            | Error::TlsDowngrade(_)
                            | Error::InitRejected(_)
                    )
                );
                self.session_ended(reason, fatal, retryable);
                Vec::new()
            }
            ClientEvent::BufferActivity {
                buffer_id,
                types,
                highlights,
            } => {
                self.core_activity(*buffer_id, *types, *highlights);
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
            return Vec::new();
        }
        // Where the user left off last time, if this session has it.
        let remembered = self
            .start_buffer
            .filter(|id| self.state.buffers.contains_key(id));
        let Some(id) = remembered.or_else(|| pick_default_buffer(&self.state)) else {
            return Vec::new();
        };
        self.start_buffer = None;
        self.active_buffer_id = Some(id);
        self.active_updated(Some(id))
    }

    fn handle_message(&mut self, message: &IrcMessage) -> Vec<Effect> {
        let buffer = message.buffer_id;
        if message.flags.contains(MessageFlags::SELF) || !is_chat(message.kind) {
            return Vec::new();
        }
        let info = self.state.buffers.get(&buffer);
        let query = info.is_some_and(|info| info.kind == BufferType::Query);
        let highlight = query || message.flags.contains(MessageFlags::HIGHLIGHT);
        let in_view = Some(buffer) == self.active_buffer_id;
        if !in_view {
            self.mark_activity(buffer, highlight);
        }
        if !highlight || (in_view && self.terminal_focused) || !self.log.settings.notify {
            return Vec::new();
        }
        let now = Instant::now();
        if self
            .last_alert
            .get(&buffer)
            .is_some_and(|at| now.saturating_duration_since(*at) < ALERT_INTERVAL)
        {
            return Vec::new();
        }
        self.last_alert.insert(buffer, now);
        let nick = short_sender(&message.sender);
        let title = match self.state.buffers.get(&buffer) {
            Some(info) if !query => format!("{nick} in {}", buffer_label(info)),
            _ => nick.to_string(),
        };
        let body = strip_mirc_formatting(&message.contents);
        vec![Effect::Alert { title, body }]
    }

    /// The terminal window gained or lost focus.
    pub fn set_terminal_focus(&mut self, focused: bool) {
        self.terminal_focused = focused;
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

    /// Adopt the core's unread state for a buffer. No chat lines or
    /// highlights since its last-seen message clears the mark; otherwise the mark is raised
    /// to match but never lowered, so a local highlight stays sticky.
    fn core_activity(&mut self, buffer: BufferId, types: u32, highlights: i64) {
        let is_query = self
            .state
            .buffers
            .get(&buffer)
            .is_some_and(|info| info.kind == BufferType::Query);
        let level = if highlights > 0 {
            Some(Activity::Highlight)
        } else if types & CHAT_TYPES != 0 {
            Some(if is_query {
                Activity::Highlight
            } else {
                Activity::Message
            })
        } else {
            None
        };
        match level {
            None => {
                self.buffer_activity.remove(&buffer);
            }
            Some(level) if Some(buffer) != self.active_buffer_id => {
                let entry = self.buffer_activity.entry(buffer).or_insert(level);
                *entry = (*entry).max(level);
            }
            Some(_) => {}
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
                self.initial_pending.insert(id, Instant::now());
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
        self.completion = None;
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

    fn session_ended(&mut self, reason: &str, fatal: bool, retryable: bool) {
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
        self.enter_disconnected(&safe, retryable);
    }

    /// Latch the disconnected state and make it visible, and schedule an
    /// automatic reconnect when one could help. Typed text is stashed so
    /// the placeholder shows, and restored once a session opens again.
    fn enter_disconnected(&mut self, reason: &str, retryable: bool) {
        if self.connection_lost {
            return;
        }
        self.connection_lost = true;
        // Only the first drop gets a toast; failed retries just update the
        // input bar, or a long outage would toast every minute.
        if self.retries == 0 {
            self.notify(&format!("Disconnected: {reason}"), Severity::Error);
        }
        self.input.disabled = true;
        // An unconfirmed paste can't be sent now; keep what was typed.
        if let Some(paste) = self.pending_paste.take() {
            self.input.set_value(paste.previous);
        }
        if !self.input.value.is_empty() {
            self.pending_input = Some(std::mem::take(&mut self.input.value));
            self.input.cursor = 0;
        }
        if self.focus == Focus::Input {
            self.focus = Focus::Log;
        }
        self.disconnect_reason = reason.to_string();
        let now = Instant::now();
        self.retry_at = (retryable && self.live && self.reconnectable).then(|| {
            let delay = RETRY_FIRST.saturating_mul(1 << self.retries.min(16));
            now + delay.min(RETRY_MAX)
        });
        self.update_disconnected_placeholder(now);
    }

    /// The input bar's account of the outage, with the retry countdown.
    /// Returns whether it changed.
    fn update_disconnected_placeholder(&mut self, now: Instant) -> bool {
        if !self.connection_lost {
            return false;
        }
        let reason = &self.disconnect_reason;
        let text = match self.retry_at {
            Some(at) => {
                let secs = at.saturating_duration_since(now).as_secs_f64().ceil();
                format!("Disconnected: {reason} — retrying in {secs}s (Ctrl+R now, Ctrl+Q to quit)")
            }
            None => format!("Disconnected: {reason} — Ctrl+R to reconnect, Ctrl+Q to quit"),
        };
        if text == self.input.placeholder {
            return false;
        }
        self.input.placeholder = text;
        true
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
        self.notify("Reconnecting…", Severity::Info);
        self.start_reconnect()
    }

    /// Reconnect on our own once the backoff has run out.
    pub fn retry_due(&mut self, now: Instant) -> Vec<Effect> {
        match self.retry_at {
            Some(at) if self.connection_lost && now >= at => self.start_reconnect(),
            _ => Vec::new(),
        }
    }

    /// Replace the connection. The input stays disabled until the new
    /// session opens (see [`Self::reconnected`]), so a failing attempt
    /// doesn't flip it, or the focus, back and forth.
    fn start_reconnect(&mut self) -> Vec<Effect> {
        self.connection_lost = false;
        self.retry_at = None;
        self.retries = self.retries.saturating_add(1);
        // Requests on the old connection will never be answered.
        self.initial_pending.clear();
        self.older_pending.clear();
        self.input.placeholder = "Reconnecting…".into();
        // A still-down core must re-enter the disconnected state rather
        // than exit and destroy the scrollback.
        self.presession_fatal = false;
        self.session_opened = false;
        vec![Effect::Reconnect]
    }

    /// A session opened after a reconnect: typing works again, and a
    /// stashed draft comes back (with the focus, to finish it).
    fn reconnected(&mut self) {
        self.retries = 0;
        if !self.input.disabled {
            return;
        }
        self.input.disabled = false;
        self.input.placeholder = DEFAULT_PLACEHOLDER.into();
        if let Some(text) = self.pending_input.take()
            && self.input.value.is_empty()
        {
            self.input.set_value(text);
            self.focus = Focus::Input;
        }
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
        // Submitted lines are in the input history, so one that can't go
        // back in the input is still a press of Up away.
        let in_history = self.input.history.contains(&text);
        let hint = if self.restore_input(text) || !in_history {
            ""
        } else {
            " (Up recalls it)"
        };
        self.notify(
            &format!("Message not sent: {error}{hint}"),
            Severity::Warning,
        );
    }

    /// Put text back in the input unless the user already typed something
    /// new. While disconnected it's stashed for the reconnect instead.
    /// Returns whether it was put back (or stashed).
    fn restore_input(&mut self, text: String) -> bool {
        if self.connection_lost {
            if self.pending_input.is_some() {
                return false;
            }
            self.pending_input = Some(text);
            return true;
        }
        if !self.input.value.is_empty() {
            return false;
        }
        self.input.set_value(text);
        true
    }

    /// Route a backlog reply to the request it answers (see
    /// `initial_pending`). An older-history reply with nothing new means the
    /// core has no more.
    fn older_backlog_arrived(&mut self, buffer: BufferId, count: usize) {
        if self.initial_pending.remove(&buffer).is_some() {
            return;
        }
        // Nothing new with room to spare: the core has nothing older. (At
        // the cap, a page trimmed away by racing live traffic proves
        // nothing.)
        if self.older_pending.remove(&buffer).is_some() && count == 0 && !self.at_cap(buffer) {
            self.history_start.insert(buffer);
        }
    }

    fn at_cap(&self, buffer: BufferId) -> bool {
        let cap = self.state.max_messages_per_buffer;
        cap > 0 && self.state.messages_for_buffer(buffer).len() >= cap
    }

    /// After a move upward: at the top of what we have, fetch the next page
    /// of older history from the core.
    fn fetch_older_if_at_top(&mut self) -> Vec<Effect> {
        let Some(buffer) = self.log.buffer else {
            return Vec::new();
        };
        if !self.live
            || self.connection_lost
            || self.older_pending.contains_key(&buffer)
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
        // Ask only for what fits under the retention cap: a merge past it
        // trims the oldest messages, which is exactly this page.
        let cap = self.state.max_messages_per_buffer;
        let room = if cap == 0 {
            usize::MAX
        } else {
            cap.saturating_sub(messages.len())
        };
        if room == 0 {
            return Vec::new();
        }
        let limit =
            i32::try_from(room).map_or(DEFAULT_BACKLOG_LIMIT, |r| r.min(DEFAULT_BACKLOG_LIMIT));
        self.older_pending.insert(buffer, Instant::now());
        vec![Effect::RequestOlderBacklog {
            buffer,
            before: oldest,
            limit,
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
        if self.older_pending.contains_key(&buffer) {
            return Some("loading older messages…");
        }
        if self.log.top_line(&self.state) != 0 {
            return None;
        }
        if self.history_start.contains(&buffer) {
            Some("start of history")
        } else if self.at_cap(buffer) {
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

    /// `yy`: copy the highlighted message as it's shown.
    fn yank(&mut self) -> Vec<Effect> {
        let msg = self
            .log
            .buffer
            .zip(self.log.highlighted)
            .and_then(|(buffer, id)| {
                self.state
                    .messages
                    .get(&buffer)?
                    .iter()
                    .rev()
                    .find(|m| m.msg_id == id)
            });
        let Some(text) = msg.map(format_message_plain) else {
            return Vec::new();
        };
        self.notify("Copied message", Severity::Info);
        vec![Effect::Copy(text)]
    }

    // -- nick completion --------------------------------------------------------

    /// Whether Tab completes a nick (typing) rather than switching modes.
    fn completes_on_tab(&self) -> bool {
        self.focus == Focus::Input && !self.input.disabled && !self.show_help
    }

    /// Tab (Shift+Tab: `backward`) while typing: complete the word before
    /// the cursor to a nick, or cycle to the next match. Recent speakers
    /// come first, then the channel's other members; a nick at the start of
    /// the line gets `": "`, elsewhere a space.
    fn complete_nick(&mut self, backward: bool) {
        if let Some(mut c) = self.completion.take() {
            let n = c.candidates.len();
            c.index = if backward {
                (c.index + n - 1) % n
            } else {
                (c.index + 1) % n
            };
            self.input.delete_range(c.start, c.start + c.inserted);
            c.inserted = self.insert_completion(&c.candidates[c.index], c.start);
            self.completion = Some(c);
            return;
        }
        let chars: Vec<char> = self.input.value.chars().collect();
        let end = self.input.cursor;
        let start = chars[..end]
            .iter()
            .rposition(|c| c.is_whitespace())
            .map_or(0, |i| i + 1);
        let prefix: String = chars[start..end].iter().collect::<String>().to_lowercase();
        if prefix.is_empty() {
            return;
        }
        let candidates: Vec<String> = self
            .nick_candidates()
            .into_iter()
            .filter(|nick| nick.to_lowercase().starts_with(&prefix))
            .collect();
        if candidates.is_empty() {
            return;
        }
        let index = if backward { candidates.len() - 1 } else { 0 };
        self.input.delete_range(start, end);
        let inserted = self.insert_completion(&candidates[index], start);
        self.completion = Some(Completion {
            start,
            inserted,
            candidates,
            index,
        });
    }

    fn insert_completion(&mut self, nick: &str, start: usize) -> usize {
        let text = if start == 0 {
            format!("{nick}: ")
        } else {
            format!("{nick} ")
        };
        self.input.insert(&text);
        text.chars().count()
    }

    /// Nicks to complete in the active buffer: recent speakers (newest
    /// first), then the remaining members alphabetically. Speakers who
    /// left are dropped when the member list is known. Never our own nick.
    fn nick_candidates(&self) -> Vec<String> {
        let Some(buffer) = self.active_buffer_id else {
            return Vec::new();
        };
        let mut members: Vec<&str> = Vec::new();
        if let Some(info) = self.state.buffers.get(&buffer) {
            if info.kind == BufferType::Query {
                members.push(&info.name);
            } else if let Some(channel) = self.state.channel_for_buffer(info) {
                members.extend(channel.members().map(String::as_str));
            }
        }
        let me = self
            .state
            .network_for_buffer(buffer)
            .map(|n| n.my_nick.to_lowercase())
            .unwrap_or_default();
        let mut seen = HashSet::from([me]);
        let mut out = Vec::new();
        let member_keys: HashSet<String> = members.iter().map(|m| m.to_lowercase()).collect();
        let speakers = self
            .state
            .messages_for_buffer(buffer)
            .iter()
            .rev()
            .take(COMPLETION_SCAN)
            .filter(|m| is_chat(m.kind))
            .map(|m| short_sender(&m.sender));
        for nick in speakers {
            let key = nick.to_lowercase();
            if nick.is_empty() || (!member_keys.is_empty() && !member_keys.contains(&key)) {
                continue;
            }
            if seen.insert(key) {
                out.push(nick.to_string());
            }
        }
        members.sort_by_key(|m| m.to_lowercase());
        for nick in members {
            if seen.insert(nick.to_lowercase()) {
                out.push(nick.to_string());
            }
        }
        out
    }

    // -- mouse ------------------------------------------------------------------

    /// A click on a sidebar row. A click on a buffer selects it; a
    /// double-click on a network (or its status buffer) connects or
    /// disconnects it, and on a channel joins or parts it.
    pub fn click_tree(&mut self, row: TreeRow, now: Instant) -> Vec<Effect> {
        self.cancel_pending();
        let double = self.last_click.is_some_and(|(prev, at)| {
            prev == row && now.saturating_duration_since(at) <= DOUBLE_CLICK
        });
        // A third click starts a new pair rather than toggling back.
        self.last_click = if double { None } else { Some((row, now)) };
        let mut effects = match row {
            TreeRow::Buffer(buffer) => self.select_buffer(buffer),
            TreeRow::Network(_) => Vec::new(),
        };
        if double {
            effects.extend(self.toggle_row(row));
        }
        effects
    }

    fn toggle_row(&mut self, row: TreeRow) -> Vec<Effect> {
        if !self.live || self.connection_lost {
            return Vec::new();
        }
        let buffer = match row {
            TreeRow::Network(network) => return self.toggle_network(network),
            TreeRow::Buffer(id) => match self.state.buffers.get(&id) {
                Some(info) => info.clone(),
                None => return Vec::new(),
            },
        };
        match buffer.kind {
            BufferType::Status => self.toggle_network(buffer.network_id),
            BufferType::Channel => {
                let Some(network) = self.state.networks.get(&buffer.network_id) else {
                    return Vec::new();
                };
                if !network.is_connected {
                    self.notify(
                        &format!("Can't join {}: network not connected", buffer.name),
                        Severity::Warning,
                    );
                    return Vec::new();
                }
                let verb = if self.is_joined(&buffer) {
                    "part"
                } else {
                    "join"
                };
                let text = format!("/{verb} {}", buffer.name);
                vec![Effect::SendCommand { buffer, text }]
            }
            _ => Vec::new(),
        }
    }

    fn toggle_network(&mut self, network: NetworkId) -> Vec<Effect> {
        let Some(net) = self.state.networks.get(&network) else {
            return Vec::new();
        };
        let connect = !net.is_connected;
        let name = if net.network_name.is_empty() {
            format!("network {network}")
        } else {
            net.network_name.clone()
        };
        let verb = if connect {
            "Connecting"
        } else {
            "Disconnecting"
        };
        self.notify(&format!("{verb} {name}"), Severity::Info);
        vec![Effect::SetNetworkConnected { network, connect }]
    }

    /// Whether we're in a channel, per the core's channel list.
    pub fn is_joined(&self, buffer: &BufferInfo) -> bool {
        self.state
            .networks
            .get(&buffer.network_id)
            .is_some_and(|net| {
                net.channels
                    .iter()
                    .any(|c| c.eq_ignore_ascii_case(&buffer.name))
            })
    }

    /// A `/command` sent from a click failed.
    pub fn command_failed(&mut self, text: &str, error: &str) {
        tracing::warn!("{text} failed: {error}");
        self.notify(&format!("{text} failed: {error}"), Severity::Warning);
    }

    /// A click on scrollback line `line` (from the top of the view). It
    /// moves the cursor but never the marker; that stays on Enter.
    pub fn click_log(&mut self, line: usize) {
        self.cancel_pending();
        self.focus = Focus::Log;
        self.log.click(&self.state, line);
    }

    pub fn scroll_log(&mut self, lines: isize) -> Vec<Effect> {
        self.cancel_pending();
        self.log.scroll_lines(&self.state, lines);
        if lines < 0 {
            self.fetch_older_if_at_top()
        } else {
            Vec::new()
        }
    }

    // -- keys -------------------------------------------------------------------

    /// Mouse input or a paste ends a half-typed `yy` and a Tab completion.
    fn cancel_pending(&mut self) {
        self.pending_yank = false;
        self.completion = None;
    }

    /// A paste. One line goes into the input; several, merged with what
    /// was typed around the cursor, wait for confirmation to be sent as
    /// separate lines, like Quassel's own client does.
    pub fn on_paste(&mut self, text: &str) {
        self.cancel_pending();
        if self.focus != Focus::Input || self.input.disabled || self.pending_paste.is_some() {
            return;
        }
        let previous = self.input.value.clone();
        let previous_cursor = self.input.cursor;
        self.input.insert(text);
        let lines: Vec<String> = self
            .input
            .value
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(str::to_string)
            .collect();
        if lines.len() <= 1 {
            // A trailing newline or blank lines: keep it on one line.
            let flat: String = self
                .input
                .value
                .chars()
                .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
                .collect();
            let cursor = self.input.cursor;
            self.input.set_value(flat);
            self.input.cursor = cursor;
            return;
        }
        self.input.set_value("");
        self.pending_paste = Some(PendingPaste {
            lines,
            previous,
            previous_cursor,
        });
    }

    /// Enter or Esc on a pending paste.
    fn paste_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        match key.code {
            KeyCode::Enter => {
                let Some(paste) = self.pending_paste.take() else {
                    return Vec::new();
                };
                let mut effects = Vec::new();
                for line in paste.lines {
                    // Pasted text is text: a line that happens to start
                    // with `/` must not run as a command.
                    let line = if line.starts_with('/') {
                        format!("/say {line}")
                    } else {
                        line
                    };
                    effects.extend(self.line_submitted(line));
                }
                effects
            }
            KeyCode::Esc => {
                if let Some(paste) = self.pending_paste.take() {
                    self.input.set_value(paste.previous);
                    self.input.cursor = paste.previous_cursor;
                }
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    pub fn on_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        // A `yy` only counts if nothing else happened in between.
        let yank_started = std::mem::take(&mut self.pending_yank);
        if matches!(key.code, KeyCode::Tab | KeyCode::BackTab) && self.completes_on_tab() {
            self.complete_nick(key.code == KeyCode::BackTab);
            return Vec::new();
        }
        // Any other key ends a Tab completion.
        self.completion = None;
        if self.pending_paste.is_some() && !(ctrl && key.code == KeyCode::Char('q')) {
            return self.paste_key(key);
        }

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
                self.set_focus(Focus::Input);
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
            Focus::Log => self.normal_key(key, yank_started),
        }
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

    /// Normal-mode keys: j/k walk the channel list, the arrows move
    /// within the current channel.
    fn normal_key(&mut self, key: KeyEvent, yank_started: bool) -> Vec<Effect> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        let half_page = (self.log.height as isize / 2).max(1);
        let upward = !shift
            && match key.code {
                KeyCode::Char('u' | 'y') => ctrl,
                KeyCode::Up | KeyCode::Char('g') | KeyCode::Home => !ctrl,
                _ => false,
            };
        match key.code {
            KeyCode::Char('d') if ctrl => self.log.scroll_lines(&self.state, half_page),
            KeyCode::Char('u') if ctrl => self.log.scroll_lines(&self.state, -half_page),
            KeyCode::Char('e') if ctrl => self.log.scroll_lines(&self.state, 1),
            KeyCode::Char('y') if ctrl => self.log.scroll_lines(&self.state, -1),
            _ if ctrl => {}
            KeyCode::Char('k') => return self.cycle_buffer(-1),
            KeyCode::Char('j') => return self.cycle_buffer(1),
            KeyCode::Up if shift => return self.cycle_buffer(-1),
            KeyCode::Down if shift => return self.cycle_buffer(1),
            KeyCode::Up => self.log.move_highlight(&self.state, -1),
            KeyCode::Down => self.log.move_highlight(&self.state, 1),
            KeyCode::Home | KeyCode::Char('g') => self.log.highlight_first(&self.state),
            KeyCode::End | KeyCode::Char('G') => self.log.highlight_last(&self.state),
            KeyCode::Char('y') if yank_started => return self.yank(),
            KeyCode::Char('y') => self.pending_yank = true,
            KeyCode::Char('?') => self.show_help = true,
            KeyCode::Char('q') => {
                self.exit = Some(Exit {
                    code: 0,
                    message: None,
                });
            }
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
