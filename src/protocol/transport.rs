//! TCP and TLS primitives for reaching a core.
//!
//! The probe runs over plain TCP; if it negotiates `Encryption`, the same
//! socket is upgraded to TLS in place, because the core does not accept a
//! fresh connection after the probe.
//!
//! TLS goes through OpenSSL (`native-tls`) rather than rustls: the typical
//! quasselcore certificate is self-signed with `CA:TRUE`, which webpki
//! refuses as an end-entity even when the user trusts it via `--cafile`.

use std::path::PathBuf;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_native_tls::TlsStream;

use crate::protocol::error::{Error, Result};

pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Upper bound on a graceful close. A TLS peer that never answers our
/// `close_notify` must not wedge teardown (and with it Ctrl+Q).
pub const CLOSE_GRACE: Duration = Duration::from_secs(2);

/// How to verify the core's certificate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TlsOptions {
    /// Off only for self-signed cores the user trusts (`--insecure`).
    pub verify: bool,
    /// Extra PEM trust anchors (`--cafile`). `~` is expanded, since this
    /// usually comes from the config file where no shell expands it.
    pub cafile: Option<String>,
    /// SNI/verification name when it differs from the connect host.
    pub server_hostname: Option<String>,
}

impl Default for TlsOptions {
    fn default() -> Self {
        Self {
            verify: true,
            cafile: None,
            server_hostname: None,
        }
    }
}

/// Expand a leading `~/` to the home directory.
pub fn expand_user(path: &str) -> PathBuf {
    if path == "~"
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home);
    }
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    PathBuf::from(path)
}

impl TlsOptions {
    pub fn build_connector(&self) -> Result<native_tls::TlsConnector> {
        let mut builder = native_tls::TlsConnector::builder();
        if let Some(cafile) = &self.cafile {
            let expanded = expand_user(cafile);
            let load = || -> std::result::Result<Vec<native_tls::Certificate>, String> {
                let pem = std::fs::read(&expanded).map_err(|e| e.to_string())?;
                let certs = parse_pem_certificates(&pem)?;
                if certs.is_empty() {
                    return Err("no certificates found in file".into());
                }
                Ok(certs)
            };
            let certs = load().map_err(|e| {
                Error::Transport(format!(
                    "failed to load TLS trust anchors (cafile={:?}): {e}",
                    expanded.display().to_string()
                ))
            })?;
            for cert in certs {
                builder.add_root_certificate(cert);
            }
        }
        if !self.verify {
            builder.danger_accept_invalid_certs(true);
            builder.danger_accept_invalid_hostnames(true);
        }
        builder
            .build()
            .map_err(|e| Error::Transport(format!("failed to build TLS context: {e}")))
    }
}

/// Split a PEM bundle into certificates.
fn parse_pem_certificates(pem: &[u8]) -> std::result::Result<Vec<native_tls::Certificate>, String> {
    const BEGIN: &str = "-----BEGIN CERTIFICATE-----";
    const END: &str = "-----END CERTIFICATE-----";
    let text = String::from_utf8_lossy(pem);
    let mut certs = Vec::new();
    let mut rest = &text[..];
    while let Some(start) = rest.find(BEGIN) {
        let after = &rest[start..];
        let Some(end) = after.find(END) else {
            return Err("unterminated PEM certificate block".into());
        };
        let block = &after[..end + END.len()];
        certs.push(native_tls::Certificate::from_pem(block.as_bytes()).map_err(|e| e.to_string())?);
        rest = &after[end + END.len()..];
    }
    Ok(certs)
}

/// Open a TCP connection with a bounded connect time.
///
/// Enables `SO_KEEPALIVE`: a Quassel session is long-lived and a half-open
/// connection (suspend/resume, NAT expiry) is otherwise never noticed by
/// the kernel. The application-level liveness watchdog usually fires
/// first; this is the OS backstop.
pub async fn open_tcp_connection(
    host: &str,
    port: u16,
    connect_timeout: Duration,
) -> Result<TcpStream> {
    let stream = match tokio::time::timeout(connect_timeout, TcpStream::connect((host, port))).await
    {
        Err(_) => {
            return Err(Error::Transport(format!(
                "timed out connecting to {host}:{port} after {}s",
                fmt_secs(connect_timeout)
            )));
        }
        Ok(Err(e)) => {
            return Err(Error::Transport(format!(
                "failed to connect to {host}:{port}: {e}"
            )));
        }
        Ok(Ok(stream)) => stream,
    };
    // Failure here only happens on exotic transports; the watchdog still
    // covers us, so it isn't worth failing the connection over.
    let _ = socket2::SockRef::from(&stream).set_keepalive(true);
    Ok(stream)
}

/// Upgrade an open stream to TLS. Must happen right after the probe reply,
/// with no intervening reads or writes.
pub async fn start_tls<S>(stream: S, host: &str, options: &TlsOptions) -> Result<TlsStream<S>>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let connector = tokio_native_tls::TlsConnector::from(options.build_connector()?);
    let server_name = options.server_hostname.as_deref().unwrap_or(host);
    connector.connect(server_name, stream).await.map_err(|e| {
        let text = e.to_string();
        if text.contains("certificate verify failed")
            || text.contains("self-signed")
            || text.contains("self signed")
        {
            // Most real quasselcores are self-signed; tell a first-time
            // user what to do instead of leaving a raw OpenSSL error.
            Error::Transport(format!(
                "TLS certificate verification for {host} failed: {text}. The core's \
                 certificate is not trusted by the system store — for a self-signed \
                 core, pass --cafile <core-cert.pem> to trust it explicitly, or \
                 --insecure to skip verification (or set cafile/insecure in the \
                 server config)."
            ))
        } else {
            Error::Transport(format!("TLS upgrade to {host} failed: {text}"))
        }
    })
}

/// Best-effort close: shut down the write side (sending TLS `close_notify`
/// where applicable), bounded by [`CLOSE_GRACE`]. Errors from a peer that
/// already hung up are ignored; dropping the stream closes the socket.
pub async fn close_stream<S: AsyncWrite + Unpin>(stream: &mut S) {
    let _ = tokio::time::timeout(CLOSE_GRACE, stream.shutdown()).await;
}

/// Format a duration the way `%g` formats seconds: `10`, `0.05`, `1.5`.
pub fn fmt_secs(duration: Duration) -> String {
    let secs = duration.as_secs_f64();
    let mut text = format!("{secs:.3}");
    while text.ends_with('0') {
        text.pop();
    }
    if text.ends_with('.') {
        text.pop();
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn open_tcp_connection_enables_keepalive() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let accept = tokio::spawn(async move { listener.accept().await.unwrap() });
        let stream = open_tcp_connection("127.0.0.1", port, Duration::from_secs(5))
            .await
            .unwrap();
        assert!(socket2::SockRef::from(&stream).keepalive().unwrap());
        drop(accept.await.unwrap());
    }

    #[tokio::test]
    async fn connect_failure_is_transport_error() {
        // Grab a free port, then close it so nothing is listening.
        let port = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.local_addr().unwrap().port()
        };
        let err = open_tcp_connection("127.0.0.1", port, Duration::from_secs(5))
            .await
            .unwrap_err();
        assert!(err.is_transport(), "{err:?}");
        assert!(err.to_string().contains(&format!("127.0.0.1:{port}")));
    }

    #[test]
    fn bad_cafile_names_the_expanded_path() {
        let options = TlsOptions {
            cafile: Some("~/definitely-missing-quasseltui-test.pem".into()),
            ..TlsOptions::default()
        };
        let err = options.build_connector().unwrap_err();
        let message = err.to_string();
        assert!(err.is_transport());
        assert!(message.contains("trust anchors"), "{message}");
        assert!(message.contains("definitely-missing-quasseltui-test.pem"));
        if std::env::var_os("HOME").is_some() {
            assert!(!message.contains("cafile=\"~"), "{message}");
        }
    }

    #[test]
    fn expand_user_handles_tilde() {
        let home = std::env::var("HOME").unwrap();
        assert_eq!(expand_user("~/x.pem"), PathBuf::from(&home).join("x.pem"));
        assert_eq!(expand_user("/etc/x.pem"), PathBuf::from("/etc/x.pem"));
        assert_eq!(expand_user("rel/~/x"), PathBuf::from("rel/~/x"));
    }

    #[test]
    fn fmt_secs_matches_g_format() {
        assert_eq!(fmt_secs(Duration::from_secs(10)), "10");
        assert_eq!(fmt_secs(Duration::from_millis(50)), "0.05");
        assert_eq!(fmt_secs(Duration::from_millis(1500)), "1.5");
        assert_eq!(fmt_secs(Duration::from_secs(90)), "90");
    }
}
