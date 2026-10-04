//! `IrcUser`: one IRC user on a network. Object name `"<networkId>/<nick>"`.
//!
//! Quassel re-addresses users on nick change (`IrcUser::setNick` ->
//! `renameObject`, broadcast as `__objectRenamed__`); the dispatcher handles
//! that and calls [`IrcUser::rename`]. The `setNick` slot still updates the
//! field for completeness.

use std::collections::BTreeSet;

use crate::qt::variant::{Variant, VariantMap};
use crate::sync::{
    exact_params, log_unknown_field, log_unknown_slot, split_object_name, string_or_empty,
};

pub const CLASS_NAME: &[u8] = b"IrcUser";

#[derive(Debug, Clone, PartialEq, Default)]
pub struct IrcUser {
    pub object_name: String,
    pub initialized: bool,
    pub network_id: i32,
    pub nick: String,
    pub user: String,
    pub host: String,
    pub real_name: String,
    pub account: String,
    pub away: bool,
    pub away_message: String,
    /// Channels (without network prefix) the user is in.
    pub channels: BTreeSet<String>,
}

impl IrcUser {
    pub fn new(object_name: impl Into<String>) -> Self {
        let object_name = object_name.into();
        let (network_id, nick) = split_object_name(&object_name);
        Self {
            object_name,
            network_id,
            nick,
            ..Self::default()
        }
    }

    /// Re-address after `__objectRenamed__`, keeping object name, network
    /// id, and nick consistent.
    pub fn rename(&mut self, object_name: &str) {
        self.object_name = object_name.to_string();
        let (network_id, nick) = split_object_name(object_name);
        self.network_id = network_id;
        self.nick = nick;
    }

    pub fn handle_sync(&mut self, slot: &[u8], params: &[Variant]) {
        let object = self.object_name.clone();
        let one = || exact_params::<1>("IrcUser", &object, slot, params);
        match slot {
            b"setNick" => {
                if let Some([v]) = one()
                    && v.truthy()
                {
                    self.nick = string_or_empty(v);
                }
            }
            b"setUser" => {
                if let Some([v]) = one() {
                    self.user = string_or_empty(v);
                }
            }
            b"setHost" => {
                if let Some([v]) = one() {
                    self.host = string_or_empty(v);
                }
            }
            b"setRealName" => {
                if let Some([v]) = one() {
                    self.real_name = string_or_empty(v);
                }
            }
            b"setAccount" => {
                if let Some([v]) = one() {
                    self.account = string_or_empty(v);
                }
            }
            b"setAway" => {
                if let Some([v]) = one() {
                    self.away = v.truthy();
                }
            }
            b"setAwayMessage" => {
                if let Some([v]) = one() {
                    self.away_message = string_or_empty(v);
                }
            }
            b"joinChannel" => {
                if let Some([v]) = one()
                    && v.truthy()
                {
                    self.channels.insert(string_or_empty(v));
                }
            }
            b"partChannel" => {
                if let Some([v]) = one() {
                    self.channels.remove(&string_or_empty(v));
                }
            }
            b"quit" => {
                // The dispatcher's cascade removes us from every roster.
                if exact_params::<0>("IrcUser", &object, slot, params).is_some() {
                    self.channels.clear();
                }
            }
            _ => log_unknown_slot("IrcUser", &object, slot, params),
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
            "nick" => {
                if value.truthy() {
                    self.nick = string_or_empty(value);
                }
            }
            "user" => self.user = string_or_empty(value),
            "host" => self.host = string_or_empty(value),
            "realName" => self.real_name = string_or_empty(value),
            "account" => self.account = string_or_empty(value),
            "away" => self.away = value.truthy(),
            "awayMessage" => self.away_message = string_or_empty(value),
            _ => log_unknown_field("IrcUser", &self.object_name, key, value),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_name_and_rename() {
        let mut u = IrcUser::new("1/seanr");
        assert_eq!((u.network_id, u.nick.as_str()), (1, "seanr"));
        u.rename("1/sean_away");
        assert_eq!(u.object_name, "1/sean_away");
        assert_eq!(u.nick, "sean_away");
    }

    #[test]
    fn slots() {
        let mut u = IrcUser::new("1/seanr");
        u.handle_sync(b"setNick", &["sean2".into()]);
        assert_eq!(u.nick, "sean2");
        u.handle_sync(b"setAway", &[Variant::Bool(true)]);
        u.handle_sync(b"setAwayMessage", &["lunch".into()]);
        assert!(u.away);
        assert_eq!(u.away_message, "lunch");
        u.handle_sync(b"joinChannel", &["#python".into()]);
        u.handle_sync(b"joinChannel", &["#rust".into()]);
        u.handle_sync(b"partChannel", &["#python".into()]);
        assert_eq!(u.channels.iter().collect::<Vec<_>>(), ["#rust"]);
        u.handle_sync(b"quit", &[]);
        assert!(u.channels.is_empty());
    }
}
