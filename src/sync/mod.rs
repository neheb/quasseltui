//! The syncable object model: `Network`, `IrcChannel`, `IrcUser`,
//! `Identity`, `BufferSyncer`, and `BacklogManager`.
//!
//! Each object mirrors one Quassel C++ class addressed on the wire by
//! `(className, objectName)`. `Sync` frames invoke named slots with
//! positional parameters (state changes); `InitData` frames carry a flat
//! property map (current state). Unknown slots and fields are dropped, so a
//! newer core can add slots without breaking the connection. A slot called
//! with the wrong number of parameters is logged and dropped too.
//!
//! The [`Dispatcher`] routes frames to objects and turns their effects into
//! [`events::ClientEvent`]s.

pub mod backlog_manager;
pub mod buffer_syncer;
pub mod dispatcher;
pub mod events;
pub mod identity;
pub mod irc_channel;
pub mod irc_user;
pub mod network;
pub mod state;

pub use backlog_manager::BacklogManager;
pub use buffer_syncer::BufferSyncer;
pub use dispatcher::Dispatcher;
pub use identity::Identity;
pub use irc_channel::IrcChannel;
pub use irc_user::IrcUser;
pub use network::{Network, NetworkConnectionState};
pub use state::ClientState;

use crate::qt::variant::Variant;

/// Borrow exactly `N` slot parameters, or log and return `None`.
pub(crate) fn exact_params<'a, const N: usize>(
    class: &str,
    object_name: &str,
    slot: &[u8],
    params: &'a [Variant],
) -> Option<&'a [Variant; N]> {
    let got = params.try_into().ok();
    if got.is_none() {
        tracing::warn!(
            "{class}({object_name:?}): slot {:?} expects {N} params, got {}",
            String::from_utf8_lossy(slot),
            params.len()
        );
    }
    got
}

pub(crate) fn log_unknown_slot(class: &str, object_name: &str, slot: &[u8], params: &[Variant]) {
    tracing::debug!(
        "{class}({object_name:?}): unknown slot {:?}, dropping {} params",
        String::from_utf8_lossy(slot),
        params.len()
    );
}

pub(crate) fn log_unknown_field(class: &str, object_name: &str, key: &str, value: &Variant) {
    tracing::debug!(
        "{class}({object_name:?}): unknown init field {key:?} (value type {})",
        value.type_name()
    );
}

/// String value of a parameter; null becomes `""`.
pub(crate) fn string_or_empty(value: &Variant) -> String {
    value.coerce_string().unwrap_or_default()
}

/// The nick part of a `nick!user@host` hostmask.
pub(crate) fn nick_of(hostmask: &str) -> &str {
    hostmask.split('!').next().unwrap_or_default()
}

/// Split `"<netId>/<rest>"`. Returns `(-1, ...)` when the prefix is
/// missing or not a number (an old core, or a test building objects by
/// hand).
pub(crate) fn split_object_name(object_name: &str) -> (i32, String) {
    match object_name.split_once('/') {
        None => (-1, object_name.to_string()),
        Some((prefix, rest)) => (prefix.parse().unwrap_or(-1), rest.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helpers() {
        assert_eq!(nick_of("seanr!s@h"), "seanr");
        assert_eq!(nick_of("seanr"), "seanr");
        assert_eq!(split_object_name("1/#python"), (1, "#python".into()));
        assert_eq!(split_object_name("1/a/b"), (1, "a/b".into()));
        assert_eq!(split_object_name("weird"), (-1, "weird".into()));
        assert_eq!(split_object_name("x/#c"), (-1, "#c".into()));
        assert_eq!(string_or_empty(&Variant::Null), "");
        let params = [Variant::Int(1)];
        assert!(exact_params::<1>("X", "", b"s", &params).is_some());
        assert!(exact_params::<2>("X", "", b"s", &params).is_none());
    }
}
