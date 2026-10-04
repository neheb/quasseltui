//! Pure formatting and ordering helpers shared by the model and the view.
//!
//! Every string from the core goes through `sanitize_terminal` before it is
//! drawn: ratatui writes cell contents to the terminal verbatim, so an ESC
//! in a message body would otherwise reach the terminal.

use chrono::{DateTime, Local, Utc};

use crate::client::{ClientState, IrcMessage};
use crate::protocol::types::{BufferId, BufferInfo, BufferType, MessageType};
use crate::util::text::{sanitize_terminal, strip_mirc_formatting};

/// What a piece of a log line is, so the view can style it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SegmentKind {
    Timestamp,
    /// The type marker for non-chat lines (`-->`, `NOTICE`, ...).
    Prefix,
    Nick,
    Body,
    /// Join/part/quit/mode/... lines, drawn subdued.
    Event,
    /// Error and server lines.
    Alert,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    pub text: String,
    pub kind: SegmentKind,
}

impl Segment {
    fn new(text: impl Into<String>, kind: SegmentKind) -> Self {
        Self {
            text: text.into(),
            kind,
        }
    }
}

fn type_prefix(kind: MessageType) -> &'static str {
    match kind {
        MessageType::Notice => "NOTICE ",
        MessageType::Action => "* ",
        MessageType::Join => "--> ",
        MessageType::Part | MessageType::Quit | MessageType::Kick => "<-- ",
        MessageType::Nick | MessageType::Mode | MessageType::Topic | MessageType::Info => "-- ",
        MessageType::Server | MessageType::Error => "!! ",
        _ => "",
    }
}

fn is_event(kind: MessageType) -> bool {
    matches!(
        kind,
        MessageType::Join
            | MessageType::Part
            | MessageType::Quit
            | MessageType::Kick
            | MessageType::Kill
            | MessageType::Nick
            | MessageType::Mode
            | MessageType::Topic
            | MessageType::Info
            | MessageType::DayChange
            | MessageType::NetsplitJoin
            | MessageType::NetsplitQuit
            | MessageType::Invite
    )
}

/// The nick part of a `nick!user@host` sender.
pub fn short_sender(sender: &str) -> &str {
    sender.split('!').next().unwrap_or(sender)
}

/// Local wall-clock time of a message.
pub fn local_time(ts: DateTime<Utc>) -> DateTime<Local> {
    ts.with_timezone(&Local)
}

/// One message as styled segments:
/// `HH:MM:SS <type prefix><sender prefixes><nick>: <contents>`, or
/// `HH:MM:SS * nick contents` for actions. mIRC formatting is stripped
/// (it's routine and meaningless here); other control bytes are escaped.
pub fn format_message(msg: &IrcMessage) -> Vec<Segment> {
    let ts = local_time(msg.timestamp).format("%H:%M:%S").to_string();
    let prefix = type_prefix(msg.kind);
    let sender = sanitize_terminal(short_sender(&msg.sender));
    let contents = sanitize_terminal(&strip_mirc_formatting(&msg.contents));
    let mut out = vec![Segment::new(format!("{ts} "), SegmentKind::Timestamp)];
    if is_event(msg.kind) || matches!(msg.kind, MessageType::Server | MessageType::Error) {
        let kind = if is_event(msg.kind) {
            SegmentKind::Event
        } else {
            SegmentKind::Alert
        };
        let prefixes = sanitize_terminal(&msg.sender_prefixes);
        // Server-generated lines (topic announcements, netsplits) have no
        // sender; don't print a dangling ": ".
        let text = if sender.is_empty() {
            format!("{prefix}{contents}")
        } else {
            format!("{prefix}{prefixes}{sender}: {contents}")
        };
        out.push(Segment::new(text, kind));
        return out;
    }
    if !prefix.is_empty() {
        out.push(Segment::new(prefix, SegmentKind::Prefix));
    }
    if msg.kind == MessageType::Action {
        out.push(Segment::new(sender, SegmentKind::Nick));
        out.push(Segment::new(format!(" {contents}"), SegmentKind::Body));
    } else {
        let prefixes = sanitize_terminal(&msg.sender_prefixes);
        out.push(Segment::new(
            format!("{prefixes}{sender}"),
            SegmentKind::Nick,
        ));
        out.push(Segment::new(format!(": {contents}"), SegmentKind::Body));
    }
    out
}

/// The plain text of a formatted message.
pub fn format_message_plain(msg: &IrcMessage) -> String {
    format_message(msg).into_iter().map(|s| s.text).collect()
}

/// Sidebar label. Status buffers have no name on the wire.
pub fn buffer_label(buf: &BufferInfo) -> String {
    if buf.kind == BufferType::Status {
        return "(status)".into();
    }
    if buf.name.is_empty() {
        "(unnamed)".into()
    } else {
        sanitize_terminal(&buf.name)
    }
}

/// Status first, then channels, then queries; names case-insensitively,
/// since real users mix `#Python` and `#python`.
pub fn buffer_sort_key(buf: &BufferInfo) -> (i16, String) {
    (buf.kind.value(), buf.name.to_lowercase())
}

/// Buffers in sidebar order: networks by id, then by [`buffer_sort_key`].
/// Buffers whose network isn't known aren't shown.
pub fn ordered_buffers(state: &ClientState) -> Vec<&BufferInfo> {
    let mut out = Vec::new();
    for network_id in state.networks.keys() {
        let mut buffers: Vec<&BufferInfo> = state
            .buffers
            .values()
            .filter(|b| b.network_id == *network_id)
            .collect();
        buffers.sort_by_key(|b| buffer_sort_key(b));
        out.extend(buffers);
    }
    out
}

pub fn ordered_buffer_ids(state: &ClientState) -> Vec<BufferId> {
    ordered_buffers(state).iter().map(|b| b.buffer_id).collect()
}

/// The first buffer with any messages, or any buffer, or `None`.
pub fn pick_default_buffer(state: &ClientState) -> Option<BufferId> {
    state
        .messages
        .iter()
        .find(|(id, msgs)| !msgs.is_empty() && state.buffers.contains_key(id))
        .map(|(id, _)| *id)
        .or_else(|| state.buffers.keys().next().copied())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::types::{MessageFlags, MsgId, NetworkId};
    use crate::sync::Network;

    fn msg(sender: &str, contents: &str, kind: MessageType, prefixes: &str) -> IrcMessage {
        IrcMessage {
            msg_id: MsgId(1),
            buffer_id: BufferId(1),
            network_id: NetworkId(1),
            timestamp: Utc::now(),
            kind,
            flags: MessageFlags::NONE,
            sender: sender.into(),
            sender_prefixes: prefixes.into(),
            contents: contents.into(),
        }
    }

    fn info(id: i32, net: i32, kind: BufferType, name: &str) -> BufferInfo {
        BufferInfo {
            buffer_id: BufferId(id),
            network_id: NetworkId(net),
            kind,
            group_id: 0,
            name: name.into(),
        }
    }

    #[test]
    fn message_shapes() {
        let plain = format_message_plain(&msg(
            "seanr!sean@example.com",
            "hello",
            MessageType::Plain,
            "@",
        ));
        assert!(plain.ends_with(" @seanr: hello"), "{plain}");
        assert!(!plain.contains("NOTICE"));
        let action = format_message_plain(&msg("seanr", "waves", MessageType::Action, ""));
        assert!(action.ends_with(" * seanr waves"), "{action}");
        let notice =
            format_message_plain(&msg("server.example.com", "MOTD", MessageType::Notice, ""));
        assert!(
            notice.contains("NOTICE server.example.com: MOTD"),
            "{notice}"
        );
        let join = format_message_plain(&msg("alice!a@h", "", MessageType::Join, ""));
        assert!(join.contains("--> alice: "), "{join}");
        let topic =
            format_message_plain(&msg("", "Topic for #x is \"hi\"", MessageType::Topic, ""));
        assert!(topic.ends_with(" -- Topic for #x is \"hi\""), "{topic}");
    }

    #[test]
    fn hostile_text_is_escaped() {
        let plain = format_message_plain(&msg("bad", "\x1b[31mREDRUM", MessageType::Plain, ""));
        assert!(!plain.contains('\x1b') && plain.contains("\\x1b") && plain.contains("REDRUM"));
        let plain = format_message_plain(&msg("spoof\x07\x08er", "hi", MessageType::Plain, ""));
        assert!(!plain.contains('\x07') && !plain.contains('\x08'));
        let plain = format_message_plain(&msg("nick", "hi", MessageType::Plain, "@\x1b"));
        assert!(!plain.contains('\x1b'));
        let plain = format_message_plain(&msg(
            "eve",
            "line1\nnick!eve@evil: line2",
            MessageType::Plain,
            "",
        ));
        assert!(!plain.contains('\n') && plain.contains("\\x0a"));
        let plain = format_message_plain(&msg(
            "bot",
            "\x02bold\x02 \x034,2red",
            MessageType::Plain,
            "",
        ));
        assert!(plain.ends_with("bot: bold red"), "{plain}");
    }

    #[test]
    fn labels_and_sorting() {
        assert_eq!(
            buffer_label(&info(1, 1, BufferType::Status, "")),
            "(status)"
        );
        assert_eq!(
            buffer_label(&info(1, 1, BufferType::Channel, "#python")),
            "#python"
        );
        assert_eq!(
            buffer_label(&info(1, 1, BufferType::Query, "")),
            "(unnamed)"
        );
        assert_eq!(
            buffer_label(&info(1, 1, BufferType::Channel, "#a\x1b")),
            "#a\\x1b"
        );
        assert!(
            buffer_sort_key(&info(1, 1, BufferType::Status, "z"))
                < buffer_sort_key(&info(2, 1, BufferType::Channel, "a"))
        );
        assert_eq!(
            buffer_sort_key(&info(1, 1, BufferType::Channel, "#Python")),
            buffer_sort_key(&info(2, 1, BufferType::Channel, "#python"))
        );
        assert_eq!(short_sender("seanr!s@h"), "seanr");
        assert_eq!(short_sender("seanr"), "seanr");
    }

    #[test]
    fn ordering_and_default_pick() {
        let mut state = ClientState::default();
        assert_eq!(pick_default_buffer(&state), None);
        state.networks.insert(NetworkId(2), Network::new("2"));
        state.networks.insert(NetworkId(1), Network::new("1"));
        for b in [
            info(20, 2, BufferType::Status, ""),
            info(13, 1, BufferType::Query, "nickbot"),
            info(11, 1, BufferType::Channel, "#python"),
            info(10, 1, BufferType::Status, ""),
            info(99, 9, BufferType::Channel, "#orphan"),
        ] {
            state.messages.insert(b.buffer_id, vec![]);
            state.buffers.insert(b.buffer_id, b);
        }
        assert_eq!(
            ordered_buffer_ids(&state),
            [BufferId(10), BufferId(11), BufferId(13), BufferId(20)]
        );
        assert_eq!(pick_default_buffer(&state), Some(BufferId(10)));
        state
            .messages
            .get_mut(&BufferId(13))
            .unwrap()
            .push(msg("a", "b", MessageType::Plain, ""));
        assert_eq!(pick_default_buffer(&state), Some(BufferId(13)));
    }
}
