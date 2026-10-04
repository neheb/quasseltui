//! Quassel's custom `QVariant<UserType>` payloads.
//!
//! On the wire a user-type variant is `quint32 127`, the null flag, the type
//! name as a QByteArray (e.g. `b"BufferInfo"`), then the payload written by
//! that type's `operator<<` in the Quassel C++ source. The set of types is
//! closed, so it is modelled as an enum rather than a runtime registry.

use chrono::{DateTime, NaiveDate, TimeZone, Utc};

use crate::protocol::features::Features;
use crate::protocol::types::{
    AccountId, BufferId, BufferInfo, BufferType, IdentityId, Message, MessageFlags, MessageType,
    MsgId, NetworkId, UserId,
};
use crate::qt::datastream::{DecodeError, Reader, Writer};
use crate::qt::variant::{VariantMap, read_qvariantmap, write_qvariantmap};

pub const USER_TYPE_BUFFER_ID: &[u8] = b"BufferId";
pub const USER_TYPE_NETWORK_ID: &[u8] = b"NetworkId";
pub const USER_TYPE_IDENTITY_ID: &[u8] = b"IdentityId";
pub const USER_TYPE_USER_ID: &[u8] = b"UserId";
pub const USER_TYPE_ACCOUNT_ID: &[u8] = b"AccountId";
pub const USER_TYPE_MSG_ID: &[u8] = b"MsgId";
pub const USER_TYPE_BUFFER_INFO: &[u8] = b"BufferInfo";
pub const USER_TYPE_IDENTITY: &[u8] = b"Identity";
pub const USER_TYPE_MESSAGE: &[u8] = b"Message";
pub const USER_TYPE_NETWORK_SERVER: &[u8] = b"Network::Server";

/// A decoded Quassel user-type value.
#[derive(Debug, Clone, PartialEq)]
pub enum UserValue {
    BufferId(BufferId),
    NetworkId(NetworkId),
    IdentityId(IdentityId),
    UserId(UserId),
    AccountId(AccountId),
    MsgId(MsgId),
    BufferInfo(BufferInfo),
    Message(Box<Message>),
    /// `Identity::toVariantMap()`; the sync layer models the fields.
    Identity(VariantMap),
    /// `Network::Server::toVariantMap()`, passed through unmodelled.
    NetworkServer(VariantMap),
}

impl UserValue {
    /// The on-wire type name.
    pub fn type_name(&self) -> &'static [u8] {
        match self {
            Self::BufferId(_) => USER_TYPE_BUFFER_ID,
            Self::NetworkId(_) => USER_TYPE_NETWORK_ID,
            Self::IdentityId(_) => USER_TYPE_IDENTITY_ID,
            Self::UserId(_) => USER_TYPE_USER_ID,
            Self::AccountId(_) => USER_TYPE_ACCOUNT_ID,
            Self::MsgId(_) => USER_TYPE_MSG_ID,
            Self::BufferInfo(_) => USER_TYPE_BUFFER_INFO,
            Self::Message(_) => USER_TYPE_MESSAGE,
            Self::Identity(_) => USER_TYPE_IDENTITY,
            Self::NetworkServer(_) => USER_TYPE_NETWORK_SERVER,
        }
    }

    /// Integer value of the id-shaped user types.
    pub fn as_id(&self) -> Option<i64> {
        match self {
            Self::BufferId(v) => Some(i64::from(v.0)),
            Self::NetworkId(v) => Some(i64::from(v.0)),
            Self::IdentityId(v) => Some(i64::from(v.0)),
            Self::UserId(v) => Some(i64::from(v.0)),
            Self::AccountId(v) => Some(i64::from(v.0)),
            Self::MsgId(v) => Some(v.0),
            _ => None,
        }
    }

    /// Decode the payload that follows the type name.
    ///
    /// `name` must already have its trailing NUL bytes stripped: older
    /// Quassel versions wrote the C string terminator into the name.
    pub fn read_payload(reader: &mut Reader<'_>, name: &[u8]) -> Result<Self, DecodeError> {
        Ok(match name {
            USER_TYPE_BUFFER_ID => Self::BufferId(BufferId(reader.read_i32()?)),
            USER_TYPE_NETWORK_ID => Self::NetworkId(NetworkId(reader.read_i32()?)),
            USER_TYPE_IDENTITY_ID => Self::IdentityId(IdentityId(reader.read_i32()?)),
            USER_TYPE_USER_ID => Self::UserId(UserId(reader.read_i32()?)),
            USER_TYPE_ACCOUNT_ID => Self::AccountId(AccountId(reader.read_i32()?)),
            USER_TYPE_MSG_ID => Self::MsgId(MsgId(reader.read_i64()?)),
            USER_TYPE_BUFFER_INFO => Self::BufferInfo(read_buffer_info(reader)?),
            USER_TYPE_MESSAGE => Self::Message(Box::new(read_message(reader)?)),
            USER_TYPE_IDENTITY => Self::Identity(read_qvariantmap(reader)?),
            USER_TYPE_NETWORK_SERVER => Self::NetworkServer(read_qvariantmap(reader)?),
            other => {
                return Err(DecodeError(format!(
                    "unsupported QVariant<UserType> name {:?} at offset {}; no codec registered",
                    String::from_utf8_lossy(other),
                    reader.position()
                )));
            }
        })
    }

    pub fn write_payload(&self, writer: &mut Writer) {
        match self {
            Self::BufferId(v) => writer.write_i32(v.0),
            Self::NetworkId(v) => writer.write_i32(v.0),
            Self::IdentityId(v) => writer.write_i32(v.0),
            Self::UserId(v) => writer.write_i32(v.0),
            Self::AccountId(v) => writer.write_i32(v.0),
            Self::MsgId(v) => writer.write_i64(v.0),
            Self::BufferInfo(v) => write_buffer_info(writer, v),
            Self::Message(v) => write_message(writer, v),
            Self::Identity(map) | Self::NetworkServer(map) => write_qvariantmap(writer, map),
        }
    }
}

/// Strip trailing NULs from a user-type name, mirroring Quassel's
/// `deserializeQVariant` cleanup.
pub fn normalize_name(name: &[u8]) -> &[u8] {
    let end = name.iter().rposition(|b| *b != 0).map_or(0, |i| i + 1);
    &name[..end]
}

/// `BufferInfo::operator<<`: qint32 id, qint32 network, qint16 type,
/// quint32 group, QByteArray UTF-8 name.
pub fn read_buffer_info(reader: &mut Reader<'_>) -> Result<BufferInfo, DecodeError> {
    let buffer_id = reader.read_i32()?;
    let network_id = reader.read_i32()?;
    let kind = BufferType::from_wire(reader.read_i16()?);
    let group_id = reader.read_u32()?;
    let name = utf8_lossy(reader.read_qbytearray()?);
    Ok(BufferInfo {
        buffer_id: BufferId(buffer_id),
        network_id: NetworkId(network_id),
        kind,
        group_id,
        name,
    })
}

pub fn write_buffer_info(writer: &mut Writer, value: &BufferInfo) {
    writer.write_i32(value.buffer_id.0);
    writer.write_i32(value.network_id.0);
    writer.write_i16(value.kind.value());
    writer.write_u32(value.group_id);
    writer.write_qbytearray(Some(value.name.as_bytes()));
}

/// `Message::operator<<`. The field layout depends on negotiated features:
///
/// ```text
/// msgId          qint64
/// timestamp      qint64 ms (LongTime) | quint32 seconds
/// type           quint32
/// flags          quint8
/// bufferInfo     BufferInfo
/// sender         QByteArray UTF-8
/// senderPrefixes QByteArray UTF-8   (SenderPrefixes only)
/// realName       QByteArray UTF-8   (RichMessages only)
/// avatarUrl      QByteArray UTF-8   (RichMessages only)
/// contents       QByteArray UTF-8
/// ```
///
/// String fields decode leniently so one bad byte in a message body can't
/// take down the connection.
pub fn read_message(reader: &mut Reader<'_>) -> Result<Message, DecodeError> {
    let features = reader.features;
    let msg_id = MsgId(reader.read_i64()?);
    let timestamp = if features.contains(Features::LONG_TIME) {
        clamped_utc_from_millis(reader.read_i64()?)
    } else {
        clamped_utc_from_millis(i64::from(reader.read_u32()?) * 1000)
    };
    let kind = MessageType::from_wire(reader.read_u32()?);
    let flags = MessageFlags(reader.read_u8()?);
    let buffer_info = read_buffer_info(reader)?;
    let sender = utf8_lossy(reader.read_qbytearray()?);
    let sender_prefixes = if features.contains(Features::SENDER_PREFIXES) {
        utf8_lossy(reader.read_qbytearray()?)
    } else {
        String::new()
    };
    let (real_name, avatar_url) = if features.contains(Features::RICH_MESSAGES) {
        (
            utf8_lossy(reader.read_qbytearray()?),
            utf8_lossy(reader.read_qbytearray()?),
        )
    } else {
        (String::new(), String::new())
    };
    let contents = utf8_lossy(reader.read_qbytearray()?);
    Ok(Message {
        msg_id,
        timestamp,
        kind,
        flags,
        buffer_info,
        sender,
        sender_prefixes,
        real_name,
        avatar_url,
        contents,
    })
}

pub fn write_message(writer: &mut Writer, value: &Message) {
    let features = writer.features;
    writer.write_i64(value.msg_id.0);
    if features.contains(Features::LONG_TIME) {
        writer.write_i64(value.timestamp.timestamp_millis());
    } else {
        writer.write_u32(u32::try_from(value.timestamp.timestamp()).unwrap_or(0));
    }
    writer.write_u32(value.kind as u32);
    writer.write_u8(value.flags.0);
    write_buffer_info(writer, &value.buffer_info);
    writer.write_qbytearray(Some(value.sender.as_bytes()));
    if features.contains(Features::SENDER_PREFIXES) {
        writer.write_qbytearray(Some(value.sender_prefixes.as_bytes()));
    }
    if features.contains(Features::RICH_MESSAGES) {
        writer.write_qbytearray(Some(value.real_name.as_bytes()));
        writer.write_qbytearray(Some(value.avatar_url.as_bytes()));
    }
    writer.write_qbytearray(Some(value.contents.as_bytes()));
}

fn utf8_lossy(bytes: Option<Vec<u8>>) -> String {
    bytes
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default()
}

/// Timestamp from epoch milliseconds, clamped to 0001-01-01..9999-12-31.
///
/// A garbage timestamp is the classic symptom of a feature-negotiation
/// mismatch misaligning the read cursor. Clamping keeps the rest of the
/// message usable, and the absurd date stays visible in the UI.
pub fn clamped_utc_from_millis(ms: i64) -> DateTime<Utc> {
    let min = Utc.from_utc_datetime(
        &NaiveDate::from_ymd_opt(1, 1, 1)
            .expect("valid date")
            .and_hms_opt(0, 0, 0)
            .expect("valid time"),
    );
    let max = Utc.from_utc_datetime(
        &NaiveDate::from_ymd_opt(9999, 12, 31)
            .expect("valid date")
            .and_hms_milli_opt(23, 59, 59, 999)
            .expect("valid time"),
    );
    match DateTime::from_timestamp_millis(ms) {
        Some(ts) => ts.clamp(min, max),
        None if ms > 0 => max,
        None => min,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::qt::variant::{Variant, read_variant, write_variant};
    use chrono::Datelike;

    fn round_trip(value: &Variant, features: Features) -> Variant {
        let mut w = Writer::with_features(features);
        write_variant(&mut w, value);
        let bytes = w.into_bytes();
        let mut r = Reader::with_features(&bytes, features);
        let out = read_variant(&mut r).unwrap();
        assert!(r.at_end());
        out
    }

    fn sample_buffer() -> BufferInfo {
        BufferInfo {
            buffer_id: BufferId(1),
            network_id: NetworkId(1),
            kind: BufferType::Channel,
            group_id: 0,
            name: "#python".into(),
        }
    }

    fn sample_message() -> Message {
        Message {
            msg_id: MsgId(42),
            timestamp: Utc.with_ymd_and_hms(2026, 4, 14, 12, 34, 56).unwrap(),
            kind: MessageType::Plain,
            flags: MessageFlags::NONE,
            buffer_info: sample_buffer(),
            sender: "sean!sean@example.org".into(),
            sender_prefixes: "@".into(),
            real_name: "Sean R.".into(),
            avatar_url: String::new(),
            contents: "hello world".into(),
        }
    }

    const MODERN: Features = Features::client_default();

    #[test]
    fn int32_ids_round_trip() {
        for value in [
            UserValue::BufferId(BufferId(1)),
            UserValue::BufferId(BufferId(2_000_000_000)),
            UserValue::BufferId(BufferId(-1)),
            UserValue::NetworkId(NetworkId(7)),
            UserValue::IdentityId(IdentityId(99)),
            UserValue::UserId(UserId(3)),
            UserValue::AccountId(AccountId(4)),
        ] {
            let v = Variant::User(value);
            assert_eq!(round_trip(&v, Features::empty()), v);
        }
    }

    #[test]
    fn msg_id_uses_qint64() {
        let big = 1_000_000_000_000i64;
        let mut w = Writer::new();
        write_variant(&mut w, &Variant::User(UserValue::MsgId(MsgId(big))));
        let bytes = w.into_bytes();
        assert_eq!(&bytes[bytes.len() - 8..], &big.to_be_bytes());
        let decoded = read_variant(&mut Reader::new(&bytes)).unwrap();
        assert_eq!(decoded, Variant::User(UserValue::MsgId(MsgId(big))));
    }

    #[test]
    fn buffer_info_payload_byte_layout() {
        let buf = BufferInfo {
            buffer_id: BufferId(0x1122_3344),
            network_id: NetworkId(0x5566_7788),
            kind: BufferType::Query,
            group_id: 0xAABB_CCDD,
            name: "x".into(),
        };
        let mut w = Writer::new();
        write_buffer_info(&mut w, &buf);
        assert_eq!(
            w.as_bytes(),
            b"\x11\x22\x33\x44\x55\x66\x77\x88\x00\x04\xaa\xbb\xcc\xdd\x00\x00\x00\x01x"
        );
    }

    #[test]
    fn buffer_info_round_trips_with_unicode() {
        let mut buf = sample_buffer();
        buf.name = "#日本語".into();
        let v = Variant::User(UserValue::BufferInfo(buf));
        assert_eq!(round_trip(&v, Features::empty()), v);
    }

    #[test]
    fn buffer_info_unknown_type_coerced_to_invalid() {
        let mut w = Writer::new();
        w.write_i32(1);
        w.write_i32(1);
        w.write_i16(0x40);
        w.write_u32(0);
        w.write_qbytearray(Some(b"#future"));
        let decoded = read_buffer_info(&mut Reader::new(w.as_bytes())).unwrap();
        assert_eq!(decoded.kind, BufferType::Invalid);
        assert_eq!(decoded.name, "#future");
    }

    #[test]
    fn identity_round_trips_as_map() {
        let mut map = VariantMap::new();
        map.insert("identityName".into(), Variant::String("my-identity".into()));
        map.insert(
            "nicks".into(),
            Variant::StringList(vec!["sean".into(), "sean_".into()]),
        );
        map.insert("autoAwayEnabled".into(), Variant::Bool(false));
        let v = Variant::User(UserValue::Identity(map));
        assert_eq!(round_trip(&v, Features::empty()), v);
    }

    #[test]
    fn network_server_round_trips() {
        let mut map = VariantMap::new();
        map.insert("Host".into(), Variant::String("irc.example.com".into()));
        map.insert("Port".into(), Variant::Int(6697));
        map.insert("UseSSL".into(), Variant::Bool(true));
        let v = Variant::User(UserValue::NetworkServer(map));
        assert_eq!(round_trip(&v, Features::empty()), v);
    }

    #[test]
    fn modern_message_round_trip() {
        let v = Variant::User(UserValue::Message(Box::new(sample_message())));
        assert_eq!(round_trip(&v, MODERN), v);
    }

    #[test]
    fn legacy_message_omits_conditional_fields() {
        let mut msg = sample_message();
        msg.sender_prefixes.clear();
        msg.real_name.clear();
        let v = Variant::User(UserValue::Message(Box::new(msg)));
        assert_eq!(round_trip(&v, Features::empty()), v);
    }

    #[test]
    fn legacy_timestamp_is_quint32_seconds() {
        let mut msg = sample_message();
        msg.timestamp = Utc.with_ymd_and_hms(2000, 1, 1, 0, 0, 0).unwrap();
        let mut w = Writer::new();
        write_message(&mut w, &msg);
        let bytes = w.as_bytes();
        assert_eq!(i64::from_be_bytes(bytes[..8].try_into().unwrap()), 42);
        assert_eq!(
            u32::from_be_bytes(bytes[8..12].try_into().unwrap()),
            946_684_800
        );
    }

    #[test]
    fn modern_timestamp_is_qint64_ms() {
        let mut msg = sample_message();
        msg.timestamp = Utc.with_ymd_and_hms(2000, 1, 1, 0, 0, 0).unwrap();
        let mut w = Writer::with_features(Features::LONG_TIME);
        write_message(&mut w, &msg);
        let bytes = w.as_bytes();
        assert_eq!(
            i64::from_be_bytes(bytes[8..16].try_into().unwrap()),
            946_684_800_000
        );
    }

    #[test]
    fn unknown_message_type_is_forward_compatible() {
        let mut w = Writer::with_features(MODERN);
        write_message(&mut w, &sample_message());
        let mut bytes = w.into_bytes();
        bytes[16..20].copy_from_slice(&0xFFFFu32.to_be_bytes());
        let decoded = read_message(&mut Reader::with_features(&bytes, MODERN)).unwrap();
        assert_eq!(decoded.kind, MessageType::Plain);
    }

    fn message_with_timestamp(ms: i64) -> Message {
        let mut w = Writer::with_features(Features::LONG_TIME);
        w.write_i64(1);
        w.write_i64(ms);
        w.write_u32(1);
        w.write_u8(0);
        w.write_i32(10);
        w.write_i32(1);
        w.write_i16(2);
        w.write_u32(0);
        w.write_qbytearray(Some(b"#chan"));
        w.write_qbytearray(Some(b"nick!u@h"));
        w.write_qbytearray(Some(b"hello"));
        let bytes = w.into_bytes();
        read_message(&mut Reader::with_features(&bytes, Features::LONG_TIME)).unwrap()
    }

    #[test]
    fn absurd_timestamps_are_clamped() {
        let high = message_with_timestamp(1 << 62);
        assert_eq!(high.timestamp.year(), 9999);
        assert_eq!(high.contents, "hello");
        let low = message_with_timestamp(-(1 << 62));
        assert_eq!(low.timestamp.year(), 1);
    }

    #[test]
    fn trailing_nuls_are_normalized() {
        assert_eq!(normalize_name(b"NetworkId\0"), b"NetworkId");
        assert_eq!(normalize_name(b"NetworkId\0\0"), b"NetworkId");
        assert_eq!(normalize_name(b"NetworkId"), b"NetworkId");
        assert_eq!(normalize_name(b"\0"), b"");
    }
}
