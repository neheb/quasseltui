# Changelog

All notable changes to quasseltui are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

- **Unread channels show up on connect.** The sidebar marks channels with
  messages you haven't read, using the core's unread state (Quassel core
  0.13+), and clears the mark when another client reads them. Only chat
  lines (messages, notices, actions) count; joins, parts, quits and other
  events no longer mark a channel.
- **`j`/`k` switch channels** in normal mode; `J`/`K` are gone. `Down`/`Up`
  still move through the current channel.
- **Start in normal mode**, like vim: single keys navigate right away and
  `i` starts typing. A reconnect returns to normal mode unless it restores
  a half-typed draft.

### Added

- **Alerts for highlights and private messages** that arrive out of sight:
  the terminal bell plus a desktop notification (OSC 777), at most every
  30 seconds per buffer. `notify = false` in `[quasseltui]` turns them off.
- **Automatic reconnect.** After a disconnect quasseltui retries on its
  own, waiting 2s, then 4s, 8s, and so on up to once a minute; the input
  bar counts down, and `Ctrl+R` still reconnects right away. A rejected
  password, an unconfigured core, or a TLS downgrade isn't retried. The
  input comes back (with any half-typed draft) once the session opens.
- **Double-click to join, part, connect and disconnect.** Double-clicking a
  channel in the sidebar joins it, or parts it if you're in it;
  double-clicking a network or its `(status)` buffer connects or
  disconnects it. Channels you're not in are dimmed.
- **`yy` copies the selected message** in normal mode, through the
  terminal's clipboard support (OSC 52), so it also works over SSH. In
  tmux it needs `set-clipboard on`. To select text with the mouse instead,
  hold `Shift` while dragging.
- **Tab completes nicks** while typing: recent speakers first, then the
  rest of the channel; Tab again cycles, Shift+Tab goes back. A nick at
  the start of the line gets `: `. `Esc` (not `Tab`) leaves typing.
- **`q` quits** in normal mode, like `Ctrl+Q`.
- `hide_nick_changes = true` in the `[quasseltui]` config section hides
  nick changes from the scrollback, like `hide_joins_parts`.

### Fixed

- **Lines sent in quick succession can no longer arrive out of order.**
  Each request is queued when it's made, not when its background task
  first runs.
- **A hostile core can no longer bloat the session at startup.**
  `SessionInit` lists are capped (networks, identities, buffers) before
  they are seeded or turned into init requests.
- **Core-supplied text in CLI errors is escaped**, so a login rejection or
  handshake error can't send terminal control sequences.
- **`--connect-timeout` and `--duration` reject values that can't be a
  timeout** (negative, `inf`, `NaN`, huge) instead of panicking; the config
  file's `connect_timeout` gets the same check.
- **`probe-only` and `login-only` time out** on a core that stops answering
  mid-handshake, like the UI does.
- **`kill -INT` ends the UI cleanly**, restoring the terminal.
- **An empty `QUASSEL_PASSWORD` prompts** instead of failing.
- **A config file holding a password warns** when other users can read it,
  and is read once per run. Unknown keys in `[DEFAULT]` are reported.
- **Usage errors exit 1**, not clap's 2, which meant "connect failed".
- **A failed send says where the line went** when it can't go back in the
  input (Up recalls it). Long notices are no longer cut off at four lines.
- **`yy` only copies when nothing happened between the two `y`s.**

- **Busy channels no longer re-wrap the whole scrollback per line.** Each
  message is formatted and wrapped once, so a new line in a full channel
  costs about a tenth of what it did.
- `stream-only` no longer hangs at exit if the connection stalls while
  closing.
- `login-only`/`stream-only`/`dump-state` exit codes now follow the kind of
  error rather than the wording of its message.
- **Loading older history no longer discards most of what it fetched**
  near the 5000-line limit; requests only ask for what fits, and "start of
  history" is no longer shown while the core still has more.
- A history request the core never answers expires after 60 s instead of
  showing "loading older messages…" forever.
- Quitting no longer waits up to 90 s on a core that stopped reading, and a
  stalled UI ends the session with a clear reason instead of a misleading
  "core closed connection".
- Repeated or out-of-order live messages no longer show twice or land after
  the newest line.
- A channel renamed by the core is now shown under its new name, with the
  right joined/parted dimming.
- State the core can create is bounded: objects for unknown networks are
  ignored, and IRC users, channels and buffers are capped. Network
  initialization no longer copies the whole roster several times.
- The theme file is checked off the UI thread, and the sidebar only builds
  the rows on screen.
- `j`/`k` no longer stop working when the selected message is trimmed from
  a full channel.
- A core that answers with `FeatureList` but no `CoreFeatures` no longer
  fails the handshake.

## [0.10.0] - 2026-10-03

### Changed

- **Rewritten in Rust.** quasseltui is now a single native binary built on
  Tokio, ratatui and crossterm. Install it with `mise use -g
  github:neheb/quasseltui` or `cargo install --git
  https://github.com/neheb/quasseltui --branch rust-port`; releases attach
  static Linux binaries (x86_64 and ARMv7) instead of publishing to PyPI. The subcommands, flags, config file, exit codes, and
  keys are unchanged.
- **Colors follow the terminal.** The UI uses the terminal's own palette, so
  a themed terminal themes quasseltui. On Omarchy the theme's accent, muted,
  and selection colors are read too, and a theme switch applies live.
- Scrolling is anchored to the message at the top of the view, so incoming
  messages, fetched history, and window resizes never move what you're
  reading.
- Writes to the core go through a single queue, so a reply, a typed line,
  and a history request can no longer interleave on the wire. Closing the
  connection flushes what was already queued.

### Added

- aerc-style navigation: `Esc` (or `Tab`) switches from typing to a normal
  mode where `j`/`k` move through the current channel and `J`/`K` switch to
  the next/previous channel immediately; `Ctrl+D`/`Ctrl+U` scroll half a
  page, `g`/`G` jump to the ends, and `i` returns to typing. The input bar
  shows `NORMAL` in that mode. The sidebar is no longer a separate focus
  stop with its own cursor.
- Older history on demand: moving or scrolling up past the oldest loaded
  message fetches the previous 100 messages from the core, all the way back
  to the start of the buffer.
- `hide_joins_parts = true` in the `[quasseltui]` config section hides
  joins, parts, quits, and netsplits from the scrollback, and they no longer
  mark a buffer unread.
- `F1` shows every key; `PgUp`/`PgDn` and the mouse wheel scroll from
  anywhere; `Ctrl+P`/`Ctrl+N` switch buffers; clicking a buffer switches to
  it; `Esc` returns to the input bar.
- `tests/live_core.rs`, an opt-in end-to-end test against a real core.

### Fixed

- Closing the terminal window (e.g. Super+W on Hyprland) no longer crashes
  with SIGABRT. Restoring the dead terminal failed, the error was printed to
  the closed stderr, and that panicked twice. Terminal restore is now
  silent, and the process exits with 129 on SIGHUP (143 on SIGTERM).

- Server lines without a sender (such as topic announcements) no longer
  show a dangling `: `.

### Removed

- The Python package, the PyPI release job, and the developer-only
  `tests/tools/capture_session.py` (`stream-only -v` covers live
  inspection).

## [0.9.1] - 2026-06-01

This release is a stability and "feels less flaky" pass over the live client.
A review traced a vague flakiness to a handful of disconnect, scroll, and
buffer-selection gaps; each is fixed below and covered by new regression
tests.

### Fixed

- **Disconnects are now visible.** A mid-session drop used to fail silently —
  the UI went quiet and kept accepting typed lines that went nowhere. The
  input bar is now disabled with a placeholder that names the reason, and a
  notification is shown, so it's clear the connection is gone.
- **Scrollback stays put while you read.** On a busy channel, scrolling up to
  read history no longer yanks you back: incoming live messages and fetched
  backlog now keep the viewport anchored on the message you were reading.
  This is correct even for long lines that wrap across multiple rows.
- **The active channel no longer jumps on its own.** A message arriving in
  another channel can no longer steal your place, and the initial channel
  selection now lands where the activity actually is instead of an arbitrary
  one.
- **Failed actions tell you why.** A message that fails to send, or history
  that fails to load, now raises a notification instead of silently bouncing
  your text back or doing nothing.
- **The "read up to here" marker no longer drags the view.** Moving the
  marker (including the empty-Enter "mark latest" shortcut) keeps your scroll
  position instead of jumping the viewport to the marker's new spot.
- **Tabbing into the message log no longer jumps to the newest message** when
  you have scrolled up — the cursor lands on a visible row and the view stays
  where it was.
- **No more "Could not load history" spam after a disconnect.** History
  requests are no longer issued once the connection is gone, so switching
  channels post-drop doesn't produce repeated failures.

### Security

- Notifications and disconnect reasons that embed untrusted core-supplied
  text are now sanitized (control bytes escaped), length-bounded, and shown
  with markup disabled, so a hostile or malformed string such as
  `[Errno 104]` can't restyle or break the on-screen toast.

### Changed

- **Release builds carry the real version.** The CI release workflow now
  resolves the version from the release tag (stripping a leading `v`) and
  stamps it into the published PyPI sdist and wheel instead of the `0.0.0`
  placeholder, and attaches the built sdist + wheel to the GitHub release as
  downloadable assets. Manual `workflow_dispatch` runs accept an optional
  `version` input.

### Documentation

- Expanded the README with additional usage examples.

[0.10.0]: https://github.com/neheb/quasseltui/compare/v0.9.1...v0.10.0
[0.9.1]: https://github.com/linsomniac/quasseltui/compare/v0.9.0...v0.9.1
