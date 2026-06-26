//! The tile-content **fetch contract** for the actorless 3D-Tiles stack (ADR-0009, Milestone F): an
//! async-fetch-then-poll source the streaming loop drives on its single owner thread, generalising the
//! kernel's existing `Provider`. The selection traversal decides *which* tiles to load; a
//! [`TileContentSource`] fetches their content bytes and hands them back when ready.
//!
//! Three sources implement the same trait: the deterministic [`SyntheticSource`] (latency + payload derived
//! from the request id — reproducible, no network, the test/offline lane); the blocking [`HttpSource`] (HTTP
//! via `ureq` + gzip + an on-disk cache, fetched on a worker pool and drained here); and — under the
//! default `cyper-h2` feature (legacy name) — the **HTTP/3-over-QUIC** `CyperSource` (compio-quic =
//! quinn-proto on the compio reactor, upgraded via Alt-Svc; h2 over compio-net + rustls as bootstrap +
//! fallback, all on one owned thread). The scene's `from_ion` picks `CyperSource` for Google P3DT by
//! default; the runtime FFI `fetch_backend` knob can force either at world-create time.

#![forbid(unsafe_code)]

// The HTTP/3 tile-fetch backend (feature `cyper-h2` — the legacy feature name predates the h3 upgrade — ON
// BY DEFAULT): a vendored `cyper::Client` on one dedicated owned thread, HTTP/3 over QUIC via Alt-Svc
// auto-upgrade with h2-over-compio-net+rustls bootstrap/fallback. Adopted on STRUCTURAL grounds (connection
// economy at the capped worker count, 1-RTT QUIC connect, keep-alive idle survival): the live keyed-P3DT A/B
// (`tests/cyper_p3dt_bench.rs`) measures ~parity with `ureq` on warm real-body bursts — the h2/h3 win is the
// handshake/RTT collapse, not bandwidth. Build `--no-default-features` for the lean ureq-only cdylib (no
// QUIC/h2 stack). See `cyper_source.rs` for the design.
#[cfg(feature = "cyper-h2")]
mod cyper_source;
#[cfg(feature = "cyper-h2")]
pub use cyper_source::CyperSource;

use std::collections::{BTreeMap, BinaryHeap};

use hashbrown::{HashMap, HashSet};
use std::cell::RefCell;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::Duration;

/// Identifies an in-flight request (the caller's tile id / URL hash).
pub type RequestId = u64;

/// A completed fetch handed back to the owner thread: the request id + a **typed** outcome.
///
/// Why a `Result` rather than a bare `Vec<u8>` (audit A10): the old payload was `bytes: Vec<u8>` filled
/// via `http_get(&url).unwrap_or_default()`, which collapsed *every* outcome — 404, 429, 503, a connect
/// timeout, a DNS failure, AND a legitimate empty-200 body — to an empty `Vec`. The owner literally could
/// not tell "the server said this tile doesn't exist, give up" from "the server is rate-limiting us, back
/// off and retry" from "the tile is genuinely 0 bytes, that's a success". Carrying a [`Result`] lets the
/// owner branch on the **failure class** ([`FetchError::is_transient`] / [`FetchError::retry_after`]):
/// retry transient errors with backoff, abandon terminal ones immediately, and accept an empty body as the
/// success it is. The actual *when-to-retry* policy lives on the owner (`geo_world`), not here — see the
/// crate docs / [`FetchError`] for the contract.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Loaded {
    pub id: RequestId,
    /// `Ok(bytes)` — the content as a shared `Arc<[u8]>` (already gzip-inflated by the source; an empty slice
    /// is a valid empty-200). The source hands back an `Arc` so the owner can park it on the entity
    /// (`FetchedBytes`) with a cheap clone/move — the one alloc+copy happens **on the worker**, not on the
    /// serial owner thread that drains results (audit L4).
    /// `Err(e)` — a classified failure the owner can act on without guessing.
    pub result: Result<Arc<[u8]>, FetchError>,
}

/// An async tile-content source driven on the owner thread: `request` enqueues, `poll_completed` drains
/// finished fetches, `cancel` drops one. Implementations do the actual IO/decoding off-thread; nothing
/// here blocks the kernel. `now_tick` is the kernel's monotonic step counter. `priority` (higher = more
/// urgent — the selector's `fetch_priority`) lets a worker pool SERVICE the most render-critical pending tile
/// first instead of FIFO (audit H1); sources without a worker pool may ignore it.
pub trait TileContentSource {
    fn request(&mut self, id: RequestId, url: &str, priority: f64, now_tick: u64);
    fn poll_completed(&mut self, now_tick: u64, out: &mut Vec<Loaded>);
    fn cancel(&mut self, id: RequestId);
    fn in_flight(&self) -> usize;
    /// Bytes resident in a persistent cache behind this source, if any (telemetry). Defaults to `0` for
    /// sources without a cache (e.g. the synthetic test source).
    fn cached_bytes(&self) -> u64 {
        0
    }
    /// Cumulative hits / misses of a persistent cache behind this source (telemetry; audit L1 — un-blinds
    /// disk-cache hit-rate so the budget can be tuned). Default `0` for cacheless sources.
    fn cache_hits(&self) -> u64 {
        0
    }
    fn cache_misses(&self) -> u64 {
        0
    }
    /// The source's fetch CONCURRENCY — how many requests it can service simultaneously (its worker count). The
    /// owner sizes its outstanding-fetch cap as a small multiple of this (audit H2), so the cap tracks the one
    /// knob that actually governs download parallelism instead of a magic constant. Default `32` for sources
    /// without a worker pool (the synthetic/test lane), preserving the prior 128 cap at `IN_FLIGHT_PER_WORKER=4`.
    fn fetch_concurrency(&self) -> usize {
        32
    }
}

/// SplitMix64 — a fast, deterministic mixer (also used by the kernel's existing synthetic provider) so a
/// request id maps to a stable pseudo-random latency + payload.
#[inline]
pub fn splitmix64(x: u64) -> u64 {
    let mut z = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// A deterministic, in-process source for tests + offline/no-network dev. Each request's latency and
/// payload are pure functions of its id, so a recorded run replays byte-for-byte. No threads, no IO.
#[derive(Debug, Default)]
pub struct SyntheticSource {
    /// id → (ready_tick, payload_len)
    pending: HashMap<RequestId, (u64, usize)>,
    min_latency: u64,
    latency_range: u64,
    min_bytes: usize,
    bytes_range: usize,
}

impl SyntheticSource {
    /// Latencies in `1..=10` ticks, payloads in `256..=4351` bytes.
    pub fn new() -> Self {
        Self {
            pending: HashMap::new(),
            min_latency: 1,
            latency_range: 10,
            min_bytes: 256,
            bytes_range: 4096,
        }
    }

    fn latency_for(&self, id: RequestId) -> u64 {
        self.min_latency + splitmix64(id) % self.latency_range
    }
    fn bytes_for(&self, id: RequestId) -> usize {
        self.min_bytes + (splitmix64(id ^ 0x0AB0_1DE5) as usize) % self.bytes_range
    }
}

/// Deterministic payload of `len` bytes seeded by `id` — what a `SyntheticSource` "downloads".
pub fn synthetic_payload(id: RequestId, len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len + 8);
    let mut s = id ^ 0x5EED;
    while out.len() < len {
        s = splitmix64(s);
        out.extend_from_slice(&s.to_le_bytes());
    }
    out.truncate(len);
    out
}

impl TileContentSource for SyntheticSource {
    fn request(&mut self, id: RequestId, _url: &str, _priority: f64, now_tick: u64) {
        let ready = now_tick.saturating_add(self.latency_for(id));
        let len = self.bytes_for(id);
        self.pending.insert(id, (ready, len));
    }

    fn poll_completed(&mut self, now_tick: u64, out: &mut Vec<Loaded>) {
        let mut done: Vec<RequestId> = self
            .pending
            .iter()
            .filter(|(_, (ready, _))| *ready <= now_tick)
            .map(|(id, _)| *id)
            .collect();
        // Deterministic completion order: `HashMap` iteration order is randomized per run, which would
        // make a budgeted/order-sensitive consumer non-reproducible even though every payload is a pure
        // function of its id. Sort by id so a replay is byte- AND order-identical.
        done.sort_unstable();
        for id in done {
            let (_, len) = self.pending.remove(&id).unwrap_or((0, 0));
            // The synthetic lane never fails — every request "downloads" successfully (audit A10: the
            // payload is `Ok`, not bare bytes, so it shares the typed channel with the HTTP source).
            out.push(Loaded {
                id,
                result: Ok(synthetic_payload(id, len).into()),
            });
        }
    }

    fn cancel(&mut self, id: RequestId) {
        self.pending.remove(&id);
    }

    fn in_flight(&self) -> usize {
        self.pending.len()
    }
}

// ----------------------------------------------------------------------------------------------------
// Production HTTP backend (the network counterpart to SyntheticSource). `resolve_url` builds an absolute
// tile URL from the tileset base + a content URI, carrying the base's auth query (Google's `key`, ion's
// `access_token`) onto child requests; `http_get` is a blocking fetch. The worker-pool TileContentSource
// impl that drains these into `poll_completed` lands with the scene wiring.

/// A fetch / URL-resolution error, **classified** so the owner can decide whether to retry (audit A1).
///
/// The fetch worker only *classifies*; it never sleeps or retries. The owner (`geo_world`'s
/// `select_request` / `fail_fetch`) reads [`is_transient`](Self::is_transient) and
/// [`retry_after`](Self::retry_after) and runs the backoff loop. The split that matters:
///
/// * **Transient** (retry-worthy): HTTP 429 + any 5xx, and every transport-level failure (connect/read/
///   write timeout, DNS, connection reset). These are "try again later", not "this will never work".
/// * **Terminal** (give up immediately — retrying only wastes bandwidth and a worker slot): 4xx other than
///   429 (404 the tile doesn't exist, 403 the key is wrong, 400 a malformed URL), a URL that won't parse,
///   and an over-cap body ([`TooLarge`](Self::TooLarge) — a server streaming junk won't stream less next
///   time).
/// * [`Cancelled`](Self::Cancelled) is neither: the owner already moved on, so there is nothing to retry.
///   It reports `is_transient() == false` so the owner does not re-queue it on the retry path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FetchError {
    /// The content URI / tileset base wouldn't resolve to a valid absolute URL. **Terminal** — a bad URL
    /// is bad every time.
    Url(String),
    /// The server returned an HTTP status >= 400. `status` is the code; `retry_after` is the parsed
    /// `Retry-After` header when the server sent one (429/503 commonly do). Transient iff `status` is 429
    /// or 5xx; every other 4xx is terminal — see [`is_transient`](Self::is_transient).
    Http {
        status: u16,
        /// Server-supplied "don't come back before this" delay, parsed from the `Retry-After` header
        /// (delta-seconds form). `None` when absent/unparseable — the owner then uses its own backoff.
        retry_after: Option<Duration>,
    },
    /// A transport-level failure with no HTTP status: connect/read/write timeout, DNS failure, TLS error,
    /// connection reset, etc. Always **transient** — the host or network hiccuped; a later attempt may
    /// succeed. The `String` is the underlying error for telemetry only (never matched on).
    Transport(String),
    /// The response body exceeded [`MAX_TILE_BYTES`] — a hostile/misconfigured server streaming an
    /// unbounded payload. **Terminal**: rejected rather than letting it OOM the process, and a server
    /// streaming an over-cap body will do so again, so retrying is pointless.
    TooLarge,
    /// The fetch was cancelled mid-flight (the owner called [`HttpSource::cancel`] while the bytes were on
    /// the wire — see audit A3). Neither transient nor terminal: the owner already discarded this request,
    /// so there is nothing to retry. Surfaced (instead of silently dropped) so the owner can free the
    /// in-flight slot and bookkeeping cleanly.
    Cancelled,
}

impl FetchError {
    /// `true` if a later attempt could plausibly succeed (429, 5xx, transport/timeout/DNS). The owner
    /// retries these with capped exponential backoff; everything else (`false`) it abandons at once. See
    /// the type-level docs for the full transient-vs-terminal split.
    pub fn is_transient(&self) -> bool {
        match self {
            // 429 (rate-limited) and 5xx (server-side) are temporary; every other 4xx is the client's
            // fault (404/403/400) and will not change on retry. `>= 500` also catches non-standard 5xx.
            FetchError::Http { status, .. } => *status == 429 || *status >= 500,
            // The network/host failed, not the request itself — worth another try.
            FetchError::Transport(_) => true,
            // Bad URL, over-cap body, and an owner-initiated cancel are all "do not retry".
            FetchError::Url(_) | FetchError::TooLarge | FetchError::Cancelled => false,
        }
    }

    /// The server's `Retry-After` hint, if it sent one (429/503). The owner should wait *at least* this
    /// long before retrying — honoring it is what keeps us from hammering a server that explicitly asked us
    /// to slow down (the North Star: don't melt the tile server). `None` for every non-HTTP error and any
    /// response without the header; the owner then falls back to its own exponential backoff.
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            FetchError::Http { retry_after, .. } => *retry_after,
            _ => None,
        }
    }
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FetchError::Url(e) => write!(f, "url resolution failed: {e}"),
            FetchError::Http { status, retry_after } => match retry_after {
                Some(d) => write!(f, "http status {status} (retry-after {}s)", d.as_secs()),
                None => write!(f, "http status {status}"),
            },
            FetchError::Transport(e) => write!(f, "transport error: {e}"),
            FetchError::TooLarge => write!(f, "response body exceeded {MAX_TILE_BYTES} bytes"),
            FetchError::Cancelled => write!(f, "fetch cancelled"),
        }
    }
}

impl std::error::Error for FetchError {}

/// Parse an HTTP `Retry-After` header value into a [`Duration`]. Only the **delta-seconds** form (a bare
/// integer like `Retry-After: 120`) is handled — that is what `tile.googleapis.com` and Cesium ion send on
/// 429/503. The HTTP-date form (`Retry-After: Wed, 21 Oct 2025 07:28:00 GMT`) is intentionally NOT parsed:
/// it would drag in a date-parsing dependency for a case these providers don't use, and the owner's own
/// backoff is a safe fallback (we just return `None`). A `0` or non-numeric value also yields `None` so a
/// degenerate header can't pin a retry to "immediately".
fn parse_retry_after(raw: &str) -> Option<Duration> {
    let secs: u64 = raw.trim().parse().ok()?;
    if secs == 0 {
        return None;
    }
    Some(Duration::from_secs(secs))
}

/// Hard cap on a single tile/tileset response body. Tile content URIs are attacker-influenced (streamed
/// from a remote, grafted recursively — see [`resolve_url`]), so an unbounded `read_to_end` is an OOM/DoS
/// vector. 64 MiB comfortably exceeds any real glTF/b3dm/quantized-mesh tile or `tileset.json`; a body that
/// hits the cap is rejected ([`FetchError::TooLarge`]). Sized to a `usize` for the read path.
pub const MAX_TILE_BYTES: usize = 64 * 1024 * 1024;

/// Per-connection/overall HTTP timeouts for the shared agent. A server that accepts then stalls must not
/// pin a worker thread forever (a few stuck hosts would starve the whole fetch pool and silently stop
/// streaming); these bound every phase of every request.
const HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const HTTP_IO_TIMEOUT: Duration = Duration::from_secs(30); // per read / per write
const HTTP_OVERALL_TIMEOUT: Duration = Duration::from_secs(60); // whole call, incl. body transfer

/// Idle keep-alive connection pool sizes for the shared ureq agent. `PER_HOST` MUST be ≥ the max fetch-worker
/// count (the FFI clamps workers to 64) so every worker reuses a warm connection to the one P3DT host instead
/// of re-paying a TLS handshake per request — the #1 throughput bug (8 threads → ~8 MB/s of re-handshaking).
/// Keep `PER_HOST` in lockstep with `logi_geo_ffi`'s `MAX_FETCH_WORKERS`.
const HTTP_MAX_IDLE_CONNECTIONS: usize = 256;
const HTTP_MAX_IDLE_PER_HOST: usize = 64;

/// Process-wide shared [`ureq::Agent`] with bounded connect/read/write/overall timeouts, reused by every
/// fetch (this crate's [`http_get`] and the ion resolver) so requests share a connection pool and no
/// request can block indefinitely (audit H9). Built once on first use.
pub fn http_agent() -> &'static ureq::Agent {
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    AGENT.get_or_init(|| {
        ureq::AgentBuilder::new()
            .timeout_connect(HTTP_CONNECT_TIMEOUT)
            .timeout_read(HTTP_IO_TIMEOUT)
            .timeout_write(HTTP_IO_TIMEOUT)
            .timeout(HTTP_OVERALL_TIMEOUT)
            // Keep connections ALIVE across the whole fetch-worker pool. ureq's default idle pool is tiny
            // (≈1 per host), so with N workers all hammering ONE host (P3DT's `tile.googleapis.com`) only one
            // connection is reused — the other N-1 close after each request and re-pay a TLS handshake
            // (~1-2 RTT) on the next one, throttling throughput to the *handshake* rate, not the link rate.
            // Sizing the per-host idle pool AT the worker cap (`HTTP_MAX_IDLE_PER_HOST` = 64 = the FFI's
            // `MAX_FETCH_WORKERS` clamp) lets every worker keep reusing a warm keep-alive connection, so the
            // pool streams at the link's real bandwidth (audit: the #1 reason 8 fetch threads only managed
            // ~8 MB/s — they were re-handshaking, not downloading).
            .max_idle_connections(HTTP_MAX_IDLE_CONNECTIONS)
            .max_idle_connections_per_host(HTTP_MAX_IDLE_PER_HOST)
            .build()
    })
}

/// Resolve a tile `content_uri` against the tileset `base` URL, carrying over the base's auth query
/// params (e.g. `?key=…` / `?access_token=…`) that the child URI doesn't already carry. Handles absolute,
/// host-relative (`/v1/…`), and path-relative content URIs.
pub fn resolve_url(base: &str, content_uri: &str) -> Result<String, FetchError> {
    // Owner-thread parsed-base memo (audit resolve_url-memo): a single selection step resolves hundreds of
    // tiles against only a HANDFUL of distinct bases (the root + a few grafted subtrees), yet the base was
    // re-parsed on every call. Cache the parse per distinct base → one `Url::parse` per base per thread, not
    // one per tile (the join + same-origin auth-carry are genuinely per-`content_uri`, so they stay). This is
    // owner-thread-only — workers fetch already-resolved URLs — so a thread-local is lock-free + sufficient;
    // a self-healing `clear` past the cap bounds it (a still-live base just re-parses + re-inserts).
    const MAX_RESOLVE_BASE_CACHE: usize = 256;
    thread_local! {
        static RESOLVE_BASE_CACHE: RefCell<HashMap<String, url::Url>> = RefCell::new(HashMap::new());
    }
    RESOLVE_BASE_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        if !cache.contains_key(base) {
            // Parse the base ONCE per distinct base (the memoized work). A parse error is returned WITHOUT
            // caching, so a transient/garbage base can never poison the memo.
            let parsed = url::Url::parse(base).map_err(|e| FetchError::Url(e.to_string()))?;
            if cache.len() >= MAX_RESOLVE_BASE_CACHE {
                cache.clear();
            }
            cache.insert(base.to_string(), parsed);
        }
        let base_url = cache.get(base).expect("base parsed + inserted directly above");
        let mut joined = base_url.join(content_uri).map_err(|e| FetchError::Url(e.to_string()))?;
        // SECURITY: only carry the base's auth query params (`access_token` / `key` / `session`) when the
        // resolved URL stays on the SAME ORIGIN as the base. A tileset's `content.uri` is attacker-influenced
        // (streamed from a remote, and external tilesets are grafted recursively), and `Url::join` with an
        // absolute URI replaces the host — carrying the token cross-origin would exfiltrate the user's Cesium
        // ion / Google credentials to an arbitrary host. Relative / same-host URIs keep the same origin and so
        // are unaffected.
        if joined.origin() == base_url.origin() {
            let have: HashSet<String> = joined.query_pairs().map(|(k, _)| k.into_owned()).collect();
            let carry: Vec<(String, String)> = base_url
                .query_pairs()
                .into_owned()
                .filter(|(k, _)| !have.contains(k))
                .collect();
            if !carry.is_empty() {
                let mut q = joined.query_pairs_mut();
                for (k, v) in &carry {
                    q.append_pair(k, v);
                }
            }
        }
        Ok(joined.to_string())
    })
}

/// How many bytes the cancel-aware reader pulls per loop iteration. The cancel flag is probed **once per
/// chunk** (audit A3), so this trades cancel latency against probe overhead: smaller = a fast camera abort
/// sees the flag sooner; larger = fewer atomic loads on the steady-state download path. 64 KiB is well
/// below any tile (so even small tiles get a probe or two) yet large enough that the per-chunk atomic load
/// is noise next to the socket read — one `Relaxed` load per 64 KiB, not per byte.
const READ_CHUNK_BYTES: usize = 64 * 1024;

/// Blocking HTTP GET → raw bytes, over the shared timeout-bounded [`http_agent`]. Run off the kernel's
/// owner thread (on a worker pool). Three safeguards, all preserved/added here:
///
/// * **Size cap** (audit H8): the body is read through a [`MAX_TILE_BYTES`] wall; a server streaming more
///   yields [`FetchError::TooLarge`] rather than an unbounded allocation.
/// * **Status classification** (audit A1): a non-2xx status becomes a *classified* [`FetchError::Http`]
///   (with any `Retry-After`), and a transport failure a [`FetchError::Transport`] — so the owner can tell
///   "retry later" from "give up" instead of seeing one opaque error. An empty-200 body returns
///   `Ok(empty)`: a 0-byte tile is a success, not a failure.
/// * **Mid-download cancel** (audit A3): `cancel` is probed once per 64 KiB read chunk; when the
///   owner flips it (camera moved on), the read aborts *immediately* with [`FetchError::Cancelled`] instead
///   of finishing the (up to 64 MiB) download just to discard it — the dominant wasted bandwidth under fast
///   motion. Pass an already-false flag (or use [`http_get`]) when there is nothing to cancel.
pub fn http_get_cancellable(url: &str, cancel: &AtomicBool) -> Result<Vec<u8>, FetchError> {
    // Fast pre-flight: if the owner cancelled before we even opened the socket, don't pay for the request.
    if cancel.load(Ordering::Relaxed) {
        return Err(FetchError::Cancelled);
    }
    let resp = match http_agent().get(url).call() {
        Ok(resp) => resp,
        // ureq 2.x splits errors cleanly: `Status(code, resp)` is any HTTP >= 400 (the connection worked,
        // the *server* refused), `Transport(_)` is everything below HTTP — connect/read/write timeout, DNS,
        // TLS, connection reset. We classify on exactly that split (A1).
        Err(ureq::Error::Status(code, resp)) => {
            // Pull the `Retry-After` hint off the *error* response (429/503 carry it). Owner honors it.
            let retry_after = resp.header("Retry-After").and_then(parse_retry_after);
            return Err(FetchError::Http {
                status: code,
                retry_after,
            });
        }
        Err(ureq::Error::Transport(t)) => return Err(FetchError::Transport(t.to_string())),
    };
    // Pre-size from a sane Content-Length (clamped to the cap so a lying header can't pre-allocate
    // gigabytes), but never trust it as the actual length — the capped reader below is the real bound.
    let hint = resp
        .header("Content-Length")
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(0)
        .min(MAX_TILE_BYTES);
    let mut bytes = Vec::with_capacity(hint);
    // `.take(MAX_TILE_BYTES + 1)` makes the cap a hard wall regardless of the (untrusted) Content-Length:
    // if the reader can still yield byte MAX+1, the body is over-budget. We read in `READ_CHUNK_BYTES`
    // chunks (rather than one `read_to_end`) for one reason: to probe `cancel` between chunks (A3). The
    // previous `read_to_end` had no opportunity to bail — it downloaded the entire tile, *then* the worker
    // discarded it; under fast camera motion that is the single largest source of wasted bandwidth.
    let mut reader = resp.into_reader().take(MAX_TILE_BYTES as u64 + 1);
    let mut chunk = [0u8; READ_CHUNK_BYTES];
    loop {
        // One `Relaxed` atomic load per chunk (~per 64 KiB) — cheap, and `Relaxed` is sufficient: we only
        // need to *eventually* observe the owner's store, not synchronize other memory with it.
        if cancel.load(Ordering::Relaxed) {
            return Err(FetchError::Cancelled);
        }
        let n = reader
            .read(&mut chunk)
            .map_err(|e| FetchError::Transport(e.to_string()))?;
        if n == 0 {
            break; // EOF — full body received (possibly empty: a legitimate empty-200).
        }
        bytes.extend_from_slice(&chunk[..n]);
        if bytes.len() > MAX_TILE_BYTES {
            return Err(FetchError::TooLarge);
        }
    }
    Ok(bytes)
}

/// [`http_get_cancellable`] with no cancellation signal — for callers (the ion resolver, the blocking root
/// fetch) that have nothing to cancel. Keeps the size-cap + status-classification safeguards.
pub fn http_get(url: &str) -> Result<Vec<u8>, FetchError> {
    http_get_cancellable(url, &AtomicBool::new(false))
}

/// One worker's cache-first, cancel-aware fetch of `url`, resolved to a **typed** outcome (audit A10):
/// serve a disk-cache hit, else download (honoring `cancel` mid-flight, A3) and populate the cache on a
/// successful non-empty body. Used only by the [`HttpSource`] worker loop; factored out so the cache
/// hit/miss/populate + cancellation logic lives in one place (and is unit-testable).
///
/// Cache invariant (unchanged): only a *successful, non-empty* body is stored. A classified failure
/// ([`FetchError::Http`]/`Transport`/`TooLarge`) never returns bytes to cache, a cancelled/partial download
/// is [`FetchError::Cancelled`] (never cached), and an empty-200 is not worth a file — so a 4xx / expired
/// session / aborted fetch can never poison the cache, exactly as before.
fn fetch_one(cache: Option<&DiskCache>, url: &str, cancel: &AtomicBool) -> Result<Vec<u8>, FetchError> {
    // Pre-flight cancel: the owner moved on before this request reached a worker. Bail before touching the
    // cache or the network — this subsumes the old "skip before dequeue" hard cancel.
    if cancel.load(Ordering::Relaxed) {
        return Err(FetchError::Cancelled);
    }
    if let Some(c) = cache {
        // Hash the URL path to the cache key ONCE (audit L6), then use the key-based get/put — `get(url)` +
        // `put(url, …)` would each re-hash the same ~150-byte URL across the intervening download (LLVM can't
        // CSE across it). A cache hit costs no bandwidth, so cancellation doesn't matter for it; serve it. (A
        // result the owner has since cancelled is harmlessly dropped on `poll_completed` via the in-flight backstop.)
        let key = cache_key(url);
        if let Some(bytes) = c.get_raw(key) {
            return Ok(bytes);
        }
        let bytes = http_get_cancellable(url, cancel)?;
        if !bytes.is_empty() {
            c.put_raw(key, &bytes);
        }
        Ok(bytes)
    } else {
        http_get_cancellable(url, cancel)
    }
}

// ----------------------------------------------------------------------------------------------------
// Persistent on-disk tile cache. A byte-budgeted LRU over fetched tile bytes, keyed by a 128-bit hash of
// the URL PATH (query stripped) so a provider's per-session auth (Google P3DT's `?session=…&key=…`)
// doesn't fragment the cache and the same tile replays across sessions. The key is 128-bit (not 64) so a
// hash collision — which would silently serve one tile's bytes for another URL — is astronomically
// unlikely across any realistic cache size (audit M16). Survives restarts (the files ARE the cache; the
// index is rebuilt by scanning the dir). Each entry's bytes carry a CRC-32 baked into its filename
// (`<key>.<crc>.bin`) and verified on read, so bit-rot / truncation / a partial write surfaces as a clean
// miss + re-fetch rather than a silently-served corrupt tile (audit M1).
//
// LRU recency after a restart: the rebuilt order is the directory-scan order, NOT true last-use order —
// filesystem mtime is non-monotonic (clock skew, copies, `noatime`) and was deliberately dropped as an
// ordering signal. The cost is only a slightly-off eviction order for the FIRST eviction pass after a
// cold start; live use immediately re-establishes real recency via `touch`. Thread-safe for the worker
// pool. NOTE: persistent caching of some providers' tiles (e.g. Google P3DT) violates their ToS — that
// policy decision belongs to the caller; this layer is provider-agnostic.

struct CacheEntry {
    size: u64,
    used: u64, // monotonic recency tick
    crc: u32,  // CRC-32 of the stored bytes — baked into the filename, verified on every read (audit M1)
}

struct CacheIndex {
    entries: HashMap<u128, CacheEntry>, // key → entry
    lru: BTreeMap<u64, u128>,           // recency tick → key (ordered, for O(log n) eviction)
    total: u64,
    tick: u64,
}

impl CacheIndex {
    fn touch(&mut self, key: u128) {
        self.tick += 1;
        let t = self.tick;
        if let Some(e) = self.entries.get_mut(&key) {
            self.lru.remove(&e.used);
            e.used = t;
            self.lru.insert(t, key);
        }
    }
    /// Insert/replace an entry, returning the previous entry's CRC **iff** it differs from the new one — i.e.
    /// the old content lived in a different `<key>.<crc>.bin` file the caller must now unlink (a re-`put` of
    /// CHANGED bytes for the same key; rare, since tile content is path-immutable, but bullet-proofed so the
    /// crc-in-filename scheme can never strand the superseded file). A same-content re-`put` overwrites the
    /// identical path in place → `None`.
    fn insert(&mut self, key: u128, size: u64, crc: u32) -> Option<u32> {
        self.tick += 1;
        let t = self.tick;
        let stale = self
            .entries
            .insert(key, CacheEntry { size, used: t, crc })
            .and_then(|old| {
                self.lru.remove(&old.used);
                self.total = self.total.saturating_sub(old.size);
                (old.crc != crc).then_some(old.crc)
            });
        self.lru.insert(t, key);
        self.total = self.total.saturating_add(size);
        stale
    }
    /// Evict the coldest entries from the in-memory index until back within `budget`, returning the
    /// `(key, crc)` pairs whose files must be unlinked. **Does no filesystem I/O** — the caller unlinks AFTER dropping the index lock
    /// (H5), so a slow `remove_file` (network FS / Windows) can't serialize every other worker's `get`/`put`
    /// behind it. The just-inserted key has the highest tick, so `pop_first` (oldest) never evicts it unless
    /// it is the sole entry (budget < one tile — degenerate); LRU recency is preserved.
    fn evict_to(&mut self, budget: u64) -> Vec<(u128, u32)> {
        let mut to_unlink = Vec::new();
        while self.total > budget {
            let Some((_, key)) = self.lru.pop_first() else { break };
            if let Some(e) = self.entries.remove(&key) {
                self.total = self.total.saturating_sub(e.size);
                to_unlink.push((key, e.crc));
            }
        }
        to_unlink
    }
}

/// FNV-1a 128 over the URL's PATH (query/fragment stripped) — the session-independent tile identity. A
/// 128-bit digest makes a cross-URL collision (which would serve the wrong tile's bytes) negligible at any
/// realistic cache size, unlike a 64-bit key (audit M16).
fn cache_key(url: &str) -> u128 {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    // FNV-1a 128 constants (offset basis + prime), per the canonical FNV spec.
    let mut h: u128 = 0x6C62_272E_07BB_0142_62B8_2175_6295_C58D;
    const PRIME: u128 = 0x0000_0000_0100_0000_0000_0000_0000_013B;
    for b in path.as_bytes() {
        h ^= *b as u128;
        h = h.wrapping_mul(PRIME);
    }
    h
}

/// `<dir>/<first-2-hex>/<32-hex-key>.<8-hex-crc>.bin` — sharded so a 100k-file cache doesn't put everything in
/// one dir, and the CRC-32 of the stored bytes is baked into the name so `open` recovers it by a pure filename
/// scan (no per-file read) and `get` can verify the bytes against it (audit M1).
fn cache_path(dir: &Path, key: u128, crc: u32) -> PathBuf {
    let name = format!("{key:032x}");
    dir.join(&name[..2]).join(format!("{name}.{crc:08x}.bin"))
}

/// Parse a cache filename stem `<32-hex-key>.<8-hex-crc>` back into `(key, crc)`. Returns `None` for any stem
/// that isn't exactly that shape — a stale pre-integrity `<key>` (no embedded CRC) or foreign junk — which
/// `open` then unlinks (audit M2 / no-compat reclaim). `.tmpN` files never reach here: `open` filters on the
/// `.bin` extension first, and a half-written temp's extension is `tmpN`.
fn parse_cache_stem(stem: &str) -> Option<(u128, u32)> {
    let (k, c) = stem.split_once('.')?;
    if k.len() != 32 || c.len() != 8 {
        return None;
    }
    Some((u128::from_str_radix(k, 16).ok()?, u32::from_str_radix(c, 16).ok()?))
}

/// `true` if `p`'s final component is exactly two lowercase-hex chars — a cache shard dir (the shape
/// [`cache_path`] makes via `&name[..2]`). `open`'s destructive reclaim descends ONLY into shard dirs, so a
/// FOREIGN nested dir sharing this root (e.g. the M3 geometry tier at `<dir>/geom`) is skipped, never scanned
/// into and `remove_file`-swept (audit recheck L-3).
fn is_shard_dir(p: &Path) -> bool {
    p.file_name()
        .and_then(|s| s.to_str())
        .is_some_and(|s| s.len() == 2 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')))
}

/// A persistent, byte-budgeted, LRU on-disk cache of fetched tile bytes. Cheap clones (it's `Arc`-shared
/// across the worker pool).
pub struct DiskCache {
    dir: PathBuf,
    budget: u64,
    idx: Mutex<CacheIndex>,
    hits: AtomicU64,   // cumulative get_raw hits (audit L1 — un-blinds hit-rate / budget tuning)
    misses: AtomicU64, // cumulative get_raw misses
}

impl DiskCache {
    /// Open (creating) a cache at `dir` bounded to `budget_bytes`. Rebuilds its index by scanning the
    /// existing files (reclaiming orphaned `.tmpN` temp files + stale pre-integrity entries — audit M2).
    /// Recency order is the scan order, not mtime: filesystem mtime is non-monotonic and is
    /// deliberately NOT used as an LRU signal (audit M16) — live `touch`/`insert` re-establish real recency
    /// the moment the cache is used, so the only effect is a slightly-arbitrary first cold-start eviction.
    pub fn open(dir: impl Into<PathBuf>, budget_bytes: u64) -> std::io::Result<Self> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir)?;
        let mut found: Vec<(u128, u64, u32)> = Vec::new();
        for shard in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            // Descend ONLY into well-formed 2-hex shard dirs, so the destructive reclaim below can never wander
            // into a foreign nested cache sharing this root (e.g. the M3 geometry tier at `<dir>/geom`) — recheck L-3.
            if !shard.path().is_dir() || !is_shard_dir(&shard.path()) {
                continue;
            }
            for f in std::fs::read_dir(shard.path()).into_iter().flatten().flatten() {
                let p = f.path();
                // Reclaim anything that is NOT a well-formed `<32-hex-key>.<8-hex-crc>.bin`: an orphaned `.tmpN`
                // from a write killed mid-rename (audit M2 — otherwise never scanned, never budgeted, leaked
                // forever), a stale pre-integrity `<key>.bin` (no-compat — re-fetched on demand), or foreign junk.
                // Safe to unlink only HERE: `open` is the constructor, run BEFORE any worker / `put` exists (the
                // `Arc<DiskCache>` reaches `HttpSource` only after this returns), so there is provably no
                // concurrent writer and no live reader holding any of these files.
                let parsed = (p.extension().and_then(|e| e.to_str()) == Some("bin"))
                    .then(|| p.file_stem().and_then(|s| s.to_str()).and_then(parse_cache_stem))
                    .flatten();
                let Some((key, crc)) = parsed else {
                    let _ = std::fs::remove_file(&p); // unparseable name / orphan .tmpN → reclaim (M2)
                    continue;
                };
                let Ok(md) = f.metadata() else {
                    // Well-formed NAME but a transient metadata() error (AV/indexer lock, permission race): SKIP,
                    // don't unlink — it's a valid cached tile that reads fine next open, and M1's CRC-on-read
                    // catches genuine corruption harmlessly. Unlinking it was an over-applied M2 reclaim (recheck L-4).
                    continue;
                };
                found.push((key, md.len(), crc));
            }
        }
        let mut entries = HashMap::new();
        let mut lru = BTreeMap::new();
        let mut total = 0u64;
        let mut tick = 0u64;
        for (key, size, crc) in found {
            tick += 1;
            entries.insert(key, CacheEntry { size, used: tick, crc });
            lru.insert(tick, key);
            total = total.saturating_add(size);
        }
        Ok(Self {
            dir,
            budget: budget_bytes,
            idx: Mutex::new(CacheIndex {
                entries,
                lru,
                total,
                tick,
            }),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
        })
    }

    /// Cached bytes for `url` if present AND intact (bumping LRU recency). Thin wrapper over
    /// [`get_raw`](Self::get_raw) keyed by the URL-path hash.
    pub fn get(&self, url: &str) -> Option<Vec<u8>> {
        self.get_raw(cache_key(url))
    }

    /// Cached bytes for a caller-supplied 128-bit `key` if present AND intact (bumping LRU recency); reads the
    /// file outside the index lock. Bytes are verified against the CRC-32 baked into the filename (audit M1): a
    /// mismatch — bit-rot, external truncation, a filesystem short read — OR a missing file (a concurrent evict
    /// unlinked it, or a crash lost it) is a clean miss: the stale index entry + its `total` contribution + its
    /// file are dropped (self-reconcile — also closes the get-TOCTOU phantom-entry path), so the caller cleanly
    /// re-fetches/re-builds. Bumps the hit/miss counters (audit L1). A SECOND store reuses this for non-URL
    /// blobs — the persisted decoded-geometry tier keys on `(origin, tile_key)` (audit M3).
    pub fn get_raw(&self, key: u128) -> Option<Vec<u8>> {
        let crc = {
            let idx = self.idx.lock().ok()?;
            match idx.entries.get(&key) {
                Some(e) => e.crc,
                None => {
                    self.misses.fetch_add(1, Ordering::Relaxed);
                    return None;
                }
            }
        };
        let path = cache_path(&self.dir, key, crc);
        if let Some(bytes) = std::fs::read(&path).ok().filter(|b| crc32fast::hash(b) == crc) {
            if let Ok(mut idx) = self.idx.lock() {
                idx.touch(key);
            }
            self.hits.fetch_add(1, Ordering::Relaxed);
            return Some(bytes);
        }
        // Missing or corrupt → drop the poisoned entry (keeping `total`/`lru` honest) and its file, then miss.
        // Guard on the CRC we tried: if a concurrent `put` already replaced this key with new content (a
        // different CRC), leave that fresh entry untouched — only this superseded file is stale.
        if let Ok(mut idx) = self.idx.lock() {
            if idx.entries.get(&key).is_some_and(|e| e.crc == crc) {
                if let Some(e) = idx.entries.remove(&key) {
                    idx.lru.remove(&e.used);
                    idx.total = idx.total.saturating_sub(e.size);
                }
            }
        }
        let _ = std::fs::remove_file(&path);
        self.misses.fetch_add(1, Ordering::Relaxed);
        None
    }

    /// Store `bytes` for `url` (atomic temp+rename so a crash never leaves a partial file), then evict the
    /// oldest entries until back within budget. Thin wrapper over [`put_raw`](Self::put_raw).
    pub fn put(&self, url: &str, bytes: &[u8]) {
        self.put_raw(cache_key(url), bytes);
    }

    /// Store `bytes` under a caller-supplied 128-bit `key` (no URL) — the geometry tier's `(origin, tile_key)`
    /// store reuses this (audit M3). Atomic temp+rename; evicts to budget; CRC baked into the filename.
    pub fn put_raw(&self, key: u128, bytes: &[u8]) {
        if bytes.is_empty() || self.budget == 0 {
            return;
        }
        let crc = crc32fast::hash(bytes);
        let path = cache_path(&self.dir, key, crc);
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        // Unique per-write tmp name: a single fixed ".tmp" per key lets two workers writing the SAME content URL
        // interleave their bytes into one file, so the rename could promote a torn, same-length-but-corrupt blob
        // (disk-cache audit Issue D). A process-unique suffix gives every concurrent write its own tmp file; the
        // rename to the final path stays atomic, so a reader still sees either the complete old or complete new file.
        static TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmp = path.with_extension(format!("tmp{seq}"));
        if std::fs::write(&tmp, bytes).is_err() {
            return;
        }
        if std::fs::rename(&tmp, &path).is_err() {
            let _ = std::fs::remove_file(&tmp);
            return;
        }
        // Update the index under the lock, but unlink the evicted files OUTSIDE it (H5): `remove_file` can be
        // slow (network FS / Windows), and holding `idx` across it would serialize every other worker's
        // `get`/`put`/`bytes` behind the disk I/O. The worst case of the wider window is a benign cache miss
        // (a key re-`put` between unlock and unlink loses its file but keeps its index entry → next `get`
        // reads a missing file → graceful `None`, then re-fetch) — acceptable for a best-effort cache.
        let (stale, to_unlink) = match self.idx.lock() {
            Ok(mut idx) => {
                let stale = idx.insert(key, bytes.len() as u64, crc);
                (stale, idx.evict_to(self.budget))
            }
            Err(_) => return,
        };
        // A re-`put` of CHANGED bytes for the same key lands in a different `<key>.<crc>.bin` file — unlink the
        // superseded one so the crc-in-filename scheme can't strand it (rare; tile content is path-immutable).
        if let Some(old_crc) = stale {
            let _ = std::fs::remove_file(cache_path(&self.dir, key, old_crc));
        }
        for (k, c) in to_unlink {
            let _ = std::fs::remove_file(cache_path(&self.dir, k, c));
        }
    }

    /// Current resident byte total (for telemetry).
    pub fn bytes(&self) -> u64 {
        self.idx.lock().map(|i| i.total).unwrap_or(0)
    }

    /// `true` if `key` is currently indexed (no file read). Lets the geometry-tier writer skip re-serializing +
    /// re-writing a tile already persisted this/last session (audit M3).
    pub fn contains(&self, key: u128) -> bool {
        self.idx.lock().map(|i| i.entries.contains_key(&key)).unwrap_or(false)
    }

    /// Cumulative cache hits / misses since `open` (audit L1 telemetry — un-blinds hit-rate / budget tuning).
    pub fn hits(&self) -> u64 {
        self.hits.load(Ordering::Relaxed)
    }
    pub fn misses(&self) -> u64 {
        self.misses.load(Ordering::Relaxed)
    }
}

/// THE disk cache — ONE cell (ADR-0019), opened on one root directory with one total budget, containing
/// two tiers as sub-cells:
///
/// * **`bytes`** (`<root>/`) — raw fetched payloads, keyed by URL path (session-independent). Consulted by
///   the fetch sources before the network; its value is surviving decode-format bumps and serving content
///   the geometry tier evicted or never held (tileset manifests, `.subtree` files).
/// * **`geom`** (`<root>/geom/`) — persisted decoded [`TileGeometry`] blobs, keyed by `(origin, tile_key)`
///   (audit M3). A hit skips the dominant Draco decode + rebase entirely, which is why restores consult it
///   before any fetch is even issued — it SHADOWS the byte tier for everything previously decoded.
///
/// The cell's membrane is its aggregate telemetry: a disk **hit** is any tier serving (geometry restore OR
/// cached bytes — work the disk saved), a disk **miss** is a request that ultimately needed the network
/// (the byte tier's miss; a geometry miss that then hits bytes is one HIT, not a miss). The total budget is
/// split evenly between the tiers: the geometry tier is the high-value half (it shadows bytes for
/// revisits), and before the merge it ran at a hardcoded fraction of the byte budget and sat pinned at its
/// cap while the byte tier idled at 9× the size with a ~0% hit rate.
///
/// The two tiers stay independently `Arc`-shareable because they live in different cells at runtime — the
/// fetch source owns the byte tier's probe path, the Geo World owns the geometry tier's — but they are
/// CREATED, BUDGETED, and OBSERVED as one cell, here.
pub struct TileDiskCache {
    pub bytes: Arc<DiskCache>,
    /// `None` = the geometry tier's subdir couldn't open (degraded: bytes-only, re-decode on every miss).
    /// Never a zero-budget stand-in on the byte tier's directory — two `DiskCache` indexes over one dir
    /// would unlink each other's files on trim.
    pub geom: Option<Arc<DiskCache>>,
}

impl TileDiskCache {
    /// Open both tiers under `root`, splitting `total_budget_bytes` evenly between them. Fails only if the
    /// BYTE tier can't open (no disk caching at all); a geometry-tier failure degrades to bytes-only
    /// (re-decoding on every miss) with the error returned alongside so the caller can log it.
    pub fn open(root: impl Into<PathBuf>, total_budget_bytes: u64) -> std::io::Result<(Self, Option<std::io::Error>)> {
        let root = root.into();
        let per_tier = (total_budget_bytes / 2).max(1);
        let bytes = Arc::new(DiskCache::open(&root, per_tier)?);
        let (geom, geom_error) = match DiskCache::open(root.join("geom"), per_tier) {
            Ok(g) => (Some(Arc::new(g)), None),
            Err(e) => (None, Some(e)),
        };
        Ok((Self { bytes, geom }, geom_error))
    }

    /// Aggregate occupancy across both tiers (the cell's one "disk MB" number).
    pub fn total_bytes(&self) -> u64 {
        self.bytes.bytes() + self.geom.as_ref().map(|g| g.bytes()).unwrap_or(0)
    }
    /// Aggregate hits: any tier serving saved work (geometry restore or cached bytes).
    pub fn total_hits(&self) -> u64 {
        self.bytes.hits() + self.geom.as_ref().map(|g| g.hits()).unwrap_or(0)
    }
    /// Aggregate misses: requests that needed the network — the BYTE tier's misses only (a geometry miss
    /// that then hits bytes already counted as a hit; counting it as a miss too would double-count the probe).
    pub fn total_misses(&self) -> u64 {
        self.bytes.misses()
    }
}

/// A worker-pool HTTP [`TileContentSource`]: `request` enqueues a fully-resolved URL, background workers
/// fetch it (blocking ureq), and `poll_completed` drains finished tiles on the owner thread. The caller
/// (the scene) resolves the URL via [`resolve_url`] before requesting. Workers exit when the source drops.
/// An optional [`DiskCache`] is consulted before the network and populated after a miss.
/// The per-request cancel flag, shared between the owner (which flips it) and the worker fetching that
/// request (which probes it once per read chunk). One `Arc<AtomicBool>` per in-flight id.
type CancelFlag = Arc<AtomicBool>;

/// The registry of live cancel flags, keyed by request id, shared owner ↔ workers. The owner inserts on
/// `request`, looks up + flips on `cancel`, and reclaims at TWO sites: `poll_completed` (a POPPED request,
/// cancelled or not, yields exactly one drained result) and `cancel`'s amortized heap purge (a cancelled
/// request still QUEUED may sit below the live waterline and never pop — its entry is removed when the
/// oversized heap is rebuilt; re-eval 2026-06-09). Together the map stays bounded by the live in-flight
/// set + the bounded heap + a brief drain lag.
type CancelRegistry = Arc<Mutex<HashMap<RequestId, CancelFlag>>>;

/// One queued fetch in the worker pool's PRIORITY queue (audit H1). Workers pop the MAX, so the most
/// render-critical pending tile downloads first instead of FIFO. `seq` breaks ties FIFO-fairly (older first)
/// and supplies the total order the `f64` `score` lacks (`f64` is not `Ord`).
struct PrioReq {
    score: f64,
    seq: u64,
    id: RequestId,
    url: String,
    cancel: CancelFlag,
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

/// The shared pending-request priority queue + a shutdown flag behind a `Mutex`; the paired `Condvar` parks
/// idle workers and wakes them on a new request or on teardown. Replaces the old `flume` FIFO so the owner's
/// `fetch_priority` ordering governs SERVICE order, not just enqueue order (audit H1).
struct PendingQueue {
    heap: BinaryHeap<PrioReq>,
    shutdown: bool,
}

pub struct HttpSource {
    /// Shared pending-request PRIORITY queue (audit H1): the owner pushes `PrioReq`s; workers pop the MAX. The
    /// `Condvar` wakes one idle worker per push and wakes all on `Drop`'s shutdown.
    pending: Arc<(Mutex<PendingQueue>, Condvar)>,
    done_rx: flume::Receiver<Loaded>,
    /// Worker thread handles, joined on `Drop` so teardown never leaves fetch threads running detached (they
    /// each hold an `Arc<DiskCache>` clone — a thread + cache-retention leak across world re-creation).
    workers: Vec<JoinHandle<()>>,
    in_flight: HashSet<RequestId>,
    /// Monotonic sequence for `PrioReq` tie-breaking (FIFO fairness among equal-priority requests).
    next_seq: u64,
    cache: Option<Arc<DiskCache>>,
    /// Live per-request cancel flags (audit A3): `cancel` flips the request's `AtomicBool`, which the worker's
    /// chunked read loop probes each chunk and aborts on — a real mid-download cancel; it also short-circuits a
    /// not-yet-started fetch (the worker's pre-flight load). On `Drop` ALL flags are flipped so a worker
    /// mid-BODY frees promptly instead of finishing up to a 60 s fetch before observing shutdown (audit H3).
    /// Scope honestly: the flag is probed per body chunk and pre-flight ONLY — a worker blocked inside
    /// connect/TLS (`HTTP_CONNECT_TIMEOUT`, 10 s) or a stalled read (`HTTP_IO_TIMEOUT`, 30 s) does not see it
    /// until that phase times out, so a black-holed network can hold `Drop`'s join up to those bounds
    /// (re-eval 2026-06-09; the default CyperSource lane cancels via task drop and has no such window).
    cancels: CancelRegistry,
}

impl HttpSource {
    /// Spawn `workers` (clamped ≥ 1) fetch threads draining a shared PRIORITY queue, optionally backed by a
    /// persistent [`DiskCache`]. Each freed worker pops the highest-`fetch_priority` pending request (audit H1).
    pub fn new(workers: usize, cache: Option<Arc<DiskCache>>) -> Self {
        let pending = Arc::new((
            Mutex::new(PendingQueue {
                heap: BinaryHeap::new(),
                shutdown: false,
            }),
            Condvar::new(),
        ));
        let (done_tx, done_rx) = flume::unbounded::<Loaded>();
        let cancels: CancelRegistry = Arc::new(Mutex::new(HashMap::new()));
        let mut handles = Vec::with_capacity(workers.max(1));
        for _ in 0..workers.max(1) {
            let pending = Arc::clone(&pending);
            let done_tx = done_tx.clone();
            let cache = cache.clone();
            let handle = std::thread::Builder::new()
                .name("LogiTileFetch".to_string())
                .spawn(move || {
                    let (lock, cvar) = &*pending;
                    loop {
                        // Pop the MAX-priority pending request; park on the Condvar while the queue is empty. The
                        // inner loop re-checks shutdown + pop on every (incl. spurious) wake-up, then re-parks.
                        let req = {
                            let mut q = lock.lock().unwrap_or_else(|e| e.into_inner());
                            loop {
                                if q.shutdown {
                                    return; // teardown — exit the worker
                                }
                                if let Some(req) = q.heap.pop() {
                                    break req;
                                }
                                q = cvar.wait(q).unwrap_or_else(|e| e.into_inner());
                            }
                        };
                        // Cache-first, cancel-aware fetch → a TYPED outcome (audit A10). `fetch_one` owns the
                        // hit/miss/populate + cancellation logic; a failure is never collapsed to empty bytes and
                        // never poisons the cache. The queue lock is released before this blocking call.
                        // L4: Arc-ify the bytes HERE, on the worker (parallel), so the owner's `drain_raw`
                        // parks them on the entity with a cheap Arc move — the one alloc+copy stays off the
                        // serial owner thread. A classified failure stays a typed `Err` (carries no bytes).
                        let result: Result<Arc<[u8]>, FetchError> =
                            fetch_one(cache.as_deref(), &req.url, &req.cancel).map(Arc::from);
                        if done_tx.send(Loaded { id: req.id, result }).is_err() {
                            return; // owner gone
                        }
                    }
                })
                .expect("spawn LogiTileFetch worker");
            handles.push(handle);
        }
        Self {
            pending,
            done_rx,
            workers: handles,
            in_flight: HashSet::new(),
            next_seq: 0,
            cache,
            cancels,
        }
    }

    /// Bytes currently resident in the disk cache (0 if no cache).
    pub fn cached_bytes(&self) -> u64 {
        self.cache.as_ref().map(|c| c.bytes()).unwrap_or(0)
    }
}

impl Drop for HttpSource {
    fn drop(&mut self) {
        // Teardown must never leave a fetch thread running detached (each holds an `Arc<DiskCache>` clone — a
        // thread + cache-retention leak on every world re-creation / editor hot-reload). (1) Flip EVERY live
        // cancel flag so a worker mid-`http_get` aborts at its next chunk probe instead of finishing up to a
        // 60 s download before noticing (audit H3). (2) Set `shutdown` + `notify_all` so idle workers parked on
        // the Condvar wake and exit. (3) Join. Mirrors `GlobeStreamer`'s join-on-`Drop`.
        if let Ok(c) = self.cancels.lock() {
            for flag in c.values() {
                flag.store(true, Ordering::Relaxed);
            }
        }
        {
            let (lock, cvar) = &*self.pending;
            let mut q = lock.lock().unwrap_or_else(|e| e.into_inner());
            q.shutdown = true;
            drop(q);
            cvar.notify_all();
        }
        for h in self.workers.drain(..) {
            let _ = h.join();
        }
    }
}

impl TileContentSource for HttpSource {
    fn request(&mut self, id: RequestId, url: &str, priority: f64, _now_tick: u64) {
        // Mint this request's cancel flag and register it BEFORE pushing, so a `cancel` racing in right after a
        // worker pops it still finds the flag and can abort the in-progress download (A3).
        let flag: CancelFlag = Arc::new(AtomicBool::new(false));
        if let Ok(mut c) = self.cancels.lock() {
            c.insert(id, Arc::clone(&flag));
        }
        let seq = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1);
        let (lock, cvar) = &*self.pending;
        let mut q = lock.lock().unwrap_or_else(|e| e.into_inner());
        if q.shutdown {
            // Teardown in progress: undo the registry insert so it can't leak (no worker will drain a result).
            drop(q);
            if let Ok(mut c) = self.cancels.lock() {
                c.remove(&id);
            }
            return;
        }
        q.heap.push(PrioReq {
            score: priority,
            seq,
            id,
            url: url.to_string(),
            cancel: flag,
        });
        drop(q);
        cvar.notify_one(); // wake one idle worker to service it
        self.in_flight.insert(id);
    }
    fn poll_completed(&mut self, _now_tick: u64, out: &mut Vec<Loaded>) {
        while let Ok(loaded) = self.done_rx.try_recv() {
            // This request is finished (success, classified failure, or cancelled) → retire its cancel flag
            // from the registry. Every request yields exactly one result, so this is the single, definitive
            // reclamation point — the map stays bounded by the live in-flight set + a brief drain lag.
            if let Ok(mut c) = self.cancels.lock() {
                c.remove(&loaded.id);
            }
            // Backstop soft-drop: if the owner cancelled AFTER the bytes were already fully on the wire (too
            // late for the mid-download abort), the id is gone from `in_flight` → drop the stale result.
            if self.in_flight.remove(&loaded.id) {
                out.push(loaded);
            }
        }
    }
    fn cancel(&mut self, id: RequestId) {
        if self.in_flight.remove(&id) {
            // Flip the live cancel flag (A3): a worker mid-download probes it each chunk and aborts early
            // (returning `Err(Cancelled)` instead of finishing up to 64 MiB it would then discard); a
            // not-yet-started worker's pre-flight load short-circuits the fetch entirely. A POPPED request
            // still produces one result, which `poll_completed` drains + removes from the registry — so we
            // do NOT remove a popped request's registry entry here (the worker may be reading the flag).
            // The `Relaxed` store pairs with the worker's `Relaxed` loads (eventual visibility is all we
            // need; no other memory is published through this flag).
            if let Ok(c) = self.cancels.lock() {
                if let Some(flag) = c.get(&id) {
                    flag.store(true, Ordering::Relaxed);
                }
            }
            // Amortized tombstone purge (re-eval 2026-06-09): a cancelled request still QUEUED in the heap
            // is dead weight a worker only reclaims by popping it — and an entry whose stale score sits
            // below the live waterline (a cancelled −1e9 prefetch under sustained motion) may never pop, so
            // heap + registry grew without bound for the duration of continuous flight. When the heap
            // outgrows the live set, rebuild it without cancelled entries. SAFE to drop their registry
            // entries directly: a worker only ever touches a request's flag AFTER popping its `PrioReq`,
            // so an entry still in the heap has no worker reader; and the scene already settled its own
            // bookkeeping at cancel time (`cancel_stale` removes its in-flight map/path/index entries), so
            // no `Loaded` result is owed for a purged id. O(heap) rebuild amortized over ≥ half-heap cancels.
            let (lock, _cvar) = &*self.pending;
            let mut q = lock.lock().unwrap_or_else(|e| e.into_inner());
            if q.heap.len() > 2 * self.in_flight.len() + 64 {
                let drained = std::mem::take(&mut q.heap).into_vec();
                let (keep, purged): (Vec<PrioReq>, Vec<PrioReq>) =
                    drained.into_iter().partition(|r| !r.cancel.load(Ordering::Relaxed));
                q.heap = BinaryHeap::from(keep);
                drop(q);
                if !purged.is_empty() {
                    if let Ok(mut c) = self.cancels.lock() {
                        for r in &purged {
                            c.remove(&r.id);
                        }
                    }
                }
            }
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
        self.workers.len().max(1)
    }
}

#[cfg(test)]
mod fetch_tests {
    use super::*;
    use std::io::Write;
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    // ---- A1: status classification (transient vs terminal) ------------------------------------------

    #[test]
    fn http_status_classification_transient_vs_terminal() {
        // Transient (retry-worthy): 429 + every 5xx.
        for code in [429u16, 500, 502, 503, 504, 599] {
            let e = FetchError::Http {
                status: code,
                retry_after: None,
            };
            assert!(e.is_transient(), "HTTP {code} must be transient (retry-worthy)");
        }
        // Terminal (give up now): every 4xx except 429.
        for code in [400u16, 401, 403, 404, 410, 451] {
            let e = FetchError::Http {
                status: code,
                retry_after: None,
            };
            assert!(!e.is_transient(), "HTTP {code} must be terminal (do not retry)");
        }
        // Transport failures are always transient; URL / TooLarge / Cancelled are terminal (never retried).
        assert!(FetchError::Transport("dns".into()).is_transient());
        assert!(!FetchError::Url("bad".into()).is_transient());
        assert!(!FetchError::TooLarge.is_transient());
        assert!(
            !FetchError::Cancelled.is_transient(),
            "a cancelled fetch must not be re-queued on the retry path"
        );
    }

    // ---- A1: Retry-After parsing --------------------------------------------------------------------

    #[test]
    fn retry_after_parses_delta_seconds_only() {
        assert_eq!(parse_retry_after("120"), Some(Duration::from_secs(120)));
        assert_eq!(
            parse_retry_after("  30 "),
            Some(Duration::from_secs(30)),
            "trims whitespace"
        );
        // 0 / non-numeric / HTTP-date → None (owner falls back to its own backoff; a 0 can't mean "now").
        assert_eq!(parse_retry_after("0"), None);
        assert_eq!(parse_retry_after("soon"), None);
        assert_eq!(parse_retry_after("Wed, 21 Oct 2025 07:28:00 GMT"), None);
        // The hint surfaces on the error and via the accessor; non-HTTP errors have no hint.
        let e = FetchError::Http {
            status: 503,
            retry_after: Some(Duration::from_secs(5)),
        };
        assert_eq!(e.retry_after(), Some(Duration::from_secs(5)));
        assert_eq!(FetchError::Transport("x".into()).retry_after(), None);
    }

    // ---- A1/A3 live-server fetch tests --------------------------------------------------------------
    //
    // A one-shot local HTTP/1.1 server on an ephemeral 127.0.0.1 port — no test-server dependency. Each
    // helper spawns a thread that accepts ONE connection, serves a hand-rolled response, and returns the
    // bound URL. `ureq` drives a real socket against it, so these exercise the actual `http_get_cancellable`
    // read path (status branch, empty-200, chunked cancel) end to end.

    /// Accept one connection and write `response_head` (status line + headers + blank line) followed by
    /// `body`, then close. Returns the `http://127.0.0.1:<port>/` URL to GET.
    fn serve_once(response_head: &'static str, body: Vec<u8>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral");
        let url = format!("http://{}/", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            if let Ok((mut sock, _)) = listener.accept() {
                // Read (and ignore) the request line/headers up to the blank line so the client can finish
                // writing before we respond — otherwise a write-then-close can RST the request on some OSes.
                let mut buf = [0u8; 1024];
                let _ = std::io::Read::read(&mut sock, &mut buf);
                let _ = sock.write_all(response_head.as_bytes());
                let _ = sock.write_all(&body);
                let _ = sock.flush();
            }
        });
        url
    }

    #[test]
    fn empty_200_is_ok_not_failure() {
        // The crux of A10/A1: a legitimate empty-200 body must be `Ok(empty)`, NOT collapsed to a failure.
        let url = serve_once("HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n", Vec::new());
        let cancel = AtomicBool::new(false);
        let got = http_get_cancellable(&url, &cancel);
        assert_eq!(got, Ok(Vec::new()), "empty-200 is success, not an error");
    }

    #[test]
    fn ok_200_returns_body_bytes() {
        let url = serve_once("HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\n", b"hello".to_vec());
        let cancel = AtomicBool::new(false);
        assert_eq!(http_get_cancellable(&url, &cancel), Ok(b"hello".to_vec()));
    }

    #[test]
    fn status_404_is_terminal_http_error() {
        let url = serve_once("HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n", Vec::new());
        let cancel = AtomicBool::new(false);
        match http_get_cancellable(&url, &cancel) {
            Err(e @ FetchError::Http { status: 404, .. }) => {
                assert!(!e.is_transient(), "404 is terminal")
            }
            other => panic!("expected terminal HTTP 404, got {other:?}"),
        }
    }

    #[test]
    fn status_503_with_retry_after_is_transient_and_carries_hint() {
        // A rate-limited / overloaded server: transient, and we capture its Retry-After so the owner honors
        // it instead of hammering back immediately (the North Star: don't melt the tile server).
        let url = serve_once(
            "HTTP/1.1 503 Service Unavailable\r\nRetry-After: 7\r\nContent-Length: 0\r\n\r\n",
            Vec::new(),
        );
        let cancel = AtomicBool::new(false);
        match http_get_cancellable(&url, &cancel) {
            Err(e @ FetchError::Http { status: 503, .. }) => {
                assert!(e.is_transient(), "503 is transient");
                assert_eq!(e.retry_after(), Some(Duration::from_secs(7)), "Retry-After captured");
            }
            other => panic!("expected transient HTTP 503, got {other:?}"),
        }
    }

    #[test]
    fn preflight_cancel_skips_the_fetch() {
        // Flag already set before the read starts → bail without downloading (subsumes the old "skip before
        // dequeue" hard cancel). The body is large enough that if we DID download it the test would notice.
        let url = serve_once("HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\n", b"data".to_vec());
        let cancel = AtomicBool::new(true);
        assert_eq!(http_get_cancellable(&url, &cancel), Err(FetchError::Cancelled));
    }

    #[test]
    fn mid_download_cancel_aborts_early() {
        // The core A3 fix: a worker already INSIDE the read loop must abort the instant the owner cancels —
        // not finish the (up to 64 MiB) download and discard it. We drip a large body in chunks; once the
        // first chunk is on the wire we flip the flag and assert the read returns `Cancelled` having pulled
        // only a SMALL fraction of the declared body (proving it didn't download the whole thing).
        const TOTAL: usize = 8 * 1024 * 1024; // 8 MiB declared
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let (first_chunk_tx, first_chunk_rx) = mpsc::channel::<()>();

        let server = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().expect("accept");
            let mut buf = [0u8; 1024];
            let _ = std::io::Read::read(&mut sock, &mut buf);
            let head = format!("HTTP/1.1 200 OK\r\nContent-Length: {TOTAL}\r\n\r\n");
            if sock.write_all(head.as_bytes()).is_err() {
                return;
            }
            let chunk = vec![0u8; READ_CHUNK_BYTES];
            let mut sent = 0usize;
            // Drip with a small per-chunk sleep so the client reliably observes the cancel flag within a
            // chunk or two — and so the WHOLE body would take ~hundreds of ms, far longer than the abort.
            while sent < TOTAL {
                if sock.write_all(&chunk).is_err() {
                    break; // client dropped the connection on cancel — expected, stop dripping.
                }
                let _ = sock.flush();
                if sent == 0 {
                    let _ = first_chunk_tx.send(()); // signal: bytes are now flowing.
                }
                sent += chunk.len();
                std::thread::sleep(Duration::from_millis(2));
            }
        });

        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_for_fetch = Arc::clone(&cancel);
        let url_for_fetch = url.clone();
        let started = Instant::now();
        let fetcher = std::thread::spawn(move || http_get_cancellable(&url_for_fetch, &cancel_for_fetch));

        // Wait until the server has actually put the first chunk on the wire, THEN cancel — guarantees we
        // are testing the mid-download read-loop abort, not the pre-flight short-circuit.
        first_chunk_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("server should send a first chunk");
        cancel.store(true, Ordering::Relaxed);

        let got = fetcher.join().expect("fetch thread");
        let elapsed = started.elapsed();
        let _ = server.join();

        assert_eq!(
            got,
            Err(FetchError::Cancelled),
            "mid-download cancel must yield Cancelled"
        );
        // Aborted EARLY: dripping all 8 MiB at 2ms/64KiB would take ~250ms+; the abort returns well before
        // that. A generous 3s ceiling keeps this robust on a loaded CI box while still proving we did not
        // wait out the full body.
        assert!(
            elapsed < Duration::from_secs(3),
            "cancel should abort promptly, took {elapsed:?}"
        );
    }

    // ---- HttpSource end-to-end cancel ---------------------------------------------------------------

    #[test]
    fn httpsource_cancel_frees_in_flight() {
        // The public surface: a cancelled id leaves the in-flight set and never surfaces from poll_completed
        // (the result, whatever it is, is dropped). Point it at a dead port so the fetch fails fast.
        let mut src = HttpSource::new(2, None);
        src.request(1, "http://127.0.0.1:1/never", 0.0, 0);
        src.request(2, "http://127.0.0.1:1/never", 0.0, 0);
        assert_eq!(src.in_flight(), 2);
        src.cancel(1);
        assert_eq!(src.in_flight(), 1, "cancel drops the id from in-flight immediately");

        // Drain until id 2's (failed) result arrives; id 1 must NEVER appear.
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

    #[test]
    fn prio_req_heap_pops_highest_score_then_fifo() {
        // The worker pool's priority queue (audit H1) must pop the GREATEST `fetch_priority` first, and among
        // EQUAL scores the OLDER request (smaller `seq`) — the contract that makes "highest-priority-to-render
        // fetched first" hold regardless of arrival order across passes.
        use std::collections::BinaryHeap;
        let mk = |score: f64, seq: u64, id: RequestId| PrioReq {
            score,
            seq,
            id,
            url: String::new(),
            cancel: Arc::new(AtomicBool::new(false)),
        };
        let mut h = BinaryHeap::new();
        h.push(mk(1.0, 0, 10)); // low score, but enqueued FIRST
        h.push(mk(5.0, 1, 20)); // high score
        h.push(mk(5.0, 2, 21)); // high score, later seq → serviced after 20
        h.push(mk(3.0, 3, 30)); // mid score
        assert_eq!(h.pop().unwrap().id, 20, "highest score first");
        assert_eq!(h.pop().unwrap().id, 21, "equal score → FIFO (smaller seq first)");
        assert_eq!(h.pop().unwrap().id, 30, "then the mid score");
        assert_eq!(h.pop().unwrap().id, 10, "then the lowest, despite being enqueued first");
    }
}

#[cfg(test)]
mod http_tests {
    use super::resolve_url;

    #[test]
    fn carries_base_auth_onto_child_requests() {
        // Google P3DT: host-relative content URI with its own session; the base's key is carried over.
        let u = resolve_url(
            "https://tile.googleapis.com/v1/3dtiles/root.json?key=ABC",
            "/v1/3dtiles/x.glb?session=XYZ",
        )
        .unwrap();
        assert!(u.starts_with("https://tile.googleapis.com/v1/3dtiles/x.glb?"), "{u}");
        assert!(u.contains("session=XYZ") && u.contains("key=ABC"), "{u}");
        // ion direct: a path-relative content URI resolves against the tileset.json dir + carries the token.
        let u = resolve_url("https://assets.cesium.com/1/tileset.json?access_token=T", "0/0.b3dm").unwrap();
        assert_eq!(u, "https://assets.cesium.com/1/0/0.b3dm?access_token=T");
        // a child URI that already has the param isn't duplicated.
        let u = resolve_url("https://h/r.json?key=A", "/c.glb?key=B").unwrap();
        assert_eq!(u, "https://h/c.glb?key=B");
    }
}

#[cfg(test)]
mod disk_cache_tests {
    use super::*;

    /// A unique, self-cleaning temp dir for one test (no `tempfile` dep). Removed on construction (idempotent
    /// re-run) and again by the caller at the end.
    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("logi_dc_{}_{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn put_evicts_oldest_and_unlinks_its_file_within_budget() {
        let dir = temp_dir("evict");
        let cache = DiskCache::open(&dir, 100).expect("open");

        cache.put("https://h/u1.glb", &[1u8; 60]); // total 60 ≤ 100 → kept
        cache.put("https://h/u2.glb", &[2u8; 60]); // total 120 > 100 → evict the oldest (u1) back to 60

        // The cold entry is gone from BOTH the index (get → None) and the disk (the file was unlinked — H5
        // does this after releasing the lock; this proves the deferred unlink still actually deletes).
        assert!(cache.get("https://h/u1.glb").is_none(), "u1 should have been evicted");
        assert!(
            !cache_path(&dir, cache_key("https://h/u1.glb"), crc32fast::hash(&[1u8; 60])).exists(),
            "u1's file must be unlinked on eviction"
        );
        // The fresh entry survives and is readable; the byte total reflects only it.
        assert_eq!(
            cache.get("https://h/u2.glb"),
            Some(vec![2u8; 60]),
            "u2 should still be cached"
        );
        assert_eq!(cache.bytes(), 60, "byte total reflects the single surviving entry");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cache_is_session_independent_and_survives_reopen() {
        let dir = temp_dir("reopen");
        {
            let cache = DiskCache::open(&dir, 10_000).expect("open");
            // Same PATH, different auth query → same key → one cached entry, replayable across sessions.
            cache.put("https://h/t.glb?session=A", &[7u8; 128]);
            assert_eq!(cache.get("https://h/t.glb?session=B"), Some(vec![7u8; 128]));
        }
        // Reopen: the index is rebuilt by scanning the dir, so the entry is still served (survives restart).
        let reopened = DiskCache::open(&dir, 10_000).expect("reopen");
        assert_eq!(reopened.get("https://h/t.glb?session=C"), Some(vec![7u8; 128]));
        assert_eq!(reopened.bytes(), 128);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_cached_file_is_detected_and_served_as_a_miss() {
        // M1 integrity: a cached file whose bytes drift from the CRC in its name (bit-rot / external
        // truncation / a partial write) must NOT be served — `get` detects the mismatch, drops the poisoned
        // entry + its file + its byte total, and reports a clean miss so `fetch_one` re-fetches.
        let dir = temp_dir("integrity");
        let cache = DiskCache::open(&dir, 10_000).expect("open");
        let url = "https://h/tile.glb";
        cache.put(url, &[9u8; 200]);
        assert_eq!(cache.get(url), Some(vec![9u8; 200]), "intact bytes are served");

        // Corrupt the on-disk file in place (same length, different bytes ⇒ same name, wrong CRC).
        let path = cache_path(&dir, cache_key(url), crc32fast::hash(&[9u8; 200]));
        std::fs::write(&path, [0u8; 200]).expect("overwrite in place");
        assert_eq!(cache.get(url), None, "corrupt bytes are rejected as a miss");
        assert!(!path.exists(), "the poisoned file is unlinked");
        assert_eq!(cache.bytes(), 0, "and its bytes are dropped from the budget total");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn open_reclaims_orphaned_tmp_and_stale_files() {
        // M2 (+ no-compat): a crash between write and rename strands a `<key>.<crc>.tmpN`; a pre-integrity
        // build leaves a `<key>.bin` (no embedded CRC). Both are invisible to the byte budget and must be
        // reclaimed on open — not leaked forever — while a real, well-formed entry survives.
        let dir = temp_dir("reclaim");
        {
            let cache = DiskCache::open(&dir, 10_000).expect("open");
            cache.put("https://h/keep.glb", &[5u8; 100]);
        }
        // Hand-plant an orphan temp + a stale legacy file in the (existing) shard dir of the real entry.
        let name = format!("{:032x}", cache_key("https://h/keep.glb"));
        let shard = dir.join(&name[..2]);
        let orphan_tmp = shard.join("deadbeefdeadbeefdeadbeefdeadbeef.cafebabe.tmp7");
        let legacy_bin = shard.join("00112233445566778899aabbccddeeff.bin");
        std::fs::write(&orphan_tmp, [0u8; 50]).expect("plant tmp");
        std::fs::write(&legacy_bin, [0u8; 50]).expect("plant legacy");

        let reopened = DiskCache::open(&dir, 10_000).expect("reopen");
        assert!(!orphan_tmp.exists(), "orphaned .tmpN reclaimed on open (M2)");
        assert!(
            !legacy_bin.exists(),
            "stale pre-integrity .bin reclaimed on open (no-compat)"
        );
        assert_eq!(
            reopened.get("https://h/keep.glb"),
            Some(vec![5u8; 100]),
            "the real, well-formed entry survives reclaim"
        );
        assert_eq!(reopened.bytes(), 100, "budget total counts only the real entry");

        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splitmix64_is_deterministic_and_mixes() {
        assert_eq!(splitmix64(0), splitmix64(0));
        assert_ne!(splitmix64(1), splitmix64(2));
        assert_ne!(splitmix64(0), 0);
    }

    #[test]
    fn synthetic_payload_is_deterministic_and_sized() {
        assert_eq!(synthetic_payload(42, 100).len(), 100);
        assert_eq!(synthetic_payload(42, 100), synthetic_payload(42, 100));
        assert_ne!(synthetic_payload(42, 100), synthetic_payload(43, 100));
    }

    #[test]
    fn request_completes_after_its_latency() {
        let mut src = SyntheticSource::new();
        let id = 7;
        src.request(id, "tile://7", 0.0, 0);
        assert_eq!(src.in_flight(), 1);

        let latency = src.latency_for(id);
        let mut out = Vec::new();
        // not ready yet
        src.poll_completed(latency - 1, &mut out);
        assert!(out.is_empty());
        assert_eq!(src.in_flight(), 1);
        // ready now
        src.poll_completed(latency, &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, id);
        // The synthetic lane always succeeds → `Ok(payload)` (audit A10: typed outcome, not bare bytes).
        assert_eq!(out[0].result, Ok(synthetic_payload(id, src.bytes_for(id)).into()));
        assert_eq!(src.in_flight(), 0);
    }

    #[test]
    fn cancel_removes_an_in_flight_request() {
        let mut src = SyntheticSource::new();
        src.request(3, "u", 0.0, 0);
        src.request(4, "u", 0.0, 0);
        assert_eq!(src.in_flight(), 2);
        src.cancel(3);
        assert_eq!(src.in_flight(), 1);
        let mut out = Vec::new();
        src.poll_completed(100, &mut out); // far past any latency
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, 4);
    }

    #[test]
    fn resolve_url_carries_auth_only_same_origin() {
        let base = "https://assets.cesium.com/1/tileset.json?access_token=SECRET";

        // Relative child → same origin → token is carried (the normal streaming case).
        let rel = resolve_url(base, "0/0/0.b3dm").unwrap();
        assert!(rel.starts_with("https://assets.cesium.com/1/0/0/0.b3dm"));
        assert!(
            rel.contains("access_token=SECRET"),
            "relative child must keep the token"
        );

        // Same-host absolute child → same origin → token carried.
        let same = resolve_url(base, "https://assets.cesium.com/other/x.glb").unwrap();
        assert!(same.contains("access_token=SECRET"));

        // Cross-origin absolute child → token MUST NOT leak (the exfiltration guard).
        let evil = resolve_url(base, "https://evil.example/x.glb").unwrap();
        assert!(!evil.contains("SECRET"), "token must NOT be sent cross-origin: {evil}");
        assert!(!evil.contains("access_token"), "no auth param cross-origin: {evil}");

        // Different port is a different origin too.
        let other_port = resolve_url(base, "https://assets.cesium.com:8443/x.glb").unwrap();
        assert!(
            !other_port.contains("SECRET"),
            "different port is cross-origin: {other_port}"
        );
    }

    #[test]
    fn replay_is_byte_for_byte() {
        // The same request sequence + poll schedule yields identical completions — the determinism the
        // streaming-stress / golden tests rely on.
        fn run() -> Vec<Loaded> {
            let mut src = SyntheticSource::new();
            for id in 0..20 {
                src.request(id, "u", 0.0, 0);
            }
            let mut out = Vec::new();
            for tick in 0..=12 {
                src.poll_completed(tick, &mut out);
            }
            out.sort_by_key(|l| l.id);
            out
        }
        assert_eq!(run(), run());
    }
}
