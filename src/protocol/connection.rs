//! The connection state machine: probe -> TLS -> handshake -> connected.
//!
//! [`QuasselConnection::start`] consumes the connection (it is single-use;
//! a reconnect builds a new one) and spawns a task that:
//!
//! 1. opens TCP and runs the probe,
//! 2. upgrades to TLS if negotiated. If we offered TLS and the core declined,
//!    it aborts *before* `ClientInit`, so credentials never touch a
//!    plaintext socket unless the caller explicitly disabled TLS,
//! 3. exchanges `ClientInit`/`ClientLogin`/`SessionInit` and negotiates
//!    features,
//! 4. emits `SessionReady`, then one event per SignalProxy frame,
//! 5. ends with exactly one terminal `Disconnected`, whatever happens.
//!
//! Writes go through a dedicated writer task fed by a queue, so the UI,
//! the client's init-request fan-out, and heartbeat replies never interleave
//! partial frames. Heartbeats are answered before the event is emitted, so
//! the reply doesn't depend on how fast the consumer drains events.
//!
//! Liveness: the core heartbeats every ~30s, so a read that produces no
//! frame header within `liveness_timeout` means the connection is dead
//! (half-open TCP after suspend, NAT expiry). Once a header arrives, the
//! payload gets ten times that window so a large backlog frame on a slow
//! link isn't killed mid-transfer.

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite, ReadHalf, WriteHalf};
use tokio::sync::{mpsc, oneshot, watch};

use crate::protocol::error::{Error, Result};
use crate::protocol::features::{Features, LEGACY_EXTENDED_FEATURES};
use crate::protocol::framing::{
    DEFAULT_MAX_FRAME_BYTES, read_frame_header, read_frame_payload, write_frame,
};
use crate::protocol::handshake::{encode_client_init, encode_client_login, recv_handshake_message};
use crate::protocol::messages::{
    ClientInit, ClientInitAck, ClientLogin, HandshakeMessage, SessionInit,
};
use crate::protocol::probe::{ConnectionFeatures, probe};
use crate::protocol::signalproxy::{
    InitData, InitRequest, RpcCall, SignalProxyMessage, SyncMessage, decode_signalproxy_payload,
    encode_signalproxy_payload,
};
use crate::protocol::transport::{
    DEFAULT_CONNECT_TIMEOUT, TlsOptions, close_stream, fmt_secs, open_tcp_connection, start_tls,
};
use crate::qt::datastream::QDateTime;

/// Three missed ~30s core heartbeats.
pub const DEFAULT_LIVENESS_TIMEOUT: Duration = Duration::from_secs(90);
/// The handshake window includes the whole SessionInit transfer, which
/// takes tens of seconds on large cores.
pub const DEFAULT_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(60);
const PAYLOAD_TIMEOUT_FACTOR: u32 = 10;
/// Consecutive undecodable frames before giving up. One-off oddities are
/// skipped quietly; a stream where nothing decodes is a desync, and endless
/// skipping would be silent message loss.
const MAX_CONSECUTIVE_DECODE_FAILURES: u32 = 5;
const EVENT_CHANNEL_CAPACITY: usize = 256;

/// Any bidirectional byte stream (TCP, TLS, or an in-memory test pipe).
pub trait Stream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Stream for T {}
pub type BoxedStream = Box<dyn Stream>;
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// How the connection reaches a core. Swappable so tests can run the full
/// state machine over an in-memory pipe.
pub trait Connector: Send + Sync + 'static {
    fn connect<'a>(
        &'a self,
        host: &'a str,
        port: u16,
        timeout: Duration,
    ) -> BoxFuture<'a, Result<BoxedStream>>;

    fn start_tls<'a>(
        &'a self,
        stream: BoxedStream,
        host: &'a str,
        options: &'a TlsOptions,
    ) -> BoxFuture<'a, Result<BoxedStream>>;
}

/// The real network: TCP plus OpenSSL.
pub struct TcpConnector;

impl Connector for TcpConnector {
    fn connect<'a>(
        &'a self,
        host: &'a str,
        port: u16,
        timeout: Duration,
    ) -> BoxFuture<'a, Result<BoxedStream>> {
        Box::pin(async move {
            let stream = open_tcp_connection(host, port, timeout).await?;
            Ok(Box::new(stream) as BoxedStream)
        })
    }

    fn start_tls<'a>(
        &'a self,
        stream: BoxedStream,
        host: &'a str,
        options: &'a TlsOptions,
    ) -> BoxFuture<'a, Result<BoxedStream>> {
        Box::pin(async move {
            let tls = start_tls(stream, host, options).await?;
            Ok(Box::new(tls) as BoxedStream)
        })
    }
}

/// Where the state machine is. Advances monotonically.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnState {
    Initial,
    Probing,
    TlsUpgrading,
    HandshakeInit,
    HandshakeLogin,
    HandshakeSession,
    Connected,
    Closed,
}

impl fmt::Display for ConnState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Initial => "initial",
            Self::Probing => "probing",
            Self::TlsUpgrading => "tls_upgrading",
            Self::HandshakeInit => "handshake_init",
            Self::HandshakeLogin => "handshake_login",
            Self::HandshakeSession => "handshake_session",
            Self::Connected => "connected",
            Self::Closed => "closed",
        })
    }
}

/// Everything needed to connect and log in.
#[derive(Clone)]
pub struct ConnectionOptions {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: String,
    /// Offer TLS and refuse to continue without it.
    pub tls: bool,
    pub tls_options: TlsOptions,
    pub client_version: String,
    pub build_date: String,
    pub connect_timeout: Duration,
    /// Defaults to max(60s, connect_timeout).
    pub handshake_timeout: Option<Duration>,
    pub liveness_timeout: Duration,
    pub offered_features: Features,
}

impl fmt::Debug for ConnectionOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ConnectionOptions")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("user", &self.user)
            .field("password", &"<redacted>")
            .field("tls", &self.tls)
            .finish_non_exhaustive()
    }
}

impl ConnectionOptions {
    pub fn new(
        host: impl Into<String>,
        port: u16,
        user: impl Into<String>,
        password: impl Into<String>,
    ) -> Self {
        Self {
            host: host.into(),
            port,
            user: user.into(),
            password: password.into(),
            tls: true,
            tls_options: TlsOptions::default(),
            client_version: "quasseltui".into(),
            build_date: "1970-01-01".into(),
            connect_timeout: DEFAULT_CONNECT_TIMEOUT,
            handshake_timeout: None,
            liveness_timeout: DEFAULT_LIVENESS_TIMEOUT,
            offered_features: Features::client_default(),
        }
    }

    /// The post-connect handshake window. A raised `connect_timeout` acts
    /// as a floor, never a ceiling; an explicit `handshake_timeout` wins.
    pub fn effective_handshake_timeout(&self) -> Duration {
        self.handshake_timeout
            .unwrap_or_else(|| DEFAULT_HANDSHAKE_TIMEOUT.max(self.connect_timeout))
    }
}

/// Events from a running connection, in order. `Disconnected` is always
/// the last one.
#[derive(Debug, Clone)]
pub enum ProtocolEvent {
    SessionReady {
        session: SessionInit,
        peer_features: Features,
        init_ack: ClientInitAck,
    },
    Sync(SyncMessage),
    Rpc(RpcCall),
    InitData(InitData),
    InitRequest(InitRequest),
    /// The core's heartbeat. It has already been answered.
    HeartBeat(QDateTime),
    Disconnected {
        reason: String,
        /// The error that ended the connection; `None` for a local close.
        error: Option<Arc<Error>>,
    },
}

struct Outbound {
    message: SignalProxyMessage,
    ack: Option<oneshot::Sender<Result<()>>>,
}

struct Shared {
    state: Mutex<ConnState>,
    features: Mutex<Features>,
}

impl Shared {
    fn set_state(&self, state: ConnState) {
        *self.state.lock().expect("state lock") = state;
    }

    fn state(&self) -> ConnState {
        *self.state.lock().expect("state lock")
    }
}

/// A cloneable handle for writing to, observing, and closing a running
/// connection.
#[derive(Clone)]
pub struct ConnectionHandle {
    outbound: mpsc::UnboundedSender<Outbound>,
    shared: Arc<Shared>,
    shutdown: Arc<watch::Sender<bool>>,
}

impl ConnectionHandle {
    pub fn state(&self) -> ConnState {
        self.shared.state()
    }

    pub fn peer_features(&self) -> Features {
        *self.shared.features.lock().expect("features lock")
    }

    fn check_connected(&self) -> Result<()> {
        match self.state() {
            ConnState::Connected => Ok(()),
            state => Err(Error::Other(format!(
                "cannot send SignalProxy message in state {}",
                format!("{state}").to_uppercase()
            ))),
        }
    }

    /// Send a message and wait until it is written (or the write failed).
    pub async fn send(&self, message: SignalProxyMessage) -> Result<()> {
        self.check_connected()?;
        let (tx, rx) = oneshot::channel();
        self.outbound
            .send(Outbound {
                message,
                ack: Some(tx),
            })
            .map_err(|_| Error::ConnectionClosed("connection is closed".into()))?;
        rx.await.unwrap_or_else(|_| {
            Err(Error::ConnectionClosed(
                "connection closed before the message was sent".into(),
            ))
        })
    }

    /// Queue a message without waiting. A write failure still tears the
    /// connection down (and so surfaces as `Disconnected`).
    pub fn send_nowait(&self, message: SignalProxyMessage) -> Result<()> {
        self.check_connected()?;
        self.outbound
            .send(Outbound { message, ack: None })
            .map_err(|_| Error::ConnectionClosed("connection is closed".into()))
    }

    /// Ask the connection to shut down. The event stream then ends with a
    /// `Disconnected("client closed")` unless it had already ended.
    pub fn close(&self) {
        self.shutdown.send_replace(true);
    }
}

/// One connection attempt.
pub struct QuasselConnection {
    options: ConnectionOptions,
    connector: Arc<dyn Connector>,
}

impl QuasselConnection {
    pub fn new(options: ConnectionOptions) -> Self {
        Self::with_connector(options, Arc::new(TcpConnector))
    }

    pub fn with_connector(options: ConnectionOptions, connector: Arc<dyn Connector>) -> Self {
        Self { options, connector }
    }

    pub fn options(&self) -> &ConnectionOptions {
        &self.options
    }

    /// Spawn the connection task. Must be called inside a Tokio runtime.
    pub fn start(self) -> (ConnectionHandle, mpsc::Receiver<ProtocolEvent>) {
        let (events_tx, events_rx) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
        let (outbound_tx, outbound_rx) = mpsc::unbounded_channel();
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let shared = Arc::new(Shared {
            state: Mutex::new(ConnState::Initial),
            features: Mutex::new(Features::empty()),
        });
        let handle = ConnectionHandle {
            outbound: outbound_tx.clone(),
            shared: shared.clone(),
            shutdown: Arc::new(shutdown_tx),
        };
        let terminal_sent = Arc::new(AtomicBool::new(false));
        let runner = Runner {
            options: self.options,
            connector: self.connector,
            shared: shared.clone(),
            events: events_tx.clone(),
            outbound_tx,
            terminal_sent: terminal_sent.clone(),
        };
        let inner = tokio::spawn(runner.run(outbound_rx, shutdown_rx));
        // Supervisor: even a panic inside the runner must end the stream
        // with exactly one Disconnected.
        tokio::spawn(async move {
            if let Err(join_error) = inner.await {
                shared.set_state(ConnState::Closed);
                if !terminal_sent.swap(true, Ordering::SeqCst) {
                    let reason = format!("internal error in receive loop: {join_error}");
                    let _ = events_tx
                        .send(ProtocolEvent::Disconnected {
                            error: Some(Arc::new(Error::Other(reason.clone()))),
                            reason,
                        })
                        .await;
                }
            }
        });
        (handle, events_rx)
    }
}

struct Runner {
    options: ConnectionOptions,
    connector: Arc<dyn Connector>,
    shared: Arc<Shared>,
    events: mpsc::Sender<ProtocolEvent>,
    outbound_tx: mpsc::UnboundedSender<Outbound>,
    terminal_sent: Arc<AtomicBool>,
}

/// Resolves when a close was requested. If every handle is gone nobody
/// can request one, so it never resolves.
async fn wait_shutdown(rx: &mut watch::Receiver<bool>) {
    loop {
        if *rx.borrow_and_update() {
            return;
        }
        if rx.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

enum Step<T> {
    Value(T),
    Stop {
        reason: String,
        error: Option<Error>,
    },
}

impl Runner {
    async fn disconnect(&self, reason: String, error: Option<Error>) {
        self.shared.set_state(ConnState::Closed);
        if !self.terminal_sent.swap(true, Ordering::SeqCst) {
            let _ = self
                .events
                .send(ProtocolEvent::Disconnected {
                    reason,
                    error: error.map(Arc::new),
                })
                .await;
        }
    }

    async fn run(
        self,
        outbound_rx: mpsc::UnboundedReceiver<Outbound>,
        mut shutdown_rx: watch::Receiver<bool>,
    ) {
        let handshake = tokio::select! {
            biased;
            () = wait_shutdown(&mut shutdown_rx) => {
                self.disconnect("client closed".into(), None).await;
                return;
            }
            result = self.do_handshake() => result,
        };
        let (stream, ack, session, features) = match handshake {
            Ok(done) => done,
            Err(error) => {
                let reason = match &error {
                    Error::Auth(msg) => format!("auth rejected: {msg}"),
                    Error::Io(e) => format!("transport error during handshake: {e}"),
                    other => format!("handshake failed: {other}"),
                };
                self.disconnect(reason, Some(error)).await;
                return;
            }
        };

        *self.shared.features.lock().expect("features lock") = features;
        self.shared.set_state(ConnState::Connected);
        let ready = ProtocolEvent::SessionReady {
            session,
            peer_features: features,
            init_ack: ack,
        };
        if self.events.send(ready).await.is_err() {
            self.shared.set_state(ConnState::Closed);
            return;
        }

        let (read_half, write_half) = tokio::io::split(stream);
        let (stop_tx, stop_rx) = watch::channel(false);
        let (failed_tx, failed_rx) = oneshot::channel();
        let writer = tokio::spawn(writer_task(
            write_half,
            outbound_rx,
            stop_rx,
            failed_tx,
            features,
            self.options.liveness_timeout,
        ));

        let (reason, error) = self.connected_loop(read_half, shutdown_rx, failed_rx).await;
        stop_tx.send_replace(true);
        let _ = writer.await;
        if let Some(reason) = reason {
            self.disconnect(reason, error).await;
        } else {
            self.shared.set_state(ConnState::Closed);
        }
    }

    async fn do_handshake(&self) -> Result<(BoxedStream, ClientInitAck, SessionInit, Features)> {
        self.shared.set_state(ConnState::Probing);
        let stream = self
            .connector
            .connect(
                &self.options.host,
                self.options.port,
                self.options.connect_timeout,
            )
            .await?;
        let window = self.options.effective_handshake_timeout();
        match tokio::time::timeout(window, self.handshake_after_connect(stream)).await {
            Ok(result) => result,
            Err(_) => Err(Error::Handshake(format!(
                "core accepted the TCP connection but the handshake stalled (no reply within {}s during {})",
                fmt_secs(window),
                self.shared.state()
            ))),
        }
    }

    async fn handshake_after_connect(
        &self,
        mut stream: BoxedStream,
    ) -> Result<(BoxedStream, ClientInitAck, SessionInit, Features)> {
        let opts = &self.options;
        let offered = if opts.tls {
            ConnectionFeatures::ENCRYPTION
        } else {
            ConnectionFeatures::NONE
        };
        let negotiated = probe(&mut stream, offered).await?;
        if negotiated.tls_required() {
            self.shared.set_state(ConnState::TlsUpgrading);
            stream = self
                .connector
                .start_tls(stream, &opts.host, &opts.tls_options)
                .await?;
        } else if opts.tls {
            // Fail closed: never send credentials over plaintext unless the
            // user explicitly opted out of TLS.
            return Err(Error::Probe(
                "core did not enable TLS but we offered it; refusing to send credentials \
                 over plaintext (re-run with --no-tls, or set tls = false in the server \
                 config, only if you trust this network path)"
                    .into(),
            ));
        }

        self.shared.set_state(ConnState::HandshakeInit);
        let init = ClientInit {
            client_version: opts.client_version.clone(),
            build_date: opts.build_date.clone(),
            features: opts.offered_features.to_bitmask(),
            feature_list: opts
                .offered_features
                .names()
                .into_iter()
                .map(String::from)
                .collect(),
        };
        write_frame(&mut stream, &encode_client_init(&init)).await?;
        let ack = match recv_handshake_message(&mut stream).await? {
            HandshakeMessage::ClientInitAck(ack) => ack,
            HandshakeMessage::ClientInitReject { error } => {
                return Err(Error::Handshake(format!(
                    "core rejected ClientInit: '{error}'"
                )));
            }
            HandshakeMessage::CoreSetupReject { error } => {
                return Err(Error::Handshake(format!("core setup rejected: '{error}'")));
            }
            other => {
                return Err(Error::Handshake(format!(
                    "unexpected handshake reply at init phase: {}",
                    other.kind()
                )));
            }
        };
        if !ack.configured {
            return Err(Error::Handshake(
                "core is not configured (run quasselcore --setup first)".into(),
            ));
        }
        let features = negotiate_features(opts.offered_features, &ack);

        self.shared.set_state(ConnState::HandshakeLogin);
        let login = ClientLogin {
            user: opts.user.clone(),
            password: opts.password.clone(),
        };
        write_frame(&mut stream, &encode_client_login(&login)).await?;
        match recv_handshake_message(&mut stream).await? {
            HandshakeMessage::ClientLoginAck => {}
            other => {
                return Err(Error::Handshake(format!(
                    "expected ClientLoginAck, got {}",
                    other.kind()
                )));
            }
        }

        self.shared.set_state(ConnState::HandshakeSession);
        let session = match recv_handshake_message(&mut stream).await? {
            HandshakeMessage::SessionInit(session) => session,
            other => {
                return Err(Error::Handshake(format!(
                    "expected SessionInit, got {}",
                    other.kind()
                )));
            }
        };
        Ok((stream, ack, session, features))
    }

    /// Returns the terminal reason, or `None` if the consumer went away.
    async fn connected_loop(
        &self,
        mut reader: ReadHalf<BoxedStream>,
        mut shutdown_rx: watch::Receiver<bool>,
        mut writer_failed: oneshot::Receiver<Error>,
    ) -> (Option<String>, Option<Error>) {
        let liveness = self.options.liveness_timeout;
        let features = *self.shared.features.lock().expect("features lock");
        let mut consecutive_failures = 0;
        loop {
            let read = async {
                let length = match tokio::time::timeout(
                    liveness,
                    read_frame_header(&mut reader, DEFAULT_MAX_FRAME_BYTES),
                )
                .await
                {
                    Err(_) => {
                        return Step::Stop {
                            reason: format!(
                                "no data from core in {}s (connection presumed dead; core heartbeats every ~30s)",
                                fmt_secs(liveness)
                            ),
                            error: Some(Error::Other("liveness timeout".into())),
                        };
                    }
                    Ok(Err(e)) => return read_error(e),
                    Ok(Ok(length)) => length,
                };
                match tokio::time::timeout(
                    liveness * PAYLOAD_TIMEOUT_FACTOR,
                    read_frame_payload(&mut reader, length),
                )
                .await
                {
                    Err(_) => Step::Stop {
                        reason: format!(
                            "no data from core in {}s (connection presumed dead; core heartbeats every ~30s)",
                            fmt_secs(liveness * PAYLOAD_TIMEOUT_FACTOR)
                        ),
                        error: Some(Error::Other("payload timeout".into())),
                    },
                    Ok(Err(e)) => read_error(e),
                    Ok(Ok(payload)) => Step::Value(payload),
                }
            };
            let step = tokio::select! {
                biased;
                () = wait_shutdown(&mut shutdown_rx) => Step::Stop { reason: "client closed".into(), error: None },
                failure = &mut writer_failed => {
                    let error = failure.unwrap_or_else(|_| Error::Other("writer task ended".into()));
                    Step::Stop { reason: format!("failed to send to core: {error}"), error: Some(error) }
                }
                step = read => step,
            };
            let payload = match step {
                Step::Value(payload) => payload,
                Step::Stop { reason, error } => return (Some(reason), error),
            };

            let message = match decode_signalproxy_payload(&payload, features) {
                Ok(message) => {
                    consecutive_failures = 0;
                    message
                }
                Err(error) => {
                    // A bad payload doesn't desynchronize the stream: it was
                    // read completely and the next header is at a known
                    // offset. Skip it, unless nothing decodes any more.
                    consecutive_failures += 1;
                    if consecutive_failures >= MAX_CONSECUTIVE_DECODE_FAILURES {
                        return (
                            Some(format!(
                                "{consecutive_failures} consecutive undecodable frames (protocol desync or feature mismatch); last: {error}"
                            )),
                            Some(error),
                        );
                    }
                    tracing::warn!(
                        "skipping undecodable frame ({} bytes): {error}",
                        payload.len()
                    );
                    continue;
                }
            };

            let event = match message {
                SignalProxyMessage::HeartBeat(ts) => {
                    if let Err(error) = self.reply_heartbeat(ts).await {
                        return (
                            Some(format!("failed to process frame: {error}")),
                            Some(error),
                        );
                    }
                    ProtocolEvent::HeartBeat(ts)
                }
                SignalProxyMessage::HeartBeatReply(ts) => {
                    // We never send heartbeats ourselves.
                    tracing::debug!("received unexpected HeartBeatReply ts={ts:?}");
                    continue;
                }
                SignalProxyMessage::Sync(m) => ProtocolEvent::Sync(m),
                SignalProxyMessage::RpcCall(m) => ProtocolEvent::Rpc(m),
                SignalProxyMessage::InitData(m) => ProtocolEvent::InitData(m),
                SignalProxyMessage::InitRequest(m) => ProtocolEvent::InitRequest(m),
            };
            if self.events.send(event).await.is_err() {
                return (None, None);
            }
        }
    }

    async fn reply_heartbeat(&self, ts: QDateTime) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.outbound_tx
            .send(Outbound {
                message: SignalProxyMessage::HeartBeatReply(ts),
                ack: Some(tx),
            })
            .map_err(|_| Error::ConnectionClosed("writer is gone".into()))?;
        let liveness = self.options.liveness_timeout;
        match tokio::time::timeout(liveness, rx).await {
            Err(_) => Err(Error::Other(format!(
                "heartbeat reply stalled for {}s (send buffer full, connection presumed dead)",
                fmt_secs(liveness)
            ))),
            Ok(Err(_)) => Err(Error::ConnectionClosed("writer is gone".into())),
            Ok(Ok(result)) => result,
        }
    }
}

fn read_error(error: Error) -> Step<Vec<u8>> {
    let reason = match &error {
        Error::Io(e) => format!("core closed connection: {e}"),
        other => format!("frame read error: {other}"),
    };
    Step::Stop {
        reason,
        error: Some(error),
    }
}

/// Quassel feature negotiation has three tiers:
///
/// 1. The core returns a non-empty FeatureList: use the string
///    intersection, plus any features only expressed in the legacy bitmask.
/// 2. The core sets ExtendedFeatures but returns an empty list: it honors
///    our FeatureList for whatever it was compiled with, so assume all
///    offered features are active (safe because we only offer what we can
///    decode), or we'd misread the wire.
/// 3. A truly legacy core: the binary bitmask only.
pub fn negotiate_features(offered: Features, ack: &ClientInitAck) -> Features {
    let string_features = offered & Features::from_names(&ack.feature_list);
    let binary_features = offered & Features::from_bitmask(ack.core_features);
    if !ack.feature_list.is_empty() {
        string_features | binary_features
    } else if ack.core_features & LEGACY_EXTENDED_FEATURES != 0 {
        offered
    } else {
        binary_features
    }
}

async fn writer_task(
    mut writer: WriteHalf<BoxedStream>,
    mut outbound: mpsc::UnboundedReceiver<Outbound>,
    mut stop: watch::Receiver<bool>,
    failed: oneshot::Sender<Error>,
    features: Features,
    write_timeout: Duration,
) {
    let mut stopping = false;
    loop {
        let item = if stopping {
            // Flush what was queued before the close, then stop.
            match outbound.try_recv() {
                Ok(item) => item,
                Err(_) => break,
            }
        } else {
            tokio::select! {
                biased;
                _ = stop.changed() => {
                    stopping = true;
                    continue;
                }
                item = outbound.recv() => match item {
                    Some(item) => item,
                    None => break,
                },
            }
        };
        let payload = encode_signalproxy_payload(&item.message, features);
        let result =
            match tokio::time::timeout(write_timeout, write_frame(&mut writer, &payload)).await {
                Ok(result) => result,
                Err(_) => Err(Error::Other(format!(
                    "send stalled for {}s (send buffer full, connection presumed dead)",
                    fmt_secs(write_timeout)
                ))),
            };
        match result {
            Ok(()) => {
                if let Some(ack) = item.ack {
                    let _ = ack.send(Ok(()));
                }
            }
            Err(error) => {
                // A failed write may have left a partial frame on the wire,
                // so the stream is unusable: report and stop.
                let text = error.to_string();
                if let Some(ack) = item.ack {
                    let _ = ack.send(Err(Error::Transport(text)));
                }
                let _ = failed.send(error);
                return;
            }
        }
    }
    close_stream(&mut writer).await;
}

#[cfg(test)]
mod tests;
