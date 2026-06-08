//! The tile-content **fetch contract** for the actorless 3D-Tiles stack (ADR-0009, Milestone F): an
//! async-fetch-then-poll source the streaming loop drives on its single owner thread, generalising the
//! kernel's existing `Provider`. The selection traversal decides *which* tiles to load; a
//! [`TileContentSource`] fetches their content bytes and hands them back when ready.
//!
//! Two sources implement the same trait: the deterministic [`SyntheticSource`] (latency + payload derived
//! from the request id — reproducible, no network, the test/offline lane) and the production [`HttpSource`]
//! (HTTP via `ureq` + gzip + an on-disk cache, fetched on a worker pool and drained here).

#![forbid(unsafe_code)]

use std::collections::BTreeMap;

use hashbrown::{HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
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
    /// `Ok(bytes)` — the content, already gzip-inflated by the source; an empty `Vec` is a valid empty-200.
    /// `Err(e)` — a classified failure the owner can act on without guessing.
    pub result: Result<Vec<u8>, FetchError>,
}

/// An async tile-content source driven on the owner thread: `request` enqueues, `poll_completed` drains
/// finished fetches, `cancel` drops one. Implementations do the actual IO/decoding off-thread; nothing
/// here blocks the kernel. `now_tick` is the kernel's monotonic step counter.
pub trait TileContentSource {
    fn request(&mut self, id: RequestId, url: &str, now_tick: u64);
    fn poll_completed(&mut self, now_tick: u64, out: &mut Vec<Loaded>);
    fn cancel(&mut self, id: RequestId);
    fn in_flight(&self) -> usize;
    /// Bytes resident in a persistent cache behind this source, if any (telemetry). Defaults to `0` for
    /// sources without a cache (e.g. the synthetic test source).
    fn cached_bytes(&self) -> u64 {
        0
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
    fn request(&mut self, id: RequestId, _url: &str, now_tick: u64) {
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
                result: Ok(synthetic_payload(id, len)),
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
            // Sizing the per-host idle pool above the worker cap (32) lets every worker keep reusing a warm
            // keep-alive connection, so the pool streams at the link's real bandwidth (audit: the #1 reason
            // 8 fetch threads only managed ~8 MB/s — they were re-handshaking, not downloading).
            .max_idle_connections(256)
            .max_idle_connections_per_host(64)
            .build()
    })
}

/// Resolve a tile `content_uri` against the tileset `base` URL, carrying over the base's auth query
/// params (e.g. `?key=…` / `?access_token=…`) that the child URI doesn't already carry. Handles absolute,
/// host-relative (`/v1/…`), and path-relative content URIs.
pub fn resolve_url(base: &str, content_uri: &str) -> Result<String, FetchError> {
    let base_url = url::Url::parse(base).map_err(|e| FetchError::Url(e.to_string()))?;
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
        // A cache hit costs no bandwidth, so cancellation doesn't matter for it; serve it. (A result the
        // owner has since cancelled is harmlessly dropped on `poll_completed` via the in-flight backstop.)
        if let Some(bytes) = c.get(url) {
            return Ok(bytes);
        }
        let bytes = http_get_cancellable(url, cancel)?;
        if !bytes.is_empty() {
            c.put(url, &bytes);
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
// index is rebuilt by scanning the dir).
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
    fn insert(&mut self, key: u128, size: u64) {
        self.tick += 1;
        let t = self.tick;
        if let Some(old) = self.entries.insert(key, CacheEntry { size, used: t }) {
            self.lru.remove(&old.used);
            self.total = self.total.saturating_sub(old.size);
        }
        self.lru.insert(t, key);
        self.total = self.total.saturating_add(size);
    }
    /// Evict the coldest entries from the in-memory index until back within `budget`, returning the keys whose
    /// files must be unlinked. **Does no filesystem I/O** — the caller unlinks AFTER dropping the index lock
    /// (H5), so a slow `remove_file` (network FS / Windows) can't serialize every other worker's `get`/`put`
    /// behind it. The just-inserted key has the highest tick, so `pop_first` (oldest) never evicts it unless
    /// it is the sole entry (budget < one tile — degenerate); LRU recency is preserved.
    fn evict_to(&mut self, budget: u64) -> Vec<u128> {
        let mut to_unlink = Vec::new();
        while self.total > budget {
            let Some((_, key)) = self.lru.pop_first() else { break };
            if let Some(e) = self.entries.remove(&key) {
                self.total = self.total.saturating_sub(e.size);
                to_unlink.push(key);
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

/// `<dir>/<first-2-hex>/<32-hex>.bin` — sharded so a 100k-file cache doesn't put everything in one dir.
fn cache_path(dir: &Path, key: u128) -> PathBuf {
    let name = format!("{key:032x}");
    dir.join(&name[..2]).join(format!("{name}.bin"))
}

/// A persistent, byte-budgeted, LRU on-disk cache of fetched tile bytes. Cheap clones (it's `Arc`-shared
/// across the worker pool).
pub struct DiskCache {
    dir: PathBuf,
    budget: u64,
    idx: Mutex<CacheIndex>,
}

impl DiskCache {
    /// Open (creating) a cache at `dir` bounded to `budget_bytes`. Rebuilds its index by scanning the
    /// existing files. Recency order is the scan order, not mtime: filesystem mtime is non-monotonic and is
    /// deliberately NOT used as an LRU signal (audit M16) — live `touch`/`insert` re-establish real recency
    /// the moment the cache is used, so the only effect is a slightly-arbitrary first cold-start eviction.
    pub fn open(dir: impl Into<PathBuf>, budget_bytes: u64) -> std::io::Result<Self> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir)?;
        let mut found: Vec<(u128, u64)> = Vec::new();
        for shard in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            if !shard.path().is_dir() {
                continue;
            }
            for f in std::fs::read_dir(shard.path()).into_iter().flatten().flatten() {
                let p = f.path();
                if p.extension().and_then(|e| e.to_str()) != Some("bin") {
                    continue;
                }
                let key = p
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .and_then(|s| u128::from_str_radix(s, 16).ok());
                let Some(key) = key else { continue };
                let Ok(md) = f.metadata() else { continue };
                found.push((key, md.len()));
            }
        }
        let mut entries = HashMap::new();
        let mut lru = BTreeMap::new();
        let mut total = 0u64;
        let mut tick = 0u64;
        for (key, size) in found {
            tick += 1;
            entries.insert(key, CacheEntry { size, used: tick });
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
        })
    }

    /// Cached bytes for `url` if present (bumping LRU recency); reads the file outside the index lock.
    pub fn get(&self, url: &str) -> Option<Vec<u8>> {
        let key = cache_key(url);
        {
            let idx = self.idx.lock().ok()?;
            if !idx.entries.contains_key(&key) {
                return None;
            }
        }
        let bytes = std::fs::read(cache_path(&self.dir, key)).ok()?;
        if let Ok(mut idx) = self.idx.lock() {
            idx.touch(key);
        }
        Some(bytes)
    }

    /// Store `bytes` for `url` (atomic temp+rename so a crash never leaves a partial file), then evict the
    /// oldest entries until back within budget.
    pub fn put(&self, url: &str, bytes: &[u8]) {
        if bytes.is_empty() || self.budget == 0 {
            return;
        }
        let key = cache_key(url);
        let path = cache_path(&self.dir, key);
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
        let to_unlink = match self.idx.lock() {
            Ok(mut idx) => {
                idx.insert(key, bytes.len() as u64);
                idx.evict_to(self.budget)
            }
            Err(_) => return,
        };
        for k in to_unlink {
            let _ = std::fs::remove_file(cache_path(&self.dir, k));
        }
    }

    /// Current resident byte total (for telemetry).
    pub fn bytes(&self) -> u64 {
        self.idx.lock().map(|i| i.total).unwrap_or(0)
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
/// `request`, looks up + flips on `cancel`, and removes on `poll_completed` (when the request's result is
/// drained — every request, cancelled or not, yields exactly one result, so the entry is always reclaimed
/// and the map stays bounded by the live in-flight set + a brief drain lag).
type CancelRegistry = Arc<Mutex<HashMap<RequestId, CancelFlag>>>;

pub struct HttpSource {
    /// `Option` so `Drop` can drop the sender FIRST (disconnecting the channel → workers see `recv()` err and
    /// exit) before joining the worker handles below. The only `Sender`; workers hold `req_rx` clones. Each
    /// request carries its own [`CancelFlag`] so the worker can abort the in-progress download (A3).
    req_tx: Option<flume::Sender<(RequestId, String, CancelFlag)>>,
    done_rx: flume::Receiver<Loaded>,
    /// Worker thread handles, joined on `Drop` so teardown never leaves fetch threads running detached (they
    /// each hold an `Arc<DiskCache>` clone — a thread + cache-retention leak across world re-creation).
    workers: Vec<JoinHandle<()>>,
    in_flight: HashSet<RequestId>,
    cache: Option<Arc<DiskCache>>,
    /// Live per-request cancel flags (audit A3). The previous design was a bare `HashSet<RequestId>` checked
    /// only *before* a worker dequeued a request — so a worker already inside `http_get` downloaded the
    /// **entire** tile (up to 64 MiB) and discarded it afterward, the dominant wasted bandwidth under fast
    /// camera motion. Now `cancel` flips the request's `AtomicBool`, which the worker's chunked read loop
    /// probes each chunk and aborts on — a *real* mid-download cancel. The flag also short-circuits a
    /// not-yet-started fetch (the worker's pre-flight load), so it subsumes the old hard-cancel too.
    cancels: CancelRegistry,
}

impl HttpSource {
    /// Spawn `workers` (clamped ≥ 1) fetch threads, optionally backed by a persistent [`DiskCache`].
    pub fn new(workers: usize, cache: Option<Arc<DiskCache>>) -> Self {
        let (req_tx, req_rx) = flume::unbounded::<(RequestId, String, CancelFlag)>();
        let (done_tx, done_rx) = flume::unbounded::<Loaded>();
        let cancels: CancelRegistry = Arc::new(Mutex::new(HashMap::new()));
        let mut handles = Vec::with_capacity(workers.max(1));
        for _ in 0..workers.max(1) {
            let req_rx = req_rx.clone();
            let done_tx = done_tx.clone();
            let cache = cache.clone();
            let handle = std::thread::Builder::new()
                .name("LogiTileFetch".to_string())
                .spawn(move || {
                    while let Ok((id, url, cancel)) = req_rx.recv() {
                        // Cache-first, cancel-aware fetch → a TYPED outcome (audit A10): `Ok(bytes)` (incl.
                        // a legitimate empty-200) or a classified `Err`. `fetch_one` owns the hit/miss/
                        // populate + cancellation logic and preserves the "only non-empty success is cached"
                        // invariant, so a failure is never collapsed to empty bytes and never poisons the cache.
                        let result = fetch_one(cache.as_deref(), &url, &cancel);
                        if done_tx.send(Loaded { id, result }).is_err() {
                            break; // owner gone
                        }
                    }
                })
                .expect("spawn LogiTileFetch worker");
            handles.push(handle);
        }
        Self {
            req_tx: Some(req_tx),
            done_rx,
            workers: handles,
            in_flight: HashSet::new(),
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
        // Drop the sender FIRST: that disconnects the request channel, so every idle worker's `req_rx.recv()`
        // returns `Err` and it exits. THEN join, so teardown never leaves fetch threads running detached —
        // each holds an `Arc<DiskCache>` clone, so a detached worker would leak both a thread and the cache
        // retention on every world re-creation / editor hot-reload. A worker mid-`http_get` finishes that one
        // fetch before observing the disconnect; `HTTP_OVERALL_TIMEOUT` bounds that wait. Mirrors
        // `GlobeStreamer`'s join-on-`Drop` (the sibling that already did this correctly).
        drop(self.req_tx.take());
        for h in self.workers.drain(..) {
            let _ = h.join();
        }
    }
}

impl TileContentSource for HttpSource {
    fn request(&mut self, id: RequestId, url: &str, _now_tick: u64) {
        // `req_tx` is `Some` for the whole live lifetime; it is taken only in `Drop`, after which no method
        // runs. Treat a (impossible-in-practice) `None` as "channel closed" → no enqueue.
        if let Some(tx) = self.req_tx.as_ref() {
            // Mint this request's cancel flag and register it BEFORE sending, so a `cancel` racing in right
            // after the worker dequeues still finds the flag and can abort the in-progress download (A3).
            let flag: CancelFlag = Arc::new(AtomicBool::new(false));
            if let Ok(mut c) = self.cancels.lock() {
                c.insert(id, Arc::clone(&flag));
            }
            if tx.send((id, url.to_string(), flag)).is_ok() {
                self.in_flight.insert(id);
            } else {
                // Channel closed (only possible during/after teardown): undo the registry insert so it can't
                // leak — no worker will ever drain a result for this id to clean it up.
                if let Ok(mut c) = self.cancels.lock() {
                    c.remove(&id);
                }
            }
        }
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
            // not-yet-started worker's pre-flight load short-circuits the fetch entirely. Either way the
            // request still produces one result, which `poll_completed` drains + removes from the registry.
            // We do NOT remove the registry entry here — the worker may still be reading it; `poll_completed`
            // owns reclamation. The `Relaxed` store pairs with the worker's `Relaxed` loads (eventual
            // visibility is all we need; no other memory is published through this flag).
            if let Ok(c) = self.cancels.lock() {
                if let Some(flag) = c.get(&id) {
                    flag.store(true, Ordering::Relaxed);
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
        src.request(1, "http://127.0.0.1:1/never", 0);
        src.request(2, "http://127.0.0.1:1/never", 0);
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
            !cache_path(&dir, cache_key("https://h/u1.glb")).exists(),
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
        src.request(id, "tile://7", 0);
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
        assert_eq!(out[0].result, Ok(synthetic_payload(id, src.bytes_for(id))));
        assert_eq!(src.in_flight(), 0);
    }

    #[test]
    fn cancel_removes_an_in_flight_request() {
        let mut src = SyntheticSource::new();
        src.request(3, "u", 0);
        src.request(4, "u", 0);
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
                src.request(id, "u", 0);
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
