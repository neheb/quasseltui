//! Command-line entry point.
//!
//! Subcommands:
//!
//! - `probe-only`: probe + optional TLS + `ClientInit`, print the reply.
//! - `login-only`: the full handshake through `SessionInit`, print a summary.
//! - `stream-only`: log in, then print SignalProxy events for `--duration`.
//! - `dump-state`: run the client stack for `--duration`, print the state.
//! - `ui-demo`: the UI on static placeholder data, no core needed.
//! - `ui`: the UI against a live core.
//!
//! With a config file, bare `quasseltui` runs `ui` against the default
//! server and `quasseltui NAME` runs it against `[server:NAME]`.
//!
//! Exit codes (headless commands): 0 ok, 1 bad arguments or missing
//! credentials, 2 connect failed, 3 core rejected ClientInit, 4 protocol
//! error, 5 TLS downgrade, 6 core not configured, 7 auth rejected,
//! 130 interrupted.

use std::collections::BTreeMap;
use std::io::{self, IsTerminal, Write};
use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use clap::{Args, Parser, Subcommand};

use crate::app::demo::build_demo_state;
use crate::app::format::DisplaySettings;
use crate::client::{ClientState, QuasselClient};
use crate::config::{self, Config, ConfigError};
use crate::protocol::connection::{
    BoxedStream, ConnectionOptions, ProtocolEvent, QuasselConnection,
};
use crate::protocol::error::Error;
use crate::protocol::handshake::{recv_handshake_message, send_client_init, send_client_login};
use crate::protocol::messages::{
    ClientInit, ClientInitAck, ClientLogin, HandshakeMessage, SessionInit,
};
use crate::protocol::probe::{ConnectionFeatures, NegotiatedProtocol, probe};
use crate::protocol::transport::{TlsOptions, close_stream, open_tcp_connection, start_tls};
use crate::protocol::types::BufferInfo;
use crate::qt::variant::{Variant, VariantMap};
use crate::sync::events::ClientEvent;
use crate::util::text::sanitize_terminal;

pub const BUILD_DATE: &str = "2026-04-14";
const DEFAULT_CONNECT_TIMEOUT: f64 = 10.0;
const SUBCOMMANDS: [&str; 6] = [
    "probe-only",
    "login-only",
    "stream-only",
    "dump-state",
    "ui-demo",
    "ui",
];

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
    /// Run the probe + ClientInit handshake against a core, print the reply, and exit.
    ProbeOnly(ProbeArgs),
    /// Run the full handshake (probe + ClientInit + ClientLogin) against a
    /// core, print the SessionInit summary, and exit.
    LoginOnly(LoginArgs),
    /// Run the full handshake, then stream SignalProxy events from the core
    /// for --duration seconds. Pretty-prints each event.
    StreamOnly(StreamArgs),
    /// Run the client stack for --duration seconds, then print a snapshot of
    /// the client state (networks, buffers, messages, identities).
    DumpState(DumpArgs),
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
    #[arg(long)]
    pub port: Option<u16>,
    /// Do not offer encryption during the probe (plain TCP only). WARNING:
    /// commands that log in then send your password in plaintext. Use only
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
    #[arg(long)]
    pub connect_timeout: Option<f64>,
}

#[derive(Debug, Clone, Default, Args)]
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
pub struct ProbeArgs {
    #[command(flatten)]
    pub connect: ConnectArgs,
    /// Continue if the core does not enable TLS even though we offered it.
    /// Without this flag we abort to prevent a downgrade attack.
    #[arg(long)]
    pub allow_plaintext: bool,
}

#[derive(Debug, Clone, Args)]
pub struct LoginArgs {
    #[command(flatten)]
    pub connect: ConnectArgs,
    #[command(flatten)]
    pub credentials: CredentialArgs,
}

#[derive(Debug, Clone, Args)]
pub struct StreamArgs {
    #[command(flatten)]
    pub login: LoginArgs,
    /// Seconds to stream events after handshake.
    #[arg(long, default_value_t = 60.0)]
    pub duration: f64,
    /// Optional cap on how many events to print before exiting.
    #[arg(long)]
    pub max_events: Option<u64>,
    /// Print the raw params/init_data on each event (may be long).
    #[arg(long, short)]
    pub verbose: bool,
}

#[derive(Debug, Clone, Args)]
pub struct DumpArgs {
    #[command(flatten)]
    pub login: LoginArgs,
    /// Seconds to accumulate state before dumping.
    #[arg(long, default_value_t = 30.0)]
    pub duration: f64,
    /// Maximum messages per buffer to print in the summary.
    #[arg(long, default_value_t = 5)]
    pub max_messages: usize,
}

/// Connection settings after merging CLI flags with the config file.
#[derive(Debug, Clone, PartialEq)]
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
fn prompt_password(prompt: &str) -> io::Result<Option<String>> {
    use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers, read};
    use crossterm::terminal::{disable_raw_mode, enable_raw_mode};

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
    // terminal's echo still switched off.
    enable_raw_mode()?;
    let result = (|| -> io::Result<Option<String>> {
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
    })();
    disable_raw_mode()?;
    writeln!(stderr)?;
    result
}

/// User and password from flags, config, environment, or a prompt.
fn credentials(command: &str, resolved: &Resolved) -> Result<(String, String), ExitCode> {
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
        .or_else(|| std::env::var("QUASSEL_PASSWORD").ok())
    {
        Some(p) => p,
        None => match prompt_password(&format!("Password for {user}@{}: ", resolved.host)) {
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
    resolve_connection(args, creds, || config::load(None)).map_err(|message| {
        eprintln!("{command}: {message}");
        ExitCode::from(1)
    })
}

#[derive(Clone, Copy)]
enum LogTarget {
    Stderr,
    /// `QUASSELTUI_LOG` if set, otherwise nothing: a full-screen UI can't
    /// show log lines without corrupting the screen.
    UiFile,
}

fn init_logging(target: LogTarget, verbose: bool) {
    use tracing_subscriber::fmt;
    let level = if verbose {
        tracing::Level::INFO
    } else {
        tracing::Level::WARN
    };
    match target {
        LogTarget::Stderr => {
            let _ = fmt()
                .with_max_level(level)
                .with_writer(io::stderr)
                .try_init();
        }
        LogTarget::UiFile => {
            let Some(path) = std::env::var_os("QUASSELTUI_LOG").filter(|p| !p.is_empty()) else {
                return;
            };
            match std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
            {
                Ok(file) => {
                    let _ = fmt()
                        .with_max_level(level)
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
    }
}

pub fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let normalized = match normalize_argv(argv, || config::load(None)) {
        Ok(argv) => argv,
        Err(e) => {
            eprintln!("quasseltui: config: {e}");
            return ExitCode::from(1);
        }
    };
    let cli = Cli::parse_from(std::iter::once("quasseltui".to_string()).chain(normalized));
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
    match cli.mode {
        Mode::UiDemo => runtime.block_on(run_ui_demo()),
        Mode::Ui(args) => runtime.block_on(run_ui(args)),
        Mode::ProbeOnly(args) => interruptible(&runtime, probe_only(args)),
        Mode::LoginOnly(args) => interruptible(&runtime, login_only(args)),
        Mode::StreamOnly(args) => interruptible(&runtime, stream_only(args)),
        Mode::DumpState(args) => interruptible(&runtime, dump_state(args)),
    }
}

/// Headless commands run for a while by design; Ctrl+C is a normal way to
/// end them early (128 + SIGINT, the shell convention).
fn interruptible(
    runtime: &tokio::runtime::Runtime,
    command: impl Future<Output = ExitCode>,
) -> ExitCode {
    runtime.block_on(async {
        tokio::select! {
            code = command => code,
            _ = tokio::signal::ctrl_c() => {
                eprintln!("interrupted");
                ExitCode::from(130)
            }
        }
    })
}

/// Print to stderr after the UI has run. The terminal may be gone (its
/// window was closed), and `eprintln!` would panic on the failed write.
fn report(message: &str) {
    let _ = writeln!(io::stderr(), "{message}");
}

/// UI preferences from the config file.
fn display_settings(command: &str) -> Result<DisplaySettings, ExitCode> {
    match config::load(None) {
        Ok(cfg) => Ok(DisplaySettings {
            hide_joins_parts: cfg.is_some_and(|c| c.hide_joins_parts),
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
    match crate::app::run(build_demo_state(), None, display).await {
        Ok(exit) => ExitCode::from(exit.code as u8),
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
    let (user, password) = match credentials("ui", &resolved) {
        Ok(c) => c,
        Err(code) => return code,
    };
    init_logging(LogTarget::UiFile, false);
    let options = resolved.connection_options(&user, &password);
    let factory: crate::app::ClientFactory =
        Box::new(move || QuasselClient::connect(options.clone()));
    match crate::app::run(ClientState::default(), Some(factory), display).await {
        Ok(exit) => {
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

fn negotiated_report(n: &NegotiatedProtocol) -> String {
    format!(
        "protocol:    {:?}\npeer feats:  {:#06x}\nconn feats:  {}\n",
        n.protocol, n.peer_features, n.connection_features
    )
}

fn python_bool(value: bool) -> &'static str {
    if value { "True" } else { "False" }
}

pub fn write_init_reply(out: &mut impl Write, reply: &HandshakeMessage) -> io::Result<()> {
    match reply {
        HandshakeMessage::ClientInitReject { error } => {
            writeln!(
                out,
                "core REJECTED ClientInit: '{}'",
                sanitize_terminal(error)
            )
        }
        HandshakeMessage::ClientInitAck(ack) => write_init_ack(out, ack),
        _ => Ok(()),
    }
}

fn write_init_ack(out: &mut impl Write, ack: &ClientInitAck) -> io::Result<()> {
    writeln!(out, "core accepted ClientInit:")?;
    writeln!(out, "  configured:    {}", python_bool(ack.configured))?;
    writeln!(out, "  core features: {:#010x}", ack.core_features)?;
    if !ack.feature_list.is_empty() {
        writeln!(
            out,
            "  feature list:  {}",
            sanitize_terminal(&ack.feature_list.join(", "))
        )?;
    }
    if let Some(version) = ack.protocol_version {
        writeln!(out, "  proto version: {version}")?;
    }
    if !ack.storage_backends.is_empty() {
        writeln!(out, "  storage backends:")?;
        for b in &ack.storage_backends {
            writeln!(
                out,
                "    - {}: {}",
                sanitize_terminal(&b.display_name),
                sanitize_terminal(&b.description)
            )?;
        }
    }
    if !ack.authenticators.is_empty() {
        writeln!(out, "  authenticators:")?;
        for a in &ack.authenticators {
            writeln!(
                out,
                "    - {}: {}",
                sanitize_terminal(&a.display_name),
                sanitize_terminal(&a.description)
            )?;
        }
    }
    Ok(())
}

/// Open TCP, probe, and upgrade to TLS when negotiated. On a TLS downgrade
/// (offered, not enabled) returns exit code 5 unless allowed.
async fn connect_and_probe(
    resolved: &Resolved,
    allow_plaintext: bool,
    downgrade_message: &str,
) -> Result<BoxedStream, ExitCode> {
    let tcp = match open_tcp_connection(&resolved.host, resolved.port, resolved.connect_timeout())
        .await
    {
        Ok(s) => s,
        Err(e) => {
            eprintln!("connect: {e}");
            return Err(ExitCode::from(2));
        }
    };
    let mut stream: BoxedStream = Box::new(tcp);
    let offered = if resolved.no_tls {
        ConnectionFeatures::NONE
    } else {
        ConnectionFeatures::ENCRYPTION
    };
    let negotiated = match probe(&mut stream, offered).await {
        Ok(n) => n,
        Err(e) => {
            eprintln!("protocol: {e}");
            return Err(ExitCode::from(4));
        }
    };
    print!("{}", negotiated_report(&negotiated));
    if negotiated.tls_required() {
        match start_tls(stream, &resolved.host, &resolved.tls_options()).await {
            Ok(tls) => stream = Box::new(tls),
            Err(e) => {
                eprintln!("protocol: {e}");
                return Err(ExitCode::from(4));
            }
        }
        println!("TLS upgrade ok");
    } else if !resolved.no_tls && !allow_plaintext {
        // The probe reply is unauthenticated, so an active MITM can strip
        // the Encryption bit. Fail closed.
        eprintln!("{downgrade_message}");
        return Err(ExitCode::from(5));
    }
    Ok(stream)
}

async fn probe_only(args: ProbeArgs) -> ExitCode {
    init_logging(LogTarget::Stderr, false);
    let resolved = match resolve_or_exit("probe-only", &args.connect, None) {
        Ok(r) => r,
        Err(code) => return code,
    };
    let mut stream = match connect_and_probe(
        &resolved,
        args.allow_plaintext,
        "abort: core did not enable TLS but we offered it. This is a downgrade and could be a \
         MITM. Re-run with --allow-plaintext if you actually trust this network path.",
    )
    .await
    {
        Ok(s) => s,
        Err(code) => return code,
    };
    let code = async {
        send_client_init(&mut stream, &ClientInit::new(client_version(), BUILD_DATE)).await?;
        let reply = recv_handshake_message(&mut stream).await?;
        let code = match &reply {
            HandshakeMessage::ClientInitAck(_) => 0,
            HandshakeMessage::ClientInitReject { .. } => 3,
            other => {
                eprintln!("unexpected handshake reply at init phase: {}", other.kind());
                return Ok::<u8, Error>(4);
            }
        };
        write_init_reply(&mut io::stdout(), &reply)?;
        Ok(code)
    }
    .await
    .unwrap_or_else(|e| {
        eprintln!("protocol: {e}");
        4
    });
    close_stream(&mut stream).await;
    ExitCode::from(code)
}

async fn login_only(args: LoginArgs) -> ExitCode {
    init_logging(LogTarget::Stderr, false);
    let resolved = match resolve_or_exit("login-only", &args.connect, Some(&args.credentials)) {
        Ok(r) => r,
        Err(code) => return code,
    };
    let (user, password) = match credentials("login-only", &resolved) {
        Ok(c) => c,
        Err(code) => return code,
    };
    let mut stream = match connect_and_probe(
        &resolved,
        false,
        "abort: core did not enable TLS but we offered it. This is a downgrade and would leak \
         the password. Re-run with --no-tls if you actually trust this network path.",
    )
    .await
    {
        Ok(s) => s,
        Err(code) => return code,
    };
    let code = login_exchange(&mut stream, user, password)
        .await
        .unwrap_or_else(|e| {
            eprintln!("protocol: {e}");
            4
        });
    close_stream(&mut stream).await;
    ExitCode::from(code)
}

async fn login_exchange(
    stream: &mut BoxedStream,
    user: String,
    password: String,
) -> Result<u8, Error> {
    let mut stdout = io::stdout();
    send_client_init(stream, &ClientInit::new(client_version(), BUILD_DATE)).await?;
    let init = recv_handshake_message(stream).await?;
    match &init {
        HandshakeMessage::ClientInitReject { .. } => {
            write_init_reply(&mut stdout, &init)?;
            return Ok(3);
        }
        HandshakeMessage::ClientInitAck(ack) => {
            write_init_ack(&mut stdout, ack)?;
            if !ack.configured {
                eprintln!(
                    "abort: core is not configured yet. quasseltui does not implement the \
                     CoreSetupData wizard — finish setup in quasselclient first."
                );
                return Ok(6);
            }
        }
        other => {
            eprintln!(
                "unexpected handshake message during init phase: {}",
                other.kind()
            );
            return Ok(4);
        }
    }
    send_client_login(stream, &ClientLogin { user, password }).await?;
    match recv_handshake_message(stream).await {
        Err(Error::Auth(msg)) => {
            eprintln!("login rejected: {msg}");
            return Ok(7);
        }
        Err(e) => return Err(e),
        Ok(HandshakeMessage::ClientLoginAck) => println!("login ok"),
        Ok(HandshakeMessage::CoreSetupReject { error }) => {
            eprintln!("core setup rejected: '{}'", sanitize_terminal(&error));
            return Ok(6);
        }
        Ok(other) => {
            eprintln!(
                "unexpected handshake message during login phase: {}",
                other.kind()
            );
            return Ok(4);
        }
    }
    match recv_handshake_message(stream).await? {
        HandshakeMessage::SessionInit(session) => {
            write_session_init(&mut stdout, &session)?;
            Ok(0)
        }
        other => {
            eprintln!("expected SessionInit, got {}", other.kind());
            Ok(4)
        }
    }
}

fn identity_field(ident: &VariantMap, a: &str, b: &str) -> Option<Variant> {
    ident
        .get(a)
        .filter(|v| v.truthy())
        .or_else(|| ident.get(b))
        .cloned()
}

/// The `SessionInit` summary. Names are core-controlled (and ultimately
/// IRC-user-controlled), so everything is sanitized.
pub fn write_session_init(out: &mut impl Write, session: &SessionInit) -> io::Result<()> {
    writeln!(
        out,
        "connected — {} identities, {} networks, {} buffers",
        session.identities.len(),
        session.network_ids.len(),
        session.buffer_infos.len()
    )?;
    if !session.identities.is_empty() {
        writeln!(out, "identities:")?;
        for ident in &session.identities {
            let name = identity_field(ident, "identityName", "IdentityName")
                .and_then(|v| v.coerce_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "?".into());
            let id = identity_field(ident, "identityId", "IdentityId")
                .and_then(|v| v.coerce_string())
                .unwrap_or_else(|| "None".into());
            writeln!(
                out,
                "  - {} (id={})",
                sanitize_terminal(&name),
                sanitize_terminal(&id)
            )?;
        }
    }
    if !session.network_ids.is_empty() {
        writeln!(out, "networks:")?;
        let mut ids = session.network_ids.clone();
        ids.sort();
        for id in ids {
            writeln!(out, "  - network_id={id}")?;
        }
    }
    if !session.buffer_infos.is_empty() {
        writeln!(out, "buffers:")?;
        let mut by_network: BTreeMap<i32, Vec<&BufferInfo>> = BTreeMap::new();
        for buf in &session.buffer_infos {
            by_network.entry(buf.network_id.0).or_default().push(buf);
        }
        for (net, mut buffers) in by_network {
            writeln!(out, "  network_id={net} ({} buffers)", buffers.len())?;
            buffers.sort_by_key(|b| (b.kind.value(), b.name.to_lowercase()));
            for buf in buffers {
                let name = if buf.name.is_empty() {
                    "(unnamed)"
                } else {
                    &buf.name
                };
                writeln!(
                    out,
                    "    [{}] {} (buffer_id={})",
                    buf.kind.label(),
                    sanitize_terminal(name),
                    buf.buffer_id
                )?;
            }
        }
    }
    Ok(())
}

/// Map a terminal disconnect to the `login-only` exit codes.
pub fn disconnect_exit_code(reason: &str, error: Option<&Error>) -> u8 {
    if error.is_some_and(Error::is_auth) {
        return 7;
    }
    if error.is_some_and(Error::is_transport) {
        return 2;
    }
    let reason = reason.to_lowercase();
    if reason.contains("tls") && reason.contains("plaintext") {
        5
    } else if reason.contains("not configured") {
        6
    } else if reason.contains("rejected clientinit") {
        3
    } else {
        4
    }
}

async fn stream_only(args: StreamArgs) -> ExitCode {
    init_logging(LogTarget::Stderr, args.verbose);
    let login = &args.login;
    let resolved = match resolve_or_exit("stream-only", &login.connect, Some(&login.credentials)) {
        Ok(r) => r,
        Err(code) => return code,
    };
    let (user, password) = match credentials("stream-only", &resolved) {
        Ok(c) => c,
        Err(code) => return code,
    };
    let (handle, mut events) =
        QuasselConnection::new(resolved.connection_options(&user, &password)).start();
    let verbose = args.verbose;
    let max_events = args.max_events;
    let run = async {
        let mut count = 0u64;
        while let Some(event) = events.recv().await {
            write_stream_event(&mut io::stdout(), &event, verbose).ok();
            match &event {
                ProtocolEvent::Disconnected { reason, error } => {
                    return disconnect_exit_code(reason, error.as_deref());
                }
                // The handshake banner isn't one of the streamed events
                // that --max-events counts.
                ProtocolEvent::SessionReady { .. } => continue,
                _ => {}
            }
            count += 1;
            if max_events.is_some_and(|max| count >= max) {
                println!(
                    "[max-events={} reached, stopping]",
                    max_events.unwrap_or_default()
                );
                return 0;
            }
        }
        0
    };
    let code =
        match tokio::time::timeout(Duration::from_secs_f64(args.duration.max(0.0)), run).await {
            Ok(code) => code,
            Err(_) => {
                println!("[duration={}s elapsed, stopping]", args.duration);
                0
            }
        };
    handle.close();
    while events.recv().await.is_some() {}
    ExitCode::from(code)
}

fn ascii(bytes: &[u8]) -> String {
    sanitize_terminal(&String::from_utf8_lossy(bytes))
}

pub fn write_stream_event(
    out: &mut impl Write,
    event: &ProtocolEvent,
    verbose: bool,
) -> io::Result<()> {
    match event {
        ProtocolEvent::SessionReady {
            session,
            peer_features,
            ..
        } => {
            write_session_init(out, session)?;
            writeln!(out, "negotiated features: {peer_features}")?;
            writeln!(out, "[streaming events…]")
        }
        ProtocolEvent::Sync(m) => {
            let suffix = if verbose {
                format!("  params={}", sanitize_terminal(&format!("{:?}", m.params)))
            } else {
                String::new()
            };
            writeln!(
                out,
                "Sync  {}::{} {} ({} params){suffix}",
                ascii(&m.class_name),
                sanitize_terminal(&m.object_name),
                ascii(&m.slot_name),
                m.params.len()
            )
        }
        ProtocolEvent::Rpc(m) => {
            let suffix = if verbose {
                format!("  params={}", sanitize_terminal(&format!("{:?}", m.params)))
            } else {
                String::new()
            };
            writeln!(
                out,
                "Rpc   {} ({} params){suffix}",
                ascii(&m.signal_name),
                m.params.len()
            )
        }
        ProtocolEvent::InitData(m) => {
            let suffix = if verbose {
                format!(
                    "  data={}",
                    sanitize_terminal(&format!("{:?}", m.init_data))
                )
            } else {
                String::new()
            };
            writeln!(
                out,
                "Init  {}::{} ({} keys){suffix}",
                ascii(&m.class_name),
                sanitize_terminal(&m.object_name),
                m.init_data.len()
            )
        }
        ProtocolEvent::InitRequest(m) => writeln!(
            out,
            "IReq  {}::{}",
            ascii(&m.class_name),
            sanitize_terminal(&m.object_name)
        ),
        ProtocolEvent::HeartBeat(ts) => writeln!(
            out,
            "Heart ts={}",
            ts.to_naive().format("%Y-%m-%dT%H:%M:%S%.3f")
        ),
        ProtocolEvent::Disconnected { reason, .. } => {
            writeln!(out, "-- disconnected: {}", sanitize_terminal(reason))
        }
    }
}

async fn dump_state(args: DumpArgs) -> ExitCode {
    init_logging(LogTarget::Stderr, false);
    let login = &args.login;
    let resolved = match resolve_or_exit("dump-state", &login.connect, Some(&login.credentials)) {
        Ok(r) => r,
        Err(code) => return code,
    };
    let (user, password) = match credentials("dump-state", &resolved) {
        Ok(c) => c,
        Err(code) => return code,
    };
    let mut client = QuasselClient::connect(resolved.connection_options(&user, &password));
    let mut state = ClientState::default();
    let mut counts: BTreeMap<&'static str, usize> = BTreeMap::new();
    let mut last_disconnect: Option<(String, Option<Arc<Error>>)> = None;
    let run = async {
        while let Some(event) = client.next_event(&mut state).await {
            *counts.entry(event.name()).or_default() += 1;
            if let ClientEvent::Disconnected { reason, error } = event {
                last_disconnect = Some((reason, error));
                return;
            }
        }
    };
    let _ = tokio::time::timeout(Duration::from_secs_f64(args.duration.max(0.0)), run).await;
    client.close();
    // Let the connection finish closing (bounded) before printing.
    let _ = tokio::time::timeout(Duration::from_secs(3), async {
        while client.recv().await.is_some() {}
    })
    .await;

    let mut code = 0;
    if let Some((reason, error)) = &last_disconnect {
        code = disconnect_exit_code(reason, error.as_deref());
        println!(
            "-- disconnected before duration elapsed: {}",
            sanitize_terminal(reason)
        );
    }
    if let Err(e) = write_state_snapshot(&mut io::stdout(), &state, args.max_messages, &counts) {
        eprintln!("dump-state: {e}");
    }
    ExitCode::from(code)
}

/// The `ClientState` snapshot. Every core-provided string is sanitized so a
/// hostile IRC payload can't inject terminal escapes into the output.
pub fn write_state_snapshot(
    out: &mut impl Write,
    state: &ClientState,
    max_messages: usize,
    counts: &BTreeMap<&'static str, usize>,
) -> io::Result<()> {
    writeln!(out)?;
    writeln!(out, "=== ClientState snapshot ===")?;
    writeln!(
        out,
        "peer_features: {}",
        sanitize_terminal(&state.peer_features.to_string())
    )?;
    writeln!(
        out,
        "counts: networks={}, buffers={}, identities={}, messages={}",
        state.networks.len(),
        state.buffers.len(),
        state.identities.len(),
        state.total_message_count()
    )?;
    if !counts.is_empty() {
        writeln!(out, "event_counts:")?;
        for (name, count) in counts {
            writeln!(out, "  {}: {count}", sanitize_terminal(name))?;
        }
    }
    if !state.identities.is_empty() {
        writeln!(out, "identities:")?;
        for (id, identity) in &state.identities {
            let nicks = if identity.nicks.is_empty() {
                "(no nicks)".to_string()
            } else {
                identity.nicks.join(", ")
            };
            let name = if identity.identity_name.is_empty() {
                "(unnamed)"
            } else {
                &identity.identity_name
            };
            writeln!(
                out,
                "  - [{id}] {} ({})",
                sanitize_terminal(name),
                sanitize_terminal(&nicks)
            )?;
        }
    }
    if state.networks.is_empty() {
        return writeln!(out, "networks: (none)");
    }
    writeln!(out, "networks:")?;
    let or = |s: &str, fallback: &str| {
        if s.is_empty() {
            fallback.to_string()
        } else {
            sanitize_terminal(s)
        }
    };
    for (network_id, network) in &state.networks {
        writeln!(
            out,
            "  - [{network_id}] {}  state={} nick={} server={}",
            or(&network.network_name, "(unnamed)"),
            network.connection_state.name(),
            or(&network.my_nick, "?"),
            or(&network.current_server, "?")
        )?;
        let mut buffers: Vec<&BufferInfo> = state
            .buffers
            .values()
            .filter(|b| b.network_id == *network_id)
            .collect();
        buffers.sort_by_key(|b| (b.kind.value(), b.name.to_lowercase()));
        for buf in buffers {
            let messages = state.messages_for_buffer(buf.buffer_id);
            writeln!(
                out,
                "      [{}] {} (buffer_id={}, {} msgs)",
                buf.kind.label(),
                or(&buf.name, "(unnamed)"),
                buf.buffer_id,
                messages.len()
            )?;
            if max_messages > 0 {
                for msg in &messages[messages.len().saturating_sub(max_messages)..] {
                    let prefix = if msg.sender_prefixes.is_empty() {
                        " ".to_string()
                    } else {
                        sanitize_terminal(&msg.sender_prefixes)
                    };
                    writeln!(
                        out,
                        "          {} {prefix}{}: {}",
                        msg.timestamp.format("%Y-%m-%d %H:%M:%S"),
                        sanitize_terminal(&msg.sender),
                        sanitize_terminal(&msg.contents)
                    )?;
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
