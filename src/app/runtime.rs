//! The terminal loop: draws the [`App`], feeds it terminal input and client
//! events, and carries out its effects.
//!
//! Everything runs on one task. The client's `recv` is cancel-safe, so it
//! sits in the same `select!` as terminal input. Requests to the core are
//! queued in order on this task; waiting for each write runs as a small
//! spawned task that reports failures back through a channel, so a slow
//! socket never blocks typing. Bursts of client events are applied
//! together before a redraw, which replaces the old 50 ms debounce.

use std::io;
use std::time::{Duration, Instant};

use crossterm::cursor::Show;
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event,
    EventStream, KeyEventKind, MouseButton, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use futures::{FutureExt, StreamExt};
use ratatui::layout::{Margin, Position};
use tokio::sync::mpsc;

use crate::app::format::DisplaySettings;
use crate::app::model::{App, Effect, Exit};
use crate::app::theme::{Theme, ThemeWatcher};
use crate::app::view::{self, Areas};
use crate::client::{ClientState, DEFAULT_BACKLOG_LIMIT, QuasselClient, Request};
use crate::protocol::connection::ProtocolEvent;
use crate::protocol::types::BufferId;

/// Builds a fresh client over the same credentials (for Ctrl+R).
pub type ClientFactory = Box<dyn Fn() -> QuasselClient + Send>;

/// Failures of spawned requests, reported back to the app.
enum Failure {
    Send { text: String, error: String },
    Command { text: String, error: String },
    Backlog { buffer: BufferId, error: String },
    OlderBacklog { buffer: BufferId, error: String },
}

enum Input {
    Terminal(Option<io::Result<Event>>),
    Protocol(Option<ProtocolEvent>),
    Outcome(Failure),
    Theme(Theme),
    Tick,
    Signal(i32),
}

/// Enter raw mode and the alternate screen, and make panics restore the
/// terminal first.
///
/// This replaces `ratatui::init`, whose restore reports failure with
/// `eprintln!`. When the terminal window has been closed, stderr is gone,
/// so that `eprintln!` panics, ratatui's panic hook restores (and prints)
/// again, and the second panic aborts the process.
fn init_terminal() -> io::Result<ratatui::DefaultTerminal> {
    terminal::enable_raw_mode()?;
    let setup = execute!(
        io::stdout(),
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableBracketedPaste
    );
    if let Err(e) = setup {
        restore_terminal();
        return Err(e);
    }
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        previous(info);
    }));
    match ratatui::Terminal::new(ratatui::backend::CrosstermBackend::new(io::stdout())) {
        Ok(t) => Ok(t),
        Err(e) => {
            restore_terminal();
            Err(e)
        }
    }
}

/// Undo [`init_terminal`]. Best effort and silent: the terminal may already
/// be gone, and there is nowhere to report that.
fn restore_terminal() {
    let _ = execute!(
        io::stdout(),
        DisableMouseCapture,
        DisableBracketedPaste,
        LeaveAlternateScreen,
        Show
    );
    let _ = terminal::disable_raw_mode();
}

/// How many already-queued client events to apply before redrawing.
const EVENT_BATCH: usize = 512;

/// Run the UI until the user quits or a fatal error. With no factory the
/// app runs offline (the demo).
pub async fn run(
    state: ClientState,
    factory: Option<ClientFactory>,
    display: DisplaySettings,
) -> io::Result<Exit> {
    // Set up the terminal before connecting: without one there is no point
    // opening a session.
    let mut terminal = init_terminal().map_err(|e| {
        io::Error::new(
            e.kind(),
            format!("cannot start the terminal UI ({e}); run it from an interactive terminal"),
        )
    })?;

    let mut client = factory.as_ref().map(|make| make());
    let mut app = App::new(state, client.is_some(), factory.is_some());
    app.set_display(display);
    let result = event_loop(&mut terminal, &mut app, &mut client, factory.as_ref()).await;
    restore_terminal();

    // Close after the terminal is back, so a slow TLS goodbye can't leave
    // the user staring at a frozen screen.
    if let Some(mut client) = client {
        client.close();
        let _ = tokio::time::timeout(Duration::from_secs(3), async {
            while client.recv().await.is_some() {}
        })
        .await;
    }
    result
}

async fn event_loop(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut App,
    client: &mut Option<QuasselClient>,
    factory: Option<&ClientFactory>,
) -> io::Result<Exit> {
    let mut terminal_events = EventStream::new();
    let (outcome_tx, mut outcome_rx) = mpsc::unbounded_channel();
    let mut watcher = ThemeWatcher::new();
    let mut theme = watcher.load();
    let mut theme_rx = watch_theme(watcher);
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    let mut client_done = client.is_none();
    let mut areas = Areas::default();
    let mut dirty = true;
    let mut terminate = termination_signals()?;

    loop {
        if dirty {
            terminal.draw(|frame| areas = view::render(frame, app, &theme))?;
            dirty = false;
        }
        if let Some(exit) = app.exit.take() {
            return Ok(exit);
        }

        let input = tokio::select! {
            event = terminal_events.next() => Input::Terminal(event),
            event = async {
                match client.as_mut() {
                    Some(c) if !client_done => c.recv().await,
                    _ => std::future::pending().await,
                }
            } => Input::Protocol(event),
            Some(outcome) = outcome_rx.recv() => Input::Outcome(outcome),
            Some(new_theme) = theme_rx.recv() => Input::Theme(new_theme),
            _ = tick.tick() => Input::Tick,
            Some(code) = terminate.recv() => Input::Signal(code),
        };

        let mut effects = Vec::new();
        match input {
            Input::Terminal(None) => {
                return Ok(Exit {
                    code: 0,
                    message: None,
                });
            }
            Input::Terminal(Some(Err(e))) => return Err(e),
            Input::Terminal(Some(Ok(event))) => {
                effects.extend(handle_terminal_event(app, &areas, event));
                dirty = true;
            }
            Input::Protocol(None) => client_done = true,
            Input::Protocol(Some(first)) => {
                let Some(c) = client.as_mut() else { continue };
                let mut next = Some(first);
                let mut handled = 0;
                while let Some(event) = next.take() {
                    for client_event in c.apply(event, &mut app.state) {
                        effects.extend(app.on_client_event(&client_event));
                    }
                    handled += 1;
                    if handled < EVENT_BATCH {
                        // `recv` is cancel-safe, so polling it once is too.
                        next = c.recv().now_or_never().flatten();
                    }
                }
                dirty = true;
            }
            Input::Outcome(Failure::Send { text, error }) => {
                app.send_failed(text, &error);
                dirty = true;
            }
            Input::Outcome(Failure::Command { text, error }) => {
                app.command_failed(&text, &error);
                dirty = true;
            }
            Input::Outcome(Failure::Backlog { buffer, error }) => {
                app.backlog_failed(buffer, &error);
                dirty = true;
            }
            Input::Outcome(Failure::OlderBacklog { buffer, error }) => {
                app.older_backlog_failed(buffer, &error);
                dirty = true;
            }
            Input::Theme(new_theme) => {
                theme = new_theme;
                dirty = true;
            }
            Input::Tick => {
                let now = Instant::now();
                dirty |= app.tick(now);
                effects.extend(app.retry_due(now));
            }
            Input::Signal(code) => {
                return Ok(Exit {
                    code,
                    message: None,
                });
            }
        }

        for effect in effects {
            if let Effect::Copy(text) = &effect {
                copy_to_clipboard(text);
                continue;
            }
            if let Effect::Reconnect = effect {
                if let Some(make) = factory {
                    if let Some(old) = client.take() {
                        old.close();
                    }
                    *client = Some(make());
                    client_done = false;
                }
                continue;
            }
            if let Some(c) = client.as_ref() {
                spawn_effect(c, effect, outcome_tx.clone());
            }
        }
    }
}

fn handle_terminal_event(app: &mut App, areas: &Areas, event: Event) -> Vec<Effect> {
    match event {
        Event::Key(key) if key.kind != KeyEventKind::Release => app.on_key(key),
        Event::Paste(text) => {
            app.on_paste(&text);
            Vec::new()
        }
        Event::Mouse(mouse) => {
            let at = Position::new(mouse.column, mouse.row);
            match mouse.kind {
                MouseEventKind::Down(MouseButton::Left) => {
                    let tree = areas.tree.inner(Margin::new(1, 1));
                    if tree.contains(at) {
                        let line = usize::from(at.y - tree.y);
                        if let Some(Some(row)) = areas.tree_rows.get(line) {
                            return app.click_tree(*row, Instant::now());
                        }
                    } else if areas.log.contains(at) {
                        app.click_log(usize::from(at.y - areas.log.y));
                    } else if areas.input.contains(at) {
                        app.set_focus(crate::app::model::Focus::Input);
                    }
                    Vec::new()
                }
                MouseEventKind::ScrollUp => app.scroll_log(-3),
                MouseEventKind::ScrollDown => app.scroll_log(3),
                _ => Vec::new(),
            }
        }
        _ => Vec::new(),
    }
}

/// Poll the theme file once a second on a thread of its own, so a slow
/// filesystem can't stall the UI. The thread ends with the receiver.
fn watch_theme(mut watcher: ThemeWatcher) -> mpsc::UnboundedReceiver<Theme> {
    let (tx, rx) = mpsc::unbounded_channel();
    if watcher.is_active() {
        let spawned = std::thread::Builder::new()
            .name("theme-watcher".into())
            .spawn(move || {
                while !tx.is_closed() {
                    std::thread::sleep(Duration::from_secs(1));
                    if let Some(theme) = watcher.poll()
                        && tx.send(theme).is_err()
                    {
                        break;
                    }
                }
            });
        if let Err(e) = spawned {
            tracing::warn!("theme watcher not started: {e}");
        }
    }
    rx
}

/// Queue one request now, so requests reach the core in the order the app
/// asked for them; only the wait for the write runs as a task. Failures
/// come back as outcomes.
fn spawn_effect(client: &QuasselClient, effect: Effect, outcomes: mpsc::UnboundedSender<Failure>) {
    let handle = client.handle();
    let report = move |failure: Failure| {
        let _ = outcomes.send(failure);
    };
    match effect {
        Effect::SendInput { buffer, text } => {
            let sent = handle.send_input(buffer, text.clone());
            on_failure(sent, move |error| report(Failure::Send { text, error }));
        }
        Effect::SendCommand { buffer, text } => {
            let sent = handle.send_input(buffer, text.clone());
            on_failure(sent, move |error| report(Failure::Command { text, error }));
        }
        Effect::SetNetworkConnected { network, connect } => {
            let sent = handle.set_network_connected(network, connect);
            let text = if connect { "Connect" } else { "Disconnect" }.to_string();
            on_failure(sent, move |error| report(Failure::Command { text, error }));
        }
        Effect::RequestOlderBacklog {
            buffer,
            before,
            limit,
        } => {
            let sent = handle.request_backlog_before(buffer, before, limit);
            on_failure(sent, move |error| {
                report(Failure::OlderBacklog { buffer, error });
            });
        }
        Effect::RequestBacklog(buffer) => {
            let sent = handle.request_backlog(buffer, DEFAULT_BACKLOG_LIMIT);
            on_failure(sent, move |error| {
                report(Failure::Backlog { buffer, error })
            });
        }
        // Losing one read-state sync is invisible locally, and the next
        // switch retries naturally, so these are only logged.
        Effect::SetLastSeen(buffer, msg) => {
            let sent = handle.set_last_seen(buffer, msg);
            on_failure(sent, move |error| {
                tracing::warn!("set_last_seen failed for buffer {buffer}: {error}");
            });
        }
        Effect::SetMarkerLine(buffer, msg) => {
            let sent = handle.set_marker_line(buffer, msg);
            on_failure(sent, move |error| {
                tracing::warn!("set_marker_line failed for buffer {buffer}: {error}");
            });
        }
        Effect::Reconnect | Effect::Copy(_) => {}
    }
}

/// Wait for an already-queued request in the background.
fn on_failure(sent: Request, failed: impl FnOnce(String) + Send + 'static) {
    tokio::spawn(async move {
        if let Err(e) = sent.await {
            failed(e.to_string());
        }
    });
}

/// Ask the terminal to put `text` on the clipboard (OSC 52). This works
/// over SSH, and in tmux with `set-clipboard on`; terminals that don't
/// support it ignore the sequence.
fn copy_to_clipboard(text: &str) {
    use std::io::Write;
    let mut out = io::stdout();
    let written =
        write!(out, "\x1b]52;c;{}\x07", base64(text.as_bytes())).and_then(|()| out.flush());
    if let Err(e) = written {
        tracing::warn!("copy to clipboard failed: {e}");
    }
}

/// Standard base64 with padding.
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, &b)| n | u32::from(b) << (16 - 8 * i));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(char::from(ALPHABET[(n >> (18 - 6 * i)) as usize & 63]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// SIGTERM, SIGHUP and SIGINT end the UI cleanly (restoring the terminal) instead
/// of killing the process in raw mode.
/// Yields the exit code to use (128 + signal number).
fn termination_signals() -> io::Result<mpsc::UnboundedReceiver<i32>> {
    use tokio::signal::unix::{SignalKind, signal};
    let (tx, rx) = mpsc::unbounded_channel();
    // Ctrl+C in raw mode is a key, not SIGINT, but `kill -INT` still
    // arrives as a signal.
    for (kind, code) in [
        (SignalKind::terminate(), 143),
        (SignalKind::hangup(), 129),
        (SignalKind::interrupt(), 130),
    ] {
        let mut stream = signal(kind)?;
        let tx = tx.clone();
        tokio::spawn(async move {
            if stream.recv().await.is_some() {
                let _ = tx.send(code);
            }
        });
    }
    Ok(rx)
}

#[cfg(test)]
mod tests {
    use super::base64;

    #[test]
    fn base64_pads() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64("é\u{1f600}".as_bytes()), "w6nwn5iA");
    }
}
