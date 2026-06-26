//! `CyperSource` — the **HTTP/3-over-QUIC tile-fetch backend** (ADR-0014 I/O lane, feature `cyper-h2`): a
//! `cyper::Client` on ONE dedicated owned compio thread, multiplexing many tile streams over a few pooled
//! connections instead of one TCP connection + TLS handshake per request. With the `http3`/`http3-altsvc`
//! features it negotiates **HTTP/3 over QUIC** (compio-quic = `quinn-proto`, the sans-IO QUIC state machine,
//! driven on the compio completion reactor — **no tokio**) via Alt-Svc auto-upgrade: the first request to a
//! host goes HTTP/2, the response's `Alt-Svc: h3=":443"` is cached on the (long-lived) `Client`, and every
//! subsequent request to that host switches to h3. So for Google P3DT (`tile.googleapis.com`, which serves
//! h3) the steady-state stream is HTTP/3; HTTP/2 stays the bootstrap + the fallback for non-h3 hosts.
//!
//! The Exp-B research established the transport itself: the live keyed-P3DT A/B (`tests/cyper_p3dt_bench.rs`)
//! measured the h2/h3 win as a **handshake/RTT collapse** — large on cold connects (~4–7× on empty 204s),
//! ~parity on warm real-body bursts — so the multiplexing + faster QUIC handshake win on cold starts /
//! teleports / many-host fan-out, not steady-state bandwidth (see the plan §7.1).
//!
//! ## Why a separate thread (not the bare proactor on the sim loop)
//! `compio_http_bench.rs`'s finding: socket I/O over compio realistically goes through **compio-net on
//! compio's own thread-per-core runtime** (the bare `Proactor` is for file/custom ops). So the I/O lane is
//! *compio-net on a Rust-owned thread, draining completed work to the owner via a channel* — ADR-0007/0012
//! legal (a separate runtime on a Rust-owned thread; gameplay stays fixed-step deterministic elsewhere).
//! The geo lane is non-authoritative, so async / wall-clock / OS-threads are fine here.
//!
//! ## Shape (mirrors [`HttpSource`] exactly so the owner is backend-agnostic)
//! The owner thread (`request`/`poll_completed`/`cancel`) never touches cyper: the `Client`, every spawned
//! request task, the pending-request priority heap, and the `id → JoinHandle` map all live **only on the
//! runtime thread** (the runtime is `Rc`-based / `!Send`). Owner ↔ runtime thread talk over two `flume` channels:
//!   * **commands** (owner → runtime): [`Cmd::Request`] / [`Cmd::Cancel`] / [`Cmd::Shutdown`]. The runtime
//!     thread also posts [`Cmd::Completed`] onto this same channel from finished tasks (the completion-pump wake).
//!   * **results** (runtime → owner): [`Loaded`], drained in `poll_completed` — identical to `HttpSource`'s
//!     `done_rx`, including the `in_flight` bookkeeping + soft-drop backstop.
//!
//! ## Priority + in-flight budget (mirrors the ureq lane's heap)
//! The runtime loop keeps pending requests in a max-heap by the owner's `fetch_priority` and admits at most
//! [`CYPER_STREAM_BUDGET`] as live streams at once; the surplus waits, highest-priority first. So the owner's
//! render-critical ordering governs h3 service order under contention (it was discarded before — every request
//! streamed immediately). A completion frees a slot and pumps the next pending request in.
//!
//! ## Cancellation (the stream-reset path)
//! `cancel(id)` sends [`Cmd::Cancel`]. If the request was admitted, the loop **drops its `JoinHandle`**, and
//! compio-executor's `JoinHandle::drop` cancels the task (`task.cancel(true)`), which drops the in-flight
//! cyper future → the underlying stream is reset (QUIC stream reset on h3, RST_STREAM on h2) instead of
//! finishing a download the owner discarded. If it was still pending (never admitted), it is dropped from the
//! heap's live set so the pump skips it. Either way the loop synthesises the one `Loaded { Err(Cancelled) }`
//! the request must yield, so the **exactly-one-result-per-request** contract holds (same as `HttpSource`).
//!
//! ## Cache
//! Reuses the transport-independent [`DiskCache`] cache-first, exactly like [`fetch_one`]: consult by
//! [`cache_key`], serve a hit with no network, else fetch over the negotiated protocol (h3/h2) and populate
//! on a non-empty success. A classified failure / empty-200 is never cached — the same invariant as the ureq
//! lane. The cache's sync `std::fs` get/put run on a `spawn_blocking` thread, NOT inline on the reactor, so a
//! slow read/write never head-of-line-blocks the other in-flight QUIC streams (lever L1).

use std::collections::BinaryHeap;
use std::sync::Arc;
use std::thread::JoinHandle as ThreadJoinHandle;

use hashbrown::HashMap;

use crate::{
    cache_key, parse_retry_after, DiskCache, FetchError, Loaded, RequestId, TileContentSource, MAX_TILE_BYTES,
};

/// Keep-alive interval for the h3/QUIC tile connection. quinn's default `max_idle_timeout` is 30 s and its
/// default `keep_alive_interval` is `None` (OFF) — so a pooled connection that goes idle (camera paused, a gap
/// between teleports) is torn down at 30 s, forcing a fresh QUIC handshake on the next render-critical tile.
/// 10 s (≈ idle/3, tolerant of two lost keep-alive packets, and under the ~30 s NAT-binding expiry) holds the
/// connection warm. The timer is reset by ANY activity, so it costs nothing during active streaming and only
/// emits a packet during a genuine idle gap — exactly when we want to avoid the reconnect (RFC 9000 §10.1.2).
const CYPER_KEEP_ALIVE: std::time::Duration = std::time::Duration::from_secs(10);

/// Build the pooled cyper client with an HTTP/3 transport tuned for a long-lived, reused tile connection: set
/// `keep_alive_interval` (the one knob upstream cyper leaves at quinn's `None` default) via the vendored cyper
/// `TransportConfig` seam ([`ClientBuilder::h3_transport_config`], Rust/third_party/cyper). All other transport
/// settings stay at quinn's defaults; the same seam accepts initial_rtt / congestion-controller tuning later.
fn build_h3_client() -> cyper::Result<cyper::Client> {
    let mut tc = compio::quic::TransportConfig::default();
    tc.keep_alive_interval(Some(CYPER_KEEP_ALIVE));
    cyper::Client::builder().h3_transport_config(Arc::new(tc)).build()
}

/// Concurrent in-flight **stream** budget for the h2/h3 client — now ENFORCED on the runtime thread (it caps
/// `live`, the set of admitted streaming tasks), not just an advisory sizing hint. When more than this are
/// outstanding the surplus waits in a PRIORITY heap and the highest-`fetch_priority` tiles stream first (so the
/// owner's render-critical ordering governs h3 service order — it was discarded before). On ONE multiplexed
/// connection all admitted streams share one congestion window, so 64 already keeps that window full; admitting
/// hundreds more would only round-robin the same bandwidth more thinly while losing the ordering lever. Also
/// sizes the owner's outstanding-fetch cap via [`fetch_concurrency`](CyperSource::fetch_concurrency) (audit H2).
/// 64 matches the live-bench burst width (`cyper_h2_bench.rs` `N = 64`); measure-gated (CLAUDE.md §1B).
const CYPER_STREAM_BUDGET: usize = 64;

/// Cap on compio's `spawn_blocking` AsyncifyPool — the threads that run the synchronous `DiskCache`
/// `get_raw`/`put_raw` off the reactor (L1). compio's DEFAULT limit is **256** (60 s idle TTL), so a
/// streaming burst (up to [`CYPER_STREAM_BUDGET`] streams × a get on entry + a put on success) could
/// transiently spawn dozens of OS threads entirely OUTSIDE the physical-core budget the compute pools
/// honor — the single largest unbudgeted thread source on the machine (re-eval 2026-06-09). The cache is
/// local-SSD I/O: a handful of blocking threads saturates it, and queueing behind them is cheaper than
/// thread-thrashing the cores UE + the compute pool are using.
const CYPER_BLOCKING_POOL_LIMIT: usize = 4;

/// Commands the owner thread sends to the runtime thread over the command channel. One per owner call; the
/// runtime loop parks on `recv_async` until one arrives (no busy-spin).
enum Cmd {
    /// Fetch `url` for request `id` at `priority` (the owner's `fetch_priority`). The runtime loop pushes it on
    /// a PRIORITY heap; when the in-flight budget has room the highest-priority pending request is admitted
    /// first (so render-critical tiles stream before speculative prefetch under contention).
    Request { id: RequestId, url: String, priority: f64 },
    /// Cancel request `id`. If admitted (streaming): drop its task's `JoinHandle` (→ RST_STREAM). If still
    /// pending (never admitted): drop it from the heap's live set so the pump skips it. Either way synthesise
    /// its one `Loaded { Err(Cancelled) }` to keep the exactly-one-result contract.
    Cancel(RequestId),
    /// Internal completion signal: a spawned task sends this (on a clone of the command channel) AFTER it has
    /// delivered its `Loaded`, so the loop reaps its `JoinHandle` from `live` AND — the reason it rides the
    /// command channel rather than a side channel — WAKES the loop to admit the next pending request off the
    /// heap into the freed budget slot. Merging it here keeps the loop a single await point (no select, no
    /// busy-spin) while still pumping promptly on completion, not only on the next owner command.
    Completed(RequestId),
    /// Teardown: stop the loop, abort every in-flight task, let the runtime thread exit + be joined.
    Shutdown,
}

/// Panic-path completion guard for a spawned fetch task (re-eval 2026-06-09). Its `Drop` emits the task's
/// one `Loaded { Err(Transport) }` + `Cmd::Completed` **only while unwinding** (`std::thread::panicking()`),
/// so a panicking `fetch_one_h2` cannot silently leak a stream-budget slot + an owner in-flight entry. A
/// normal completion or a cancel-drop (which already synthesised `Cancelled`) keeps it silent.
struct PanicCompletion {
    id: RequestId,
    done_tx: flume::Sender<Loaded>,
    cmd_tx: flume::Sender<Cmd>,
}
impl Drop for PanicCompletion {
    fn drop(&mut self) {
        if std::thread::panicking() {
            let _ = self.done_tx.send(Loaded {
                id: self.id,
                result: Err(FetchError::Transport("fetch task panicked".to_string())),
            });
            let _ = self.cmd_tx.send(Cmd::Completed(self.id));
        }
    }
}

/// One pending (not-yet-admitted) request in the runtime thread's PRIORITY heap. The loop pops the MAX so the
/// owner's `fetch_priority` (carried through [`Cmd::Request`]) decides which tiles stream first when more than
/// [`CYPER_STREAM_BUDGET`] are outstanding. `seq` breaks ties FIFO-fairly (older first) and supplies the total
/// order `f64` lacks. Mirrors the ureq lane's `PrioReq` so both backends order service identically.
struct PrioReq {
    score: f64,
    seq: u64,
    id: RequestId,
    url: String,
}
impl Ord for PrioReq {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // Max-heap pops the GREATEST: higher score first; equal score → smaller `seq` is "greater" (older first).
        self.score
            .total_cmp(&other.score)
            .then_with(|| other.seq.cmp(&self.seq))
    }
}
impl PartialOrd for PrioReq {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl PartialEq for PrioReq {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other).is_eq()
    }
}
impl Eq for PrioReq {}

/// One worker-thread async fetch of `url` → a TYPED outcome, cache-first, mirroring [`fetch_one`] for the
/// h2 transport. A disk-cache hit costs no network; a miss fetches over the shared multiplexed `client` and
/// populates the cache on a non-empty success. Classification matches [`HttpSource`] precisely (the owner's
/// retry logic depends on it): 429 / 5xx / transport = transient; 404 / 403 / 400 / over-cap = terminal.
async fn fetch_one_h2(
    client: &cyper::Client,
    cache: Option<&Arc<DiskCache>>,
    url: &str,
) -> Result<Arc<[u8]>, FetchError> {
    // Cache-first (audit L6: hash the URL path to the key ONCE, then key-based get/put). A hit is the cheap
    // common path on a re-visit; serve it with zero bytes on the wire. The disk read is sync `std::fs` + a
    // `Mutex` (`DiskCache::get_raw`) — run it on a BLOCKING thread via `spawn_blocking`, NOT inline on this
    // reactor (lever L1). Every in-flight QUIC stream is multiplexed on this ONE runtime thread, so a cold /
    // contended / network-FS / AV-scanned read inline here would stall EVERY other stream until it returned
    // (a cooperative head-of-line block — the very thing multiplexing exists to avoid). Offloading suspends
    // only THIS fetch task; the other streams keep flowing. `spawn_blocking`'s `JoinHandle` resolves to `Err`
    // only if the blocking task panicked — treat that, like a genuine miss, as "no hit, go to network".
    let key = cache.map(|_| cache_key(url));
    if let (Some(c), Some(k)) = (cache, key) {
        let c = Arc::clone(c);
        if let Ok(Some(bytes)) = compio::runtime::spawn_blocking(move || c.get_raw(k)).await {
            return Ok(Arc::from(bytes));
        }
    }

    // h2/h3 GET over the pooled, multiplexed client. cyper does NOT map a non-2xx status to `Err` (like reqwest):
    // `.send()` / `.bytes()` only error on the TRANSPORT (connect/TLS/read/reset/timeout) — a 4xx/5xx is a
    // successful `Response` we must classify ourselves on `status()`.
    let exchange = async {
        let resp = client
            .get(url)
            .map_err(classify_send_err)?
            .send()
            .await
            .map_err(classify_send_err)?;

        let status = resp.status();
        // Reject a declared over-cap body BEFORE collecting it (C1, audit 2026-06-10): `bytes()` below buffers
        // the WHOLE body in RAM, so without this pre-check a hostile/misconfigured server could make us
        // allocate gigabytes before the post-collect `MAX_TILE_BYTES` test fired — the ureq lane streams
        // through a chunked wall and never had this window. `Content-Length` is untrusted (absent on chunked
        // transfer, and a lying server can understate it), so the post-collect check below REMAINS the
        // authoritative wall; this just refuses the honest-oversized case at zero cost.
        if resp
            .headers()
            .get("content-length")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok())
            .is_some_and(|len| len > MAX_TILE_BYTES as u64)
        {
            return Err(FetchError::TooLarge);
        }
        if !status.is_success() {
            // Pull the server's `Retry-After` hint off the response (429/503 carry it) so the owner honors it
            // instead of hammering back — same as the ureq path. Only the delta-seconds form is parsed.
            // `HeaderMap::get` takes any `AsHeaderName`, so the `&str` name avoids pulling the `http` crate in as
            // a direct dependency just for the `RETRY_AFTER` constant (it stays a transitive dep of cyper).
            let retry_after = resp
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(parse_retry_after);
            return Err(FetchError::Http {
                status: status.as_u16(),
                retry_after,
            });
        }

        // Body. cyper's `bytes()` collects the whole (transparently gzip-inflated, per the `gzip` accept) body.
        // It has no streaming size-cap hook like the ureq chunk loop, so we enforce [`MAX_TILE_BYTES`] AFTER the
        // collect: a hostile/misconfigured server streaming an unbounded payload is rejected as `TooLarge`
        // (terminal) rather than handed up. (The h2/h3 flow-control window already bounds in-flight bytes per
        // stream, so this is a backstop, not the only guard.) An empty-200 stays `Ok(empty)` — a 0-byte tile is
        // a success, not a failure (audit A10).
        resp.bytes().await.map_err(classify_send_err)
    };
    // Bound the WHOLE exchange (connect/send/headers/body) like the ureq lane's `HTTP_OVERALL_TIMEOUT`. QUIC's
    // 30 s `max_idle_timeout` only reaps a DEAD connection — our 10 s keep-alive holds a stalled-but-ALIVE
    // stream's connection warm indefinitely, so without this cap a server that stops sending mid-body would
    // hang this fetch (and its in-flight slot) forever. Elapsed maps to a TRANSIENT transport error, identical
    // to a ureq timeout, so the owner's backoff/retry semantics stay lane-congruent.
    let body = match compio::time::timeout(crate::HTTP_OVERALL_TIMEOUT, exchange).await {
        Ok(r) => r?,
        Err(_) => {
            return Err(FetchError::Transport(format!(
                "request exceeded the {:?} overall timeout",
                crate::HTTP_OVERALL_TIMEOUT
            )))
        }
    };
    if body.len() > MAX_TILE_BYTES {
        return Err(FetchError::TooLarge);
    }
    // ONE copy/alloc: `bytes::Bytes` → `Arc<[u8]>` directly (was `to_vec()` then the caller's `Arc::from` =
    // two, lever L5). The owner parks this `Arc` straight onto the tile entity with a cheap refcount move
    // (no re-copy) in `drain_raw`.
    let arc: Arc<[u8]> = Arc::from(body.as_ref());

    if let (Some(c), Some(k)) = (cache, key) {
        if !arc.is_empty() {
            // Populate the cache on a BLOCKING thread too (lever L1): the sync `std::fs` write + evict must
            // not stall the other in-flight streams on this reactor. FIRE-AND-FORGET — the handle is dropped,
            // but `spawn_blocking` tasks are NOT cancelled on drop (they run to completion), so the write still
            // lands; we don't await it (the bytes are already on their way to the owner). Shares the `Arc` (a
            // refcount bump, no byte copy).
            let c = Arc::clone(c);
            let arc = Arc::clone(&arc);
            // Explicit `drop` (not `let _ =`): detaches the handle. A `spawn_blocking` task is NOT cancelled on
            // handle-drop (unlike an async `spawn`), so the write still runs — exactly the fire-and-forget intent.
            drop(compio::runtime::spawn_blocking(move || c.put_raw(k, &arc)));
        }
    }
    Ok(arc)
}

/// Map a `cyper::Error` (transport / client-construction failure — never an HTTP status, which cyper surfaces
/// as a successful `Response`) to our classified [`FetchError`]. A bad URL is **terminal** ([`FetchError::Url`]);
/// every wire/timeout/TLS failure is **transient** ([`FetchError::Transport`]) — identical
/// transient-vs-terminal semantics to the ureq lane's `ureq::Error` split, which the owner's backoff depends on.
fn classify_send_err(e: cyper::Error) -> FetchError {
    match e {
        // A URL that won't parse / a scheme cyper can't speak is bad every time — terminal, like ureq's
        // bad-URL path. (`InvalidUrl` carries the parsed `Url`; the others carry strings.)
        cyper::Error::InvalidUrl(u) => FetchError::Url(u.to_string()),
        cyper::Error::UrlParse(e) => FetchError::Url(e.to_string()),
        cyper::Error::BadScheme(s) => FetchError::Url(format!("bad scheme: {s}")),
        // Everything else is a transport-level hiccup (connect/read/write timeout, DNS, TLS, connection
        // reset, an h2/hyper protocol error) → transient, exactly like `ureq::Error::Transport`. The string
        // is telemetry only (never matched on), so collapsing the variants here loses nothing the owner uses.
        other => FetchError::Transport(other.to_string()),
    }
}

/// A worker-pool-shaped HTTP/3-over-QUIC [`TileContentSource`] backed by a single compio runtime thread
/// running a multiplexed `cyper::Client` (feature `cyper-h2`, the default; negotiates h3 via Alt-Svc with h2
/// bootstrap/fallback). Feature-gated: a `--no-default-features` cdylib pulls none of the h2/h3/quinn-proto
/// stack and uses the `ureq` [`HttpSource`] instead. Cheap to construct, joins its runtime thread cleanly on `Drop`.
///
/// [`HttpSource`]: crate::HttpSource
pub struct CyperSource {
    /// Owner → runtime-thread command channel. `request`/`cancel` push here; the runtime loop drains it.
    cmd_tx: flume::Sender<Cmd>,
    /// Runtime → owner results channel, drained in `poll_completed`. Mirrors `HttpSource::done_rx`.
    done_rx: flume::Receiver<Loaded>,
    /// The dedicated runtime thread handle, joined on `Drop` so teardown never leaves it (and its `Client` +
    /// `Arc<DiskCache>` clone) running detached — mirrors `HttpSource`'s join-on-`Drop`.
    runtime_thread: Option<ThreadJoinHandle<()>>,
    /// Live in-flight request ids (owner-side), exactly as `HttpSource::in_flight`. `request` inserts;
    /// `poll_completed` removes on drain; `cancel` removes immediately (so a result that beat the cancel onto
    /// the channel is soft-dropped as stale — the backstop).
    in_flight: hashbrown::HashSet<RequestId>,
    /// Disk cache handle (telemetry mirror so `cached_bytes`/`cache_hits`/`cache_misses` answer on the owner
    /// thread without a round-trip). `None` ⇒ no cache. The runtime thread holds its own `Arc` clone.
    cache: Option<Arc<DiskCache>>,
}

impl CyperSource {
    /// Spawn the runtime thread + h2 `Client`, optionally backed by a persistent [`DiskCache`]. Construction
    /// fails only if the runtime thread can't build its compio runtime or cyper client — surfaced as an
    /// `io::Error` so the caller can fall back to [`HttpSource`].
    ///
    /// [`HttpSource`]: crate::HttpSource
    pub fn new(cache: Option<Arc<DiskCache>>) -> std::io::Result<Self> {
        let (cmd_tx, cmd_rx) = flume::unbounded::<Cmd>();
        let (done_tx, done_rx) = flume::unbounded::<Loaded>();
        // A one-shot "did the runtime + client build?" handshake so `new` reports a clean error instead of
        // returning a source whose thread silently died (the owner could then fall back to HttpSource).
        let (ready_tx, ready_rx) = flume::bounded::<std::io::Result<()>>(1);

        let thread_cache = cache.clone();
        // The runtime thread holds a clone of the command SENDER too, so its spawned fetch tasks can post
        // `Cmd::Completed` back onto the same channel the loop awaits (the completion-pump wake). One
        // consequence: `cmd_rx` no longer closes when the owner drops its `cmd_tx` (the thread's clone keeps it
        // open), so teardown relies on the explicit `Cmd::Shutdown` that `Drop` always sends — never on a
        // channel-close. (The `recv_async` error arm stays as a defensive backstop.)
        let thread_cmd_tx = cmd_tx.clone();
        let runtime_thread = std::thread::Builder::new()
            .name("LogiTileFetchH2".to_string())
            .spawn(move || {
                run_runtime_thread(cmd_rx, thread_cmd_tx, done_tx, ready_tx, thread_cache);
            })?;

        // Block until the thread reports build success/failure (bounded; the thread always sends exactly one).
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                cmd_tx,
                done_rx,
                runtime_thread: Some(runtime_thread),
                in_flight: hashbrown::HashSet::new(),
                cache,
            }),
            Ok(Err(e)) => {
                let _ = runtime_thread.join();
                Err(e)
            }
            // The thread panicked before sending readiness (e.g. an allocator OOM building the runtime).
            Err(_) => {
                let _ = runtime_thread.join();
                Err(std::io::Error::other(
                    "CyperSource runtime thread exited before readiness",
                ))
            }
        }
    }
}

/// The runtime thread's body: build the compio runtime + cyper `Client`, report readiness, then `block_on`
/// the async command loop. Everything cyper-side (`Client`, the spawned request tasks, the heap, the
/// `id → JoinHandle` map) lives entirely here — none of it is `Send`, none of it crosses back to the owner.
///
/// The loop maintains a PRIORITY heap of pending requests and admits at most [`CYPER_STREAM_BUDGET`] as live
/// streams at once: when more are outstanding, the highest-`fetch_priority` tiles stream first and the rest
/// wait — so the owner's render-critical ordering finally governs h3 service order. It wakes on owner commands
/// AND on task completions (both arrive as `Cmd` on one channel), pumping the heap after each.
fn run_runtime_thread(
    cmd_rx: flume::Receiver<Cmd>,
    cmd_tx: flume::Sender<Cmd>,
    done_tx: flume::Sender<Loaded>,
    ready_tx: flume::Sender<std::io::Result<()>>,
    cache: Option<Arc<DiskCache>>,
) {
    // Build the runtime (IOCP proactor on Windows) on THIS thread; it is `!Send` and must stay here. The
    // proactor's `spawn_blocking` pool is capped to [`CYPER_BLOCKING_POOL_LIMIT`] (compio's default is 256
    // — see the const for why that was an unbudgeted thread bomb under a streaming burst).
    let rt = {
        let mut proactor = compio::driver::ProactorBuilder::new();
        proactor.thread_pool_limit(CYPER_BLOCKING_POOL_LIMIT);
        match compio::runtime::Runtime::builder().with_proactor(proactor).build() {
            Ok(rt) => rt,
            Err(e) => {
                let _ = ready_tx.send(Err(e));
                return;
            }
        }
    };
    rt.block_on(async move {
        // The pooled, multiplexed client. ALPN negotiates h2 on the first connection (the `http2` feature);
        // with `http3-altsvc` the response's `Alt-Svc: h3` then upgrades the host to HTTP/3 over QUIC for all
        // later requests on this long-lived client. hyper-util's pool keeps warm connections so streams reuse
        // them. Built via `build_h3_client` so the QUIC transport carries a keep-alive (the vendored-cyper
        // `TransportConfig` seam) — otherwise a pooled connection idle >30 s is torn down and the next tile pays
        // a fresh handshake.
        let client = match build_h3_client() {
            Ok(c) => c,
            Err(e) => {
                let _ = ready_tx.send(Err(std::io::Error::other(format!("cyper client: {e}"))));
                return;
            }
        };
        let _ = ready_tx.send(Ok(()));

        // ADMITTED (streaming) tasks: id → JoinHandle. Dropping a handle CANCELS the task (compio-executor
        // `JoinHandle::drop` → `task.cancel(true)`), which is exactly the RST_STREAM cancel path. `live.len()`
        // is the in-flight STREAM count, capped at `CYPER_STREAM_BUDGET`.
        let mut live: HashMap<RequestId, compio::runtime::JoinHandle<()>> = HashMap::new();
        // PENDING (not-yet-admitted) requests, ordered by `fetch_priority` (max-heap). Drained into `live` by
        // the pump whenever a budget slot is free.
        let mut heap: BinaryHeap<PrioReq> = BinaryHeap::new();
        // Authoritative "still wants admission" set: inserted on `Request`, removed on admit OR cancel. The
        // heap uses lazy deletion — a cancelled-while-pending request stays physically in `heap` and is skipped
        // when popped (its id is no longer here), so a cancel needs no O(n) heap rebuild.
        let mut pending_ids: hashbrown::HashSet<RequestId> = hashbrown::HashSet::new();
        let mut next_seq: u64 = 0;

        loop {
            // Park until a command arrives — an owner `Request`/`Cancel`/`Shutdown`, OR a spawned task's
            // `Completed` (merged onto this channel so a completion WAKES the pump). One await point, no
            // busy-spin, no select. The error arm is a defensive teardown backstop — in practice the loop
            // exits via the explicit `Shutdown` that `Drop` always sends (the thread holds a `cmd_tx` clone,
            // so the channel does not close on owner-drop).
            let Ok(cmd) = cmd_rx.recv_async().await else {
                break;
            };
            match cmd {
                Cmd::Request { id, url, priority } => {
                    heap.push(PrioReq {
                        score: priority,
                        seq: next_seq,
                        id,
                        url,
                    });
                    next_seq = next_seq.wrapping_add(1);
                    pending_ids.insert(id);
                }
                Cmd::Cancel(id) => {
                    if let Some(handle) = live.remove(&id) {
                        // Admitted/streaming: drop the handle → cancel the task → RST_STREAM. It will NOT send
                        // its `Loaded`, so synthesise the one `Cancelled` here (exactly-one-result contract).
                        drop(handle);
                        let _ = done_tx.send(Loaded {
                            id,
                            result: Err(FetchError::Cancelled),
                        });
                    } else if pending_ids.remove(&id) {
                        // Pending (never admitted): no task to abort; its stale heap entry will be skipped on
                        // pop (its id is gone from `pending_ids`). Still synthesise its one `Cancelled` result.
                        let _ = done_tx.send(Loaded {
                            id,
                            result: Err(FetchError::Cancelled),
                        });
                        // Amortized tombstone purge (C2, audit 2026-06-10 — mirrors `HttpSource::cancel`'s heap
                        // rebuild): lazy deletion only reclaims a dead entry when it POPS, and a LOW-priority
                        // cancelled entry (a −1e9-demoted prefetch under sustained motion) never reaches the top
                        // while higher-priority live work keeps arriving — so the heap (each entry carrying a
                        // ~150-byte URL String) grew for the session's duration. When the tombstones outnumber
                        // the live pending set (+ slack), rebuild the heap retaining only live ids — O(heap),
                        // amortized over at least that many cancels.
                        if heap.len() > 2 * pending_ids.len() + 64 {
                            heap.retain(|r| pending_ids.contains(&r.id));
                        }
                    }
                    // else: already completed — its real result is already on the wire; nothing to do (the
                    // owner soft-drops it as stale, having removed the id from its in-flight set in `cancel`).
                }
                Cmd::Completed(id) => {
                    // A task finished (it already delivered its `Loaded`). Reap its handle so the budget slot
                    // frees — dropping a COMPLETED task's handle is a no-op cancel.
                    live.remove(&id);
                }
                Cmd::Shutdown => break,
            }

            // PUMP: fill every free budget slot with the highest-priority pending request. Runs after each
            // command since a `Request` adds work and a `Cancel`/`Completed` frees a slot.
            while live.len() < CYPER_STREAM_BUDGET {
                let Some(req) = heap.pop() else { break };
                // Lazy-deletion skip: if the id is no longer in `pending_ids` it was cancelled while pending —
                // this heap entry is dead, drop it and keep popping.
                if !pending_ids.remove(&req.id) {
                    continue;
                }
                let PrioReq { id, url, .. } = req;
                // Spawn one multiplexed h2/h3 fetch. The task owns clones of the client, cache, the result
                // sender, and a command sender; it sends its `Loaded` straight to the owner, then a
                // `Cmd::Completed(id)` so the loop reaps the handle AND admits the next pending request.
                let client = client.clone();
                let cache = cache.clone();
                let done_tx = done_tx.clone();
                let cmd_tx = cmd_tx.clone();
                let handle = compio::runtime::spawn(async move {
                    // Unwind guard (re-eval 2026-06-09): if `fetch_one_h2` PANICS, async-task catches the
                    // unwind into the never-awaited `JoinHandle` and NEITHER send below runs — silently
                    // leaking one of the [`CYPER_STREAM_BUDGET`] slots (`live` keeps the handle forever)
                    // and an owner in-flight entry. The guard fires ONLY during a panic unwind
                    // (`thread::panicking()`), restoring the exactly-one-result contract; a cancel-drop
                    // stays silent (Cmd::Cancel already synthesised that id's `Cancelled`).
                    let panic_guard = PanicCompletion {
                        id,
                        done_tx: done_tx.clone(),
                        cmd_tx: cmd_tx.clone(),
                    };
                    let result = fetch_one_h2(&client, cache.as_ref(), &url).await;
                    let _ = done_tx.send(Loaded { id, result });
                    let _ = cmd_tx.send(Cmd::Completed(id));
                    drop(panic_guard); // off-panic its Drop is a no-op; kept last so any unwind above fires it
                });
                live.insert(id, handle);
            }
        }
        // Teardown: dropping `live` cancels every still-in-flight task (RST_STREAM on each open stream); the
        // heap's never-admitted requests are abandoned (the owner is tearing down too). The runtime then drops,
        // draining the proactor.
        drop(live);
    });
}

impl Drop for CyperSource {
    fn drop(&mut self) {
        // Mirror `HttpSource::drop`: signal the loop to stop, then JOIN the runtime thread so teardown never
        // leaves it (its `Client` + `Arc<DiskCache>` clone) running detached. Sending may fail if the loop
        // already exited (channel closed) — harmless; the join still reclaims the thread.
        let _ = self.cmd_tx.send(Cmd::Shutdown);
        if let Some(h) = self.runtime_thread.take() {
            let _ = h.join();
        }
    }
}

impl TileContentSource for CyperSource {
    fn request(&mut self, id: RequestId, url: &str, priority: f64, _now_tick: u64) {
        // Hand the fetch to the runtime thread with its `priority`: the loop pushes it on a max-heap and admits
        // the highest-priority pending requests first once the in-flight budget has room (so render-critical
        // tiles stream before speculative prefetch under contention — the ureq lane's `fetch_priority`
        // ordering, now on h3 too). If the loop is already gone (teardown) the send fails and we don't track it.
        if self
            .cmd_tx
            .send(Cmd::Request {
                id,
                url: url.to_string(),
                priority,
            })
            .is_ok()
        {
            self.in_flight.insert(id);
        }
    }

    fn poll_completed(&mut self, _now_tick: u64, out: &mut Vec<Loaded>) {
        // Identical to `HttpSource::poll_completed` minus the cancel-registry (the runtime loop owns
        // cancellation): drain finished results; a result whose id the owner already cancelled (gone from
        // `in_flight`) is soft-dropped as stale (the backstop for a result that beat its cancel onto the
        // channel). Every surviving id is removed exactly once.
        while let Ok(loaded) = self.done_rx.try_recv() {
            if self.in_flight.remove(&loaded.id) {
                out.push(loaded);
            }
        }
    }

    fn cancel(&mut self, id: RequestId) {
        // Drop from the owner-side in-flight set immediately (so any result already racing onto the channel
        // is soft-dropped in `poll_completed`), then tell the runtime thread to abort the in-flight stream.
        if self.in_flight.remove(&id) {
            let _ = self.cmd_tx.send(Cmd::Cancel(id));
        }
    }

    fn in_flight(&self) -> usize {
        self.in_flight.len()
    }

    fn cached_bytes(&self) -> u64 {
        self.cache.as_ref().map(|c| c.bytes()).unwrap_or(0)
    }
    fn cache_hits(&self) -> u64 {
        self.cache.as_ref().map(|c| c.hits()).unwrap_or(0)
    }
    fn cache_misses(&self) -> u64 {
        self.cache.as_ref().map(|c| c.misses()).unwrap_or(0)
    }
    fn fetch_concurrency(&self) -> usize {
        // The in-flight STREAM budget the runtime loop now ENFORCES (caps `live`), not a thread count (there is
        // one runtime thread). The owner sizes its outstanding-fetch cap as a small multiple of this (audit H2);
        // requests beyond the budget queue on the runtime's priority heap rather than all streaming at once.
        CYPER_STREAM_BUDGET
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// `cancel` before any `poll_completed` drops the id from the in-flight set immediately AND the
    /// subsequently-drained synthetic `Cancelled` result is soft-dropped (its id is already gone) — so a
    /// cancelled request NEVER surfaces from `poll_completed`, exactly mirroring `HttpSource`'s contract.
    /// No network: the request is cancelled before the h2 stream can possibly complete. This is the
    /// non-ignored bookkeeping test the brief asks for (feasible without network).
    #[test]
    fn cancel_drops_in_flight_and_never_surfaces() {
        let mut src = CyperSource::new(None).expect("build CyperSource runtime");
        // Point at a dead port so the fetch would fail fast even if it raced ahead of the cancel.
        src.request(1, "http://127.0.0.1:1/never", 0.0, 0);
        src.request(2, "http://127.0.0.1:1/never", 0.0, 0);
        assert_eq!(src.in_flight(), 2, "two requests tracked in-flight");

        src.cancel(1);
        assert_eq!(src.in_flight(), 1, "cancel drops the id from in-flight immediately");

        // Drain for a bounded window. id 1 must NEVER surface (cancelled → soft-dropped); id 2's result
        // (a fast transport failure against the dead port) eventually arrives and clears in-flight.
        let mut out = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            src.poll_completed(0, &mut out);
            if out.iter().any(|l| l.id == 2) {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(out.iter().any(|l| l.id == 2), "id 2's result should surface");
        assert!(!out.iter().any(|l| l.id == 1), "cancelled id 1 must never surface");
        assert_eq!(src.in_flight(), 0, "all in-flight resolved");
    }

    /// A failed fetch (dead port → transport error) surfaces as exactly one `Loaded` with a TRANSIENT
    /// classified error — proving the cyper→`FetchError` mapping + the one-result-per-request contract
    /// without needing a live network host (a `127.0.0.1:1` connect refuses immediately).
    #[test]
    fn dead_port_yields_one_transient_transport_error() {
        let mut src = CyperSource::new(None).expect("build CyperSource runtime");
        src.request(7, "http://127.0.0.1:1/nope", 0.0, 0);
        assert_eq!(src.in_flight(), 1);

        let mut out = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            src.poll_completed(0, &mut out);
            if !out.is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(out.len(), 1, "exactly one result for the one request");
        assert_eq!(out[0].id, 7);
        match &out[0].result {
            Err(e @ FetchError::Transport(_)) => {
                assert!(e.is_transient(), "a connect failure is transient (retry-worthy)")
            }
            other => panic!("expected a transient transport error, got {other:?}"),
        }
        assert_eq!(src.in_flight(), 0);
    }

    /// `Drop` joins the runtime thread cleanly even with requests still in flight (mirrors `HttpSource`'s
    /// join-on-`Drop`): no detached-thread leak. If the join hung this test would time out the suite.
    #[test]
    fn drop_joins_runtime_thread_with_inflight() {
        let mut src = CyperSource::new(None).expect("build CyperSource runtime");
        src.request(1, "http://127.0.0.1:1/never", 0.0, 0);
        src.request(2, "http://127.0.0.1:1/never", 0.0, 0);
        // Drop without draining — teardown must cancel the in-flight tasks and join the thread.
        drop(src);
    }

    /// A burst LARGER than the in-flight budget must still surface EVERY request exactly once: the loop admits
    /// `CYPER_STREAM_BUDGET` as live streams, and as each completes its `Cmd::Completed` pumps the next pending
    /// request off the priority heap until the backlog drains. Proves the heap + budget + completion-pump never
    /// strands a queued request (no network host needed — `127.0.0.1:1` refuses fast, so each is a quick
    /// transport error). Guards against the deadlock a "pump only on owner command" design would have.
    #[test]
    fn burst_over_budget_surfaces_every_request_once() {
        let mut src = CyperSource::new(None).expect("build CyperSource runtime");
        let n: u64 = CYPER_STREAM_BUDGET as u64 + 40; // > budget, forcing the heap/pump backlog path
        for id in 0..n {
            src.request(id, "http://127.0.0.1:1/never", 0.0, 0);
        }
        assert_eq!(src.in_flight(), n as usize, "every request is tracked in-flight");

        let mut seen: hashbrown::HashSet<RequestId> = hashbrown::HashSet::new();
        let mut out = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(30);
        while (seen.len() as u64) < n && Instant::now() < deadline {
            src.poll_completed(0, &mut out);
            for l in out.drain(..) {
                assert!(seen.insert(l.id), "id {} surfaced more than once", l.id);
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(
            seen.len() as u64,
            n,
            "every queued request must surface exactly once (none stranded)"
        );
        assert_eq!(src.in_flight(), 0, "all in-flight resolved");
    }
}
