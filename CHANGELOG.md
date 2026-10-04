# Changelog

All notable changes to quasseltui are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

- **Rewritten in Rust.** quasseltui is now a single native binary built on
  Tokio, ratatui and crossterm. Install it with
  `cargo install --git https://github.com/linsomniac/quasseltui`; release
  builds attach a Linux binary instead of publishing to PyPI. The
  subcommands, flags, config file, exit codes, and keys are unchanged.
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

- `F1` shows every key; `PgUp`/`PgDn` and the mouse wheel scroll from
  anywhere; `Ctrl+P`/`Ctrl+N` switch buffers; clicking a buffer switches to
  it; `Esc` returns to the input bar.
- `tests/live_core.rs`, an opt-in end-to-end test against a real core.

### Fixed

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

[0.9.1]: https://github.com/linsomniac/quasseltui/compare/v0.9.0...v0.9.1
