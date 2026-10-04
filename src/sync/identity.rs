//! `Identity`: nick/real name/away configuration. Object name is the
//! identity id. Fields we don't model are kept in `extra` so debug tools and
//! newer cores lose nothing.

use crate::qt::variant::{Variant, VariantMap};
use crate::sync::{exact_params, log_unknown_slot, string_or_empty};

pub const CLASS_NAME: &[u8] = b"Identity";

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Identity {
    pub object_name: String,
    pub initialized: bool,
    pub identity_id: i32,
    pub identity_name: String,
    pub real_name: String,
    pub ident: String,
    pub nicks: Vec<String>,
    pub away_nick: String,
    pub extra: VariantMap,
}

fn string_list(value: &Variant) -> Option<Vec<String>> {
    value
        .list_items()
        .map(|items| items.iter().map(string_or_empty).collect())
}

impl Identity {
    pub fn new(object_name: impl Into<String>) -> Self {
        let object_name = object_name.into();
        Self {
            identity_id: object_name.parse().unwrap_or(-1),
            object_name,
            ..Self::default()
        }
    }

    pub fn handle_sync(&mut self, slot: &[u8], params: &[Variant]) {
        let object = self.object_name.clone();
        let one = || exact_params::<1>("Identity", &object, slot, params);
        match slot {
            b"setIdentityName" => {
                if let Some([v]) = one() {
                    self.identity_name = string_or_empty(v);
                }
            }
            b"setRealName" => {
                if let Some([v]) = one() {
                    self.real_name = string_or_empty(v);
                }
            }
            b"setIdent" => {
                if let Some([v]) = one() {
                    self.ident = string_or_empty(v);
                }
            }
            b"setNicks" => {
                if let Some([v]) = one()
                    && let Some(nicks) = string_list(v)
                {
                    self.nicks = nicks;
                }
            }
            _ => log_unknown_slot("Identity", &object, slot, params),
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
            "identityName" => self.identity_name = string_or_empty(value),
            "realName" => self.real_name = string_or_empty(value),
            "ident" => self.ident = string_or_empty(value),
            "nicks" => {
                if let Some(nicks) = string_list(value) {
                    self.nicks = nicks;
                }
            }
            "awayNick" => self.away_nick = string_or_empty(value),
            _ => {
                self.extra.insert(key.to_string(), value.clone());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_from_object_name() {
        assert_eq!(Identity::new("3").identity_id, 3);
        assert_eq!(Identity::new("x").identity_id, -1);
    }

    #[test]
    fn fields_and_extra() {
        let mut i = Identity::new("1");
        let data: VariantMap = [
            ("identityName".to_string(), Variant::from("default")),
            ("realName".to_string(), "Sean".into()),
            ("ident".to_string(), "sean".into()),
            (
                "nicks".to_string(),
                Variant::StringList(vec!["sean".into(), "sean_".into()]),
            ),
            ("awayNick".to_string(), "sean_away".into()),
            ("autoAwayEnabled".to_string(), Variant::Bool(true)),
        ]
        .into_iter()
        .collect();
        i.apply_init_data(&data);
        assert_eq!(i.identity_name, "default");
        assert_eq!(i.nicks, ["sean", "sean_"]);
        assert_eq!(i.away_nick, "sean_away");
        assert_eq!(i.extra["autoAwayEnabled"], Variant::Bool(true));
        i.handle_sync(b"setNicks", &[Variant::List(vec!["x".into()])]);
        assert_eq!(i.nicks, ["x"]);
    }
}
