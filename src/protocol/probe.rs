//! The unframed probe exchange that opens every Quassel connection.
//!
//! ```text
//! Client -> Core:  magic   = 0x42b33f00 | connection_features    (u32 BE)
//!                  proto_i = type | (proto_features << 8)        (u32 BE)
//!                  ... the last entry also has 0x80000000 set
//! Core -> Client:  reply   = type | (peer_features << 8) | (conn_features << 24)
//! ```
//!
//! If the reply enables `Encryption`, both sides switch the same socket to
//! TLS immediately, with no further plaintext. We never offer Compression:
//! it would need a zlib layer under the framing that we don't implement.

use std::fmt;

use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};

use crate::protocol::error::{Error, Result};
use crate::protocol::framing::read_exactly;

const QUASSEL_MAGIC: u32 = 0x42B3_3F00;
const END_LIST_BIT: u32 = 0x8000_0000;

/// Quassel `Protocol::Type`. We only ever speak `DataStream`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtocolType {
    Internal = 0x00,
    Legacy = 0x01,
    DataStream = 0x02,
}

/// Connection-level feature bits exchanged during the probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ConnectionFeatures(pub u8);

impl ConnectionFeatures {
    pub const NONE: Self = Self(0x00);
    pub const ENCRYPTION: Self = Self(0x01);
    pub const COMPRESSION: Self = Self(0x02);

    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0 && other.0 != 0
    }
}

impl std::ops::BitOr for ConnectionFeatures {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

impl fmt::Display for ConnectionFeatures {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut names = Vec::new();
        if self.contains(Self::ENCRYPTION) {
            names.push("Encryption");
        }
        if self.contains(Self::COMPRESSION) {
            names.push("Compression");
        }
        if names.is_empty() {
            f.write_str("none")
        } else {
            f.write_str(&names.join("|"))
        }
    }
}

/// The result of a successful probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NegotiatedProtocol {
    pub protocol: ProtocolType,
    pub peer_features: u16,
    pub connection_features: ConnectionFeatures,
}

impl NegotiatedProtocol {
    /// The caller must upgrade this socket to TLS before anything else.
    pub fn tls_required(&self) -> bool {
        self.connection_features
            .contains(ConnectionFeatures::ENCRYPTION)
    }

    pub fn compression_enabled(&self) -> bool {
        self.connection_features
            .contains(ConnectionFeatures::COMPRESSION)
    }
}

/// Build the probe request. Protocols are in preference order; the core
/// picks the first it supports.
pub fn build_probe_request(
    offered: ConnectionFeatures,
    protocols: &[(ProtocolType, u16)],
) -> Result<Vec<u8>> {
    if protocols.is_empty() {
        return Err(Error::Other("at least one protocol must be offered".into()));
    }
    let mut out = Vec::with_capacity(4 + protocols.len() * 4);
    out.extend_from_slice(&(QUASSEL_MAGIC | u32::from(offered.0)).to_be_bytes());
    for (i, (kind, features)) in protocols.iter().enumerate() {
        let mut entry = (*kind as u32) | (u32::from(*features) << 8);
        if i == protocols.len() - 1 {
            entry |= END_LIST_BIT;
        }
        out.extend_from_slice(&entry.to_be_bytes());
    }
    Ok(out)
}

/// The default request: the given connection features plus DataStream.
pub fn default_probe_request(offered: ConnectionFeatures) -> Vec<u8> {
    build_probe_request(offered, &[(ProtocolType::DataStream, 0)]).expect("one protocol offered")
}

/// Decode the 4-byte reply.
///
/// Rejects anything other than DataStream, connection features we didn't
/// offer (half of the TLS-downgrade defense: a hostile core can't push a
/// `--no-tls` client into a confused TLS handshake), unknown feature bits,
/// and Compression. The other half of the downgrade defense, refusing to
/// continue when we offered TLS and the core declined, belongs to the
/// caller, which knows whether plaintext was requested.
pub fn parse_probe_reply(reply: &[u8], offered: ConnectionFeatures) -> Result<NegotiatedProtocol> {
    let bytes: [u8; 4] = reply
        .try_into()
        .map_err(|_| Error::Probe(format!("probe reply must be 4 bytes, got {}", reply.len())))?;
    let word = u32::from_be_bytes(bytes);
    let proto_byte = (word & 0xFF) as u8;
    let peer_features = ((word >> 8) & 0xFFFF) as u16;
    let conn_bits = ((word >> 24) & 0xFF) as u8;

    let protocol = match proto_byte {
        0x00 => ProtocolType::Internal,
        0x01 => ProtocolType::Legacy,
        0x02 => ProtocolType::DataStream,
        other => {
            return Err(Error::Probe(format!(
                "core selected unknown protocol type {other:#x}; we only speak DataStream"
            )));
        }
    };
    if protocol != ProtocolType::DataStream {
        return Err(Error::Probe(format!(
            "core selected protocol {protocol:?} — quasseltui only speaks DataStream"
        )));
    }

    let supported = ConnectionFeatures::ENCRYPTION.0 | ConnectionFeatures::COMPRESSION.0;
    let unknown = conn_bits & !supported;
    if unknown != 0 {
        return Err(Error::Probe(format!(
            "core enabled unknown connection feature bits {unknown:#x}"
        )));
    }
    let negotiated = ConnectionFeatures(conn_bits);
    let extra = ConnectionFeatures(conn_bits & !offered.0);
    if extra.0 != 0 {
        return Err(Error::Probe(format!(
            "core enabled features we did not offer: {extra} (offered {offered})"
        )));
    }
    if negotiated.contains(ConnectionFeatures::COMPRESSION) {
        return Err(Error::Probe(
            "core enabled Compression but quasseltui does not implement it".into(),
        ));
    }
    Ok(NegotiatedProtocol {
        protocol,
        peer_features,
        connection_features: negotiated,
    })
}

/// Send the probe and read the reply on an open socket. If the result says
/// `tls_required`, the caller must upgrade this exact socket next.
pub async fn probe<S>(stream: &mut S, offered: ConnectionFeatures) -> Result<NegotiatedProtocol>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    stream.write_all(&default_probe_request(offered)).await?;
    stream.flush().await?;
    let reply = read_exactly(stream, 4).await?;
    parse_probe_reply(&reply, offered)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(words: &[u32]) -> Vec<u8> {
        words.iter().flat_map(|w| w.to_be_bytes()).collect()
    }

    #[test]
    fn default_request_offers_encryption_and_datastream() {
        assert_eq!(
            default_probe_request(ConnectionFeatures::ENCRYPTION),
            words(&[0x42B3_3F01, 0x8000_0002])
        );
        assert_eq!(
            default_probe_request(ConnectionFeatures::NONE),
            words(&[0x42B3_3F00, 0x8000_0002])
        );
        assert_eq!(
            default_probe_request(ConnectionFeatures::ENCRYPTION | ConnectionFeatures::COMPRESSION),
            words(&[0x42B3_3F03, 0x8000_0002])
        );
    }

    #[test]
    fn multiple_protocols_mark_only_last() {
        let got = build_probe_request(
            ConnectionFeatures::NONE,
            &[
                (ProtocolType::DataStream, 0x0042),
                (ProtocolType::Legacy, 0),
            ],
        )
        .unwrap();
        assert_eq!(got, words(&[0x42B3_3F00, 0x0000_4202, 0x8000_0001]));
    }

    #[test]
    fn empty_protocol_list_rejected() {
        let err = build_probe_request(ConnectionFeatures::NONE, &[]).unwrap_err();
        assert!(err.to_string().contains("at least one protocol"));
    }

    #[test]
    fn datastream_with_tls() {
        let word = 0x02 | (0x1234 << 8) | (0x01 << 24);
        let n = parse_probe_reply(&words(&[word]), ConnectionFeatures::ENCRYPTION).unwrap();
        assert_eq!(
            n,
            NegotiatedProtocol {
                protocol: ProtocolType::DataStream,
                peer_features: 0x1234,
                connection_features: ConnectionFeatures::ENCRYPTION,
            }
        );
        assert!(n.tls_required());
        assert!(!n.compression_enabled());
    }

    #[test]
    fn datastream_without_features() {
        let n = parse_probe_reply(&words(&[0x02]), ConnectionFeatures::ENCRYPTION).unwrap();
        assert_eq!(n.peer_features, 0);
        assert_eq!(n.connection_features, ConnectionFeatures::NONE);
        assert!(!n.tls_required());
    }

    fn reject(reply: &[u8], offered: ConnectionFeatures, needle: &str) {
        let err = parse_probe_reply(reply, offered).unwrap_err();
        assert!(matches!(err, Error::Probe(_)), "{err:?}");
        assert!(err.to_string().contains(needle), "{err} missing {needle}");
    }

    #[test]
    fn rejections() {
        let both = ConnectionFeatures::ENCRYPTION | ConnectionFeatures::COMPRESSION;
        reject(&words(&[0x02 | (0x02 << 24)]), both, "Compression");
        reject(b"\x00\x00", ConnectionFeatures::ENCRYPTION, "4 bytes");
        reject(
            &words(&[0xFF]),
            ConnectionFeatures::ENCRYPTION,
            "unknown protocol type",
        );
        reject(&words(&[0x01]), ConnectionFeatures::ENCRYPTION, "Legacy");
        reject(&words(&[0x00]), ConnectionFeatures::ENCRYPTION, "Internal");
        reject(
            &words(&[0x02 | (0x01 << 24)]),
            ConnectionFeatures::NONE,
            "did not offer",
        );
        reject(
            &words(&[0x02 | (0x80 << 24)]),
            ConnectionFeatures::ENCRYPTION,
            "unknown connection feature",
        );
    }

    #[tokio::test]
    async fn probe_round_trip_over_duplex() {
        let (mut client, mut core) = tokio::io::duplex(64);
        let core_task = tokio::spawn(async move {
            let request = read_exactly(&mut core, 8).await.unwrap();
            core.write_all(&(0x02u32 | (0x01 << 24)).to_be_bytes())
                .await
                .unwrap();
            request
        });
        let n = probe(&mut client, ConnectionFeatures::ENCRYPTION)
            .await
            .unwrap();
        assert_eq!(n.protocol, ProtocolType::DataStream);
        assert!(n.tls_required());
        assert_eq!(
            core_task.await.unwrap(),
            default_probe_request(ConnectionFeatures::ENCRYPTION)
        );
    }
}
