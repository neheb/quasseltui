//! Typed handshake messages.
//!
//! ```text
//! Client -> ClientInit
//! Core   -> ClientInitAck | ClientInitReject
//! Client -> ClientLogin
//! Core   -> ClientLoginAck | ClientLoginReject
//! Core   -> SessionInit
//! ```
//!
//! Each is a flat key/value map on the wire. Parsing validates every field
//! type so a broken or hostile peer surfaces as `Error::Handshake` instead
//! of a confusing failure later.

use crate::protocol::error::{Error, Result};
use crate::protocol::types::{BufferInfo, NetworkId};
use crate::protocol::usertypes::UserValue;
use crate::qt::variant::{Variant, VariantMap};

pub const CLIENT_INIT: &str = "ClientInit";
pub const CLIENT_INIT_ACK: &str = "ClientInitAck";
pub const CLIENT_INIT_REJECT: &str = "ClientInitReject";
pub const CLIENT_LOGIN: &str = "ClientLogin";
pub const CLIENT_LOGIN_ACK: &str = "ClientLoginAck";
pub const CLIENT_LOGIN_REJECT: &str = "ClientLoginReject";
pub const SESSION_INIT: &str = "SessionInit";
pub const CORE_SETUP_REJECT: &str = "CoreSetupReject";

fn handshake(msg: String) -> Error {
    Error::Handshake(msg)
}

fn missing(key: &str) -> Error {
    handshake(format!("handshake message missing required field '{key}'"))
}

fn wrong_type(key: &str, expected: &str, got: &Variant) -> Error {
    handshake(format!(
        "handshake field '{key}' expected {expected}, got {}",
        got.type_name()
    ))
}

fn require_int(data: &VariantMap, key: &str) -> Result<i64> {
    let value = data.get(key).ok_or_else(|| missing(key))?;
    value.as_i64().ok_or_else(|| wrong_type(key, "int", value))
}

fn require_bool(data: &VariantMap, key: &str) -> Result<bool> {
    match data.get(key) {
        None => Err(missing(key)),
        Some(Variant::Bool(b)) => Ok(*b),
        Some(other) => Err(wrong_type(key, "bool", other)),
    }
}

fn optional_str(data: &VariantMap, key: &str) -> Result<String> {
    match data.get(key) {
        None | Some(Variant::Null) => Ok(String::new()),
        Some(Variant::String(s)) => Ok(s.clone()),
        Some(other) => Err(wrong_type(key, "str", other)),
    }
}

fn optional_int(data: &VariantMap, key: &str) -> Result<Option<i64>> {
    match data.get(key) {
        None | Some(Variant::Null) => Ok(None),
        Some(value) => value
            .as_i64()
            .map(Some)
            .ok_or_else(|| wrong_type(key, "int", value)),
    }
}

fn optional_map(data: &VariantMap, key: &str) -> Result<VariantMap> {
    match data.get(key) {
        None | Some(Variant::Null) => Ok(VariantMap::new()),
        Some(Variant::Map(m)) => Ok(m.clone()),
        Some(other) => Err(wrong_type(key, "dict", other)),
    }
}

fn optional_str_list(data: &VariantMap, key: &str) -> Result<Vec<String>> {
    match data.get(key) {
        None | Some(Variant::Null) => Ok(Vec::new()),
        Some(Variant::StringList(items)) => Ok(items.clone()),
        Some(Variant::List(items)) => items
            .iter()
            .enumerate()
            .map(|(i, item)| match item {
                Variant::String(s) => Ok(s.clone()),
                other => Err(handshake(format!(
                    "handshake field '{key}'[{i}] expected str, got {}",
                    other.type_name()
                ))),
            })
            .collect(),
        Some(other) => Err(wrong_type(key, "list", other)),
    }
}

/// A list of maps; non-map entries are skipped for forward compatibility.
fn optional_map_list(data: &VariantMap, key: &str) -> Result<Vec<VariantMap>> {
    match data.get(key) {
        None | Some(Variant::Null) => Ok(Vec::new()),
        Some(Variant::List(items)) => Ok(items
            .iter()
            .filter_map(|item| item.as_map().cloned())
            .collect()),
        Some(other) => Err(wrong_type(key, "list", other)),
    }
}

/// The first framed message a client sends.
///
/// `features` is the legacy bitmask that pre-modern cores look at;
/// `feature_list` is the modern string list. We send both.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientInit {
    pub client_version: String,
    pub build_date: String,
    pub features: u32,
    pub feature_list: Vec<String>,
}

impl ClientInit {
    pub fn new(client_version: impl Into<String>, build_date: impl Into<String>) -> Self {
        Self {
            client_version: client_version.into(),
            build_date: build_date.into(),
            features: 0,
            feature_list: Vec::new(),
        }
    }

    /// The wire fields. `Features` must be a `UInt` (the high bit of the
    /// legacy mask would mis-encode as a signed Int) and `FeatureList` a
    /// `QStringList` (not a QVariantList of strings).
    pub fn to_map(&self) -> VariantMap {
        let mut map = VariantMap::new();
        map.insert("MsgType".into(), CLIENT_INIT.into());
        map.insert("ClientVersion".into(), self.client_version.clone().into());
        map.insert("ClientDate".into(), self.build_date.clone().into());
        map.insert("Features".into(), Variant::UInt(self.features));
        map.insert(
            "FeatureList".into(),
            Variant::StringList(self.feature_list.clone()),
        );
        map
    }
}

/// One entry of `StorageBackends` in `ClientInitAck`.
#[derive(Debug, Clone, PartialEq)]
pub struct StorageBackendInfo {
    pub display_name: String,
    pub description: String,
    pub setup_keys: Vec<String>,
    pub setup_defaults: VariantMap,
}

impl StorageBackendInfo {
    fn from_map(data: &VariantMap) -> Result<Self> {
        Ok(Self {
            display_name: optional_str(data, "DisplayName")?,
            description: optional_str(data, "Description")?,
            setup_keys: optional_str_list(data, "SetupKeys")?,
            setup_defaults: optional_map(data, "SetupDefaults")?,
        })
    }
}

/// One entry of `Authenticators` in `ClientInitAck` (absent on old cores).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthenticatorInfo {
    pub display_name: String,
    pub description: String,
}

impl AuthenticatorInfo {
    fn from_map(data: &VariantMap) -> Result<Self> {
        Ok(Self {
            display_name: optional_str(data, "DisplayName")?,
            description: optional_str(data, "Description")?,
        })
    }
}

/// The core accepted our `ClientInit`.
///
/// `configured == false` means the core still wants its setup wizard,
/// which this client does not implement.
#[derive(Debug, Clone, PartialEq)]
pub struct ClientInitAck {
    pub core_features: u32,
    pub feature_list: Vec<String>,
    pub configured: bool,
    pub storage_backends: Vec<StorageBackendInfo>,
    pub authenticators: Vec<AuthenticatorInfo>,
    pub protocol_version: Option<i64>,
}

impl ClientInitAck {
    pub fn from_map(data: &VariantMap) -> Result<Self> {
        Ok(Self {
            core_features: require_int(data, "CoreFeatures")? as u32,
            feature_list: optional_str_list(data, "FeatureList")?,
            configured: require_bool(data, "Configured")?,
            storage_backends: optional_map_list(data, "StorageBackends")?
                .iter()
                .map(StorageBackendInfo::from_map)
                .collect::<Result<_>>()?,
            authenticators: optional_map_list(data, "Authenticators")?
                .iter()
                .map(AuthenticatorInfo::from_map)
                .collect::<Result<_>>()?,
            protocol_version: optional_int(data, "ProtocolVersion")?,
        })
    }
}

/// Outbound credentials. The core dispatches to whatever authenticator it
/// is configured with; the client side is always `User`/`Password`.
#[derive(Clone, PartialEq, Eq)]
pub struct ClientLogin {
    pub user: String,
    pub password: String,
}

impl std::fmt::Debug for ClientLogin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientLogin")
            .field("user", &self.user)
            .field("password", &"<redacted>")
            .finish()
    }
}

impl ClientLogin {
    pub fn to_map(&self) -> VariantMap {
        let mut map = VariantMap::new();
        map.insert("MsgType".into(), CLIENT_LOGIN.into());
        map.insert("User".into(), self.user.clone().into());
        map.insert("Password".into(), self.password.clone().into());
        map
    }
}

/// The first message after login: everything the session contains.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SessionInit {
    /// Raw identity maps; the sync layer models the fields.
    pub identities: Vec<VariantMap>,
    pub network_ids: Vec<NetworkId>,
    pub buffer_infos: Vec<BufferInfo>,
}

impl SessionInit {
    pub fn from_map(data: &VariantMap) -> Result<Self> {
        let state = match data.get("SessionState") {
            None => {
                return Err(handshake(
                    "SessionInit message missing required field 'SessionState'".into(),
                ));
            }
            Some(Variant::Map(m)) => m,
            Some(other) => {
                return Err(handshake(format!(
                    "SessionInit field 'SessionState' expected dict, got {}",
                    other.type_name()
                )));
            }
        };
        let identities = session_list(state, "Identities", "dict", |item| match item {
            Variant::Map(m) | Variant::User(UserValue::Identity(m)) => Some(m.clone()),
            _ => None,
        })?;
        let network_ids = session_list(state, "NetworkIds", "NetworkId", |item| match item {
            Variant::User(UserValue::NetworkId(id)) => Some(*id),
            _ => None,
        })?;
        let buffer_infos = session_list(state, "BufferInfos", "BufferInfo", |item| match item {
            Variant::User(UserValue::BufferInfo(info)) => Some(info.clone()),
            _ => None,
        })?;
        Ok(Self {
            identities,
            network_ids,
            buffer_infos,
        })
    }
}

/// A typed list inside `SessionState`. Every element must match: a
/// different shape means the core sends something we don't understand, and
/// failing visibly beats silently dropping networks or buffers.
fn session_list<T>(
    state: &VariantMap,
    key: &str,
    expected: &str,
    convert: impl Fn(&Variant) -> Option<T>,
) -> Result<Vec<T>> {
    let items = match state.get(key) {
        None | Some(Variant::Null) => return Ok(Vec::new()),
        Some(Variant::List(items)) => items,
        Some(other) => {
            return Err(handshake(format!(
                "SessionState field '{key}' expected list, got {}",
                other.type_name()
            )));
        }
    };
    items
        .iter()
        .enumerate()
        .map(|(i, item)| {
            convert(item).ok_or_else(|| {
                handshake(format!(
                    "SessionState field '{key}'[{i}] expected {expected}, got {}",
                    item.type_name()
                ))
            })
        })
        .collect()
}

/// A decoded inbound handshake message.
///
/// `ClientLoginReject` is not a variant: it is surfaced as `Error::Auth` so
/// no caller can forget to handle a credentials failure.
#[derive(Debug, Clone, PartialEq)]
pub enum HandshakeMessage {
    ClientInitAck(ClientInitAck),
    ClientInitReject { error: String },
    ClientLoginAck,
    SessionInit(SessionInit),
    CoreSetupReject { error: String },
}

impl HandshakeMessage {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::ClientInitAck(_) => CLIENT_INIT_ACK,
            Self::ClientInitReject { .. } => CLIENT_INIT_REJECT,
            Self::ClientLoginAck => CLIENT_LOGIN_ACK,
            Self::SessionInit(_) => SESSION_INIT,
            Self::CoreSetupReject { .. } => CORE_SETUP_REJECT,
        }
    }
}

/// Dispatch a decoded handshake map by its `MsgType`.
pub fn parse_handshake_message(data: &VariantMap) -> Result<HandshakeMessage> {
    let Some(msg_type) = data.get("MsgType") else {
        return Err(handshake("handshake message has no MsgType field".into()));
    };
    match msg_type.as_str() {
        Some(CLIENT_INIT_ACK) => Ok(HandshakeMessage::ClientInitAck(ClientInitAck::from_map(
            data,
        )?)),
        Some(CLIENT_INIT_REJECT) => Ok(HandshakeMessage::ClientInitReject {
            error: optional_str(data, "Error")?,
        }),
        Some(CLIENT_LOGIN_ACK) => Ok(HandshakeMessage::ClientLoginAck),
        Some(CLIENT_LOGIN_REJECT) => {
            let error = optional_str(data, "Error")?;
            Err(Error::Auth(if error.is_empty() {
                "core rejected credentials".into()
            } else {
                error
            }))
        }
        Some(SESSION_INIT) => Ok(HandshakeMessage::SessionInit(SessionInit::from_map(data)?)),
        Some(CORE_SETUP_REJECT) => Ok(HandshakeMessage::CoreSetupReject {
            error: optional_str(data, "Error")?,
        }),
        _ => Err(handshake(format!(
            "unknown handshake MsgType {:?}",
            msg_type
                .coerce_string()
                .unwrap_or_else(|| msg_type.type_name().to_string())
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::types::{BufferId, BufferType};

    fn map(entries: Vec<(&str, Variant)>) -> VariantMap {
        entries
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect()
    }

    fn ack(overrides: Vec<(&str, Variant)>) -> VariantMap {
        let mut base = map(vec![
            ("MsgType", "ClientInitAck".into()),
            ("CoreFeatures", Variant::UInt(0)),
            ("FeatureList", Variant::StringList(vec![])),
            ("Configured", Variant::Bool(true)),
        ]);
        for (k, v) in overrides {
            base.insert(k.to_string(), v);
        }
        base
    }

    fn handshake_err(data: &VariantMap) -> String {
        match parse_handshake_message(data) {
            Err(Error::Handshake(msg)) => msg,
            other => panic!("expected handshake error, got {other:?}"),
        }
    }

    #[test]
    fn minimal_ack() {
        let parsed = ClientInitAck::from_map(&ack(vec![
            ("CoreFeatures", Variant::UInt(0xC03F)),
            (
                "FeatureList",
                Variant::StringList(vec!["SynchronizedMarkerLine".into()]),
            ),
            ("ProtocolVersion", Variant::Int(10)),
        ]))
        .unwrap();
        assert_eq!(parsed.core_features, 0xC03F);
        assert_eq!(parsed.feature_list, ["SynchronizedMarkerLine"]);
        assert!(parsed.configured);
        assert_eq!(parsed.protocol_version, Some(10));
        assert!(parsed.storage_backends.is_empty());
        assert!(parsed.authenticators.is_empty());
    }

    #[test]
    fn unconfigured_core_with_backends() {
        let parsed = ClientInitAck::from_map(&ack(vec![
            ("Configured", Variant::Bool(false)),
            (
                "StorageBackends",
                Variant::List(vec![Variant::Map(map(vec![
                    ("DisplayName", "SQLite".into()),
                    ("Description", "Default file-backed storage".into()),
                    ("SetupKeys", Variant::StringList(vec!["Database".into()])),
                    (
                        "SetupDefaults",
                        Variant::Map(map(vec![("Database", "quassel-storage.sqlite".into())])),
                    ),
                ]))]),
            ),
            (
                "Authenticators",
                Variant::List(vec![Variant::Map(map(vec![
                    ("DisplayName", "Database".into()),
                    ("Description", "Use the storage DB".into()),
                ]))]),
            ),
        ]))
        .unwrap();
        assert!(!parsed.configured);
        assert_eq!(parsed.storage_backends[0].display_name, "SQLite");
        assert_eq!(parsed.storage_backends[0].setup_keys, ["Database"]);
        assert_eq!(
            parsed.storage_backends[0].setup_defaults["Database"],
            Variant::String("quassel-storage.sqlite".into())
        );
        assert_eq!(parsed.authenticators[0].display_name, "Database");
    }

    #[test]
    fn strict_ack_validation() {
        let err = ClientInitAck::from_map(&map(vec![
            ("MsgType", "ClientInitAck".into()),
            ("Configured", Variant::Bool(true)),
        ]))
        .unwrap_err();
        assert!(
            err.to_string()
                .contains("missing required field 'CoreFeatures'")
        );

        for (overrides, needle) in [
            (
                vec![("CoreFeatures", Variant::from("not-an-int"))],
                "'CoreFeatures' expected int",
            ),
            (
                vec![("CoreFeatures", Variant::Bool(true))],
                "'CoreFeatures' expected int",
            ),
            (
                vec![("Configured", Variant::Int(1))],
                "'Configured' expected bool",
            ),
            (
                vec![(
                    "FeatureList",
                    Variant::List(vec!["ok".into(), Variant::Int(42)]),
                )],
                "'FeatureList'[1] expected str",
            ),
            (
                vec![("ProtocolVersion", Variant::from("ten"))],
                "'ProtocolVersion' expected int",
            ),
            (
                vec![("StorageBackends", Variant::from("oops"))],
                "'StorageBackends' expected list",
            ),
        ] {
            let msg = handshake_err(&ack(overrides));
            assert!(msg.contains(needle), "{msg} missing {needle}");
        }
        assert_eq!(
            ClientInitAck::from_map(&ack(vec![]))
                .unwrap()
                .protocol_version,
            None
        );
    }

    #[test]
    fn dispatch() {
        let reject = parse_handshake_message(&map(vec![
            ("MsgType", "ClientInitReject".into()),
            ("Error", "core too old".into()),
        ]))
        .unwrap();
        assert_eq!(
            reject,
            HandshakeMessage::ClientInitReject {
                error: "core too old".into()
            }
        );
        assert!(
            handshake_err(&map(vec![("MsgType", "Mystery".into())]))
                .contains("unknown handshake MsgType")
        );
        assert!(handshake_err(&VariantMap::new()).contains("no MsgType"));
        assert_eq!(
            parse_handshake_message(&map(vec![("MsgType", "ClientLoginAck".into())])).unwrap(),
            HandshakeMessage::ClientLoginAck
        );
        assert_eq!(
            parse_handshake_message(&map(vec![
                ("MsgType", "CoreSetupReject".into()),
                ("Error", "missing field".into()),
            ]))
            .unwrap(),
            HandshakeMessage::CoreSetupReject {
                error: "missing field".into()
            }
        );
    }

    #[test]
    fn login_reject_is_auth_error() {
        let err = parse_handshake_message(&map(vec![
            ("MsgType", "ClientLoginReject".into()),
            ("Error", "bad password".into()),
        ]))
        .unwrap_err();
        assert!(err.is_auth());
        assert_eq!(err.to_string(), "bad password");

        let err = parse_handshake_message(&map(vec![("MsgType", "ClientLoginReject".into())]))
            .unwrap_err();
        assert!(err.is_auth());
        assert_eq!(err.to_string(), "core rejected credentials");
    }

    fn session(state: Variant) -> VariantMap {
        map(vec![
            ("MsgType", "SessionInit".into()),
            ("SessionState", state),
        ])
    }

    #[test]
    fn session_init_parsing() {
        let empty = parse_handshake_message(&session(Variant::Map(map(vec![
            ("Identities", Variant::List(vec![])),
            ("NetworkIds", Variant::List(vec![])),
            ("BufferInfos", Variant::List(vec![])),
        ]))))
        .unwrap();
        assert_eq!(empty, HandshakeMessage::SessionInit(SessionInit::default()));

        let info = BufferInfo {
            buffer_id: BufferId(10),
            network_id: NetworkId(1),
            kind: BufferType::Channel,
            group_id: 0,
            name: "#python".into(),
        };
        let full = parse_handshake_message(&session(Variant::Map(map(vec![
            (
                "Identities",
                Variant::List(vec![
                    Variant::Map(map(vec![("identityName", "default".into())])),
                    Variant::User(UserValue::Identity(map(vec![(
                        "identityName",
                        "alt".into(),
                    )]))),
                ]),
            ),
            (
                "NetworkIds",
                Variant::List(vec![
                    Variant::User(UserValue::NetworkId(NetworkId(1))),
                    Variant::User(UserValue::NetworkId(NetworkId(2))),
                ]),
            ),
            (
                "BufferInfos",
                Variant::List(vec![Variant::User(UserValue::BufferInfo(info.clone()))]),
            ),
        ]))))
        .unwrap();
        let HandshakeMessage::SessionInit(s) = full else {
            panic!("expected SessionInit");
        };
        assert_eq!(s.identities.len(), 2);
        assert_eq!(s.network_ids, [NetworkId(1), NetworkId(2)]);
        assert_eq!(s.buffer_infos, [info]);
    }

    #[test]
    fn session_init_validation() {
        assert!(
            handshake_err(&map(vec![("MsgType", "SessionInit".into())]))
                .contains("missing required field 'SessionState'")
        );
        assert!(handshake_err(&session("oops".into())).contains("'SessionState' expected dict"));
        let bad = |key: &str, item: Variant| {
            handshake_err(&session(Variant::Map(map(vec![(
                key,
                Variant::List(vec![item]),
            )]))))
        };
        assert!(bad("Identities", "not a dict".into()).contains("'Identities'[0] expected dict"));
        assert!(bad("NetworkIds", Variant::Int(42)).contains("'NetworkIds'[0] expected NetworkId"));
        assert!(
            bad("BufferInfos", Variant::Map(VariantMap::new()))
                .contains("'BufferInfos'[0] expected BufferInfo")
        );
    }

    #[test]
    fn client_login_debug_redacts_password() {
        let login = ClientLogin {
            user: "sean".into(),
            password: "hunter2".into(),
        };
        assert!(!format!("{login:?}").contains("hunter2"));
    }
}
