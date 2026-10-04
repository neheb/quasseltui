//! Handshake message (de)serialization.
//!
//! The DataStream protocol sends a handshake map as a flat `QVariantList`
//! alternating key and value, with keys as `QVariant<QByteArray>` holding
//! UTF-8 (`DataStreamPeer::writeMessage(const QVariantMap&)`). Keys go out
//! in sorted order, which is Qt's QMap iteration order.

use tokio::io::{AsyncRead, AsyncWrite};

use crate::protocol::error::{Error, Result};
use crate::protocol::framing::{DEFAULT_MAX_FRAME_BYTES, read_frame, write_frame};
use crate::protocol::messages::{
    ClientInit, ClientLogin, HandshakeMessage, parse_handshake_message,
};
use crate::qt::datastream::{Reader, Writer};
use crate::qt::variant::{Variant, VariantMap, read_qvariantlist, write_variant};

/// Flatten a handshake map into the frame payload (no length prefix).
pub fn encode_handshake_payload(fields: &VariantMap) -> Vec<u8> {
    let mut writer = Writer::new();
    writer.write_u32((fields.len() * 2) as u32);
    // BTreeMap iterates in sorted key order.
    for (key, value) in fields {
        write_variant(&mut writer, &Variant::ByteArray(key.as_bytes().to_vec()));
        write_variant(&mut writer, value);
    }
    writer.into_bytes()
}

/// Rebuild a handshake map from a frame payload, regardless of key order.
pub fn decode_handshake_payload(payload: &[u8]) -> Result<VariantMap> {
    let mut reader = Reader::new(payload);
    let items = read_qvariantlist(&mut reader)?;
    if !reader.at_end() {
        return Err(Error::Handshake(format!(
            "trailing {} bytes after handshake payload",
            reader.remaining()
        )));
    }
    if items.len() % 2 != 0 {
        return Err(Error::Handshake(format!(
            "handshake payload has odd item count {}; expected key/value pairs",
            items.len()
        )));
    }
    let mut out = VariantMap::new();
    let mut iter = items.into_iter();
    while let (Some(raw_key), Some(value)) = (iter.next(), iter.next()) {
        let key = match raw_key {
            Variant::ByteArray(bytes) => String::from_utf8(bytes)
                .map_err(|e| Error::Handshake(format!("handshake key is not valid UTF-8: {e}")))?,
            Variant::String(s) => s,
            other => {
                return Err(Error::Handshake(format!(
                    "handshake key has unexpected type {}",
                    other.type_name()
                )));
            }
        };
        out.insert(key, value);
    }
    Ok(out)
}

pub fn encode_client_init(msg: &ClientInit) -> Vec<u8> {
    encode_handshake_payload(&msg.to_map())
}

pub fn encode_client_login(msg: &ClientLogin) -> Vec<u8> {
    encode_handshake_payload(&msg.to_map())
}

pub async fn send_client_init<W: AsyncWrite + Unpin>(
    writer: &mut W,
    msg: &ClientInit,
) -> Result<()> {
    write_frame(writer, &encode_client_init(msg)).await
}

pub async fn send_client_login<W: AsyncWrite + Unpin>(
    writer: &mut W,
    msg: &ClientLogin,
) -> Result<()> {
    write_frame(writer, &encode_client_login(msg)).await
}

/// Read one framed handshake reply and parse it.
///
/// Binary decode failures surface as `Error::Handshake`; `ClientLoginReject`
/// surfaces as `Error::Auth`.
pub async fn recv_handshake_message<R: AsyncRead + Unpin>(
    reader: &mut R,
) -> Result<HandshakeMessage> {
    let payload = read_frame(reader, DEFAULT_MAX_FRAME_BYTES).await?;
    let fields = decode_handshake_payload(&payload).map_err(|e| match e {
        Error::Decode(inner) => {
            Error::Handshake(format!("failed to decode handshake payload: {inner}"))
        }
        other => other,
    })?;
    parse_handshake_message(&fields)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::framing::encode_frame;
    use crate::protocol::messages::{CLIENT_INIT, CLIENT_LOGIN};
    use crate::protocol::types::{BufferId, BufferInfo, BufferType, NetworkId};
    use crate::protocol::usertypes::UserValue;
    use crate::qt::variant::type_id;

    fn map(entries: Vec<(&str, Variant)>) -> VariantMap {
        entries
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect()
    }

    #[test]
    fn keys_are_qbytearray_in_sorted_order() {
        let payload =
            encode_handshake_payload(&map(vec![("zeta", Variant::Int(1)), ("alpha", "x".into())]));
        let items = read_qvariantlist(&mut Reader::new(&payload)).unwrap();
        assert_eq!(
            items,
            [
                Variant::ByteArray(b"alpha".to_vec()),
                Variant::String("x".into()),
                Variant::ByteArray(b"zeta".to_vec()),
                Variant::Int(1),
            ]
        );
    }

    #[test]
    fn client_init_types_features_as_uint_and_list_as_qstringlist() {
        let mut msg = ClientInit::new("v", "d");
        msg.features = 0x8000_0001;
        msg.feature_list = vec!["A".into()];
        let decoded = decode_handshake_payload(&encode_client_init(&msg)).unwrap();
        assert_eq!(decoded["Features"], Variant::UInt(0x8000_0001));
        assert_eq!(
            decoded["FeatureList"],
            Variant::StringList(vec!["A".into()])
        );

        // Check the wire type ids directly too.
        let payload = encode_handshake_payload(&map(vec![("Features", Variant::UInt(1))]));
        let mut r = Reader::new(&payload);
        assert_eq!(r.read_u32().unwrap(), 2);
        assert_eq!(r.read_u32().unwrap(), type_id::QBYTE_ARRAY);
        r.read_u8().unwrap();
        let len = r.read_u32().unwrap() as usize;
        assert_eq!(r.read_bytes(len).unwrap(), b"Features");
        assert_eq!(r.read_u32().unwrap(), type_id::UINT);
    }

    #[test]
    fn client_init_full_field_set() {
        let decoded = decode_handshake_payload(&encode_client_init(&ClientInit::new(
            "quasseltui v0.0.0",
            "2026-04-14",
        )))
        .unwrap();
        assert_eq!(
            decoded,
            map(vec![
                ("MsgType", CLIENT_INIT.into()),
                ("ClientVersion", "quasseltui v0.0.0".into()),
                ("ClientDate", "2026-04-14".into()),
                ("Features", Variant::UInt(0)),
                ("FeatureList", Variant::StringList(vec![])),
            ])
        );
    }

    #[test]
    fn client_login_round_trips() {
        let login = ClientLogin {
            user: "user".into(),
            password: "🔒passwørd".into(),
        };
        let decoded = decode_handshake_payload(&encode_client_login(&login)).unwrap();
        assert_eq!(decoded["MsgType"], Variant::String(CLIENT_LOGIN.into()));
        assert_eq!(decoded["Password"], Variant::String("🔒passwørd".into()));
    }

    #[test]
    fn empty_payload_decodes_to_empty_map() {
        assert!(
            decode_handshake_payload(&encode_handshake_payload(&VariantMap::new()))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn decode_errors() {
        let mut w = Writer::new();
        w.write_u32(1);
        write_variant(&mut w, &Variant::ByteArray(b"orphan".to_vec()));
        let err = decode_handshake_payload(w.as_bytes()).unwrap_err();
        assert!(err.to_string().contains("odd item count"));

        let mut w = Writer::new();
        w.write_u32(2);
        write_variant(&mut w, &Variant::Int(42));
        write_variant(&mut w, &"value".into());
        let err = decode_handshake_payload(w.as_bytes()).unwrap_err();
        assert!(err.to_string().contains("unexpected type"));

        let mut payload = encode_handshake_payload(&map(vec![("k", "v".into())]));
        payload.extend_from_slice(b"junk");
        let err = decode_handshake_payload(&payload).unwrap_err();
        assert!(err.to_string().contains("trailing"));
    }

    #[tokio::test]
    async fn recv_reads_framed_messages() {
        let ack = map(vec![
            ("MsgType", "ClientInitAck".into()),
            ("CoreFeatures", Variant::UInt(0)),
            ("FeatureList", Variant::StringList(vec![])),
            ("Configured", Variant::Bool(true)),
        ]);
        let mut data = encode_frame(&encode_handshake_payload(&ack));
        data.extend(encode_frame(&encode_handshake_payload(&map(vec![
            ("MsgType", "ClientInitReject".into()),
            ("Error", "no good".into()),
        ]))));
        let mut reader = &data[..];
        let first = recv_handshake_message(&mut reader).await.unwrap();
        assert!(matches!(first, HandshakeMessage::ClientInitAck(a) if a.configured));
        let second = recv_handshake_message(&mut reader).await.unwrap();
        assert_eq!(
            second,
            HandshakeMessage::ClientInitReject {
                error: "no good".into()
            }
        );
    }

    #[tokio::test]
    async fn recv_wraps_binary_errors_as_handshake() {
        let data = encode_frame(b"\x00\x00\x00\x01\x00\x00\x00\xff\x00");
        let err = recv_handshake_message(&mut &data[..]).await.unwrap_err();
        assert!(matches!(err, Error::Handshake(_)), "{err:?}");
        assert!(
            err.to_string()
                .contains("failed to decode handshake payload")
        );
    }

    #[test]
    fn hand_built_session_init_decodes() {
        // Mirrors what a real core emits: the outer flattened list with a
        // nested QVariantMap whose lists carry user-type envelopes.
        let info = BufferInfo {
            buffer_id: BufferId(1),
            network_id: NetworkId(1),
            kind: BufferType::Channel,
            group_id: 0,
            name: "#test".into(),
        };
        let mut w = Writer::new();
        w.write_u32(4);
        write_variant(&mut w, &Variant::ByteArray(b"MsgType".to_vec()));
        write_variant(&mut w, &"SessionInit".into());
        write_variant(&mut w, &Variant::ByteArray(b"SessionState".to_vec()));
        w.write_u32(type_id::QVARIANT_MAP);
        w.write_u8(0);
        w.write_u32(3);
        w.write_qstring(Some("BufferInfos"));
        write_variant(
            &mut w,
            &Variant::List(vec![Variant::User(UserValue::BufferInfo(info.clone()))]),
        );
        w.write_qstring(Some("Identities"));
        write_variant(&mut w, &Variant::List(vec![]));
        w.write_qstring(Some("NetworkIds"));
        write_variant(
            &mut w,
            &Variant::List(vec![Variant::User(UserValue::NetworkId(NetworkId(1)))]),
        );
        let fields = decode_handshake_payload(w.as_bytes()).unwrap();
        let HandshakeMessage::SessionInit(session) = parse_handshake_message(&fields).unwrap()
        else {
            panic!("expected SessionInit");
        };
        assert_eq!(session.network_ids, [NetworkId(1)]);
        assert_eq!(session.buffer_infos, [info]);
        assert!(session.identities.is_empty());
    }
}
