//! Static placeholder state for `quasseltui ui-demo`: two networks, a
//! status buffer, a busy channel, a quiet channel, and a query, so the
//! layout can be checked without a core.

use chrono::{DateTime, Duration, TimeZone, Utc};

use crate::client::{ClientState, IrcMessage};
use crate::protocol::Features;
use crate::protocol::types::{
    BufferId, BufferInfo, BufferType, IdentityId, MessageFlags, MessageType, MsgId, NetworkId,
};
use crate::sync::{Identity, Network, NetworkConnectionState};

fn epoch() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 4, 15, 14, 30, 0).unwrap()
}

/// A populated state covering the visible UI surface. Each call returns an
/// independent state.
pub fn build_demo_state() -> ClientState {
    let mut state = ClientState {
        peer_features: Features::LONG_TIME | Features::SENDER_PREFIXES,
        ..ClientState::default()
    };

    state.networks.insert(
        NetworkId(1),
        network(1, "Libera.Chat", "irc.libera.chat", "seanr"),
    );
    state
        .networks
        .insert(NetworkId(2), network(2, "OFTC", "irc.oftc.net", "seanr"));

    let mut identity = Identity::new("1");
    identity.identity_name = "default".into();
    identity.nicks = vec!["seanr".into(), "seanr_".into()];
    state.identities.insert(IdentityId(1), identity);

    for (id, net, kind, name) in [
        (10, 1, BufferType::Status, ""),
        (11, 1, BufferType::Channel, "#python"),
        (12, 1, BufferType::Channel, "#rust"),
        (13, 1, BufferType::Query, "nickbot"),
        (20, 2, BufferType::Status, ""),
        (21, 2, BufferType::Channel, "#debian"),
    ] {
        let info = BufferInfo {
            buffer_id: BufferId(id),
            network_id: NetworkId(net),
            kind,
            group_id: 0,
            name: name.into(),
        };
        state.messages.insert(info.buffer_id, Vec::new());
        state.buffers.insert(info.buffer_id, info);
    }

    let python = [
        (1001, 0, "guido", "@", "anyone awake on 3.14?"),
        (1002, 12, "raymondh", "+", "I'm here. what's up?"),
        (
            1003,
            24,
            "guido",
            "@",
            "PEP 703 landed, wondering if anyone tried it under load",
        ),
        (
            1004,
            40,
            "seanr",
            "",
            "I can benchmark tomorrow if you want comparison numbers",
        ),
        (
            1005,
            55,
            "raymondh",
            "+",
            "nice. post them in the topic when you do",
        ),
    ];
    state.messages.insert(
        BufferId(11),
        python
            .iter()
            .map(|(id, offset, sender, prefixes, text)| {
                message(*id, 11, *offset, sender, prefixes, text)
            })
            .collect(),
    );
    state.messages.insert(
        BufferId(12),
        vec![
            message(
                2001,
                12,
                -600,
                "graydon",
                "",
                "ok the async trait work finally merged",
            ),
            message(
                2002,
                12,
                -590,
                "seanr",
                "",
                "huge. does it hit stable in the next cycle?",
            ),
        ],
    );
    state
}

fn network(id: i32, name: &str, server: &str, nick: &str) -> Network {
    let mut net = Network::new(id.to_string());
    net.network_name = name.into();
    net.current_server = server.into();
    net.my_nick = nick.into();
    net.connection_state = NetworkConnectionState::Initialized;
    net.is_connected = true;
    net
}

fn message(
    id: i64,
    buffer: i32,
    offset: i64,
    sender: &str,
    prefixes: &str,
    contents: &str,
) -> IrcMessage {
    IrcMessage {
        msg_id: MsgId(id),
        buffer_id: BufferId(buffer),
        network_id: NetworkId(1),
        timestamp: epoch() + Duration::seconds(offset),
        kind: MessageType::Plain,
        flags: MessageFlags::NONE,
        sender: sender.into(),
        sender_prefixes: prefixes.into(),
        contents: contents.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn demo_state_shape() {
        let state = build_demo_state();
        assert!(state.networks.values().all(|n| !n.my_nick.is_empty()));
        assert!(
            state
                .buffers
                .keys()
                .all(|id| state.messages.contains_key(id))
        );
        assert!(state.messages.values().any(|m| !m.is_empty()));
        let kinds: std::collections::HashSet<_> = state.buffers.values().map(|b| b.kind).collect();
        assert!(kinds.contains(&BufferType::Status));
        assert!(kinds.contains(&BufferType::Channel));
        assert!(kinds.contains(&BufferType::Query));
        let mut other = build_demo_state();
        other.messages.clear();
        assert!(!state.messages.is_empty());
    }
}
