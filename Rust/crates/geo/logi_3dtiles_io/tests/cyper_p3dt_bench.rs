//! Exp B (LIVE, KEYED P3DT) — the decisive A/B: HTTP/3 over QUIC (cyper over compio-quic = quinn-proto on the
//! compio reactor, reached via Alt-Svc auto-upgrade) vs HTTP/1.1 (the `ureq` thread-per-request pool) on REAL
//! Google Photorealistic 3D Tiles bodies (Draco-compressed glTF + JPEG atlases) fetched from the keyed
//! `tile.googleapis.com` endpoint over a real network — i.e. real auth (`?session=&key=`) + real payload
//! sizes, not the empty-204 proxy of `cyper_h2_bench.rs`. `tile.googleapis.com` advertises h3 (`Alt-Svc:
//! h3=":443"`), so after a warmup burst the long-lived client is on HTTP/3. The question: does QUIC's
//! faster handshake / no-HoL-blocking help on real bodies, or (like the h2 A/B) is it ~parity once the
//! connection is warm and the regime is bandwidth-bound?
//!
//! Network + token gated. The Google P3DT asset (2275207) resolves through Cesium ion, so provide an ion
//! token (the repo's `Intermediate/CesiumIonToken.txt` works):
//!   $env:CESIUM_ION_TOKEN = (Get-Content .\Intermediate\CesiumIonToken.txt -Raw).Trim()
//!   cargo test -p logi_3dtiles_io --features cyper-h2 --release --test cyper_p3dt_bench -- --ignored --nocapture
//!
//! Feature-gated on `cyper-h2` (cyper/compio are optional deps pulled only by that feature), so without it
//! this file compiles to nothing.
#![cfg(feature = "cyper-h2")]

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use logi_3dtiles_io::{http_get, resolve_url};
use logi_3dtiles_tileset::parse_tileset;

const ASSET_ID: u64 = 2275207; // Google Photorealistic 3D Tiles
const WANT_TILES: usize = 32; // real .glb tile URLs to A/B (the tile-burst width)
const MAX_CLASSIFY_FETCHES: usize = 256; // bound the one-time tree walk that discovers tile URLs
const ROUNDS: usize = 4; // measured rounds after a warmup; report the median burst time

/// glTF binary magic ("glTF") — the first 4 bytes of a `.glb` tile (vs a sub-tileset JSON, which starts `{`).
const GLB_MAGIC: &[u8; 4] = b"glTF";

fn median(mut v: Vec<Duration>) -> Duration {
    v.sort();
    v.get(v.len() / 2).copied().unwrap_or(Duration::ZERO)
}

fn mb_per_s(bytes: u64, dt: Duration) -> f64 {
    let secs = dt.as_secs_f64();
    if secs <= 0.0 {
        0.0
    } else {
        (bytes as f64 / (1024.0 * 1024.0)) / secs
    }
}

/// Walk the (grafted-explicit) P3DT tileset from `root_url`, classifying each `content.uri` by its body
/// magic — a `glTF`-magic body is a real tile (collect its resolved URL), a `{`-leading body is a
/// sub-`tileset.json` (recurse into it as a new base). Returns up to `want` resolved `.glb` tile URLs, each
/// carrying the live `?session=&key=` auth. Bounded by `MAX_CLASSIFY_FETCHES` so a deep tree can't run away.
fn collect_glb_urls(root_url: &str, root_bytes: Vec<u8>, want: usize) -> Vec<String> {
    let mut tiles: Vec<String> = Vec::new();
    let mut fetches = 0usize;
    // (tileset base URL, tileset bytes) still to walk.
    let mut tilesets: VecDeque<(String, Vec<u8>)> = VecDeque::new();
    tilesets.push_back((root_url.to_string(), root_bytes));

    while let Some((base, bytes)) = tilesets.pop_front() {
        if tiles.len() >= want || fetches >= MAX_CLASSIFY_FETCHES {
            break;
        }
        let Ok(ts) = parse_tileset(&bytes) else { continue };
        // BFS this tileset's explicit tile tree, gathering content URIs in (coarse → fine) tree order.
        let mut uris: Vec<String> = Vec::new();
        let mut nodes: VecDeque<&logi_3dtiles_tileset::Tile> = VecDeque::new();
        nodes.push_back(&ts.root);
        while let Some(t) = nodes.pop_front() {
            if let Some(c) = &t.content {
                uris.push(c.uri.clone());
            }
            for c in &t.contents {
                uris.push(c.uri.clone());
            }
            for ch in &t.children {
                nodes.push_back(ch);
            }
        }
        for uri in uris {
            if tiles.len() >= want || fetches >= MAX_CLASSIFY_FETCHES {
                break;
            }
            let Ok(resolved) = resolve_url(&base, &uri) else {
                continue;
            };
            fetches += 1;
            let Ok(body) = http_get(&resolved) else { continue };
            if body.len() >= 4 && &body[..4] == GLB_MAGIC {
                tiles.push(resolved); // a real Draco/JPEG tile
            } else if body.first().is_some_and(|b| *b == b'{' || b.is_ascii_whitespace()) {
                tilesets.push_back((resolved, body)); // a sub-tileset to descend
            }
        }
    }
    tiles
}

/// ureq arm: fetch every URL concurrently, thread-per-request (ureq's best case — unbounded concurrency over
/// the shared keep-alive agent; the production `HttpSource` caps at its worker count, so this FAVOURS ureq).
/// Returns (wall-clock for the whole burst, total bytes downloaded).
fn ureq_burst(urls: &[String]) -> (Duration, u64) {
    let t0 = Instant::now();
    let handles: Vec<_> = urls
        .iter()
        .cloned()
        .map(|u| std::thread::spawn(move || http_get(&u).map(|b| b.len() as u64).unwrap_or(0)))
        .collect();
    let bytes: u64 = handles.into_iter().map(|h| h.join().unwrap_or(0)).sum();
    (t0.elapsed(), bytes)
}

#[ignore = "live network; needs CESIUM_ION_TOKEN (e.g. Intermediate/CesiumIonToken.txt)"]
#[test]
fn bench_cyper_vs_ureq_keyed_p3dt() {
    let Ok(token) = std::env::var("CESIUM_ION_TOKEN") else {
        eprintln!("CESIUM_ION_TOKEN not set — skipping the keyed P3DT A/B");
        return;
    };

    // Resolve Google P3DT through ion → the keyed `tile.googleapis.com` root, then discover real tile URLs.
    let asset = logi_3dtiles_ion::resolve_asset(ASSET_ID, &token).expect("resolve ion asset");
    let root = http_get(&asset.tileset_url).expect("fetch root tileset");
    let urls = collect_glb_urls(&asset.tileset_url, root, WANT_TILES);
    assert!(
        !urls.is_empty(),
        "no .glb tile URLs collected from the P3DT tileset (tree-walk found none)"
    );

    // ---- ureq: HTTP/1.1, thread-per-request ----
    let _ = ureq_burst(&urls); // warmup (open connections / warm any CDN edge)
                               // Single warm-tile latency: the per-tile floor (RTT + server tile-production + 13 KiB transfer) over an
                               // already-open keep-alive connection. If the N-concurrent burst ≈ this, the burst is latency-bound (the
                               // transport can't help); if the burst ≫ this, concurrency is the bottleneck.
    let ureq_single = {
        let t = Instant::now();
        let _ = http_get(&urls[0]);
        t.elapsed()
    };
    let mut ureq_times = Vec::with_capacity(ROUNDS);
    let mut ureq_bytes = 0u64;
    for _ in 0..ROUNDS {
        let (dt, b) = ureq_burst(&urls);
        ureq_times.push(dt);
        ureq_bytes = b;
    }

    // ---- cyper: HTTP/3 over QUIC (Alt-Svc auto-upgrade), multiplexed over compio-quic ----
    let rt = compio::runtime::Runtime::new().expect("compio runtime");
    let (negotiated, cyper_single, cyper_times, cyper_bytes) = rt.block_on(async {
        let client = cyper::Client::new().expect("cyper client");
        let fetch_all = |client: cyper::Client, urls: Vec<String>| async move {
            let mut tasks = Vec::with_capacity(urls.len());
            for u in urls {
                let c = client.clone();
                tasks.push(compio::runtime::spawn(async move {
                    match c.get(&u) {
                        Ok(rb) => match rb.send().await {
                            Ok(resp) => resp.bytes().await.map(|b| b.len() as u64).unwrap_or(0),
                            Err(_) => 0,
                        },
                        Err(_) => 0,
                    }
                }));
            }
            let mut total = 0u64;
            for t in tasks {
                total += t.await.unwrap_or(0);
            }
            total
        };
        // Warmup: a full burst so the `http3-altsvc` upgrade is learned + cached on this long-lived client
        // (first request per host = h2 bootstrap → response `Alt-Svc: h3` cached → host switched to h3).
        let _ = fetch_all(client.clone(), urls.clone()).await;
        // Probe the negotiated protocol AFTER warmup — now HTTP/3 over QUIC via Alt-Svc, NOT the h2 bootstrap.
        // (Probing the first request, as the 204 bench does, would catch the h2 bootstrap and mislabel the run.)
        let negotiated = match client.get(&urls[0]).expect("url").send().await {
            Ok(r) => format!("{:?}", r.version()),
            Err(e) => format!("ERR {e}"),
        };
        // Single warm-tile latency over the established h3/QUIC connection (the per-tile floor, same role as
        // the ureq single probe). Burst ÷ this = how serial vs parallel the multiplexed lane really is.
        let single = {
            let t = Instant::now();
            if let Ok(rb) = client.get(&urls[0]) {
                if let Ok(resp) = rb.send().await {
                    let _ = resp.bytes().await;
                }
            }
            t.elapsed()
        };
        let mut times = Vec::with_capacity(ROUNDS);
        let mut bytes = 0u64;
        for _ in 0..ROUNDS {
            let t0 = Instant::now();
            bytes = fetch_all(client.clone(), urls.clone()).await;
            times.push(t0.elapsed());
        }
        (negotiated, single, times, bytes)
    });

    let uq = median(ureq_times);
    let h3 = median(cyper_times);
    let n = urls.len();
    let avg_kib = (cyper_bytes.max(ureq_bytes) as f64 / n as f64) / 1024.0;
    eprintln!("\n=== Exp B (LIVE, keyed P3DT): h3/QUIC (cyper/compio-quic) vs h1.1 (ureq) — {n} real .glb tiles ===");
    eprintln!("  cyper protocol negotiated (post Alt-Svc warmup): {negotiated}");
    eprintln!("  avg tile body: {avg_kib:.0} KiB  (real Draco/JPEG bodies, not empty 204s)");
    eprintln!(
        "  cyper (HTTP/3 over QUIC):     median {h3:.2?}  → {:.1} MB/s  ({} KiB / burst)",
        mb_per_s(cyper_bytes, h3),
        cyper_bytes / 1024
    );
    eprintln!(
        "  ureq  (HTTP/1.1, thread/req): median {uq:.2?}  → {:.1} MB/s  ({} KiB / burst)  [ureq best case]",
        mb_per_s(ureq_bytes, uq),
        ureq_bytes / 1024
    );
    // The per-tile floor + how parallel the burst actually is. If burst ≈ single, the burst is latency-bound
    // (RTT + server tile-production dominate, the body is tiny) → the transport CANNOT help and parity is the
    // correct, unavoidable result. effective-parallelism = n×single ÷ burst (→ n = perfect; → 1 = serial).
    let par = |single: Duration, burst: Duration| {
        (n as f64 * single.as_secs_f64()) / burst.as_secs_f64().max(f64::MIN_POSITIVE)
    };
    eprintln!(
        "  single warm tile:  cyper {cyper_single:.2?}  |  ureq {ureq_single:.2?}   (per-tile floor: RTT + server tile-gen + 13 KiB body)"
    );
    eprintln!(
        "  effective parallelism (n×single ÷ burst):  cyper {:.0}× / {n}  |  ureq {:.0}× / {n}   (→{n} = perfect concurrency, →1 = serial)",
        par(cyper_single, h3),
        par(ureq_single, uq)
    );
    eprintln!(
        "  → h3 ÷ h1.1 (burst time) = {:.2}x  ({})\n",
        h3.as_secs_f64() / uq.as_secs_f64().max(f64::MIN_POSITIVE),
        if h3 < uq {
            "h3/QUIC faster on real bodies"
        } else {
            "h1.1 faster/comparable — bandwidth-bound on these bodies (h3's win is cold-connect/RTT)"
        }
    );

    assert!(
        negotiated.contains('3'),
        "cyper should be on HTTP/3 over QUIC after the Alt-Svc warmup against tile.googleapis.com (got {negotiated})"
    );
    assert!(
        cyper_bytes > 0 && ureq_bytes > 0,
        "both arms must download real tile bytes"
    );
}

/// PAIRED, INTERLEAVED, MANY-ROUND re-measure of the same 32-tile P3DT burst — built to settle whether a
/// "ureq ~2× faster" report is a real transport gap or sampling noise on the weak median-of-4 in
/// [`bench_cyper_vs_ureq_keyed_p3dt`]. Two hardenings over that bench:
///   1. **Paired design** — the two lanes are timed BACK-TO-BACK inside each round (not all-ureq-then-all-cyper),
///      so per-round server/RTT/edge jitter hits both lanes at the same network moment and cancels in the
///      per-round ratio instead of biasing whichever lane happened to run during a slow patch.
///   2. **Distribution, not a point** — `ROUNDS_PAIRED` rounds reported as min/median/p90/mean per lane, plus
///      the per-round paired-ratio spread (p10/median/p90) and a win count. A 408 KiB burst with a ~35 ms
///      RTT+tile-gen floor is dominated by server/network jitter; one median-of-4 sample is mostly noise.
///
/// If the paired ratio clusters at ~1 with a wide p10..p90 band, the "2×" is a tail sample, not a transport gap.
#[ignore = "live network; needs CESIUM_ION_TOKEN (e.g. Intermediate/CesiumIonToken.txt)"]
#[test]
fn bench_interleaved_paired_p3dt() {
    let Ok(token) = std::env::var("CESIUM_ION_TOKEN") else {
        eprintln!("CESIUM_ION_TOKEN not set — skipping the paired P3DT A/B");
        return;
    };
    const ROUNDS_PAIRED: usize = 20;

    let asset = logi_3dtiles_ion::resolve_asset(ASSET_ID, &token).expect("resolve ion asset");
    let root = http_get(&asset.tileset_url).expect("fetch root tileset");
    let urls = collect_glb_urls(&asset.tileset_url, root, WANT_TILES);
    assert!(!urls.is_empty(), "no .glb tile URLs collected from the P3DT tileset");
    let n = urls.len();

    let rt = compio::runtime::Runtime::new().expect("compio runtime");
    rt.block_on(async {
        let client = cyper::Client::new().expect("cyper client");
        // One multiplexed h3 burst → (wall-clock, bytes), same shape as the other bench's `fetch_all`.
        let cyper_burst = |client: cyper::Client, urls: Vec<String>| async move {
            let t0 = Instant::now();
            let mut tasks = Vec::with_capacity(urls.len());
            for u in urls {
                let c = client.clone();
                tasks.push(compio::runtime::spawn(async move {
                    match c.get(&u) {
                        Ok(rb) => match rb.send().await {
                            Ok(resp) => resp.bytes().await.map(|b| b.len() as u64).unwrap_or(0),
                            Err(_) => 0,
                        },
                        Err(_) => 0,
                    }
                }));
            }
            let mut total = 0u64;
            for t in tasks {
                total += t.await.unwrap_or(0);
            }
            (t0.elapsed(), total)
        };

        // Warm both lanes (open keep-alive conns, learn+cache Alt-Svc h3, warm the CDN edge) before measuring.
        let _ = ureq_burst(&urls);
        let _ = cyper_burst(client.clone(), urls.clone()).await;
        let negotiated = match client.get(&urls[0]).expect("url").send().await {
            Ok(r) => format!("{:?}", r.version()),
            Err(e) => format!("ERR {e}"),
        };

        let mut uq = Vec::with_capacity(ROUNDS_PAIRED);
        let mut cy = Vec::with_capacity(ROUNDS_PAIRED);
        let mut ratios = Vec::with_capacity(ROUNDS_PAIRED);
        let mut cyper_wins = 0usize;
        for _ in 0..ROUNDS_PAIRED {
            // PAIRED: ureq burst then cyper burst, back-to-back, so both sample the same network moment.
            let (u_dt, _) = ureq_burst(&urls);
            let (c_dt, _) = cyper_burst(client.clone(), urls.clone()).await;
            ratios.push(c_dt.as_secs_f64() / u_dt.as_secs_f64().max(f64::MIN_POSITIVE));
            if c_dt < u_dt {
                cyper_wins += 1;
            }
            uq.push(u_dt);
            cy.push(c_dt);
        }

        let stats = |label: &str, mut v: Vec<Duration>| {
            v.sort();
            let min = v[0];
            let med = v[v.len() / 2];
            let p90 = v[(v.len() * 9 / 10).min(v.len() - 1)];
            let mean = v.iter().sum::<Duration>() / v.len() as u32;
            eprintln!("  {label:<24} min {min:.2?}  median {med:.2?}  p90 {p90:.2?}  mean {mean:.2?}");
        };
        ratios.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let rmed = ratios[ratios.len() / 2];
        let rp10 = ratios[ratios.len() / 10];
        let rp90 = ratios[(ratios.len() * 9 / 10).min(ratios.len() - 1)];

        eprintln!(
            "\n=== Paired interleaved P3DT A/B — {n} tiles × {ROUNDS_PAIRED} paired rounds (proto {negotiated}) ==="
        );
        stats("cyper (HTTP/3 burst)", cy);
        stats("ureq  (HTTP/1.1 burst)", uq);
        eprintln!("  per-round ratio h3÷h1.1:   p10 {rp10:.2}x   median {rmed:.2}x   p90 {rp90:.2}x   (1.0 = parity)");
        eprintln!("  cyper faster in {cyper_wins}/{ROUNDS_PAIRED} paired rounds\n");
        assert!(
            negotiated.contains('3'),
            "expected HTTP/3 after warmup (got {negotiated})"
        );
    });
}

/// COLD-vs-WARM connect decomposition — isolates the handshake cost each lane pays, and empirically checks the
/// claim that our cyper lane bootstraps over **h2 (TCP+TLS)** on first contact (NOT a QUIC 1-RTT connect) and
/// only reaches h3 once Alt-Svc has upgraded the host. For each lane it times a request on a FRESH client (cold
/// = full handshake) vs a reused one (warm = RTT + server tile-gen only); `cold − warm ≈ the handshake`. If the
/// cold cyper request reports HTTP/2 and its cold time ≈ ureq's, that proves we forfeit QUIC's 1-RTT-connect
/// edge on first contact — the number h3 *could* move (via keep-alive / 0-RTT / forced-h3) lives here, NOT in
/// the warm burst (which is floored by RTT + Google's tile-production time, transport-invariant).
#[ignore = "live network; needs CESIUM_ION_TOKEN (e.g. Intermediate/CesiumIonToken.txt)"]
#[test]
fn bench_cold_vs_warm_connect() {
    use std::io::Read;
    let Ok(token) = std::env::var("CESIUM_ION_TOKEN") else {
        eprintln!("CESIUM_ION_TOKEN not set — skipping the cold/warm connect bench");
        return;
    };
    const REPS: usize = 10;

    let asset = logi_3dtiles_ion::resolve_asset(ASSET_ID, &token).expect("resolve ion asset");
    let root = http_get(&asset.tileset_url).expect("fetch root tileset");
    let urls = collect_glb_urls(&asset.tileset_url, root, WANT_TILES);
    assert!(!urls.is_empty(), "no .glb tile URLs collected");
    let url = urls[0].clone();

    // ureq: a FRESH agent (empty pool) → cold TCP+TLS handshake; the same agent's 2nd GET → warm keep-alive reuse.
    fn ureq_fetch(agent: &ureq::Agent, url: &str) -> usize {
        match agent.get(url).call() {
            Ok(resp) => {
                let mut v = Vec::new();
                let _ = resp.into_reader().read_to_end(&mut v);
                v.len()
            }
            Err(_) => 0,
        }
    }
    let mut uq_cold = Vec::with_capacity(REPS);
    let mut uq_warm = Vec::with_capacity(REPS);
    for _ in 0..REPS {
        let agent = ureq::AgentBuilder::new().build();
        let t = Instant::now();
        let _ = ureq_fetch(&agent, &url);
        uq_cold.push(t.elapsed());
        let t = Instant::now();
        let _ = ureq_fetch(&agent, &url);
        uq_warm.push(t.elapsed());
    }

    // cyper: a FRESH Client (empty Alt-Svc cache) → first request bootstraps over h2/TCP+TLS; after 2 warmup
    // requests Alt-Svc has upgraded the host, so the measured "warm" request is h3-over-QUIC.
    let rt = compio::runtime::Runtime::new().expect("compio runtime");
    let (cy_cold, cy_warm, cold_proto, warm_proto) = rt.block_on(async {
        let fetch = |client: &cyper::Client, url: &str| {
            let fut = client.get(url);
            async move {
                match fut {
                    Ok(rb) => match rb.send().await {
                        Ok(resp) => {
                            let v = format!("{:?}", resp.version());
                            let _ = resp.bytes().await;
                            v
                        }
                        Err(e) => format!("ERR {e}"),
                    },
                    Err(e) => format!("ERR {e}"),
                }
            }
        };
        let mut cold = Vec::with_capacity(REPS);
        let mut warm = Vec::with_capacity(REPS);
        let mut cold_proto = String::new();
        let mut warm_proto = String::new();
        for i in 0..REPS {
            let client = cyper::Client::new().expect("cyper client");
            let t = Instant::now();
            let p = fetch(&client, &url).await; // COLD: h2 bootstrap on a fresh client
            let dt = t.elapsed();
            if i == 0 {
                cold_proto = p;
            }
            cold.push(dt);
            // Warm up: learn + apply the Alt-Svc h3 upgrade on this client.
            let _ = fetch(&client, &url).await;
            let _ = fetch(&client, &url).await;
            let t = Instant::now();
            let p = fetch(&client, &url).await; // WARM: now h3 over QUIC
            let dt = t.elapsed();
            if i == 0 {
                warm_proto = p;
            }
            warm.push(dt);
        }
        (cold, warm, cold_proto, warm_proto)
    });

    let med = |v: &[Duration]| {
        let mut v = v.to_vec();
        v.sort();
        v[v.len() / 2]
    };
    let (uqc, uqw) = (med(&uq_cold), med(&uq_warm));
    let (cyc, cyw) = (med(&cy_cold), med(&cy_warm));
    eprintln!("\n=== Cold-vs-warm connect — {REPS} reps, one real P3DT tile ===");
    eprintln!(
        "  ureq   cold {uqc:.2?}  warm {uqw:.2?}   → handshake ≈ {:.2?}  (TCP+TLS1.3)",
        uqc.saturating_sub(uqw)
    );
    eprintln!(
        "  cyper  cold {cyc:.2?} ({cold_proto})  warm {cyw:.2?} ({warm_proto})   → handshake ≈ {:.2?}",
        cyc.saturating_sub(cyw)
    );
    eprintln!(
        "  → cold cyper÷ureq = {:.2}x ;  warm cyper÷ureq = {:.2}x   (cold cyper is the h2 bootstrap, NOT a QUIC connect)\n",
        cyc.as_secs_f64() / uqc.as_secs_f64().max(f64::MIN_POSITIVE),
        cyw.as_secs_f64() / uqw.as_secs_f64().max(f64::MIN_POSITIVE),
    );
    assert!(
        cyw > Duration::ZERO && uqw > Duration::ZERO,
        "both warm paths must fetch"
    );
}

/// DRIFT-RESISTANT warm single-tile A/B — the clean measure for the h3 warm penalty (and the `windows-gro`
/// effect). Per rep it times one warm ureq GET and one warm h3 GET BACK-TO-BACK (same network moment), so the
/// per-rep `cyper÷ureq` ratio cancels network drift; reports the ratio distribution over many reps. Run it
/// twice — once with `windows-gro` in the compio feature list, once without — and compare the median warm
/// ratio: a lower ratio with GRO on means receive-coalescing closed the per-tile userspace-recv penalty.
#[ignore = "live network; needs CESIUM_ION_TOKEN (e.g. Intermediate/CesiumIonToken.txt)"]
#[test]
fn bench_warm_single_interleaved() {
    let Ok(token) = std::env::var("CESIUM_ION_TOKEN") else {
        eprintln!("CESIUM_ION_TOKEN not set — skipping the warm single A/B");
        return;
    };
    const REPS: usize = 24;

    let asset = logi_3dtiles_ion::resolve_asset(ASSET_ID, &token).expect("resolve ion asset");
    let root = http_get(&asset.tileset_url).expect("fetch root tileset");
    let urls = collect_glb_urls(&asset.tileset_url, root, WANT_TILES);
    assert!(!urls.is_empty(), "no .glb tile URLs collected");
    let url = urls[0].clone();

    let rt = compio::runtime::Runtime::new().expect("compio runtime");
    let (proto, ratios, uq_med, cy_med) = rt.block_on(async {
        let client = cyper::Client::new().expect("cyper client");
        let cy_fetch = |client: &cyper::Client, url: &str| {
            let rb = client.get(url);
            async move {
                if let Ok(rb) = rb {
                    if let Ok(resp) = rb.send().await {
                        let v = format!("{:?}", resp.version());
                        let _ = resp.bytes().await;
                        return v;
                    }
                }
                String::new()
            }
        };
        // Warm BOTH lanes: 3 h3 requests upgrade the client past the h2 bootstrap; ureq's shared agent pool was
        // already warmed by collect_glb_urls. Then probe the negotiated protocol (must be h3).
        for _ in 0..3 {
            let _ = cy_fetch(&client, &url).await;
        }
        let _ = http_get(&url);
        let proto = cy_fetch(&client, &url).await;

        let mut ratios = Vec::with_capacity(REPS);
        let mut uq = Vec::with_capacity(REPS);
        let mut cy = Vec::with_capacity(REPS);
        for _ in 0..REPS {
            // PAIRED per rep: ureq warm single, then h3 warm single, back-to-back.
            let t = Instant::now();
            let _ = http_get(&url);
            let u = t.elapsed();
            let t = Instant::now();
            let _ = cy_fetch(&client, &url).await;
            let c = t.elapsed();
            ratios.push(c.as_secs_f64() / u.as_secs_f64().max(f64::MIN_POSITIVE));
            uq.push(u);
            cy.push(c);
        }
        uq.sort();
        cy.sort();
        (proto, ratios, uq[uq.len() / 2], cy[cy.len() / 2])
    });

    let mut r = ratios;
    r.sort_by(|a, b| a.partial_cmp(b).unwrap());
    eprintln!("\n=== Warm single-tile interleaved A/B — {REPS} reps (proto {proto}) ===");
    eprintln!("  ureq  warm median {uq_med:.2?}   |   cyper(h3) warm median {cy_med:.2?}");
    eprintln!(
        "  per-rep ratio h3÷ureq:   p10 {:.2}x   median {:.2}x   p90 {:.2}x   (<1 = h3 faster)",
        r[r.len() / 10],
        r[r.len() / 2],
        r[(r.len() * 9 / 10).min(r.len() - 1)],
    );
    assert!(proto.contains('3'), "warm path must be h3 (got {proto})");
}

/// KEEP-ALIVE IDLE-SURVIVAL probe — the live proof for the vendored-cyper `TransportConfig` seam: with
/// `keep_alive_interval = 10 s` (the production transport, `build_h3_client` in `cyper_source.rs`), does the
/// pooled QUIC tile connection survive an idle gap longer than quinn's 30 s default `max_idle_timeout`?
///
/// Two sequential arms on one reactor, each with its own client:
///   - **KA-ON** — `Client::builder().h3_transport_config(keep-alive 10 s)`: the production transport.
///   - **KA-OFF** — stock `Client::new()`: quinn's default (`keep_alive_interval = None`), the pre-patch lane.
///
/// Each arm: 3 warmup fetches (h2 bootstrap → Alt-Svc → h3), one timed warm fetch, **35 s idle**, then the
/// timed post-idle fetch (+ a re-warm control). The idle MUST sleep on the blocking pool
/// (`spawn_blocking(|| thread::sleep)`) — a blocking sleep ON the reactor would stall quinn's timer driver and
/// suppress the very keep-alive packets under test.
///
/// Read-out: `post-idle − warm` ≈ 0 ⇒ the connection survived; ≈ +1 RTT ⇒ idle teardown + fresh QUIC
/// handshake; an `ERR` ⇒ the pool surfaced the dead connection (worse than a slow fetch: after every idle gap
/// a render-critical tile would fail into the retry ladder). The post-idle proto string also shows whether a
/// reconnect rides the cached Alt-Svc straight back to h3 or re-bootstraps over h2 (TCP+TLS). Asserts only on
/// the production arm: KA-ON must stay h3 and succeed post-idle.
#[ignore = "live network + ~80 s of idling; needs CESIUM_ION_TOKEN (e.g. Intermediate/CesiumIonToken.txt)"]
#[test]
fn bench_keepalive_idle_survival() {
    let Ok(token) = std::env::var("CESIUM_ION_TOKEN") else {
        eprintln!("CESIUM_ION_TOKEN not set — skipping the keep-alive idle-survival probe");
        return;
    };
    // Past quinn's 30 s default max_idle_timeout, with margin for timer slop.
    const IDLE: Duration = Duration::from_secs(35);
    // Mirrors the production `CYPER_KEEP_ALIVE` (private to cyper_source.rs).
    const KEEP_ALIVE: Duration = Duration::from_secs(10);

    let asset = logi_3dtiles_ion::resolve_asset(ASSET_ID, &token).expect("resolve ion asset");
    let root = http_get(&asset.tileset_url).expect("fetch root tileset");
    let urls = collect_glb_urls(&asset.tileset_url, root, WANT_TILES);
    assert!(!urls.is_empty(), "no .glb tile URLs collected");
    let url = urls[0].clone();

    // Nested `async fn`s (not async closures): the closure form borrows `client`/`url` for the closure's
    // lifetime and trips the borrow checker when reused across awaits; plain items dodge that entirely.
    async fn timed_fetch(client: &cyper::Client, url: &str) -> (Duration, String) {
        let t = Instant::now();
        let proto = match client.get(url) {
            Ok(rb) => match rb.send().await {
                Ok(resp) => {
                    let v = format!("{:?}", resp.version());
                    let _ = resp.bytes().await;
                    v
                }
                Err(e) => format!("ERR {e}"),
            },
            Err(e) => format!("ERR {e}"),
        };
        (t.elapsed(), proto)
    }
    // Sleep OFF the reactor: `spawn_blocking` parks a blocking-pool thread while the reactor keeps driving
    // quinn's timers — which is what actually emits the keep-alives during the idle.
    async fn idle_sleep(d: Duration) {
        let _ = compio::runtime::spawn_blocking(move || std::thread::sleep(d)).await;
    }
    // One arm: warm ×3 (past the h2 bootstrap) → timed warm → idle → timed post-idle → timed re-warm control.
    async fn run_arm(client: &cyper::Client, url: &str, idle: Duration) -> [(Duration, String); 3] {
        for _ in 0..3 {
            let _ = timed_fetch(client, url).await;
        }
        let warm = timed_fetch(client, url).await;
        idle_sleep(idle).await;
        let post_idle = timed_fetch(client, url).await;
        let rewarm = timed_fetch(client, url).await;
        [warm, post_idle, rewarm]
    }

    let rt = compio::runtime::Runtime::new().expect("compio runtime");
    let (ka_on, ka_off) = rt.block_on(async {
        // KA-ON: the same transport shape `build_h3_client` ships (keep-alive via the vendored seam).
        let mut tc = compio::quic::TransportConfig::default();
        tc.keep_alive_interval(Some(KEEP_ALIVE));
        let on = cyper::Client::builder()
            .h3_transport_config(std::sync::Arc::new(tc))
            .build()
            .expect("keep-alive client");
        // KA-OFF: stock cyper — quinn defaults, no keep-alive (the pre-patch behaviour).
        let off = cyper::Client::new().expect("stock client");
        (run_arm(&on, &url, IDLE).await, run_arm(&off, &url, IDLE).await)
    });

    let fmt_arm = |arm: &[(Duration, String); 3]| {
        format!(
            "warm {:.2?} ({})   post-idle {:.2?} ({})   re-warm {:.2?} ({})",
            arm[0].0, arm[0].1, arm[1].0, arm[1].1, arm[2].0, arm[2].1
        )
    };
    eprintln!("\n=== Keep-alive idle-survival — one P3DT tile, {IDLE:?} idle vs quinn's 30 s idle timeout ===");
    eprintln!("  KA-ON  ({KEEP_ALIVE:?} keep-alive):    {}", fmt_arm(&ka_on));
    eprintln!("  KA-OFF (stock, no keep-alive): {}", fmt_arm(&ka_off));
    eprintln!(
        "  post-idle − warm:  KA-ON {:.2?}  |  KA-OFF {:.2?}   (≈0 = survived idle; ≈RTT = re-handshake; ERR = dead conn surfaced)\n",
        ka_on[1].0.saturating_sub(ka_on[0].0),
        ka_off[1].0.saturating_sub(ka_off[0].0),
    );

    assert!(
        ka_on[0].1.contains('3') && ka_on[1].1.contains('3'),
        "KA-ON arm must be h3 both warm and post-idle (got warm {} / post-idle {})",
        ka_on[0].1,
        ka_on[1].1
    );
    assert!(
        !ka_on[1].1.starts_with("ERR"),
        "KA-ON post-idle fetch must succeed (got {})",
        ka_on[1].1
    );
}
