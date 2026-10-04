//! QVariant read/write.
//!
//! A QVariant on the wire is `quint32 type_id`, `quint8 is_null`, then the
//! payload. Qt's `QVariant::save` writes the payload even when `is_null` is
//! set, so the decoder always consumes it; otherwise the rest of the
//! stream would desynchronize.
//!
//! Type ids are Quassel's `Types::VariantType` table, not raw Qt
//! `QMetaType` ids: Quassel pins `UserType = 127`, `Short = 130` and
//! `UShort = 133` regardless of Qt version.

use std::collections::BTreeMap;

use crate::protocol::usertypes::{UserValue, normalize_name};
use crate::qt::datastream::{DecodeError, QDateTime, Reader, Writer};

/// Wire type ids used in QVariant envelopes.
pub mod type_id {
    pub const INVALID: u32 = 0;
    pub const BOOL: u32 = 1;
    pub const INT: u32 = 2;
    pub const UINT: u32 = 3;
    pub const LONG_LONG: u32 = 4;
    pub const ULONG_LONG: u32 = 5;
    pub const DOUBLE: u32 = 6;
    pub const QCHAR: u32 = 7;
    pub const QVARIANT_MAP: u32 = 8;
    pub const QVARIANT_LIST: u32 = 9;
    pub const QSTRING: u32 = 10;
    pub const QSTRING_LIST: u32 = 11;
    pub const QBYTE_ARRAY: u32 = 12;
    pub const QDATE_TIME: u32 = 16;
    pub const USER_TYPE: u32 = 127;
    pub const SHORT: u32 = 130;
    pub const USHORT: u32 = 133;
}

pub type VariantMap = BTreeMap<String, Variant>;

/// A decoded QVariant.
///
/// `Null` covers both the Invalid variant and typed nulls of scalar types
/// (including null QStrings and QByteArrays); callers never need to know
/// which type a null value had. Containers and user types ignore the null
/// flag, because some cores set it on lists that carry real data (backlog
/// replies, notably) and real Quassel clients use the payload regardless.
#[derive(Debug, Clone, PartialEq)]
pub enum Variant {
    Null,
    Bool(bool),
    Int(i32),
    UInt(u32),
    LongLong(i64),
    ULongLong(u64),
    Double(f64),
    /// One UTF-16 code unit. Cores send these in channel mode change syncs.
    Char(u16),
    String(String),
    ByteArray(Vec<u8>),
    DateTime(QDateTime),
    Map(VariantMap),
    List(Vec<Variant>),
    StringList(Vec<String>),
    Short(i16),
    UShort(u16),
    User(UserValue),
}

impl Variant {
    /// The integer value of any integer-typed variant (not `Bool`).
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Self::Int(v) => Some(i64::from(*v)),
            Self::UInt(v) => Some(i64::from(*v)),
            Self::LongLong(v) => Some(*v),
            Self::ULongLong(v) => i64::try_from(*v).ok(),
            Self::Short(v) => Some(i64::from(*v)),
            Self::UShort(v) => Some(i64::from(*v)),
            _ => None,
        }
    }

    /// Lenient integer coercion for sync-slot parameters: integers, the
    /// id-shaped user types, decimal strings, and truncated doubles.
    pub fn coerce_i64(&self) -> Option<i64> {
        match self {
            Self::Bool(_) => None,
            Self::User(user) => user.as_id(),
            Self::String(s) => s.trim().parse().ok(),
            Self::Double(d) if d.is_finite() => Some(d.trunc() as i64),
            other => other.as_i64(),
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(s) => Some(s),
            _ => None,
        }
    }

    /// Lenient string coercion for sync-slot parameters. `None` for null.
    pub fn coerce_string(&self) -> Option<String> {
        match self {
            Self::Null => None,
            Self::String(s) => Some(s.clone()),
            Self::Char(c) => Some(String::from_utf16_lossy(&[*c])),
            Self::ByteArray(b) => Some(String::from_utf8_lossy(b).into_owned()),
            Self::Bool(b) => Some(if *b { "True" } else { "False" }.to_string()),
            Self::Double(d) => Some(d.to_string()),
            Self::User(user) => user.as_id().map(|id| id.to_string()),
            other => other.as_i64().map(|i| i.to_string()),
        }
    }

    /// Truthiness, as the Python client's `bool(value)` applied it.
    pub fn truthy(&self) -> bool {
        match self {
            Self::Null => false,
            Self::Bool(b) => *b,
            Self::Double(d) => *d != 0.0,
            Self::Char(c) => *c != 0,
            Self::String(s) => !s.is_empty(),
            Self::ByteArray(b) => !b.is_empty(),
            Self::Map(m) => !m.is_empty(),
            Self::List(l) => !l.is_empty(),
            Self::StringList(l) => !l.is_empty(),
            Self::DateTime(_) | Self::User(_) => true,
            other => other.as_i64().is_some_and(|i| i != 0),
        }
    }

    /// Items of a `List` or `StringList`, as variants.
    pub fn list_items(&self) -> Option<Vec<Variant>> {
        match self {
            Self::List(items) => Some(items.clone()),
            Self::StringList(items) => Some(items.iter().cloned().map(Self::String).collect()),
            _ => None,
        }
    }

    pub fn as_map(&self) -> Option<&VariantMap> {
        match self {
            Self::Map(m) => Some(m),
            _ => None,
        }
    }

    /// Short type name for diagnostics.
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::Null => "null",
            Self::Bool(_) => "bool",
            Self::Int(_) => "int",
            Self::UInt(_) => "uint",
            Self::LongLong(_) => "qlonglong",
            Self::ULongLong(_) => "qulonglong",
            Self::Double(_) => "double",
            Self::Char(_) => "QChar",
            Self::String(_) => "QString",
            Self::ByteArray(_) => "QByteArray",
            Self::DateTime(_) => "QDateTime",
            Self::Map(_) => "QVariantMap",
            Self::List(_) => "QVariantList",
            Self::StringList(_) => "QStringList",
            Self::Short(_) => "short",
            Self::UShort(_) => "ushort",
            Self::User(user) => match user {
                UserValue::BufferId(_) => "BufferId",
                UserValue::NetworkId(_) => "NetworkId",
                UserValue::IdentityId(_) => "IdentityId",
                UserValue::UserId(_) => "UserId",
                UserValue::AccountId(_) => "AccountId",
                UserValue::MsgId(_) => "MsgId",
                UserValue::BufferInfo(_) => "BufferInfo",
                UserValue::Message(_) => "Message",
                UserValue::Identity(_) => "Identity",
                UserValue::NetworkServer(_) => "Network::Server",
            },
        }
    }

    fn wire_type_id(&self) -> u32 {
        match self {
            Self::Null => type_id::INVALID,
            Self::Bool(_) => type_id::BOOL,
            Self::Int(_) => type_id::INT,
            Self::UInt(_) => type_id::UINT,
            Self::LongLong(_) => type_id::LONG_LONG,
            Self::ULongLong(_) => type_id::ULONG_LONG,
            Self::Double(_) => type_id::DOUBLE,
            Self::Char(_) => type_id::QCHAR,
            Self::String(_) => type_id::QSTRING,
            Self::ByteArray(_) => type_id::QBYTE_ARRAY,
            Self::DateTime(_) => type_id::QDATE_TIME,
            Self::Map(_) => type_id::QVARIANT_MAP,
            Self::List(_) => type_id::QVARIANT_LIST,
            Self::StringList(_) => type_id::QSTRING_LIST,
            Self::Short(_) => type_id::SHORT,
            Self::UShort(_) => type_id::USHORT,
            Self::User(_) => type_id::USER_TYPE,
        }
    }
}

impl From<&str> for Variant {
    fn from(value: &str) -> Self {
        Self::String(value.to_string())
    }
}

impl From<String> for Variant {
    fn from(value: String) -> Self {
        Self::String(value)
    }
}

impl From<bool> for Variant {
    fn from(value: bool) -> Self {
        Self::Bool(value)
    }
}

impl From<i32> for Variant {
    fn from(value: i32) -> Self {
        Self::Int(value)
    }
}

impl From<UserValue> for Variant {
    fn from(value: UserValue) -> Self {
        Self::User(value)
    }
}

/// Decode one QVariant envelope.
pub fn read_variant(reader: &mut Reader<'_>) -> Result<Variant, DecodeError> {
    reader.push_nesting()?;
    let result = read_variant_inner(reader);
    reader.pop_nesting();
    result
}

fn read_variant_inner(reader: &mut Reader<'_>) -> Result<Variant, DecodeError> {
    let type_id = reader.read_u32()?;
    let is_null = reader.read_u8()? != 0;
    let value = match type_id {
        type_id::INVALID => return Ok(Variant::Null),
        type_id::BOOL => Variant::Bool(reader.read_bool()?),
        type_id::INT => Variant::Int(reader.read_i32()?),
        type_id::UINT => Variant::UInt(reader.read_u32()?),
        type_id::LONG_LONG => Variant::LongLong(reader.read_i64()?),
        type_id::ULONG_LONG => Variant::ULongLong(reader.read_u64()?),
        type_id::DOUBLE => Variant::Double(reader.read_f64()?),
        type_id::QCHAR => Variant::Char(reader.read_u16()?),
        type_id::QSTRING => reader
            .read_qstring()?
            .map_or(Variant::Null, Variant::String),
        type_id::QBYTE_ARRAY => reader
            .read_qbytearray()?
            .map_or(Variant::Null, Variant::ByteArray),
        type_id::QDATE_TIME => Variant::DateTime(reader.read_qdatetime()?),
        type_id::SHORT => Variant::Short(reader.read_i16()?),
        type_id::USHORT => Variant::UShort(reader.read_u16()?),
        // Containers and user types ignore the null flag (see `Variant`).
        type_id::QVARIANT_MAP => return Ok(Variant::Map(read_qvariantmap(reader)?)),
        type_id::QVARIANT_LIST => return Ok(Variant::List(read_qvariantlist(reader)?)),
        type_id::QSTRING_LIST => return Ok(Variant::StringList(read_qstringlist(reader)?)),
        type_id::USER_TYPE => {
            let Some(raw_name) = reader.read_qbytearray()? else {
                return Err(DecodeError(format!(
                    "QVariant<UserType> name is null at offset {}",
                    reader.position()
                )));
            };
            let name = normalize_name(&raw_name);
            return Ok(Variant::User(UserValue::read_payload(reader, name)?));
        }
        other => {
            return Err(DecodeError(format!(
                "unsupported QVariant type id {other} at offset {}",
                reader.position()
            )));
        }
    };
    Ok(if is_null { Variant::Null } else { value })
}

/// Encode one QVariant envelope. `Null` is written as the Invalid variant.
pub fn write_variant(writer: &mut Writer, value: &Variant) {
    writer.write_u32(value.wire_type_id());
    writer.write_u8(u8::from(matches!(value, Variant::Null)));
    match value {
        Variant::Null => {}
        Variant::Bool(v) => writer.write_bool(*v),
        Variant::Int(v) => writer.write_i32(*v),
        Variant::UInt(v) => writer.write_u32(*v),
        Variant::LongLong(v) => writer.write_i64(*v),
        Variant::ULongLong(v) => writer.write_u64(*v),
        Variant::Double(v) => writer.write_f64(*v),
        Variant::Char(v) => writer.write_u16(*v),
        Variant::String(v) => writer.write_qstring(Some(v)),
        Variant::ByteArray(v) => writer.write_qbytearray(Some(v)),
        Variant::DateTime(v) => writer.write_qdatetime(v),
        Variant::Map(v) => write_qvariantmap(writer, v),
        Variant::List(v) => write_qvariantlist(writer, v),
        Variant::StringList(v) => write_qstringlist(writer, v),
        Variant::Short(v) => writer.write_i16(*v),
        Variant::UShort(v) => writer.write_u16(*v),
        Variant::User(user) => {
            writer.write_qbytearray(Some(user.type_name()));
            user.write_payload(writer);
        }
    }
}

pub fn read_qvariantlist(reader: &mut Reader<'_>) -> Result<Vec<Variant>, DecodeError> {
    let count = reader.read_u32()?;
    let count = reader.check_count(count, "QVariantList")?;
    // Don't trust `count` for preallocation beyond what the buffer could
    // possibly hold (every variant is at least 5 bytes).
    let mut out = Vec::with_capacity(count.min(reader.remaining() / 5));
    for _ in 0..count {
        out.push(read_variant(reader)?);
    }
    Ok(out)
}

pub fn write_qvariantlist(writer: &mut Writer, items: &[Variant]) {
    writer.write_u32(items.len() as u32);
    for item in items {
        write_variant(writer, item);
    }
}

/// Read a QVariantMap. A null key is coerced to `""`: Qt treats null and
/// empty QStrings alike, and failing would discard the whole frame over one
/// degenerate key.
pub fn read_qvariantmap(reader: &mut Reader<'_>) -> Result<VariantMap, DecodeError> {
    let count = reader.read_u32()?;
    let count = reader.check_count(count, "QVariantMap")?;
    let mut out = VariantMap::new();
    for _ in 0..count {
        let key = reader.read_qstring()?.unwrap_or_default();
        let value = read_variant(reader)?;
        out.insert(key, value);
    }
    Ok(out)
}

/// Write a QVariantMap in sorted key order, which is Qt's QMap order.
pub fn write_qvariantmap(writer: &mut Writer, map: &VariantMap) {
    writer.write_u32(map.len() as u32);
    for (key, value) in map {
        writer.write_qstring(Some(key));
        write_variant(writer, value);
    }
}

/// Read a QStringList. Null elements (which some cores send) become `""`.
pub fn read_qstringlist(reader: &mut Reader<'_>) -> Result<Vec<String>, DecodeError> {
    let count = reader.read_u32()?;
    let count = reader.check_count(count, "QStringList")?;
    let mut out = Vec::with_capacity(count.min(reader.remaining() / 4));
    for _ in 0..count {
        out.push(reader.read_qstring()?.unwrap_or_default());
    }
    Ok(out)
}

pub fn write_qstringlist(writer: &mut Writer, items: &[String]) {
    writer.write_u32(items.len() as u32);
    for item in items {
        writer.write_qstring(Some(item));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::features::Features;
    use crate::protocol::types::{BufferId, BufferInfo, BufferType, NetworkId};
    use crate::qt::datastream::Limits;

    fn round_trip(value: &Variant) -> Variant {
        let mut w = Writer::new();
        write_variant(&mut w, value);
        let bytes = w.into_bytes();
        let mut r = Reader::new(&bytes);
        let out = read_variant(&mut r).unwrap();
        assert!(r.at_end());
        out
    }

    fn map(entries: &[(&str, Variant)]) -> VariantMap {
        entries
            .iter()
            .map(|(k, v)| ((*k).to_string(), v.clone()))
            .collect()
    }

    #[test]
    fn primitives_round_trip() {
        for value in [
            Variant::Bool(true),
            Variant::Bool(false),
            Variant::Int(0),
            Variant::Int(-1),
            Variant::Int(2_000_000_000),
            Variant::UInt(4_000_000_000),
            Variant::LongLong(1_000_000_000_000),
            Variant::ULongLong(1_000_000_000_000_000_000),
            Variant::String("hello".into()),
            Variant::String(String::new()),
            Variant::String("日本語".into()),
            Variant::ByteArray(vec![0, 1, 2]),
            Variant::ByteArray(vec![]),
            Variant::Double(2.5),
            Variant::Char(u16::from(b'm')),
            Variant::Short(-1),
            Variant::Short(42),
            Variant::UShort(443),
        ] {
            assert_eq!(round_trip(&value), value);
        }
    }

    #[test]
    fn null_serializes_as_invalid_variant() {
        let mut w = Writer::new();
        write_variant(&mut w, &Variant::Null);
        assert_eq!(w.as_bytes(), b"\x00\x00\x00\x00\x01");
        assert_eq!(
            read_variant(&mut Reader::new(w.as_bytes())).unwrap(),
            Variant::Null
        );
    }

    #[test]
    fn bool_wire_type_is_bool() {
        let mut w = Writer::new();
        write_variant(&mut w, &Variant::Bool(true));
        assert_eq!(&w.as_bytes()[..4], b"\x00\x00\x00\x01");
    }

    #[test]
    fn unsupported_type_is_rejected() {
        let err = read_variant(&mut Reader::new(b"\x00\x00\x00\xff\x00")).unwrap_err();
        assert!(err.0.contains("unsupported QVariant type"), "{err}");
    }

    #[test]
    fn empty_containers() {
        let mut w = Writer::new();
        write_qvariantlist(&mut w, &[]);
        assert_eq!(w.as_bytes(), b"\x00\x00\x00\x00");
        assert!(
            read_qvariantlist(&mut Reader::new(w.as_bytes()))
                .unwrap()
                .is_empty()
        );

        let mut w = Writer::new();
        write_qvariantmap(&mut w, &VariantMap::new());
        assert_eq!(w.as_bytes(), b"\x00\x00\x00\x00");
        assert!(
            read_qvariantmap(&mut Reader::new(w.as_bytes()))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn mixed_list_round_trips() {
        let value = Variant::List(vec![
            Variant::Bool(true),
            Variant::Int(42),
            Variant::String("hello".into()),
            Variant::ByteArray(b"bytes".to_vec()),
            Variant::List(vec![Variant::Int(1), Variant::Int(2), Variant::Int(3)]),
            Variant::Map(map(&[("k", Variant::String("v".into()))])),
        ]);
        assert_eq!(round_trip(&value), value);
    }

    #[test]
    fn client_init_shaped_and_nested_maps_round_trip() {
        let value = Variant::Map(map(&[
            ("MsgType", "ClientInit".into()),
            ("ClientVersion", "quasseltui v0.0.0".into()),
            ("Features", Variant::UInt(0xC03F)),
            (
                "FeatureList",
                Variant::StringList(vec!["SynchronizedMarkerLine".into()]),
            ),
            (
                "outer",
                Variant::Map(map(&[(
                    "inner",
                    Variant::Map(map(&[("leaf", "value".into()), ("n", Variant::Int(7))])),
                )])),
            ),
        ]));
        assert_eq!(round_trip(&value), value);
    }

    #[test]
    fn typed_null_payloads_are_consumed() {
        // Null QString followed by Int(7).
        let blob = b"\x00\x00\x00\x0a\x01\xff\xff\xff\xff\x00\x00\x00\x02\x00\x00\x00\x00\x07";
        let mut r = Reader::new(blob);
        assert_eq!(read_variant(&mut r).unwrap(), Variant::Null);
        assert_eq!(read_variant(&mut r).unwrap(), Variant::Int(7));
        assert!(r.at_end());

        // Null Int (4-byte payload still present) followed by QString "H".
        let blob = b"\x00\x00\x00\x02\x01\x00\x00\x00\x00\x00\x00\x00\x0a\x00\x00\x00\x00\x02\x00H";
        let mut r = Reader::new(blob);
        assert_eq!(read_variant(&mut r).unwrap(), Variant::Null);
        assert_eq!(read_variant(&mut r).unwrap(), Variant::String("H".into()));
        assert!(r.at_end());

        // Null QByteArray followed by Bool(true).
        let blob = b"\x00\x00\x00\x0c\x01\xff\xff\xff\xff\x00\x00\x00\x01\x00\x01";
        let mut r = Reader::new(blob);
        assert_eq!(read_variant(&mut r).unwrap(), Variant::Null);
        assert_eq!(read_variant(&mut r).unwrap(), Variant::Bool(true));
        assert!(r.at_end());

        // Invalid variant has no payload.
        let blob = b"\x00\x00\x00\x00\x01\x00\x00\x00\x02\x00\x00\x00\x00\x09";
        let mut r = Reader::new(blob);
        assert_eq!(read_variant(&mut r).unwrap(), Variant::Null);
        assert_eq!(read_variant(&mut r).unwrap(), Variant::Int(9));
    }

    #[test]
    fn container_limits() {
        let limits = Limits {
            max_container_items: 5,
            ..Limits::default()
        };
        let blob = b"\x00\x00\x00\x64";
        let err =
            read_qvariantlist(&mut Reader::with(blob, limits, Features::empty())).unwrap_err();
        assert!(err.0.contains("QVariantList count") && err.0.contains("exceeds"));
        let err = read_qvariantmap(&mut Reader::with(blob, limits, Features::empty())).unwrap_err();
        assert!(err.0.contains("QVariantMap count"));
        let err = read_qstringlist(&mut Reader::with(blob, limits, Features::empty())).unwrap_err();
        assert!(err.0.contains("QStringList count"));

        let err = read_qvariantlist(&mut Reader::new(b"\xff\xff\xff\x00")).unwrap_err();
        assert!(err.0.contains("exceeds max_container_items"));

        let mut w = Writer::new();
        write_qvariantlist(&mut w, &[Variant::Int(1), Variant::Int(2), Variant::Int(3)]);
        let at_limit = Limits {
            max_container_items: 3,
            ..Limits::default()
        };
        assert_eq!(
            read_qvariantlist(&mut Reader::with(w.as_bytes(), at_limit, Features::empty()))
                .unwrap()
                .len(),
            3
        );
    }

    #[test]
    fn user_type_envelope_byte_layout() {
        let mut w = Writer::new();
        write_variant(&mut w, &Variant::User(UserValue::BufferId(BufferId(3))));
        let blob = w.as_bytes();
        assert_eq!(&blob[..4], b"\x00\x00\x00\x7f");
        assert_eq!(blob[4], 0);
        assert_eq!(&blob[5..9], b"\x00\x00\x00\x08");
        assert_eq!(&blob[9..17], b"BufferId");
        assert_eq!(&blob[17..], b"\x00\x00\x00\x03");
    }

    #[test]
    fn user_type_name_with_trailing_nul_is_normalized() {
        let mut w = Writer::new();
        w.write_u32(127);
        w.write_u8(0);
        w.write_qbytearray(Some(b"NetworkId\0"));
        w.write_i32(11);
        let mut r = Reader::new(w.as_bytes());
        assert_eq!(
            read_variant(&mut r).unwrap(),
            Variant::User(UserValue::NetworkId(NetworkId(11)))
        );
        assert!(r.at_end());
    }

    #[test]
    fn unregistered_user_type_is_rejected() {
        let mut w = Writer::new();
        w.write_u32(127);
        w.write_u8(0);
        w.write_qbytearray(Some(b"Mystery"));
        w.write_u32(0);
        let err = read_variant(&mut Reader::new(w.as_bytes())).unwrap_err();
        assert!(
            err.0.contains("unsupported QVariant<UserType> name"),
            "{err}"
        );
    }

    #[test]
    fn user_type_with_null_flag_keeps_payload() {
        let mut w = Writer::new();
        w.write_u32(127);
        w.write_u8(1);
        w.write_qbytearray(Some(b"BufferId"));
        w.write_i32(3);
        write_variant(&mut w, &Variant::Int(42));
        let mut r = Reader::new(w.as_bytes());
        assert_eq!(
            read_variant(&mut r).unwrap(),
            Variant::User(UserValue::BufferId(BufferId(3)))
        );
        assert_eq!(read_variant(&mut r).unwrap(), Variant::Int(42));
        assert!(r.at_end());
    }

    #[test]
    fn short_and_ushort_wire_ids() {
        let mut w = Writer::new();
        write_variant(&mut w, &Variant::Short(1));
        assert_eq!(w.as_bytes(), b"\x00\x00\x00\x82\x00\x00\x01");
        let mut w = Writer::new();
        write_variant(&mut w, &Variant::UShort(0));
        assert_eq!(&w.as_bytes()[..4], b"\x00\x00\x00\x85");
    }

    #[test]
    fn qdatetime_wire_id_and_round_trip() {
        let value = Variant::DateTime(QDateTime {
            julian_day: 2_461_145,
            ms_of_day: 45_296_789,
            is_utc: true,
        });
        let mut w = Writer::new();
        write_variant(&mut w, &value);
        assert_eq!(&w.as_bytes()[..4], b"\x00\x00\x00\x10");
        assert_eq!(round_trip(&value), value);
    }

    #[test]
    fn double_payload_is_ieee754() {
        let mut w = Writer::new();
        write_variant(&mut w, &Variant::Double(1.0));
        assert_eq!(
            w.as_bytes(),
            b"\x00\x00\x00\x06\x00\x3f\xf0\x00\x00\x00\x00\x00\x00"
        );
    }

    #[test]
    fn qchar_decodes_from_core_wire_bytes() {
        let mut r = Reader::new(b"\x00\x00\x00\x07\x00\x00\x6d");
        let value = read_variant(&mut r).unwrap();
        assert_eq!(value, Variant::Char(0x6d));
        assert_eq!(value.coerce_string().as_deref(), Some("m"));
        assert!(r.at_end());
    }

    #[test]
    fn legacy_core_compat() {
        // Null element inside a QStringList.
        let mut w = Writer::new();
        w.write_u32(3);
        w.write_qstring(Some("hello"));
        w.write_qstring(None);
        w.write_qstring(Some("world"));
        assert_eq!(
            read_qstringlist(&mut Reader::new(w.as_bytes())).unwrap(),
            ["hello", "", "world"]
        );

        // is_null=1 on a QVariantList with content.
        let mut w = Writer::new();
        w.write_u32(type_id::QVARIANT_LIST);
        w.write_u8(1);
        w.write_u32(1);
        write_variant(&mut w, &Variant::Int(42));
        assert_eq!(
            read_variant(&mut Reader::new(w.as_bytes())).unwrap(),
            Variant::List(vec![Variant::Int(42)])
        );

        // is_null=1 on a QVariantMap with content.
        let mut w = Writer::new();
        w.write_u32(type_id::QVARIANT_MAP);
        w.write_u8(1);
        w.write_u32(1);
        w.write_qstring(Some("key"));
        write_variant(&mut w, &Variant::String("val".into()));
        assert_eq!(
            read_variant(&mut Reader::new(w.as_bytes())).unwrap(),
            Variant::Map(map(&[("key", "val".into())]))
        );

        // is_null=1 on a scalar still yields Null.
        let mut w = Writer::new();
        w.write_u32(type_id::INT);
        w.write_u8(1);
        w.write_i32(0);
        assert_eq!(
            read_variant(&mut Reader::new(w.as_bytes())).unwrap(),
            Variant::Null
        );
    }

    #[test]
    fn null_map_key_is_coerced_to_empty() {
        let mut w = Writer::new();
        w.write_u32(1);
        w.write_qstring(None);
        write_variant(&mut w, &Variant::String("val".into()));
        assert_eq!(
            read_qvariantmap(&mut Reader::new(w.as_bytes())).unwrap(),
            map(&[("", "val".into())])
        );
    }

    #[test]
    fn deeply_nested_containers_fail_cleanly() {
        let mut payload = b"\x00\x00\x00\x00".to_vec();
        for _ in 0..1000 {
            let mut wrapped = b"\x00\x00\x00\x01\x00\x00\x00\x09\x00".to_vec();
            wrapped.extend_from_slice(&payload);
            payload = wrapped;
        }
        let mut blob = b"\x00\x00\x00\x09\x00".to_vec();
        blob.extend_from_slice(&payload);
        let err = read_variant(&mut Reader::new(&blob)).unwrap_err();
        assert!(err.0.contains("nest"), "{err}");
    }

    #[test]
    fn list_of_user_types() {
        let ids = vec![
            Variant::User(UserValue::NetworkId(NetworkId(1))),
            Variant::User(UserValue::NetworkId(NetworkId(2))),
            Variant::User(UserValue::NetworkId(NetworkId(99))),
        ];
        let mut w = Writer::new();
        write_qvariantlist(&mut w, &ids);
        assert_eq!(
            read_qvariantlist(&mut Reader::new(w.as_bytes())).unwrap(),
            ids
        );

        let buf = Variant::User(UserValue::BufferInfo(BufferInfo {
            buffer_id: BufferId(42),
            network_id: NetworkId(1),
            kind: BufferType::Channel,
            group_id: 0,
            name: "#python".into(),
        }));
        assert_eq!(round_trip(&buf), buf);
    }

    #[test]
    fn coercions() {
        assert_eq!(Variant::Short(5).as_i64(), Some(5));
        assert_eq!(Variant::Bool(true).as_i64(), None);
        assert_eq!(Variant::String(" 12 ".into()).coerce_i64(), Some(12));
        assert_eq!(
            Variant::User(UserValue::BufferId(BufferId(7))).coerce_i64(),
            Some(7)
        );
        assert!(Variant::Int(3).truthy());
        assert!(!Variant::Int(0).truthy());
        assert!(!Variant::Null.truthy());
        assert_eq!(Variant::Null.coerce_string(), None);
        assert_eq!(Variant::Int(5).coerce_string().as_deref(), Some("5"));
    }
}
