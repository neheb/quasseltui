//! Command-line entry point.
//!
//! Subcommands:
//!
//! - `ui`: the UI against a live core.
//! - `ui-demo`: the UI on static placeholder data, no core needed.
//!
//! With a config file, bare `quasseltui` runs `ui` against the default
//! server and `quasseltui NAME` runs it against `[server:NAME]`.

use std::io::{self, IsTerminal, Write};
use std::process::ExitCode;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use clap::{Args, Parser, Subcommand};

use crate::app::demo::build_demo_state;
use crate::app::format::DisplaySettings;
use crate::app::last_buffer;
use crate::client::{ClientState, QuasselClient};
use crate::config::{self, Config, ConfigError};
use crate::protocol::connection::ConnectionOptions;
use crate::protocol::transport::TlsOptions;

pub const BUILD_DATE: &str = "2026-04-14";
const DEFAULT_CONNECT_TIMEOUT: f64 = 10.0;
const SUBCOMMANDS: [&str; 2] = ["ui-demo", "ui"];

pub fn client_version() -> String {
    format!("quasseltui v{}", crate::VERSION)
}

const EPILOG: &str = "Connection settings can live in a config file \
(~/.config/quasseltui/config.ini, XDG-aware) with named [server:NAME] sections. \
With a config in place, `quasseltui SERVER` is shorthand for \
`quasseltui ui --server SERVER`, and bare `quasseltui` connects to the default \
server. Note: put options AFTER the subcommand (`quasseltui ui --server home`, \
not `quasseltui --server home`).";

#[derive(Debug, Parser)]
#[command(
    name = "quasseltui",
    version,
    about = "Terminal client for Quassel IRC cores.",
    after_help = EPILOG,
    subcommand_required = true,
    arg_required_else_help = true
)]
pub struct Cli {
    #[command(subcommand)]
    pub mode: Mode,
}

#[derive(Debug, Subcommand)]
pub enum Mode {
    /// Launch the UI against static placeholder data. Useful for eyeballing
    /// the layout without a core. Press Ctrl+Q to quit.
    UiDemo,
    /// Launch the UI against a live Quassel core. Press Ctrl+Q to quit.
    Ui(LoginArgs),
}

#[derive(Debug, Clone, Default, Args)]
pub struct ConnectArgs {
    /// Use connection settings from [server:NAME] in the config file.
    /// Defaults to the config's default_server when omitted. Any explicit
    /// flag below overrides the corresponding config value.
    #[arg(long)]
    pub server: Option<String>,
    /// Quassel core hostname or IP
    #[arg(long)]
    pub host: Option<String>,
    /// Quassel core port
    #[arg(long, value_parser = clap::value_parser!(u16).range(1..))]
    pub port: Option<u16>,
    /// Do not offer encryption during the probe (plain TCP only). WARNING:
    /// your password is then sent in plaintext. Use only
    /// against trusted local cores.
    #[arg(long)]
    pub no_tls: bool,
    /// Skip TLS certificate verification (self-signed cores).
    #[arg(long)]
    pub insecure: bool,
    /// Path to a PEM bundle of trust anchors to use during TLS verification.
    #[arg(long)]
    pub cafile: Option<String>,
    /// Seconds to wait for the TCP connect, and again for the protocol
    /// handshake after it (default: 10).
    #[arg(long, value_parser = config::parse_seconds)]
    pub connect_timeout: Option<f64>,
}

#[derive(Clone, Default, Args)]
pub struct CredentialArgs {
    /// Username (env: QUASSEL_USER; config: [server:*] user; prompted if unset)
    #[arg(long)]
    pub user: Option<String>,
    /// Password — discouraged on the command line because it shows up in
    /// shell history and `ps`. Prefer the QUASSEL_PASSWORD env var, a
    /// password in the config file (mode 0600), or the interactive prompt.
    #[arg(long)]
    pub password: Option<String>,
}

#[derive(Debug, Clone, Args)]
pub struct LoginArgs {
    #[command(flatten)]
    pub connect: ConnectArgs,
    #[command(flatten)]
    pub credentials: CredentialArgs,
}

impl std::fmt::Debug for CredentialArgs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CredentialArgs")
            .field("user", &self.user)
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

/// Connection settings after merging CLI flags with the config file.
#[derive(Clone, PartialEq)]
pub struct Resolved {
    pub host: String,
    pub port: u16,
    pub user: Option<String>,
    pub password: Option<String>,
    pub no_tls: bool,
    pub insecure: bool,
    pub cafile: Option<String>,
    pub connect_timeout: f64,
}

impl std::fmt::Debug for Resolved {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Resolved")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("user", &self.user)
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .field("no_tls", &self.no_tls)
            .field("insecure", &self.insecure)
            .field("cafile", &self.cafile)
            .field("connect_timeout", &self.connect_timeout)
            .finish()
    }
}

impl Resolved {
    fn tls_options(&self) -> TlsOptions {
        TlsOptions {
            verify: !self.insecure,
            cafile: self.cafile.clone(),
            server_hostname: None,
        }
    }

    fn connect_timeout(&self) -> Duration {
        Duration::from_secs_f64(self.connect_timeout)
    }

    fn connection_options(&self, user: &str, password: &str) -> ConnectionOptions {
        let mut opts = ConnectionOptions::new(self.host.clone(), self.port, user, password);
        opts.tls = !self.no_tls;
        opts.tls_options = self.tls_options();
        opts.client_version = client_version();
        opts.build_date = BUILD_DATE.into();
        opts.connect_timeout = self.connect_timeout();
        opts
    }
}

/// Rewrite argv so the shortcuts become `ui`:
///
/// - `[]` -> `["ui"]`, only when the config has a default server (otherwise
///   the full help is shown);
/// - `["name", ...]` -> `["ui", "--server", "name", ...]`;
/// - a known subcommand or a leading flag is left alone.
pub fn normalize_argv(
    argv: Vec<String>,
    load: impl FnOnce() -> Result<Option<Config>, ConfigError>,
) -> Result<Vec<String>, ConfigError> {
    let Some(first) = argv.first() else {
        let cfg = load()?;
        return Ok(match cfg {
            Some(cfg) if cfg.default_server.is_some() => vec!["ui".into()],
            _ => argv,
        });
    };
    if SUBCOMMANDS.contains(&first.as_str()) || first.starts_with('-') {
        return Ok(argv);
    }
    let mut out = vec!["ui".to_string(), "--server".to_string()];
    out.extend(argv);
    Ok(out)
}

/// Merge CLI flags with the config. Precedence: explicit flag, then the
/// `[server:NAME]` value, then the built-in default. `NAME` is `--server`
/// or the config's `default_server`. The config isn't read at all when the
/// CLI already names a host and port and no `--server` was given.
pub fn resolve_connection(
    args: &ConnectArgs,
    credentials: Option<&CredentialArgs>,
    load: impl FnOnce() -> Result<Option<Config>, ConfigError>,
) -> Result<Resolved, String> {
    let mut resolved = Resolved {
        host: args.host.clone().unwrap_or_default(),
        port: args.port.unwrap_or(0),
        user: credentials.and_then(|c| c.user.clone()),
        password: credentials.and_then(|c| c.password.clone()),
        no_tls: args.no_tls,
        insecure: args.insecure,
        cafile: args.cafile.clone(),
        connect_timeout: args.connect_timeout.unwrap_or(f64::NAN),
    };
    let complete = args.host.is_some() && args.port.is_some();
    if args.server.is_some() || !complete {
        let cfg = load().map_err(|e| format!("config: {e}"))?;
        let server = match (&cfg, &args.server) {
            (Some(cfg), name) => {
                let server = cfg.resolve_server(name.as_deref());
                if let (Some(name), None) = (name, server) {
                    return Err(format!(
                        "no [server:{name}] section in {}",
                        cfg.path.display()
                    ));
                }
                server
            }
            (None, Some(name)) => {
                return Err(format!(
                    "--server '{name}' given but no config file at {}",
                    config::default_config_path().display()
                ));
            }
            (None, None) => None,
        };
        if let Some(server) = server {
            if args.host.is_none() {
                resolved.host = server.host.clone().unwrap_or_default();
            }
            if args.port.is_none() {
                resolved.port = server.port.unwrap_or(0);
            }
            if resolved.user.is_none() {
                resolved.user = server.user.clone();
            }
            if resolved.password.is_none() {
                resolved.password = server.password.clone();
            }
            // Flags can only turn TLS off and verification off, so config
            // can do the same but never undo an explicit flag.
            if server.tls == Some(false) {
                resolved.no_tls = true;
            }
            if server.insecure == Some(true) {
                resolved.insecure = true;
            }
            if resolved.cafile.is_none() {
                resolved.cafile = server.cafile.clone();
            }
            if resolved.connect_timeout.is_nan() {
                resolved.connect_timeout = server.connect_timeout.unwrap_or(f64::NAN);
            }
        }
    }
    if resolved.connect_timeout.is_nan() {
        resolved.connect_timeout = DEFAULT_CONNECT_TIMEOUT;
    }
    if resolved.host.is_empty() {
        return Err("host is required (set --host or configure a server)".into());
    }
    if resolved.port == 0 {
        return Err("port is required (set --port or configure a server)".into());
    }
    Ok(resolved)
}

/// Read a password without echo. `Ok(None)` if the user aborted (Ctrl+C,
/// Ctrl+D, EOF).
async fn prompt_password(prompt: &str) -> io::Result<Option<String>> {
    use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers, read};
    use tokio::signal::unix::{SignalKind, signal};

    let mut stderr = io::stderr();
    write!(stderr, "{prompt}")?;
    stderr.flush()?;
    if !io::stdin().is_terminal() {
        let mut line = String::new();
        if io::stdin().read_line(&mut line)? == 0 {
            return Ok(None);
        }
        return Ok(Some(line.trim_end_matches(['\n', '\r']).to_string()));
    }
    // Raw mode turns Ctrl+C into a key press, so it can't kill us with the
    // terminal's echo still switched off. The guard restores the terminal
    // however this ends, including this future being dropped.
    let raw = RawMode::enable()?;
    let read_password = tokio::task::spawn_blocking(|| -> io::Result<Option<String>> {
        let mut password = String::new();
        loop {
            let Event::Key(key) = read()? else { continue };
            if key.kind == KeyEventKind::Release {
                continue;
            }
            let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
            match key.code {
                KeyCode::Enter => return Ok(Some(password)),
                KeyCode::Char('c' | 'd') if ctrl => return Ok(None),
                KeyCode::Char('u') if ctrl => password.clear(),
                KeyCode::Backspace => {
                    password.pop();
                }
                KeyCode::Esc => return Ok(None),
                KeyCode::Char(c) if !ctrl => password.push(c),
                _ => {}
            }
        }
    });
    // A signal would otherwise kill us in raw mode: restore, then exit as
    // the signal would have (128 + its number).
    let mut term = signal(SignalKind::terminate())?;
    let mut hup = signal(SignalKind::hangup())?;
    let mut int = signal(SignalKind::interrupt())?;
    let code = tokio::select! {
        result = read_password => {
            drop(raw);
            writeln!(stderr)?;
            return result.map_err(io::Error::other)?;
        }
        _ = term.recv() => 143,
        _ = hup.recv() => 129,
        _ = int.recv() => 130,
    };
    drop(raw);
    let _ = writeln!(stderr);
    std::process::exit(code);
}

/// Raw terminal mode for as long as this lives.
struct RawMode;

impl RawMode {
    fn enable() -> io::Result<Self> {
        crossterm::terminal::enable_raw_mode()?;
        Ok(Self)
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

/// User and password from flags, config, environment, or a prompt.
async fn credentials(command: &str, resolved: &Resolved) -> Result<(String, String), ExitCode> {
    let user = resolved
        .user
        .clone()
        .filter(|u| !u.is_empty())
        .or_else(|| std::env::var("QUASSEL_USER").ok().filter(|u| !u.is_empty()));
    let Some(user) = user else {
        eprintln!("{command}: --user or QUASSEL_USER is required");
        return Err(ExitCode::from(1));
    };
    let password = match resolved
        .password
        .clone()
        .filter(|p| !p.is_empty())
        .or_else(|| {
            std::env::var("QUASSEL_PASSWORD")
                .ok()
                .filter(|p| !p.is_empty())
        }) {
        Some(p) => p,
        None => match prompt_password(&format!("Password for {user}@{}: ", resolved.host)).await {
            Ok(Some(p)) => p,
            Ok(None) | Err(_) => {
                eprintln!("\n{command}: aborted at password prompt");
                return Err(ExitCode::from(1));
            }
        },
    };
    if password.is_empty() {
        eprintln!("{command}: empty password not allowed");
        return Err(ExitCode::from(1));
    }
    Ok((user, password))
}

fn resolve_or_exit(
    command: &str,
    args: &ConnectArgs,
    creds: Option<&CredentialArgs>,
) -> Result<Resolved, ExitCode> {
    if creds.is_some_and(|c| c.password.is_some()) {
        eprintln!(
            "{command}: warning: --password shows up in `ps` and shell history; prefer \
             QUASSEL_PASSWORD, the config file, or the prompt"
        );
    }
    resolve_connection(args, creds, load_config).map_err(|message| {
        eprintln!("{command}: {message}");
        ExitCode::from(1)
    })
}

/// Log to `QUASSELTUI_LOG` if set, otherwise nowhere: a full-screen UI
/// can't show log lines without corrupting the screen.
fn init_logging() {
    let Some(path) = std::env::var_os("QUASSELTUI_LOG").filter(|p| !p.is_empty()) else {
        return;
    };
    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        Ok(file) => {
            let _ = tracing_subscriber::fmt()
                .with_max_level(tracing::Level::WARN)
                .with_ansi(false)
                .with_writer(Mutex::new(file))
                .try_init();
        }
        Err(e) => eprintln!(
            "ui: QUASSELTUI_LOG: {}: {e} — logging disabled",
            std::path::Path::new(&path).display()
        ),
    }
}

pub fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let normalized = match normalize_argv(argv, load_config) {
        Ok(argv) => argv,
        Err(e) => {
            eprintln!("quasseltui: config: {e}");
            return ExitCode::from(1);
        }
    };
    let cli = match Cli::try_parse_from(std::iter::once("quasseltui".to_string()).chain(normalized))
    {
        Ok(cli) => cli,
        Err(e) => return usage_exit(&e),
    };
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("quasseltui: failed to start the async runtime: {e}");
            return ExitCode::from(1);
        }
    };
    let code = match cli.mode {
        Mode::UiDemo => runtime.block_on(run_ui_demo()),
        Mode::Ui(args) => runtime.block_on(run_ui(args)),
    };
    // Don't wait for blocking threads: an interrupted password prompt
    // leaves one parked in a terminal read until a key is pressed.
    runtime.shutdown_background();
    code
}

/// The config file, read once per run: argv normalization, connection
/// settings and display settings all share the first result.
fn load_config() -> Result<Option<Config>, ConfigError> {
    static CONFIG: OnceLock<Result<Option<Config>, ConfigError>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            let cfg = config::load(None);
            if let Ok(Some(cfg)) = &cfg
                && let Some(warning) = cfg.permission_warning()
            {
                eprintln!("quasseltui: {warning}");
            }
            cfg
        })
        .clone()
}

/// Print clap's help, version or usage error. Usage errors exit 1, as
/// documented: clap's own 2 would read as "connect failed".
fn usage_exit(error: &clap::Error) -> ExitCode {
    let _ = error.print();
    if error.use_stderr() {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    }
}

/// Print to stderr after the UI has run. The terminal may be gone (its
/// window was closed), and `eprintln!` would panic on the failed write.
fn report(message: &str) {
    let _ = writeln!(io::stderr(), "{message}");
}

/// UI preferences from the config file.
fn display_settings(command: &str) -> Result<DisplaySettings, ExitCode> {
    match load_config() {
        Ok(cfg) => Ok(DisplaySettings {
            hide_joins_parts: cfg.as_ref().is_some_and(|c| c.hide_joins_parts),
            hide_nick_changes: cfg.as_ref().is_some_and(|c| c.hide_nick_changes),
            notify: cfg.is_none_or(|c| c.notify),
        }),
        Err(e) => {
            eprintln!("{command}: config: {e}");
            Err(ExitCode::from(1))
        }
    }
}

async fn run_ui_demo() -> ExitCode {
    let display = match display_settings("ui-demo") {
        Ok(d) => d,
        Err(code) => return code,
    };
    match crate::app::run(build_demo_state(), None, display, None).await {
        Ok((exit, _)) => ExitCode::from(exit.code as u8),
        Err(e) => {
            report(&format!("ui-demo: terminal error: {e}"));
            ExitCode::from(1)
        }
    }
}

/// The interactive client. Exits 1 on bad arguments or a failure before
/// the session opened (handshake, auth, TLS), printing the reason after the
/// terminal is restored. A mid-session drop stays in the UI.
async fn run_ui(args: LoginArgs) -> ExitCode {
    let resolved = match resolve_or_exit("ui", &args.connect, Some(&args.credentials)) {
        Ok(r) => r,
        Err(code) => return code,
    };
    let display = match display_settings("ui") {
        Ok(d) => d,
        Err(code) => return code,
    };
    let (user, password) = match credentials("ui", &resolved).await {
        Ok(c) => c,
        Err(code) => return code,
    };
    init_logging();
    let options = resolved.connection_options(&user, &password);
    let factory: crate::app::ClientFactory =
        Box::new(move || QuasselClient::connect(options.clone()));
    let state_file = last_buffer::default_path();
    let account = last_buffer::key(&user, &resolved.host, resolved.port);
    let start = last_buffer::load(&state_file, &account);
    match crate::app::run(ClientState::default(), Some(factory), display, start).await {
        Ok((exit, active)) => {
            if let Some(buffer) = active {
                last_buffer::save(&state_file, &account, buffer);
            }
            if let Some(message) = exit.message {
                report(&message);
            }
            ExitCode::from(exit.code as u8)
        }
        Err(e) => {
            report(&format!("ui: terminal error: {e}"));
            ExitCode::from(1)
        }
    }
}

#[cfg(test)]
mod tests;
