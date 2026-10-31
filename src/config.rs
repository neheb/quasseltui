//! The user config file.
//!
//! Location: `$XDG_CONFIG_HOME/quasseltui/config.toml`, or
//! `~/.config/quasseltui/config.toml`.
//!
//! ```toml
//! default_server = "home"
//! # hide_joins_parts = false
//! # hide_nick_changes = false
//! # notify = true
//!
//! [server.home]
//! host = "irc.example.com"
//! port = 4242
//! user = "sean"
//! password = "hunter2"
//! # tls = true            (default)
//! # insecure = false      (skip TLS cert verification; self-signed cores)
//! # cafile = "/etc/ssl/certs/quassel.pem"
//! # connect_timeout = 10
//! ```
//!
//! Every setting is optional and falls back to the CLI flag (and then, for
//! the user and password, to `QUASSEL_USER`/`QUASSEL_PASSWORD` or a
//! prompt). Unknown keys are errors so typos surface.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// The file exists but can't be read, parsed, or validated.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct ConfigError(pub String);

/// One `[server.NAME]` table. `None` means "not set here".
#[derive(Clone, PartialEq, Default)]
pub struct ServerConfig {
    pub name: String,
    pub host: Option<String>,
    pub port: Option<u16>,
    pub user: Option<String>,
    pub password: Option<String>,
    pub tls: Option<bool>,
    pub insecure: Option<bool>,
    pub cafile: Option<String>,
    pub connect_timeout: Option<f64>,
}

impl std::fmt::Debug for ServerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServerConfig")
            .field("name", &self.name)
            .field("host", &self.host)
            .field("port", &self.port)
            .field("user", &self.user)
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .field("tls", &self.tls)
            .field("insecure", &self.insecure)
            .field("cafile", &self.cafile)
            .field("connect_timeout", &self.connect_timeout)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub path: PathBuf,
    pub default_server: Option<String>,
    /// Leave joins, parts, quits and netsplits out of the scrollback.
    pub hide_joins_parts: bool,
    /// Leave nick changes out of the scrollback.
    pub hide_nick_changes: bool,
    /// Ring the bell and send a desktop notification on highlights and
    /// private messages (default on).
    pub notify: bool,
    pub servers: BTreeMap<String, ServerConfig>,
}

impl Config {
    /// The named server, or the default when `name` is `None`.
    pub fn resolve_server(&self, name: Option<&str>) -> Option<&ServerConfig> {
        let target = name.or(self.default_server.as_deref())?;
        self.servers.get(target)
    }
}

/// The XDG-aware default path, whether or not it exists.
pub fn default_config_path() -> PathBuf {
    config_path_from(
        std::env::var_os("XDG_CONFIG_HOME"),
        std::env::var_os("HOME"),
    )
}

fn config_path_from(xdg: Option<std::ffi::OsString>, home: Option<std::ffi::OsString>) -> PathBuf {
    let base = match xdg {
        Some(xdg) if !xdg.is_empty() => PathBuf::from(xdg),
        _ => home
            .map_or_else(|| PathBuf::from("."), PathBuf::from)
            .join(".config"),
    };
    base.join("quasseltui").join("config.toml")
}

impl Config {
    /// A warning when the file holds a password but other users can read
    /// it (the help asks for mode 0600).
    pub fn permission_warning(&self) -> Option<String> {
        use std::os::unix::fs::PermissionsExt;
        let has_password = self.servers.values().any(|s| s.password.is_some());
        let mode = std::fs::metadata(&self.path).ok()?.permissions().mode();
        (has_password && mode & 0o077 != 0).then(|| {
            format!(
                "warning: {} holds a password but is readable by other users \
                 (mode {:o}); run chmod 600 on it",
                self.path.display(),
                mode & 0o777
            )
        })
    }
}

/// The file as written.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    default_server: Option<String>,
    hide_joins_parts: Option<bool>,
    hide_nick_changes: Option<bool>,
    notify: Option<bool>,
    #[serde(default)]
    server: BTreeMap<String, RawServer>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawServer {
    host: Option<String>,
    port: Option<i64>,
    user: Option<String>,
    password: Option<String>,
    tls: Option<bool>,
    insecure: Option<bool>,
    cafile: Option<String>,
    connect_timeout: Option<Seconds>,
}

/// TOML keeps integers and floats apart; a timeout may be either.
#[derive(Deserialize)]
#[serde(untagged)]
enum Seconds {
    Int(i64),
    Float(f64),
}

/// Load the config at `path` (or the default path). `Ok(None)` when the
/// file doesn't exist: that's the first-run case, not an error.
pub fn load(path: Option<&Path>) -> Result<Option<Config>, ConfigError> {
    let path = path.map_or_else(default_config_path, Path::to_path_buf);
    let shown = path.display();
    // Only a missing file means "no config": one we can't read (say, a
    // permission problem) is an error, not a silent fallback.
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // The config used to be INI: don't silently ignore an old one.
            let old = path.with_extension("ini");
            if old.exists() {
                return Err(ConfigError(format!(
                    "{} is no longer read; convert it to TOML at {shown} \
                     ([server:NAME] becomes [server.NAME], strings are quoted)",
                    old.display()
                )));
            }
            return Ok(None);
        }
        Err(e) => return Err(ConfigError(format!("{shown}: {e}"))),
    };
    let raw: RawConfig = toml::from_str(&text).map_err(|e| ConfigError(format!("{shown}: {e}")))?;

    let mut servers = BTreeMap::new();
    for (name, server) in raw.server {
        let name = name.trim().to_string();
        if name.is_empty() {
            return Err(ConfigError(format!("{shown}: empty server name")));
        }
        let server = parse_server(&path, &name, server)?;
        servers.insert(name, server);
    }
    let default_server = raw
        .default_server
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty());
    if let Some(default) = &default_server
        && !servers.contains_key(default)
    {
        return Err(ConfigError(format!(
            "{shown}: default_server = '{default}' has no matching [server.{default}] table"
        )));
    }
    Ok(Some(Config {
        path,
        default_server,
        hide_joins_parts: raw.hide_joins_parts.unwrap_or(false),
        hide_nick_changes: raw.hide_nick_changes.unwrap_or(false),
        notify: raw.notify.unwrap_or(true),
        servers,
    }))
}

fn parse_server(path: &Path, name: &str, raw: RawServer) -> Result<ServerConfig, ConfigError> {
    let shown = path.display();
    let field_error =
        |key: &str, msg: String| ConfigError(format!("{shown}: [server.{name}] {key}: {msg}"));
    // Empty means "not set", so the CLI flag or prompt still applies.
    let text = |value: Option<String>| {
        value
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    };
    let port = raw
        .port
        .map(|value| {
            u16::try_from(value)
                .ok()
                .filter(|port| *port != 0)
                .ok_or_else(|| field_error("port", format!("{value} is out of range (1-65535)")))
        })
        .transpose()?;
    let connect_timeout = raw
        .connect_timeout
        .map(|value| {
            // Integers this large lose precision as f64; the range check
            // rejects them all the same.
            #[allow(clippy::cast_precision_loss)]
            let secs = match value {
                Seconds::Int(v) => v as f64,
                Seconds::Float(v) => v,
            };
            check_seconds(secs).map_err(|e| field_error("connect_timeout", e))
        })
        .transpose()?;
    Ok(ServerConfig {
        name: name.to_string(),
        host: text(raw.host),
        port,
        user: text(raw.user),
        // Not trimmed: spaces may be part of the password.
        password: raw.password.filter(|v| !v.is_empty()),
        tls: raw.tls,
        insecure: raw.insecure,
        cafile: text(raw.cafile),
        connect_timeout,
    })
}

fn check_seconds(v: f64) -> Result<f64, String> {
    if v >= 0.0 && std::time::Duration::try_from_secs_f64(v).is_ok() {
        Ok(v)
    } else {
        Err(format!("{v} is not a usable timeout in seconds"))
    }
}

/// A number of seconds that fits a `Duration`: finite, not negative, and
/// not absurdly large (`Duration::from_secs_f64` panics otherwise).
pub fn parse_seconds(raw: &str) -> Result<f64, String> {
    raw.trim()
        .parse::<f64>()
        .ok()
        .filter(|v| *v >= 0.0 && std::time::Duration::try_from_secs_f64(*v).is_ok())
        .ok_or_else(|| format!("could not convert string to a timeout: '{raw}'"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &tempfile::TempDir, body: &str) -> PathBuf {
        let path = dir.path().join("config.toml");
        std::fs::write(&path, body).unwrap();
        path
    }

    fn load_err(body: &str) -> String {
        let dir = tempfile::tempdir().unwrap();
        load(Some(&write(&dir, body))).unwrap_err().0
    }

    fn load_ok(body: &str) -> Config {
        let dir = tempfile::tempdir().unwrap();
        load(Some(&write(&dir, body))).unwrap().unwrap()
    }

    #[test]
    fn debug_output_redacts_the_password() {
        let cfg = load_ok("[server.home]\nhost = \"x\"\npassword = \"hunter2\"\n");
        let shown = format!("{cfg:?}");
        assert!(
            !shown.contains("hunter2") && shown.contains("<redacted>"),
            "{shown}"
        );
    }

    #[test]
    fn readable_password_files_warn() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            &dir,
            "[server.home]\nhost = \"x\"\npassword = \"hunter2\"\n",
        );
        let chmod = |mode| std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode));
        chmod(0o644).unwrap();
        let cfg = load(Some(&path)).unwrap().unwrap();
        let warning = cfg.permission_warning().unwrap();
        assert!(
            warning.contains("644") && !warning.contains("hunter2"),
            "{warning}"
        );
        chmod(0o600).unwrap();
        assert!(cfg.permission_warning().is_none());
        // No password, nothing to protect.
        let cfg = load_ok("[server.home]\nhost = \"x\"\n");
        assert!(cfg.permission_warning().is_none());
    }

    #[test]
    fn unreadable_config_is_an_error_not_missing() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = write(&dir, "[server.home]\nhost = \"x\"\n");
        // No search permission on the directory: the file can't even be
        // stat'ed, which `Path::exists` reported as "no config".
        let locked = std::fs::Permissions::from_mode(0o000);
        std::fs::set_permissions(dir.path(), locked).unwrap();
        let result = load(Some(&path));
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        if std::fs::metadata(&path).is_ok() && result.is_ok() {
            // Running as root: permissions don't apply.
            return;
        }
        let err = result.unwrap_err();
        assert!(err.0.contains("ermission denied"), "{err}");
    }

    #[test]
    fn missing_file_is_none() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            load(Some(&dir.path().join("config.toml")))
                .unwrap()
                .is_none()
        );
        // A leftover INI file is pointed out rather than ignored.
        std::fs::write(dir.path().join("config.ini"), "[server:home]\n").unwrap();
        let err = load(Some(&dir.path().join("config.toml"))).unwrap_err();
        assert!(err.0.contains("config.ini is no longer read"), "{err}");
    }

    #[test]
    fn full_server() {
        let cfg = load_ok(
            "default_server = \"home\"\n\n[server.home]\nhost = \"irc.example.com\"\n\
             port = 4242\nuser = \"sean\"\npassword = \"hunter2\"\ntls = true\ninsecure = false\n\
             cafile = \"/etc/ssl/certs/quassel.pem\"\nconnect_timeout = 15\n",
        );
        assert_eq!(cfg.default_server.as_deref(), Some("home"));
        let home = &cfg.servers["home"];
        assert_eq!(home.host.as_deref(), Some("irc.example.com"));
        assert_eq!(home.port, Some(4242));
        assert_eq!(home.user.as_deref(), Some("sean"));
        assert_eq!(home.password.as_deref(), Some("hunter2"));
        assert_eq!(home.tls, Some(true));
        assert_eq!(home.insecure, Some(false));
        assert_eq!(home.cafile.as_deref(), Some("/etc/ssl/certs/quassel.pem"));
        assert_eq!(home.connect_timeout, Some(15.0));
        let cfg = load_ok("[server.home]\nconnect_timeout = 2.5\n");
        assert_eq!(cfg.servers["home"].connect_timeout, Some(2.5));
    }

    #[test]
    fn multiple_servers_and_resolution() {
        let cfg = load_ok(
            "default_server = \"work\"\n[server.home]\nhost = \"a\"\nport = 4242\n\
             [server.work]\nhost = \"b\"\nport = 4242\nuser = \"alice\"\n",
        );
        assert_eq!(cfg.servers.len(), 2);
        assert_eq!(cfg.resolve_server(None).unwrap().name, "work");
        assert_eq!(cfg.resolve_server(Some("home")).unwrap().name, "home");
        assert!(cfg.resolve_server(Some("nope")).is_none());
        // Quoted names may hold dots.
        let cfg = load_ok("[server.\"irc.example\"]\nhost = \"a\"\n");
        assert!(cfg.servers.contains_key("irc.example"));
    }

    #[test]
    fn global_settings_are_optional() {
        let cfg = load_ok("[server.home]\nhost = \"irc.example.com\"\nport = 4242\n");
        assert!(cfg.default_server.is_none());
        assert!(!cfg.hide_joins_parts && !cfg.hide_nick_changes && cfg.notify);
        assert!(cfg.resolve_server(None).is_none());
        assert!(cfg.resolve_server(Some("home")).is_some());
        assert!(load_ok("").servers.is_empty());
    }

    #[test]
    fn display_options() {
        let cfg = load_ok("hide_joins_parts = true\nhide_nick_changes = true\nnotify = false\n");
        assert!(cfg.hide_joins_parts && cfg.hide_nick_changes && !cfg.notify);
    }

    #[test]
    fn validation_errors() {
        let has = |body: &str, needle: &str| {
            let err = load_err(body);
            assert!(err.contains(needle), "{body:?}: {err}");
        };
        has("[servers.home]\nhost = \"x\"\n", "unknown field `servers`");
        has("[server.\"\"]\nhost = \"x\"\n", "empty server name");
        has(
            "[server.home]\npasswrod = \"oops\"\n",
            "unknown field `passwrod`",
        );
        has(
            "hide_join_parts = true\n",
            "unknown field `hide_join_parts`",
        );
        // A global setting written under a server table lands in it.
        has("[server.home]\nnotify = false\n", "unknown field `notify`");
        has("[server.home]\nport = \"4242\"\n", "port");
        has("[server.home]\nport = 70000\n", "1-65535");
        has("[server.home]\nport = 0\n", "1-65535");
        has("[server.home]\ntls = \"yes\"\n", "tls");
        has("hide_joins_parts = \"maybe\"\n", "hide_joins_parts");
        has(
            "[server.home]\nconnect_timeout = \"soon\"\n",
            "connect_timeout",
        );
        for bad in ["-1", "inf", "nan", "1e30"] {
            has(
                &format!("[server.home]\nconnect_timeout = {bad}\n"),
                "connect_timeout",
            );
        }
        has(
            "default_server = \"ghost\"\n[server.home]\nhost = \"x\"\n",
            "default_server",
        );
        has("[server.a]\nhost = \"x\"\nhost = \"y\"\n", "duplicate key");
        has("host = x\n", "config.toml");
    }

    #[test]
    fn empty_strings_are_unset() {
        let cfg = load_ok("[server.home]\nhost = \" \"\npassword = \"\"\n");
        assert!(cfg.servers["home"].host.is_none());
        assert!(cfg.servers["home"].password.is_none());
        let cfg = load_ok("[server.home]\npassword = \" sup%r s3cret \"\n");
        assert_eq!(
            cfg.servers["home"].password.as_deref(),
            Some(" sup%r s3cret ")
        );
    }

    #[test]
    fn default_path_honors_xdg() {
        assert_eq!(
            config_path_from(Some("/x/cfg".into()), Some("/home/u".into())),
            PathBuf::from("/x/cfg/quasseltui/config.toml")
        );
        assert_eq!(
            config_path_from(None, Some("/home/u".into())),
            PathBuf::from("/home/u/.config/quasseltui/config.toml")
        );
        assert_eq!(
            config_path_from(Some("".into()), Some("/home/u".into())),
            PathBuf::from("/home/u/.config/quasseltui/config.toml")
        );
    }
}
