//! The tile-content **fetch contract** for the actorless 3D-Tiles stack (ADR-0009, Milestone F): an
//! async-fetch-then-poll source the streaming loop drives on its single owner thread, generalising the
//! kernel's existing `Provider`. The selection traversal decides *which* tiles to load; a
//! [`TileContentSource`] fetches their content bytes and hands them back when ready.
//!
//! v1 ships only the deterministic [`SyntheticSource`] (latency + payload derived from the request id —
//! reproducible, no network, the test/offline lane). The production source (HTTP via `ureq` + gzip + an
//! on-disk cache, fetched on a worker pool and drained here) is the network-phase follow-up and slots in
//! behind the same trait.

#![forbid(unsafe_code)]

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Identifies an in-flight request (the caller's tile id / URL hash).
pub type RequestId = u64;

/// A completed fetch: the request id + the raw content bytes (already gzip-inflated by the source).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Loaded {
    pub id: RequestId,
    pub bytes: Vec<u8>,
}

/// An async tile-content source driven on the owner thread: `request` enqueues, `poll_completed` drains
/// finished fetches, `cancel` drops one. Implementations do the actual IO/decoding off-thread; nothing
/// here blocks the kernel. `now_tick` is the kernel's monotonic step counter.
pub trait TileContentSource {
    fn request(&mut self, id: RequestId, url: &str, now_tick: u64);
    fn poll_completed(&mut self, now_tick: u64, out: &mut Vec<Loaded>);
    fn cancel(&mut self, id: RequestId);
    fn in_flight(&self) -> usize;
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
        Self { pending: HashMap::new(), min_latency: 1, latency_range: 10, min_bytes: 256, bytes_range: 4096 }
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
        let mut done: Vec<RequestId> =
            self.pending.iter().filter(|(_, (ready, _))| *ready <= now_tick).map(|(id, _)| *id).collect();
        // Deterministic completion order: `HashMap` iteration order is randomized per run, which would
        // make a budgeted/order-sensitive consumer non-reproducible even though every payload is a pure
        // function of its id. Sort by id so a replay is byte- AND order-identical.
        done.sort_unstable();
        for id in done {
            let (_, len) = self.pending.remove(&id).unwrap_or((0, 0));
            out.push(Loaded { id, bytes: synthetic_payload(id, len) });
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

/// A fetch / URL-resolution error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FetchError {
    Url(String),
    Http(String),
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
        let carry: Vec<(String, String)> =
            base_url.query_pairs().into_owned().filter(|(k, _)| !have.contains(k)).collect();
        if !carry.is_empty() {
            let mut q = joined.query_pairs_mut();
            for (k, v) in &carry {
                q.append_pair(k, v);
            }
        }
    }
    Ok(joined.to_string())
}

/// Blocking HTTP GET → raw bytes. Run off the kernel's owner thread (on a worker pool).
pub fn http_get(url: &str) -> Result<Vec<u8>, FetchError> {
    let resp = ureq::get(url).call().map_err(|e| FetchError::Http(e.to_string()))?;
    let mut bytes = Vec::new();
    resp.into_reader().read_to_end(&mut bytes).map_err(|e| FetchError::Http(e.to_string()))?;
    Ok(bytes)
}

// ----------------------------------------------------------------------------------------------------
// Persistent on-disk tile cache. A byte-budgeted LRU over fetched tile bytes, keyed by the URL PATH
// (query stripped) so a provider's per-session auth (Google P3DT's `?session=…&key=…`) doesn't fragment
// the cache and the same tile replays across sessions. Survives restarts (the files ARE the cache; the
// index is rebuilt by scanning the dir, seeding LRU order from file mtime). Thread-safe for the worker
// pool. NOTE: persistent caching of some providers' tiles (e.g. Google P3DT) violates their ToS — that
// policy decision belongs to the caller; this layer is provider-agnostic.

struct CacheEntry {
    size: u64,
    used: u64, // monotonic recency tick
}

struct CacheIndex {
    entries: HashMap<u64, CacheEntry>, // key → entry
    lru: BTreeMap<u64, u64>,           // recency tick → key (ordered, for O(log n) eviction)
    total: u64,
    tick: u64,
}

impl CacheIndex {
    fn touch(&mut self, key: u64) {
        self.tick += 1;
        let t = self.tick;
        if let Some(e) = self.entries.get_mut(&key) {
            self.lru.remove(&e.used);
            e.used = t;
            self.lru.insert(t, key);
        }
    }
    fn insert(&mut self, key: u64, size: u64) {
        self.tick += 1;
        let t = self.tick;
        if let Some(old) = self.entries.insert(key, CacheEntry { size, used: t }) {
            self.lru.remove(&old.used);
            self.total = self.total.saturating_sub(old.size);
        }
        self.lru.insert(t, key);
        self.total = self.total.saturating_add(size);
    }
    fn evict_to(&mut self, budget: u64, dir: &Path) {
        // The just-inserted key has the highest tick, so `pop_first` (oldest) never evicts it unless it is
        // the sole entry (budget < one tile — degenerate); LRU recency is preserved.
        while self.total > budget {
            let Some((_, key)) = self.lru.pop_first() else { break };
            if let Some(e) = self.entries.remove(&key) {
                self.total = self.total.saturating_sub(e.size);
                let _ = std::fs::remove_file(cache_path(dir, key));
            }
        }
    }
}

/// FNV-1a 64 over the URL's PATH (query/fragment stripped) — the session-independent tile identity.
fn cache_key(url: &str) -> u64 {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    let mut h: u64 = 0xCBF2_9CE4_8422_2325;
    for b in path.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01B3);
    }
    h
}

/// `<dir>/<first-2-hex>/<16-hex>.bin` — sharded so a 100k-file cache doesn't put everything in one dir.
fn cache_path(dir: &Path, key: u64) -> PathBuf {
    let name = format!("{key:016x}");
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
    /// Open (creating) a cache at `dir` bounded to `budget_bytes`. Rebuilds its index from the existing
    /// files, seeding LRU order by mtime (so a restart keeps roughly the right eviction order).
    pub fn open(dir: impl Into<PathBuf>, budget_bytes: u64) -> std::io::Result<Self> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir)?;
        let mut found: Vec<(u64, u64, std::time::SystemTime)> = Vec::new();
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
                    .and_then(|s| u64::from_str_radix(s, 16).ok());
                let Some(key) = key else { continue };
                let Ok(md) = f.metadata() else { continue };
                found.push((key, md.len(), md.modified().unwrap_or(std::time::UNIX_EPOCH)));
            }
        }
        found.sort_by_key(|(_, _, m)| *m); // oldest first → lowest recency tick
        let mut entries = HashMap::new();
        let mut lru = BTreeMap::new();
        let mut total = 0u64;
        let mut tick = 0u64;
        for (key, size, _) in found {
            tick += 1;
            entries.insert(key, CacheEntry { size, used: tick });
            lru.insert(tick, key);
            total = total.saturating_add(size);
        }
        Ok(Self { dir, budget: budget_bytes, idx: Mutex::new(CacheIndex { entries, lru, total, tick }) })
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
        let tmp = path.with_extension("tmp");
        if std::fs::write(&tmp, bytes).is_err() {
            return;
        }
        if std::fs::rename(&tmp, &path).is_err() {
            let _ = std::fs::remove_file(&tmp);
            return;
        }
        if let Ok(mut idx) = self.idx.lock() {
            idx.insert(key, bytes.len() as u64);
            idx.evict_to(self.budget, &self.dir);
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
pub struct HttpSource {
    req_tx: crossbeam_channel::Sender<(RequestId, String)>,
    done_rx: crossbeam_channel::Receiver<Loaded>,
    in_flight: HashSet<RequestId>,
    cache: Option<Arc<DiskCache>>,
}

impl HttpSource {
    /// Spawn `workers` (clamped ≥ 1) fetch threads, optionally backed by a persistent [`DiskCache`].
    pub fn new(workers: usize, cache: Option<Arc<DiskCache>>) -> Self {
        let (req_tx, req_rx) = crossbeam_channel::unbounded::<(RequestId, String)>();
        let (done_tx, done_rx) = crossbeam_channel::unbounded::<Loaded>();
        for _ in 0..workers.max(1) {
            let req_rx = req_rx.clone();
            let done_tx = done_tx.clone();
            let cache = cache.clone();
            std::thread::spawn(move || {
                while let Ok((id, url)) = req_rx.recv() {
                    // Cache-first: serve a disk hit, else fetch + populate. A failed fetch is empty bytes
                    // (the owner treats empty as a failed load — no panic, no retry storm); only a
                    // non-empty result is cached, so a 4xx/expired-session never poisons the cache.
                    let bytes = match &cache {
                        Some(c) => match c.get(&url) {
                            Some(b) => b,
                            None => {
                                let b = http_get(&url).unwrap_or_default();
                                if !b.is_empty() {
                                    c.put(&url, &b);
                                }
                                b
                            }
                        },
                        None => http_get(&url).unwrap_or_default(),
                    };
                    if done_tx.send(Loaded { id, bytes }).is_err() {
                        break; // owner gone
                    }
                }
            });
        }
        Self { req_tx, done_rx, in_flight: HashSet::new(), cache }
    }

    /// Bytes currently resident in the disk cache (0 if no cache).
    pub fn cached_bytes(&self) -> u64 {
        self.cache.as_ref().map(|c| c.bytes()).unwrap_or(0)
    }
}

impl TileContentSource for HttpSource {
    fn request(&mut self, id: RequestId, url: &str, _now_tick: u64) {
        if self.req_tx.send((id, url.to_string())).is_ok() {
            self.in_flight.insert(id);
        }
    }
    fn poll_completed(&mut self, _now_tick: u64, out: &mut Vec<Loaded>) {
        while let Ok(loaded) = self.done_rx.try_recv() {
            // Drop results for cancelled requests (no longer in-flight).
            if self.in_flight.remove(&loaded.id) {
                out.push(loaded);
            }
        }
    }
    fn cancel(&mut self, id: RequestId) {
        self.in_flight.remove(&id); // the in-progress fetch still runs; its result is dropped on poll.
    }
    fn in_flight(&self) -> usize {
        self.in_flight.len()
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
        let u =
            resolve_url("https://assets.cesium.com/1/tileset.json?access_token=T", "0/0.b3dm").unwrap();
        assert_eq!(u, "https://assets.cesium.com/1/0/0.b3dm?access_token=T");
        // a child URI that already has the param isn't duplicated.
        let u = resolve_url("https://h/r.json?key=A", "/c.glb?key=B").unwrap();
        assert_eq!(u, "https://h/c.glb?key=B");
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
        assert_eq!(out[0].bytes, synthetic_payload(id, src.bytes_for(id)));
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
        assert!(rel.contains("access_token=SECRET"), "relative child must keep the token");

        // Same-host absolute child → same origin → token carried.
        let same = resolve_url(base, "https://assets.cesium.com/other/x.glb").unwrap();
        assert!(same.contains("access_token=SECRET"));

        // Cross-origin absolute child → token MUST NOT leak (the exfiltration guard).
        let evil = resolve_url(base, "https://evil.example/x.glb").unwrap();
        assert!(!evil.contains("SECRET"), "token must NOT be sent cross-origin: {evil}");
        assert!(!evil.contains("access_token"), "no auth param cross-origin: {evil}");

        // Different port is a different origin too.
        let other_port = resolve_url(base, "https://assets.cesium.com:8443/x.glb").unwrap();
        assert!(!other_port.contains("SECRET"), "different port is cross-origin: {other_port}");
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
