//! Exp B (live) — **the decisive throughput A/B**: HTTP/2 (cyper over compio-net + rustls, multiplexed) vs
//! HTTP/1.1 (the production `ureq` blocking pool, connection-per-request), against a real endpoint over a
//! real network — the only setting where the h2 multiplexing win (it hides per-request RTT and collapses N
//! TLS handshakes to ~a few) is measurable. This is the question the loopback spikes could not settle.
//!
//! Workload = the tile shape: N concurrent **small** GETs to ONE host. `generate_204` returns an empty 204,
//! so the measurement isolates connection/RTT/handshake overhead (not body bytes) — exactly where h2 wins.
//!
//! NOTE (since `http3-altsvc` became default): the `negotiated` probe (first request, cold) still reports h2,
//! but the *measured* rounds auto-upgrade to **HTTP/3** once Google's `Alt-Svc: h3` is cached — so this now
//! measures h3-vs-h1.1 on 204s. Either way it's the cold-connect/handshake regime (where h2/h3 both win big);
//! the real-body A/B is `cyper_p3dt_bench.rs`.
//!
//! Network-gated: run with
//! `cargo test -p logi_3dtiles_io --features cyper-h2 --release --test cyper_h2_bench -- --ignored --nocapture`.
//!
//! Feature-gated on `cyper-h2`: cyper/compio are optional deps pulled only by that feature, so without it
//! this file compiles to nothing (the default build never touches the h2 stack).
#![cfg(feature = "cyper-h2")]

use std::io::Read;
use std::time::{Duration, Instant};

const URL: &str = "https://www.google.com/generate_204"; // empty 204, h2-capable, tolerates high concurrency
const N: usize = 64; // concurrent requests to one host (the tile burst shape)
const ROUNDS: usize = 5; // + 1 warmup; report median

fn median(mut v: Vec<Duration>) -> Duration {
    v.sort();
    v[v.len() / 2]
}

#[ignore = "hits a real network host; run explicitly with --ignored"]
#[test]
fn bench_cyper_h2_vs_ureq_h1_live() {
    // ---- cyper: HTTP/2, multiplexed over compio-net (one pooled Client) ----
    let rt = compio::runtime::Runtime::new().unwrap();
    let (negotiated, h2_times) = rt.block_on(async {
        let client = cyper::Client::new().unwrap();
        // One request first: confirm the protocol ALPN actually negotiated + warm the connection pool.
        let negotiated = match client.get(URL).unwrap().send().await {
            Ok(r) => format!("{:?}", r.version()),
            Err(e) => format!("ERR {e}"),
        };
        let mut times = Vec::with_capacity(ROUNDS);
        for round in 0..=ROUNDS {
            let t0 = Instant::now();
            let mut tasks = Vec::with_capacity(N);
            for _ in 0..N {
                let c = client.clone();
                tasks.push(compio::runtime::spawn(async move {
                    if let Ok(rb) = c.get(URL) {
                        if let Ok(resp) = rb.send().await {
                            let _ = resp.bytes().await;
                        }
                    }
                }));
            }
            for t in tasks {
                let _ = t.await;
            }
            if round > 0 {
                times.push(t0.elapsed());
            } // round 0 = warmup
        }
        (negotiated, times)
    });

    // ---- ureq: HTTP/1.1, thread-per-request (the HttpSource blocking-pool model) ----
    let mut ureq_times = Vec::with_capacity(ROUNDS);
    for round in 0..=ROUNDS {
        let t0 = Instant::now();
        let handles: Vec<_> = (0..N)
            .map(|_| {
                std::thread::spawn(|| {
                    if let Ok(resp) = ureq::get(URL).call() {
                        let mut sink = Vec::new();
                        let _ = resp.into_reader().read_to_end(&mut sink);
                    }
                })
            })
            .collect();
        for h in handles {
            let _ = h.join();
        }
        if round > 0 {
            ureq_times.push(t0.elapsed());
        }
    }

    let h2 = median(h2_times);
    let uq = median(ureq_times);
    eprintln!("\n=== Exp B (live): h2 (cyper/compio) vs h1.1 (ureq) — {N} concurrent GETs to {URL} ===");
    eprintln!("  cyper protocol negotiated: {negotiated}");
    eprintln!("  cyper  (HTTP/2, multiplexed):   median {h2:.2?} over {ROUNDS} rounds");
    eprintln!("  ureq   (HTTP/1.1, thread/req):  median {uq:.2?} over {ROUNDS} rounds");
    eprintln!(
        "  → h2 ÷ h1.1 = {:.2}x  ({})\n",
        h2.as_secs_f64() / uq.as_secs_f64(),
        if h2 < uq {
            "h2 faster — multiplexing hides RTT + collapses handshakes"
        } else {
            "h1.1 faster/comparable here"
        }
    );
    assert!(
        negotiated.contains('2'),
        "cyper should negotiate HTTP/2 via ALPN (got {negotiated})"
    );
}
