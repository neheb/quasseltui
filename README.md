# quasseltui

Terminal client for [Quassel IRC](https://www.quassel-irc.org/) cores. Connects
to your existing `quasselcore` and gives you a terminal UI as an alternative to
`quasselclient` (the Qt GUI) or Quasseldroid. It's a single Rust binary with
no runtime to install.

![quasseltui in action](docs/screenshot.png)

## Quick start

Install with Cargo (Rust 1.88 or newer; TLS uses the system OpenSSL):

```sh
cargo install --git https://github.com/neheb/quasseltui --branch rust-port
quasseltui --help
```

Or from a clone:

```sh
cargo run --release -- --help
```

Static Linux binaries for x86_64 and 32-bit ARMv7 (armhf) are attached to
each [GitHub release](https://github.com/neheb/quasseltui/releases); they
don't depend on the system's glibc or OpenSSL. With
[mise](https://mise.jdx.dev):

```sh
mise use -g github:neheb/quasseltui
```

## Config file

Instead of using command-line arguments (`quasseltui --help` for more information), you can use a config file.

Connection settings can be loaded from `~/.config/quasseltui/config.ini`
(or `$XDG_CONFIG_HOME/quasseltui/config.ini`) so that `--host`, `--port`,
`--user`, etc. don't have to be repeated on every invocation.

Example:

```ini
[quasseltui]
default_server = home
# hide_joins_parts = false  (true hides joins, parts, quits and netsplits)

[server:home]
host = irc.example.com
port = 4242
user = linsomniac
password = hunter2
# tls = true              (default; set to false for plain TCP)
# insecure = false        (skip cert verification; self-signed cores)
# cafile = /path/to/ca.pem
# connect_timeout = 10

[server:work]
host = irc.work.example
port = 4242
user = linsomniac
```

Because the file stores the password, make sure it's readable only by
you:

```sh
chmod 600 ~/.config/quasseltui/config.ini
```

With a config in place, three shortcuts become available:

- `quasseltui` — connects to `default_server`.
- `quasseltui <NAME>` — connects to `[server:<NAME>]`.
- `quasseltui ui --server <NAME>` — same as above, explicit form, and
  also works with `login-only` / `stream-only` / `dump-state` / `probe-only`.

Any command-line flag still overrides the corresponding config value.

`hide_joins_parts = true` in `[quasseltui]` leaves joins, parts, quits, and
netsplit joins/quits out of the scrollback. They also stop counting as
unread activity. Nothing is discarded: turn the option off and restart to
see them again.

## Keys

quasseltui has two modes, like aerc or vim. You start out **typing**; `Esc`
(or `Tab`) switches to **normal** mode, where single keys navigate. The input
bar shows `NORMAL` while you're in it.

| Key | Action |
| --- | --- |
| `Ctrl+Q` | Quit |
| `Ctrl+R` | Reconnect after a disconnect (history is kept; the gap is re-fetched) |
| `Esc` / `Tab` | Switch between typing and normal mode |
| `Alt+Up` / `Alt+Down` (or `Ctrl+P` / `Ctrl+N`) | Previous / next channel, in either mode |
| `PgUp` / `PgDn`, mouse wheel | Scroll, in either mode |
| `Up` / `Down` (typing) | Recall previously sent lines |
| `Enter` (empty input bar) | Move the read marker to the newest message |
| `j` / `k` or `Down` / `Up` (normal) | Move through the current channel |
| `J` / `K` or `Shift+Down` / `Shift+Up` (normal) | Next / previous channel |
| `Ctrl+D` / `Ctrl+U` (normal) | Scroll half a page (`Ctrl+E` / `Ctrl+Y`: one line) |
| `g` / `G` (normal) | First / last message |
| `Enter` (normal) | Place the read marker on the selected message |
| `i` (normal) | Back to typing |
| Click a channel | Switch to it |
| `F1` (or `?` in normal mode) | Show all keys |

Scrolling or moving up past the oldest loaded message fetches the next 100
older messages from the core, so the whole history is reachable. The
scrollback's top border says `loading older messages…` while that's in
flight and `start of history` once there's nothing older. Each buffer keeps up
to 5000 messages in memory; past that, the border says `history limit
reached`.

Buffers with unseen activity are bold in the sidebar; highlights and
private messages are bold yellow. Read state and markers sync through
the core, so reading here marks things read in your other Quassel
clients (and vice versa on the next run).

Set `QUASSELTUI_LOG=/path/to/file` to capture runtime log output for
debugging — by default the TUI swallows it so it can't corrupt the
screen.

## Colors

The UI uses your terminal's own colors: the default foreground and
background, plus the named ANSI colors for meaning (yellow for highlights,
red for errors). A themed terminal themes quasseltui too.

On [Omarchy](https://omarchy.org), the current theme's `accent`, `muted`
and `selection` colors are also read from
`~/.local/state/omarchy/current/theme/colors.toml` for focus borders, the
active buffer, and the cursor row. Switching themes updates a running
quasseltui within a second. This needs a truecolor terminal
(`COLORTERM=truecolor`, which Omarchy's terminals set); elsewhere the accent
falls back to ANSI blue. `NO_COLOR` turns colors off.

## Headless commands

`probe-only`, `login-only`, `stream-only`, and `dump-state` exercise the
protocol without the UI, which is handy for checking a core or debugging.
They exit with distinct codes: 0 ok, 1 bad arguments or credentials,
2 connect failed, 3 core rejected the client, 4 protocol error, 5 TLS
downgrade refused, 6 core not configured, 7 login rejected, 130 interrupted.

## Development

```sh
cargo test                                   # unit tests
cargo clippy --all-targets -- -D warnings    # lint
cargo fmt                                    # format
```

The code is layered bottom to top: `qt` (Qt binary serialization),
`protocol` (probe, TLS, handshake, SignalProxy, connection), `sync` (the
syncable object model and `ClientState`), `client` (the embeddable client),
and `app` (the terminal UI).

`tests/live_core.rs` runs against a real core when `QUASSEL_TEST_HOST` (and
`QUASSEL_TEST_PORT`, `QUASSEL_TEST_USER`, `QUASSEL_TEST_PASSWORD`, optionally
`QUASSEL_TEST_INSECURE=1`) are set; otherwise it skips.
