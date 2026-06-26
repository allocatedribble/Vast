# Vendored cyper — patch manifest

**Upstream:** `cyper` 0.9 (crates.io / https://github.com/compio-rs/cyper), vendored under
`Rust/third_party/cyper` and wired via the workspace `[patch.crates-io]` in `Rust/Cargo.toml`.

**Why vendored:** upstream cyper builds its HTTP/3 QUIC endpoint with quinn's default
`TransportConfig` and exposes no seam to change it. Quinn's defaults leave
`keep_alive_interval = None`, so a pooled tile connection dies on the 30 s idle timeout and the
next render-critical tile re-pays a full QUIC handshake. `logi_3dtiles_io::CyperSource` needs that
knob (10 s keep-alive — proven live by `bench_keepalive_idle_survival`).

**Patch convention:** every divergence from upstream is commented with a `LogiUE patch:` banner at
the site. Keep the diff minimal — tuning belongs in `logi_3dtiles_io`, not here.

## Patches

1. **`src/client.rs` — `ClientBuilder::h3_transport_config`** (`LogiUE patch:` banner above the fn):
   a builder seam accepting an `Arc<compio::quic::TransportConfig>`, stored on the builder
   (`h3_transport` field) and threaded into the HTTP/3 connector.
2. **`src/http3.rs` — apply the transport config** (`LogiUE patch:` banner in `Connector::new`):
   `client_config.transport_config(tc)` before the endpoints are built; `None` ⇒ upstream default
   behaviour.

## Re-vendoring / upgrading

Diff this tree against the matching upstream tag first, re-apply the `LogiUE patch:` sites (grep
for the banner), and re-run the io crate's keep-alive proof:
`cargo test -p logi_3dtiles_io --test cyper_p3dt_bench -- --ignored bench_keepalive_idle_survival`
(needs `CESIUM_ION_TOKEN`).
