//! The user config file.
//!
//! Location: `$XDG_CONFIG_HOME/quasseltui/config.ini`, or
//! `~/.config/quasseltui/config.ini`.
//!
//! ```ini
//! [quasseltui]
//! default_server = home
//!
//! [server:home]
//! host = irc.example.com
//! port = 4242
//! user = sean
//! password = hunter2
//! # tls = true            (default)
//! # insecure = false      (skip TLS cert verification; self-signed cores)
//! # cafile = /etc/ssl/certs/quassel.pem
//! # connect_timeout = 10
//! ```
//!
//! Every setting is optional and falls back to the CLI flag (and then, for
//! the user and password, to `QUASSEL_USER`/`QUASSEL_PASSWORD` or a
//! prompt). Unknown sections and keys are errors so typos surface. Values
//! are literal: `%` has no special meaning. The format follows Python's
//! `configparser` (the original implementation), so existing files keep
//! working: `=` or `:` delimiters, `#`/`;` comment lines, case-insensitive
//! keys, indented continuation lines, and a `[DEFAULT]` section.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const MAIN_SECTION: &str = "quasseltui";
const SERVER_PREFIX: &str = "server:";
const ALLOWED_SERVER_KEYS: [&str; 8] = [
    "cafile",
    "connect_timeout",
    "host",
    "insecure",
    "password",
    "port",
    "tls",
    "user",
];

/// The file exists but can't be read, parsed, or validated.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct ConfigError(pub String);

/// One `[server:NAME]` section. `None` means "not set here".
#[derive(Debug, Clone, PartialEq, Default)]
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

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub path: PathBuf,
    pub default_server: Option<String>,
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
    base.join("quasseltui").join("config.ini")
}

/// Load the config at `path` (or the default path). `Ok(None)` when the
/// file doesn't exist: that's the first-run case, not an error.
pub fn load(path: Option<&Path>) -> Result<Option<Config>, ConfigError> {
    let path = path.map_or_else(default_config_path, Path::to_path_buf);
    if !path.exists() {
        return Ok(None);
    }
    let shown = path.display();
    let text = std::fs::read_to_string(&path).map_err(|e| ConfigError(format!("{shown}: {e}")))?;
    let sections = parse_ini(&text).map_err(|e| ConfigError(format!("{shown}: {e}")))?;

    let defaults = sections.get("DEFAULT").cloned().unwrap_or_default();
    let with_defaults = |values: &BTreeMap<String, String>| {
        let mut merged = defaults.clone();
        merged.extend(values.iter().map(|(k, v)| (k.clone(), v.clone())));
        merged
    };

    let mut default_server = None;
    let mut servers = BTreeMap::new();
    for (section, values) in &sections {
        if section == "DEFAULT" {
            continue;
        }
        if section == MAIN_SECTION {
            let value = with_defaults(values)
                .get("default_server")
                .map(|v| v.trim().to_string())
                .unwrap_or_default();
            if !value.is_empty() {
                default_server = Some(value);
            }
            continue;
        }
        let Some(rest) = section.strip_prefix(SERVER_PREFIX) else {
            return Err(ConfigError(format!(
                "{shown}: unknown section [{section}] (expected [quasseltui] or [server:NAME])"
            )));
        };
        let name = rest.trim();
        if name.is_empty() {
            return Err(ConfigError(format!(
                "{shown}: empty server name in section [{section}]"
            )));
        }
        let server = parse_server(&path, name, &with_defaults(values))?;
        servers.insert(name.to_string(), server);
    }

    if let Some(default) = &default_server
        && !servers.contains_key(default)
    {
        return Err(ConfigError(format!(
            "{shown}: default_server = '{default}' has no matching [server:{default}] section"
        )));
    }
    Ok(Some(Config {
        path,
        default_server,
        servers,
    }))
}

fn parse_server(
    path: &Path,
    name: &str,
    values: &BTreeMap<String, String>,
) -> Result<ServerConfig, ConfigError> {
    let shown = path.display();
    for key in values.keys() {
        if !ALLOWED_SERVER_KEYS.contains(&key.as_str()) {
            return Err(ConfigError(format!(
                "{shown}: [server:{name}] unknown setting '{key}' (allowed: {})",
                ALLOWED_SERVER_KEYS.join(", ")
            )));
        }
    }
    let field_error =
        |key: &str, msg: String| ConfigError(format!("{shown}: [server:{name}] {key}: {msg}"));
    let text = |key: &str| {
        values
            .get(key)
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    };
    let boolean = |key: &str| -> Result<Option<bool>, ConfigError> {
        let Some(raw) = text(key) else {
            return Ok(None);
        };
        match raw.to_lowercase().as_str() {
            "1" | "yes" | "true" | "on" => Ok(Some(true)),
            "0" | "no" | "false" | "off" => Ok(Some(false)),
            _ => Err(field_error(key, format!("Not a boolean: {raw}"))),
        }
    };
    let port =
        match text("port") {
            None => None,
            Some(raw) => {
                let value: i64 = raw.parse().map_err(|_| {
                    field_error(
                        "port",
                        format!("invalid literal for int() with base 10: '{raw}'"),
                    )
                })?;
                Some(u16::try_from(value).map_err(|_| {
                    field_error("port", format!("{value} is out of range (0-65535)"))
                })?)
            }
        };
    let connect_timeout = match text("connect_timeout") {
        None => None,
        Some(raw) => Some(
            raw.parse::<f64>()
                .ok()
                .filter(|v| v.is_finite() && *v >= 0.0)
                .ok_or_else(|| {
                    field_error(
                        "connect_timeout",
                        format!("could not convert string to a timeout: '{raw}'"),
                    )
                })?,
        ),
    };
    // Passwords aren't trimmed beyond what the INI format already does
    // (INI can't represent surrounding spaces anyway). Empty means "not
    // set", so the interactive prompt still fires.
    let password = values.get("password").filter(|v| !v.is_empty()).cloned();
    Ok(ServerConfig {
        name: name.to_string(),
        host: text("host"),
        port,
        user: text("user"),
        password,
        tls: boolean("tls")?,
        insecure: boolean("insecure")?,
        cafile: text("cafile"),
        connect_timeout,
    })
}

/// Parse INI text into section -> key -> value, configparser-style.
fn parse_ini(text: &str) -> Result<BTreeMap<String, BTreeMap<String, String>>, String> {
    let mut sections: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    let mut current: Option<String> = None;
    let mut last_key: Option<String> = None;
    for (index, raw_line) in text.lines().enumerate() {
        let lineno = index + 1;
        let trimmed = raw_line.trim();
        if trimmed.starts_with('#') || trimmed.starts_with(';') {
            continue;
        }
        if trimmed.is_empty() {
            last_key = None;
            continue;
        }
        let indented = raw_line.starts_with([' ', '\t']);
        if indented && let (Some(section), Some(key)) = (&current, &last_key) {
            let value = sections
                .get_mut(section)
                .and_then(|s| s.get_mut(key))
                .expect("continuation of an existing key");
            value.push('\n');
            value.push_str(trimmed);
            continue;
        }
        if let Some(inner) = trimmed.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            let name = inner.to_string();
            if sections.contains_key(&name) {
                return Err(format!(
                    "While reading from config [line {lineno:2}]: section '{name}' already exists"
                ));
            }
            sections.insert(name.clone(), BTreeMap::new());
            current = Some(name);
            last_key = None;
            continue;
        }
        let Some(section) = &current else {
            return Err(format!(
                "File contains no section headers.\nfile: config, line: {lineno}\n{raw_line:?}"
            ));
        };
        let Some(split) = trimmed.find(['=', ':']) else {
            return Err(format!(
                "Source contains parsing errors: [line {lineno:2}]: {raw_line:?}"
            ));
        };
        let key = trimmed[..split].trim().to_lowercase();
        let value = trimmed[split + 1..].trim().to_string();
        let entries = sections.get_mut(section).expect("current section exists");
        if entries.contains_key(&key) {
            return Err(format!(
                "While reading from config [line {lineno:2}]: option '{key}' in section '{section}' already exists"
            ));
        }
        entries.insert(key.clone(), value);
        last_key = Some(key);
    }
    Ok(sections)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &tempfile::TempDir, body: &str) -> PathBuf {
        let path = dir.path().join("config.ini");
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
    fn missing_file_is_none() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            load(Some(&dir.path().join("config.ini")))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn full_server() {
        let cfg = load_ok(
            "[quasseltui]\ndefault_server = home\n\n[server:home]\nhost = irc.example.com\n\
             port = 4242\nuser = sean\npassword = hunter2\ntls = true\ninsecure = false\n\
             cafile = /etc/ssl/certs/quassel.pem\nconnect_timeout = 15\n",
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
    }

    #[test]
    fn multiple_servers_and_resolution() {
        let cfg = load_ok(
            "[quasseltui]\ndefault_server = work\n[server:home]\nhost = a\nport = 4242\n\
             [server:work]\nhost = b\nport = 4242\nuser = alice\n",
        );
        assert_eq!(cfg.servers.len(), 2);
        assert_eq!(cfg.resolve_server(None).unwrap().name, "work");
        assert_eq!(cfg.resolve_server(Some("home")).unwrap().name, "home");
        assert!(cfg.resolve_server(Some("nope")).is_none());
    }

    #[test]
    fn main_section_is_optional() {
        let cfg = load_ok("[server:home]\nhost = irc.example.com\nport = 4242\n");
        assert!(cfg.default_server.is_none());
        assert!(cfg.resolve_server(None).is_none());
        assert!(cfg.resolve_server(Some("home")).is_some());
    }

    #[test]
    fn validation_errors() {
        assert!(load_err("[servers:home]\nhost = x\n").contains("unknown section"));
        assert!(load_err("[server:]\nhost = x\n").contains("empty server name"));
        assert!(load_err("[server:home]\nhost = x\npasswrod = oops\n").contains("unknown setting"));
        assert!(load_err("[server:home]\nport = notanumber\n").contains("port"));
        assert!(load_err("[server:home]\nport = 70000\n").contains("port"));
        assert!(load_err("[server:home]\ntls = maybe\n").contains("tls"));
        assert!(load_err("[server:home]\nconnect_timeout = soon\n").contains("connect_timeout"));
        assert!(
            load_err("[quasseltui]\ndefault_server = ghost\n[server:home]\nhost = x\n")
                .contains("default_server")
        );
        assert!(load_err("host = x\n").contains("no section headers"));
        assert!(load_err("[server:a]\nhost\n").contains("parsing errors"));
        assert!(load_err("[server:a]\n[server:a]\n").contains("already exists"));
        assert!(load_err("[server:a]\nhost = x\nhost = y\n").contains("already exists"));
    }

    #[test]
    fn empty_password_is_unset_and_percent_is_literal() {
        let cfg = load_ok("[server:home]\nhost = x\npassword =\n");
        assert!(cfg.servers["home"].password.is_none());
        let cfg = load_ok("[server:home]\npassword = sup%r,s3cret%\n");
        assert_eq!(
            cfg.servers["home"].password.as_deref(),
            Some("sup%r,s3cret%")
        );
    }

    #[test]
    fn configparser_syntax() {
        let cfg = load_ok(
            "# comment\n; another\n[DEFAULT]\nport = 4242\n\n[server:home]\nHOST: irc.example.com\n\
             TLS = Off\n  # indented comment\n",
        );
        let home = &cfg.servers["home"];
        assert_eq!(home.host.as_deref(), Some("irc.example.com"));
        assert_eq!(home.tls, Some(false));
        assert_eq!(home.port, Some(4242));
        let cfg = load_ok("[server:home]\npassword = a=b:c\n");
        assert_eq!(cfg.servers["home"].password.as_deref(), Some("a=b:c"));
    }

    #[test]
    fn default_path_honors_xdg() {
        assert_eq!(
            config_path_from(Some("/x/cfg".into()), Some("/home/u".into())),
            PathBuf::from("/x/cfg/quasseltui/config.ini")
        );
        assert_eq!(
            config_path_from(None, Some("/home/u".into())),
            PathBuf::from("/home/u/.config/quasseltui/config.ini")
        );
        assert_eq!(
            config_path_from(Some("".into()), Some("/home/u".into())),
            PathBuf::from("/home/u/.config/quasseltui/config.ini")
        );
    }
}
