//! Test support: a fake Quassel core on an in-memory pipe.
//!
//! The fake core reads the client's 8-byte probe, replies, then plays back
//! a prepared byte sequence. It records everything the client writes after
//! the probe so tests can inspect the frames that went out.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::task::JoinHandle;

use crate::protocol::connection::{BoxFuture, BoxedStream, ConnectionOptions, Connector};
use crate::protocol::error::Result;
use crate::protocol::features::Features;
use crate::protocol::framing::encode_frame;
use crate::protocol::handshake::encode_handshake_payload;
use crate::protocol::messages::{CLIENT_INIT_ACK, CLIENT_LOGIN_ACK, SESSION_INIT};
use crate::protocol::signalproxy::{SignalProxyMessage, encode_signalproxy_payload};
use crate::protocol::transport::TlsOptions;
use crate::qt::variant::{Variant, VariantMap};

pub struct FakeConnector {
    pub stream: Mutex<Option<BoxedStream>>,
    pub tls_called: Arc<AtomicBool>,
}

impl Connector for FakeConnector {
    fn connect<'a>(
        &'a self,
        _host: &'a str,
        _port: u16,
        _timeout: Duration,
    ) -> BoxFuture<'a, Result<BoxedStream>> {
        let stream = self.stream.lock().unwrap().take().expect("connected once");
        Box::pin(async move { Ok(stream) })
    }

    fn start_tls<'a>(
        &'a self,
        stream: BoxedStream,
        _host: &'a str,
        _options: &'a TlsOptions,
    ) -> BoxFuture<'a, Result<BoxedStream>> {
        // Pretend the upgrade succeeded and keep talking plaintext.
        self.tls_called.store(true, Ordering::SeqCst);
        Box::pin(async move { Ok(stream) })
    }
}

pub struct Fake {
    pub connector: Arc<FakeConnector>,
    pub tls_called: Arc<AtomicBool>,
    /// Everything the client wrote after the probe.
    pub written: JoinHandle<Vec<u8>>,
}

/// `tls_enabled`: whether the probe reply turns on Encryption.
/// `hang`: keep the pipe open (no EOF) once the inbound bytes run out.
pub fn fake_core(inbound: Vec<u8>, tls_enabled: bool, hang: bool) -> Fake {
    let (client, core) = tokio::io::duplex(1 << 22);
    let (mut core_r, mut core_w) = tokio::io::split(core);
    let written = tokio::spawn(async move {
        let mut probe = [0u8; 8];
        if core_r.read_exact(&mut probe).await.is_err() {
            return Vec::new();
        }
        let reply = 0x02u32 | (u32::from(tls_enabled) << 24);
        tokio::spawn(async move {
            core_w.write_all(&reply.to_be_bytes()).await.unwrap();
            core_w.write_all(&inbound).await.unwrap();
            if hang {
                std::future::pending::<()>().await;
            }
            let _ = core_w.shutdown().await;
        });
        let mut rest = Vec::new();
        let _ = core_r.read_to_end(&mut rest).await;
        rest
    });
    let tls_called = Arc::new(AtomicBool::new(false));
    Fake {
        connector: Arc::new(FakeConnector {
            stream: Mutex::new(Some(Box::new(client))),
            tls_called: tls_called.clone(),
        }),
        tls_called,
        written,
    }
}

pub fn map(entries: Vec<(&str, Variant)>) -> VariantMap {
    entries
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect()
}

pub fn framed_map(data: &VariantMap) -> Vec<u8> {
    encode_frame(&encode_handshake_payload(data))
}

pub fn framed_sp(message: &SignalProxyMessage) -> Vec<u8> {
    encode_frame(&encode_signalproxy_payload(message, modern()))
}

pub fn modern() -> Features {
    Features::LONG_TIME | Features::RICH_MESSAGES
}

pub fn base_init_ack() -> VariantMap {
    map(vec![
        ("MsgType", CLIENT_INIT_ACK.into()),
        ("Configured", Variant::Bool(true)),
        ("CoreFeatures", Variant::UInt(0)),
        (
            "FeatureList",
            Variant::StringList(vec!["LongTime".into(), "RichMessages".into()]),
        ),
        ("StorageBackends", Variant::List(vec![])),
    ])
}

pub fn login_ack() -> VariantMap {
    map(vec![("MsgType", CLIENT_LOGIN_ACK.into())])
}

pub fn session_init() -> VariantMap {
    map(vec![
        ("MsgType", SESSION_INIT.into()),
        (
            "SessionState",
            Variant::Map(map(vec![
                ("Identities", Variant::List(vec![])),
                ("NetworkIds", Variant::List(vec![])),
                ("BufferInfos", Variant::List(vec![])),
            ])),
        ),
    ])
}

pub fn inbound(init_ack: VariantMap, login: VariantMap, frames: &[Vec<u8>]) -> Vec<u8> {
    let mut out = framed_map(&init_ack);
    out.extend(framed_map(&login));
    out.extend(framed_map(&session_init()));
    for frame in frames {
        out.extend_from_slice(frame);
    }
    out
}

pub fn options(tls: bool) -> ConnectionOptions {
    let mut opts = ConnectionOptions::new("core", 4242, "u", "p");
    opts.tls = tls;
    opts
}

/// Split a byte buffer into frame payloads.
pub fn split_frames(mut buf: &[u8]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    while buf.len() >= 4 {
        let len = u32::from_be_bytes(buf[..4].try_into().unwrap()) as usize;
        out.push(buf[4..4 + len].to_vec());
        buf = &buf[4 + len..];
    }
    out
}

/// A `SessionInit` map with the given networks and buffers.
pub fn session_with(
    network_ids: &[i32],
    buffers: &[crate::protocol::types::BufferInfo],
) -> VariantMap {
    use crate::protocol::types::NetworkId;
    use crate::protocol::usertypes::UserValue;
    map(vec![
        ("MsgType", SESSION_INIT.into()),
        (
            "SessionState",
            Variant::Map(map(vec![
                ("Identities", Variant::List(vec![])),
                (
                    "NetworkIds",
                    Variant::List(
                        network_ids
                            .iter()
                            .map(|n| Variant::User(UserValue::NetworkId(NetworkId(*n))))
                            .collect(),
                    ),
                ),
                (
                    "BufferInfos",
                    Variant::List(
                        buffers
                            .iter()
                            .map(|b| Variant::User(UserValue::BufferInfo(b.clone())))
                            .collect(),
                    ),
                ),
            ])),
        ),
    ])
}

/// Handshake replies (default ack and login) with a custom session.
pub fn inbound_with_session(session: VariantMap, frames: &[Vec<u8>]) -> Vec<u8> {
    let mut out = framed_map(&base_init_ack());
    out.extend(framed_map(&login_ack()));
    out.extend(framed_map(&session));
    for frame in frames {
        out.extend_from_slice(frame);
    }
    out
}
