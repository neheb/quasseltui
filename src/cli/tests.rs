use std::path::PathBuf;

use clap::CommandFactory;

use super::*;
use crate::config::ServerConfig;

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
        hide_joins_parts: false,
        hide_nick_changes: false,
        notify: true,
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
        normalize_argv(argv(&["ui", "--host", "h"]), never).unwrap(),
        ["ui", "--host", "h"]
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

    let parsed = Cli::try_parse_from(["quasseltui", "ui", "--host", "h", "--port", "1"]).unwrap();
    let Mode::Ui(login) = parsed.mode else {
        panic!("expected ui");
    };
    assert_eq!(login.connect.host.as_deref(), Some("h"));
    assert!(Cli::try_parse_from(["quasseltui", "dump-state"]).is_err());
    assert!(Cli::try_parse_from(["quasseltui"]).is_err());
    assert!(Cli::try_parse_from(["quasseltui", "--server", "home"]).is_err());
}

#[test]
fn flags_reject_bad_values() {
    let parse = |flag: &str, value: &str| {
        Cli::try_parse_from(["quasseltui", "ui", "--host", "h", flag, value])
    };
    for bad in ["-1", "inf", "NaN", "1e30", "soon"] {
        assert!(parse("--connect-timeout", bad).is_err(), "{bad}");
    }
    assert!(parse("--connect-timeout", "2.5").is_ok());
    assert!(parse("--port", "0").is_err());
    assert!(parse("--port", "4242").is_ok());
}

#[test]
fn debug_output_redacts_passwords() {
    let creds = CredentialArgs {
        user: Some("me".into()),
        password: Some("hunter2".into()),
    };
    assert!(!format!("{creds:?}").contains("hunter2"));
    let resolved = resolve_connection(
        &ConnectArgs {
            host: Some("h".into()),
            port: Some(1),
            ..ConnectArgs::default()
        },
        Some(&creds),
        || Ok(None),
    )
    .unwrap();
    let shown = format!("{resolved:?}");
    assert!(
        !shown.contains("hunter2") && shown.contains("<redacted>"),
        "{shown}"
    );
}
