//! `Network`: one configured IRC network. Its object name is the network id.
//!
//! The `IrcUsersAndChannels` init field carries the whole roster. The
//! network can't create `IrcUser`/`IrcChannel` objects itself (the
//! dispatcher owns the registry and knows the object-name conventions), so
//! it normalizes the roster into per-name field maps that the dispatcher
//! then expands.

use std::collections::{BTreeMap, BTreeSet};

use crate::qt::variant::{Variant, VariantMap};
use crate::sync::{exact_params, log_unknown_field, log_unknown_slot, nick_of, string_or_empty};

/// Mirror of `Network::ConnectionState`. Unknown values degrade to
/// `Disconnected`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NetworkConnectionState {
    #[default]
    Disconnected,
    Connecting,
    Initializing,
    Initialized,
    Reconnecting,
    Disconnecting,
}

impl NetworkConnectionState {
    pub fn from_wire(value: &Variant) -> Self {
        match value.coerce_i64() {
            Some(1) => Self::Connecting,
            Some(2) => Self::Initializing,
            Some(3) => Self::Initialized,
            Some(4) => Self::Reconnecting,
            Some(5) => Self::Disconnecting,
            _ => Self::Disconnected,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Disconnected => "Disconnected",
            Self::Connecting => "Connecting",
            Self::Initializing => "Initializing",
            Self::Initialized => "Initialized",
            Self::Reconnecting => "Reconnecting",
            Self::Disconnecting => "Disconnecting",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Network {
    pub object_name: String,
    pub initialized: bool,
    pub network_name: String,
    pub current_server: String,
    pub my_nick: String,
    pub latency: i64,
    pub connection_state: NetworkConnectionState,
    pub is_connected: bool,
    /// Per-channel field maps from `IrcUsersAndChannels`, keyed by channel
    /// name (no network prefix).
    pub channels_seed: BTreeMap<String, VariantMap>,
    /// Per-user field maps, keyed by nick.
    pub users_seed: BTreeMap<String, VariantMap>,
    /// Channel names on this network, maintained by slots and the seed.
    pub channels: BTreeSet<String>,
    /// Nicks on this network.
    pub users: BTreeSet<String>,
}

pub const CLASS_NAME: &[u8] = b"Network";

impl Network {
    pub fn new(object_name: impl Into<String>) -> Self {
        Self {
            object_name: object_name.into(),
            ..Self::default()
        }
    }

    /// The id parsed from the object name, or -1.
    pub fn network_id(&self) -> i32 {
        self.object_name.parse().unwrap_or(-1)
    }

    pub fn handle_sync(&mut self, slot: &[u8], params: &[Variant]) {
        let name = self.object_name.clone();
        let one = || exact_params::<1>("Network", &name, slot, params);
        match slot {
            b"setNetworkName" => {
                if let Some([v]) = one() {
                    self.network_name = string_or_empty(v);
                }
            }
            b"setCurrentServer" => {
                if let Some([v]) = one() {
                    self.current_server = string_or_empty(v);
                }
            }
            b"setMyNick" => {
                if let Some([v]) = one() {
                    self.my_nick = string_or_empty(v);
                }
            }
            b"setLatency" => {
                if let Some([v]) = one() {
                    match v.coerce_i64() {
                        Some(latency) => self.latency = latency,
                        None => tracing::warn!("Network({name:?}): bad setLatency value"),
                    }
                }
            }
            b"setConnected" => {
                if let Some([v]) = one() {
                    self.is_connected = v.truthy();
                }
            }
            b"setConnectionState" => {
                if let Some([v]) = one() {
                    self.connection_state = NetworkConnectionState::from_wire(v);
                }
            }
            b"addIrcUser" => {
                if let Some([v]) = one() {
                    let hostmask = string_or_empty(v);
                    let nick = nick_of(&hostmask);
                    if !nick.is_empty() {
                        self.users.insert(nick.to_string());
                    }
                }
            }
            b"addIrcChannel" => {
                if let Some([v]) = one() {
                    let channel = string_or_empty(v);
                    if !channel.is_empty() {
                        self.channels.insert(channel);
                    }
                }
            }
            _ => log_unknown_slot("Network", &name, slot, params),
        }
    }

    pub fn apply_init_data(&mut self, data: &VariantMap) {
        for (key, value) in data {
            self.apply_init_field(key, value);
        }
        self.initialized = true;
    }

    pub fn apply_init_field(&mut self, key: &str, value: &Variant) {
        match key {
            "networkName" => self.network_name = string_or_empty(value),
            "currentServer" => self.current_server = string_or_empty(value),
            "myNick" => self.my_nick = string_or_empty(value),
            "latency" => match value {
                Variant::Null => self.latency = 0,
                other => match other.coerce_i64() {
                    Some(latency) => self.latency = latency,
                    None => tracing::warn!("Network({:?}): bad latency", self.object_name),
                },
            },
            "isConnected" => self.is_connected = value.truthy(),
            "connectionState" => self.connection_state = NetworkConnectionState::from_wire(value),
            "IrcUsersAndChannels" => self.apply_users_and_channels(value),
            _ => log_unknown_field("Network", &self.object_name, key, value),
        }
    }

    /// Capture the roster seed. Two wire shapes exist:
    ///
    /// - Modern cores (>= 0.10) send struct-of-arrays: `Users` and
    ///   `Channels` map each attribute to a parallel list, e.g.
    ///   `{"nick": ["alice", "bob"], "user": [...]}`. This is what every
    ///   real core sends today.
    /// - The legacy shape maps object name to a per-object field map.
    fn apply_users_and_channels(&mut self, value: &Variant) {
        let Some(map) = value.as_map() else {
            return;
        };
        let pick = |a: &str, b: &str| {
            map.get(a)
                .filter(|v| v.truthy())
                .or_else(|| map.get(b))
                .and_then(Variant::as_map)
                .cloned()
        };
        if let Some(users) = pick("Users", "users") {
            self.users_seed =
                explode_parallel_arrays(&users, "nick").unwrap_or_else(|| legacy_seed(&users));
            self.users.extend(self.users_seed.keys().cloned());
        }
        if let Some(channels) = pick("Channels", "channels") {
            self.channels_seed = explode_parallel_arrays(&channels, "name")
                .unwrap_or_else(|| legacy_seed(&channels));
            self.channels.extend(self.channels_seed.keys().cloned());
        }
    }
}

fn legacy_seed(map: &VariantMap) -> BTreeMap<String, VariantMap> {
    map.iter()
        .filter_map(|(k, v)| v.as_map().map(|m| (k.clone(), m.clone())))
        .collect()
}

/// Turn `{"nick": ["alice", "bob"], "user": ["al", "bo"]}` into per-name
/// field maps. `None` when the key column isn't a list (the legacy shape).
/// Columns shorter than the key column omit the attribute for the missing
/// entries.
pub fn explode_parallel_arrays(
    value: &VariantMap,
    key_field: &str,
) -> Option<BTreeMap<String, VariantMap>> {
    let keys = value.get(key_field)?.list_items()?;
    let columns: Vec<(&String, Vec<Variant>)> = value
        .iter()
        .filter_map(|(attr, column)| column.list_items().map(|items| (attr, items)))
        .collect();
    let mut out = BTreeMap::new();
    for (i, key) in keys.iter().enumerate() {
        let name = string_or_empty(key);
        if name.is_empty() {
            continue;
        }
        let entry: VariantMap = columns
            .iter()
            .filter_map(|(attr, items)| items.get(i).map(|item| ((*attr).clone(), item.clone())))
            .collect();
        out.insert(name, entry);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(entries: Vec<(&str, Variant)>) -> VariantMap {
        entries
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect()
    }

    #[test]
    fn network_id_from_object_name() {
        assert_eq!(Network::new("7").network_id(), 7);
        assert_eq!(Network::new("nope").network_id(), -1);
    }

    #[test]
    fn slots() {
        let mut n = Network::new("1");
        n.handle_sync(b"setNetworkName", &["Libera".into()]);
        assert_eq!(n.network_name, "Libera");
        n.handle_sync(b"setConnectionState", &[Variant::Int(3)]);
        assert_eq!(n.connection_state, NetworkConnectionState::Initialized);
        n.handle_sync(b"setConnectionState", &[Variant::Int(99)]);
        assert_eq!(n.connection_state, NetworkConnectionState::Disconnected);
        n.handle_sync(b"addIrcUser", &["seanr!s@example.org".into()]);
        assert!(n.users.contains("seanr"));
        n.handle_sync(b"setConnected", &[Variant::Bool(true)]);
        assert!(n.is_connected);
        // Wrong arity is dropped, not applied.
        n.handle_sync(b"setNetworkName", &["a".into(), "b".into()]);
        assert_eq!(n.network_name, "Libera");
        n.handle_sync(b"setLatency", &[Variant::Int(42)]);
        assert_eq!(n.latency, 42);
    }

    #[test]
    fn init_data_scalars() {
        let mut n = Network::new("1");
        n.apply_init_data(&map(vec![
            ("networkName", "Libera".into()),
            ("currentServer", "irc.libera.chat".into()),
            ("myNick", "seanr".into()),
            ("latency", Variant::Int(12)),
            ("isConnected", Variant::Bool(true)),
            ("connectionState", Variant::Int(3)),
            ("somethingNew", Variant::Int(1)),
        ]));
        assert!(n.initialized);
        assert_eq!(n.network_name, "Libera");
        assert_eq!(n.current_server, "irc.libera.chat");
        assert_eq!(n.my_nick, "seanr");
        assert_eq!(n.latency, 12);
        assert!(n.is_connected);
        assert_eq!(n.connection_state, NetworkConnectionState::Initialized);
    }

    #[test]
    fn legacy_roster_seed() {
        let mut n = Network::new("1");
        n.apply_init_field(
            "IrcUsersAndChannels",
            &Variant::Map(map(vec![
                (
                    "Users",
                    Variant::Map(map(vec![(
                        "seanr",
                        Variant::Map(map(vec![("nick", "seanr".into())])),
                    )])),
                ),
                (
                    "Channels",
                    Variant::Map(map(vec![(
                        "#python",
                        Variant::Map(map(vec![("topic", "hi".into())])),
                    )])),
                ),
            ])),
        );
        assert!(n.users.contains("seanr"));
        assert!(n.channels.contains("#python"));
        assert_eq!(
            n.channels_seed["#python"]["topic"],
            Variant::String("hi".into())
        );
    }

    #[test]
    fn struct_of_arrays_seed() {
        let mut n = Network::new("1");
        n.apply_init_field(
            "IrcUsersAndChannels",
            &Variant::Map(map(vec![(
                "Users",
                Variant::Map(map(vec![
                    (
                        "nick",
                        Variant::StringList(vec!["alice".into(), "bob".into(), String::new()]),
                    ),
                    ("user", Variant::List(vec!["al".into()])),
                ])),
            )])),
        );
        assert_eq!(n.users.iter().collect::<Vec<_>>(), ["alice", "bob"]);
        assert_eq!(n.users_seed["alice"]["user"], Variant::String("al".into()));
        assert!(!n.users_seed["bob"].contains_key("user"));
    }
}
