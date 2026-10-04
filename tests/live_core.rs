//! End-to-end tests against a real quasselcore. Skipped unless
//! `QUASSEL_TEST_HOST` is set:
//!
//! ```sh
//! QUASSEL_TEST_HOST=127.0.0.1 QUASSEL_TEST_PORT=4242 \
//! QUASSEL_TEST_USER=tester QUASSEL_TEST_PASSWORD=secret \
//! QUASSEL_TEST_INSECURE=1 cargo test --test live_core -- --nocapture
//! ```
//!
//! `QUASSEL_TEST_INSECURE=1` skips certificate verification (the usual
//! self-signed core); `QUASSEL_TEST_NO_TLS=1` uses plain TCP.

use std::time::Duration;

use quasseltui::client::{ClientState, QuasselClient};
use quasseltui::protocol::connection::ConnectionOptions;
use quasseltui::sync::events::ClientEvent;

fn options() -> Option<ConnectionOptions> {
    let host = std::env::var("QUASSEL_TEST_HOST").ok()?;
    let port = std::env::var("QUASSEL_TEST_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(4242);
    let user = std::env::var("QUASSEL_TEST_USER").unwrap_or_default();
    let password = std::env::var("QUASSEL_TEST_PASSWORD").unwrap_or_default();
    let mut opts = ConnectionOptions::new(host, port, user, password);
    opts.tls = std::env::var_os("QUASSEL_TEST_NO_TLS").is_none();
    opts.tls_options.verify = std::env::var_os("QUASSEL_TEST_INSECURE").is_none();
    Some(opts)
}

#[tokio::test]
async fn logs_in_and_receives_session_state() {
    let Some(opts) = options() else {
        eprintln!("QUASSEL_TEST_HOST not set; skipping live-core test");
        return;
    };
    let mut client = QuasselClient::connect(opts);
    let mut state = ClientState::default();
    let mut opened = false;
    let run = async {
        while let Some(event) = client.next_event(&mut state).await {
            match event {
                ClientEvent::SessionOpened { .. } => opened = true,
                ClientEvent::Disconnected { reason, .. } => panic!("disconnected: {reason}"),
                _ => {}
            }
        }
    };
    // A healthy session never ends on its own; run for a few seconds to
    // let InitData arrive, then check what we have.
    let _ = tokio::time::timeout(Duration::from_secs(5), run).await;
    assert!(opened, "the core never sent SessionInit");
    assert!(state.session.is_some());
    assert!(state.buffer_syncer.is_some());
    for network in state.networks.values() {
        assert!(
            network.initialized,
            "no InitData for network {}",
            network.object_name
        );
    }
    client.close();
}
