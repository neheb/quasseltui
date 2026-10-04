//! The scrollback for the active buffer: rows, wrapping, scrolling, and the
//! highlighted row used to place the read marker.
//!
//! The scroll position is stored as an anchor, a message id plus a line
//! offset from that message's first line, instead of a line number. Live
//! messages appended below and backlog prepended above both leave the
//! reader's view exactly where it was, and so does a resize. Only when the
//! view is at the bottom does it follow new messages.
//!
//! The anchor never sits on the marker or a date separator, so moving the
//! marker can't drag the view.

use chrono::{Local, NaiveDate};
use unicode_width::UnicodeWidthChar;

use crate::app::format::{Segment, SegmentKind, format_message, local_time};
use crate::client::ClientState;
use crate::protocol::types::{BufferId, MsgId};

pub const MARKER_TEXT: &str = "── read up to here ──";
/// Continuation lines of a wrapped message are indented past the
/// timestamp.
const HANGING_INDENT: usize = 9;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowKey {
    /// "The day changed" separator before the first message of a day.
    Date(NaiveDate),
    Message(MsgId),
    Marker,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub key: RowKey,
    pub segments: Vec<Segment>,
}

impl Row {
    pub fn is_message(&self) -> bool {
        matches!(self.key, RowKey::Message(_))
    }

    pub fn msg_id(&self) -> Option<MsgId> {
        match self.key {
            RowKey::Message(id) => Some(id),
            _ => None,
        }
    }
}

/// Build the rows for a buffer.
///
/// A date separator goes before the first message of each local day, and
/// before the first message at all when it isn't from today: a quiet
/// buffer's backlog can span weeks, and time-only stamps would make it all
/// look like today's traffic. The marker row goes right after the marked
/// message.
pub fn build_rows(state: &ClientState, buffer: BufferId, today: NaiveDate) -> Vec<Row> {
    let marker = state.read_markers.get(&buffer).copied();
    let mut rows = Vec::new();
    let mut previous_date = today;
    for msg in state.messages_for_buffer(buffer) {
        let date = local_time(msg.timestamp).date_naive();
        if date != previous_date {
            rows.push(Row {
                key: RowKey::Date(date),
                segments: vec![Segment {
                    text: format!("── {} ──", date.format("%A, %Y-%m-%d")),
                    kind: SegmentKind::Event,
                }],
            });
            previous_date = date;
        }
        rows.push(Row {
            key: RowKey::Message(msg.msg_id),
            segments: format_message(msg),
        });
        if marker == Some(msg.msg_id) {
            rows.push(Row {
                key: RowKey::Marker,
                segments: vec![Segment {
                    text: MARKER_TEXT.into(),
                    kind: SegmentKind::Prefix,
                }],
            });
        }
    }
    rows
}

/// Word-wrap styled segments to `width` columns. Continuation lines get
/// `indent` columns of indentation. Words longer than a line are split.
pub fn wrap_segments(segments: &[Segment], width: usize, indent: usize) -> Vec<Vec<Segment>> {
    let width = width.max(1);
    let indent = if width > indent + 10 { indent } else { 0 };
    let mut lines: Vec<Vec<(char, SegmentKind)>> = Vec::new();
    let mut current: Vec<(char, SegmentKind)> = Vec::new();
    let mut current_width = 0;
    let mut line_start = 0; // chars of indentation on the current line
    for segment in segments {
        for c in segment.text.chars() {
            let w = c.width().unwrap_or(0);
            if current_width + w > width && current.len() > line_start {
                // Break at the last space past the indent; a word with no
                // space before it is split hard.
                let carry = if c == ' ' {
                    Vec::new()
                } else {
                    match current[line_start..].iter().rposition(|(ch, _)| *ch == ' ') {
                        Some(pos) => current.split_off(line_start + pos + 1),
                        None => Vec::new(),
                    }
                };
                while current.len() > line_start && current.last().is_some_and(|(ch, _)| *ch == ' ')
                {
                    current.pop();
                }
                lines.push(std::mem::take(&mut current));
                current.extend(std::iter::repeat_n((' ', SegmentKind::Body), indent));
                line_start = indent;
                current.extend(carry);
                current_width = current.iter().map(|(ch, _)| ch.width().unwrap_or(0)).sum();
                if c == ' ' {
                    continue;
                }
            }
            current.push((c, segment.kind));
            current_width += w;
        }
    }
    if !current.is_empty() || lines.is_empty() {
        lines.push(current);
    }
    lines
        .into_iter()
        .map(|chars| {
            let mut out: Vec<Segment> = Vec::new();
            for (c, kind) in chars {
                match out.last_mut() {
                    Some(seg) if seg.kind == kind => seg.text.push(c),
                    _ => out.push(Segment {
                        text: c.to_string(),
                        kind,
                    }),
                }
            }
            out
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CacheKey {
    buffer: BufferId,
    width: u16,
    count: usize,
    first: Option<MsgId>,
    last: Option<MsgId>,
    marker: Option<MsgId>,
    today: NaiveDate,
}

/// The wrapped layout of one buffer at one width.
#[derive(Debug, Clone, Default)]
pub struct Layout {
    pub rows: Vec<Row>,
    /// Wrapped lines, each tagged with its row index.
    pub lines: Vec<(usize, Vec<Segment>)>,
    /// First line index of each row.
    pub row_start: Vec<usize>,
}

impl Layout {
    fn build(rows: Vec<Row>, width: usize) -> Self {
        let mut lines = Vec::new();
        let mut row_start = Vec::with_capacity(rows.len());
        for (i, row) in rows.iter().enumerate() {
            row_start.push(lines.len());
            let indent = if row.is_message() { HANGING_INDENT } else { 0 };
            for line in wrap_segments(&row.segments, width, indent) {
                lines.push((i, line));
            }
        }
        Self {
            rows,
            lines,
            row_start,
        }
    }

    pub fn total_lines(&self) -> usize {
        self.lines.len()
    }

    fn row_end(&self, row: usize) -> usize {
        self.row_start
            .get(row + 1)
            .copied()
            .unwrap_or(self.lines.len())
    }

    fn row_of_message(&self, id: MsgId) -> Option<usize> {
        self.rows.iter().position(|r| r.msg_id() == Some(id))
    }

    /// The first message row at or after `row`, else the nearest before.
    fn message_row_near(&self, row: usize) -> Option<usize> {
        (row..self.rows.len())
            .find(|i| self.rows[*i].is_message())
            .or_else(|| {
                (0..row.min(self.rows.len()))
                    .rev()
                    .find(|i| self.rows[*i].is_message())
            })
    }
}

#[derive(Debug, Clone)]
pub struct LogView {
    pub buffer: Option<BufferId>,
    /// At the bottom, following new messages.
    pub follow_tail: bool,
    /// When not following: the top of the view is `offset` lines below
    /// the first line of message `id`.
    pub anchor: Option<(MsgId, isize)>,
    /// The cursor row (a message), used to place the read marker.
    pub highlighted: Option<MsgId>,
    pub width: u16,
    pub height: u16,
    cache: Option<(CacheKey, Layout)>,
}

impl Default for LogView {
    fn default() -> Self {
        Self {
            buffer: None,
            follow_tail: true,
            anchor: None,
            highlighted: None,
            width: 80,
            height: 20,
            cache: None,
        }
    }
}

impl LogView {
    /// Point the view at a buffer. A different buffer starts fresh at the
    /// bottom with no cursor; the same buffer keeps its position (that is a
    /// refresh).
    pub fn set_buffer(&mut self, buffer: Option<BufferId>) {
        if buffer == self.buffer {
            return;
        }
        self.buffer = buffer;
        self.follow_tail = true;
        self.anchor = None;
        self.highlighted = None;
    }

    pub fn set_viewport(&mut self, width: u16, height: u16) {
        self.width = width.max(1);
        self.height = height;
    }

    /// The wrapped layout of the current buffer (cached).
    pub fn layout(&mut self, state: &ClientState) -> &Layout {
        self.layout_at(state, Local::now().date_naive())
    }

    pub fn layout_at(&mut self, state: &ClientState, today: NaiveDate) -> &Layout {
        let Some(buffer) = self.buffer else {
            self.cache = None;
            return EMPTY_LAYOUT.get_or_init(Layout::default);
        };
        let msgs = state.messages_for_buffer(buffer);
        let key = CacheKey {
            buffer,
            width: self.width,
            count: msgs.len(),
            first: msgs.first().map(|m| m.msg_id),
            last: msgs.last().map(|m| m.msg_id),
            marker: state.read_markers.get(&buffer).copied(),
            today,
        };
        let stale = !matches!(&self.cache, Some((k, _)) if *k == key);
        if stale {
            let layout = Layout::build(build_rows(state, buffer, today), usize::from(self.width));
            self.cache = Some((key, layout));
        }
        &self.cache.as_ref().expect("cache filled").1
    }

    /// The first visible line.
    pub fn top_line(&mut self, state: &ClientState) -> usize {
        let height = usize::from(self.height);
        let follow = self.follow_tail;
        let anchor = self.anchor;
        let layout = self.layout(state);
        let max_top = layout.total_lines().saturating_sub(height);
        if follow {
            return max_top;
        }
        let Some((id, offset)) = anchor else {
            return max_top;
        };
        // An anchor that was trimmed away falls back to where it would
        // have been: the first newer message, else the bottom.
        let row = layout.row_of_message(id).or_else(|| {
            layout
                .rows
                .iter()
                .position(|r| r.msg_id().is_some_and(|m| m > id))
        });
        match row {
            Some(row) => {
                let start = layout.row_start[row] as isize;
                (start + offset).clamp(0, max_top as isize) as usize
            }
            None => max_top,
        }
    }

    /// Scroll to `top`, re-deriving the anchor (or following the tail at
    /// the bottom).
    fn scroll_to(&mut self, state: &ClientState, top: usize) {
        let height = usize::from(self.height);
        let layout = self.layout(state);
        let max_top = layout.total_lines().saturating_sub(height);
        let top = top.min(max_top);
        if top >= max_top {
            self.follow_tail = true;
            self.anchor = None;
            return;
        }
        let row = layout.lines.get(top).map_or(0, |(row, _)| *row);
        let anchor = layout.message_row_near(row).and_then(|r| {
            let id = layout.rows[r].msg_id()?;
            Some((id, top as isize - layout.row_start[r] as isize))
        });
        self.follow_tail = anchor.is_none();
        self.anchor = anchor;
    }

    pub fn scroll_lines(&mut self, state: &ClientState, delta: isize) {
        let top = self.top_line(state) as isize;
        self.scroll_to(state, (top + delta).max(0) as usize);
    }

    pub fn page(&mut self, state: &ClientState, pages: isize) {
        let step = (self.height as isize - 1).max(1);
        self.scroll_lines(state, pages * step);
    }

    pub fn scroll_home(&mut self, state: &ClientState) {
        self.scroll_to(state, 0);
    }

    pub fn scroll_end(&mut self) {
        self.follow_tail = true;
        self.anchor = None;
    }

    fn message_ids(&mut self, state: &ClientState) -> Vec<MsgId> {
        self.layout(state)
            .rows
            .iter()
            .filter_map(Row::msg_id)
            .collect()
    }

    /// The first message at the top of the view.
    fn top_visible_message(&mut self, state: &ClientState) -> Option<MsgId> {
        let top = self.top_line(state);
        let layout = self.layout(state);
        let row = layout.lines.get(top).map(|(row, _)| *row)?;
        layout
            .message_row_near(row)
            .and_then(|r| layout.rows[r].msg_id())
    }

    /// Focus arrived: give the cursor somewhere to be without moving the
    /// view. At the bottom that's the newest message; scrolled up, the top
    /// visible one. An existing cursor is kept.
    pub fn on_focus(&mut self, state: &ClientState) {
        if self.highlighted.is_some() {
            return;
        }
        self.highlighted = if self.follow_tail {
            self.message_ids(state).last().copied()
        } else {
            self.top_visible_message(state)
        };
    }

    /// Move the cursor by `delta` messages and scroll it into view.
    pub fn move_highlight(&mut self, state: &ClientState, delta: isize) {
        let ids = self.message_ids(state);
        if ids.is_empty() {
            self.highlighted = None;
            return;
        }
        let current = self
            .highlighted
            .and_then(|h| ids.iter().position(|id| *id == h));
        let target = match current {
            None => {
                self.on_focus(state);
                return;
            }
            Some(i) => (i as isize + delta).clamp(0, ids.len() as isize - 1) as usize,
        };
        self.highlighted = Some(ids[target]);
        self.ensure_visible(state, ids[target]);
    }

    pub fn highlight_first(&mut self, state: &ClientState) {
        if let Some(first) = self.message_ids(state).first().copied() {
            self.highlighted = Some(first);
            self.ensure_visible(state, first);
        }
    }

    pub fn highlight_last(&mut self, state: &ClientState) {
        if let Some(last) = self.message_ids(state).last().copied() {
            self.highlighted = Some(last);
            self.ensure_visible(state, last);
        }
    }

    fn ensure_visible(&mut self, state: &ClientState, id: MsgId) {
        let height = usize::from(self.height).max(1);
        let top = self.top_line(state);
        let layout = self.layout(state);
        let Some(row) = layout.row_of_message(id) else {
            return;
        };
        let (start, end) = (layout.row_start[row], layout.row_end(row));
        if start < top {
            self.scroll_to(state, start);
        } else if end > top + height {
            self.scroll_to(state, end.saturating_sub(height));
        }
    }

    /// A click on viewport line `line`: highlight that message (if it is
    /// one). Clicking never places the marker, so a stray click can't
    /// destroy the record of where the user stopped reading.
    pub fn click(&mut self, state: &ClientState, line: usize) {
        let top = self.top_line(state);
        let layout = self.layout(state);
        if let Some((row, _)) = layout.lines.get(top + line)
            && let Some(id) = layout.rows[*row].msg_id()
        {
            self.highlighted = Some(id);
        }
    }

    /// The visible lines, top to bottom, with their row keys.
    pub fn visible(&mut self, state: &ClientState) -> Vec<(RowKey, Vec<Segment>)> {
        let height = usize::from(self.height);
        let top = self.top_line(state);
        let layout = self.layout(state);
        layout
            .lines
            .iter()
            .skip(top)
            .take(height)
            .map(|(row, segments)| (layout.rows[*row].key, segments.clone()))
            .collect()
    }
}

static EMPTY_LAYOUT: std::sync::OnceLock<Layout> = std::sync::OnceLock::new();

#[cfg(test)]
mod tests {
    use chrono::{Duration, TimeZone, Utc};

    use super::*;
    use crate::client::IrcMessage;
    use crate::protocol::types::{MessageFlags, MessageType, NetworkId};

    fn seg(text: &str) -> Vec<Segment> {
        vec![Segment {
            text: text.into(),
            kind: SegmentKind::Body,
        }]
    }

    fn line_texts(lines: &[Vec<Segment>]) -> Vec<String> {
        lines
            .iter()
            .map(|l| l.iter().map(|s| s.text.as_str()).collect())
            .collect()
    }

    #[test]
    fn wrapping() {
        assert_eq!(
            line_texts(&wrap_segments(&seg("hello world"), 20, 0)),
            ["hello world"]
        );
        assert_eq!(
            line_texts(&wrap_segments(&seg("hello world"), 7, 0)),
            ["hello", "world"]
        );
        assert_eq!(
            line_texts(&wrap_segments(&seg("abcdefghij"), 4, 0)),
            ["abcd", "efgh", "ij"]
        );
        assert_eq!(line_texts(&wrap_segments(&seg(""), 4, 0)), [""]);
        assert_eq!(
            line_texts(&wrap_segments(&seg("aaaa bbbb cccc dddd eeee"), 15, 2)),
            ["aaaa bbbb cccc", "  dddd eeee"]
        );
        // Wide characters count as two columns.
        assert_eq!(
            line_texts(&wrap_segments(&seg("日本語"), 4, 0)),
            ["日本", "語"]
        );
    }

    #[test]
    fn wrapping_keeps_segment_kinds() {
        let segments = vec![
            Segment {
                text: "12:00:00 ".into(),
                kind: SegmentKind::Timestamp,
            },
            Segment {
                text: "nick".into(),
                kind: SegmentKind::Nick,
            },
        ];
        let lines = wrap_segments(&segments, 40, 0);
        assert_eq!(lines[0].len(), 2);
        assert_eq!(lines[0][1].kind, SegmentKind::Nick);
    }

    fn message(id: i64, buffer: i32, ts: chrono::DateTime<Utc>, contents: &str) -> IrcMessage {
        IrcMessage {
            msg_id: MsgId(id),
            buffer_id: BufferId(buffer),
            network_id: NetworkId(1),
            timestamp: ts,
            kind: MessageType::Plain,
            flags: MessageFlags::NONE,
            sender: "n".into(),
            sender_prefixes: String::new(),
            contents: contents.into(),
        }
    }

    fn state_with(ids: std::ops::RangeInclusive<i64>) -> ClientState {
        let mut state = ClientState::new(0);
        let now = Utc::now();
        state.messages.insert(
            BufferId(1),
            ids.map(|i| message(i, 1, now, &format!("message {i}")))
                .collect(),
        );
        state
    }

    fn view(height: u16) -> LogView {
        let mut v = LogView::default();
        v.set_buffer(Some(BufferId(1)));
        v.set_viewport(80, height);
        v
    }

    fn top_id(v: &mut LogView, state: &ClientState) -> Option<MsgId> {
        v.visible(state).first().and_then(|(k, _)| match k {
            RowKey::Message(id) => Some(*id),
            _ => None,
        })
    }

    #[test]
    fn rows_have_date_separators_and_marker() {
        let mut state = ClientState::new(0);
        let today = Local::now().date_naive();
        let day1 = Utc.with_ymd_and_hms(2026, 1, 5, 12, 0, 0).unwrap();
        state.messages.insert(
            BufferId(1),
            vec![
                message(1, 1, day1, "a"),
                message(2, 1, day1 + Duration::days(1), "b"),
                message(3, 1, Utc::now(), "c"),
            ],
        );
        state.read_markers.insert(BufferId(1), MsgId(2));
        let rows = build_rows(&state, BufferId(1), today);
        let keys: Vec<_> = rows.iter().map(|r| r.key).collect();
        assert!(matches!(keys[0], RowKey::Date(_)));
        assert_eq!(keys[1], RowKey::Message(MsgId(1)));
        assert!(matches!(keys[2], RowKey::Date(_)));
        assert_eq!(keys[3], RowKey::Message(MsgId(2)));
        assert_eq!(keys[4], RowKey::Marker);
        assert!(matches!(keys[5], RowKey::Date(d) if d == today));
        assert_eq!(keys[6], RowKey::Message(MsgId(3)));
        assert!(rows[0].segments[0].text.contains("2026-01-05"));
    }

    #[test]
    fn todays_messages_get_no_leading_separator() {
        let state = state_with(1..=3);
        let rows = build_rows(&state, BufferId(1), Local::now().date_naive());
        assert!(rows.iter().all(Row::is_message));
    }

    #[test]
    fn follows_tail_by_default() {
        let mut state = state_with(1..=50);
        let mut v = view(10);
        assert_eq!(top_id(&mut v, &state), Some(MsgId(41)));
        state
            .messages
            .get_mut(&BufferId(1))
            .unwrap()
            .push(message(51, 1, Utc::now(), "new"));
        assert_eq!(top_id(&mut v, &state), Some(MsgId(42)));
    }

    #[test]
    fn scrolled_up_reader_stays_anchored_through_appends_and_prepends() {
        let mut state = state_with(10..=50);
        let mut v = view(10);
        v.scroll_lines(&state, -20);
        assert!(!v.follow_tail);
        let anchored = top_id(&mut v, &state);
        assert_eq!(anchored, Some(MsgId(21)));

        // Live append below: the reader doesn't move.
        state
            .messages
            .get_mut(&BufferId(1))
            .unwrap()
            .push(message(51, 1, Utc::now(), "new"));
        assert_eq!(top_id(&mut v, &state), anchored);

        // Backlog prepend above: same content stays in view.
        let list = state.messages.get_mut(&BufferId(1)).unwrap();
        let older: Vec<_> = (1..10).map(|i| message(i, 1, Utc::now(), "old")).collect();
        list.splice(0..0, older);
        assert_eq!(top_id(&mut v, &state), anchored);
    }

    #[test]
    fn wrapped_rows_keep_the_anchor_offset() {
        let mut state = ClientState::new(0);
        let long = "word ".repeat(40);
        state.messages.insert(
            BufferId(1),
            (1..=20).map(|i| message(i, 1, Utc::now(), &long)).collect(),
        );
        let mut v = view(10);
        v.set_viewport(40, 10);
        v.scroll_lines(&state, -25);
        let before = v.visible(&state);
        let list = state.messages.get_mut(&BufferId(1)).unwrap();
        list.insert(0, message(0, 1, Utc::now(), &long));
        assert_eq!(v.visible(&state), before);
    }

    #[test]
    fn scrolling_to_the_bottom_resumes_following() {
        let state = state_with(1..=50);
        let mut v = view(10);
        v.scroll_lines(&state, -5);
        assert!(!v.follow_tail);
        v.scroll_lines(&state, 100);
        assert!(v.follow_tail);
        v.scroll_home(&state);
        assert_eq!(top_id(&mut v, &state), Some(MsgId(1)));
        v.page(&state, 1);
        assert_eq!(top_id(&mut v, &state), Some(MsgId(10)));
    }

    #[test]
    fn trimmed_anchor_falls_back_to_next_message() {
        let mut state = state_with(1..=50);
        let mut v = view(10);
        v.scroll_home(&state);
        state.messages.get_mut(&BufferId(1)).unwrap().drain(..5);
        assert_eq!(top_id(&mut v, &state), Some(MsgId(6)));
    }

    #[test]
    fn marker_row_is_never_the_anchor() {
        let mut state = state_with(1..=50);
        state.read_markers.insert(BufferId(1), MsgId(20));
        let mut v = view(10);
        v.scroll_home(&state);
        v.scroll_lines(&state, 20); // the top row is the marker (after msg 20)
        assert_eq!(v.visible(&state)[0].0, RowKey::Marker);
        assert!(matches!(v.anchor, Some((MsgId(21), -1))));
        assert_eq!(v.visible(&state)[1].0, RowKey::Message(MsgId(21)));
        // Moving the marker elsewhere doesn't drag the view: msg 21 keeps
        // its place on screen.
        state.read_markers.insert(BufferId(1), MsgId(45));
        assert_eq!(v.visible(&state)[1].0, RowKey::Message(MsgId(21)));
    }

    #[test]
    fn focus_and_cursor_movement() {
        let state = state_with(1..=50);
        let mut v = view(10);
        v.on_focus(&state);
        assert_eq!(v.highlighted, Some(MsgId(50)));
        v.move_highlight(&state, -1);
        assert_eq!(v.highlighted, Some(MsgId(49)));
        assert!(v.follow_tail, "moving within the view doesn't scroll");
        v.move_highlight(&state, -20);
        assert_eq!(v.highlighted, Some(MsgId(29)));
        assert_eq!(top_id(&mut v, &state), Some(MsgId(29)));

        // Focus while scrolled up lands on the top visible row, no jump.
        let mut v = view(10);
        v.scroll_lines(&state, -30);
        let top = top_id(&mut v, &state);
        v.on_focus(&state);
        assert_eq!(v.highlighted, top);
        assert_eq!(top_id(&mut v, &state), top);
    }

    #[test]
    fn switching_buffers_resets_but_refresh_keeps_position() {
        let state = state_with(1..=50);
        let mut v = view(10);
        v.scroll_lines(&state, -10);
        v.highlighted = Some(MsgId(3));
        v.set_buffer(Some(BufferId(1)));
        assert!(!v.follow_tail);
        assert_eq!(v.highlighted, Some(MsgId(3)));
        v.set_buffer(Some(BufferId(2)));
        assert!(v.follow_tail);
        assert_eq!(v.highlighted, None);
    }

    #[test]
    fn click_highlights_without_marking() {
        let state = state_with(1..=50);
        let mut v = view(10);
        v.click(&state, 0);
        assert_eq!(v.highlighted, Some(MsgId(41)));
        assert!(state.read_markers.is_empty());
    }
}
