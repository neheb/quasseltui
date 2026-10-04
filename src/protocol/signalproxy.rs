//! SignalProxy messages: the framing of the connected state.
//!
//! Every frame after the handshake is a `QVariantList` whose first element
//! is a `qint16` discriminator, sent as `QVariant<Short>` (Quassel type id
//! 130, not `Int`). The layouts come from
//! `datastreampeer.cpp::handlePackedFunc` and the `dispatch()` overloads.
//!
//! A frame that fails to decode raises an error. Recovery is the
//! connection's job: frames are length-prefixed, so a bad payload can be
//! skipped without desynchronizing the stream.

use crate::protocol::error::{Error, Result};
use crate::protocol::features::Features;
use crate::qt::datastream::{QDateTime, Reader, Writer};
use crate::qt::variant::{Variant, VariantMap, read_qvariantlist, write_qvariantlist};

pub const REQUEST_SYNC: i16 = 1;
pub const REQUEST_RPC_CALL: i16 = 2;
pub const REQUEST_INIT_REQUEST: i16 = 3;
pub const REQUEST_INIT_DATA: i16 = 4;
pub const REQUEST_HEARTBEAT: i16 = 5;
pub const REQUEST_HEARTBEAT_REPLY: i16 = 6;

/// A slot invocation on a SyncObject, addressed by `(class_name,
/// object_name)`. Class and slot names are raw bytes because they travel
/// as QByteArray; the object name is `objectName.toUtf8()`.
#[derive(Debug, Clone, PartialEq)]
pub struct SyncMessage {
    pub class_name: Vec<u8>,
    pub object_name: String,
    pub slot_name: Vec<u8>,
    pub params: Vec<Variant>,
}

/// A top-level signal, e.g. `2displayMsg(Message)`. The leading `2` is
/// Qt's signal marker.
#[derive(Debug, Clone, PartialEq)]
pub struct RpcCall {
    pub signal_name: Vec<u8>,
    pub params: Vec<Variant>,
}

/// Request for the full initial state of `(class_name, object_name)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InitRequest {
    pub class_name: Vec<u8>,
    pub object_name: String,
}

/// Reply to an `InitRequest`: a flat property map.
#[derive(Debug, Clone, PartialEq)]
pub struct InitData {
    pub class_name: Vec<u8>,
    pub object_name: String,
    pub init_data: VariantMap,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SignalProxyMessage {
    Sync(SyncMessage),
    RpcCall(RpcCall),
    InitRequest(InitRequest),
    InitData(InitData),
    /// Keepalive. Reply with `HeartBeatReply` carrying the same timestamp.
    HeartBeat(QDateTime),
    HeartBeatReply(QDateTime),
}

fn sp_err(msg: String) -> Error {
    Error::SignalProxy(msg)
}

/// Decode one connected-state frame payload.
///
/// Bytes left over after the top-level list are an error: framing keeps
/// the stream in sync regardless, but silently dropping the tail would hide
/// payload corruption.
pub fn decode_signalproxy_payload(
    payload: &[u8],
    features: Features,
) -> Result<SignalProxyMessage> {
    let mut reader = Reader::with_features(payload, features);
    let items = read_qvariantlist(&mut reader)?;
    if !reader.at_end() {
        return Err(sp_err(format!(
            "trailing {} bytes after SignalProxy QVariantList",
            reader.remaining()
        )));
    }
    let mut items = items.into_iter();
    let Some(first) = items.next() else {
        return Err(sp_err(
            "empty SignalProxy frame (zero-element QVariantList)".into(),
        ));
    };
    let Some(discriminator) = first.as_i64() else {
        return Err(sp_err(format!(
            "first SignalProxy element must be an int discriminator, got {}",
            first.type_name()
        )));
    };
    let rest: Vec<Variant> = items.collect();
    match i16::try_from(discriminator) {
        Ok(REQUEST_SYNC) => decode_sync(rest),
        Ok(REQUEST_RPC_CALL) => decode_rpc_call(rest),
        Ok(REQUEST_INIT_REQUEST) => decode_init_request(rest),
        Ok(REQUEST_INIT_DATA) => decode_init_data(rest),
        Ok(REQUEST_HEARTBEAT) => decode_heartbeat(rest, false),
        Ok(REQUEST_HEARTBEAT_REPLY) => decode_heartbeat(rest, true),
        _ => Err(sp_err(format!(
            "unknown SignalProxy discriminator {discriminator}"
        ))),
    }
}

fn expect_bytes(value: Variant, field: &str, kind: &str) -> Result<Vec<u8>> {
    match value {
        Variant::ByteArray(bytes) => Ok(bytes),
        other => Err(sp_err(format!(
            "{kind}: expected QByteArray for {field}, got {}",
            other.type_name()
        ))),
    }
}

/// `QString::fromUtf8(objectName)`. A null QByteArray is `""`: some cores
/// encode singleton object names (BufferSyncer's) as null.
fn object_name(value: Variant, kind: &str) -> Result<String> {
    match value {
        Variant::Null => Ok(String::new()),
        other => {
            Ok(String::from_utf8_lossy(&expect_bytes(other, "objectName", kind)?).into_owned())
        }
    }
}

fn decode_sync(rest: Vec<Variant>) -> Result<SignalProxyMessage> {
    if rest.len() < 3 {
        return Err(sp_err(format!(
            "Sync: needs at least className/objectName/slotName, got {} items",
            rest.len()
        )));
    }
    let mut iter = rest.into_iter();
    let class_name = expect_bytes(iter.next().expect("len checked"), "className", "Sync")?;
    let object_name = object_name(iter.next().expect("len checked"), "Sync")?;
    let slot_name = expect_bytes(iter.next().expect("len checked"), "slotName", "Sync")?;
    Ok(SignalProxyMessage::Sync(SyncMessage {
        class_name,
        object_name,
        slot_name,
        params: iter.collect(),
    }))
}

fn decode_rpc_call(rest: Vec<Variant>) -> Result<SignalProxyMessage> {
    let mut iter = rest.into_iter();
    let Some(first) = iter.next() else {
        return Err(sp_err("RpcCall: needs at least a signalName".into()));
    };
    Ok(SignalProxyMessage::RpcCall(RpcCall {
        signal_name: expect_bytes(first, "signalName", "RpcCall")?,
        params: iter.collect(),
    }))
}

fn decode_init_request(rest: Vec<Variant>) -> Result<SignalProxyMessage> {
    if rest.len() != 2 {
        return Err(sp_err(format!(
            "InitRequest: needs exactly className/objectName, got {} items",
            rest.len()
        )));
    }
    let mut iter = rest.into_iter();
    let class_name = expect_bytes(
        iter.next().expect("len checked"),
        "className",
        "InitRequest",
    )?;
    let object_name = object_name(iter.next().expect("len checked"), "InitRequest")?;
    Ok(SignalProxyMessage::InitRequest(InitRequest {
        class_name,
        object_name,
    }))
}

fn decode_init_data(rest: Vec<Variant>) -> Result<SignalProxyMessage> {
    if rest.len() < 2 {
        return Err(sp_err(format!(
            "InitData: needs at least className/objectName, got {} items",
            rest.len()
        )));
    }
    let pairs = rest.len() - 2;
    if !pairs.is_multiple_of(2) {
        return Err(sp_err(format!(
            "InitData: trailing key/value pairs must be even, got {pairs} items"
        )));
    }
    let mut iter = rest.into_iter();
    let class_name = expect_bytes(iter.next().expect("len checked"), "className", "InitData")?;
    let object_name = object_name(iter.next().expect("len checked"), "InitData")?;
    let mut init_data = VariantMap::new();
    let mut index = 0;
    while let (Some(key), Some(value)) = (iter.next(), iter.next()) {
        let key = expect_bytes(key, &format!("key#{index}"), "InitData")?;
        init_data.insert(String::from_utf8_lossy(&key).into_owned(), value);
        index += 1;
    }
    Ok(SignalProxyMessage::InitData(InitData {
        class_name,
        object_name,
        init_data,
    }))
}

fn decode_heartbeat(rest: Vec<Variant>, reply: bool) -> Result<SignalProxyMessage> {
    let kind = if reply { "HeartBeatReply" } else { "HeartBeat" };
    if rest.len() != 1 {
        return Err(sp_err(format!(
            "{kind}: needs exactly a timestamp, got {} items",
            rest.len()
        )));
    }
    match rest.into_iter().next().expect("len checked") {
        Variant::DateTime(ts) if reply => Ok(SignalProxyMessage::HeartBeatReply(ts)),
        Variant::DateTime(ts) => Ok(SignalProxyMessage::HeartBeat(ts)),
        other => Err(sp_err(format!(
            "{kind}: expected QDateTime timestamp, got {}",
            other.type_name()
        ))),
    }
}

/// Encode a SignalProxy message into a frame payload (no length prefix).
pub fn encode_signalproxy_payload(message: &SignalProxyMessage, features: Features) -> Vec<u8> {
    let bytes = |b: &[u8]| Variant::ByteArray(b.to_vec());
    let items: Vec<Variant> = match message {
        SignalProxyMessage::Sync(m) => {
            let mut items = vec![
                Variant::Short(REQUEST_SYNC),
                bytes(&m.class_name),
                bytes(m.object_name.as_bytes()),
                bytes(&m.slot_name),
            ];
            items.extend(m.params.iter().cloned());
            items
        }
        SignalProxyMessage::RpcCall(m) => {
            let mut items = vec![Variant::Short(REQUEST_RPC_CALL), bytes(&m.signal_name)];
            items.extend(m.params.iter().cloned());
            items
        }
        SignalProxyMessage::InitRequest(m) => vec![
            Variant::Short(REQUEST_INIT_REQUEST),
            bytes(&m.class_name),
            bytes(m.object_name.as_bytes()),
        ],
        SignalProxyMessage::InitData(m) => {
            let mut items = vec![
                Variant::Short(REQUEST_INIT_DATA),
                bytes(&m.class_name),
                bytes(m.object_name.as_bytes()),
            ];
            for (key, value) in &m.init_data {
                items.push(bytes(key.as_bytes()));
                items.push(value.clone());
            }
            items
        }
        SignalProxyMessage::HeartBeat(ts) => {
            vec![Variant::Short(REQUEST_HEARTBEAT), Variant::DateTime(*ts)]
        }
        SignalProxyMessage::HeartBeatReply(ts) => {
            vec![
                Variant::Short(REQUEST_HEARTBEAT_REPLY),
                Variant::DateTime(*ts),
            ]
        }
    };
    let mut writer = Writer::with_features(features);
    write_qvariantlist(&mut writer, &items);
    writer.into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::qt::variant::write_variant;

    fn round_trip(message: SignalProxyMessage) {
        let encoded = encode_signalproxy_payload(&message, Features::empty());
        assert_eq!(
            decode_signalproxy_payload(&encoded, Features::empty()).unwrap(),
            message
        );
    }

    fn sync(class: &[u8], object: &str, slot: &[u8], params: Vec<Variant>) -> SignalProxyMessage {
        SignalProxyMessage::Sync(SyncMessage {
            class_name: class.to_vec(),
            object_name: object.into(),
            slot_name: slot.to_vec(),
            params,
        })
    }

    fn ts() -> QDateTime {
        QDateTime {
            julian_day: 2_461_145,
            ms_of_day: 45_296_789,
            is_utc: true,
        }
    }

    #[test]
    fn round_trips() {
        round_trip(sync(
            b"Network",
            "1",
            b"setNetworkName",
            vec!["freenode".into()],
        ));
        round_trip(sync(b"IrcChannel", "1/#python", b"joinIrcUser", vec![]));
        round_trip(sync(
            b"BufferSyncer",
            "",
            b"markBufferAsRead",
            vec![Variant::Int(42), "extra".into(), Variant::Bool(true)],
        ));
        round_trip(SignalProxyMessage::RpcCall(RpcCall {
            signal_name: b"2sendInput(BufferInfo,QString)".to_vec(),
            params: vec!["hello".into(), Variant::Int(42)],
        }));
        round_trip(SignalProxyMessage::RpcCall(RpcCall {
            signal_name: b"something".to_vec(),
            params: vec![],
        }));
        round_trip(SignalProxyMessage::InitRequest(InitRequest {
            class_name: b"Network".to_vec(),
            object_name: "5".into(),
        }));
        round_trip(SignalProxyMessage::InitRequest(InitRequest {
            class_name: b"BufferSyncer".to_vec(),
            object_name: String::new(),
        }));
        let mut init = VariantMap::new();
        init.insert("networkName".into(), "freenode".into());
        init.insert("connectionState".into(), Variant::Int(2));
        round_trip(SignalProxyMessage::InitData(InitData {
            class_name: b"Network".to_vec(),
            object_name: "1".into(),
            init_data: init,
        }));
        round_trip(SignalProxyMessage::InitData(InitData {
            class_name: b"X".to_vec(),
            object_name: String::new(),
            init_data: VariantMap::new(),
        }));
        round_trip(SignalProxyMessage::HeartBeat(ts()));
        round_trip(SignalProxyMessage::HeartBeatReply(ts()));
    }

    #[test]
    fn discriminator_is_short() {
        let blob = encode_signalproxy_payload(&sync(b"X", "", b"y", vec![]), Features::empty());
        assert_eq!(&blob[..4], b"\x00\x00\x00\x04");
        assert_eq!(&blob[4..8], b"\x00\x00\x00\x82");

        let hb =
            encode_signalproxy_payload(&SignalProxyMessage::HeartBeat(ts()), Features::empty());
        let reply = encode_signalproxy_payload(
            &SignalProxyMessage::HeartBeatReply(ts()),
            Features::empty(),
        );
        assert_eq!(&hb[9..11], b"\x00\x05");
        assert_eq!(&reply[9..11], b"\x00\x06");
    }

    #[test]
    fn sync_known_blob() {
        let blob = encode_signalproxy_payload(
            &sync(b"Network", "1", b"setNetworkName", vec!["freenode".into()]),
            Features::empty(),
        );
        let mut expected = Vec::new();
        expected.extend_from_slice(b"\x00\x00\x00\x05");
        expected.extend_from_slice(b"\x00\x00\x00\x82\x00\x00\x01");
        expected.extend_from_slice(b"\x00\x00\x00\x0c\x00\x00\x00\x00\x07Network");
        expected.extend_from_slice(b"\x00\x00\x00\x0c\x00\x00\x00\x00\x011");
        expected.extend_from_slice(b"\x00\x00\x00\x0c\x00\x00\x00\x00\x0esetNetworkName");
        expected.extend_from_slice(b"\x00\x00\x00\x0a\x00\x00\x00\x00\x10");
        expected.extend_from_slice(b"\x00f\x00r\x00e\x00e\x00n\x00o\x00d\x00e");
        assert_eq!(blob, expected);
    }

    #[test]
    fn rpc_known_blob() {
        let msg = SignalProxyMessage::RpcCall(RpcCall {
            signal_name: b"test".to_vec(),
            params: vec![Variant::Int(42)],
        });
        let blob = encode_signalproxy_payload(&msg, Features::empty());
        let expected = b"\x00\x00\x00\x03\
            \x00\x00\x00\x82\x00\x00\x02\
            \x00\x00\x00\x0c\x00\x00\x00\x00\x04test\
            \x00\x00\x00\x02\x00\x00\x00\x00\x2a";
        assert_eq!(blob, expected);
        assert_eq!(
            decode_signalproxy_payload(&blob, Features::empty()).unwrap(),
            msg
        );
    }

    fn list_payload(items: &[Variant]) -> Vec<u8> {
        let mut w = Writer::new();
        write_qvariantlist(&mut w, items);
        w.into_bytes()
    }

    fn decode_err(payload: &[u8]) -> String {
        let err = decode_signalproxy_payload(payload, Features::empty()).unwrap_err();
        assert!(matches!(err, Error::SignalProxy(_)), "{err:?}");
        err.to_string()
    }

    #[test]
    fn error_paths() {
        assert!(decode_err(&list_payload(&[])).contains("empty SignalProxy frame"));
        assert!(decode_err(&list_payload(&["not an int".into()])).contains("discriminator"));
        assert!(
            decode_err(&list_payload(&[Variant::Short(999)]))
                .contains("unknown SignalProxy discriminator")
        );
        assert!(
            decode_err(&list_payload(&[
                Variant::Short(REQUEST_SYNC),
                Variant::ByteArray(b"Network".to_vec()),
            ]))
            .contains("Sync:")
        );
        assert!(
            decode_err(&list_payload(&[
                Variant::Short(REQUEST_INIT_DATA),
                Variant::ByteArray(b"X".to_vec()),
                Variant::ByteArray(vec![]),
                Variant::ByteArray(b"stranded".to_vec()),
            ]))
            .contains("must be even")
        );
        assert!(
            decode_err(&list_payload(&[
                Variant::Short(REQUEST_INIT_REQUEST),
                Variant::ByteArray(b"X".to_vec()),
            ]))
            .contains("InitRequest")
        );
    }

    #[test]
    fn trailing_bytes_are_rejected() {
        let mut valid = encode_signalproxy_payload(
            &SignalProxyMessage::RpcCall(RpcCall {
                signal_name: b"test".to_vec(),
                params: vec![Variant::Int(42)],
            }),
            Features::empty(),
        );
        valid.extend_from_slice(b"JUNK");
        assert!(decode_err(&valid).contains("trailing"));

        let mut hb =
            encode_signalproxy_payload(&SignalProxyMessage::HeartBeat(ts()), Features::empty());
        hb.push(0);
        assert!(decode_err(&hb).contains("trailing"));
    }

    fn raw_payload(parts: &[&[u8]]) -> Vec<u8> {
        let mut out = (parts.len() as u32).to_be_bytes().to_vec();
        for part in parts {
            out.extend_from_slice(part);
        }
        out
    }

    fn variant_bytes(value: &Variant) -> Vec<u8> {
        let mut w = Writer::new();
        write_variant(&mut w, value);
        w.into_bytes()
    }

    /// QVariant<QByteArray> with the null length sentinel.
    const NULL_BYTEARRAY: &[u8] = b"\x00\x00\x00\x0c\x00\xff\xff\xff\xff";

    #[test]
    fn null_object_names_decode_as_empty() {
        let disc = |d| variant_bytes(&Variant::Short(d));
        let class = variant_bytes(&Variant::ByteArray(b"BufferSyncer".to_vec()));
        let slot = variant_bytes(&Variant::ByteArray(b"requestSetLastSeenMsg".to_vec()));

        let payload = raw_payload(&[&disc(REQUEST_SYNC), &class, NULL_BYTEARRAY, &slot]);
        let SignalProxyMessage::Sync(m) =
            decode_signalproxy_payload(&payload, Features::empty()).unwrap()
        else {
            panic!("expected Sync");
        };
        assert_eq!(m.object_name, "");
        assert_eq!(m.slot_name, b"requestSetLastSeenMsg");

        let payload = raw_payload(&[&disc(REQUEST_INIT_DATA), &class, NULL_BYTEARRAY]);
        let SignalProxyMessage::InitData(m) =
            decode_signalproxy_payload(&payload, Features::empty()).unwrap()
        else {
            panic!("expected InitData");
        };
        assert_eq!(m.object_name, "");
        assert!(m.init_data.is_empty());

        let payload = raw_payload(&[&disc(REQUEST_INIT_REQUEST), &class, NULL_BYTEARRAY]);
        assert_eq!(
            decode_signalproxy_payload(&payload, Features::empty()).unwrap(),
            SignalProxyMessage::InitRequest(InitRequest {
                class_name: b"BufferSyncer".to_vec(),
                object_name: String::new(),
            })
        );
    }
}
