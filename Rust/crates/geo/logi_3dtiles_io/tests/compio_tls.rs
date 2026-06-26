//! Exp B (cont.) — the TLS layer of the compio I/O lane.
//!
//! De-risks two things: that **compio-tls + rustls + ring build** on the Windows nightly toolchain (no
//! aws-lc-rs build dependency — `cargo` confirmed this), and that a real rustls `ClientConfig` + compio
//! `TlsConnector` **assemble** end-to-end, using compio's *own re-exported* rustls so there is zero version
//! skew. This is the layer directly below h2.
//!
//! Out of scope here (it is the integration step, not a local spike): the live TLS handshake + HTTP/2 GET +
//! the throughput-vs-ureq verdict — those need a real HTTPS host (network latency is what makes the h2
//! multiplexing win measurable) and the audit's live A/B against the P3DT endpoint.
//!
//! Feature-gated on `cyper-h2`: compio/webpki-roots are optional deps pulled only by that feature.
#![cfg(feature = "cyper-h2")]

use compio::tls::{rustls, TlsConnector};
use std::sync::Arc;

#[test]
fn compio_tls_rustls_connector_assembles_with_real_roots() {
    // Real Mozilla root trust store, via compio's re-exported rustls + pki-types (no version skew).
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    assert!(!roots.is_empty(), "webpki-roots populated the trust store");

    // Explicit ring provider so we never depend on a process-global default being installed.
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .expect("safe default protocol versions")
        .with_root_certificates(roots)
        .with_no_client_auth();

    // The compio TLS connector composes from the rustls config — the I/O lane's TLS layer assembles cleanly.
    let _connector = TlsConnector::from(Arc::new(config));
}
