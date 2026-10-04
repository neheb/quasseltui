//! Drawing. Three panes: buffer sidebar, scrollback, and input, plus toast
//! and help overlays.
//!
//! ```text
//! ╭ Buffers ─╮╭ #python — Libera.Chat ───────────╮
//! │ Libera   ││ 14:30:00 @guido: anyone awake?    │
//! │  (status)││ ...                               │
//! │  #python │╰───────────────────────────────────╯
//! │          │╭ seanr ────────────────────────────╮
//! ╰──────────╯╰ Type a message and press Enter…  ─╯
//! ```

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Margin, Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap};
use unicode_width::UnicodeWidthStr;

use crate::app::format::{SegmentKind, buffer_label, ordered_buffers};
use crate::app::log_view::RowKey;
use crate::app::model::{Activity, App, Focus, Severity};
use crate::app::theme::Theme;
use crate::protocol::types::BufferId;
use crate::util::text::sanitize_terminal;

const SIDEBAR_WIDTH: u16 = 28;

/// Where things landed, for mouse hit-testing.
#[derive(Debug, Clone, Default)]
pub struct Areas {
    pub tree: Rect,
    /// Buffer under each sidebar line (from the top of the tree's inner
    /// area); `None` for network headers.
    pub tree_rows: Vec<Option<BufferId>>,
    pub log: Rect,
    pub input: Rect,
}

fn block<'a>(title: impl Into<Line<'a>>, focused: bool, theme: &Theme) -> Block<'a> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(theme.border_style(focused))
        .title(title)
}

pub fn render(frame: &mut Frame<'_>, app: &mut App, theme: &Theme) -> Areas {
    let area = frame.area();
    let sidebar = SIDEBAR_WIDTH.min(area.width / 3).max(12.min(area.width));
    let [tree_area, main] =
        Layout::horizontal([Constraint::Length(sidebar), Constraint::Min(1)]).areas(area);
    let [log_area, input_area] =
        Layout::vertical([Constraint::Min(3), Constraint::Length(3)]).areas(main);

    let tree_rows = render_tree(frame, app, theme, tree_area);
    render_log(frame, app, theme, log_area);
    render_input(frame, app, theme, input_area);
    render_toasts(frame, app, theme, log_area);
    if app.show_help {
        render_help(frame, theme, area);
    }
    Areas {
        tree: tree_area,
        tree_rows,
        log: log_area.inner(Margin::new(1, 1)),
        input: input_area,
    }
}

fn render_tree(
    frame: &mut Frame<'_>,
    app: &App,
    theme: &Theme,
    area: Rect,
) -> Vec<Option<BufferId>> {
    let focused = app.focus == Focus::Tree;
    let inner_width = usize::from(area.width.saturating_sub(2));
    let mut lines: Vec<(Line<'static>, Option<BufferId>)> = Vec::new();
    let buffers = ordered_buffers(&app.state);
    for (network_id, network) in &app.state.networks {
        let name = if network.network_name.is_empty() {
            format!("(network {network_id})")
        } else {
            sanitize_terminal(&network.network_name)
        };
        let style = if network.is_connected || !app.is_live() {
            Style::default().add_modifier(Modifier::BOLD)
        } else {
            theme.muted_style().add_modifier(Modifier::BOLD)
        };
        lines.push((
            Line::from(Span::styled(truncate(&name, inner_width), style)),
            None,
        ));
        for buf in buffers.iter().filter(|b| b.network_id == *network_id) {
            let label = format!("  {}", buffer_label(buf));
            let mut style = Style::default();
            match app.buffer_activity.get(&buf.buffer_id) {
                Some(Activity::Highlight) => {
                    style = theme.fg(theme.highlight).add_modifier(Modifier::BOLD)
                }
                Some(Activity::Message) => style = style.add_modifier(Modifier::BOLD),
                None => {}
            }
            if Some(buf.buffer_id) == app.active_buffer_id {
                style = theme.accent_style();
            }
            if focused && Some(buf.buffer_id) == app.tree_cursor {
                style = style.patch(theme.selection_style());
            }
            let text = format!("{:<inner_width$}", truncate(&label, inner_width));
            lines.push((Line::from(Span::styled(text, style)), Some(buf.buffer_id)));
        }
    }

    // Keep the cursor (or the active buffer) on screen.
    let height = usize::from(area.height.saturating_sub(2));
    let target = app.tree_cursor.or(app.active_buffer_id);
    let target_line = lines
        .iter()
        .position(|(_, id)| id.is_some() && *id == target);
    let offset = target_line.map_or(0, |line| (line + 1).saturating_sub(height));

    let rows: Vec<Option<BufferId>> = lines.iter().skip(offset).map(|(_, id)| *id).collect();
    let text: Vec<Line<'static>> = lines
        .into_iter()
        .skip(offset)
        .map(|(line, _)| line)
        .collect();
    frame.render_widget(
        Paragraph::new(text).block(block(" Buffers ", focused, theme)),
        area,
    );
    rows
}

fn truncate(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.to_string();
    }
    let mut out = String::new();
    let mut used = 0;
    for c in text.chars() {
        let w = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if used + w + 1 > width {
            break;
        }
        out.push(c);
        used += w;
    }
    out.push('…');
    out
}

fn segment_style(kind: SegmentKind, theme: &Theme) -> Style {
    match kind {
        SegmentKind::Timestamp | SegmentKind::Event => theme.muted_style(),
        SegmentKind::Prefix => theme.fg(theme.accent),
        SegmentKind::Nick => Style::default().add_modifier(Modifier::BOLD),
        SegmentKind::Body => Style::default(),
        SegmentKind::Alert => theme.fg(theme.error),
    }
}

fn render_log(frame: &mut Frame<'_>, app: &mut App, theme: &Theme, area: Rect) {
    let focused = app.focus == Focus::Log;
    let title = match app
        .active_buffer_id
        .and_then(|id| app.state.buffers.get(&id))
    {
        Some(info) => {
            let network = app
                .state
                .networks
                .get(&info.network_id)
                .map(|n| sanitize_terminal(&n.network_name))
                .filter(|n| !n.is_empty());
            match network {
                Some(net) => format!(" {} — {net} ", buffer_label(info)),
                None => format!(" {} ", buffer_label(info)),
            }
        }
        None => " quasseltui ".to_string(),
    };
    let inner = area.inner(Margin::new(1, 1));
    app.log.set_viewport(inner.width, inner.height);

    let mut log_block = block(title, focused, theme);
    if !app.log.follow_tail {
        log_block = log_block.title_bottom(Line::from(" ↓ more below (End/PgDn) ").right_aligned());
    }

    if app.log.buffer.is_none() {
        let hint = if app.is_live() && !app.connection_lost {
            "Connecting…"
        } else {
            "No buffer selected"
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(hint, theme.muted_style()))).block(log_block),
            area,
        );
        return;
    }

    let highlighted = if focused { app.log.highlighted } else { None };
    let lines: Vec<Line<'static>> = app
        .log
        .visible(&app.state)
        .into_iter()
        .map(|(key, segments)| {
            let base = match key {
                RowKey::Marker => theme.fg(theme.highlight).add_modifier(Modifier::BOLD),
                RowKey::Date(_) => theme.muted_style().add_modifier(Modifier::DIM),
                RowKey::Message(_) => Style::default(),
            };
            let mut line = Line::from(
                segments
                    .into_iter()
                    .map(|s| {
                        let style = if matches!(key, RowKey::Message(_)) {
                            segment_style(s.kind, theme)
                        } else {
                            base
                        };
                        Span::styled(s.text, style)
                    })
                    .collect::<Vec<_>>(),
            );
            if matches!(key, RowKey::Message(id) if Some(id) == highlighted) {
                line = line.patch_style(theme.selection_style());
            }
            line
        })
        .collect();
    frame.render_widget(Paragraph::new(lines).block(log_block), area);
}

fn render_input(frame: &mut Frame<'_>, app: &App, theme: &Theme, area: Rect) {
    let focused = app.focus == Focus::Input && !app.input.disabled;
    let nick = app
        .active_buffer_id
        .and_then(|id| app.state.network_for_buffer(id))
        .map(|n| sanitize_terminal(&n.my_nick))
        .filter(|n| !n.is_empty());
    let title = nick.map_or_else(String::new, |n| format!(" {n} "));
    let mut input_block = block(title, focused, theme);
    if app.input.disabled {
        input_block = input_block.border_style(theme.fg(theme.error));
    }
    let inner = area.inner(Margin::new(1, 1));
    let width = usize::from(inner.width.max(1));

    if app.input.value.is_empty() {
        let placeholder = Paragraph::new(Span::styled(
            truncate(&app.input.placeholder, width),
            theme.muted_style(),
        ))
        .block(input_block);
        frame.render_widget(placeholder, area);
        if focused {
            frame.set_cursor_position(Position::new(inner.x, inner.y));
        }
        return;
    }

    // Scroll horizontally so the cursor stays visible.
    let chars: Vec<char> = app.input.value.chars().collect();
    let before: String = chars[..app.input.cursor.min(chars.len())].iter().collect();
    let cursor_col = before.width();
    let skip_cols = (cursor_col + 1).saturating_sub(width);
    let mut skipped = 0;
    let mut shown = String::new();
    for c in &chars {
        let w = unicode_width::UnicodeWidthChar::width(*c).unwrap_or(0);
        if skipped < skip_cols {
            skipped += w;
            continue;
        }
        shown.push(*c);
    }
    frame.render_widget(Paragraph::new(shown).block(input_block), area);
    if focused {
        let x = inner.x + u16::try_from(cursor_col - skipped.min(cursor_col)).unwrap_or(0);
        frame.set_cursor_position(Position::new(
            x.min(inner.right().saturating_sub(1)),
            inner.y,
        ));
    }
}

fn render_toasts(frame: &mut Frame<'_>, app: &App, theme: &Theme, area: Rect) {
    let width = area.width.saturating_sub(4).min(60);
    if width < 10 {
        return;
    }
    // Stack upward from the bottom of the scrollback, inside its border.
    let mut bottom = area.bottom().saturating_sub(1);
    for toast in app.toasts.iter().rev() {
        let text_width = usize::from(width - 2);
        let lines = toast.text.width().div_ceil(text_width.max(1)).clamp(1, 4) as u16;
        let height = lines + 2;
        if bottom < area.y + 1 + height {
            break;
        }
        let rect = Rect::new(
            area.right().saturating_sub(width + 2),
            bottom - height,
            width,
            height,
        );
        let color = match toast.severity {
            Severity::Info => theme.accent,
            Severity::Warning => theme.warning,
            Severity::Error => theme.error,
        };
        frame.render_widget(Clear, rect);
        frame.render_widget(
            Paragraph::new(toast.text.clone())
                .wrap(Wrap { trim: true })
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_type(BorderType::Rounded)
                        .border_style(theme.fg(color)),
                ),
            rect,
        );
        bottom -= height;
    }
}

const HELP: &[(&str, &str)] = &[
    ("Ctrl+Q", "Quit"),
    ("Ctrl+R", "Reconnect after a disconnect"),
    (
        "Alt+Up / Alt+Down",
        "Previous / next buffer (also Ctrl+P / Ctrl+N)",
    ),
    ("Tab / Shift+Tab", "Move focus: input, scrollback, sidebar"),
    ("PgUp / PgDn", "Scroll the scrollback"),
    ("Up / Down (input)", "Recall sent lines"),
    (
        "Enter (empty input)",
        "Move the read marker to the newest message",
    ),
    (
        "Esc (input)",
        "Leave the input for the scrollback (normal mode)",
    ),
    ("j / k", "Move the cursor (scrollback, sidebar)"),
    (
        "Ctrl+D / Ctrl+U",
        "Scroll half a page (Ctrl+E / Ctrl+Y: a line)",
    ),
    ("g / G", "First / last message or buffer"),
    ("J / K", "Next / previous buffer"),
    ("h / l", "Sidebar / open the buffer under the cursor"),
    (
        "Enter (scrollback)",
        "Place the read marker on that message",
    ),
    ("Enter (sidebar)", "Switch to that buffer"),
    ("i or Esc", "Back to the input"),
    ("F1 or ?", "This help"),
];

fn render_help(frame: &mut Frame<'_>, theme: &Theme, area: Rect) {
    let key_width = HELP.iter().map(|(k, _)| k.width()).max().unwrap_or(0);
    let width = (HELP.iter().map(|(_, d)| d.width()).max().unwrap_or(0) + key_width + 6)
        .min(usize::from(area.width.saturating_sub(2))) as u16;
    let height = (HELP.len() as u16 + 4).min(area.height);
    let rect = Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    );
    let mut lines: Vec<Line<'static>> = HELP
        .iter()
        .map(|(key, desc)| {
            Line::from(vec![
                Span::styled(format!("{key:<key_width$}  "), theme.accent_style()),
                Span::raw(*desc),
            ])
        })
        .collect();
    lines.push(Line::default());
    lines.push(Line::from(Span::styled(
        "Esc to close",
        theme.muted_style(),
    )));
    frame.render_widget(Clear, rect);
    frame.render_widget(
        Paragraph::new(lines).block(block(" Keys ", true, theme)),
        rect,
    );
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;
    use crate::app::demo::build_demo_state;
    use crate::client::{ClientState, IrcMessage};
    use crate::protocol::types::{
        BufferInfo, BufferType, MessageFlags, MessageType, MsgId, NetworkId,
    };
    use crate::sync::Network;

    fn draw(app: &mut App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| {
                render(frame, app, &Theme::default());
            })
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        let mut out = String::new();
        for y in 0..height {
            for x in 0..width {
                out.push_str(buffer[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn demo_renders_three_panes() {
        let mut app = App::new(build_demo_state(), false, false);
        let screen = draw(&mut app, 100, 20);
        assert!(screen.contains("Buffers"));
        assert!(screen.contains("Libera.Chat"));
        assert!(screen.contains("#python"));
        assert!(screen.contains("anyone awake on 3.14?"));
        assert!(screen.contains("Type a message"));
    }

    #[test]
    fn hostile_state_renders_without_escape_bytes() {
        let mut state = ClientState::new(0);
        let mut network = Network::new("1");
        network.network_name = "\x1b]0;pwned\x07Evil".into();
        state.networks.insert(NetworkId(1), network);
        let info = BufferInfo {
            buffer_id: BufferId(1),
            network_id: NetworkId(1),
            kind: BufferType::Channel,
            group_id: 0,
            name: "#\x1b[31mred".into(),
        };
        state.buffers.insert(BufferId(1), info);
        state.messages.insert(
            BufferId(1),
            vec![IrcMessage {
                msg_id: MsgId(1),
                buffer_id: BufferId(1),
                network_id: NetworkId(1),
                timestamp: chrono::Utc::now(),
                kind: MessageType::Plain,
                flags: MessageFlags::NONE,
                sender: "bad\x1b[2J".into(),
                sender_prefixes: String::new(),
                contents: "\x1b[31mREDRUM\x07".into(),
            }],
        );
        let mut app = App::new(state, false, false);
        let screen = draw(&mut app, 100, 12);
        assert!(!screen.contains('\x1b') && !screen.contains('\x07'));
        assert!(screen.contains("REDRUM"));
    }

    #[test]
    fn help_and_tiny_terminals_render() {
        let mut app = App::new(build_demo_state(), false, false);
        app.show_help = true;
        let screen = draw(&mut app, 100, 24);
        assert!(screen.contains("Keys"));
        // Absurdly small sizes must not panic.
        for (w, h) in [(1, 1), (5, 3), (20, 4), (30, 6)] {
            draw(&mut app, w, h);
        }
    }

    #[test]
    fn tree_rows_map_lines_to_buffers() {
        let mut app = App::new(build_demo_state(), false, false);
        let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
        let mut areas = Areas::default();
        terminal
            .draw(|frame| areas = render(frame, &mut app, &Theme::default()))
            .unwrap();
        assert_eq!(areas.tree_rows[0], None);
        assert_eq!(areas.tree_rows[1], Some(BufferId(10)));
        assert_eq!(areas.tree_rows[2], Some(BufferId(11)));
    }
}
