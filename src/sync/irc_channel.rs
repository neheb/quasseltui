//! `IrcChannel`: one joined channel. Object name `"<networkId>/<channel>"`.

use std::collections::BTreeMap;

use crate::qt::variant::{Variant, VariantMap};
use crate::sync::{
    exact_params, log_unknown_field, log_unknown_slot, nick_of, split_object_name, string_or_empty,
};

pub const CLASS_NAME: &[u8] = b"IrcChannel";

#[derive(Debug, Clone, PartialEq, Default)]
pub struct IrcChannel {
    pub object_name: String,
    pub initialized: bool,
    pub network_id: i32,
    pub name: String,
    pub topic: String,
    pub password: String,
    pub encrypted: bool,
    /// Mode prefix string (e.g. `"@+"`) by nick. Present with `""` means a
    /// member without prefix modes; absent means not in the channel.
    pub user_modes: BTreeMap<String, String>,
    pub channel_modes: String,
}

impl IrcChannel {
    pub fn new(object_name: impl Into<String>) -> Self {
        let object_name = object_name.into();
        let (network_id, name) = split_object_name(&object_name);
        Self {
            object_name,
            network_id,
            name,
            ..Self::default()
        }
    }

    pub fn members(&self) -> impl Iterator<Item = &String> {
        self.user_modes.keys()
    }

    pub fn handle_sync(&mut self, slot: &[u8], params: &[Variant]) {
        let object = self.object_name.clone();
        let one = || exact_params::<1>("IrcChannel", &object, slot, params);
        let two = || exact_params::<2>("IrcChannel", &object, slot, params);
        match slot {
            b"setTopic" => {
                if let Some([v]) = one() {
                    self.topic = string_or_empty(v);
                }
            }
            b"setPassword" => {
                if let Some([v]) = one() {
                    self.password = string_or_empty(v);
                }
            }
            b"setEncrypted" => {
                if let Some([v]) = one() {
                    self.encrypted = v.truthy();
                }
            }
            b"joinIrcUsers" => {
                // `users` and `modes` are parallel lists; a shorter `modes`
                // pads with "". Anything that isn't a list is ignored rather
                // than crashing the dispatcher.
                if let Some([users, modes]) = two() {
                    let Some(users) = users.list_items() else {
                        return;
                    };
                    let modes = modes.list_items().unwrap_or_default();
                    for (i, user) in users.iter().enumerate() {
                        let hostmask = string_or_empty(user);
                        let nick = nick_of(&hostmask);
                        if !nick.is_empty() {
                            let mode = modes.get(i).map(string_or_empty).unwrap_or_default();
                            self.user_modes.insert(nick.to_string(), mode);
                        }
                    }
                }
            }
            b"part" => {
                if let Some([v]) = one() {
                    let hostmask = string_or_empty(v);
                    self.user_modes.remove(nick_of(&hostmask));
                }
            }
            b"addUserMode" => {
                if let Some([user, mode]) = two() {
                    let hostmask = string_or_empty(user);
                    let nick = nick_of(&hostmask);
                    if nick.is_empty() {
                        return;
                    }
                    let mode = string_or_empty(mode);
                    let existing = self.user_modes.get(nick).cloned().unwrap_or_default();
                    if !mode.is_empty() && !existing.contains(&mode) {
                        self.user_modes.insert(nick.to_string(), existing + &mode);
                    }
                }
            }
            b"removeUserMode" => {
                if let Some([user, mode]) = two() {
                    let hostmask = string_or_empty(user);
                    let nick = nick_of(&hostmask);
                    if nick.is_empty() {
                        return;
                    }
                    let mode = string_or_empty(mode);
                    let existing = self.user_modes.get(nick).cloned().unwrap_or_default();
                    let updated = if mode.is_empty() {
                        existing
                    } else {
                        existing.replace(&mode, "")
                    };
                    self.user_modes.insert(nick.to_string(), updated);
                }
            }
            _ => log_unknown_slot("IrcChannel", &object, slot, params),
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
            "name" => {
                if value.truthy() {
                    self.name = string_or_empty(value);
                }
            }
            "topic" => self.topic = string_or_empty(value),
            "password" => self.password = string_or_empty(value),
            "encrypted" => self.encrypted = value.truthy(),
            // A full snapshot replaces the roster wholesale.
            "UserModes" => {
                if let Some(map) = value.as_map() {
                    self.user_modes = map
                        .iter()
                        .map(|(nick, mode)| (nick.clone(), string_or_empty(mode)))
                        .collect();
                }
            }
            "ChanModes" => {
                if let Variant::String(modes) = value {
                    self.channel_modes = modes.clone();
                }
            }
            _ => log_unknown_field("IrcChannel", &self.object_name, key, value),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn members(c: &IrcChannel) -> Vec<&str> {
        c.members().map(String::as_str).collect()
    }

    #[test]
    fn object_name_split() {
        let c = IrcChannel::new("1/#python");
        assert_eq!((c.network_id, c.name.as_str()), (1, "#python"));
        let c = IrcChannel::new("garbage");
        assert_eq!((c.network_id, c.name.as_str()), (-1, "garbage"));
    }

    #[test]
    fn topic_join_part() {
        let mut c = IrcChannel::new("1/#python");
        c.handle_sync(b"setTopic", &["welcome".into()]);
        assert_eq!(c.topic, "welcome");
        c.handle_sync(
            b"joinIrcUsers",
            &[
                Variant::StringList(vec!["alice!a@h".into(), "bob".into()]),
                Variant::StringList(vec!["@".into()]),
            ],
        );
        assert_eq!(members(&c), ["alice", "bob"]);
        assert_eq!(c.user_modes["alice"], "@");
        assert_eq!(c.user_modes["bob"], "");
        c.handle_sync(b"part", &["alice!a@h".into()]);
        assert_eq!(members(&c), ["bob"]);
    }

    #[test]
    fn user_modes() {
        let mut c = IrcChannel::new("1/#python");
        c.handle_sync(b"addUserMode", &["bob".into(), "o".into()]);
        c.handle_sync(b"addUserMode", &["bob".into(), "v".into()]);
        c.handle_sync(b"addUserMode", &["bob".into(), "o".into()]);
        assert_eq!(c.user_modes["bob"], "ov");
        c.handle_sync(b"removeUserMode", &["bob".into(), "o".into()]);
        assert_eq!(c.user_modes["bob"], "v");
        c.handle_sync(b"removeUserMode", &["bob".into(), Variant::Null]);
        assert_eq!(c.user_modes["bob"], "v");
    }

    #[test]
    fn init_user_modes_replaces_roster() {
        let mut c = IrcChannel::new("1/#python");
        c.handle_sync(
            b"joinIrcUsers",
            &[Variant::StringList(vec!["old".into()]), Variant::Null],
        );
        let mut modes = VariantMap::new();
        modes.insert("alice".into(), "@".into());
        modes.insert("bob".into(), Variant::Null);
        c.apply_init_data(
            &[("UserModes".to_string(), Variant::Map(modes))]
                .into_iter()
                .collect(),
        );
        assert_eq!(members(&c), ["alice", "bob"]);
        assert_eq!(c.user_modes["bob"], "");
        assert!(c.initialized);
    }
}
