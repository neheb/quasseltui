use std::path::PathBuf;

use chrono::Utc;
use clap::CommandFactory;

use super::*;
use crate::client::IrcMessage;
use crate::config::ServerConfig;
use crate::protocol::Features;
use crate::protocol::types::{BufferId, BufferType, MessageFlags, MessageType, MsgId, NetworkId};
use crate::sync::Network;

fn args(server: Option<&str>, host: Option<&str>, port: Option<u16>) -> ConnectArgs {
    ConnectArgs {
        server: server.map(String::from),
        host: host.map(String::from),
        port,
        ..ConnectArgs::default()
    }
}

fn config_with(servers: Vec<ServerConfig>, default: Option<&str>) -> Config {
    Config {
        path: PathBuf::from("/tmp/config.ini"),
        default_server: default.map(String::from),
        servers: servers.into_iter().map(|s| (s.name.clone(), s)).collect(),
    }
}

fn home() -> ServerConfig {
    ServerConfig {
        name: "home".into(),
        host: Some("irc.example.com".into()),
        port: Some(4242),
        user: Some("sean".into()),
        password: Some("hunter2".into()),
        tls: Some(false),
        insecure: Some(true),
        cafile: Some("/etc/ssl/custom.pem".into()),
        connect_timeout: Some(15.0),
    }
}

fn argv(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}

#[test]
fn normalize_argv_shortcuts() {
    let no_default = || Ok(Some(config_with(vec![home()], None)));
    assert!(normalize_argv(vec![], no_default).unwrap().is_empty());
    assert!(normalize_argv(vec![], || Ok(None)).unwrap().is_empty());
    let with_default = || Ok(Some(config_with(vec![home()], Some("home"))));
    assert_eq!(normalize_argv(vec![], with_default).unwrap(), ["ui"]);

    let never = || -> Result<Option<Config>, ConfigError> { panic!("config must not be read") };
    assert_eq!(
        normalize_argv(argv(&["work", "--insecure"]), never).unwrap(),
        ["ui", "--server", "work", "--insecure"]
    );
    assert_eq!(
        normalize_argv(argv(&["dump-state", "--duration", "5"]), never).unwrap(),
        ["dump-state", "--duration", "5"]
    );
    assert_eq!(
        normalize_argv(argv(&["--version"]), never).unwrap(),
        ["--version"]
    );
    let broken = || Err(ConfigError("bad file".into()));
    assert!(normalize_argv(vec![], broken).is_err());
}

#[test]
fn resolve_short_circuits_when_cli_is_complete() {
    let never = || -> Result<Option<Config>, ConfigError> { panic!("config must not be read") };
    let resolved =
        resolve_connection(&args(None, Some("cli.example"), Some(4242)), None, never).unwrap();
    assert_eq!(resolved.host, "cli.example");
    assert_eq!(resolved.connect_timeout, 10.0);
}

#[test]
fn resolve_fills_from_server() {
    let resolved = resolve_connection(
        &args(Some("home"), None, None),
        Some(&CredentialArgs::default()),
        || Ok(Some(config_with(vec![home()], None))),
    )
    .unwrap();
    assert_eq!(
        resolved,
        Resolved {
            host: "irc.example.com".into(),
            port: 4242,
            user: Some("sean".into()),
            password: Some("hunter2".into()),
            no_tls: true,
            insecure: true,
            cafile: Some("/etc/ssl/custom.pem".into()),
            connect_timeout: 15.0,
        }
    );
}

#[test]
fn resolve_uses_default_server_and_cli_wins() {
    let creds = CredentialArgs {
        user: Some("cliuser".into()),
        password: None,
    };
    let mut cli = args(None, Some("cli.example"), None);
    cli.connect_timeout = Some(3.0);
    let resolved = resolve_connection(&cli, Some(&creds), || {
        Ok(Some(config_with(vec![home()], Some("home"))))
    })
    .unwrap();
    assert_eq!(resolved.host, "cli.example");
    assert_eq!(resolved.port, 4242);
    assert_eq!(resolved.user.as_deref(), Some("cliuser"));
    assert_eq!(resolved.connect_timeout, 3.0);
}

#[test]
fn resolve_errors() {
    let err = resolve_connection(&args(Some("ghost"), None, None), None, || {
        Ok(Some(config_with(vec![home()], None)))
    })
    .unwrap_err();
    assert!(err.contains("ghost"), "{err}");

    let err = resolve_connection(&args(Some("ghost"), None, None), None, || Ok(None)).unwrap_err();
    assert!(err.contains("no config file"), "{err}");

    let err = resolve_connection(&args(None, None, None), None, || Ok(None)).unwrap_err();
    assert!(err.contains("host is required"), "{err}");

    let err = resolve_connection(&args(None, Some("h"), None), None, || Ok(None)).unwrap_err();
    assert!(err.contains("port is required"), "{err}");

    let err = resolve_connection(&args(None, None, None), None, || {
        Err(ConfigError("boom".into()))
    })
    .unwrap_err();
    assert!(err.starts_with("config:") && err.contains("boom"), "{err}");
}

#[test]
fn cli_parses_and_help_mentions_config() {
    Cli::command().debug_assert();
    let help = Cli::command().render_long_help().to_string();
    assert!(help.contains("config.ini"));
    assert!(help.contains("quasseltui SERVER"));

    let parsed = Cli::try_parse_from([
        "quasseltui",
        "stream-only",
        "--host",
        "h",
        "--port",
        "1",
        "-v",
    ])
    .unwrap();
    let Mode::StreamOnly(stream) = parsed.mode else {
        panic!("expected stream-only");
    };
    assert!(stream.verbose);
    assert_eq!(stream.duration, 60.0);
    assert!(Cli::try_parse_from(["quasseltui"]).is_err());
    assert!(Cli::try_parse_from(["quasseltui", "--server", "home"]).is_err());
}

#[test]
fn disconnect_exit_codes() {
    assert_eq!(
        disconnect_exit_code("auth rejected: x", Some(&Error::Auth("x".into()))),
        7
    );
    assert_eq!(
        disconnect_exit_code("handshake failed", Some(&Error::Transport("x".into()))),
        2
    );
    assert_eq!(
        disconnect_exit_code(
            "core did not enable TLS ... plaintext",
            Some(&Error::Probe("x".into()))
        ),
        5
    );
    assert_eq!(disconnect_exit_code("core is not configured", None), 6);
    assert_eq!(
        disconnect_exit_code("core rejected ClientInit: 'old'", None),
        3
    );
    assert_eq!(disconnect_exit_code("frame read error", None), 4);
}

#[test]
fn session_summary_is_sanitized() {
    let mut ident = VariantMap::new();
    ident.insert("identityId".into(), Variant::Int(1));
    ident.insert("identityName".into(), "evil\x1b[31mname".into());
    let session = SessionInit {
        identities: vec![ident],
        network_ids: vec![NetworkId(1)],
        buffer_infos: vec![BufferInfo {
            buffer_id: BufferId(10),
            network_id: NetworkId(1),
            kind: BufferType::Channel,
            group_id: 0,
            name: "#chan\x07beep".into(),
        }],
    };
    let mut out = Vec::new();
    write_session_init(&mut out, &session).unwrap();
    let text = String::from_utf8(out).unwrap();
    assert!(!text.contains('\x1b') && !text.contains('\x07'));
    assert!(text.contains("\\x1b") && text.contains("\\x07"));
    assert!(text.contains("connected — 1 identities, 1 networks, 1 buffers"));
    assert!(text.contains("(id=1)"));
    assert!(text.contains("[chan] #chan\\x07beep (buffer_id=10)"));
}

#[test]
fn state_snapshot_is_sanitized_and_complete() {
    let mut state = ClientState::new(0);
    state.peer_features = Features::LONG_TIME;
    let mut network = Network::new("1");
    network.network_name = "Libera\x1b[2J".into();
    network.my_nick = "seanr".into();
    state.networks.insert(NetworkId(1), network);
    state.buffers.insert(
        BufferId(10),
        BufferInfo {
            buffer_id: BufferId(10),
            network_id: NetworkId(1),
            kind: BufferType::Channel,
            group_id: 0,
            name: "#python".into(),
        },
    );
    state.messages.insert(
        BufferId(10),
        (1..=3)
            .map(|i| IrcMessage {
                msg_id: MsgId(i),
                buffer_id: BufferId(10),
                network_id: NetworkId(1),
                timestamp: Utc::now(),
                kind: MessageType::Plain,
                flags: MessageFlags::NONE,
                sender: "evil\x07".into(),
                sender_prefixes: "@".into(),
                contents: format!("line {i}\x1b[31m"),
            })
            .collect(),
    );
    let mut counts = BTreeMap::new();
    counts.insert("MessageReceived", 3);
    let mut out = Vec::new();
    write_state_snapshot(&mut out, &state, 2, &counts).unwrap();
    let text = String::from_utf8(out).unwrap();
    assert!(!text.contains('\x1b') && !text.contains('\x07'));
    assert!(text.contains("peer_features: LongTime"));
    assert!(text.contains("counts: networks=1, buffers=1, identities=0, messages=3"));
    assert!(text.contains("  MessageReceived: 3"));
    assert!(text.contains("[1] Libera\\x1b[2J  state=Disconnected nick=seanr server=?"));
    assert!(text.contains("[chan] #python (buffer_id=10, 3 msgs)"));
    assert!(!text.contains("line 1"));
    assert!(text.contains("@evil\\x07: line 3\\x1b[31m"));

    let mut out = Vec::new();
    write_state_snapshot(&mut out, &ClientState::default(), 5, &BTreeMap::new()).unwrap();
    assert!(String::from_utf8(out).unwrap().contains("networks: (none)"));
}

#[test]
fn stream_event_lines() {
    let mut out = Vec::new();
    write_stream_event(
        &mut out,
        &ProtocolEvent::Disconnected {
            reason: "bye\x1b".into(),
            error: None,
        },
        false,
    )
    .unwrap();
    assert_eq!(
        String::from_utf8(out).unwrap(),
        "-- disconnected: bye\\x1b\n"
    );
}
