//! The Geo **ECS orchestration layer** (ADR-0012 Addendum A; design = the plan §2A; constraints =
//! `Docs/Research/ECS-Multithreading-for-Geo.md`; the working-set reduction = ADR-0018). Terrain tiles are
//! `bevy_ecs` **entities**; the Geo stream thread runs this `Schedule`, and every per-step pass is bounded by
//! the **active working-set** (resident + in-flight + just-fetched), never the pre-spawned pool. This is the
//! full orchestration layer: the entity/component model,
//! the pre-spawned tile pool, the right-sized systems, AND the streaming pipeline
//! ([`from_tileset`](GeoWorld::from_tileset) / [`from_ion`](GeoWorld::from_ion) →
//! [`advance_streaming`](GeoWorld::advance_streaming) → [`latest_frame`](GeoWorld::latest_frame)) that the
//! [`GlobeStreamer`](crate::stream_thread::GlobeStreamer) drives. The per-tile decode is injected
//! ([`production_decoder`] in production, a stub in tests), so the layer stays headless-testable.
//!
//! ## The hard constraints this encodes (from the research)
//! - **No structural changes in the hot loop.** Spawn/despawn/insert/remove are `ApplyDeferred` barriers
//!   that serialize the schedule, so tiles come from a **pre-spawned pool + free-list** and are born/evicted
//!   by **resetting component *fields***, never by `spawn`/`despawn` or add/remove markers. Every tile shares
//!   one archetype → dense, fragmentation-free iteration.
//! - **Small, Table-stored components; heavy geometry behind `Arc`.** The hot [`sse_cull_lru`] pass touches
//!   only small fields; the multi-KB mesh is reached only via the [`ResidentMesh`] `Arc` handle.
//! - **Working-set passes, not pool scans (ADR-0018).** Every per-step system is driven off a maintained
//!   active set — [`sse_cull_lru`] over [`LiveSet`] (all non-`Free` tiles), [`decode`] over [`JustFetched`]
//!   (this step's arrivals), [`evict`]/[`publish`] over [`ResidentSet`] — so per-step work is `O(active + Δ)`,
//!   never `O(pool)`. A settled scene with no arrivals does ~no work.
//! - **The schedule runs SingleThreaded.** It is fully `.chain()`ed (no concurrent systems), so bevy's
//!   multi-threaded executor would be pure per-step overhead (ADR-0018 / round-4 Exp C). Geo parallelism is
//!   intra-pass data parallelism and the measure-gated decode decouple, never the system DAG.

use crate::frustum::ViewFrustum;
use crate::streaming::{is_tileset_uri, sniff_content_kind, tile_at_mut, ContentKind, StreamError, TileGeometry};
use bevy_ecs::prelude::*;
use bevy_ecs::schedule::SingleThreadedExecutor;
use bevy_tasks::{ComputeTaskPool, TaskPoolBuilder};
use glam::{DMat4, DVec3};
use hashbrown::{HashMap, HashSet};
use logi_3dtiles_implicit::{parse_subtree, Subtree};
use logi_3dtiles_io::{http_get, resolve_url, DiskCache, FetchError, HttpSource, RequestId, TileContentSource};
use logi_3dtiles_select::{
    select_tree_implicit, AvailabilityProvider, ImplicitContext, PathArena, SelectParams, SelectedTile,
    SelectionResult, SelectionState, TileAvailability, Visibility, MAX_TILE_DEPTH, NO_NODE,
};
use logi_3dtiles_tileset::{parse_tileset, Content, Refine, Tile};
use logi_geo_mesh::ue_cm_to_enu;
use logi_geospatial::{Cartographic, EnuFrame};
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Which tile-fetch backend [`GeoWorld::from_ion`] constructs — the ADR-0014 I/O-lane selection, switchable
/// at world-create time via the FFI `fetch_backend` knob (`LogiGlobeCreateInfo`, ABI v11). Named by the
/// underlying client library (the wire protocol is negotiated), not by protocol.
///
/// * [`Auto`](Self::Auto) — the default policy: pick the [`Cyper`](Self::Cyper) lane for Google P3DT (when the
///   `cyper-h2` feature is compiled), and the blocking `ureq` [`HttpSource`] otherwise / for other providers.
/// * [`Ureq`](Self::Ureq) / [`Cyper`](Self::Cyper) — force a specific backend (the live A/B knob).
///
/// Selecting [`Cyper`](Self::Cyper) (directly or via `Auto`) in a build that did NOT compile `cyper-h2`, or
/// when the lane's runtime thread fails to start, degrades gracefully to [`HttpSource`] — world creation
/// never fails on a non-fatal backend hiccup.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum FetchBackend {
    /// The `Cyper` lane for Google P3DT (when compiled), `ureq` otherwise. The zero value, so a zero-initialised
    /// FFI `LogiGlobeCreateInfo` gets this (the new default).
    #[default]
    Auto,
    /// Force the blocking `ureq` [`HttpSource`] worker pool (HTTP/1.1; the A/B's h1 arm; other providers).
    Ureq,
    /// Force the [`CyperSource`](logi_3dtiles_io::CyperSource) lane — **HTTP/3 over QUIC** (compio-quic /
    /// quinn-proto on the compio reactor) via Alt-Svc auto-upgrade, with HTTP/2 as the bootstrap + fallback.
    Cyper,
}

impl FetchBackend {
    /// Map the FFI `fetch_backend` u32 knob → policy: `0` (and any unknown value) ⇒ [`Auto`](Self::Auto),
    /// `1` ⇒ [`Ureq`](Self::Ureq), `2` ⇒ [`Cyper`](Self::Cyper). Mirrors `ELogiFetchBackend` in the C++
    /// terrain actor and the `LOGI_GLOBE_FETCH_BACKEND_*` constants in `logi_geo_ffi.h`.
    pub fn from_ffi(v: u32) -> Self {
        match v {
            1 => FetchBackend::Ureq,
            2 => FetchBackend::Cyper,
            _ => FetchBackend::Auto,
        }
    }
}

/// A tile's child-index path from the root — the stable identity keying the tree ↔ entity map.
type TilePath = Vec<u32>;

/// One placeholder restore queued by [`GeoWorld::prune_cold_grafts`]: `(path, transform, content, contents,
/// children, geometric_error, refine)` — the [`GraftRecord`] fields re-applied to un-graft a cold subtree's
/// placeholder tile.
type GraftRestore = (
    TilePath,
    Option<[f64; 16]>,
    Option<Content>,
    Vec<Content>,
    Vec<Tile>,
    f64,
    Option<Refine>,
);

// ---- Components (small, Table storage = default; heavy data behind `Arc`) --------------------------

/// Interned tile identity (a hash of the tileset path) — a cache-friendly key. The full `Vec<u32>` path
/// lives in a side map, never in the hot row.
#[derive(Component, Clone, Copy, PartialEq, Eq, Debug)]
pub struct TileId(pub u64);

/// The tile's interned [`PathArena`] node id — the reverse of `GeoStreaming::index` (node → entity), carried
/// on the entity so the in-schedule systems (`evict`/`decode`), which cannot reach `streaming`, can name the
/// tile they touched for the O(Δ) [`reconcile_index`](GeoWorld::reconcile_index) (audit 2026-06-10 §2).
/// [`NO_NODE`] for a slot never tasked by the streaming traversal (the test-only `acquire_tile` path).
#[derive(Component, Clone, Copy, PartialEq, Eq, Debug)]
pub struct TileNode(pub u32);

/// Bounding sphere in ECEF (metres) — drives SSE + culling.
#[derive(Component, Clone, Copy, Debug)]
pub struct TileBounds {
    pub center: DVec3,
    pub radius: f64,
}

/// 3D Tiles geometric error (world units) — the SSE refine input.
#[derive(Component, Clone, Copy, Debug)]
pub struct GeometricError(pub f64);

/// Refinement tree depth (root = 0) — the tile's `path.len()`. Carried as a tiny POD component so [`publish`]
/// can mirror it across the FFI for the Geo debug-visualisation / telemetry tools; not used by streaming.
#[derive(Component, Clone, Copy, Debug)]
pub struct Depth(pub u32);

/// The composed tile→ECEF transform (the rebase input for [`decode`]).
#[derive(Component, Clone, Copy, Debug)]
pub struct TileTransform(pub DMat4);

/// The tile's place in the pipeline. **A field, never an add/removed marker component** — flipping it is a
/// cheap write with no archetype move (research §2/§7).
#[derive(Component, Clone, Copy, PartialEq, Eq, Debug)]
pub enum TileState {
    /// Pool slot, unused — available on the free-list.
    Free,
    /// Selected by the traversal this step (wants content).
    Selected,
    /// A fetch has been requested for it.
    Requested,
    /// Raw bytes have arrived ([`FetchedBytes`] set) — awaiting decode.
    Fetched,
    /// Decoded geometry is resident ([`ResidentMesh`] set) — renderable.
    Resident,
}

/// Frame index this tile was last in the working set — drives LRU eviction.
#[derive(Component, Clone, Copy, Debug)]
pub struct LastSeen(pub u64);

/// Raw tile bytes between fetch and decode — a **handle** (`Arc`), never inline in the hot row.
#[derive(Component, Default)]
pub struct FetchedBytes(pub Option<Arc<[u8]>>);

/// The decoded mesh — an `Arc` handle (the Phase-0 retain). The bytes never sit in the hot iteration row.
/// For a multiple-contents tile (3D Tiles 1.1) this holds content **[0]**; the rest live in [`MultiContent`].
#[derive(Component, Default)]
pub struct ResidentMesh(pub Option<Arc<TileGeometry>>);

/// 3D Tiles 1.1 **multiple contents** (`tile.contents: [a, b, …]`): a tile may carry more than one renderable
/// content, and a conformant client renders them ALL. Content `[0]` decodes into [`ResidentMesh`] on the normal
/// `decode` path — so the single-content case (every Google P3DT tile, and the overwhelming majority of
/// tilesets) is **byte-identical** to before and this vec stays EMPTY, leaving the hot path untouched. Each
/// ADDITIONAL content (`[1..]`) rides this same entity: it is fetched with a `content_index` tag, inline-decoded
/// when it lands (the rare path — off the `decode` system), and parked here. [`publish`] emits one render tile
/// per [`ResidentMesh`] plus one per filled [`ExtraContent`], so all of a tile's contents reach the renderer.
#[derive(Component, Default)]
pub struct MultiContent(pub Vec<ExtraContent>);

/// One additional content of a multiple-contents tile: a distinct, stable render key (so the C++ section cache
/// treats it as its own sub-mesh) and the decoded mesh once its fetch lands — `None` while the fetch is
/// outstanding or after it failed/decoded-empty (that content is simply not drawn; the tile still renders).
pub struct ExtraContent {
    pub key: u64,
    pub mesh: Option<Arc<TileGeometry>>,
}

// ---- Resources -------------------------------------------------------------------------------------

/// The latest view the stream thread advances against (newest-wins), plus the monotonic step frame.
#[derive(Resource, Clone, Copy, Debug, Default)]
pub struct GeoView {
    pub camera_ecef: DVec3,
    pub fov_y_radians: f64,
    pub viewport_height: f64,
    pub max_sse: f64,
    /// Distance cull radius (metres) — a tile farther than this is "out of the working set": its `LastSeen`
    /// is NOT stamped, so it becomes LRU-evictable. `0` (or negative) = no cull (everything is in view).
    pub cull_distance_m: f64,
    pub frame: u64,
}

/// The per-tile decode function: raw bytes + the tile's world transform → rebased geometry (`None` on a
/// decode failure — it never panics). An `Arc<dyn Fn>` + `Send + Sync` so one impl is shared by [`decode`]
/// (and, under the measure-gated ADR-0018 decode decouple, every fan-out worker). Production wires
/// `logi_gltf::decode_glb` + the ENU rebase; tests wire a stub.
pub type TileDecodeFn = Arc<dyn Fn(&[u8], DMat4) -> Option<TileGeometry> + Send + Sync>;

/// The pluggable per-tile decode, as a resource the [`decode`] system reads.
#[derive(Resource, Clone)]
pub struct TileDecoder(pub TileDecodeFn);

/// The **production** [`TileDecoder`]: real glTF decode (`logi_gltf::decode_glb`) + the ENU rebase
/// ([`crate::streaming::rebase_gltf`]), sharing one `EnuFrame` across every [`decode`] call. A
/// decode/parse/rebase failure yields `None` — it never panics across the boundary. (Tests use a stub
/// instead of crafting a GLB; the rebase math is covered by `streaming`'s fold tests.)
pub fn production_decoder(enu: EnuFrame) -> TileDecoder {
    TileDecoder(Arc::new(move |bytes, world| {
        logi_gltf::decode_glb(bytes)
            .ok()
            .and_then(|g| crate::streaming::rebase_gltf(&enu, g, world))
    }))
}

/// Resident-tile cap (off-screen slack); eviction keeps the world under this.
#[derive(Resource, Clone, Copy, Debug)]
pub struct MaxResident(pub usize);

/// Free pool slots (entities in [`TileState::Free`]) available to back a newly-selected tile **without a
/// spawn**. Acquire = pop; release = push (on eviction).
#[derive(Resource, Default)]
pub struct FreeList(pub Vec<Entity>);

/// The set of entities currently `Resident` (audit Stage 1): lets `evict`/`publish` operate over the resident
/// working set (O(resident) / O(visible)) instead of scanning the whole pre-spawned pool (O(pool)) every step.
/// Maintained at every `Resident` transition — `decode` (now sequential, so it inserts directly), every
/// `place_decoded` caller (`restore_cached`, `try_restore_from_geom`), `evict`, `prune_cold_grafts` — and
/// drift-checked against a full scan in debug builds (`evict`), so a missed transition fails tests rather than
/// silently dropping renders in release. `ResidentSet` ⊆ [`LiveSet`].
#[derive(Resource, Default)]
struct ResidentSet(HashSet<Entity>);

/// The **active working-set** (ADR-0018): every NON-`Free` tile entity (`Selected`/`Requested`/`Fetched`/
/// `Resident`). Lets [`sse_cull_lru`] stamp the LRU over the live tiles (O(active)) instead of `par_iter`-
/// scanning the whole pre-spawned pool (O(pool)) every step. Maintained O(1) at every `Free`↔non-`Free`
/// transition: INSERT on acquire (`request_one`, `place_decoded`, `acquire_tile`), REMOVE on release
/// (`free_slot`, `evict`) — and drift-checked against a full pool scan in debug (`sse_cull_lru`), so a missed
/// transition fails a test rather than silently mis-stamping the LRU in release.
#[derive(Resource, Default)]
struct LiveSet(HashSet<Entity>);

/// Entities that transitioned `Requested`→`Fetched` THIS step — their GLB bytes just landed (ADR-0018).
/// [`drain_raw`] appends as it delivers bytes; the sequential [`decode`] drains this and decodes each via
/// random-access `tiles.get_mut(e)` → **O(Δ_fetched)**, replacing the `par_iter`-over-pool +
/// `Changed<FetchedBytes>` scan that spawned ~one task per pooled row every step. Empty on a no-arrival step
/// ⇒ `decode` does nothing (the common settled-scene case).
#[derive(Resource, Default)]
struct JustFetched(Vec<Entity>);

/// The entities the in-schedule systems handed back for index reclamation THIS step — `evict`'s freed slots
/// and `decode`'s failures (the only two producers of a stale `index` entry; every other reclaim path removes
/// its entry inline). [`reconcile_index`](GeoWorld::reconcile_index) drains both lists and reclaims exactly
/// these entries via their [`TileNode`] — **O(Δ)**, replacing the old whole-`index` scan that any
/// eviction/failure step paid (audit 2026-06-10 §2; the previous `DecodeFailedThisStep` flag + settled-step
/// skip only spared the no-churn case).
#[derive(Resource, Default)]
struct Reclaim {
    evicted: Vec<Entity>,
    decode_failed: Vec<Entity>,
}

/// Default decoded-mesh LRU budget (bytes). Sized as a re-entry buffer in front of the resident set: large
/// enough that a camera reversing direction restores meshes instead of re-decoding, small enough to bound
/// RAM. Tune on a flamegraph / RAM headroom (plan §6, Phase 1); override per-world with
/// [`GeoWorld::set_flash_cache_budget`].
pub const DEFAULT_FLASH_CACHE_BYTES: usize = 384 * 1024 * 1024;

/// Pre-spawned tile-entity pool size — the hard ceiling on live tiles, allocated once at world creation (no
/// spawn/despawn in the hot loop, ADR-0018). Must exceed [`DEFAULT_MAX_RESIDENT`] + the in-flight fetches +
/// decode-failed slack, so the free-list never exhausts in steady state. Raised from 10240: at the finer 6 px
/// Detail Target the in-view working set is ~1.8× the 8 px set (which already sat ~9.8k — 95% of the old pool,
/// the wall that silently dropped tiles into coarse holes). Verify the live `resident` lands comfortably below
/// [`DEFAULT_MAX_RESIDENT`]. (Was a bare `24576` literal in the FFI create call.)
pub const DEFAULT_TILE_POOL_SIZE: usize = 24576;

/// Resident-tile cap (the LRU evict target). Raised with the finer 6 px Detail Target so the deeper LOD's larger
/// working set fits without the LRU evicting still-wanted near tiles. In-view tiles are stamped every frame
/// (never evictable), so this mainly bounds the OFF-view coarse shell; [`DEFAULT_TILE_POOL_SIZE`] is the ceiling
/// above it. Invariant: `DEFAULT_TILE_POOL_SIZE > DEFAULT_MAX_RESIDENT` (asserted at world creation).
/// (Was a bare `18432` literal in the FFI create call.)
pub const DEFAULT_MAX_RESIDENT: usize = 18432;
const _: () = assert!(
    DEFAULT_TILE_POOL_SIZE > DEFAULT_MAX_RESIDENT,
    "the tile pool must exceed the resident cap (+ in-flight + decode-failed slack)"
);

/// Consecutive failed fetches a tile path is retried before it's given up. A network fetch (unlike decode)
/// can fail transiently — a dropped connection, a 5xx, an expired session — so a single failure must NOT be
/// terminal (that was the bug: a failed fetch stranded the tile as `Requested` forever AND leaked its pool
/// slot, so its region showed a coarse ancestor — "low quality" — permanently). Each failure frees the slot
/// and re-selects the path; only after this many consecutive misses is it abandoned, so a genuinely dead URL
/// (a persistent 404 / removed tile) can't storm the network or leak slots without bound. A success resets
/// the count. ~4 rides out a brief outage while still converging quickly on a truly dead tile.
const MAX_FETCH_ATTEMPTS: u32 = 4;

/// Outstanding fetches per worker — the owner's in-flight cap is `source.fetch_concurrency() · this` (audit H2),
/// so the cap tracks the worker count (the knob that actually governs download parallelism) rather than a
/// hardcoded 128. A few per worker keeps each worker's pipeline full (one downloading + a couple queued) without
/// a deep FIFO backlog that would bury high-priority tiles behind low-priority ones. (Past the priority-heap
/// rework — audit H1 — a deeper queue stays priority-ordered and this can rise; until then keep it modest so the
/// shallow FIFO ≈ the fetch-priority order.)
const IN_FLIGHT_PER_WORKER: usize = 4;

/// Fetch priority for `.subtree` files (audit M3): they GATE refinement, so a freed worker fetches a pending
/// subtree before any tile content. Far above typical tile `fetch_priority` scores (importance × boost, ≪ this)
/// so subtrees lead, but finite — subtrees are few; recording each one's requesting-tile distance for a finer
/// rank is a future refinement.
const SUBTREE_FETCH_PRIORITY: f64 = 1.0e6;

/// Priority for the M1b ASYNC session-refresh root re-fetch — ABOVE subtrees (it mints the fresh `?session=…`
/// that unblocks the ENTIRE tree, so it must pop the worker heap before any subtree or tile). One at a time
/// (gated by `session_refresh_in_flight`), so it can't crowd the pipeline.
const SESSION_REFRESH_PRIORITY: f64 = 1.0e7;

/// Priority DEMOTION for speculative M4 prefetch fetches: a prefetch tile's `fetch_priority` MINUS this stays
/// below every reactive tile's (positive) score, so the priority-heap worker pool always serves what the camera
/// needs NOW before a lookahead guess — while still ordering prefetches among themselves (the offset cancels in
/// the comparison). Cancellation/preemption of a prefetch by a reactive tile is then automatic in the heap.
const PREFETCH_PRIORITY_DEMOTE: f64 = 1.0e9;

/// Preemption margin (audit H6): a newcomer reactive tile must out-rank the lowest-priority IN-FLIGHT fetch by
/// THIS factor to cancel it at the cap. So a marginally-higher tile can't thrash an already-progressing
/// download, while a clearly-more-urgent one — or any reactive tile over a large-negative prefetch score —
/// wins. (For a negative in-flight score the threshold goes more-negative, i.e. always preemptible — exactly
/// the intent: reactive work evicts a speculative prefetch on sight.)
const PREEMPT_PRIORITY_MARGIN: f64 = 1.5;

/// Bounded caches over a long planet fly-through. The implicit-tiling bookkeeping (`subtree_cache`,
/// `subtree_failed`) and the explicit-tree failure sets (`decode_failed`, `fetch_attempts`) otherwise grow
/// **monotonically with explored extent** — none of it is covered by the cold-graft prune (which keys on
/// grafted-subtree paths, not implicit URLs / root-tree paths). Entries are tiny (URLs/paths), so these caps
/// bound RSS, not a hot path. `subtree_cache` is FIFO-evicted oldest-first (so the current view's recently
/// fetched subtrees survive); the failure "skip-hint" sets are cleared wholesale when over cap — self-healing,
/// since a still-bad path simply re-fails and re-adds, paced by the in-flight fetch cap.
const MAX_SUBTREE_CACHE: usize = 8192;
const MAX_SUBTREE_FAILED: usize = 8192;
const MAX_DEAD_PATHS: usize = 16384;

/// Tileset-level extensions this stack honors. The 3D Tiles spec requires a client to REFUSE to render a
/// tileset listing an `extensionsRequired` entry it does not support (rather than silently dropping content /
/// showing holes). `from_tileset_with_decoder` checks the required set against this allowlist. (glTF-level
/// extensions — `KHR_draco_mesh_compression`, `KHR_materials_unlit` — are a separate per-tile concern handled
/// in `logi_gltf`, not here.)
const SUPPORTED_TILESET_EXTENSIONS: &[&str] = &[
    "3DTILES_implicit_tiling", // implicit subtrees — fully supported (logi_3dtiles_implicit)
    "3DTILES_content_gltf",    // glTF tile content — the only content type we decode
];

/// Backoff for a TRANSIENT fetch failure (audit A1). The stream loop advances
/// at most ~once/ms (`SHORT_PARK`), so these are ~tens-to-hundreds of ms — enough to stop the sub-millisecond
/// retry STORM the old code caused (it dropped a failed path and re-selected it the very next iteration with
/// no delay, so a wave of 429s would hammer an already-overloaded server). Doubles per attempt; a per-path
/// jitter de-synchronizes a herd of tiles that all fail together. WALL-CLOCK [`Duration`] (audit H5) — the Geo
/// lane is non-authoritative (ADR-0007), so retry timing is REAL time, not step ticks: a backoff is the same
/// milliseconds whether the loop is busy-stepping (1 ms park) or idling (8 ms park) between retries (the
/// frame-tick version drifted ~8× when in-flight drained to 0 and the loop idled, the cadence the backoff runs at).
const RETRY_BACKOFF_BASE: Duration = Duration::from_millis(24);
/// Cap on the exponential-backoff shift: the per-attempt delay is `RETRY_BACKOFF_BASE · 2^min(attempt-1, this)`,
/// so it tops out at `24 ms · 2^6 ≈ 1.5 s` instead of growing unbounded with the attempt count.
const RETRY_MAX_SHIFT: u32 = 6;

/// Cap on a `Retry-After`-derived wait so a hostile/huge header value can't pin a path out of streaming for an
/// absurd time (it still retries eventually, bounded by [`MAX_FETCH_ATTEMPTS`]).
const RETRY_AFTER_MAX: Duration = Duration::from_secs(60);

/// Minimum wall-clock between Google P3DT **session refreshes**. A P3DT session (the `?session=…` the root
/// tileset mints) has a TTL; once it expires EVERY new tile/subtree fetch returns 401/403 at once. One refresh
/// (re-fetch the root → fresh session for the whole tree) fixes the wave, so this cooldown collapses the 401
/// storm into a single root re-fetch instead of one per failed tile.
const SESSION_REFRESH_COOLDOWN: Duration = Duration::from_secs(2);

/// How often (in advance steps) [`GeoWorld::prune_cold_grafts`] scans for cold grafted subtrees. The scan is
/// `O(grafts)`; gating it to every Nth step keeps that off the per-step hot path while still bounding tree
/// growth promptly. Real pruning work runs only when something is actually cold. Tunable.
const GRAFT_PRUNE_INTERVAL: u64 = 120;

/// Advance steps a grafted external subtree may go with **no rendered or loading descendant** before it is
/// un-grafted. Without this, the tile tree + `grafted` + `bases` grow monotonically with the explored extent
/// over a streaming session — nothing else ever removes a graft, so a planet fly-through leaks the whole
/// visited tree skeleton (mesh data is already evicted; this is the structural retention it left behind). A
/// re-visit re-fetches + re-grafts the subtree (cheap — usually a disk-cache hit), so this is a pure
/// memory↔re-fetch trade. Sized well above normal LOD churn so dwelling near a boundary never thrashes it;
/// tune against RAM headroom on a long session.
const GRAFT_COLD_FRAMES: u64 = 1800;

/// The pre-graft state of a placeholder tile plus its last-active step, retained per grafted subtree so it
/// can be (a) detected as cold and (b) **un-grafted** back to a re-fetchable placeholder. [`graft_external`]
/// saves exactly the six `Tile` fields it overwrites (transform, content, contents, children, geometricError,
/// refine); [`GeoWorld::prune_cold_grafts`] restores them, which
/// drops the grafted subtree's `Tile` nodes and lets the path re-select → re-fetch → re-graft on return.
struct GraftRecord {
    saved_transform: Option<[f64; 16]>,
    saved_content: Option<Content>,
    saved_contents: Vec<Content>,
    saved_children: Vec<Tile>,
    /// The placeholder's own geometricError + refine, saved because the graft now ADOPTS the external root's
    /// (3D Tiles 1.1: the external tileset's root carries its OWN geometricError + refine; the placeholder only
    /// points at it). Restored on un-graft. (boundingVolume is intentionally NOT adopted — it's memoized, and a
    /// stub's BV conservatively encloses the external root's, so keeping it never under-culls.)
    saved_geometric_error: f64,
    saved_refine: Option<Refine>,
    /// Most recent advance frame a tile in this subtree was rendered or queued for load (drives the cold cut).
    last_active: u64,
}

struct FlashCacheEntry {
    geom: Arc<TileGeometry>,
    bytes: usize,
    last_seen: u64,
    /// Camera distance (ECEF metres) when this tile was last deposited — the spatial half of the eviction
    /// score. A snapshot (not updated while cached), but `last_seen` staleness compensates as the camera moves.
    distance: f32,
}

/// Eviction blends **distance** (primary — a tile far from the camera is least likely to be revisited, so
/// drop it first to free room for near work) with **staleness** (`frames since last seen`, secondary — pushes
/// a long-abandoned near tile out ahead of a just-departed far one). `score = distance · (1 + FACTOR·stale)`;
/// the cache/resident evictors drop the highest score first. ~0.02 ⇒ ~50 unseen frames doubles effective range.
const EVICT_STALENESS_FACTOR: f32 = 0.02;

/// **GreedyDual-Size-Distance** size term for the BYTE-budgeted [`FlashCache::trim`] only (the resident pool's
/// `evict` is COUNT-budgeted — `MaxResident` — so tile size can't help it reach its target faster, and it stays
/// pure distance-staleness). Among similarly-distant/stale cached tiles, nudges the score so the LARGER tile is
/// dropped first: a byte budget is then reclaimed in FEWER evictions (fewer warm-restore losses + re-decodes),
/// and — since re-acquiring any tile carries a fixed per-tile cost (network RTT + decode setup) on top of the
/// per-byte cost — the smaller tiles preferentially KEPT this way are the more expensive-per-byte to refetch.
/// Factor `1 + WEIGHT·(bytes/REF)`: a GENTLE, sub-dominant nudge (distance, which varies over orders of
/// magnitude, stays primary; this only re-orders near-equally-distant tiles). `WEIGHT = 0.0` ⇒ pure
/// distance-staleness. Magnitude is a measured-elsewhere first cut — worth a hit-rate/flamegraph tune; it is
/// bounded so it can never override the distance ordering. Scores are compared only relatively (never to a
/// threshold), so the absolute scale this adds is irrelevant.
const EVICT_SIZE_WEIGHT: f32 = 0.5;
/// Reference byte size the [`EVICT_SIZE_WEIGHT`] term normalizes against — roughly a typical decoded P3DT tile
/// (a BC7-bound atlas + its mesh). Only the ratio `bytes / REF` matters.
const EVICT_SIZE_REF: f32 = 100_000.0;

/// A byte-budgeted LRU of **decoded** tile geometry, keyed by `tile_key`, that outlives slot eviction. When
/// the traversal re-selects a recently-evicted tile (camera reversing, orbiting, or jitter at the working-set
/// boundary), [`GeoWorld::select_request`] restores its mesh straight onto a pool slot — **no re-fetch and no
/// re-decode** (decode is the costly step the audit flagged for re-entry hitching). It holds an [`Arc`], so a
/// published render frame can still own a copy after the cache drops it. Eviction is **amortized**: a `trim`
/// is a no-op until total bytes cross `max_bytes`, then it evicts the **farthest + stalest** entries
/// (distance-primary score, [`EVICT_STALENESS_FACTOR`]) via an O(n)-avg `select_nth_unstable_by` partition of
/// only the overflow prefix down to a low-water mark, so the partition is paid only once per ~20% of cache turnover.
#[derive(Resource)]
pub struct FlashCache {
    entries: HashMap<u64, FlashCacheEntry>,
    bytes: usize,
    max_bytes: usize,
}

impl FlashCache {
    fn new(max_bytes: usize) -> Self {
        Self {
            entries: HashMap::new(),
            bytes: 0,
            max_bytes,
        }
    }

    #[inline]
    fn contains_key(&self, key: u64) -> bool {
        self.entries.contains_key(&key)
    }

    /// Insert/refresh a decoded mesh keyed by `tile_key` with its camera `distance` at deposit. Does **not**
    /// trim — callers batch inserts (one per evicted tile this step) then call [`trim`](Self::trim) once.
    fn insert(&mut self, key: u64, geom: Arc<TileGeometry>, last_seen: u64, distance: f32) {
        let bytes = geom.byte_size();
        let entry = FlashCacheEntry {
            geom,
            bytes,
            last_seen,
            distance,
        };
        if let Some(old) = self.entries.insert(key, entry) {
            self.bytes -= old.bytes; // replaced an existing entry — discount its bytes
        }
        self.bytes += bytes;
    }

    /// Remove + return a cached mesh: a warm restore **moves** it back onto a live slot (not a clone), so the
    /// cache stops accounting for it the moment it's resident again.
    fn take(&mut self, key: u64) -> Option<Arc<TileGeometry>> {
        let e = self.entries.remove(&key)?;
        self.bytes -= e.bytes;
        Some(e.geom)
    }

    /// Amortized batch eviction: a no-op under budget; over budget, drop the **farthest + stalest** entries
    /// ([`EVICT_STALENESS_FACTOR`]) down to an 80% low-water mark so the next ~20% of inserts won't re-trigger
    /// the sort. `now` is the current frame (for the staleness term).
    fn trim(&mut self, now: u64) {
        if self.bytes <= self.max_bytes {
            return;
        }
        let low_water = self.max_bytes - self.max_bytes / 5; // 80%
                                                             // Score each entry once (higher = colder = evict first).
        let mut scored: Vec<(u64, f32)> = self
            .entries
            .iter()
            .map(|(&k, e)| {
                let stale = now.saturating_sub(e.last_seen) as f32;
                // GreedyDual-Size-Distance: distance-primary · staleness · a gentle size nudge (drop the larger
                // of two near-equally-cold tiles first → reclaim the byte budget in fewer evictions). See
                // `EVICT_SIZE_WEIGHT`.
                let size_factor = 1.0 + EVICT_SIZE_WEIGHT * (e.bytes as f32 / EVICT_SIZE_REF);
                (k, e.distance * (1.0 + EVICT_STALENESS_FACTOR * stale) * size_factor)
            })
            .collect();
        // Partition the coldest entries to the front in O(n) average with `select_nth_unstable_by` (mirroring
        // the sibling `evict` system, geo_world.rs), then sort ONLY that overflow prefix — not all n entries.
        // The split is a BYTE budget over variable-size entries, so estimate the count from the mean entry size
        // (+1 margin); we then evict the sorted prefix coldest-first down to the low-water mark. A size estimate
        // that falls short simply spills into the (hotter, unordered) tail until the budget is met — rare, and a
        // mild overshoot past low-water is harmless (it only defers the next trim). Net: O(n) + O(cut·log cut)
        // over the small overflow, vs the old O(n·log n) total order — a real win when trim fires every step
        // under a thrashing/teleporting camera (audit L2).
        let n = scored.len();
        let mean = (self.bytes / n.max(1)).max(1);
        let cut = ((self.bytes - low_water) / mean + 1).min(n);
        let cmp = |a: &(u64, f32), b: &(u64, f32)| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal);
        if cut < n {
            scored.select_nth_unstable_by(cut - 1, cmp);
        }
        scored[..cut].sort_unstable_by(cmp); // exact coldest-first within the prefix we actually evict
        for (k, _) in scored {
            if self.bytes <= low_water {
                break;
            }
            if let Some(e) = self.entries.remove(&k) {
                self.bytes -= e.bytes;
            }
        }
    }

    #[inline]
    fn len(&self) -> usize {
        self.entries.len()
    }

    #[inline]
    fn byte_len(&self) -> usize {
        self.bytes
    }
}

// (The geometry tier's budget is no longer a scene-side const: the disk cache is ONE cell —
// `logi_3dtiles_io::TileDiskCache` — whose total budget is split across its byte + geometry tiers at open.)

/// The persisted decoded-geometry tier (audit M3): an origin-scoped [`DiskCache`] of serialized `TileGeometry`,
/// shared (`Arc`) between the `decode` writer and the `select_request` reader. `None` until
/// [`GeoWorld::set_geom_cache`] enables it (the FFI create path, when a disk-cache dir is configured).
#[derive(Resource, Default)]
struct GeomCacheRes(Option<Arc<DiskCache>>);

/// Cap on persisted-geometry-tier (M3) restores per stream step — caps SUCCESSFUL geom restores per step (a
/// CRC-filtered `get_raw` miss or a deserialize failure returns `false` and is NOT counted) so a cold-start
/// revisit over a previously-explored region can't stall the stream thread; the remainder restore over
/// subsequent steps. Decode-avoidance, not a render gate, so a modest cap suffices.
const MAX_GEOM_RESTORES_PER_STEP: usize = 8;

/// Predictive-prefetch lookahead (audit M4): project the camera forward by this multiple of its per-advance
/// displacement, run a coarser traversal there, and prefetch the tiles it needs — so fetch+decode latency is
/// hidden behind forward motion instead of showing the coarse-ancestor blur on first approach at speed.
const PREFETCH_LOOKAHEAD: f64 = 8.0;
/// Minimum per-advance displacement (metres) worth prefetching — below this the camera is ~parked and the
/// reactive selection already covers the view (skip the second traversal so a still camera pays nothing).
const PREFETCH_MIN_MOTION_M: f64 = 0.5;
/// Cap on prefetch fetches enqueued per step (drained only from SPARE in-flight budget) so a fast fly-through
/// can't let speculative tiles crowd out the tiles the camera actually needs NOW.
const MAX_PREFETCH_PER_STEP: usize = 8;
/// Run the predictive-prefetch lookahead only every Nth FULL step (audit 2026-06-10): it is a second complete
/// traversal, and its 8x-displacement target moves slowly relative to full-step cadence -- the same tiles warm
/// at 1/N of the traversal cost. The motion vector spans the whole interval, so the lookahead distance is
/// interval-invariant for a given camera speed.
const PREFETCH_INTERVAL_STEPS: u32 = 4;
/// SSE coarsening for the predictive prefetch pass: speculative tiles along the motion vector are selected at
/// `max_sse × this` (the cheaper APPROACH LOD), not full detail — the reactive pass refines them once the
/// camera actually arrives. Higher ⇒ cheaper/coarser speculation.
const PREFETCH_SSE_COARSEN_FACTOR: f64 = 2.0;

/// Per-step cap on NEW cold fetches the reactive pass issues (audit E concentration): bounds the burst so a
/// post-eviction / camera-jump frame can't enqueue the whole visible frontier at once — which floods the pool
/// and the in-flight cap with a THIN spread across every region (the patchwork). The top-K highest-priority tiles
/// are funded this step; the rest re-rank next step, so the focal region's column lands over a few steps.
const MAX_NEW_FETCHES_PER_STEP: usize = 32;

/// Depth-first concentration boost (audit E): a tile whose PARENT is already resident/grafted out-ranks a
/// fresh-region tile of equal screen importance by this factor, so the budget DESCENDS a region already
/// underway (parent→child→grandchild) instead of broadening — finishing the focal region to fine before
/// starting new ones. The parent's presence is the top-down-graft precondition, so this can't break ordering.
const CONCENTRATE_BOOST: f64 = 4.0;

/// Bump this on ANY change to the serialized `TileGeometry` blob layout OR to `rebase_gltf`'s vertex packing /
/// ENU fold (audit recheck L-1/L-2). It is folded into [`geom_key`], so a bump changes EVERY key: stale-layout
/// blobs become unreferenced (LRU-evicted) and fresh ones re-persist — no migration, and a warm cache can never
/// silently serve old-layout geometry as new (ADR-0004 discard-and-rebuild, applied at the key). The blob's own
/// `GEOM_BLOB_MAGIC` (streaming.rs) is the belt; this is the suspenders — keying it in means a forgotten magic
/// bump can't poison the tier (the old key is simply never queried again).
const GEOM_FORMAT_VERSION: u32 = 1;

/// Key for the persisted geometry tier (audit M3): FNV-1a-128 over the ENU origin's ECEF bytes + the tile key +
/// [`GEOM_FORMAT_VERSION`]. The origin term makes the SAME tile under a DIFFERENT origin a DIFFERENT entry
/// (decoded vertices are baked relative to the origin, so keying on `tile_key` alone would replay offset
/// geometry after a relaunch at another hero location); the version term makes a layout change a clean cache
/// turnover rather than a silent corruption / permanent re-decode.
fn geom_key(origin_ecef: DVec3, tile_key: u64) -> u128 {
    const PRIME: u128 = 0x0000_0000_0100_0000_0000_0000_0000_013B;
    let mut bytes = [0u8; 36];
    bytes[0..8].copy_from_slice(&origin_ecef.x.to_le_bytes());
    bytes[8..16].copy_from_slice(&origin_ecef.y.to_le_bytes());
    bytes[16..24].copy_from_slice(&origin_ecef.z.to_le_bytes());
    bytes[24..32].copy_from_slice(&tile_key.to_le_bytes());
    bytes[32..36].copy_from_slice(&GEOM_FORMAT_VERSION.to_le_bytes());
    let mut h: u128 = 0x6C62_272E_07BB_0142_62B8_2175_6295_C58D;
    for b in bytes {
        h ^= b as u128;
        h = h.wrapping_mul(PRIME);
    }
    h
}

/// Per-step telemetry (read by the stream thread for the mailbox / diagnostics).
#[derive(Resource, Clone, Copy, Debug, Default)]
pub struct GeoStats {
    pub resident: u32,
    pub decoded_this_step: u32,
    pub evicted_this_step: u32,
    /// Cumulative tiles evicted Resident→Free (the LRU reclaim). Flat-zero while resident climbs toward the
    /// cap is normal; flat-zero at a FULL pool is the 2026-06-11 starvation signature (no candidates).
    pub evicted_total: u32,
    /// Cumulative selected tiles DROPPED because the pool had no free slot (`PoolExhausted` / restore
    /// back-pressure). Any sustained climb means demand is starving — the pool/eviction is the bottleneck.
    pub pool_exhausted_total: u32,
    // --- The selection X-ray (2026-06-11 scars): LAST FULL STEP's hold-reason counts, SNAPSHOT semantics
    // (mirrors `logi_3dtiles_select::HoldCounts`; see its docs). A scar = `hold_leaf_under_sse` or
    // `fetch_skip_grafted` sustained at quiescence.
    pub hold_kick: u32,
    pub hold_dead: u32,
    pub hold_loading: u32,
    pub hold_leaf_under_sse: u32,
    pub hold_pending_subtree: u32,
    /// TILESET load candidates skipped THIS STEP because their `grafted` record says the tileset is already
    /// applied (the skip's one legitimate job). Near-zero in steady state — the selector stops emitting a
    /// tileset load once the graft's children are in the tree. Sustained nonzero = a graft applied whose
    /// children the traversal can't see (stale record / prune skew). NOTE (2026-06-11): this skip used to
    /// fire on the node id ALONE, silently blocking every grafted node's own GLB fetch — the stuck-coarse
    /// scar; GLB candidates now always proceed.
    pub fetch_skip_grafted: u32,
    /// Cumulative tiles whose payload failed to decode — terminal (never re-requested). A dataset-health
    /// signal: steady zero is healthy; a climbing count means corrupt/unsupported tiles in the stream.
    pub decode_failed_total: u32,
    /// Cumulative warm re-entries served from the decoded-mesh LRU (`FlashCache`) — a tile re-selected after
    /// eviction whose mesh was restored with **no re-fetch and no re-decode**. The cache's effectiveness.
    pub flash_cache_hits_total: u32,
    /// Cumulative in-flight fetches **pre-emptively cancelled** because their tile left the selection before
    /// arriving (fast camera motion). Bandwidth that would otherwise have been spent on already-stale tiles.
    pub fetches_cancelled_total: u32,
    /// Cumulative selected tiles NOT served warm from the flash cache (a cold path — fetch/decode, or a geom-tier
    /// restore). With `flash_cache_hits_total` this is the flash HIT-RATE that says whether the budget is sized
    /// right (audit L1 — previously un-observable).
    pub flash_cache_miss_total: u32,
    /// Cumulative tiles restored from the PERSISTED decoded-geometry tier — bytes were on local disk, so the
    /// dominant Draco decode + ENU rebase was skipped (audit M3). With `geom_cache_miss_total` = that tier's hit-rate.
    pub geom_cache_hits_total: u32,
    /// Cumulative flash-cache misses the geometry tier ALSO couldn't serve (had to fetch + Draco-decode) — the
    /// geometry-tier miss half (audit M3).
    pub geom_cache_miss_total: u32,
    /// Cumulative tile paths **given up after [`MAX_FETCH_ATTEMPTS`] consecutive failed fetches** (a dead /
    /// permanently-erroring URL). A climbing count points at a bad endpoint, expired auth, or network trouble;
    /// distinct from `decode_failed_total` (bytes arrived but didn't parse).
    pub fetch_failed_total: u32,
    /// Cumulative Google P3DT **session refreshes** — the root tileset re-fetched for a fresh `?session=…`
    /// after a 401/403 (the session TTL expired). A climbing count is NORMAL on a long session; if it climbs
    /// *and* `fetch_failed_total` keeps climbing too, the refresh isn't recovering (bad key / network).
    pub session_refreshes_total: u32,
    /// Cumulative external-tileset **manifests that fetched fine but FAILED TO GRAFT** (the child `tileset.json`
    /// didn't parse, or its graft target was gone). The subtree never appears, so its whole region is stuck at
    /// the coarse parent — a patchy-blur cause that was **silent** before this counter (the graft error was
    /// dropped). A climbing count means manifests we can't ingest (malformed/unsupported child tilesets).
    pub graft_failed_total: u32,
    /// Cumulative selected tiles whose content URI **couldn't be resolved** against its (grafted) base — a
    /// malformed relative URL or a missing subtree base. The tile is never even fetched, so its region stays
    /// coarse — also **silent** before this counter. A climbing count points at a base-URL / template-URI bug.
    pub url_resolve_failed_total: u32,
}

/// A hole-clip box in **Unreal render space** (cm, the same frame as [`TileGeometry`] vertices): the renderer
/// discards a coarse tile's fragments wherever they fall inside one of these boxes, so a coarse stand-in tiles
/// the ground edge-to-edge with the finer tiles drawn over it (no coplanar overlap, no PDO). Center + three
/// half-extent axis vectors (the box spans `center ± Σ sᵢ·half_axes[i]`, `sᵢ ∈ [-1, 1]`). The producer
/// ([`oriented_clip_box`]) emits boxes ORIENTED to each child tile's own ground frame (geocentric `up` +
/// ground basis) — the C++ material's per-axis test is fully general, and an axis-aligned box over-cut tilted
/// far-from-origin tiles (PIE 2026-06-09).
#[derive(Clone, Copy, Debug, Default)]
pub struct RenderClipBox {
    pub center: [f32; 3],
    pub half_axes: [[f32; 3]; 3],
}

/// One tile in a published Geo render frame: its stable key (for C++ section caching) + a pinned share of
/// its geometry. Holding the `Arc` keeps the buffers alive for the renderer even after eviction.
#[derive(Clone)]
pub struct GeoRenderTile {
    pub tile_key: u64,
    pub geom: Arc<TileGeometry>,
    /// Refinement tree depth (root = 0). Mirrored across the FFI for debug-viz / telemetry only.
    pub depth: u32,
    /// 3D Tiles geometric error (metres). Mirrored across the FFI for debug-viz / telemetry only.
    pub geometric_error: f32,
    /// Show now vs. build hidden. `true` = the renderer has textured this tile ⇒ show it; `false` =
    /// geometry-resident but not yet textured ⇒ a "prepare" tile the renderer builds HIDDEN (mirrored to
    /// `LogiGlobeTile.flags` bit 1) and textures under the coarse cover before Rust promotes it. Set by
    /// [`publish`] from [`TexturedSet`].
    pub visible: bool,
    /// **Geometric hole-clip** boxes in render space (empty for almost every tile): when this tile is a coarse
    /// REPLACE stand-in drawn over a region where finer tiles have loaded+textured, these are those finer
    /// tiles' footprints — each the TIGHT bounds of the child's decoded mesh ORIENTED to the child's own
    /// ground frame ([`oriented_clip_box`]), not its loose tileset bounding volume. The renderer clips this
    /// tile's fragments out of them. Materialized by [`publish`] from [`ClipChildren`]; grows one box per
    /// child as the children texture in, until this tile drops entirely.
    pub clip_boxes: Vec<RenderClipBox>,
}

/// A published render frame — the `Resident` tiles in view this step, geometry pinned via `Arc`. Built by
/// [`publish`] into [`GeoMailbox`]; the stream thread mirrors it to the cross-thread handoff (ADR-0012 §3).
#[derive(Clone, Default)]
pub struct GeoRenderFrame {
    pub frame: u64,
    pub origin_ecef: [f64; 3],
    pub tiles: Vec<GeoRenderTile>,
}

/// The latest published render frame (the schedule's output), shared as an `Arc` so [`GeoWorld::latest_frame`]
/// and the stream thread's cross-thread mailbox hand it on with an O(1) refcount bump instead of the old
/// 3×O(R) deep clones per step (audit 2026-06-10 §2 mailbox row). `fingerprint` is the content hash of the
/// CURRENT frame (tile keys + visibility + clip boxes): when a re-publish produces identical content, the
/// previous frame (and its `frame` id) is KEPT, so every downstream frame-id-equality skip — the stream
/// mirror, `GlobeStreamer::poll`'s `Same`, the C++ reconcile — fires for free (audit C9).
#[derive(Resource, Default)]
struct GeoMailbox {
    frame: Arc<GeoRenderFrame>,
    fingerprint: u64,
}

/// Memoized render-space clip boxes, keyed by tile key (audit 2026-06-10 §2): [`oriented_clip_box`] is a pure
/// function of the (immutable) decoded mesh, yet was recomputed from the FULL vertex buffer for every clip
/// child on EVERY publish — ~10⁵–10⁶ redundant vertex reads/step while a stand-in converges. `stamp` is the
/// last frame the entry was used; entries unused this publish are dropped (the memo tracks only the live clip
/// set, so it stays tiny). `None` = the mesh is degenerate (no ground area) — memoized too, so a degenerate
/// child isn't re-scanned every step.
#[derive(Resource, Default)]
struct ClipBoxMemo(HashMap<u64, (u64, Option<RenderClipBox>)>);

/// Tile keys the renderer (C++) has fully textured, mirrored from the stream handoff each advance (the
/// [`set_textured_keys`](GeoWorld::set_textured_keys) path). The selector's `is_loaded` gate requires a tile be
/// in this set AND geometry-`Resident` before it can replace its coarse parent, so the kick holds the coarse
/// until the fine tile is textured — LOD loading is invisible (no gray placeholder). [`publish`] reads it to flag
/// each emitted tile `visible` (textured ⇒ shown) or hidden (resident-but-untextured ⇒ a "prepare" tile the
/// renderer textures under the coarse cover, then reports back so the gate promotes it).
#[derive(Resource, Default)]
struct TexturedSet(HashSet<u64>);

/// Per-tile **hole-clip** children from the latest selection pass: a coarse stand-in's `tile_key` → the
/// `tile_key`s of the finer tiles drawn over it (its immediate rendered descendants, [`SelectedTile::clip_children`]
/// mapped through [`tile_key`]). Filled by [`GeoWorld::select_request`] from the selector's render set;
/// [`publish`] materializes each child's TIGHT render-space clip box from its decoded mesh's AABB
/// ([`oriented_clip_box`] — the selector emits the relationship, not geometry, because the tileset bounding volume is
/// only a loose cover of the mesh). Only a coarse stand-in drawn over loaded finer tiles has an entry, so most
/// frames this map is tiny or empty (it is `clear`ed + refilled each selection pass). Point-lookup only — never
/// iterated for output.
#[derive(Resource, Default)]
struct ClipChildren(HashMap<u64, Vec<(u8, u64)>>);

/// The entities [`publish`] draws this step — the SELECTION's render + prepare tiles, recorded by
/// [`GeoWorld::select_request`] each pass (which flips `active`). Inactive (foundation / schedule-only worlds
/// with no selection — the test surface) falls back to the legacy in-range-`Resident` snapshot. See
/// [`publish`] for why the draw set must be selection-driven (stale finer residents otherwise z-fight the
/// coarsened cut un-clippably). The `Vec` is recycled via `mem::take` each pass — no steady-state allocation.
#[derive(Resource, Default)]
struct PublishSet {
    active: bool,
    entities: Vec<Entity>,
}

/// The render-space **oriented** clip box of a decoded mesh — the tight bounds of its 32-byte-interleaved
/// vertices in the tile's OWN ground frame: `up` = the geocentric radial through the mesh centroid, `e1`/`e2`
/// = an orthonormal ground basis. [`TileGeometry`] vertices are already in render space (cm, `rebase_gltf`),
/// so no transform is applied — only a basis projection.
///
/// Why oriented (PIE evidence 2026-06-09): far from the ENU origin a tile is TILTED in render space
/// (curvature), so the previous *axis-aligned* box over-covered the tilted quad by its slab — the renderer
/// then discarded coarse stand-in fragments over ground the child does NOT draw (jagged mixed-LOD reveals),
/// and the P3DT edge skirt inflated the AABB sideways (the thin gap ring along tile edges). In the tile's own
/// frame the box hugs the surface: the skirt inflates only the `up` extent, which never widens the footprint
/// cut, and the ground extents land on the child's true edge-to-edge footprint.
///
/// Returns `None` when either GROUND extent is non-positive (a wall/line/point sliver, or the empty buffer):
/// the host's per-axis inside-test is vacuously true on a zero axis, so such a "box" would discard a stripe of
/// the stand-in over ground the child doesn't cover. (Skipping a box is always safe — brief coarse-over-fine
/// overlap, never a hole.) Two O(V) passes (centroid, then projected extents) over only the few clip children.
fn oriented_clip_box(vertices: &[u8], earth_center_render: glam::DVec3) -> Option<RenderClipBox> {
    let mut centroid = glam::DVec3::ZERO;
    let mut count = 0u32;
    for v in vertices.chunks_exact(32) {
        let p = |k: usize| f32::from_le_bytes(v[k * 4..k * 4 + 4].try_into().unwrap()) as f64;
        centroid += glam::DVec3::new(p(0), p(1), p(2));
        count += 1;
    }
    if count == 0 {
        return None;
    }
    centroid /= count as f64;
    // The tile's ground frame: `up` = geocentric radial (render space shares the planet's geometry — the
    // earth center is just a fixed render-space point); `e1`/`e2` span the local ground plane. Near the ENU
    // origin `up ≈ +Z` and this degenerates gracefully to the old axis-aligned behavior.
    let up = (centroid - earth_center_render).normalize_or_zero();
    if up == glam::DVec3::ZERO {
        return None; // centroid AT the earth center — hostile/degenerate content
    }
    let r = if up.z.abs() < 0.9 {
        glam::DVec3::Z
    } else {
        glam::DVec3::X
    };
    let e1 = up.cross(r).normalize();
    let e2 = up.cross(e1); // unit: up ⟂ e1, both unit
    let (mut mn, mut mx) = ([f64::INFINITY; 3], [f64::NEG_INFINITY; 3]);
    for v in vertices.chunks_exact(32) {
        let p = |k: usize| f32::from_le_bytes(v[k * 4..k * 4 + 4].try_into().unwrap()) as f64;
        let d = glam::DVec3::new(p(0), p(1), p(2)) - centroid;
        for (k, axis) in [e1, e2, up].iter().enumerate() {
            let t = d.dot(*axis);
            mn[k] = mn[k].min(t);
            mx[k] = mx[k].max(t);
        }
    }
    // A clip box must have ground AREA: both ground extents strictly positive (`up` may be ~0 — flat terrain
    // is exactly the plan-view footprint case this exists for).
    if mx[0] <= mn[0] || mx[1] <= mn[1] {
        return None;
    }
    let half = |k: usize| ((mx[k] - mn[k]) * 0.5).max(0.0);
    let mid = |k: usize| (mn[k] + mx[k]) * 0.5;
    let center = centroid + e1 * mid(0) + e2 * mid(1) + up * mid(2);
    let ax = |axis: glam::DVec3, h: f64| {
        let a = axis * h;
        [a.x as f32, a.y as f32, a.z as f32]
    };
    Some(RenderClipBox {
        center: [center.x as f32, center.y as f32, center.z as f32],
        half_axes: [ax(e1, half(0)), ax(e2, half(1)), ax(up, half(2))],
    })
}

/// Grow a clip box's `up` half-axis by `pad_cm` (symmetric — the center stays put), leaving the ground
/// (`e1`/`e2`) extents untouched. Used at clip ATTACH time to thicken a child's surface-hugging box by the
/// consuming STAND-IN's geometricError, so the coarser parent's deviating sheet still falls inside the
/// discarded slab (see the attach-site comment in [`publish`]). The memoized box's own `up` half-length may
/// be ~0 (flat terrain), so the unit direction is recovered from `e1 × e2` when needed — the producer built
/// the three axes orthogonal, and the inside-test is sign-symmetric, so either orientation works.
/// The renderer's hard per-tile clip-box cap (`LOGI_CLIP_MAX` in the C++ masked material's unrolled loop,
/// asserted ≤8 by the FFI contract test). [`publish`]'s group-union reduction makes reaching it structurally
/// rare (≤4 quadtree / ≤8 octree direct-child groups); a residual overflow drops the smallest footprints.
const RENDER_CLIP_MAX: usize = 8;
/// Union coverage tolerance: the members' projected ground rects must cover at least this fraction of the
/// union rect's area — measured EXACTLY (rect-union by coordinate compression below), so this only absorbs
/// f32→f64 rounding, NOT real gaps. The first cut used a 0.65 summed-area heuristic; member overlap inflated
/// the sum and admitted unions with genuine interior gaps, which the clip then cut out of the stand-in over
/// ground nothing else redraws — the 2026-06-11 black wedge HOLES. "Never have holes" demands exactness.
const CLIP_UNION_MIN_COVERAGE: f64 = 0.995;

/// Fit ONE box covering every member, in the (ground) frame of the LARGEST member — the group-union reduction
/// that keeps a skip-level frontier within [`RENDER_CLIP_MAX`] (the 2026-06-11 fast-camera coarse-over-fine
/// sheets: >8 rendered descendants per stand-in overflowed the cap and the surplus drew unclipped). Returns
/// `None` unless the members' projected footprints EXACTLY tile the union's footprint (no interior gaps —
/// [`CLIP_UNION_MIN_COVERAGE`]); the caller then keeps individual boxes (over-cut = holes; overflow = brief
/// overdraw; holes lose). Sibling members share nearly identical geocentric `up`s (one subtree ⇒ adjacent
/// ground), so one shared frame fits tightly, and adjacent tiles' slack AABBs overlap — a contiguous group
/// passes the exact test naturally while any group with a missing interior tile fails it.
fn union_clip_boxes(members: &[RenderClipBox]) -> Option<RenderClipBox> {
    debug_assert!(members.len() >= 2, "a union of <2 boxes is the box itself");
    let v = |a: [f32; 3]| glam::DVec3::new(a[0] as f64, a[1] as f64, a[2] as f64);
    // Frame = the largest-footprint member's axes (unit) — its up is representative of the whole group.
    let area = |b: &RenderClipBox| v(b.half_axes[0]).length() * v(b.half_axes[1]).length();
    let frame = members
        .iter()
        .max_by(|a, b| area(a).total_cmp(&area(b)))
        .expect("members is non-empty");
    let fc = v(frame.center);
    let mut axes = [glam::DVec3::ZERO; 3];
    for (k, ax) in axes.iter_mut().enumerate() {
        *ax = v(frame.half_axes[k]).normalize_or_zero();
        if *ax == glam::DVec3::ZERO {
            return None; // degenerate frame box — refuse; individuals are always safe
        }
    }
    // Project every member's 8 corners into the frame: per-member GROUND rect (axes 0/1) + global extents.
    let (mut mn, mut mx) = ([f64::INFINITY; 3], [f64::NEG_INFINITY; 3]);
    let mut rects: Vec<[f64; 4]> = Vec::with_capacity(members.len()); // [x0, x1, y0, y1] per member
    for m in members {
        let (c, h0, h1, h2) = (v(m.center), v(m.half_axes[0]), v(m.half_axes[1]), v(m.half_axes[2]));
        let mut r = [f64::INFINITY, f64::NEG_INFINITY, f64::INFINITY, f64::NEG_INFINITY];
        for s in 0..8u8 {
            let corner = c
                + h0 * if s & 1 == 0 { 1.0 } else { -1.0 }
                + h1 * if s & 2 == 0 { 1.0 } else { -1.0 }
                + h2 * if s & 4 == 0 { 1.0 } else { -1.0 };
            let d = corner - fc;
            for k in 0..3 {
                let t = d.dot(axes[k]);
                mn[k] = mn[k].min(t);
                mx[k] = mx[k].max(t);
            }
            let (x, y) = (d.dot(axes[0]), d.dot(axes[1]));
            r = [r[0].min(x), r[1].max(x), r[2].min(y), r[3].max(y)];
        }
        rects.push(r);
    }
    let union_area = (mx[0] - mn[0]) * (mx[1] - mn[1]);
    if union_area <= 0.0 {
        return None;
    }
    // EXACT union-of-rects coverage by coordinate compression: split the plane at every rect edge; a cell is
    // covered iff its center lies in some rect (cells never straddle an edge, so the center decides the whole
    // cell). O(n) coords ⇒ ≤(2n−1)² cells × n rects — n ≤ 32 candidates ⇒ ≤ ~127k cheap compares, and only
    // on publishes where a ≥2-member clip group exists at all.
    let mut xs: Vec<f64> = rects.iter().flat_map(|r| [r[0], r[1]]).collect();
    let mut ys: Vec<f64> = rects.iter().flat_map(|r| [r[2], r[3]]).collect();
    xs.sort_unstable_by(f64::total_cmp);
    xs.dedup();
    ys.sort_unstable_by(f64::total_cmp);
    ys.dedup();
    let mut covered = 0.0_f64;
    for wx in xs.windows(2) {
        for wy in ys.windows(2) {
            let (cx, cy) = ((wx[0] + wx[1]) * 0.5, (wy[0] + wy[1]) * 0.5);
            if rects
                .iter()
                .any(|r| cx >= r[0] && cx <= r[1] && cy >= r[2] && cy <= r[3])
            {
                covered += (wx[1] - wx[0]) * (wy[1] - wy[0]);
            }
        }
    }
    if covered < CLIP_UNION_MIN_COVERAGE * union_area {
        return None; // an interior gap — unioning would clip ground nothing redraws (holes)
    }
    let mut center = fc;
    let mut half_axes = [[0.0_f32; 3]; 3];
    for k in 0..3 {
        center += axes[k] * ((mn[k] + mx[k]) * 0.5);
        let half = axes[k] * ((mx[k] - mn[k]) * 0.5);
        half_axes[k] = [half.x as f32, half.y as f32, half.z as f32];
    }
    Some(RenderClipBox {
        center: [center.x as f32, center.y as f32, center.z as f32],
        half_axes,
    })
}

fn extend_clip_box_up(mut b: RenderClipBox, pad_cm: f64) -> RenderClipBox {
    let up = glam::DVec3::new(
        b.half_axes[2][0] as f64,
        b.half_axes[2][1] as f64,
        b.half_axes[2][2] as f64,
    );
    let len = up.length();
    let dir = if len > 1.0e-3 {
        up / len
    } else {
        let a0 = glam::DVec3::new(
            b.half_axes[0][0] as f64,
            b.half_axes[0][1] as f64,
            b.half_axes[0][2] as f64,
        );
        let a1 = glam::DVec3::new(
            b.half_axes[1][0] as f64,
            b.half_axes[1][1] as f64,
            b.half_axes[1][2] as f64,
        );
        let c = a0.cross(a1).normalize_or_zero();
        if c == glam::DVec3::ZERO {
            return b; // degenerate ground axes — producer guards make this unreachable; keep the box as-is
        }
        c
    };
    let new = dir * (len + pad_cm.max(0.0));
    b.half_axes[2] = [new.x as f32, new.y as f32, new.z as f32];
    b
}

/// LOD hysteresis dead-band fraction for the streaming selector: a tile refined last frame holds its split
/// until SSE drops below `max_sse / (1 + LOD_HYSTERESIS)`. ~15% keeps a camera dwelling on a threshold from
/// flip-flopping parent↔children frame to frame (anti-pop) without a visible LOD-too-coarse lag. Tunable.
const LOD_HYSTERESIS: f64 = 0.15;
/// In-frustum tiles BEYOND the full-detail band (`Visibility::Distant` — the visible far vista) refine to
/// `max_sse × DISTANT_SSE_FACTOR`: a mild demotion, since these pixels are on screen. (This carries the old
/// CULLED_SSE_FACTOR=2.0 behavior for the vista; the view-edge-seam concern that drove 4.0→2.0 is handled by
/// FRUSTUM_MARGIN_RAD classifying edge tiles fully Visible.)
const DISTANT_SSE_FACTOR: f64 = 2.0;
/// Off-frustum (`Visibility::Offscreen`) tiles refine to `max(max_sse × CULLED_SSE_FACTOR,
/// CULLED_SSE_FLOOR_PX)` — a genuinely COARSE shell kept resident (shadow casters + camera-turn readiness).
/// Mirrors cesium's `culledScreenSpaceError` (64) vs `maximumScreenSpaceError` (16) ratio. The FLOOR is the
/// load-bearing part (2026-06-10 ground cut flood): these tiles are INVISIBLE, and without a floor a fine
/// Detail Target knob (the live map runs max_sse=1) made the entire hidden hemisphere — out to a ~30,000 km
/// Max Tile Distance — refine to 2 px: 19k published tiles at a 2.5 km camera, >10k of them off-frustum.
const CULLED_SSE_FACTOR: f64 = 8.0;
const CULLED_SSE_FLOOR_PX: f64 = 32.0;

/// Full-detail distance band (audit E #7): in-frustum tiles within this many metres of the camera get FULL
/// detail; in-frustum but farther are demoted to the coarser `culled_screen_space_error` (a coarse shell —
/// the planet vista is kept, just not fine). A FIXED distance (not altitude-scaled): an altitude-scaled cull
/// over-restricts ground/oblique views (a surface camera that sees km of terrain) AND can't help orbit
/// (everything is ~one altitude away), whereas a fixed band correctly keeps the near sharp and the far coarse
/// at EVERY altitude — orbit (tiles ≫ band) goes all-coarse, descent brings the near ground into the band.
/// The hard out-of-range cull stays at the C++ `max_tile_distance` (the far vista is coarse, never culled).
/// THE key concentration tuning knob — raise to see further sharp, lower to concentrate the budget harder.
const DETAIL_BAND_M: f64 = 25_000.0;
/// Angular slack (radians) added to the view frustum's half-angles for the in-view test, so tiles at the
/// screen edge classify in-view (full detail) instead of flipping to the coarse shell — no LOD seam crawling
/// the screen border, and a buffer against camera rotation. 0.18 rad (≈10°, was 0.12/≈7°): a wider full-detail
/// margin so the sharp/coarse boundary sits comfortably outside the visible frame, not at its very edge.
const FRUSTUM_MARGIN_RAD: f64 = 0.18;

// ---- Systems (few, right-sized) --------------------------------------------------------------------

/// The **cheap per-tile pass**, over the ACTIVE working-set only (ADR-0018): distance + LRU stamp for
/// every live tile, iterating [`LiveSet`] and random-accessing each entity — **O(active)**, not a `par_iter`
/// over the whole pre-spawned pool (**O(pool)**) every step. The per-tile work is one length + compare,
/// so a sequential walk of the (budget-bounded) live set beats spawning tasks over thousands of `Free` slots —
/// the same `par_iter`-per-cheap-op the round-4 spike measured as a net loss. The load-bearing output is the
/// `LastSeen` LRU stamp `evict` reads. (The vestigial per-tile `Sse`/`CamDistance` component writes — read by
/// nothing, and the `Sse` one paid a full `screen_space_error` incl. a `tan()` per tile — were dropped
/// 2026-06-09; the selection traversal computes its own SSE from `RefineParams`.)
fn sse_cull_lru(
    view: Res<GeoView>,
    live: Res<LiveSet>,
    publish: Res<PublishSet>,
    mut tiles: Query<(&TileState, &TileBounds, &mut LastSeen)>,
) {
    let (cam, frame, cull) = (view.camera_ecef, view.frame, view.cull_distance_m);
    // Drift guard (debug only — compiled out in release, where the O(pool) scan is NOT paid): `LiveSet` MUST
    // equal the non-`Free` tiles. A missed `Free`↔non-`Free` transition (acquire/release) fails a test here
    // instead of silently mis-stamping the LRU (→ a wrong eviction) in release.
    debug_assert_eq!(
        live.0.len(),
        tiles.iter().filter(|t| *t.0 != TileState::Free).count(),
        "LiveSet drifted from the actual non-Free tiles — a Free↔non-Free transition wasn't recorded"
    );
    // With a LIVE SELECTION the warmth stamp is the SELECTION's job (select_request stamps exactly its
    // render + prepare tiles): the cut is what `evict` must spare, and the in-range stamp here would spare
    // far too much — at a planet-scale cull distance (the live map runs Max Tile Distance ≈ 29,559 km >
    // any camera-to-far-side distance) it stamped EVERY live tile EVERY step, so `evict` never had a
    // candidate, the resident set blew through its cap until the WHOLE pool was Resident, the free-list
    // hit zero, and every restore/fetch was silently dropped — the 2026-06-11 "zoom in and nothing loads"
    // total-starvation stall. Selection-less worlds (foundation/tests, and `publish`'s fallback snapshot
    // which keys off this stamp) keep the legacy in-range stamp.
    if publish.active {
        return;
    }
    for &e in live.0.iter() {
        let Ok((_state, bounds, mut seen)) = tiles.get_mut(e) else {
            continue; // a set entry without a live row (defensive — the drift assert catches real skew)
        };
        let d = ((cam - bounds.center).length() - bounds.radius).max(0.0);
        // LRU stamp ONLY for in-view tiles (within the cull radius); off-screen tiles keep their old
        // `LastSeen` and become evictable. `cull <= 0` ⇒ no cull, everything is in view.
        if cull <= 0.0 || d <= cull {
            seen.0 = frame;
        }
    }
}

/// Decode this step's just-fetched tiles into geometry, driven off the explicit [`JustFetched`] set (ADR-0018)
/// — **O(Δ_fetched)**, NOT a `par_iter` over the whole pool gated by `Changed<FetchedBytes>` (which spawned
/// ~one task per pooled row every step, almost all early-returning on the change check). A no-arrival step does
/// nothing.
///
/// **Decode fan-out (ADR-0018 dec. 5 — its measure-gate is now MET):** the 2026-06-11 flyby telemetry showed
/// the decode backlog climbing to 606–708 queued tiles while the fetch pipe sat pegged at its 256-slot cap —
/// arrivals outrun a single thread's sequential Draco decode (~5–15 ms each), making decode THE binding
/// constraint on convergence speed during flight (the user's "tiles don't load fast enough while flying").
/// A multi-arrival step therefore fans the pure decode closure (`TileDecodeFn: Send + Sync`, no world access)
/// across the shared bounded `ComputeTaskPool` and applies results sequentially; the geom-tier persist rides
/// the parallel task (the `DiskCache` is internally synchronized, and serialize+write are themselves CPU/IO
/// work worth keeping off the stream thread). Single arrivals keep the inline path — no task overhead on the
/// steady trickle (the ADR-0018 par-iter-per-cheap-op lesson; one Draco decode is NOT cheap, so the cutover
/// is at 2). State writes (`ResidentSet`, components, stats, reclaim) stay sequential on this thread.
// A 5-tuple bevy `Query` is inherently a complex type; a `type` alias would only hide the system's data shape.
// Bevy system params count as fn args to clippy; the +1 over the limit is the `GeoStats` telemetry resource
// (decoded_this_step — the ADR-0018 dec. 5 gate signal). Same dispensation as `evict`.
#[allow(clippy::type_complexity, clippy::too_many_arguments)]
fn decode(
    decoder: Res<TileDecoder>,
    view: Res<GeoView>,
    publish: Res<PublishSet>,
    mut just_fetched: ResMut<JustFetched>,
    mut resident_set: ResMut<ResidentSet>,
    mut reclaim: ResMut<Reclaim>,
    geom_cache: Res<GeomCacheRes>,
    enu: Res<GeoEnu>,
    mut stats: ResMut<GeoStats>,
    mut tiles: Query<(
        &TileId,
        &mut FetchedBytes,
        &TileTransform,
        &mut ResidentMesh,
        &mut TileState,
        &mut LastSeen,
    )>,
) {
    // Per-step arrival telemetry: the ADR-0018 sequential rewrite deleted the old parallel counter path and
    // left `decoded_this_step` permanently 0 (re-eval 2026-06-09). It was the burst-size signal that — via
    // the flyby sidecars — finally MET the dec.-5 measure-gate (see the fan-out note above).
    stats.decoded_this_step = 0;
    // Phase A (sequential, world access): collect this step's decodable work. `take` empties the set (a stale
    // entry can't re-decode next step); an empty set ⇒ zero work on a settled step. The GLB bytes are `Arc`,
    // so the work list borrows nothing from the world — phase B runs without it.
    let mut work: Vec<(Entity, Arc<[u8]>, DMat4, u64)> = Vec::new();
    for e in std::mem::take(&mut just_fetched.0) {
        let Ok((id, bytes, xf, _, state, _)) = tiles.get_mut(e) else {
            continue; // slot recycled out from under a stale just-fetched entry (defensive)
        };
        if *state != TileState::Fetched {
            continue; // already cancelled / freed since `drain_raw` recorded the arrival
        }
        match &bytes.0 {
            Some(raw) => work.push((e, Arc::clone(raw), xf.0, id.0)),
            None => reclaim.decode_failed.push(e), // defensive: Fetched with no bytes — reconcile frees it
        }
    }
    if work.is_empty() {
        return;
    }
    // Phase B: the pure decode (+ the geom-tier persist — audit M3: a future cold revisit or same-origin
    // relaunch skips the dominant Draco decode + rebase; `contains` skips already-persisted tiles, `put_raw`
    // is best-effort and internally synchronized — and the serialize+write are themselves work worth keeping
    // off the stream thread, subsuming the old dec.-6 inline-write caveat). One arrival decodes INLINE (no
    // task overhead on the steady trickle — one Draco decode is NOT the cheap op the ADR-0018 par-iter lesson
    // warned about, so the cutover is at 2); a burst fans across the shared bounded ComputeTaskPool, results
    // returned in spawn order.
    let decode_one = |raw: &[u8], xf: DMat4, key: u64| -> Option<Arc<TileGeometry>> {
        let geom = (decoder.0)(raw, xf)?;
        let arc = Arc::new(geom);
        if let Some(gc) = &geom_cache.0 {
            let gk = geom_key(enu.0.origin_ecef, key);
            if !gc.contains(gk) {
                gc.put_raw(gk, &crate::streaming::serialize_geometry(&arc));
            }
        }
        Some(arc)
    };
    let decoded: Vec<Option<Arc<TileGeometry>>> = if work.len() == 1 {
        vec![decode_one(&work[0].1, work[0].2, work[0].3)]
    } else {
        ComputeTaskPool::get().scope(|s| {
            for (_, raw, xf, key) in &work {
                let (raw, xf, key) = (Arc::clone(raw), *xf, *key);
                let decode_one = &decode_one;
                s.spawn(async move { decode_one(&raw, xf, key) });
            }
        })
    };
    // Phase C (sequential, world access): apply results in order.
    for ((e, _, _, _), result) in work.into_iter().zip(decoded) {
        let Ok((_, mut bytes, _, mut mesh, mut state, mut seen)) = tiles.get_mut(e) else {
            continue;
        };
        if let Some(arc) = result {
            mesh.0 = Some(arc);
            *state = TileState::Resident;
            // SELECTION worlds: stamp the ARRIVAL — the tile was demanded by the current selection (that's
            // why it was fetched), but it enters `PublishSet` only at the NEXT full selection, and `evict`
            // spares exactly (published ∪ stamped-this-frame). Selection-LESS (foundation) worlds keep the
            // legacy semantic — `sse_cull_lru`'s in-range stamp alone decides warmth, so an out-of-range
            // arrival stays immediately evictable there.
            if publish.active {
                seen.0 = view.frame;
            }
            stats.decoded_this_step += 1;
            resident_set.0.insert(e); // Fetched→Resident
                                      // The raw GLB is dead once decoded — drop it so a resident tile pins only its mesh, not also its
                                      // (often larger) source bytes. Skips ~100 KB–1 MB of dead weight per resident tile.
            bytes.0 = None;
        } else {
            // Decode failed (corrupt / unparseable GLB): leave the tile `Fetched` with its bytes and hand the
            // ENTITY to `reconcile_index` (O(Δ) — audit 2026-06-10 §2), which removes its index entry by
            // [`TileNode`], marks the node terminal (`decode_failed`), and frees the slot. We never panic
            // across the decode (the decoder returns `Option`).
            reclaim.decode_failed.push(e);
        }
    }
}

/// **Sequential** distance-primary eviction: when resident tiles exceed the cap, reset the farthest + stalest
/// (not seen this frame) tiles back to `Free` — score = `distance · (1 + EVICT_STALENESS_FACTOR · staleness)`
/// via `select_nth_unstable_by`, NOT pure LRU. The decoded mesh is **deposited into [`FlashCache`]** (not dropped) so a
/// camera returning to the tile restores it without re-fetch/-decode; the slot returns to the free-list.
/// **No despawn** (no structural change). Runs after the cheap working-set pass + `decode`.
// Bevy system params (Res/ResMut/Query) count as fn args to clippy; the +2 over the limit are the working-set
// resources `ResidentSet`/`LiveSet`. Splitting them into a struct param would only obscure the system signature.
#[allow(clippy::too_many_arguments)]
fn evict(
    view: Res<GeoView>,
    cap: Res<MaxResident>,
    publish: Res<PublishSet>,
    mut free: ResMut<FreeList>,
    mut stats: ResMut<GeoStats>,
    mut cache: ResMut<FlashCache>,
    mut resident_set: ResMut<ResidentSet>,
    mut live: ResMut<LiveSet>,
    mut reclaim: ResMut<Reclaim>,
    mut tiles: Query<(
        Entity,
        &TileId,
        &TileBounds,
        &mut TileState,
        &LastSeen,
        &mut ResidentMesh,
        &mut MultiContent,
    )>,
) {
    // `decode` (now sequential) and the restore paths insert into `ResidentSet` directly, so by the time evict
    // runs it already holds this step's arrivals — the count + candidate scan run over the resident WORKING SET
    // (O(resident)), never the whole pre-spawned pool (O(pool)).
    let resident = resident_set.0.len();
    // Drift guard (debug only — compiled out in release, so the O(pool) scan is paid for safety, not speed):
    // the set MUST equal the actual `Resident` tiles. A missed transition (decode/restore/evict/prune) fails
    // tests here instead of silently dropping renders (publish) or mis-evicting in release.
    debug_assert_eq!(
        resident,
        tiles.iter().filter(|t| *t.3 == TileState::Resident).count(),
        "ResidentSet drifted from the actual Resident tiles — a Resident transition wasn't recorded"
    );
    stats.resident = resident as u32;
    stats.evicted_this_step = 0;
    if resident <= cap.0 {
        return;
    }
    // Over cap → collect eviction candidates, iterating ONLY the resident set. The SPARE-SET is the live
    // selection when one exists ([`PublishSet`] — the current cut + prepares, the tiles that must not
    // vanish), and the legacy in-range LRU stamp otherwise (foundation/selection-less worlds, where
    // `sse_cull_lru` still stamps). The stamp test alone was the 2026-06-11 starvation stall: at a
    // planet-scale cull distance EVERYTHING was stamped every step, so this list was always empty, resident
    // blew through the cap until the whole pool was Resident, and every admission was silently dropped.
    // Each candidate carries its camera distance (ECEF m) + score = distance·(1 + FACTOR·staleness) —
    // evict the FARTHEST + STALEST first (distance-primary; `EVICT_STALENESS_FACTOR`), not merely the oldest.
    let published: HashSet<Entity> = if publish.active {
        publish.entities.iter().copied().collect()
    } else {
        HashSet::new()
    };
    let cam = view.camera_ecef;
    let mut candidates: Vec<(Entity, u64, f32, f32)> = Vec::new(); // (entity, last_seen, distance_m, score)
    for &e in resident_set.0.iter() {
        if let Ok((_, _, bounds, state, seen, _, _)) = tiles.get(e) {
            // Spare the published cut + THIS step's arrivals (stamped by `decode`/the restore paths — they
            // were demanded by the current selection but join `PublishSet` only at the next full step).
            let spared = if publish.active {
                published.contains(&e) || seen.0 == view.frame
            } else {
                seen.0 == view.frame
            };
            if *state == TileState::Resident && !spared {
                let dist = (cam - bounds.center).length() as f32;
                let stale = view.frame.saturating_sub(seen.0) as f32;
                let score = dist * (1.0 + EVICT_STALENESS_FACTOR * stale);
                candidates.push((e, seen.0, dist, score));
            }
        }
    }
    let overflow = resident - cap.0;
    let n = overflow.min(candidates.len());
    if n == 0 {
        return;
    }
    // Partition so the `n` highest-score (farthest+stalest) are first (O(len) average, no full sort).
    if n < candidates.len() {
        candidates.select_nth_unstable_by(n - 1, |a, b| b.3.partial_cmp(&a.3).unwrap_or(std::cmp::Ordering::Equal));
    }
    for (e, ls, dist, _) in candidates.into_iter().take(n) {
        if let Ok((_, id, _, mut state, _, mut mesh, mut extra)) = tiles.get_mut(e) {
            *state = TileState::Free;
            // Deposit the decoded mesh in the LRU instead of dropping it — the warm re-entry path. The `Arc`
            // also keeps any published frame's copy alive; the cache merely holds one more reference. Pass the
            // distance so the cache's OWN trim can evict far tiles first too.
            if let Some(geom) = mesh.0.take() {
                cache.insert(id.0, geom, ls, dist);
            }
            // 1.1 multiple-contents: the LRU caches only content[0]; drop the additional contents (re-fetched on
            // a warm restore). No-op for the single-content fast-path (this vec is always empty there).
            extra.0.clear();
            free.0.push(e);
            resident_set.0.remove(&e); // Resident→Free: drop from BOTH working sets
            live.0.remove(&e); // ADR-0018: evict bypasses `free_slot`, so maintain `LiveSet` here
            reclaim.evicted.push(e); // hand the entity to reconcile_index (O(Δ) index reclamation)
            stats.evicted_this_step += 1;
            stats.evicted_total += 1;
        }
    }
    cache.trim(view.frame); // amortized: only sorts/drops when the batch pushed the cache over budget
                            // Report the resident count AFTER this step's eviction (the live working set).
    stats.resident = resident as u32 - stats.evicted_this_step;
}

/// **Sequential** publish: snapshot this step's DRAW SET into the render frame the stream thread polls —
/// geometry pinned via `Arc`, keyed by the stable `TileId`. With a live selection ([`PublishSet::active`])
/// the draw set is exactly the selection's render + prepare tiles, O(selected); the fallback (foundation /
/// schedule-only worlds with no selection) is the old in-range-`Resident` snapshot (`LastSeen == frame`).
/// **Why selection-driven (PIE 2026-06-09):** `sse_cull_lru` LRU-stamps EVERY in-range live tile (eviction
/// warmth), so gating publish on the stamp also emitted STALE finer tiles after the cut coarsened — resident,
/// in range, but absent from the selection, so the hole-clip pass had no pairs for them and they z-fought the
/// coarser cut as persistent sharp/blur patchwork. Drawing exactly the selection keeps the published set ≡
/// the clip-consistent set (retention is unchanged — stale tiles stay RESIDENT for instant re-entry; they
/// just don't draw). Reuses the mailbox's `Vec` (clear + push) so the snapshot does no per-step allocation;
/// the hole-clip pass after it allocates only on the (rare) steps where any stand-in is clipping.
// Bevy system params count as fn args to clippy; the +1 over the limit is the `PublishSet` draw-set resource
// (the selection-driven publish gate). Same dispensation as `evict`/`decode`.
#[allow(clippy::type_complexity, clippy::too_many_arguments)]
fn publish(
    view: Res<GeoView>,
    enu: Res<GeoEnu>,
    clip: Res<ClipChildren>,
    publish_set: Res<PublishSet>,
    mut mailbox: ResMut<GeoMailbox>,
    mut memo: ResMut<ClipBoxMemo>,
    resident_set: Res<ResidentSet>,
    textured: Res<TexturedSet>,
    tiles: Query<(
        &TileState,
        &TileId,
        &LastSeen,
        &ResidentMesh,
        &MultiContent,
        &Depth,
        &GeometricError,
    )>,
) {
    // Build into a FRESH frame (the mailbox shares the previous one as an `Arc` — it must stay immutable for
    // whoever still holds it). Capacity from the previous frame keeps the per-step allocation a single
    // right-sized Vec; with the C9 fingerprint + event-gated stepping, identical-content rebuilds are rare.
    let mut frame = GeoRenderFrame {
        frame: view.frame,
        origin_ecef: enu.0.origin_ecef.to_array(),
        tiles: Vec::with_capacity(mailbox.frame.tiles.len() + 8),
    };
    let frame = &mut frame;
    // Stage 1: snapshot the draw set — the selection's entities when active (O(selected)), else the in-range
    // residents (O(resident); never O(pool)). (`tiles.get` confirms live state — a set is the candidate
    // list, not the truth; the stamp check applies only to the fallback, selection already chose its set.)
    let require_stamp = !publish_set.active;
    let mut emit = |e: Entity| {
        if let Ok((state, id, seen, mesh, extra, depth, ge)) = tiles.get(e) {
            if *state == TileState::Resident && (!require_stamp || seen.0 == view.frame) {
                // `depth`/`geometric_error` ride along for the Geo debug-viz + telemetry tools (white→red
                // quality heatmaps); they don't affect what or how the renderer draws.
                let geometric_error = ge.0 as f32;
                if let Some(geom) = &mesh.0 {
                    frame.tiles.push(GeoRenderTile {
                        tile_key: id.0,
                        geom: Arc::clone(geom),
                        depth: depth.0,
                        geometric_error,
                        // Textured ⇒ show now; resident-but-untextured ⇒ HIDDEN "prepare" tile (gated above).
                        visible: textured.0.contains(&id.0),
                        // Hole-clip boxes are materialized in the post-pass below (they need the OTHER published
                        // tiles' geometry). Empty for the vast majority of tiles.
                        clip_boxes: Vec::new(),
                    });
                }
                // 3D Tiles 1.1 multiple-contents: emit each ADDITIONAL content that has decoded (empty for the
                // single-content fast-path, so this inner loop is a zero-cost skip for every P3DT tile). Each
                // carries its own stable key so the renderer caches it as a distinct section of the same tile.
                // The parts share their parent tile's depth + geometric error.
                for part in &extra.0 {
                    if let Some(geom) = &part.mesh {
                        frame.tiles.push(GeoRenderTile {
                            tile_key: part.key,
                            geom: Arc::clone(geom),
                            depth: depth.0,
                            geometric_error,
                            visible: textured.0.contains(&part.key),
                            // Additional contents share the tile footprint; the hole-clip rides the main content
                            // (P3DT is single-content, so this is always empty in practice).
                            clip_boxes: Vec::new(),
                        });
                    }
                }
            }
        }
    };
    if publish_set.active {
        for &e in publish_set.entities.iter() {
            emit(e);
        }
    } else {
        for &e in resident_set.0.iter() {
            emit(e);
        }
    }
    // Stage 2: hole-clip materialization. For each published coarse stand-in with clip children ([`ClipChildren`]
    // — usually empty), resolve each child in THIS frame and take the tight axis-aligned bounds of its decoded
    // mesh ([`oriented_clip_box`] — the exact extent of the geometry the renderer draws, in the child's own
    // ground frame, so no conservative-inset hack and no tilt over-cut). A child that isn't published, or is
    // published HIDDEN (untextured "prepare" tile), contributes no box: the stand-in keeps covering ground
    // nothing else is drawing yet. Work is O(tiles) for the key index + O(vertices) over the few clip children
    // — and only runs on steps where any stand-in is clipping at all. The mutation is deferred past the child
    // reads (both sides borrow `frame.tiles`).
    if !clip.0.is_empty() {
        // The earth center as a render-space point — the anchor for each clip child's geocentric `up`
        // ([`oriented_clip_box`]). One ENU transform per step, only on steps where anything clips at all.
        // (`enu_pos_to_ue` rounds through f32 — ~64 cm at earth-radius magnitude, an ~1e-9 rad angular error
        // on a direction anchor 6 378 km away: irrelevant, and it keeps the ENU→UE convention single-homed.)
        let ec = logi_geo_mesh::enu_pos_to_ue(enu.0.ecef_to_local(DVec3::ZERO));
        let earth_center_render = glam::DVec3::new(ec[0] as f64, ec[1] as f64, ec[2] as f64);
        let index: HashMap<u64, usize> = frame.tiles.iter().enumerate().map(|(i, t)| (t.tile_key, i)).collect();
        let mut attach: Vec<(usize, Vec<RenderClipBox>)> = Vec::with_capacity(clip.0.len());
        for (parent_key, child_keys) in clip.0.iter() {
            let Some(&pi) = index.get(parent_key) else { continue };
            // The STAND-IN's surface deviates from the truth the child draws by up to its geometricError (the
            // 3D Tiles bound) — at orbit-level parents that is tens to hundreds of KILOMETERS, while the
            // memoized box hugs only the CHILD's surface. A box that thin lets the parent's curved sheet pass
            // above/below it inside the child's footprint and poke through (the first orbit-scale eyeball of
            // the partial-reveal clip, 2026-06-10: giant intersecting sheets + a parent/child checkerboard).
            // Thicken each attached box along its `up` axis by this parent's error (+50% slack + a 10 m floor)
            // — per ATTACH, not in the memo (one child can clip under parents of different errors). The slab
            // grows symmetrically and only over the child's footprint (e1/e2 are untouched), and the clip
            // material discards only THIS stand-in's own fragments, so a taller slab can never hole the ground.
            let up_pad_cm = (frame.tiles[pi].geometric_error as f64) * 100.0 * 1.5 + 1_000.0;
            // Resolve each (group, key) member to its memoized base box (UNPADDED — the pad is per-parent and
            // applied to the FINAL boxes below). Tilt-true oriented box, MEMOIZED per tile key (audit
            // 2026-06-10 §2): the box is a pure function of the immutable decoded mesh, so the O(V_child)
            // two-pass vertex scan runs ONCE per child, not per publish. `None` (degenerate) is memoized too.
            // Skipping a box is always safe (brief coarse-over-fine overlap, never a hole).
            let mut groups: HashMap<u8, Vec<RenderClipBox>> = HashMap::new();
            for &(group, ck) in child_keys.iter() {
                let Some(&ci) = index.get(&ck) else { continue };
                if !frame.tiles[ci].visible {
                    continue; // clip only where the child actually SHOWS
                }
                let entry = memo.0.entry(ck).or_insert_with(|| {
                    (
                        0,
                        oriented_clip_box(&frame.tiles[ci].geom.vertices, earth_center_render),
                    )
                });
                entry.0 = view.frame; // stamp: used this publish (the retain below keeps it)
                if let Some(b) = entry.1 {
                    groups.entry(group).or_default().push(b);
                }
            }
            // GROUP-UNION reduction (2026-06-11 fast-camera sheets): a skip-level cut can put dozens of
            // rendered descendants under one stand-in — far past the renderer's RENDER_CLIP_MAX, and the
            // unclipped surplus drew coarse-over-fine. Each direct-child GROUP's members tile contiguous
            // ground, so they reduce to ONE union box (density-guarded: a sparse union would clip ground
            // nothing redraws — holes — so it falls back to individual boxes). ≤4 quadtree / ≤8 octree
            // groups ⇒ unions alone can never overflow; mixed union+individual residue is capped below,
            // dropping the SMALLEST footprints (overdraw beats holes, and the residue is least-visible).
            let mut boxes: Vec<RenderClipBox> = Vec::new();
            for (_, members) in groups.drain() {
                match members.len() {
                    1 => boxes.extend(members),
                    _ => match union_clip_boxes(&members) {
                        Some(u) => boxes.push(u),
                        None => boxes.extend(members),
                    },
                }
            }
            if boxes.len() > RENDER_CLIP_MAX {
                let area = |b: &RenderClipBox| {
                    let v = |a: [f32; 3]| glam::DVec3::new(a[0] as f64, a[1] as f64, a[2] as f64);
                    v(b.half_axes[0]).length() * v(b.half_axes[1]).length()
                };
                boxes.sort_unstable_by(|a, b| area(b).total_cmp(&area(a)));
                boxes.truncate(RENDER_CLIP_MAX);
            }
            for b in &mut boxes {
                *b = extend_clip_box_up(*b, up_pad_cm);
            }
            if !boxes.is_empty() {
                attach.push((pi, boxes));
            }
        }
        for (pi, boxes) in attach {
            frame.tiles[pi].clip_boxes = boxes;
        }
    }
    // Drop memo entries not used this publish: the memo tracks only the LIVE clip set (tiny), and a child
    // re-entering later just recomputes once. Skipped when the memo is already empty (the common no-clip step).
    if !memo.0.is_empty() {
        memo.0.retain(|_, (stamp, _)| *stamp == view.frame);
    }
    // C9 (audit 2026-06-10): fingerprint the frame's CONTENT — tile keys + visibility + clip boxes, in emit
    // order. Identical content ⇒ keep the previous frame (and its id), so every downstream id-equality skip
    // (stream mirror, poll `Same`, the C++ reconcile + clip refresh) fires instead of reprocessing an
    // unchanged scene at retry/texture-wait cadence. FNV over the few load-bearing fields — O(R), no alloc.
    let mut fp: u64 = 0xCBF2_9CE4_8422_2325;
    let mut fold = |bytes: &[u8]| {
        for &b in bytes {
            fp ^= b as u64;
            fp = fp.wrapping_mul(0x0000_0100_0000_01B3);
        }
    };
    for t in &frame.tiles {
        fold(&t.tile_key.to_le_bytes());
        fold(&[t.visible as u8, t.clip_boxes.len() as u8]);
        for b in &t.clip_boxes {
            for c in b.center {
                fold(&c.to_le_bytes());
            }
        }
    }
    if fp == mailbox.fingerprint {
        return; // identical content — keep the previous frame + id (downstream Same-skips fire)
    }
    // (The default fingerprint is 0, which an FNV fold never produces in practice, so the FIRST publish —
    // even of an empty frame — always lands and stamps a non-zero frame id.)
    mailbox.fingerprint = fp;
    mailbox.frame = Arc::new(std::mem::take(frame));
}

// ---- The Geo World ---------------------------------------------------------------------------------

/// A `bevy_ecs` World of pre-spawned tile entities + the Geo schedule, owned by the stream thread. One
/// [`advance`](Self::advance) runs the schedule once (the heavy systems fan out over the shared
/// `ComputeTaskPool`). The tileset traversal ([`select_request`](Self::select_request)), fetch drain
/// ([`drain_raw`](Self::drain_raw)), and published-frame mailbox ([`GeoMailbox`] / [`publish`]) drive the
/// full streaming pipeline around it.
pub struct GeoWorld {
    world: World,
    schedule: Schedule,
    /// The sequential streaming machinery (tree + fetcher + node→entity index), driven by `drain_raw` +
    /// `select_request` BEFORE the schedule. `None` for the foundation [`new`](Self::new) (schedule-only,
    /// for tests / externally-driven entities); `Some` for [`from_tileset`](Self::from_tileset).
    streaming: Option<GeoStreaming>,
    /// Raised by [`set_textured_keys`](Self::set_textured_keys); consumed by the re-selection gate (audit
    /// 2026-06-10 §3.1) — a changed textured set is one of the events that makes the next advance a FULL step.
    textured_dirty: bool,
}

/// The **grafts cell**: external tilesets grafted into the tree. `records` membership marks "already
/// grafted, don't re-fetch"; each [`GraftRecord`] holds the placeholder's pre-graft state + last-active
/// step so [`GeoWorld::prune_cold_grafts`] can un-graft a cold subtree (the bound on this map + the tree +
/// `bases`, which otherwise only grow). `bases` carries the inherited auth query down each external
/// subtree (keyed by the graft root's node id; resolved by walking a tile's ancestor chain,
/// [`base_for_node`]).
#[derive(Default)]
struct Grafts {
    records: HashMap<u32, GraftRecord>,
    bases: HashMap<u32, String>,
}

/// The **failure-ledger cell**: per-node failure state, with the two failure modes honestly separated.
/// `decode_failed` is TERMINAL (decode is deterministic over the fetched bytes): membership makes
/// `select_request` skip the node permanently, freeing the pool slot without the re-fetch→re-fail
/// busy-loop. `fetch_attempts` counts consecutive TRANSIENT fetch failures (network, not decode) — the
/// tile is freed + re-selected + re-fetched until the count reaches [`MAX_FETCH_ATTEMPTS`]; a successful
/// fetch clears the entry. `next_retry` is the earliest wall-clock [`Instant`] at which a transient-failed
/// node may be re-selected (audit A1 backoff; H5 wall-clock): `select_request` skips it while
/// `now < next_retry[node]`, so a rate-limited (429/5xx) tile retries after a capped exponential delay
/// instead of every step. Cleared on success / give-up / prune.
#[derive(Default)]
struct FailureLedger {
    decode_failed: HashSet<u32>,
    fetch_attempts: HashMap<u32, u32>,
    next_retry: HashMap<u32, Instant>,
}

/// The **subtree-streams cell** (audit S1 — implicit tiling): the `.subtree` availability lane, whole.
/// Subtrees aren't tiles (no entity / pool slot), so their full lifecycle — decoded cache, in-flight
/// fetches, terminal failures — lives in this one cell, linked to the rest of the world only by
/// [`RequestId`]s. `cache` maps resolved subtree URL → availability bitstreams (the
/// [`SubtreeAvailability`] provider answers the implicit traversal from it; a miss enqueues a fetch);
/// `cache_order` is the FIFO insertion order bounding it past [`MAX_SUBTREE_CACHE`]
/// ([`cache_subtree`](Self::cache_subtree)). `in_flight` maps RequestId → (resolved URL, child_count) so
/// `drain_raw` decodes + caches the result; `in_flight_urls` mirrors its values for O(1) dedup in
/// `select_request` (instead of an O(in-flight) linear value scan). `failed` holds terminally-failed
/// (404 / malformed) URLs — answered `Unavailable` (prune the region) instead of re-pending forever;
/// transient fetch failures are simply re-fetched next walk.
#[derive(Default)]
struct SubtreeStreams {
    cache: HashMap<String, Subtree>,
    cache_order: VecDeque<String>,
    in_flight: HashMap<RequestId, (String, u32)>,
    in_flight_urls: HashSet<String>,
    failed: HashSet<String>,
}

impl SubtreeStreams {
    /// Cache a decoded `.subtree`, FIFO-evicting the oldest past [`MAX_SUBTREE_CACHE`] so the implicit-tiling
    /// cache stays bounded over a long fly-through. The current view's subtrees were inserted most recently, so
    /// they survive; only old far-away regions are dropped (re-fetched if the camera revisits them).
    fn cache_subtree(&mut self, url: String, subtree: Subtree) {
        // Only fetched-on-a-miss URLs reach here, so a key is new (insert returns `None`) → record its order.
        if self.cache.insert(url.clone(), subtree).is_none() {
            self.cache_order.push_back(url);
        }
        while self.cache.len() > MAX_SUBTREE_CACHE {
            let Some(old) = self.cache_order.pop_front() else {
                break;
            };
            self.cache.remove(&old);
        }
    }

    /// Record a terminally-failed `.subtree` URL, clearing the set wholesale past [`MAX_SUBTREE_FAILED`]
    /// (self-healing — a still-absent subtree simply re-fails and re-adds, paced by the in-flight cap).
    fn mark_failed(&mut self, url: String) {
        if self.failed.len() >= MAX_SUBTREE_FAILED {
            self.failed.clear();
        }
        self.failed.insert(url);
    }
}

/// The **prefetch cell** (audit M4): dt-free displacement prediction. `prev_camera_ecef` is the camera on
/// the PREVIOUS advance (`None` until the second) — the displacement `camera_ecef - prev` is the motion
/// vector a lookahead traversal prefetches along (the render lane needs no determinism — owner's call — so
/// no wall-clock velocity). `live_nodes` are the nodes with a LIVE speculative fetch (audit recheck M-1):
/// `cancel_stale` keeps these alive even though they sit OUTSIDE the reactive `sel.load`, so a prefetched
/// tile isn't cancelled before it can complete + warm the disk cache — the whole point of M4. An entry
/// lives for its fetch's whole flight (aged by `retain(in_flight_nodes.contains)` each prefetch pass — NOT
/// cleared wholesale, which de-protected prefetches in ~2 advances, before any real RTT; re-eval
/// 2026-06-09); a stale speculation still in flight is reclaimed by H6 preemption when a reactive tile
/// needs the slot. `steps_since_run` counts full steps since the last prefetch pass (audit 2026-06-10 §2):
/// the lookahead is a SECOND full traversal whose 8×-displacement target moves slowly relative to step
/// cadence — running it every [`PREFETCH_INTERVAL_STEPS`]th full step warms the same tiles at a fraction
/// of the traversal cost.
#[derive(Default)]
struct Prefetcher {
    prev_camera_ecef: Option<DVec3>,
    live_nodes: HashSet<u32>,
    steps_since_run: u32,
}

/// The **session-lane cell**: the Google P3DT `?session=…` lifecycle. `refresh_pending` is set when a
/// tile/subtree fetch returns 401/403 (an expired session) and consumed by
/// [`GeoWorld::refresh_session_if_needed`], which re-fetches the root tileset for a fresh session.
/// `generation` counts APPLIED refreshes ([`GeoWorld::apply_refreshed_root`]); paired with `last_4xx_gen`
/// (node → the generation in which it last failed with a session-presumed 4xx) it gives a 4xx-on-session-URL
/// exactly ONE refresh-and-retry: a path that fails again in a LATER generation has now failed WITH a fresh
/// session — genuinely dead, terminal lane. Without the generation check, a single persistently-404ing path
/// (genuinely-absent content, not an expired session) re-flagged a refresh every cooldown, and each
/// `apply_refreshed_root` cleared grafts/bases/LastSeen wholesale — the whole region perpetually re-fetched +
/// re-evicted (the 2026-06-11 rio wedge). `last_4xx_gen` is deliberately NOT cleared by
/// `apply_refreshed_root` (that survival is the whole mechanism); bounded in
/// [`GeoWorld::bound_failure_sets`]; entry removed on fetch success alongside `fetch_attempts`.
/// `refresh_in_flight` is the in-flight ASYNC root re-fetch's request id (M1b — it rides the worker pool,
/// not a blocking `http_get`), gating re-enqueue to one at a time; `last_refresh_at` gates the next refresh
/// by [`SESSION_REFRESH_COOLDOWN`] so a 401 storm triggers ONE root re-fetch (audit H5 wall-clock).
#[derive(Default)]
struct SessionLane {
    refresh_pending: bool,
    generation: u32,
    last_4xx_gen: HashMap<u32, u32>,
    refresh_in_flight: Option<RequestId>,
    last_refresh_at: Option<Instant>,
}

/// The growing 3D-Tiles tree (external `.json` tilesets graft into it as the camera descends) + the async
/// fetch source + the path→entity index, with the per-concern state grouped into sub-cells ([`Grafts`],
/// [`FailureLedger`], [`SubtreeStreams`], [`Prefetcher`], [`SessionLane`]) whose invariants live with their
/// fields. Owned by the stream thread; touched only by the sequential `drain_raw`/`select_request` methods
/// (the tree-walk is pointer-chasing — inherently sequential).
struct GeoStreaming {
    base_url: String,
    ellipsoid: logi_geospatial::Ellipsoid,
    root: Tile,
    /// The path interner (audit 2026-06-10 §3.2): every per-tile map below keys on its `u32` node ids — O(1)
    /// probes, no per-segment byte hashing, no `Vec` clones on the hot per-pass paths. Owned here so node ids
    /// are stable for the world's lifetime (the cross-frame identities `sel_state` + `index` rely on).
    arena: PathArena,
    /// The grafted-external-tilesets cell — records + per-graft-root base URLs ([`Grafts`]).
    grafts: Grafts,
    source: Box<dyn TileContentSource + Send>,
    in_flight: HashMap<RequestId, InFlight>,
    in_flight_nodes: HashSet<u32>,
    next_id: RequestId,
    max_in_flight: usize,
    max_tile_distance: f64,
    /// node → the pool entity backing that tile (in-flight or resident). The reverse of an entity's
    /// [`TileNode`].
    index: HashMap<u32, Entity>,
    /// The per-node failure-state cell — terminal decode failures, transient fetch attempts, and the
    /// wall-clock backoff deadlines ([`FailureLedger`]).
    failures: FailureLedger,
    /// The implicit-tiling `.subtree` lane cell — decoded-availability cache + in-flight fetches + terminal
    /// failures ([`SubtreeStreams`]).
    subtrees: SubtreeStreams,
    /// The **previous** `select_request`'s per-tile [`SelectionResult`] — the cross-frame traversal state fed
    /// back as `select_tree_implicit`'s `prev_state`. It drives BOTH the hysteresis dead-band (a tile `Refined` last
    /// frame holds its split, [`LOD_HYSTERESIS`]) AND the anti-pop **kick** (a fresh, not-yet-loaded
    /// refinement renders the coarse ancestor instead of holes, switching to fine atomically once loaded).
    sel_state: SelectionState,
    /// Camera **forward** direction in ECEF (unit; `ZERO` = unknown ⇒ no view-direction weighting). Set per
    /// frame by [`advance_from_ue`](GeoWorld::advance_from_ue); read by `select_request`'s fetch-priority sort
    /// (plan §9.2 step 7) so tiles in the look direction load first. Lives here (not on the per-frame
    /// `GeoView`) because only the streaming selector needs it — the schedule systems cull by distance.
    cam_dir_ecef: DVec3,
    /// The predictive-prefetch cell — previous camera, protected live speculative fetches, and the pacing
    /// counter ([`Prefetcher`]; populated by [`prefetch_along_motion`]).
    prefetch: Prefetcher,
    /// The camera view frustum in ECEF, rebuilt each frame by [`advance_from_ue`](GeoWorld::advance_from_ue)
    /// (`Default` = accept-all, for the headless `advance`/`advance_streaming` test path). `select_request`
    /// classifies each tile against it: in-view → full SSE, off-view-but-in-range → coarse `culled` SSE
    /// (frustum-aware LOD). Reduces fetch/decode by streaming a coarse shell for what the camera isn't facing.
    frustum: ViewFrustum,
    /// The Google P3DT session-lifecycle cell — refresh trigger/cooldown clockwork + the 4xx generation
    /// ledger ([`SessionLane`]).
    session: SessionLane,
    /// The current step's wall-clock, stamped by [`GeoWorld::set_now`] before each advance (the stream loop
    /// passes `Instant::now()`; the headless backoff tests pass a controlled clock). The single time source the
    /// retry-backoff + session-refresh deadlines read — keeps them off `frame` ticks (audit H5), whose cadence
    /// varies with the loop's park (1 ms busy vs 8 ms idle) and so couldn't time a wall-clock backoff.
    now_instant: Instant,
    /// The view of the last FULL step (camera/fov/viewport/sse/cull — `frame` excluded), the re-selection
    /// gate's view-change trigger (audit 2026-06-10 §3.1). `None` until the first full step.
    last_full_view: Option<GeoView>,
    /// The last `advance_from_ue` orientation inputs (camera up, aspect) — with `cam_dir_ecef`, the
    /// orientation half of the view-change trigger (`GeoView` itself carries no orientation).
    last_ue_orient: Option<(DVec3, f64)>,
    /// Set at the end of a full step that left follow-up work no external event will announce — evictions or
    /// decode failures (freed slots → restorable/refetchable tiles), or a fetch backlog (the ranked loop was
    /// budget/cap/pool-limited with candidates remaining). Forces the NEXT advance to be a full step too, so
    /// the pipeline keeps converging; cleared once consumed.
    force_full_next: bool,
}

/// One outstanding fetch: enough to route the result back to the tree (tileset graft) or the entity (GLB).
struct InFlight {
    /// The tile's interned [`PathArena`] node id (the path, when a graft needs to walk the tree, is
    /// reconstructed via `arena.path_of` — a rare op).
    node: u32,
    kind: ContentKind,
    /// The fully-resolved fetch URL — recorded so a grafted subtree inherits it as its base (P3DT auth).
    /// (The GLB tile's world transform lives on its entity's `TileTransform`, set at request time.)
    url: String,
    /// The GLB tile's entity (`None` for an external tileset — it grafts into the tree, no entity).
    entity: Option<Entity>,
    /// 3D Tiles 1.1 multiple-contents: which content slot of the tile this fetch is. `0` = the primary content
    /// (owns the pool slot + the path's index/in-flight/retry bookkeeping — the single-content fast-path). `>0`
    /// = an additional content riding the primary's entity: it fills a [`MultiContent`] part and must NOT free
    /// the slot or touch the path's retry state (the primary owns those). Always `0` for a tileset graft.
    content_index: u32,
    /// The `fetch_priority` score this fetch was issued at (audit H6). Recorded so the in-flight cap can PREEMPT
    /// the lowest-priority outstanding fetch for a clearly-more-urgent newcomer (a reactive tile beats a stale
    /// or prefetched one) instead of merely blocking. A demoted prefetch carries a large-negative score, so any
    /// reactive tile out-ranks it; among reactive tiles a 1.5× margin protects an in-progress download.
    priority: f64,
}

/// The scene-side [`AvailabilityProvider`] (audit S1 — implicit tiling): answers the implicit traversal's
/// per-coordinate subtree-availability queries from the decoded-`.subtree` cache, and RECORDS the URLs of any
/// `.subtree` files it doesn't have yet so `select_request` can fetch them after the walk. The walk borrows
/// the tree (`&s.root`) immutably, so this borrows only the *other* streaming fields it needs (all immutable)
/// plus a scratch `needed` list — split-borrowed at the call site. Fetch-free + deterministic, per the seam.
struct SubtreeAvailability<'a> {
    /// Resolved `.subtree` URL → decoded availability bitstreams.
    cache: &'a HashMap<String, Subtree>,
    /// `.subtree` URLs that failed terminally (404 / malformed) → answered `Unavailable` (prune), not `Pending`.
    failed: &'a HashSet<String>,
    /// The implicit root's fetch base + the grafted-subtree bases (node-keyed; resolved by walking the
    /// implicit root's ancestor chain through the traversal-supplied arena).
    base_url: &'a str,
    bases: &'a HashMap<u32, String>,
    /// Cache misses recorded here as `(resolved subtree URL, child_count)` for the caller to enqueue.
    needed: &'a mut Vec<(String, u32)>,
}

/// The `.subtree` covering a global implicit coordinate, plus the breakdown needed to also locate that
/// subtree's PARENT subtree (for the R2 `childSubtreeAvailability` check).
struct Covering {
    url: String,
    child_count: u32,
    /// The covering subtree's root level — `(level / subtree_levels) * subtree_levels`. `0` = the root subtree.
    root_level: u32,
    /// The covering subtree's root coordinates (global, at `root_level`).
    sx: u32,
    sy: u32,
    sz: u32,
    /// The queried tile's coordinates LOCAL to the covering subtree (for `is_tile_available`).
    llevel: u32,
    lx: u32,
    ly: u32,
    lz: u32,
}

impl SubtreeAvailability<'_> {
    /// Map a global implicit coordinate to the [`Covering`] `.subtree`: the resolved URL + the tile's
    /// subtree-local coords (for the codec's bit lookup) + the covering subtree's root level/coords (so the
    /// parent subtree can be located). `None` if the subtree template won't resolve (the tile is then treated as
    /// `Unavailable`). The covering subtree's root level is `(level / subtree_levels) * subtree_levels`; the
    /// tile's local coords are the low `level - root_level` bits of its global coords.
    fn covering(
        &self,
        ctx: &ImplicitContext,
        arena: &PathArena,
        level: u32,
        x: u32,
        y: u32,
        z: u32,
    ) -> Option<Covering> {
        let st_levels = ctx.tiling.subtree_levels.max(1);
        let child_count = ctx.tiling.subdivision_scheme.child_count();
        let root_level = (level / st_levels) * st_levels;
        let down = level - root_level; // levels from the covering subtree's root down to this tile
        let (sx, sy, sz) = (x >> down, y >> down, z >> down); // subtree-root coords at root_level
        let (lx, ly, lz) = (x - (sx << down), y - (sy << down), z - (sz << down)); // local coords in subtree
        let template = ctx.tiling.subtrees.substitute(root_level, sx, sy, sz);
        let base = base_for_node(self.base_url, self.bases, arena, ctx.root_node);
        let url = resolve_url(base, &template).ok()?;
        Some(Covering {
            url,
            child_count,
            root_level,
            sx,
            sy,
            sz,
            llevel: down,
            lx,
            ly,
            lz,
        })
    }

    /// R2 — does the CHILD subtree `cov` actually EXIST, per its PARENT subtree's `childSubtreeAvailability`?
    /// `Some(true)` = declared present, `Some(false)` = declared absent (⇒ prune, don't fetch a 404), `None` =
    /// can't tell (this IS the root subtree, the bit width is degenerate, or the parent subtree isn't cached —
    /// fetch as before). Maps the child subtree's root to the parent-local coords the codec expects: the child
    /// root's low `subtree_levels` bits, indexed at the deepest+1 level with no level offset.
    fn child_subtree_present(&self, ctx: &ImplicitContext, arena: &PathArena, cov: &Covering) -> Option<bool> {
        let st_levels = ctx.tiling.subtree_levels.max(1);
        if cov.root_level < st_levels {
            return None; // the root subtree has no parent to consult — it always "exists"
        }
        let mask = 1u32.checked_shl(st_levels)?.checked_sub(1)?; // st_levels >= 32 ⇒ skip (fetch as before)
        let parent_root_level = cov.root_level - st_levels;
        let (psx, psy, psz) = (cov.sx >> st_levels, cov.sy >> st_levels, cov.sz >> st_levels);
        let (clx, cly, clz) = (cov.sx & mask, cov.sy & mask, cov.sz & mask);
        let parent_template = ctx.tiling.subtrees.substitute(parent_root_level, psx, psy, psz);
        let base = base_for_node(self.base_url, self.bases, arena, ctx.root_node);
        let parent_url = resolve_url(base, &parent_template).ok()?;
        let parent = self.cache.get(&parent_url)?; // parent not cached ⇒ can't tell ⇒ fetch as before
        Some(parent.is_child_subtree_available(clx, cly, clz))
    }
}

impl AvailabilityProvider for SubtreeAvailability<'_> {
    fn tile_availability(
        &mut self,
        ctx: &ImplicitContext,
        arena: &PathArena,
        level: u32,
        x: u32,
        y: u32,
        z: u32,
    ) -> TileAvailability {
        let Some(cov) = self.covering(ctx, arena, level, x, y, z) else {
            return TileAvailability::Unavailable; // template won't resolve → this tile can't exist
        };
        if self.failed.contains(&cov.url) {
            return TileAvailability::Unavailable; // terminal subtree → prune (don't re-pend forever)
        }
        match self.cache.get(&cov.url) {
            Some(subtree) => {
                if subtree.is_tile_available(cov.llevel, cov.lx, cov.ly, cov.lz) {
                    TileAvailability::Available
                } else {
                    TileAvailability::Unavailable
                }
            }
            None => {
                // R2 — childSubtreeAvailability: if this covering subtree is a CHILD subtree, ask its PARENT
                // subtree whether it EXISTS before fetching. A non-existent child subtree is pruned
                // (`Unavailable`) instead of fetched as a 404 that holds the region coarse for the round trip and
                // fills `subtree_failed`. Parent is cached (we descended through it); `None` ⇒ can't tell ⇒ fetch.
                if let Some(false) = self.child_subtree_present(ctx, arena, &cov) {
                    return TileAvailability::Unavailable;
                }
                // Not fetched yet: record it (deduped) and hold the coarse ancestor until it arrives.
                if !self.needed.iter().any(|(u, _)| u == &cov.url) {
                    self.needed.push((cov.url, cov.child_count));
                }
                TileAvailability::Pending
            }
        }
    }

    fn content_availability(
        &mut self,
        ctx: &ImplicitContext,
        arena: &PathArena,
        level: u32,
        x: u32,
        y: u32,
        z: u32,
    ) -> bool {
        // Only called for already-`Available` tiles, so the covering subtree is cached. Content index 0 (P3DT
        // tiles carry a single content); a tile may exist as a pure interior node with no content.
        let Some(cov) = self.covering(ctx, arena, level, x, y, z) else {
            return false;
        };
        self.cache
            .get(&cov.url)
            .map(|st| st.is_content_available(0, cov.llevel, cov.lx, cov.ly, cov.lz))
            .unwrap_or(false)
    }
}

/// Worker-thread budget for the shared `ComputeTaskPool` — mirrors `logi_sim::resolve_worker_thread_count`
/// (kept in lockstep here rather than coupling the geo tower to the gameplay tower, so the formula is
/// intentionally duplicated across the two crates as it already was): clamp to
/// `max(1, physical_cores - 3)` so the pool never grabs all cores and starves Unreal's Game / Render / RHI
/// threads. Sizing off PHYSICAL cores (not logical/SMT) avoids piling compute workers onto a core's sibling
/// hyperthread — two HT siblings share one set of execution units, so the second adds contention, not
/// throughput, on the cache-/SIMD-bound decode path. Geo and gameplay share ONE global pool via
/// `get_or_init`; both build it BOUNDED, so whichever initializes first, Unreal keeps its headroom.
fn geo_compute_threads() -> usize {
    // Reserve three cores for Unreal's Game / Render / RHI threads. (NB: UE 5.8 itself sizes its TaskGraph
    // off LOGICAL−2 — `NumberOfWorkerThreadsToSpawn`, source-verified 2026-06-09 — so this reserve is about
    // keeping the named threads' cores uncontested, not about matching UE's own worker formula.)
    const UNREAL_RESERVED_THREADS: usize = 3;
    // Physical-core count. On Windows `num_cpus::get_physical()` never returns 0 (it falls back to the
    // logical count internally — source-verified 2026-06-09), so the guard below is cross-platform
    // belt-and-braces for non-Windows exotica, not a Windows concern.
    let mut physical = num_cpus::get_physical();
    if physical == 0 {
        physical = (std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4) / 4).max(1);
    }
    physical.saturating_sub(UNREAL_RESERVED_THREADS).max(1)
}

impl GeoWorld {
    /// Pre-spawn `pool_size` tile entities (all `Free`, one uniform archetype), build the schedule, and
    /// ensure the **shared**, BOUNDED global `ComputeTaskPool` exists (joins the gameplay pool if it ran first).
    pub fn new(pool_size: usize, enu: EnuFrame, decoder: TileDecoder, max_resident: usize) -> Self {
        // Share the one global ComputeTaskPool (research §10), BOUNDED so geo never oversubscribes Unreal —
        // `get_or_init` joins gameplay's pool if it ran first; both towers build it bounded (no more race to
        // an unbounded pool). The Geo schedule now runs SingleThreaded (ADR-0018), so this pool backs the
        // SHARED gameplay executor + the measure-gated decode decouple (dec. 5), not a per-step Geo `par_iter`.
        ComputeTaskPool::get_or_init(|| {
            TaskPoolBuilder::new()
                .num_threads(geo_compute_threads())
                .thread_name("LogiBevyCompute".to_string())
                .build()
        });

        let mut world = World::new();
        world.insert_resource(GeoView::default());
        world.insert_resource(GeoEnu(enu));
        world.insert_resource(decoder);
        world.insert_resource(MaxResident(max_resident));
        world.insert_resource(GeoStats::default());
        world.insert_resource(GeoMailbox::default());
        world.insert_resource(TexturedSet::default());
        world.insert_resource(FlashCache::new(DEFAULT_FLASH_CACHE_BYTES));
        world.insert_resource(GeomCacheRes::default());

        let mut free = Vec::with_capacity(pool_size);
        for _ in 0..pool_size {
            let e = world
                .spawn((
                    TileId(0),
                    TileNode(NO_NODE),
                    TileBounds {
                        center: DVec3::ZERO,
                        radius: 0.0,
                    },
                    GeometricError(0.0),
                    Depth(0),
                    TileTransform(DMat4::IDENTITY),
                    TileState::Free,
                    LastSeen(0),
                    FetchedBytes::default(),
                    ResidentMesh::default(),
                    MultiContent::default(),
                ))
                .id();
            free.push(e);
        }
        world.insert_resource(FreeList(free));
        world.insert_resource(ResidentSet::default());
        world.insert_resource(ClipChildren::default());
        world.insert_resource(ClipBoxMemo::default());
        world.insert_resource(PublishSet::default());
        world.insert_resource(LiveSet::default());
        world.insert_resource(JustFetched::default());
        world.insert_resource(Reclaim::default());

        let mut schedule = Schedule::default();
        // ADR-0018: the schedule is fully `.chain()`ed (every system a barrier), so bevy's multi-threaded
        // executor is pure per-step overhead here (round-4 Exp C: 8.4µs vs 1.0µs at ~3 systems) with no
        // concurrency to exploit — run it SingleThreaded. Cheap working-set pass → decode this step's arrivals
        // → evict over the resident set → publish the visible set. (The sequential tileset traversal + fetch
        // drain run as `GeoWorld` methods before the schedule — they mutate the tree/io and acquire entities.)
        schedule.set_executor(SingleThreadedExecutor::new());
        schedule.add_systems((sse_cull_lru, decode, evict, publish).chain());

        Self {
            world,
            schedule,
            streaming: None,
            textured_dirty: false,
        }
    }

    /// Advance one step against `view`: run the schedule once.
    pub fn advance(&mut self, view: GeoView) {
        self.world.insert_resource(view);
        self.schedule.run(&mut self.world);
    }

    /// Mirror the renderer-reported textured-tile set into the world. The stream thread calls this before
    /// `advance_from_ue` (only when the set changed), so the selector's `is_loaded` gate and `publish` see the
    /// latest textures. Cheap move; the set persists across advances until the next change. Raises the
    /// re-selection trigger (audit §3.1): a changed gate input means the next advance must be a FULL step.
    pub fn set_textured_keys(&mut self, keys: HashSet<u64>) {
        self.world.resource_mut::<TexturedSet>().0 = keys;
        self.textured_dirty = true;
    }

    /// Build a **streaming** Geo World from the root tileset bytes + its base URL, centred on `origin`. The
    /// `source` is the already-constructed fetch source — `HttpSource` in production, `SyntheticSource` in
    /// tests. Pre-spawns `pool_size` tile entities and wires the production glTF decoder.
    #[allow(clippy::too_many_arguments)]
    pub fn from_tileset(
        base_url: String,
        root_tileset_bytes: &[u8],
        origin: Cartographic,
        source: Box<dyn TileContentSource + Send>,
        max_tile_distance: f64,
        pool_size: usize,
        max_resident: usize,
    ) -> Result<Self, StreamError> {
        let ellipsoid = logi_geospatial::Ellipsoid::wgs84();
        let enu = EnuFrame::from_cartographic(&ellipsoid, origin);
        let decoder = production_decoder(enu);
        Self::from_tileset_with_decoder(
            base_url,
            root_tileset_bytes,
            enu,
            decoder,
            source,
            max_tile_distance,
            pool_size,
            max_resident,
        )
    }

    /// Streaming-world assembly shared by [`from_tileset`](Self::from_tileset) (production: the real glTF
    /// decoder) and tests (a stub decoder, so the resident/evict/restore cycle is exercised headlessly without
    /// crafting valid GLB bytes). Parses the root tileset, pre-spawns the pool, and wires the streaming state.
    #[allow(clippy::too_many_arguments)]
    fn from_tileset_with_decoder(
        base_url: String,
        root_tileset_bytes: &[u8],
        enu: EnuFrame,
        decoder: TileDecoder,
        source: Box<dyn TileContentSource + Send>,
        max_tile_distance: f64,
        pool_size: usize,
        max_resident: usize,
    ) -> Result<Self, StreamError> {
        let tileset = parse_tileset(root_tileset_bytes).map_err(|e| StreamError::Tileset(e.to_string()))?;
        // Spec compliance: a client MUST refuse a tileset that REQUIRES an extension it can't honor, rather than
        // silently rendering garbage/holes. Fail loud on the first unsupported required extension.
        if let Some(unsupported) = tileset
            .extensions_required
            .iter()
            .find(|e| !SUPPORTED_TILESET_EXTENSIONS.contains(&e.as_str()))
        {
            return Err(StreamError::Tileset(format!(
                "tileset requires unsupported extension '{unsupported}' (this stack supports {SUPPORTED_TILESET_EXTENSIONS:?})"
            )));
        }
        let ellipsoid = logi_geospatial::Ellipsoid::wgs84();
        let mut world = Self::new(pool_size, enu, decoder, max_resident);
        // Couple the outstanding-fetch cap to the source's worker count (audit H2): `fetch_concurrency · K`
        // tracks the one knob that governs download parallelism, vs a hardcoded 128 that, at 16 workers, was
        // ~112 slots of pure FIFO backlog. The synthetic/test source reports 32 ⇒ 128 (unchanged); HttpSource
        // reports its worker count ⇒ 16→64, 32→128, 64→256.
        let max_in_flight = source.fetch_concurrency().saturating_mul(IN_FLIGHT_PER_WORKER).max(1);
        world.streaming = Some(GeoStreaming {
            base_url,
            ellipsoid,
            root: tileset.root,
            arena: PathArena::new(),
            grafts: Grafts::default(),
            source,
            in_flight: HashMap::new(),
            in_flight_nodes: HashSet::new(),
            next_id: 1,
            max_in_flight,
            max_tile_distance,
            index: HashMap::new(),
            failures: FailureLedger::default(),
            subtrees: SubtreeStreams::default(),
            sel_state: SelectionState::new(),
            cam_dir_ecef: DVec3::ZERO,
            prefetch: Prefetcher::default(),
            frustum: ViewFrustum::default(),
            session: SessionLane::default(),
            now_instant: Instant::now(),
            last_full_view: None,
            last_ue_orient: None,
            force_full_next: false,
        });
        Ok(world)
    }

    /// Resolve a Cesium **ion** asset → fetch its root tileset → build a streaming Geo World whose tile fetch
    /// runs on the [`backend`](FetchBackend) the caller selects (`Auto` ⇒ the HTTP/3-over-QUIC [`CyperSource`]
    /// for Google P3DT, else the `ureq` [`HttpSource`] pool of `workers` threads). **Blocking** (ion resolve + root
    /// fetch) — call off the render thread (the FFI does, on the create path). `cache` is the optional
    /// persistent tile cache; `pool_size`/`max_resident` size the entity pool + resident cap.
    #[allow(clippy::too_many_arguments)]
    pub fn from_ion(
        asset_id: u64,
        token: &str,
        longitude_deg: f64,
        latitude_deg: f64,
        height_m: f64,
        workers: usize,
        backend: FetchBackend,
        max_tile_distance: f64,
        cache: Option<Arc<DiskCache>>,
        pool_size: usize,
        max_resident: usize,
    ) -> Result<Self, StreamError> {
        let asset = logi_3dtiles_ion::resolve_asset(asset_id, token)
            .map_err(|e| StreamError::Tileset(format!("ion resolve: {e:?}")))?;
        let root_bytes = http_get(&asset.tileset_url).map_err(|_e| {
            // Keep the (possibly secret-bearing) query out of the error text/logs — report only the path.
            let safe_url = asset.tileset_url.split('?').next().unwrap_or("<tileset>");
            StreamError::Tileset(format!("root tileset fetch failed: {safe_url}"))
        })?;
        let origin = Cartographic::from_degrees(longitude_deg, latitude_deg, height_m);
        // Fetch-backend selection (ADR-0014 I/O lane), driven by the runtime `backend` knob (FFI
        // `fetch_backend`). The `CyperSource` lane (a `cyper::Client` on one owned compio thread) negotiates
        // **HTTP/3 over QUIC** (compio-quic = quinn-proto on the compio reactor) for Google P3DT via Alt-Svc
        // auto-upgrade, with HTTP/2 as the bootstrap + fallback; it is the DEFAULT for Google tiles. The
        // blocking `ureq` `HttpSource` pool stays the lane for other providers and the forced-ureq A/B arm.
        // `Auto` ⇒ the Cyper lane when the host is Google P3DT (and the `cyper-h2` feature is compiled), else
        // ureq; `Cyper`/`Ureq` force a backend. Google P3DT resolves to a `tile.googleapis.com` root (see
        // `logi_3dtiles_ion`); detect it by host so other ion-direct providers (assets.cesium.com) keep the
        // conservative ureq default under `Auto`.
        let is_google = asset.tileset_url.contains("googleapis.com");
        let want_cyper = match backend {
            FetchBackend::Auto => is_google,
            FetchBackend::Cyper => true,
            FetchBackend::Ureq => false,
        };
        // `CyperSource::new` can fail (its runtime thread may not build a compio runtime); degrade GRACEFULLY to
        // `HttpSource` rather than ever failing world creation on a non-fatal hiccup. Both share the SAME
        // `DiskCache` (`workers` sizes only the ureq pool — the cyper lane multiplexes onto QUIC/h2 streams over
        // a few connections, not a thread count).
        #[cfg(feature = "cyper-h2")]
        let source: Box<dyn TileContentSource + Send> = if want_cyper {
            match logi_3dtiles_io::CyperSource::new(cache.clone()) {
                Ok(s) => {
                    tracing::info!("Geo fetch backend: CyperSource (HTTP/3 over QUIC via Alt-Svc; h2 fallback)");
                    Box::new(s)
                }
                Err(e) => {
                    tracing::warn!("CyperSource init failed ({e}); falling back to ureq HttpSource");
                    Box::new(HttpSource::new(workers, cache))
                }
            }
        } else {
            tracing::info!("Geo fetch backend: ureq HttpSource ({workers} workers)");
            Box::new(HttpSource::new(workers, cache))
        };
        #[cfg(not(feature = "cyper-h2"))]
        let source: Box<dyn TileContentSource + Send> = {
            if want_cyper {
                tracing::warn!(
                    "fetch backend cyper (HTTP/3/QUIC) requested but not compiled in; using ureq HttpSource \
                     (rebuild with default features / --features cyper-h2 to enable the QUIC lane)"
                );
            }
            Box::new(HttpSource::new(workers, cache))
        };
        Self::from_tileset(
            asset.tileset_url,
            &root_bytes,
            origin,
            source,
            max_tile_distance,
            pool_size,
            max_resident,
        )
    }

    /// Stamp the wall-clock for the step about to run. The stream loop calls this with `Instant::now()` right
    /// before [`advance_from_ue`](Self::advance_from_ue); the headless backoff tests pass a controlled clock.
    /// All retry-backoff + session-refresh deadlines read this one source (audit H5), so a backoff is real
    /// milliseconds regardless of the loop's variable step cadence. A no-op before streaming is initialized.
    pub fn set_now(&mut self, now: Instant) {
        if let Some(s) = self.streaming.as_mut() {
            s.now_instant = now;
        }
    }

    /// One **streaming** step, RE-SELECTION-GATED (audit 2026-06-10 §3.1). Every call drains completed fetches
    /// + services the session refresh (cheap, O(arrivals)); the EXPENSIVE remainder — the full tree traversal,
    ///   the schedule, reconcile, prune — runs only when an event that can change the selection's output fired:
    ///
    /// * the view changed (camera / fov / viewport / sse / cull — anything but the frame counter),
    /// * the renderer reported a new textured set ([`set_textured_keys`](Self::set_textured_keys)),
    /// * the drain delivered ≥ 1 result (arrivals also free cap slots → new fetch issuance),
    /// * a transient-failure retry deadline expired (the backed-off tile is selectable again),
    /// * the previous full step left follow-up work (`force_full_next`: evictions/decode-failures freed slots,
    ///   or the ranked fetch loop was budget/cap/pool-limited with candidates remaining).
    ///
    /// While merely WAITING on the network (in-flight high, nothing arrived this wake), the old
    /// unconditional pipeline re-ran the whole O(V·d) traversal at the stream loop's 1 ms cadence for an
    /// identical answer — this gate cuts that to one full step per event, with zero added latency (every
    /// state change still triggers an immediate full step). Returns `true` iff the full step ran (the stream
    /// loop advances its frame counter only then, keeping LRU/staleness semantics frame-of-activity-based).
    pub fn advance_streaming(&mut self, view: GeoView) -> bool {
        let drained = self.drain_raw(view.frame);
        self.refresh_session_if_needed(); // drain may have flagged an expired P3DT session (enqueue-only, cheap)
        let run_full = match self.streaming.as_mut() {
            None => true, // foundation worlds (tests) have no event sources — always step fully
            Some(s) => {
                let view_changed = s.last_full_view.is_none_or(|lv| {
                    lv.camera_ecef != view.camera_ecef
                        || lv.fov_y_radians != view.fov_y_radians
                        || lv.viewport_height != view.viewport_height
                        || lv.max_sse != view.max_sse
                        || lv.cull_distance_m != view.cull_distance_m
                });
                let now = s.now_instant;
                // O(backing-off set) — small (a handful of 429/5xx tiles); only probed on otherwise-idle wakes.
                let retry_due = s.failures.next_retry.values().any(|&t| t <= now);
                view_changed || drained > 0 || retry_due || s.force_full_next
            }
        } || self.textured_dirty;
        if !run_full {
            return false; // drain-only wake: nothing that could change the selection's output happened
        }
        self.textured_dirty = false;
        if let Some(s) = self.streaming.as_mut() {
            s.last_full_view = Some(view);
            s.force_full_next = false;
        }
        self.select_request(view);
        self.advance(view);
        self.reconcile_index();
        self.prune_cold_grafts(view.frame);
        self.bound_failure_sets();
        // Follow-up triggers: evictions freed slots (restorable/refetchable next step), and DECODES landed
        // (decode runs in the schedule AFTER this step's select — the freshly-`Resident` tile is
        // prepare-stamped/published only by the NEXT selection pass). The fetch-backlog and decode-failure
        // cases set `force_full_next` at their own sites (`select_request` / `reconcile_index`).
        let stats = *self.world.resource::<GeoStats>();
        if stats.evicted_this_step > 0 || stats.decoded_this_step > 0 {
            if let Some(s) = self.streaming.as_mut() {
                s.force_full_next = true;
            }
        }
        true
    }

    /// Re-fetch the root tileset for a FRESH Google P3DT `?session=…` when [`fail_fetch`](Self::fail_fetch)
    /// flagged an expired session (a 401/403). The session the root minted has a TTL; once it lapses, every new
    /// tile/subtree fetch 401s and (without this) dies permanently, freezing the planet at the coarse LOD it
    /// reached while the session was valid. Re-fetching the root (its URL still carries the long-lived `?key=`)
    /// mints a new session baked into fresh content/subtree templates; resetting the session-tainted streaming
    /// state then makes the traversal re-descend + re-fetch everything not yet resident with the new session.
    ///
    /// ASYNC (audit M1b): the root re-fetch rides the WORKER POOL at the top priority (it unblocks the whole
    /// tree), NOT a blocking `http_get` on this stream thread (which stalled the entire sim up to the request
    /// timeout on every session expiry). This only ENQUEUES (one at a time, cooldown-gated); the result lands
    /// in [`drain_raw`](Self::drain_raw)'s session-refresh lane → [`apply_refreshed_root`](Self::apply_refreshed_root).
    /// Rare + cooldown-gated; resident tiles keep rendering throughout (geometry is session-independent and the
    /// entity pool/index are preserved).
    fn refresh_session_if_needed(&mut self) {
        let (pending, last, now, in_flight) = match self.streaming.as_ref() {
            Some(s) => (
                s.session.refresh_pending,
                s.session.last_refresh_at,
                s.now_instant,
                s.session.refresh_in_flight.is_some(),
            ),
            None => return,
        };
        // Nothing to do unless a refresh is flagged, none is already in flight, and the cooldown has elapsed (a
        // session expiry 401s every in-flight tile at once → collapse that storm into ONE root re-fetch).
        if !pending || in_flight || last.is_some_and(|t| now.saturating_duration_since(t) < SESSION_REFRESH_COOLDOWN) {
            return;
        }
        // ENQUEUE the root re-fetch on the worker pool at the top priority (it unblocks the entire tree). Stamp
        // `last` now so the cooldown throttles even a persistently-failing refresh; `pending` stays set until a
        // refresh actually APPLIES (in `drain_raw`). The `now_tick` arg is ignored by the HTTP source (it drains
        // a channel); the deterministic sources don't drive P3DT sessions. `base_url` (a ~100+ byte keyed P3DT
        // String) is cloned HERE, inside this rare cooldown-gated branch — not in the per-step destructure
        // above, which put a heap clone+drop on every settled-step spine (re-eval 2026-06-09).
        let s = self.streaming.as_mut().unwrap();
        let id = s.next_id;
        s.next_id += 1;
        s.session.last_refresh_at = Some(now);
        s.session.refresh_in_flight = Some(id);
        let base_url = s.base_url.clone();
        s.source.request(id, &base_url, SESSION_REFRESH_PRIORITY, 0);
    }

    /// Apply a successfully re-fetched root (M1b): swap in the fresh root (its content/subtree templates carry
    /// the new `?session=…`) and reset every session-tainted streaming bit so the traversal re-descends + re-
    /// fetches everything not yet resident under the new session. URL-keyed caches (`subtree_cache`/`_failed`/
    /// `_urls`) hold stale-session keys; the path-keyed failure sets are cleared so tiles killed under the old
    /// session get a fresh chance. `index` + the entity pool + in-flight fetches are KEPT (resident tiles keep
    /// rendering; old in-flight results route by id and are harmless). Called from `drain_raw`'s refresh lane.
    fn apply_refreshed_root(&mut self, root: Tile) {
        let entities: Vec<Entity> = {
            let Some(s) = self.streaming.as_mut() else {
                return;
            };
            s.root = root;
            s.subtrees.cache.clear();
            s.subtrees.cache_order.clear();
            s.subtrees.failed.clear();
            s.subtrees.in_flight_urls.clear();
            s.failures.decode_failed.clear();
            s.failures.fetch_attempts.clear();
            s.failures.next_retry.clear();
            s.grafts.records.clear();
            s.grafts.bases.clear();
            s.session.refresh_pending = false;
            // A fresh session is now live: paths that 4xx'd under the OLD session get exactly one retry with
            // it (their `session_4xx_gen` entries are now stale-generation — deliberately NOT cleared).
            s.session.generation += 1;
            s.force_full_next = true; // the fresh tree must be re-walked promptly (§3.1)
            s.index.values().copied().collect()
        };
        // C7 (audit 2026-06-10): the fresh root REPLACES the whole tree, so index entries under paths the new
        // tree no longer produces become unreachable — `Resident`-but-never-selected slots that squat the pool
        // until cap pressure. Zero their `LastSeen` so they are FIRST in line when eviction needs room; entries
        // the new tree still selects are re-stamped on the very next pass, so live tiles lose nothing.
        for e in entities {
            if let Some(mut ls) = self.world.get_mut::<LastSeen>(e) {
                ls.0 = 0;
            }
        }
        self.world.resource_mut::<GeoStats>().session_refreshes_total += 1;
    }

    /// Bound the explicit-tree failure "skip-hint" sets over a long session. The implicit `subtree_cache` /
    /// `subtree_failed` are bounded at their insert sites ([`GeoStreaming::cache_subtree`] /
    /// [`GeoStreaming::mark_subtree_failed`]); `decode_failed` / `fetch_attempts` for the **non-grafted root
    /// tree** are not covered by [`prune_cold_grafts`](Self::prune_cold_grafts) (which keys on grafted paths),
    /// so cap them here. Cleared wholesale past [`MAX_DEAD_PATHS`] — self-healing (a still-dead path re-fails
    /// and re-adds, paced by the in-flight cap). O(1) length checks per step; the clear is rare (a high cap).
    fn bound_failure_sets(&mut self) {
        let Some(s) = self.streaming.as_mut() else { return };
        // Purge EXPIRED retry deadlines (re-eval 2026-06-09). An entry's only effect is "skip while
        // `now < deadline`" in `select_request`, so an expired entry is semantically a no-op for selection —
        // but a path whose deadline passed while it was OUT of the selection (camera moved on) kept its
        // entry forever (entries were removed only on later success / give-up / prune / refresh), holding
        // `has_pending_retry()` true and the stream-thread work-gate hot: a parked camera over a settled
        // scene re-ran the full advance at idle cadence (~125 Hz) for the rest of the session. O(|next_retry|)
        // per step over the small backing-off set.
        let now = s.now_instant;
        s.failures.next_retry.retain(|_, t| *t > now);
        if s.failures.decode_failed.len() > MAX_DEAD_PATHS {
            s.failures.decode_failed.clear();
        }
        if s.failures.fetch_attempts.len() > MAX_DEAD_PATHS {
            s.failures.fetch_attempts.clear();
            s.failures.next_retry.clear();
        }
        if s.session.last_4xx_gen.len() > MAX_DEAD_PATHS {
            s.session.last_4xx_gen.clear();
        }
    }

    /// Drain completed fetches: graft external tilesets into the tree, or hand a GLB's bytes to its entity
    /// (`FetchedBytes` + `Fetched`, recorded in [`JustFetched`]) — **no decode here**; the `decode` system does
    /// it. `now_tick` is the monotonic step counter the source's completion schedule uses (ignored by the real
    /// HTTP source, which drains its channel; used by the deterministic `SyntheticSource` in tests). Returns
    /// the number of drained results — the re-selection gate's "something arrived" trigger (audit §3.1).
    fn drain_raw(&mut self, now_tick: u64) -> usize {
        let mut done = Vec::new();
        match self.streaming.as_mut() {
            Some(s) => s.source.poll_completed(now_tick, &mut done),
            None => return 0,
        }
        let drained = done.len();
        for loaded in done {
            // Session-refresh lane (M1b): the ASYNC root re-fetch this id was issued for has landed. Re-parse
            // the root for the fresh `?session=…` + reset the session-tainted state — OFF the stream thread (no
            // blocking `http_get`). Routed FIRST: the id is neither a subtree nor a tile, so the lanes below
            // would drop it. A fetch/parse failure leaves `pending` set (the cooldown throttles the next try)
            // and is NOT a tile give-up (it never touches `fetch_failed_total`).
            if self
                .streaming
                .as_ref()
                .is_some_and(|s| s.session.refresh_in_flight == Some(loaded.id))
            {
                if let Some(s) = self.streaming.as_mut() {
                    s.session.refresh_in_flight = None;
                }
                if let Ok(bytes) = &loaded.result {
                    if let Ok(tileset) = parse_tileset(bytes) {
                        self.apply_refreshed_root(tileset.root);
                    }
                }
                continue;
            }
            // Subtree lane (audit S1): a fetched `.subtree` → decode + cache so the implicit traversal can
            // descend below its implicit root next pass. Routed by RequestId before the tile lane (ids are
            // unique across both). Malformed / 404 → terminal (`subtree_failed`) so we don't re-pend forever;
            // a transient failure is left uncached (re-pended next walk, paced by the fetch round-trip).
            {
                let s = self.streaming.as_mut().unwrap();
                if let Some((url, child_count)) = s.subtrees.in_flight.remove(&loaded.id) {
                    s.subtrees.in_flight_urls.remove(&url);
                    match loaded.result {
                        Ok(bytes) => match parse_subtree(&bytes, child_count) {
                            Ok(subtree) => s.subtrees.cache_subtree(url, subtree),
                            Err(_) => s.subtrees.mark_failed(url),
                        },
                        Err(e) if !e.is_transient() => s.subtrees.mark_failed(url),
                        Err(_) => {}
                    }
                    continue;
                }
            }
            let inflight = {
                let s = self.streaming.as_mut().unwrap();
                let Some(inflight) = s.in_flight.remove(&loaded.id) else {
                    continue;
                };
                // Only the primary content (`content_index == 0`) owns the tile's in-flight bookkeeping; a 1.1
                // additional content shares the node and must not clear it out from under the primary.
                if inflight.content_index == 0 {
                    s.in_flight_nodes.remove(&inflight.node);
                }
                inflight
            };
            // Typed outcome (audit A1/A10): an `Err` carries the failure CLASS (404 terminal vs 429/5xx/
            // transport transient) so `fail_fetch` can abandon at once or back off + retry; an `Ok` — even an
            // empty body — is a success. The old code collapsed every outcome to "empty bytes ⇒ failure".
            let bytes = match loaded.result {
                Ok(b) => b,
                Err(e) => {
                    self.fail_fetch(inflight, e);
                    continue;
                }
            };
            // Fetch succeeded → clear any prior consecutive-failure count + retry backoff for this tile (the
            // primary owns that per-node state; an extra's success must not reset the primary's failure count).
            if inflight.content_index == 0 {
                let s = self.streaming.as_mut().unwrap();
                s.failures.fetch_attempts.remove(&inflight.node);
                s.failures.next_retry.remove(&inflight.node);
                s.session.last_4xx_gen.remove(&inflight.node);
            }
            // A 1.1 additional content (`content_index > 0`) rides the primary's entity and takes the inline
            // decode path — it never grafts (an extra is renderable GLB, not a nested tileset) and never touches
            // the primary slot's `FetchedBytes`/state. Route it here, before the primary's graft/GLB dispatch.
            if inflight.content_index > 0 {
                if let Some(e) = inflight.entity {
                    self.store_extra_content(e, inflight.node, inflight.content_index, &bytes);
                }
                continue;
            }
            let kind = sniff_content_kind(&bytes).unwrap_or(inflight.kind);
            match kind {
                ContentKind::Tileset => {
                    // A fetch REQUESTED as GLB (entity + index reserved by `request_one`) can SNIFF as a
                    // tileset — the extensionless-endpoint case where byte sniffing is authoritative over the
                    // URI hint. Reclaim the reserved slot + index entry, or they leak as a permanent
                    // `Requested` ghost: a LiveSet member that never decodes, with an index entry that makes
                    // `select_request` skip the path forever (region wedged coarse if the graft also fails;
                    // re-eval 2026-06-09). Mirrors `fail_fetch`'s reclaim order.
                    if let Some(e) = inflight.entity {
                        self.streaming.as_mut().unwrap().index.remove(&inflight.node);
                        self.free_slot(e, false); // a Requested slot was never Resident
                    }
                    let grafted_ok = {
                        let s = self.streaming.as_mut().unwrap();
                        // The graft needs the actual child-index path to walk the tree — reconstruct it from
                        // the node (O(d), rare: once per grafted manifest).
                        let path = s.arena.path_of(inflight.node);
                        let ok = graft_external(
                            &mut s.root,
                            &mut s.grafts.records,
                            &path,
                            inflight.node,
                            &bytes,
                            now_tick,
                        )
                        .is_ok();
                        if ok {
                            // Descendants inherit this subtree's fetch URL (carries the P3DT `?session=…`).
                            s.grafts.bases.insert(inflight.node, inflight.url);
                        } else {
                            // Terminal marker (re-eval 2026-06-09): a manifest that fetches but won't graft
                            // (parse error / target gone) was in NONE of `select_request`'s skip sets, so it
                            // re-fetched + re-failed every advance — and the in-flight fetch held the loop at
                            // SHORT_PARK (1 ms) cadence, a near-busy loop per corrupt manifest. `decode_failed`
                            // is semantically exact (bytes arrived but can never become content): the
                            // selector's `is_dead` lets siblings refine past it, and a session refresh
                            // (`apply_refreshed_root`) clears it for a fresh chance.
                            s.failures.decode_failed.insert(inflight.node);
                        }
                        ok
                    };
                    if !grafted_ok {
                        // The manifest fetched but didn't graft: its subtree never appears, so that region is
                        // stuck coarse. Count it (was silent) — see `graft_failed_total`.
                        self.world.resource_mut::<GeoStats>().graft_failed_total += 1;
                    }
                }
                ContentKind::Glb => {
                    if let Some(e) = inflight.entity {
                        // L4: the worker already handed us a shared `Arc<[u8]>` — park it with a move (the
                        // alloc+copy happened off-thread, on the worker), no `Arc::from(into_boxed_slice())`
                        // re-copy on this serial owner thread.
                        {
                            let mut ent = self.world.entity_mut(e);
                            ent.get_mut::<FetchedBytes>().unwrap().0 = Some(bytes);
                            *ent.get_mut::<TileState>().unwrap() = TileState::Fetched;
                        }
                        // ADR-0018: record the Requested→Fetched arrival so `decode` processes exactly this
                        // step's just-fetched tiles (O(Δ_fetched)), not a `par_iter` scan of the whole pool.
                        self.world.resource_mut::<JustFetched>().0.push(e);
                    } else {
                        // Requested as a tileset MANIFEST (no slot reserved) but the bytes sniffed as GLB:
                        // there is no entity to park them in, and silently dropping them left the tile in no
                        // skip set — re-selected + re-fetched forever (re-eval 2026-06-09). Terminal, same
                        // recovery story as a failed graft (session refresh clears it).
                        self.streaming
                            .as_mut()
                            .unwrap()
                            .failures
                            .decode_failed
                            .insert(inflight.node);
                        self.world.resource_mut::<GeoStats>().decode_failed_total += 1;
                    }
                }
            }
        }
        drained
    }

    /// 3D Tiles 1.1 multiple-contents: a tile's ADDITIONAL content (`content_index > 0`) has landed — decode it
    /// **inline** (off the `decode` system — this is the rare path, not the per-frame P3DT hot loop) and
    /// park the mesh in the entity's [`MultiContent`] part. Guards against a recycled slot: if the entity has been
    /// re-tasked to another tile since this extra was requested (its `TileId` no longer matches the primary key),
    /// the stale result is dropped. A decode failure / missing part simply leaves the content undrawn.
    fn store_extra_content(&mut self, e: Entity, node: u32, content_index: u32, bytes: &[u8]) {
        // Stale-slot guard: the entity must still back this very tile (primary key == the node's path key).
        let primary_key = self.streaming.as_ref().unwrap().arena.key(node);
        if self.world.get::<TileId>(e).map(|t| t.0) != Some(primary_key) {
            return;
        }
        let key = content_key(primary_key, content_index);
        // The part must still be parked (not dropped by a slot recycle): if it's gone, the extra is stale.
        if self
            .world
            .get::<MultiContent>(e)
            .is_none_or(|mc| mc.0.iter().all(|p| p.key != key))
        {
            return;
        }
        let xf = self
            .world
            .get::<TileTransform>(e)
            .map(|t| t.0)
            .unwrap_or(DMat4::IDENTITY);
        let decode_fn = Arc::clone(&self.world.resource::<TileDecoder>().0);
        let Some(geom) = decode_fn(bytes, xf) else {
            return; // a content that won't decode is simply not drawn — the tile still renders content[0]
        };
        let mesh = Arc::new(geom);
        if let Some(mut mc) = self.world.get_mut::<MultiContent>(e) {
            if let Some(part) = mc.0.iter_mut().find(|p| p.key == key) {
                part.mesh = Some(mesh);
            }
        }
    }

    /// Reset a pool slot `e` to `Free` and return it to the free-list (pure field mutation, never despawn) — the
    /// single shared body for every slot-reclaim site (`fail_fetch`, `cancel_stale`, `preempt_inflight`,
    /// `reconcile_index`, `prune_cold_grafts`). Clears `FetchedBytes` / `ResidentMesh` / the 1.1 [`MultiContent`]
    /// parts so no stale tenant data rides the recycled slot. `drop_resident` ALSO removes `e` from
    /// [`ResidentSet`] (the prune path may free a still-`Resident` tile); the fetch/cancel paths reclaim a
    /// `Requested`/`Fetched` slot that was never in that set, so they pass `false`. ALWAYS removes `e` from
    /// [`LiveSet`] — every reclaim site is a non-`Free`→`Free` transition (ADR-0018).
    fn free_slot(&mut self, e: Entity, drop_resident: bool) {
        *self.world.get_mut::<TileState>(e).unwrap() = TileState::Free;
        self.world.get_mut::<FetchedBytes>(e).unwrap().0 = None;
        // Deposit a dropped RESIDENT mesh in the flash LRU instead of discarding it — the same warm
        // re-entry path `evict` uses. Before this (2026-06-12), only the LRU evictor deposited; the
        // prune-cold-grafts path freed whole resident subtrees through here with the mesh dropped on the
        // floor, so a revisit re-restored from the DISK geometry tier while the in-memory cache sat
        // permanently empty (the panel's flash 0-hits/0-MB signature). Decode-failure frees also pass
        // `drop_resident = true` but carry no mesh — `take()` is `None` there, nothing is cached.
        if drop_resident {
            if let Some(geom) = self.world.get_mut::<ResidentMesh>(e).unwrap().0.take() {
                let key = self.world.get::<TileId>(e).unwrap().0;
                let seen = self.world.get::<LastSeen>(e).unwrap().0;
                let center = self.world.get::<TileBounds>(e).unwrap().center;
                let view = *self.world.resource::<GeoView>();
                let dist = (view.camera_ecef - center).length() as f32;
                let mut cache = self.world.resource_mut::<FlashCache>();
                cache.insert(key, geom, seen, dist);
                cache.trim(view.frame); // amortized: only sorts/drops when the insert pushed it over budget
            }
        }
        self.world.get_mut::<ResidentMesh>(e).unwrap().0 = None;
        self.world.get_mut::<MultiContent>(e).unwrap().0.clear();
        if drop_resident {
            self.world.resource_mut::<ResidentSet>().0.remove(&e);
        }
        self.world.resource_mut::<LiveSet>().0.remove(&e); // non-Free→Free (ADR-0018 working-set)
        self.world.resource_mut::<FreeList>().0.push(e);
    }

    /// Handle a failed fetch, CLASSIFIED by the source (audit A1). Always reclaims the reserved GLB slot, then
    /// decides the path's fate by failure class:
    /// * **`Cancelled`** — the owner already moved on (the tile left the view; see `cancel_stale`): free the
    ///   slot, don't count it, don't bump attempts. Nothing to retry.
    /// * **Terminal** (`!is_transient`: 404/400, bad URL, over-cap body): give up IMMEDIATELY — mark the
    ///   path at the attempt cap so `select_request` skips it permanently, and count it once. Retrying a tile
    ///   the server says doesn't exist only wastes bandwidth + a worker slot (the old code burned all 4
    ///   attempts on a dead 404).
    /// * **Auth-expired** (401/403): almost always an expired P3DT `?session=…`, not a dead URL — flag a session
    ///   refresh + a brief backoff (`next_retry`) and stay recoverable (`gave_up = false`, not a give-up); once
    ///   the root re-fetch mints a fresh session the path re-selects and succeeds.
    /// * **Transient** (429/5xx/transport): re-selectable, but on a BACKOFF — set `next_retry` so the path is
    ///   not re-fetched until `now_instant + delay` (capped exponential by attempt + per-path jitter, floored by
    ///   the server's `Retry-After`). After [`MAX_FETCH_ATTEMPTS`] consecutive transient misses it too becomes
    ///   terminal.
    ///
    /// The old code treated every failure as a no-delay transient retry, so a wave of 429s became a sub-ms
    /// retry storm that deepened the throttling AND falsely abandoned merely-rate-limited tiles after 4 fast
    /// failures. (It still reclaims the slot the pre-A1 fix added — never strands the entity as `Requested`.)
    fn fail_fetch(&mut self, inflight: InFlight, err: FetchError) {
        // A 1.1 ADDITIONAL content (`content_index > 0`) rides the primary's slot and the primary owns the
        // path's retry state — so a failed extra frees nothing and touches no per-path counters. It just doesn't
        // draw (its `MultiContent` part keeps `mesh = None`); the tile still renders content[0] and the rest.
        if inflight.content_index > 0 {
            return;
        }
        // Return the reserved GLB slot to the pool (a tileset fetch has no entity, so nothing to free here).
        if let Some(e) = inflight.entity {
            self.free_slot(e, false); // a Requested slot was never Resident
        }
        // A cancel is not a failure: the slot is freed above; leave attempts/backoff untouched and don't count.
        if matches!(err, FetchError::Cancelled) {
            return;
        }
        let path_key = self.streaming.as_ref().unwrap().arena.key(inflight.node);
        // Honor a server `Retry-After` (already a `Duration`), capped so a hostile value can't pin us out.
        let retry_after = err
            .retry_after()
            .map(|d| d.min(RETRY_AFTER_MAX))
            .unwrap_or(Duration::ZERO);
        // A 401/403 on a P3DT tile is almost always an EXPIRED SESSION (the `?session=…` the root minted has a
        // TTL), NOT a permanently-dead URL. Treat it as RECOVERABLE: flag a session refresh (re-fetch the root
        // for a fresh session — `refresh_session_if_needed`) and merely BACK OFF this path instead of marking it
        // terminal. Without this, every fetch issued after the TTL dies forever → the planet freezes at the
        // coarse LOD it reached while the session was valid (the "tiles stuck / failing to load" symptom).
        //
        // 401/403 alone is NOT enough (2026-06-11, live): an 8.5-h-idle editor came back to Google answering
        // its stale session with a status the lane below treated as TERMINAL — 1,590 instant give-ups in 28 s,
        // 257 permanently-dead branches (`hold_dead` X-ray), coarse smears that only a reload could clear. So
        // a 4xx on a URL that CARRIES a session token is presumed session expiry too (429 excepted — that lane
        // honours `Retry-After`) — but only ONCE per session generation: a path whose `session_4xx_gen` entry
        // is from an OLDER generation has now failed WITH a fresh session — genuinely-dead content, terminal
        // lane below. Without the generation check, one persistently-404ing path re-flagged a refresh every
        // cooldown and each `apply_refreshed_root` cleared grafts/bases/LastSeen wholesale — the region
        // perpetually re-fetched + re-evicted (the rio wedge: in_flight pinned at the cap, built collapsing,
        // the station never settles).
        let session_4xx = matches!(err, FetchError::Http { status, .. }
                if (400..500).contains(&status) && !matches!(status, 401 | 403 | 429))
            && inflight.url.contains("session=")
            && {
                let s = self.streaming.as_mut().unwrap();
                let gen = s.session.generation;
                match s.session.last_4xx_gen.insert(inflight.node, gen) {
                    // Failed in an EARLIER generation already ⇒ the fresh session didn't help — not expiry.
                    Some(prev) if prev < gen => false,
                    _ => true,
                }
            };
        let auth_expired =
            session_4xx || matches!(err, FetchError::Http { status, .. } if status == 401 || status == 403);
        let gave_up = {
            let s = self.streaming.as_mut().unwrap();
            let now = s.now_instant;
            // Drop the stale index entry so the tile is re-selectable (the entity it named is now Free).
            s.index.remove(&inflight.node);
            if auth_expired {
                s.session.refresh_pending = true;
                // Back off to the SESSION-REFRESH cadence, not the transient base: this path cannot succeed
                // until a refresh applies, and refreshes are cooldown-gated at SESSION_REFRESH_COOLDOWN — a
                // 24 ms (RETRY_BACKOFF_BASE) re-try just hammered the stale session with doomed 401s at
                // worker-pool throughput until the refresh landed (re-eval 2026-06-09). The applied refresh
                // clears `next_retry` wholesale, so a successful refresh retries everything immediately.
                let delay = SESSION_REFRESH_COOLDOWN.max(retry_after);
                s.failures.next_retry.insert(inflight.node, now + delay);
                false // recoverable once the session refreshes — not a give-up
            } else if err.is_transient() {
                let n = s.failures.fetch_attempts.entry(inflight.node).or_insert(0);
                *n += 1;
                if *n >= MAX_FETCH_ATTEMPTS {
                    // Out of retries → terminal: the attempt cap (≥ MAX) makes `select_request` skip it; clear
                    // the pending backoff so the terminal marker, not the timer, governs.
                    s.failures.next_retry.remove(&inflight.node);
                    true
                } else {
                    // Back off before the next attempt: capped exponential by attempt count + a per-tile jitter
                    // (de-synchronizes a herd that failed together), floored by the server's `Retry-After`.
                    let shift = (*n - 1).min(RETRY_MAX_SHIFT);
                    let exp = RETRY_BACKOFF_BASE.saturating_mul(1u32 << shift);
                    let jitter = Duration::from_millis(path_key % RETRY_BACKOFF_BASE.as_millis() as u64);
                    let delay = exp.max(retry_after).saturating_add(jitter);
                    s.failures.next_retry.insert(inflight.node, now + delay);
                    false
                }
            } else {
                // Terminal (4xx≠429 / bad URL / over-cap): give up at once. Mark at the cap so it's skipped
                // permanently; no backoff timer needed.
                s.failures.fetch_attempts.insert(inflight.node, MAX_FETCH_ATTEMPTS);
                s.failures.next_retry.remove(&inflight.node);
                true
            }
        };
        if gave_up {
            self.world.resource_mut::<GeoStats>().fetch_failed_total += 1;
        }
    }

    /// Run the SSE traversal over the (growing) tree: stamp visible resident tiles into this frame's working
    /// set, and acquire + request a fetch for each selected-but-unloaded tile. Reuses `select_tree_implicit`.
    fn select_request(&mut self, view: GeoView) {
        if self.streaming.is_none() {
            return;
        }
        let cam = view.camera_ecef;
        // Walk the tree → render (with the cross-frame anti-pop kick: coarse ancestor instead of a hole-y
        // partial refinement) + load (to fetch). Last frame's `sel_state` drives the kick AND the hysteresis
        // dead-band so a camera on an SSE threshold can't flip a tile parent↔children every frame.
        // `.subtree` files the implicit traversal finds missing are recorded here (audit S1) and fetched after
        // the walk — declared outside the borrow block so it outlives the immutable `&s` the walk holds.
        let mut needed_subtrees: Vec<(String, u32)> = Vec::new();
        let mut sel = {
            // Split-borrow the streaming state: the walk interns into `arena` (&mut) while reading the tree +
            // the node-keyed maps (&) — disjoint fields of one `&mut GeoStreaming` (audit §3.2).
            let GeoStreaming {
                arena,
                root,
                ellipsoid,
                subtrees,
                grafts,
                base_url,
                index,
                failures,
                sel_state,
                max_tile_distance,
                cam_dir_ecef,
                frustum,
                ..
            } = self.streaming.as_mut().unwrap();
            // Split the cells into the leaf borrows the walk reads (immutable; `sel_state` stays `&mut`).
            let SubtreeStreams {
                cache: subtree_cache,
                failed: subtree_failed,
                ..
            } = &*subtrees;
            let FailureLedger {
                decode_failed,
                fetch_attempts,
                ..
            } = &*failures;
            let bases = &grafts.bases;
            // `is_loaded` probes the node→entity index + the world directly — O(1) u32 probes, no path
            // hashing (the old `HashMap<Vec<u32>, _>` keys cost O(d) bytes hashed per probe; audit §3.2).
            let world = &self.world;
            // The renderer-reported textured-tile set (C++→Rust), read by the `is_loaded` texture gate below.
            let textured = &world.resource::<TexturedSet>().0;
            let params = SelectParams {
                camera_position: cam,
                fov_y_radians: view.fov_y_radians,
                viewport_height: view.viewport_height.max(1.0),
                max_screen_space_error: view.max_sse,
                distant_screen_space_error: view.max_sse * DISTANT_SSE_FACTOR,
                culled_screen_space_error: (view.max_sse * CULLED_SSE_FACTOR).max(CULLED_SSE_FLOOR_PX),
                hysteresis: LOD_HYSTERESIS,
            };
            let max_d = *max_tile_distance;
            let cam_dir = *cam_dir_ecef; // Copy out for the behind-camera test (audit E #5).
            let frustum = *frustum; // Copy out so the visibility closure borrows nothing of `s`.
                                    // Implicit tiling (audit S1): answer the traversal's subtree-availability queries from the decoded
                                    // cache; a miss is recorded in `needed_subtrees` (fetched below) and answered `Pending`, so the
                                    // walk holds the coarse ancestor until the `.subtree` arrives. Explicit tilesets never invoke it.
            let mut provider = SubtreeAvailability {
                cache: subtree_cache,
                failed: subtree_failed,
                base_url,
                bases,
                needed: &mut needed_subtrees,
            };
            select_tree_implicit(
                root,
                ellipsoid,
                &params,
                arena,
                &mut |_, n| {
                    // Texture gate (invisible LOD loading): a tile is renderable — eligible to replace its coarse
                    // parent — only when its geometry is `Resident` AND the renderer has textured it. So the kick
                    // holds the coarse parent rendered until the fine tile is textured; the fine tile meanwhile
                    // publishes as a HIDDEN "prepare" tile the renderer decodes under cover. Resident-but-untextured
                    // ⇒ NOT loaded ⇒ stays in `load` (where the prepare-stamp below publishes it hidden).
                    index.get(&n).is_some_and(|&e| {
                        world.get::<TileState>(e) == Some(&TileState::Resident)
                            && world.get::<TileId>(e).is_some_and(|id| textured.contains(&id.0))
                    })
                },
                // is_dead (R1): content decode gave up, or the fetch hit the retry cap. `select_request` already
                // PERMANENTLY skips these, so without this signal the traversal kept treating them as "still
                // loading" → a dead fine tile pinned its REPLACE parent as a coarse stand-in forever, blurring
                // the whole region over its loaded siblings. Treating it as ABSENT lets the siblings refine sharp
                // (the dead tile's footprint falls back to coarse only if its WHOLE sibling set is dead).
                &mut |_, n| {
                    decode_failed.contains(&n) || fetch_attempts.get(&n).is_some_and(|&c| c >= MAX_FETCH_ATTEMPTS)
                },
                &mut |c, r| {
                    let dist = (cam - c).length() - r;
                    // Distance hard-cull (the altitude-scaled far plane, audit C).
                    if dist >= max_d {
                        Visibility::Culled
                    } else if cam_dir != DVec3::ZERO && (c - cam).dot(cam_dir) + r < 0.0 {
                        // Behind-camera exclusion (audit E #5): a tile fully behind the camera plane is never
                        // rendered (not even a coarse shell) — the user's "behind ⇒ nothing". Skipped when the
                        // direction is unknown (ZERO ⇒ headless ⇒ accept-all, as before). NOTE: can drop a shadow
                        // cast INTO view from just-behind geometry; if that regresses in PIE, gate it by altitude.
                        Visibility::Culled
                    } else if frustum.contains_sphere(c, r) {
                        // Distance-banded detail (audit E #7): the near OPTIMAL band gets FULL detail; in-frustum
                        // but FAR tiles — the VISIBLE vista — get the mild Distant demotion, collapsing the
                        // fine-tile demand (and the pool churn behind the "crisp beside blurry, never converges"
                        // patchwork) onto the near band the camera actually resolves.
                        if dist <= DETAIL_BAND_M {
                            Visibility::Visible
                        } else {
                            Visibility::Distant
                        }
                    } else {
                        // Off-frustum (not fully behind): kept as a genuinely COARSE shell — shadow casters +
                        // camera-turn readiness — NOT hard-culled by direction.
                        Visibility::Offscreen
                    }
                },
                sel_state,
                &mut provider,
            )
        };
        // Enqueue the `.subtree` files the walk needs (they gate refinement, so fetch them ahead of tile
        // content). Deduped vs in-flight + terminally-failed, bounded by the same in-flight cap as tiles.
        if !needed_subtrees.is_empty() {
            let now_tick = view.frame; // the source's u64 step tick (the deterministic source's schedule), NOT a wall-clock Instant
            let s = self.streaming.as_mut().unwrap();
            for (url, child_count) in needed_subtrees {
                if s.source.in_flight() >= s.max_in_flight {
                    break;
                }
                if s.subtrees.failed.contains(&url) || s.subtrees.in_flight_urls.contains(&url) {
                    continue; // already terminally failed, or a fetch is already in flight (O(1) dedup)
                }
                let id = s.next_id;
                s.next_id += 1;
                s.source.request(id, &url, SUBTREE_FETCH_PRIORITY, now_tick);
                s.subtrees.in_flight_urls.insert(url.clone());
                s.subtrees.in_flight.insert(id, (url, child_count));
            }
        }
        // The selection X-ray: publish this pass's hold-reason counts (snapshot, not cumulative) so the
        // Details panel / bench sidecars can attribute a coarse region to its cause (2026-06-11 scars).
        {
            let stats = &mut self.world.resource_mut::<GeoStats>();
            stats.hold_kick = sel.holds.kick;
            stats.hold_dead = sel.holds.dead;
            stats.hold_loading = sel.holds.loading;
            stats.hold_leaf_under_sse = sel.holds.leaf_under_sse;
            stats.hold_pending_subtree = sel.holds.pending_subtree;
        }
        // Stamp visible (render) tiles so evict spares them, and record them as this pass's DRAW SET for
        // `publish` ([`PublishSet`] — stale finer residents outside the selection must NOT draw; see
        // `publish`'s rationale). The Vec is recycled (`mem::take` + put-back) — no steady-state allocation.
        let mut draw_set = std::mem::take(&mut self.world.resource_mut::<PublishSet>().entities);
        draw_set.clear();
        for rt in &sel.render {
            let e = self.streaming.as_ref().unwrap().index.get(&rt.node).copied();
            if let Some(e) = e {
                if let Some(mut ls) = self.world.get_mut::<LastSeen>(e) {
                    ls.0 = view.frame;
                }
                draw_set.push(e);
            }
        }
        // Hole-clip: refresh the per-tile clip CHILDREN for this selection — stand-in `tile_key` → the keys of
        // the finer tiles drawn over it — so `publish` can materialize each child's tight mesh-AABB clip box.
        // Rebuilt every pass — a coarse stand-in's clip set grows one child per sibling as they texture in, then
        // the tile drops. Keyed identically to the entity's `TileId` (`tile_key`).
        {
            let clip = &mut self.world.resource_mut::<ClipChildren>().0;
            clip.clear();
            for rt in &sel.render {
                if !rt.clip_children.is_empty() {
                    clip.insert(rt.key, rt.clip_children.clone());
                }
            }
        }
        // Prepare set (invisible LOD loading): a tile still in `load` that is already geometry-`Resident` is one
        // the texture gate is holding back — geometry ready, atlas not yet decoded. Stamp it seen too, so evict
        // spares it AND `publish` emits it HIDDEN (untextured ⇒ `visible == false`). The renderer builds it hidden
        // and decodes its atlas UNDER the coarse parent the kick is still drawing, then reports it textured — only
        // then does the gate let it replace the parent. Without this stamp the tile would be load-only
        // (unpublished) → never reach the renderer → never texture → the kick would hold the coarse forever (the
        // texture-gate deadlock). Truly-unloaded `load` tiles (no mesh yet) aren't `Resident`, so they're skipped
        // here and simply fetched as before.
        for lt in &sel.load {
            let e = self.streaming.as_ref().unwrap().index.get(&lt.node).copied();
            if let Some(e) = e {
                if self.world.get::<TileState>(e) == Some(&TileState::Resident) {
                    if let Some(mut ls) = self.world.get_mut::<LastSeen>(e) {
                        ls.0 = view.frame;
                    }
                    draw_set.push(e); // prepare tile: published HIDDEN (render and load sets are disjoint)
                }
            }
        }
        // Hand the draw set to `publish` (and flip it active — the legacy all-resident fallback is only for
        // selection-less foundation worlds).
        {
            let mut ps = self.world.resource_mut::<PublishSet>();
            ps.entities = draw_set;
            ps.active = true;
        }
        // Keep each rendered/loading tile's grafted ancestors "warm" so `prune_cold_grafts` only reclaims
        // subtrees the camera has truly left — never one with a live descendant this frame. Runs before
        // `sel.load` is consumed below; O(depth) u32 ancestor hops per tile, no allocation, no byte hashing
        // (the old per-prefix path probes cost O(d²) hashed bytes per tile — audit §3.2).
        {
            let frame = view.frame;
            let s = self.streaming.as_mut().unwrap();
            for t in sel.render.iter().chain(sel.load.iter()) {
                bump_graft_activity(&mut s.grafts.records, &s.arena, t.node, frame);
            }
        }
        // Preemptive cancel: any in-flight fetch whose tile is no longer selected (the camera moved on before
        // it arrived) is wasted bandwidth — cancel it and reclaim its slot, freeing in-flight budget for the
        // tiles we DO want now. `wanted` borrows `sel.load`, so scope it before we sort `sel.load` below.
        {
            let wanted: HashSet<u32> = sel.load.iter().map(|t| t.node).collect();
            self.cancel_stale(&wanted);
        }
        // Priority: when the in-flight budget saturates, fetch the tiles that matter most first. The score
        // ([`fetch_priority`]) blends three factors: screen importance `geometric_error / distance²` (the
        // SQUARED falloff concentrates the budget on one region — coarse stand-ins lead their own fine
        // detail), a gentle view-direction lobe (ahead 1.0× … behind `FETCH_LOBE_MIN`×) so a tile in the look
        // direction outranks an equal one off to the side, and the manifest graft boost
        // (`MANIFEST_FETCH_BOOST`) so tree-descending tileset fetches lead the geometry they unlock.
        // `cam_dir == ZERO` (no direction known, e.g. the headless test path) makes the lobe uniform → pure
        // screen-importance order. Sorted **descending** (highest score first).
        //
        // Decorate-sort: each tile's `fetch_priority` (a normalize + dot + div) is evaluated ONCE here, not
        // twice per comparison inside the comparator — O(n) score evals instead of O(n·log n) (§1E). The
        // f64 key is precomputed and finite (importance is finite, the lobe ∈ [0.55, 1.0]), so the
        // comparator is a bare `partial_cmp` with a NaN-safe fallback.
        let cam_pos = view.camera_ecef;
        let cam_dir = self.streaming.as_ref().unwrap().cam_dir_ecef;
        let mut ranked: Vec<(f64, SelectedTile)> = {
            let s = self.streaming.as_ref().unwrap();
            std::mem::take(&mut sel.load)
                .into_iter()
                .map(|t| {
                    let mut pr = fetch_priority(&t, cam_pos, cam_dir);
                    // Depth-first concentration (audit E): boost a tile whose PARENT is already resident/grafted
                    // so the budget DESCENDS a region already underway before broadening to fresh ones — finishes
                    // the focal region (which started first via B + the foveation lobe) to fine instead of
                    // crawling every region up together. The parent's presence is the top-down-graft precondition,
                    // so this only breaks ties in the ~flat altitude field; it can't reorder a parent after a child.
                    if let Some(parent) = s.arena.parent(t.node) {
                        if parent != logi_3dtiles_select::ROOT_NODE
                            && (s.index.contains_key(&parent) || s.grafts.records.contains_key(&parent))
                        {
                            pr *= CONCENTRATE_BOOST;
                        }
                    }
                    (pr, t)
                })
                .collect()
        };
        ranked.sort_unstable_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        // For each selected-but-unloaded tile: skip if already accounted for, else try a warm restore from the
        // flash cache, else the PERSISTED geometry tier (M3), else (cold) enqueue a fetch. `geom_restores` caps
        // the M3 synchronous disk reads per step (back-pressure for a cold-start revisit burst).
        let mut geom_restores = 0usize;
        // H6: at most ONE preemption per step — once we've cancelled a lower-priority in-flight fetch for a
        // winner, any later at-cap tile is fetch-blocked, keeping the scan O(in_flight). `preemption_closed`
        // remembers a failed margin test: the loop runs high→low priority, so once one tile can't beat the
        // lowest in-flight by the margin, no later tile can either — never scan `in_flight` again this step.
        let mut preempted_this_step = false;
        let mut preemption_closed = false;
        // E: cap NEW cold fetches issued this step (concentration — see MAX_NEW_FETCHES_PER_STEP). C6 (audit
        // 2026-06-10): exhausting the FETCH budget no longer `break`s the loop — flash/geom RESTORES consume no
        // in-flight budget, so lower-ranked tiles whose meshes sit ready in the caches keep restoring; only
        // fetch ISSUANCE stops. The cheap skip-checks bound the residual per-tile cost.
        let mut new_fetches = 0usize;
        let mut fetch_blocked = false; // at-cap with nothing preemptible, or the new-fetch budget is spent
        let mut fetch_backlog = false; // a fetch-worthy candidate was left unfunded → force a follow-up step
        let mut skip_grafted = 0u32; // X-ray: load candidates a `grafted` record short-circuits (see GeoStats)
        for (score, lt) in &ranked {
            // Already known / in-flight / grafted / terminally-bad → nothing to do. O(1) u32 probes (§3.2).
            {
                let s = self.streaming.as_ref().unwrap();
                // The graft skip applies ONLY to TILESET candidates (its purpose: never re-fetch/re-graft a
                // tileset.json that is already applied). It used to skip on the node id alone — but grafting
                // REPLACES the node's content with the grafted ROOT's content (usually a GLB), so the blanket
                // skip made every grafted node's OWN MESH permanently unfetchable. Where the cut wanted exactly
                // that node's LOD the region could never converge — kicked/held coarse forever: THE stuck-coarse
                // scar the 2026-06-11 X-ray caught (fetch_skip_grafted 600–870 + hold_loading ~700 sustained at
                // "settled" stations; a slight camera move flipping a view between perfect and unconvergeable).
                if lt.content_uris.first().is_some_and(|u| is_tileset_uri(u)) && s.grafts.records.contains_key(&lt.node)
                {
                    skip_grafted += 1;
                    continue;
                }
                if s.index.contains_key(&lt.node)
                    || s.in_flight_nodes.contains(&lt.node)
                    || s.failures.decode_failed.contains(&lt.node)
                    || s.failures.fetch_attempts.get(&lt.node).is_some_and(|&n| n >= MAX_FETCH_ATTEMPTS)
                    // Transient-failure backoff (audit A1): not yet time to retry this rate-limited/5xx tile.
                    || s.failures.next_retry.get(&lt.node).is_some_and(|&t| s.now_instant < t)
                {
                    continue;
                }
            }
            // Only GLB (geometry) tiles are ever cached; a tileset-graft (`.json`) or empty-content tile can
            // never be a flash/geom HIT, so skip the restore probes for them — they would be pure denominator
            // understating the very hit-rate L1 exposes, plus two wasted lookups (audit recheck L-5). They fall
            // straight through to the graft/fetch path below.
            if lt.content_uris.first().is_some_and(|u| !is_tileset_uri(u)) {
                // Warm re-entry: restore the decoded mesh straight from the flash cache (no fetch, no decode).
                // `restore_cached` does the single `take` itself (audit L5; CLAUDE.md §1E) and does NOT consume an
                // in-flight slot, so it's tried regardless of the fetch cap (audit C6). `true` ⇒ restored OR
                // cached-but-no-slot (don't cold-fetch a tile we already hold decoded); `false` ⇒ genuine miss.
                // NOTE: a miss here is NOT counted — an unfunded candidate re-probes on EVERY full step, so
                // per-probe counting inflated the denominator into the tens of millions (0.0% rates for caches
                // that were working fine — 2026-06-11). Misses are counted ONCE, at fetch ISSUE below: the
                // moment the pipeline actually pays the cold cost the warm tiers failed to save.
                if self.restore_cached(lt, lt.key, view.frame) {
                    continue;
                }
                // M3: before paying for a network fetch + Draco decode, try the PERSISTED decoded-geometry tier —
                // a hit deserializes the geometry straight onto a slot (no network, no decode), capped per step.
                if geom_restores < MAX_GEOM_RESTORES_PER_STEP && self.try_restore_from_geom(lt, lt.key, view.frame) {
                    geom_restores += 1;
                    continue;
                }
            }
            // Cold: the fetch path. Once fetch issuance is blocked (cap with nothing preemptible / budget spent /
            // pool exhausted), remaining candidates are NOT fetched this step — but the loop continues so their
            // restore probes above still run (the C6 fix: the old `break` starved cache-ready tiles too). Record
            // the backlog so the re-selection gate (§3.1) runs a follow-up full step.
            if fetch_blocked {
                fetch_backlog = true;
                continue;
            }
            // At the cap, PREEMPT (audit H6): the ranked loop runs high→low priority, so if even THIS tile can't
            // beat the lowest-priority in-flight fetch by a clear margin, no later tile can either — close
            // preemption and block fetches. If it can, cancel that lowest in-flight to free a slot for this more
            // render-critical tile, then fall through to enqueue it. One preemption per step (O(in_flight) once).
            {
                let at_cap = {
                    let s = self.streaming.as_ref().unwrap();
                    s.source.in_flight() >= s.max_in_flight
                };
                if at_cap {
                    if preempted_this_step || preemption_closed {
                        fetch_blocked = true;
                        fetch_backlog = true;
                        continue;
                    }
                    match self.lowest_priority_inflight() {
                        Some((min_id, min_pri)) if *score > min_pri * PREEMPT_PRIORITY_MARGIN => {
                            self.preempt_inflight(min_id);
                            preempted_this_step = true;
                        }
                        _ => {
                            // Nothing worth preempting (or this tile doesn't clearly win) — no later tile wins
                            // either (priority-sorted), so fetch issuance is done this step.
                            preemption_closed = true;
                            fetch_blocked = true;
                            fetch_backlog = true;
                            continue;
                        }
                    }
                }
            }
            let Some(uri) = lt.content_uris.first() else {
                continue; // a selected tile with no content URI — nothing to fetch
            };
            let kind = if is_tileset_uri(uri) {
                ContentKind::Tileset
            } else {
                ContentKind::Glb
            };
            let resolved = {
                let s = self.streaming.as_ref().unwrap();
                resolve_url(base_for_node(&s.base_url, &s.grafts.bases, &s.arena, lt.node), uri)
            };
            let full = match resolved {
                Ok(full) => full,
                Err(_) => {
                    // The content URI couldn't be resolved against its (grafted) base (malformed relative URL /
                    // missing subtree base). The tile is never fetched → its region stays coarse. Count it (was
                    // silent) — see `url_resolve_failed_total`.
                    self.world.resource_mut::<GeoStats>().url_resolve_failed_total += 1;
                    continue;
                }
            };
            // Content[0] reserves the slot + drives refinement; its additional contents (1.1 multiple-contents,
            // empty for the common case) ride the same entity. A tileset graft reserves no entity → no extras.
            // `PoolExhausted` issues NO fetch — it must not burn the new-fetch budget (audit §4.1); it blocks
            // further fetches (no slot will appear mid-step) and leaves a backlog.
            match self.request_one(lt, kind, full, *score) {
                Requested::Issued(entity) => {
                    // Warm-tier MISS accounting (one per actually-issued cold GLB fetch — see the probe-site
                    // note above): this fetch is exactly the cost a flash/geom hit would have saved, so the
                    // hit-rates read hits / (hits + real cold fetches), not hits / re-probes-per-step.
                    if kind == ContentKind::Glb {
                        let stats = &mut self.world.resource_mut::<GeoStats>();
                        stats.flash_cache_miss_total += 1;
                        stats.geom_cache_miss_total += 1;
                    }
                    if let Some(e) = entity {
                        if lt.content_uris.len() > 1 {
                            self.request_extra(e, lt, *score);
                        }
                    }
                    // E per-step new-fetch cap: fund only the top-K cold tiles this step so a burst can't flood
                    // the pool/in-flight with a thin cross-region spread. The rest re-rank next step.
                    new_fetches += 1;
                    if new_fetches >= MAX_NEW_FETCHES_PER_STEP {
                        fetch_blocked = true;
                    }
                }
                Requested::PoolExhausted => {
                    self.world.resource_mut::<GeoStats>().pool_exhausted_total += 1;
                    fetch_blocked = true;
                    fetch_backlog = true;
                }
            }
        }
        self.world.resource_mut::<GeoStats>().fetch_skip_grafted = skip_grafted; // X-ray snapshot
                                                                                 // M4: predictive prefetch along the camera's motion (render lane — determinism not required). Runs AFTER
                                                                                 // the reactive fetches so it consumes only SPARE in-flight budget, and is bounded per step.
        self.prefetch_along_motion(&view);
        // Carry this pass's per-tile results forward as next frame's `prev_state` (drives the kick + the
        // hysteresis dead-band). Move it in — the prior state map is dropped.
        {
            let s = self.streaming.as_mut().unwrap();
            s.sel_state = std::mem::take(&mut sel.state);
            // §3.1 follow-up trigger: unfunded fetch candidates remain — the next advance must be a full step
            // (no external event will announce the budget this loop left on the table).
            if fetch_backlog {
                s.force_full_next = true;
            }
        }
    }

    /// Warm re-entry: try to restore a decoded mesh straight from [`FlashCache`] onto a fresh pool slot,
    /// skipping both the fetch and the decode. Returns `true` if it **handled** the tile — either restored it,
    /// or found it cached but had no free slot right now (back-pressure: the caller must NOT cold-fetch a tile
    /// we already hold decoded; it restores on a later frame as eviction frees slots) — and `false` on a genuine
    /// cache miss, where the caller falls through to a cold fetch. The restored tile is `Resident`, stamped
    /// `LastSeen = frame`, so it is spared by `evict` and snapshotted by `publish` this same step.
    fn restore_cached(&mut self, lt: &SelectedTile, key: u64, frame: u64) -> bool {
        // Secure a free slot FIRST (pop, not check-then-pop), so the pop can never race a concurrent drain into
        // an `unwrap` panic. If none is free, the tile is still "handled" IFF it is cached — probe membership
        // only in this rare back-pressure branch (the steady warm-hit path below needs just the single `take`).
        let Some(e) = self.world.resource_mut::<FreeList>().0.pop() else {
            let cached = self.world.resource::<FlashCache>().contains_key(key);
            if cached {
                // Cached-but-no-slot: the restore is deferred until eviction frees a slot. Counted — a
                // sustained climb here (with `evicted_total` flat) is the pool-starvation signature.
                self.world.resource_mut::<GeoStats>().pool_exhausted_total += 1;
            }
            return cached;
        };
        // Slot secured: a single `take` resolves hit-vs-miss (no prior `contains_key` on the hot path). On a
        // miss, return the slot to the pool (never leak a popped-but-unused entity) and report "not handled".
        let Some(geom) = self.world.resource_mut::<FlashCache>().take(key) else {
            self.world.resource_mut::<FreeList>().0.push(e);
            return false;
        };
        self.place_decoded(e, lt, key, frame, geom);
        self.world.resource_mut::<GeoStats>().flash_cache_hits_total += 1;
        true
    }

    /// Place an ALREADY-decoded `geom` onto a pre-popped free slot `e` as `Resident` — the shared tail of a warm
    /// flash restore and a persisted-geometry-tier restore (audit M3). Wires the entity's components, indexes the
    /// path, marks it resident + seen this frame (so `evict` spares it + `publish` emits it), and re-fetches any
    /// 1.1 additional contents (the tiers hold only content[0]). The CALLER pops the slot (so each tier handles
    /// no-slot back-pressure its own way) and attributes the hit to its tier — this helper bumps no counter.
    fn place_decoded(&mut self, e: Entity, lt: &SelectedTile, key: u64, frame: u64, geom: Arc<TileGeometry>) {
        {
            let mut ent = self.world.entity_mut(e);
            *ent.get_mut::<TileId>().unwrap() = TileId(key);
            *ent.get_mut::<TileBounds>().unwrap() = TileBounds {
                center: lt.bounds_center,
                radius: lt.bounds_radius,
            };
            *ent.get_mut::<GeometricError>().unwrap() = GeometricError(lt.geometric_error);
            *ent.get_mut::<Depth>().unwrap() = Depth(lt.path.len() as u32);
            *ent.get_mut::<TileTransform>().unwrap() = TileTransform(lt.world_transform);
            *ent.get_mut::<TileState>().unwrap() = TileState::Resident;
            *ent.get_mut::<LastSeen>().unwrap() = LastSeen(frame);
            ent.get_mut::<ResidentMesh>().unwrap().0 = Some(geom);
            // Recycled slot: clear any previous tenant's additional-content parts (no-op for single-content).
            ent.get_mut::<MultiContent>().unwrap().0.clear();
            *ent.get_mut::<TileNode>().unwrap() = TileNode(lt.node);
            // `FetchedBytes` is already `None` on every free slot (decode clears it; evict/reclaim never set
            // it), so leave it untouched — a restore goes straight to `Resident` and is never routed through
            // [`JustFetched`]/`decode` (which only ever sees freshly-fetched tiles from `drain_raw`).
        }
        self.streaming.as_mut().unwrap().index.insert(lt.node, e);
        self.world.resource_mut::<ResidentSet>().0.insert(e); // Free→Resident: into BOTH working sets
        self.world.resource_mut::<LiveSet>().0.insert(e); // (ADR-0018 active set)
                                                          // The tiers hold only content[0]; a multiple-contents tile must re-fetch its additional contents so they
                                                          // render again alongside the restored primary (no-op for single-content tiles).
        if lt.content_uris.len() > 1 {
            // The restored tile is selected + rendering — fetch its extras at the tile's own render priority.
            let pr = {
                let cam = self.world.resource::<GeoView>().camera_ecef;
                let dir = self.streaming.as_ref().map(|s| s.cam_dir_ecef).unwrap_or(DVec3::ZERO);
                fetch_priority(lt, cam, dir)
            };
            self.request_extra(e, lt, pr);
        }
    }

    /// Try the PERSISTED decoded-geometry tier (audit M3): if this tile's geometry is on disk under
    /// `(origin, tile_key)`, deserialize it and restore it onto a slot — skipping BOTH the network fetch and the
    /// dominant Draco decode + ENU rebase. Returns `true` if restored. A miss / corrupt blob counts a geom miss
    /// and returns `false` (caller fetches + decodes); a momentary no-slot drops the read and returns `false`
    /// (retried next step). No-op (`false`) when the tier is disabled.
    fn try_restore_from_geom(&mut self, lt: &SelectedTile, key: u64, frame: u64) -> bool {
        let Some(gc) = self.world.get_resource::<GeomCacheRes>().and_then(|g| g.0.clone()) else {
            return false; // tier disabled
        };
        // Secure a free slot FIRST (cheap) — like `restore_cached` — so a no-slot step does NOT pay the full
        // file read + deserialize it would only discard, and the caller's MAX_GEOM_RESTORES_PER_STEP cap then
        // genuinely bounds the synchronous disk I/O (audit recheck M-2). No slot ⇒ not handled, retry next step
        // (uncounted — the data IS on disk, this is back-pressure, not a miss).
        let Some(e) = self.world.resource_mut::<FreeList>().0.pop() else {
            return false;
        };
        let origin = self.world.resource::<GeoEnu>().0.origin_ecef;
        // `get_raw` only reads the file on a key HIT; a miss is a cheap index probe (no file read).
        // Misses are NOT counted here (per-probe counting re-counted the same unfunded candidate every full
        // step — the 0.0%-rate inflation, 2026-06-11); the ONE per-cold-fetch miss lands at fetch issue.
        let Some(bytes) = gc.get_raw(geom_key(origin, key)) else {
            self.world.resource_mut::<FreeList>().0.push(e); // return the unused slot — nothing to restore
            return false; // not persisted → caller fetches + decodes
        };
        let Some(geom) = crate::streaming::deserialize_geometry(&bytes) else {
            self.world.resource_mut::<FreeList>().0.push(e);
            return false; // stale-format / corrupt blob → fetch + decode
        };
        self.place_decoded(e, lt, key, frame, Arc::new(geom));
        self.world.resource_mut::<GeoStats>().geom_cache_hits_total += 1;
        true
    }

    /// Predictive prefetch (audit M4): if the camera is moving, run a second, COARSER SSE traversal at a point
    /// projected ahead along the dt-free motion vector and enqueue the tiles it needs — from SPARE in-flight
    /// budget only — so their fetch+decode completes before the camera arrives (hiding the latency that
    /// otherwise shows the coarse-ancestor blur on first approach at speed). Best-effort + bounded: it never
    /// touches the real hysteresis state (a throwaway [`SelectionState`]), never preempts a reactive fetch (it
    /// runs after them, gated on the in-flight cap), and is capped per step. The render lane needs no determinism
    /// (owner's call), so the lookahead is a plain displacement — no wall-clock velocity.
    fn prefetch_along_motion(&mut self, view: &GeoView) {
        let cam = view.camera_ecef;
        // Interval gate (audit 2026-06-10 §2): the lookahead is a SECOND full traversal, and its
        // 8×-displacement target moves slowly relative to full-step cadence — every Nth full step warms the
        // same tiles at 1/N of the traversal cost. The keep-set still AGES every call (cheap: an entry stays
        // only while its speculative fetch is in flight — re-eval 2026-06-09), so completed prefetches
        // unprotect promptly even on the skipped steps; stale-but-still-flying speculation is reclaimed by H6
        // preemption (prefetches carry the −PREFETCH_PRIORITY_DEMOTE score, so they lose first).
        {
            let s = self.streaming.as_mut().unwrap();
            let in_flight = &s.in_flight_nodes;
            s.prefetch.live_nodes.retain(|n| in_flight.contains(n));
            s.prefetch.steps_since_run = s.prefetch.steps_since_run.saturating_add(1);
            if s.prefetch.steps_since_run < PREFETCH_INTERVAL_STEPS {
                return;
            }
        }
        let prev = self.streaming.as_ref().unwrap().prefetch.prev_camera_ecef;
        {
            // Update the motion anchor for the next prefetch pass (the displacement spans the whole interval,
            // so the lookahead distance is interval-invariant for a given camera speed).
            let s = self.streaming.as_mut().unwrap();
            s.prefetch.prev_camera_ecef = Some(cam);
            s.prefetch.steps_since_run = 0;
        }
        let Some(prev) = prev else { return }; // first prefetch pass — no motion vector yet
        let motion = cam - prev;
        if motion.length() < PREFETCH_MIN_MOTION_M {
            return; // ~parked: the reactive selection already covers the view
        }
        // No spare budget → don't speculate; the reactive fetches own the in-flight slots this step.
        if self.streaming.as_ref().unwrap().source.in_flight() >= self.streaming.as_ref().unwrap().max_in_flight {
            return;
        }
        let look = cam + motion * PREFETCH_LOOKAHEAD;
        // Second traversal at the lookahead point → the paths it would load. Throwaway hysteresis state + a local
        // `needed` (we don't fetch the prefetch walk's subtrees — the reactive walk owns subtree fetching).
        let mut throwaway_needed: Vec<(String, u32)> = Vec::new();
        let throwaway_state = SelectionState::new();
        let load: Vec<SelectedTile> = {
            let GeoStreaming {
                arena,
                root,
                ellipsoid,
                subtrees,
                grafts,
                base_url,
                index,
                failures,
                max_tile_distance,
                ..
            } = self.streaming.as_mut().unwrap();
            let SubtreeStreams {
                cache: subtree_cache,
                failed: subtree_failed,
                ..
            } = &*subtrees;
            let FailureLedger {
                decode_failed,
                fetch_attempts,
                ..
            } = &*failures;
            let bases = &grafts.bases;
            let world = &self.world;
            let textured = &world.resource::<TexturedSet>().0;
            let params = SelectParams {
                camera_position: look,
                fov_y_radians: view.fov_y_radians,
                viewport_height: view.viewport_height.max(1.0),
                // Coarser than the live view: prefetch the APPROACH LOD, not full detail — cheaper, and the
                // reactive pass refines to full detail once the camera actually arrives. The predicate below
                // is accept-all-Visible, so the Distant/Offscreen targets are inert here (kept consistent).
                max_screen_space_error: view.max_sse * PREFETCH_SSE_COARSEN_FACTOR,
                distant_screen_space_error: view.max_sse * PREFETCH_SSE_COARSEN_FACTOR,
                culled_screen_space_error: (view.max_sse * CULLED_SSE_FACTOR).max(CULLED_SSE_FLOOR_PX),
                hysteresis: LOD_HYSTERESIS,
            };
            let max_d = *max_tile_distance;
            let mut provider = SubtreeAvailability {
                cache: subtree_cache,
                failed: subtree_failed,
                base_url,
                bases,
                needed: &mut throwaway_needed,
            };
            let sel = select_tree_implicit(
                root,
                ellipsoid,
                &params,
                arena,
                &mut |_, n| {
                    index.get(&n).is_some_and(|&e| {
                        world.get::<TileState>(e) == Some(&TileState::Resident)
                            && world.get::<TileId>(e).is_some_and(|id| textured.contains(&id.0))
                    })
                },
                &mut |_, n| {
                    decode_failed.contains(&n) || fetch_attempts.get(&n).is_some_and(|&c| c >= MAX_FETCH_ATTEMPTS)
                },
                // Accept-all within the far plane (no frustum cull): warm the tiles around where the camera is
                // HEADED, not only what its current frustum faces.
                &mut |c, r| {
                    if (look - c).length() - r >= max_d {
                        Visibility::Culled
                    } else {
                        Visibility::Visible
                    }
                },
                &throwaway_state,
                &mut provider,
            );
            sel.load
        };
        // Enqueue from spare budget only, deduped against everything already known/in-flight/terminal, capped.
        let mut prefetched = 0usize;
        for lt in &load {
            if prefetched >= MAX_PREFETCH_PER_STEP {
                break;
            }
            {
                let s = self.streaming.as_ref().unwrap();
                if s.source.in_flight() >= s.max_in_flight {
                    break; // spare budget consumed
                }
                if s.index.contains_key(&lt.node)
                    || s.grafts.records.contains_key(&lt.node)
                    || s.in_flight_nodes.contains(&lt.node)
                    || s.failures.decode_failed.contains(&lt.node)
                    || s.failures
                        .fetch_attempts
                        .get(&lt.node)
                        .is_some_and(|&n| n >= MAX_FETCH_ATTEMPTS)
                    || s.failures.next_retry.get(&lt.node).is_some_and(|&t| s.now_instant < t)
                {
                    continue; // already accounted for / in flight / terminal / backing off
                }
            }
            // A pure prefetch only warms the NETWORK+disk byte cache (the bytes land in the disk cache; the
            // reactive pass decodes them on arrival), so just enqueue the fetch — no flash/geom restore here.
            let Some(uri) = lt.content_uris.first() else { continue };
            // Prefetch GLB geometry ONLY. Speculatively grafting a child tileset ahead of the camera is dubious
            // value, and a `Tileset` request reserves no entity, which would let tileset prefetches BYPASS the
            // per-step cap (audit recheck L-6). `request_extra` skips tileset extras for the same reason.
            if is_tileset_uri(uri) {
                continue;
            }
            let resolved = {
                let s = self.streaming.as_ref().unwrap();
                resolve_url(base_for_node(&s.base_url, &s.grafts.bases, &s.arena, lt.node), uri)
            };
            if let Ok(full) = resolved {
                // Speculative: demote below every reactive tile so the priority pool serves the live view first.
                let pr =
                    fetch_priority(lt, look, self.streaming.as_ref().unwrap().cam_dir_ecef) - PREFETCH_PRIORITY_DEMOTE;
                if matches!(
                    self.request_one(lt, ContentKind::Glb, full, pr),
                    Requested::Issued(Some(_))
                ) {
                    // Protect this speculative fetch from the next advance's `cancel_stale` until it can complete
                    // + land in the disk cache (audit recheck M-1) — it sits outside the reactive `sel.load`.
                    self.streaming.as_mut().unwrap().prefetch.live_nodes.insert(lt.node);
                    prefetched += 1;
                }
            }
        }
    }

    /// Pre-emptively cancel in-flight fetches whose tile is no longer in this frame's selection — the
    /// fast-motion case where the camera leaves a region before its tiles arrive. `wanted` is the set of
    /// still-selected unloaded nodes (`sel.load`); any in-flight node outside it is stale: cancel the fetch,
    /// drop its `in_flight`/`index` bookkeeping, and return its reserved GLB slot to the pool. This frees
    /// in-flight budget for the tiles we actually want now (paired with the priority sort in the caller).
    fn cancel_stale(&mut self, wanted: &HashSet<u32>) {
        // Phase 1 — collect the stale fetches (immutable borrow; `wanted` borrows the caller's `sel.load`).
        // `content_index` distinguishes the primary (owns the slot + the path's bookkeeping) from a 1.1
        // additional content riding the same entity — so phase 3 frees the slot exactly once (no double-free).
        let stale: Vec<(RequestId, u32, Option<Entity>, u32)> = {
            let s = self.streaming.as_ref().unwrap();
            s.in_flight
                .iter()
                // Keep a tile alive if it is in this frame's reactive load OR was just speculatively prefetched
                // (audit recheck M-1) — otherwise M4's lookahead fetches, which sit outside `wanted` by design,
                // would be cancelled on the very next advance before they could complete + warm the cache.
                .filter(|(_, inf)| !wanted.contains(&inf.node) && !s.prefetch.live_nodes.contains(&inf.node))
                .map(|(&id, inf)| (id, inf.node, inf.entity, inf.content_index))
                .collect()
        };
        if stale.is_empty() {
            return;
        }
        // Phase 2 — cancel each fetch + drop its streaming bookkeeping (one `streaming` borrow). Only the
        // primary touches `index`/`in_flight_nodes` (extras never inserted there); guarding keeps it honest.
        {
            let s = self.streaming.as_mut().unwrap();
            for (id, node, _, content_index) in &stale {
                s.source.cancel(*id);
                s.in_flight.remove(id);
                if *content_index == 0 {
                    s.in_flight_nodes.remove(node);
                    s.index.remove(node);
                }
            }
        }
        // Phase 3 — return each reserved GLB slot to the pool ONCE, via its primary content only (a tileset
        // fetch has no entity; an extra rides the primary's slot and must not free it again).
        for (_, _, entity, content_index) in &stale {
            if let (Some(e), 0) = (entity, content_index) {
                self.free_slot(*e, false); // an in-flight (un-decoded) slot was never Resident
            }
        }
        self.world.resource_mut::<GeoStats>().fetches_cancelled_total += stale.len() as u32;
    }

    /// The lowest-priority PRIMARY (`content_index == 0`) in-flight fetch — the H6 preemption candidate. Only
    /// primaries are considered: a primary owns its pool entity + path bookkeeping, so cancelling it frees BOTH
    /// a cap slot AND a pool slot for the newcomer (an extra rides the primary's slot — cancelling it frees no
    /// entity). `None` if no primary is in flight. O(in_flight) scan, run at most once per step (at the cap).
    fn lowest_priority_inflight(&self) -> Option<(RequestId, f64)> {
        let s = self.streaming.as_ref()?;
        s.in_flight
            .iter()
            .filter(|(_, inf)| inf.content_index == 0)
            .map(|(&id, inf)| (id, inf.priority))
            .min_by(|a, b| a.1.total_cmp(&b.1))
    }

    /// Cancel one in-flight PRIMARY fetch to free its slot for a more-urgent newcomer (audit H6 preemption).
    /// Mirrors [`cancel_stale`](Self::cancel_stale)'s per-tile teardown for a single id: abort the download (the
    /// worker's cancel flag → it stops at the next chunk; a result that still lands is soft-dropped by
    /// `drain_raw` since the id is gone from `in_flight`), drop the path bookkeeping, and return the pool slot.
    fn preempt_inflight(&mut self, id: RequestId) {
        let entity = {
            let s = self.streaming.as_mut().unwrap();
            let Some(inf) = s.in_flight.remove(&id) else {
                return;
            };
            s.source.cancel(id);
            // Only ever a primary (`lowest_priority_inflight` filters content_index == 0): it owns these.
            s.in_flight_nodes.remove(&inf.node);
            s.index.remove(&inf.node);
            inf.entity
        };
        if let Some(e) = entity {
            self.free_slot(e, false); // an in-flight (un-decoded) slot was never Resident
        }
        self.world.resource_mut::<GeoStats>().fetches_cancelled_total += 1;
    }

    /// Acquire a pool entity for a GLB tile (none for an external tileset — it grafts) and enqueue its fetch
    /// for content `[0]`. Returns [`Requested::Issued`] with the reserved entity (`None` for a tileset graft —
    /// the fetch IS issued, just slot-less), or [`Requested::PoolExhausted`] when no fetch was issued at all —
    /// the caller must not count it against the per-step new-fetch budget (audit 2026-06-10 §4.1).
    fn request_one(&mut self, lt: &SelectedTile, kind: ContentKind, full: String, priority: f64) -> Requested {
        let entity = if kind == ContentKind::Glb {
            let Some(e) = self.world.resource_mut::<FreeList>().0.pop() else {
                return Requested::PoolExhausted; // no slot, NO fetch issued (grows at a step boundary later)
            };
            let mut ent = self.world.entity_mut(e);
            *ent.get_mut::<TileId>().unwrap() = TileId(lt.key);
            *ent.get_mut::<TileNode>().unwrap() = TileNode(lt.node);
            *ent.get_mut::<TileBounds>().unwrap() = TileBounds {
                center: lt.bounds_center,
                radius: lt.bounds_radius,
            };
            *ent.get_mut::<GeometricError>().unwrap() = GeometricError(lt.geometric_error);
            *ent.get_mut::<Depth>().unwrap() = Depth(lt.path.len() as u32);
            *ent.get_mut::<TileTransform>().unwrap() = TileTransform(lt.world_transform);
            *ent.get_mut::<TileState>().unwrap() = TileState::Requested;
            *ent.get_mut::<LastSeen>().unwrap() = LastSeen(0);
            // Recycled slot: drop any previous tenant's additional-content parts (no-op for single-content,
            // where this vec is always empty). The primary mesh slot (`ResidentMesh`) is overwritten by `decode`.
            ent.get_mut::<MultiContent>().unwrap().0.clear();
            Some(e)
        } else {
            None
        };
        if let Some(e) = entity {
            self.world.resource_mut::<LiveSet>().0.insert(e); // Free→Requested (ADR-0018 active set)
        }
        let s = self.streaming.as_mut().unwrap();
        let id = s.next_id;
        s.next_id += 1;
        s.source.request(id, &full, priority, 0);
        if let Some(e) = entity {
            s.index.insert(lt.node, e);
        }
        s.in_flight_nodes.insert(lt.node);
        s.in_flight.insert(
            id,
            InFlight {
                node: lt.node,
                kind,
                url: full,
                entity,
                content_index: 0,
                priority,
            },
        );
        Requested::Issued(entity)
    }

    /// 3D Tiles 1.1 multiple-contents: enqueue the tile's ADDITIONAL contents (`content_uris[1..]`) onto the
    /// SAME entity `e` reserved by [`request_one`](Self::request_one). Each gets a [`MultiContent`] part keyed by
    /// [`content_key`] (filled when the fetch lands, in [`drain_raw`](Self::drain_raw)) and an in-flight fetch
    /// tagged with its `content_index` (so the result routes to the part, not the primary slot). External-tileset
    /// extras are not supported (a tile's extra contents are renderable GLB content, not nested tilesets) and are
    /// skipped. Respects the outstanding-fetch cap — an extra skipped at the cap is DROPPED for this tile's
    /// residency (not re-tried: once the primary is in flight/resident, `select_request`'s skip sets prevent the
    /// tile from reaching this method again — re-eval 2026-06-09). Zero impact on Google P3DT (single-content
    /// tiles throughout); if a general-1.1 multi-content set ever matters, re-drive skipped extras from
    /// `drain_raw`'s primary-arrival lane instead of widening the cap here. No-op for single-content tiles.
    fn request_extra(&mut self, e: Entity, lt: &SelectedTile, priority: f64) {
        let extras: Vec<(usize, String)> = lt.content_uris.iter().skip(1).cloned().enumerate().collect::<Vec<_>>();
        if extras.is_empty() {
            return;
        }
        for (i0, uri) in extras {
            let content_index = i0 as u32 + 1; // skip(1) ⇒ enumerate starts at 0 for content[1]
            if is_tileset_uri(&uri) {
                continue; // an extra content that is itself an external tileset is out of scope — don't graft it
            }
            // Respect the outstanding-fetch cap (checked per extra so one tile can't blow the budget).
            {
                let s = self.streaming.as_ref().unwrap();
                if s.source.in_flight() >= s.max_in_flight {
                    return;
                }
            }
            let resolved = {
                let s = self.streaming.as_ref().unwrap();
                resolve_url(base_for_node(&s.base_url, &s.grafts.bases, &s.arena, lt.node), &uri)
            };
            let Ok(full) = resolved else {
                self.world.resource_mut::<GeoStats>().url_resolve_failed_total += 1;
                continue;
            };
            // Park an (unfilled) part for this content so `publish` knows it exists; the mesh lands on arrival.
            let key = content_key(lt.key, content_index);
            if let Some(mut mc) = self.world.get_mut::<MultiContent>(e) {
                if mc.0.iter().all(|p| p.key != key) {
                    mc.0.push(ExtraContent { key, mesh: None });
                }
            }
            let s = self.streaming.as_mut().unwrap();
            let id = s.next_id;
            s.next_id += 1;
            s.source.request(id, &full, priority, 0);
            // NB: extras do NOT touch `index`/`in_flight_nodes`/retry state — the primary (content[0]) owns the
            // tile's bookkeeping; an extra only routes its bytes back to the entity it shares.
            s.in_flight.insert(
                id,
                InFlight {
                    node: lt.node,
                    kind: ContentKind::Glb,
                    url: full,
                    entity: Some(e),
                    content_index,
                    priority,
                },
            );
        }
    }

    /// Reconcile the node→entity `index` with the post-schedule entity exits — the one place a tile leaves
    /// the index. The `evict`/`decode` systems mutate entity *state* but can't touch `streaming` (they're
    /// `bevy_ecs` systems), so they hand the entities they touched back through [`Reclaim`], and this drains
    /// exactly those — **true O(Δ)** (audit 2026-06-10 §2): the old version scanned the WHOLE index on any
    /// step with an eviction or decode failure, so a sustained fly-through paid O(index) per step.
    ///
    /// - **Evicted** (now `Free`): `evict` deposited the mesh into [`FlashCache`] and freed the slot; drop the
    ///   stale `index` entry (by the entity's [`TileNode`]) so the traversal can re-select the tile — a warm
    ///   restore from the cache, or a re-fetch if it aged out.
    /// - **Decode-failed** (still `Fetched` after `decode` drained it from [`JustFetched`]): decode is pure
    ///   over the fetched bytes, so failure is **terminal** — drop the entry, record the node in
    ///   `decode_failed` (never re-requested → no re-fetch→re-fail busy-loop), and free the slot.
    ///
    /// Each removal verifies `index[node]` still names THIS entity (the slot may have been re-tasked to the
    /// same node by a later restore within the step) — a stale pairing is left alone. Every other reclaim
    /// path (`fail_fetch`/`cancel_stale`/`preempt_inflight`/`prune_cold_grafts`) removes its entry inline.
    fn reconcile_index(&mut self) {
        if self.streaming.is_none() {
            return;
        }
        let Reclaim { evicted, decode_failed } = std::mem::take(&mut *self.world.resource_mut::<Reclaim>());
        if evicted.is_empty() && decode_failed.is_empty() {
            return; // settled step — nothing exited (O(1))
        }
        {
            let s = self.streaming.as_mut().unwrap();
            for &e in &evicted {
                let node = self.world.get::<TileNode>(e).map(|n| n.0).unwrap_or(NO_NODE);
                if node != NO_NODE && s.index.get(&node) == Some(&e) {
                    s.index.remove(&node);
                }
            }
            for &e in &decode_failed {
                let node = self.world.get::<TileNode>(e).map(|n| n.0).unwrap_or(NO_NODE);
                if node != NO_NODE {
                    if s.index.get(&node) == Some(&e) {
                        s.index.remove(&node);
                    }
                    s.failures.decode_failed.insert(node);
                }
            }
            // §3.1 follow-up trigger: terminal failures change the selection (siblings refine past the dead
            // tile) — make sure the next advance re-selects even with no other event.
            s.force_full_next = true;
        }
        // Free the decode-failed slots (evicted slots were already freed by `evict`).
        for &e in &decode_failed {
            self.free_slot(e, false); // a decode-failed slot is `Fetched`, never Resident
        }
        if !decode_failed.is_empty() {
            self.world.resource_mut::<GeoStats>().decode_failed_total += decode_failed.len() as u32;
        }
    }

    /// Un-graft **cold** external subtrees — the planet-scale retention fix. A grafted subtree whose tiles
    /// have not been rendered or queued for load for [`GRAFT_COLD_FRAMES`] advance steps is reclaimed: its
    /// placeholder tile is restored from the [`GraftRecord`] (dropping the grafted subtree's `Tile` nodes, so
    /// a return re-fetches + re-grafts it), and ALL bookkeeping under it — `grafted` / `bases` / `index` /
    /// `decode_failed` / `fetch_attempts` entries, in-flight fetches, and pool slots — is dropped/freed.
    /// Without this the tree, `grafted`, and `bases` grow without bound over a streaming session (nothing else
    /// removes a graft). Scanned only every [`GRAFT_PRUNE_INTERVAL`] steps; real work happens only when a
    /// subtree is actually cold. Runs after the schedule + `reconcile_index`, on the stream thread (off the
    /// game thread), so the O(grafts)/O(index) scans never touch the render path.
    fn prune_cold_grafts(&mut self, frame: u64) {
        if self.streaming.is_none() || frame == 0 || !frame.is_multiple_of(GRAFT_PRUNE_INTERVAL) {
            return;
        }
        // Phase 1 — cold graft ROOTS: cold grafts with no cold-grafted ancestor also being pruned. Sorting
        // shallowest-first means a cold ancestor is always seen before its cold descendants, so we mutate the
        // tree only at the shallowest cold node of each cold subtree (restoring it drops the rest wholesale).
        // Subtree membership is an O(d) ancestor walk against the cold-root set (`under_any`) — no path
        // prefix-compares (audit §3.2).
        let cold_roots: HashSet<u32> = {
            let s = self.streaming.as_ref().unwrap();
            let mut cold: Vec<u32> = s
                .grafts
                .records
                .iter()
                .filter(|(_, rec)| frame.saturating_sub(rec.last_active) > GRAFT_COLD_FRAMES)
                .map(|(&n, _)| n)
                .collect();
            if cold.is_empty() {
                return;
            }
            cold.sort_unstable_by_key(|&n| s.arena.depth(n));
            let mut roots: HashSet<u32> = HashSet::new();
            for n in cold {
                if !under_any(&s.arena, &roots, n) {
                    roots.insert(n);
                }
            }
            roots
        };
        // Phase 2 — gather each cold root's saved placeholder (immutable), then restore it into the tree
        // (mutable). Restoring `children` drops the grafted subtree's nodes, including any nested grafts'.
        let restores: Vec<GraftRestore> = {
            let s = self.streaming.as_ref().unwrap();
            cold_roots
                .iter()
                .filter_map(|&rn| {
                    s.grafts.records.get(&rn).map(|rec| {
                        (
                            s.arena.path_of(rn),
                            rec.saved_transform,
                            rec.saved_content.clone(),
                            rec.saved_contents.clone(),
                            rec.saved_children.clone(),
                            rec.saved_geometric_error,
                            rec.saved_refine,
                        )
                    })
                })
                .collect()
        };
        // Phase 3 — collect entities to free (index entries under a cold root) + stale in-flight fetches.
        // An in-flight GLB tile is in BOTH `index` and `in_flight`; a `HashSet` dedups so a slot is returned
        // to the free-list exactly once (a double-push would alias one entity to two tiles).
        // Carry `content_index` so Phase 4 drops `in_flight_nodes` only for the PRIMARY (`== 0`); a 1.1
        // additional content rides the primary's node and was never inserted there (mirrors `cancel_stale`).
        let stale_inflight: Vec<(RequestId, u32, u32)> = {
            let s = self.streaming.as_ref().unwrap();
            s.in_flight
                .iter()
                .filter(|(_, inf)| under_any(&s.arena, &cold_roots, inf.node))
                .map(|(&id, inf)| (id, inf.node, inf.content_index))
                .collect()
        };
        let mut to_free: HashSet<Entity> = {
            let s = self.streaming.as_ref().unwrap();
            s.index
                .iter()
                .filter(|(&n, _)| under_any(&s.arena, &cold_roots, n))
                .map(|(_, &e)| e)
                .collect()
        };
        {
            let s = self.streaming.as_ref().unwrap();
            for inf in s
                .in_flight
                .values()
                .filter(|inf| under_any(&s.arena, &cold_roots, inf.node))
            {
                if let Some(e) = inf.entity {
                    to_free.insert(e);
                }
            }
        }
        // Phase 4 — apply: restore placeholders, cancel stale fetches, drop all bookkeeping under a cold root.
        {
            let s = self.streaming.as_mut().unwrap();
            for (rp, transform, content, contents, children, geometric_error, refine) in restores {
                if let Some(tile) = tile_at_mut(&mut s.root, &rp) {
                    tile.transform = transform;
                    tile.content = content;
                    tile.contents = contents;
                    tile.children = children;
                    tile.geometric_error = geometric_error;
                    tile.refine = refine;
                }
            }
            for (id, node, content_index) in &stale_inflight {
                s.source.cancel(*id);
                s.in_flight.remove(id);
                if *content_index == 0 {
                    s.in_flight_nodes.remove(node); // only the primary owns the node's in-flight entry
                }
            }
            let arena = &s.arena;
            s.index.retain(|&n, _| !under_any(arena, &cold_roots, n));
            s.grafts.records.retain(|&n, _| !under_any(arena, &cold_roots, n)); // cold roots + any nested grafts
            s.grafts.bases.retain(|&n, _| !under_any(arena, &cold_roots, n));
            s.failures.decode_failed.retain(|&n| !under_any(arena, &cold_roots, n));
            s.failures
                .fetch_attempts
                .retain(|&n, _| !under_any(arena, &cold_roots, n));
            s.failures.next_retry.retain(|&n, _| !under_any(arena, &cold_roots, n));
        }
        // Phase 5 — return every freed pool slot to the free-list (reset fields, never despawn). A pruned cold
        // tile may be `Resident`, so also drop it from `ResidentSet` (Stage 1). Every `e` is a pre-spawned pool
        // entity (from `index` / `in_flight`), so `free_slot`'s component accesses always resolve.
        for e in to_free {
            self.free_slot(e, true);
        }
    }

    /// Per-step telemetry (resident count, decoded/evicted this step).
    pub fn stats(&self) -> GeoStats {
        *self.world.resource::<GeoStats>()
    }

    /// The latest published render frame — an O(1) `Arc` clone (audit 2026-06-10 §2: the old deep clone paid
    /// O(R) Arc bumps + clip-box Vec clones per call). The stream thread mirrors this into the cross-thread
    /// handoff after each full step.
    pub fn latest_frame(&self) -> Arc<GeoRenderFrame> {
        Arc::clone(&self.world.resource::<GeoMailbox>().frame)
    }

    /// Advance one **streaming** step from an Unreal-space camera (cm): convert to ECEF via the ENU frame,
    /// set the LRU-cull to the streaming `max_tile_distance`, and run the (re-selection-gated, audit §3.1)
    /// pipeline at `frame`. Returns `true` iff a FULL step ran (the caller commits its frame counter only
    /// then). This is what the stream thread feeds each iteration.
    #[allow(clippy::too_many_arguments)]
    pub fn advance_from_ue(
        &mut self,
        camera_ue_cm: [f64; 3],
        camera_dir_ue: [f64; 3],
        camera_up_ue: [f64; 3],
        aspect: f64,
        fov_y_radians: f64,
        viewport_height: f64,
        max_sse: f64,
        frame: u64,
    ) -> bool {
        let enu = self.world.resource::<GeoEnu>().0;
        let cull = self.streaming.as_ref().map(|s| s.max_tile_distance).unwrap_or(0.0);
        let camera_ecef = enu.local_to_ecef(ue_cm_to_enu(camera_ue_cm));
        // Rotate a UE-space direction into an ECEF unit vector: the origin cancels in the subtraction, leaving
        // the pure rotation; `ue_cm_to_enu` is linear so it maps directions too. ZERO ⇒ "unknown".
        let ue_dir_to_ecef = |d: [f64; 3]| (enu.local_to_ecef(ue_cm_to_enu(d)) - enu.origin_ecef).normalize_or_zero();
        let cam_dir_ecef = ue_dir_to_ecef(camera_dir_ue);
        let cam_up_ecef = ue_dir_to_ecef(camera_up_ue);
        // Rebuild the view frustum (ECEF) for this frame's frustum-aware LOD. A degenerate basis (no
        // direction) yields the accept-all frustum, so off-thread/unknown-view callers stream everything.
        let frustum = ViewFrustum::new(
            camera_ecef,
            cam_dir_ecef,
            cam_up_ecef,
            fov_y_radians,
            aspect,
            FRUSTUM_MARGIN_RAD,
        );
        if let Some(s) = self.streaming.as_mut() {
            // §3.1 gate input: `GeoView` carries no orientation, so a direction/up/aspect change (a camera
            // TURN with a fixed position) would otherwise read as "view unchanged" and skip re-selection —
            // the frustum classification just changed, so force a full step when any of them moved.
            if s.cam_dir_ecef != cam_dir_ecef || s.last_ue_orient != Some((cam_up_ecef, aspect)) {
                s.force_full_next = true;
            }
            s.last_ue_orient = Some((cam_up_ecef, aspect));
            s.cam_dir_ecef = cam_dir_ecef;
            s.frustum = frustum;
        }
        self.advance_streaming(GeoView {
            camera_ecef,
            fov_y_radians,
            viewport_height,
            max_sse,
            cull_distance_m: cull,
            frame,
        })
    }

    /// Resident decoded tiles after the last step (telemetry).
    pub fn resident_tiles(&self) -> usize {
        self.stats().resident as usize
    }

    /// Free pool slots right now — the admission headroom. `0` means every restore/fetch is being dropped
    /// (`pool_exhausted_total` climbs); with `evicted_total` flat that is the eviction-dead starvation stall.
    pub fn free_slots(&self) -> u64 {
        self.world.resource::<FreeList>().0.len() as u64
    }

    /// The resident-tile cap (`MaxResident`) — the LRU evict target the pool is sized above.
    pub fn max_resident(&self) -> u64 {
        self.world.resource::<MaxResident>().0 as u64
    }

    /// Outstanding/undrained fetches — the stream thread's "work pending" signal (telemetry).
    pub fn in_flight(&self) -> usize {
        self.streaming.as_ref().map(|s| s.source.in_flight()).unwrap_or(0)
    }

    /// `true` if any transient-failure backoff timer OR a P3DT session refresh is still pending (audit H4/M1a).
    /// The stream loop ORs this into its work gate so a 429/5xx wave — or an expired session — that drained the
    /// in-flight set to 0 over a PARKED camera keeps stepping (and re-stamping `now_instant`) until the
    /// wall-clock retry/cooldown deadline (`Instant`: `next_retry` / `last_session_refresh_at`, gated by
    /// `now_instant` after H5) elapses. Without it the loop stopped stepping (it only ticks on a view change /
    /// texture report / in-flight work), so `now_instant` was never re-stamped, the wall-clock retry deadline
    /// was never reached, and the rate-limited region stayed stuck at coarse LOD until an unrelated move.
    pub fn has_pending_retry(&self) -> bool {
        self.streaming
            .as_ref()
            .is_some_and(|s| !s.failures.next_retry.is_empty() || s.session.refresh_pending)
    }

    /// Bytes resident in the persistent disk cache behind the fetch source (0 if none). Telemetry.
    pub fn cached_bytes(&self) -> u64 {
        self.streaming.as_ref().map(|s| s.source.cached_bytes()).unwrap_or(0)
    }

    /// Number of LIVE tile-pool entities (those carrying `TileState`) — **invariant across `advance`**: the
    /// pool is pre-spawned and never spawns/despawns in the loop. Counted by component, NOT by
    /// `world.entities()`: bevy 0.19 stores resources (and other internals) as entities, so a raw entity
    /// count — `len()` (allocated slots) or even `count_spawned()` — adds ~one per resource and over-counts
    /// the pool. Filtering on `TileState` counts exactly the tiles, immune to engine-internal entities.
    pub fn entity_count(&self) -> usize {
        self.world.iter_entities().filter(|e| e.contains::<TileState>()).count()
    }

    /// Decoded-mesh LRU occupancy: (entries, bytes). The warm-re-entry buffer's current size (telemetry).
    pub fn flash_cache(&self) -> (usize, usize) {
        let c = self.world.resource::<FlashCache>();
        (c.len(), c.byte_len())
    }

    /// Persistent disk-cache hit / miss counts behind the fetch source (audit L1 telemetry — un-blinds the
    /// disk hit-rate so the disk budget can be tuned). `0` for a cacheless source.
    pub fn disk_cache_hits(&self) -> u64 {
        self.streaming.as_ref().map(|s| s.source.cache_hits()).unwrap_or(0)
    }
    pub fn disk_cache_misses(&self) -> u64 {
        self.streaming.as_ref().map(|s| s.source.cache_misses()).unwrap_or(0)
    }

    /// Persisted decoded-geometry tier resident bytes (audit M3). `0` until the tier is enabled / wired.
    pub fn geom_cache_bytes(&self) -> u64 {
        self.world
            .get_resource::<GeomCacheRes>()
            .and_then(|g| g.0.as_ref())
            .map(|c| c.bytes())
            .unwrap_or(0)
    }

    /// Interior tiles the last traversal **refined** (split into children) — the LOD/working-tree depth
    /// signal carried across frames for the hysteresis dead-band + kick (telemetry).
    pub fn refined_count(&self) -> usize {
        self.streaming
            .as_ref()
            .map(|s| s.sel_state.values().filter(|&&r| r == SelectionResult::Refined).count())
            .unwrap_or(0)
    }

    /// Currently grafted external subtrees (telemetry / tests). Bounded by [`prune_cold_grafts`](Self::prune_cold_grafts);
    /// a climbing count that never settles would mean the cold-prune is not keeping up with exploration.
    pub fn grafted_count(&self) -> usize {
        self.streaming.as_ref().map(|s| s.grafts.records.len()).unwrap_or(0)
    }

    /// Enable/replace the persisted decoded-geometry tier (audit M3). The FFI create path builds a second
    /// [`DiskCache`] under `<cache_dir>/geom` and installs it here; `None` disables the tier. Idempotent.
    pub fn set_geom_cache(&mut self, cache: Option<Arc<DiskCache>>) {
        self.world.insert_resource(GeomCacheRes(cache));
    }

    /// Override the decoded-mesh LRU byte budget (default [`DEFAULT_FLASH_CACHE_BYTES`]). Trims immediately if
    /// the new budget is below current occupancy. For tuning / tests that want to force cache eviction.
    pub fn set_flash_cache_budget(&mut self, max_bytes: usize) {
        let mut c = self.world.resource_mut::<FlashCache>();
        c.max_bytes = max_bytes;
        c.trim(0); // no live frame here ⇒ the staleness term is 1× (neutral), so a budget-cut trim ranks by
                   // distance with only the gentle size nudge (`EVICT_SIZE_WEIGHT`) on top
    }

    // ---- Test/integration surface (used by the test module; the production traversal uses
    // `request_one`/`place_decoded`) ----

    /// Acquire a `Free` pool slot and populate it as a freshly-selected tile (pure field mutation — no
    /// spawn). Returns `None` if the pool is exhausted (the caller grows the pool at a step boundary).
    pub fn acquire_tile(
        &mut self,
        id: u64,
        bounds: TileBounds,
        geometric_error: f64,
        transform: DMat4,
    ) -> Option<Entity> {
        let e = self.world.resource_mut::<FreeList>().0.pop()?;
        {
            let mut ent = self.world.entity_mut(e);
            *ent.get_mut::<TileId>().unwrap() = TileId(id);
            *ent.get_mut::<TileBounds>().unwrap() = bounds;
            *ent.get_mut::<GeometricError>().unwrap() = GeometricError(geometric_error);
            *ent.get_mut::<TileTransform>().unwrap() = TileTransform(transform);
            *ent.get_mut::<TileState>().unwrap() = TileState::Selected;
        }
        self.world.resource_mut::<LiveSet>().0.insert(e); // Free→Selected (ADR-0018 active set)
        Some(e)
    }

    /// Deliver fetched bytes to a tile (the test analog of [`drain_raw`](Self::drain_raw)'s GLB delivery): set
    /// `FetchedBytes` + flip to `Fetched`, and record the arrival in [`JustFetched`] so [`decode`] picks it up.
    pub fn deliver_bytes(&mut self, tile: Entity, bytes: Arc<[u8]>) {
        {
            let mut ent = self.world.entity_mut(tile);
            ent.get_mut::<FetchedBytes>().unwrap().0 = Some(bytes);
            *ent.get_mut::<TileState>().unwrap() = TileState::Fetched;
        }
        self.world.resource_mut::<JustFetched>().0.push(tile);
    }

    /// Read a tile's current state (integration/tests).
    pub fn tile_state(&self, tile: Entity) -> Option<TileState> {
        self.world.get::<TileState>(tile).copied()
    }

    /// Count tiles in a given state (integration/tests).
    pub fn count_state(&mut self, want: TileState) -> usize {
        self.world
            .query::<&TileState>()
            .iter(&self.world)
            .filter(|s| **s == want)
            .count()
    }
}

/// Graft a fetched external tileset's root into the placeholder tile at `path` (its content/children +
/// composed transform replace the placeholder's), so the next traversal descends into it.
fn graft_external(
    root: &mut Tile,
    grafted: &mut HashMap<u32, GraftRecord>,
    path: &[u32],
    node: u32,
    json_bytes: &[u8],
    frame: u64,
) -> Result<(), StreamError> {
    // Depth cap (C1), defense-in-depth: the traversal already refuses to descend (hence to request) past
    // `MAX_TILE_DEPTH`, so a graft this deep shouldn't be reachable — but reject it here too, before parsing,
    // so a hostile external-tileset chain can never grow the tree without bound even if a graft is triggered
    // by another path. Checked first: cheaper than the parse, and needs no live target tile.
    if path.len() >= MAX_TILE_DEPTH as usize {
        return Err(StreamError::Tileset(format!(
            "graft target depth {} exceeds cap {MAX_TILE_DEPTH}",
            path.len()
        )));
    }
    let child = parse_tileset(json_bytes).map_err(|e| StreamError::Tileset(e.to_string()))?;
    // Same fail-loud `extensionsRequired` gate as the root tileset (`from_tileset_with_decoder`): a GRAFTED
    // tileset declaring an extension this stack does not implement must refuse to graft, not silently render
    // wrong (the root-only check would otherwise be bypassed by any external-tileset hop).
    if let Some(unsupported) = child
        .extensions_required
        .iter()
        .find(|e| !SUPPORTED_TILESET_EXTENSIONS.contains(&e.as_str()))
    {
        return Err(StreamError::Tileset(format!(
            "grafted tileset requires unsupported extension '{unsupported}' (this stack supports {SUPPORTED_TILESET_EXTENSIONS:?})"
        )));
    }
    let ext = child.root;
    let tile = tile_at_mut(root, path)
        .ok_or_else(|| StreamError::Tileset(format!("graft target {path:?} no longer exists")))?;
    let composed = tile.matrix() * ext.matrix();
    // Save the placeholder's pre-graft state (the four fields we overwrite) so `prune_cold_grafts` can later
    // un-graft this subtree back to a re-fetchable placeholder. `take`/`mem::take` move the (small) originals
    // into the record instead of cloning; the fields are immediately overwritten below.
    let record = GraftRecord {
        saved_transform: tile.transform,
        saved_content: tile.content.take(),
        saved_contents: std::mem::take(&mut tile.contents),
        saved_children: std::mem::take(&mut tile.children),
        saved_geometric_error: tile.geometric_error,
        saved_refine: tile.refine,
        last_active: frame,
    };
    tile.transform = Some(composed.to_cols_array());
    tile.content = ext.content;
    tile.contents = ext.contents;
    tile.children = ext.children;
    // Adopt the external tileset root's geometricError + refine (3D Tiles 1.1): the grafted subtree must refine
    // by the external root's error and mode, not the placeholder stub's. (No-op when the stub already matches —
    // Google P3DT's stubs do; real for hand-authored "tree of trees".)
    tile.geometric_error = ext.geometric_error;
    tile.refine = ext.refine;
    grafted.insert(node, record);
    Ok(())
}

/// Is `node` inside (or equal to) any of `roots`' subtrees? An O(depth) ancestor walk of u32 membership
/// probes — the subtree-membership test `prune_cold_grafts` uses to drop everything under a cold graft root
/// (replaces the old per-entry path prefix-compares; audit §3.2).
fn under_any(arena: &PathArena, roots: &HashSet<u32>, node: u32) -> bool {
    let mut n = node;
    loop {
        if roots.contains(&n) {
            return true;
        }
        match arena.parent(n) {
            Some(p) => n = p,
            None => return false,
        }
    }
}

/// Bump `last_active = frame` for every grafted ancestor of `node` (including `node` itself), so a subtree
/// with any rendered/loading descendant this frame is kept warm. `O(depth)` u32 ancestor hops + map probes —
/// no allocation, no byte hashing (the old per-prefix path probes hashed O(d²) bytes per tile; audit §3.2).
fn bump_graft_activity(grafted: &mut HashMap<u32, GraftRecord>, arena: &PathArena, node: u32, frame: u64) {
    let mut n = node;
    loop {
        if let Some(rec) = grafted.get_mut(&n) {
            rec.last_active = frame;
        }
        match arena.parent(n) {
            Some(p) => n = p,
            None => return,
        }
    }
}

/// The base URL a tile's content URI resolves against: the NEAREST grafted-ancestor subtree URL (walking the
/// node's ancestor chain up from itself), else the root `base_url`. Propagates inherited auth (P3DT
/// `?session=…`) downward. `O(depth)` u32 map probes — no path hashing (audit §3.2; the pre-arena version
/// hashed O(d) bytes per prefix probe).
fn base_for_node<'a>(base_url: &'a str, bases: &'a HashMap<u32, String>, arena: &PathArena, node: u32) -> &'a str {
    let mut n = node;
    loop {
        if let Some(b) = bases.get(&n) {
            return b;
        }
        match arena.parent(n) {
            Some(p) => n = p,
            None => return base_url,
        }
    }
}

/// How much an external-tileset **manifest** (`.json`) outranks geometry (`.glb`) of equal screen-importance
/// in the fetch queue. A manifest is a tiny JSON fetch that **grafts the next tree level** — and a fine tile
/// can't even be discovered, let alone loaded, until the manifest that introduces it has grafted. Fetching
/// manifests first therefore lets the traversal descend toward the camera's target LOD *fast* (skip-LOD
/// intent: reach the finest level quickly) instead of crawling level-by-level behind heavy geometry. The
/// boost rides on top of `geometric_error / distance`, so it lifts manifests above content **at the same
/// distance** — near subtrees still graft before far ones.
const MANIFEST_FETCH_BOOST: f64 = 8.0;

/// Fetch-priority **score** for ordering the per-step load queue (higher = fetch first), the blend behind
/// the §9.2 step-7 look-direction priority. Three terms: **screen importance** `geometric_error / distance²`
/// — a distance-SQUARED falloff that CONCENTRATES the budget on the nearest region (a near *fine* tile can
/// out-rank a far *coarse* one) instead of equalizing error planet-wide, which spread the finite in-flight
/// budget across every visible parent and left a wide/orbital view stuck coarse. Within one region (same
/// distance) `geometric_error` still 2×-favors the parent each level, so the mandatory top-down graft order is
/// intact — only the across-region competition changes. Times a GENTLE **view-direction lobe**
/// `0.55 + 0.45·((align+1)/2)` (linear) on `align = dot(dir_to_tile, cam_dir) ∈ [-1, 1]`: ahead ≈ 1.0×, screen
/// edge ≈ 0.8×, behind ≈ 0.55× — a mild tie-breaker, NOT a wedge. (An earlier SQUARED lobe — ahead ~3× the edge
/// — carved a hard central detail wedge whose coarse-vs-fine LOD cliff cracked into texture seams; concentration
/// is now distance-driven, which discriminates by range at every altitude.) `cam_dir == ZERO ⇒ align 0 ⇒
/// uniform 0.775×`, so importance still totally-orders when the direction is unknown. Times a **grafting boost**
/// so a tree-descending manifest leads same-distance content ([`MANIFEST_FETCH_BOOST`]).
fn fetch_priority(t: &SelectedTile, cam_pos: DVec3, cam_dir: DVec3) -> f64 {
    // Distance-SQUARED falloff (concentrate, don't equalize): the nearest region's fine tiles out-rank a far
    // region's coarse tiles, so the budget descends ONE region to full detail rather than lifting the whole
    // view one level. Same-distance tiles within a region keep the gE-driven top-down order (parent first).
    let d = t.distance.max(1.0);
    let importance = t.geometric_error / (d * d);
    let align = (t.bounds_center - cam_pos).normalize_or_zero().dot(cam_dir);
    let graft_boost = if t.content_uris.first().is_some_and(|u| is_tileset_uri(u)) {
        MANIFEST_FETCH_BOOST
    } else {
        1.0
    };
    // View-direction lobe — a GENTLE tie-breaker on top of the distance² term, NOT a hard foveation wedge.
    // `align ∈ [-1, 1]` (dir-to-tile · look-dir) maps linearly to ahead 1.0, the screen edge ~0.8, behind 0.55
    // (behind tiles are hard-culled upstream anyway). The earlier SQUARED lobe (`0.1 + 0.9·((align+1)/2)²`, ahead
    // ~3× the edge) carved the budget into a sharp central WEDGE: the focal wedge descended to full depth while
    // the sides starved at coarse, leaving a hard LOD cliff whose mismatched-resolution tile edges read as
    // texture seams (the screenshot's sharp-wedge-amid-blur). The user's target is DISTANCE-based — optimal-range
    // tiles sharp, farther gracefully coarser — so let `1/distance²` do the concentrating (it discriminates by
    // range at every altitude, including orbit, where the sub-camera point is nearer than the limb) and keep
    // direction a mild nudge. Monotonic in `align`; `cam_dir == ZERO ⇒ align 0 ⇒ uniform 0.775×` (order falls
    // back to pure importance).
    // View-direction lobe coefficients (named, not magic numbers): a tile directly behind the camera gets
    // `FETCH_LOBE_MIN`×, the screen edge ~0.8×, dead-ahead `FETCH_LOBE_MIN + FETCH_LOBE_SPAN` = 1.0×.
    const FETCH_LOBE_MIN: f64 = 0.55;
    const FETCH_LOBE_SPAN: f64 = 0.45;
    let lobe = FETCH_LOBE_MIN + FETCH_LOBE_SPAN * ((align + 1.0) * 0.5);
    importance * lobe * graft_boost
}

// The canonical per-tile key (FNV-1a over the path's LE bytes) is single-homed in
// `logi_3dtiles_select::path_key`, cached per node by the `PathArena` and carried on every `SelectedTile` as
// `key` — this crate no longer recomputes it. COLLISION BUDGET (audit G6 — accepted): it is a 64-bit hash, so
// two distinct tile paths *could* collide and alias each other's `FlashCache` / C++ section entry; the
// birthday bound over the few thousand simultaneously-live tiles is ~1e-13, so it is accepted rather than paid
// for. (There is NO `ffi::hash_path` — the cross-boundary contract is simply "the raw FNV-1a `u64`".)

/// Whether a fetch was actually ISSUED by [`GeoWorld::request_one`] — `PoolExhausted` means no slot was free
/// and NO request went out, so the caller must not count it against the per-step new-fetch budget (audit
/// 2026-06-10 §4.1) and should stop issuing this step (no slot will appear mid-step).
enum Requested {
    Issued(Option<Entity>),
    PoolExhausted,
}

/// Distinct, stable render key for content `content_index` of a (multiple-contents) tile, derived from the
/// tile's primary `key`. Content `0` returns the primary key UNCHANGED — so a single-content tile (every P3DT
/// tile) keeps the exact same key and `FlashCache` entry as before — while each additional content extends the
/// same FNV-1a hash with its index, giving the C++ section cache a globally-unique key per rendered sub-mesh.
fn content_key(primary_key: u64, content_index: u32) -> u64 {
    if content_index == 0 {
        return primary_key;
    }
    let mut h = primary_key;
    for b in content_index.to_le_bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01B3);
    }
    h
}

/// The ENU rebase frame, as a resource the schedule reads (e.g. [`publish`] for the frame's ECEF origin).
#[derive(Resource, Clone, Copy)]
struct GeoEnu(EnuFrame);

#[cfg(test)]
mod tests {
    use super::*;
    use logi_geospatial::{Cartographic, Ellipsoid};

    /// An axis-aligned [`RenderClipBox`] helper for the union tests: ground axes X/Y, up = Z (the same
    /// shape `oriented_clip_box` emits near the ENU origin).
    fn clip_box(cx: f64, cy: f64, hx: f64, hy: f64) -> RenderClipBox {
        RenderClipBox {
            center: [cx as f32, cy as f32, 0.0],
            half_axes: [[hx as f32, 0.0, 0.0], [0.0, hy as f32, 0.0], [0.0, 0.0, 5.0]],
        }
    }

    #[test]
    fn union_clip_boxes_merges_a_dense_group_and_refuses_a_sparse_one() {
        // DENSE: two adjacent equal quads tiling a 2×1 strip — union covers both exactly (density 1.0).
        let dense = [clip_box(-0.5, 0.0, 0.5, 0.5), clip_box(0.5, 0.0, 0.5, 0.5)];
        let u = union_clip_boxes(&dense).expect("adjacent siblings union");
        assert!((u.center[0]).abs() < 1e-4 && (u.center[1]).abs() < 1e-4);
        let hx = glam::Vec3::from(u.half_axes[0]).length();
        let hy = glam::Vec3::from(u.half_axes[1]).length();
        assert!((hx - 1.0).abs() < 1e-4, "union spans both quads along x (got {hx})");
        assert!((hy - 0.5).abs() < 1e-4, "union keeps the strip height (got {hy})");
        // SPARSE: two small quads at opposite corners of a large span — unioning would clip the empty
        // middle (holes over ground nothing redraws), so the union is REFUSED.
        let sparse = [clip_box(-10.0, -10.0, 0.5, 0.5), clip_box(10.0, 10.0, 0.5, 0.5)];
        assert!(
            union_clip_boxes(&sparse).is_none(),
            "a sparse group must fall back to individual boxes"
        );
        // INTERIOR GAP (the 2026-06-11 black-wedge holes): 3 of a 2×2 quad's children present, one missing —
        // the summed-area heuristic this test replaced ADMITTED this (75% density > the old 0.65 bar) and the
        // union clipped the missing child's footprint out of the stand-in = a see-through hole. The EXACT
        // coverage test must refuse it.
        let l_shape = [
            clip_box(-0.5, -0.5, 0.5, 0.5),
            clip_box(0.5, -0.5, 0.5, 0.5),
            clip_box(-0.5, 0.5, 0.5, 0.5),
            // (0.5, 0.5) quadrant missing
        ];
        assert!(
            union_clip_boxes(&l_shape).is_none(),
            "an interior gap must refuse the union — holes beat nothing"
        );
        // FOUR-QUAD full tiling (with the slight overlap real AABBs have): unions fine.
        let full = [
            clip_box(-0.5, -0.5, 0.52, 0.52),
            clip_box(0.5, -0.5, 0.52, 0.52),
            clip_box(-0.5, 0.5, 0.52, 0.52),
            clip_box(0.5, 0.5, 0.52, 0.52),
        ];
        assert!(
            union_clip_boxes(&full).is_some(),
            "a fully-tiled (overlapping-slack) quad unions"
        );
    }

    /// The stub decoders' vertex buffer: a tiny triangle — 3 × 32-byte vertices at (0,0,0), (1,0,0), (0,1,0),
    /// non-position attributes zero. A REAL mesh shape (the indices `[0,1,2]` are in bounds; two positive
    /// extents, so `publish`'s footprint guard admits its clip box — the flat-terrain case it exists to allow),
    /// unlike the old single zeroed vertex whose degenerate point AABB the guard rightly rejected.
    fn tri_vertices() -> Vec<u8> {
        let mut v = vec![0u8; 96];
        v[32..36].copy_from_slice(&1.0f32.to_le_bytes()); // vertex 1: x = 1
        v[68..72].copy_from_slice(&1.0f32.to_le_bytes()); // vertex 2: y = 1
        v
    }

    fn test_world(pool: usize, max_resident: usize) -> GeoWorld {
        let e = Ellipsoid::wgs84();
        let enu = EnuFrame::from_cartographic(&e, Cartographic::from_degrees(-73.9857, 40.7484, 0.0));
        // Stub decoder: turns N bytes into a tiny-triangle geometry ([`tri_vertices`]) whose radius encodes
        // the byte count, so a test can prove decode transitioned state — without crafting a real GLB.
        let decoder = TileDecoder(Arc::new(|bytes: &[u8], _xf: DMat4| {
            Some(TileGeometry {
                vertices: tri_vertices(),
                indices: vec![0, 1, 2],
                texture: Vec::new(),
                bounds_center: [0.0; 3],
                bounds_radius: bytes.len() as f32,
                unlit: false,
            })
        }));
        GeoWorld::new(pool, enu, decoder, max_resident)
    }

    #[test]
    fn pool_is_prespawned_one_archetype_and_never_churns() {
        let mut w = test_world(64, 4096);
        let before = w.entity_count();
        assert_eq!(before, 64);
        assert_eq!(w.count_state(TileState::Free), 64);

        // Acquire some tiles, advance, evict — the entity count must NEVER change (no spawn/despawn).
        for i in 0..10u64 {
            let b = TileBounds {
                center: DVec3::new(6_378_137.0, 0.0, 0.0),
                radius: 1.0,
            };
            w.acquire_tile(i, b, 100.0, DMat4::IDENTITY).expect("pool slot");
        }
        w.advance(GeoView {
            camera_ecef: DVec3::new(6_378_937.0, 0.0, 0.0),
            fov_y_radians: 1.0,
            viewport_height: 1080.0,
            max_sse: 16.0,
            cull_distance_m: 0.0,
            frame: 1,
        });
        assert_eq!(w.entity_count(), before, "no structural change across advance");
    }

    #[test]
    fn sse_cull_lru_computes_for_live_tiles_only() {
        let mut w = test_world(8, 4096);
        let b = TileBounds {
            center: DVec3::new(6_378_137.0, 0.0, 0.0),
            radius: 1.0,
        };
        let t = w.acquire_tile(1, b, 1000.0, DMat4::IDENTITY).unwrap();
        w.advance(GeoView {
            camera_ecef: DVec3::new(6_380_137.0, 0.0, 0.0),
            fov_y_radians: 1.0,
            viewport_height: 1080.0,
            max_sse: 16.0,
            cull_distance_m: 0.0,
            frame: 7,
        });
        // The live tile was LRU-stamped this frame (the pass's one load-bearing output); a Free slot keeps 0.
        assert_eq!(w.world.get::<LastSeen>(t).unwrap().0, 7);
    }

    #[test]
    fn decode_transitions_fetched_tiles_to_resident() {
        let mut w = test_world(256, 4096);
        let b = TileBounds {
            center: DVec3::new(6_378_137.0, 0.0, 0.0),
            radius: 1.0,
        };
        let mut tiles = Vec::new();
        for i in 0..200u64 {
            let t = w.acquire_tile(i, b, 100.0, DMat4::IDENTITY).unwrap();
            w.deliver_bytes(t, Arc::from(vec![1u8, 2, 3].into_boxed_slice()));
            tiles.push(t);
        }
        assert_eq!(w.count_state(TileState::Fetched), 200);
        w.advance(GeoView {
            camera_ecef: DVec3::new(6_378_937.0, 0.0, 0.0),
            fov_y_radians: 1.0,
            viewport_height: 1080.0,
            max_sse: 16.0,
            cull_distance_m: 0.0,
            frame: 1,
        });
        // Every fetched tile decoded (sequential drain of `JustFetched`) and is now resident with a mesh handle.
        assert_eq!(w.count_state(TileState::Resident), 200);
        assert!(w.world.get::<ResidentMesh>(tiles[0]).unwrap().0.is_some());
    }

    #[test]
    fn evict_drops_offscreen_tiles_to_free_without_despawn() {
        let mut w = test_world(64, 3); // cap = 3 resident
        let cam = DVec3::new(6_378_937.0, 0.0, 0.0); // ~800 m above the equator surface point
        let near = TileBounds {
            center: DVec3::new(6_378_137.0, 0.0, 0.0),
            radius: 1.0,
        }; // ~800 m from the camera → in view
        let far = TileBounds {
            center: DVec3::new(6_378_137.0, 2.0e6, 0.0),
            radius: 1.0,
        }; // ~2000 km away → out of the working set
           // 2 in-view (protected) + 8 off-screen (evictable); deliver all, then ONE advance.
        for i in 0..2u64 {
            let t = w.acquire_tile(i, near, 100.0, DMat4::IDENTITY).unwrap();
            w.deliver_bytes(t, Arc::from(vec![9u8].into_boxed_slice()));
        }
        for i in 2..10u64 {
            let t = w.acquire_tile(i, far, 100.0, DMat4::IDENTITY).unwrap();
            w.deliver_bytes(t, Arc::from(vec![9u8].into_boxed_slice()));
        }
        let before = w.entity_count();
        // cull = 5 km: the 2 near tiles get LastSeen stamped (protected); the 8 far ones don't (stale).
        w.advance(GeoView {
            camera_ecef: cam,
            fov_y_radians: 1.0,
            viewport_height: 1080.0,
            max_sse: 16.0,
            cull_distance_m: 5_000.0,
            frame: 1,
        });
        // All 10 decoded → resident; evict then drops the stale (off-screen) overflow until resident ≤ cap.
        assert!(
            w.stats().resident <= 3,
            "resident {} should be ≤ cap 3 after evicting off-screen tiles",
            w.stats().resident
        );
        assert!(
            w.stats().evicted_this_step >= 7,
            "should have evicted the off-screen overflow, evicted {}",
            w.stats().evicted_this_step
        );
        assert_eq!(w.entity_count(), before, "eviction resets fields, never despawns");
        assert!(
            w.count_state(TileState::Free) >= 64 - 3,
            "evicted slots returned to Free"
        );
    }

    #[test]
    fn production_decoder_rejects_garbage_without_panicking() {
        use logi_geospatial::{Cartographic, Ellipsoid};
        let e = Ellipsoid::wgs84();
        let enu = EnuFrame::from_cartographic(&e, Cartographic::from_degrees(0.0, 0.0, 0.0));
        let dec = production_decoder(enu);
        // Non-GLB / empty bytes → None (decode_glb errors), no panic across the decode boundary.
        assert!((dec.0)(b"not a glb", DMat4::IDENTITY).is_none());
        assert!((dec.0)(&[], DMat4::IDENTITY).is_none());
    }

    #[test]
    fn streaming_pipeline_drains_then_terminally_reclaims_undecodable() {
        use logi_3dtiles_io::SyntheticSource;
        const C: f64 = 6_378_137.0;
        // A 2-level tileset: a refined root + two `.glb` leaves on the equator.
        let root = format!(
            r#"{{"asset":{{"version":"1.1"}},"geometricError":2000,
              "root":{{"boundingVolume":{{"sphere":[{C},0,0,1]}},"geometricError":1000,"refine":"REPLACE","content":{{"uri":"r.glb"}},
                "children":[
                  {{"boundingVolume":{{"sphere":[{C},0,0,1]}},"geometricError":10,"content":{{"uri":"0.glb"}}}},
                  {{"boundingVolume":{{"sphere":[{C},0,0,1]}},"geometricError":10,"content":{{"uri":"1.glb"}}}}]}}}}"#
        );
        let mut w = GeoWorld::from_tileset(
            "https://example.invalid/root.json".to_string(),
            root.as_bytes(),
            Cartographic::from_degrees(0.0, 0.0, 0.0),
            Box::new(SyntheticSource::new()),
            1.0e9, // no distance cull
            256,
            4096,
        )
        .expect("tileset parses");
        // Near camera → the root refines → the leaves are selected + requested; the deterministic
        // SyntheticSource completes each within ≤10 ticks. Run well past that so any re-request loop would
        // show: each drained tile's synthetic (non-glTF) payload fails `decode`, so `reconcile_index` frees
        // the slot and marks the path terminally bad.
        for frame in 1..=40u64 {
            w.advance_streaming(GeoView {
                camera_ecef: DVec3::new(C + 2000.0, 0.0, 0.0),
                fov_y_radians: 1.0,
                viewport_height: 1080.0,
                max_sse: 16.0,
                cull_distance_m: 0.0,
                frame,
            });
        }
        // The selected tiles went select → request → drain → `FetchedBytes` → decode(fail) → reclaim. The
        // cumulative counter is non-zero (the whole input pipeline ran) AND small — a tile set has only a
        // handful of distinct `.glb` paths, so a count that stayed near that handful over 40 frames proves
        // each failure is TERMINAL: `select_request` never re-requested a known-bad path (no busy-loop).
        let failed = w.stats().decode_failed_total;
        assert!(
            (1..=4).contains(&failed),
            "expected a small terminal-failure count, got {failed}"
        );
        assert_eq!(w.count_state(TileState::Fetched), 0, "no tile left stuck in Fetched");
        assert_eq!(
            w.count_state(TileState::Free),
            256,
            "every reclaimed slot returned to the free-list — none leaked"
        );
        assert_eq!(w.entity_count(), 256, "no structural churn — the pool is pre-spawned");
    }

    /// A deterministic source whose every fetch FAILS (empty bytes on the next poll) — the network-error
    /// path `SyntheticSource` can't exercise. Honours cancellation so it doubles for the cancel tests.
    #[derive(Default)]
    struct FailingSource {
        pending: Vec<RequestId>,
        cancelled: HashSet<RequestId>,
    }
    impl TileContentSource for FailingSource {
        fn request(&mut self, id: RequestId, _url: &str, _priority: f64, _now: u64) {
            self.pending.push(id);
        }
        fn poll_completed(&mut self, _now: u64, out: &mut Vec<logi_3dtiles_io::Loaded>) {
            for id in self.pending.drain(..) {
                if !self.cancelled.remove(&id) {
                    // A transient transport error → the owner retries with backoff, then gives up at the cap.
                    out.push(logi_3dtiles_io::Loaded {
                        id,
                        result: Err(logi_3dtiles_io::FetchError::Transport("dead".into())),
                    });
                }
            }
        }
        fn cancel(&mut self, id: RequestId) {
            self.cancelled.insert(id);
        }
        fn in_flight(&self) -> usize {
            self.pending.len()
        }
    }

    /// A source that returns one fixed HTTP error status for every fetch — a Google P3DT **expired
    /// session** (401/403, or the 400/404 Google actually answers a stale `?session=` with).
    struct SessionExpiredSource {
        pending: Vec<RequestId>,
        status: u16,
    }
    impl SessionExpiredSource {
        fn with_status(status: u16) -> Self {
            Self {
                pending: Vec::new(),
                status,
            }
        }
    }
    impl TileContentSource for SessionExpiredSource {
        fn request(&mut self, id: RequestId, _url: &str, _priority: f64, _now: u64) {
            self.pending.push(id);
        }
        fn poll_completed(&mut self, _now: u64, out: &mut Vec<logi_3dtiles_io::Loaded>) {
            for id in self.pending.drain(..) {
                out.push(logi_3dtiles_io::Loaded {
                    id,
                    result: Err(logi_3dtiles_io::FetchError::Http {
                        status: self.status,
                        retry_after: None,
                    }),
                });
            }
        }
        fn cancel(&mut self, _id: RequestId) {}
        fn in_flight(&self) -> usize {
            self.pending.len()
        }
    }

    #[test]
    fn an_expired_session_is_recoverable_not_a_permanent_give_up() {
        const C: f64 = 6_378_137.0;
        let root = format!(
            r#"{{"asset":{{"version":"1.1"}},"geometricError":2000,
              "root":{{"boundingVolume":{{"sphere":[{C},0,0,1]}},"geometricError":10,"content":{{"uri":"r.glb"}}}}}}"#
        );
        // M1b: the session refresh now ENQUEUES the root re-fetch on the source (no blocking `http_get`), so the
        // mocked `SessionExpiredSource` 401s it too — the test is fully hermetic (this base is never dialed).
        let mut w = GeoWorld::from_tileset(
            "http://127.0.0.1:1/root.json".to_string(),
            root.as_bytes(),
            Cartographic::from_degrees(0.0, 0.0, 0.0),
            Box::new(SessionExpiredSource::with_status(401)),
            1.0e9,
            256,
            4096,
        )
        .expect("tileset parses");
        let view = |frame: u64| GeoView {
            camera_ecef: DVec3::new(C + 2000.0, 0.0, 0.0),
            fov_y_radians: 1.0,
            viewport_height: 1080.0,
            max_sse: 16.0,
            cull_distance_m: 0.0,
            frame,
        };
        // Every fetch 401s (session expired). The tile must NEVER be given up: a 401/403 is a recoverable
        // session expiry, not a dead URL. Before this fix, a 401 was terminal → `fetch_failed_total` would climb
        // and the tile died permanently (the planet freezes at the coarse LOD it reached while the session was
        // valid — the user-observed "tiles stuck / failing to load"). A refresh is flagged each round.
        // H5/M1b: drive a controlled wall-clock past the refresh cooldown each round, so a refresh is enqueued,
        // 401s via the source, clears, and re-arms — exercising the async refresh path. Still never a give-up.
        let base = Instant::now();
        for f in 1..=100u64 {
            w.set_now(base + Duration::from_secs(3 * f));
            w.advance_streaming(view(f));
        }
        assert_eq!(
            w.stats().fetch_failed_total,
            0,
            "a 401/403 must NOT permanently give up the tile — it is a recoverable session expiry"
        );
    }

    #[test]
    fn a_404_on_a_session_url_is_recoverable_not_terminal() {
        // 2026-06-11 live incident: Google answers a STALE `?session=` with 400/404, not 401/403 — the
        // terminal-4xx lane then killed every path on first touch (1,590 give-ups in 28 s, 257 permanently
        // dead branches). Any 4xx (≠429) on a session-carrying URL must take the recoverable refresh lane.
        const C: f64 = 6_378_137.0;
        let root = format!(
            r#"{{"asset":{{"version":"1.1"}},"geometricError":2000,
              "root":{{"boundingVolume":{{"sphere":[{C},0,0,1]}},"geometricError":10,
              "content":{{"uri":"r.glb?session=stale-token"}}}}}}"#
        );
        let mut w = GeoWorld::from_tileset(
            "http://127.0.0.1:1/root.json".to_string(),
            root.as_bytes(),
            Cartographic::from_degrees(0.0, 0.0, 0.0),
            Box::new(SessionExpiredSource::with_status(404)),
            1.0e9,
            256,
            4096,
        )
        .expect("tileset parses");
        let view = |frame: u64| GeoView {
            camera_ecef: DVec3::new(C + 2000.0, 0.0, 0.0),
            fov_y_radians: 1.0,
            viewport_height: 1080.0,
            max_sse: 16.0,
            cull_distance_m: 0.0,
            frame,
        };
        let base = Instant::now();
        for f in 1..=100u64 {
            w.set_now(base + Duration::from_secs(3 * f));
            w.advance_streaming(view(f));
        }
        assert_eq!(
            w.stats().fetch_failed_total,
            0,
            "a 4xx on a session-carrying URL is presumed session expiry — never a permanent give-up"
        );
        // (No refresh-APPLIED assertion: the hermetic mock 404s the root re-fetch too, exactly like the
        // 401 test above — the refresh ATTEMPT is implied by the absence of give-ups across 100 rounds.)
    }

    /// Root re-fetches succeed (fresh root bytes), tile fetches 404 forever — the genuinely-dead-content
    /// case that must NOT loop refreshes (the rio wedge).
    struct DeadTileLiveRootSource {
        pending: Vec<(RequestId, String)>,
        root: Vec<u8>,
    }
    impl TileContentSource for DeadTileLiveRootSource {
        fn request(&mut self, id: RequestId, url: &str, _priority: f64, _now: u64) {
            self.pending.push((id, url.to_string()));
        }
        fn poll_completed(&mut self, _now: u64, out: &mut Vec<logi_3dtiles_io::Loaded>) {
            for (id, url) in self.pending.drain(..) {
                let result = if url.contains("root.json") {
                    Ok(self.root.clone().into())
                } else {
                    Err(logi_3dtiles_io::FetchError::Http {
                        status: 404,
                        retry_after: None,
                    })
                };
                out.push(logi_3dtiles_io::Loaded { id, result });
            }
        }
        fn cancel(&mut self, _id: RequestId) {}
        fn in_flight(&self) -> usize {
            self.pending.len()
        }
    }

    #[test]
    fn a_404_that_survives_a_session_refresh_goes_terminal_not_refresh_loop() {
        // The rio wedge (2026-06-11): one genuinely-404ing session-URL path re-flagged a session refresh
        // every cooldown; each applied refresh cleared grafts/LastSeen wholesale → the region perpetually
        // re-fetched + re-evicted, in_flight pinned at the cap, the station never settled. The generation
        // check gives the path exactly ONE refreshed retry, then the terminal lane.
        const C: f64 = 6_378_137.0;
        let root = format!(
            r#"{{"asset":{{"version":"1.1"}},"geometricError":2000,
              "root":{{"boundingVolume":{{"sphere":[{C},0,0,1]}},"geometricError":10,
              "content":{{"uri":"r.glb?session=fresh-token"}}}}}}"#
        );
        let mut w = GeoWorld::from_tileset(
            "http://127.0.0.1:1/root.json".to_string(),
            root.as_bytes(),
            Cartographic::from_degrees(0.0, 0.0, 0.0),
            Box::new(DeadTileLiveRootSource {
                pending: Vec::new(),
                root: root.clone().into_bytes(),
            }),
            1.0e9,
            256,
            4096,
        )
        .expect("tileset parses");
        let view = |frame: u64| GeoView {
            camera_ecef: DVec3::new(C + 2000.0, 0.0, 0.0),
            fov_y_radians: 1.0,
            viewport_height: 1080.0,
            max_sse: 16.0,
            cull_distance_m: 0.0,
            frame,
        };
        let base = Instant::now();
        for f in 1..=200u64 {
            // Outpace both the refresh cooldown and every backoff, so the whole lifecycle plays out.
            w.set_now(base + Duration::from_secs(30 * f));
            w.advance_streaming(view(f));
        }
        assert_eq!(
            w.stats().fetch_failed_total,
            1,
            "a 404 that persists across a refreshed session is genuinely dead — given up exactly once"
        );
        let refreshes = w.stats().session_refreshes_total;
        assert!(
            (1..=2).contains(&refreshes),
            "the dead path buys at most one refresh round (got {refreshes}) — never a refresh loop"
        );
    }

    #[test]
    fn failed_fetch_reclaims_slot_retries_then_gives_up() {
        const C: f64 = 6_378_137.0;
        // One leaf whose content fetch always fails (a dead URL / persistent network error).
        let root = format!(
            r#"{{"asset":{{"version":"1.1"}},"geometricError":2000,
              "root":{{"boundingVolume":{{"sphere":[{C},0,0,1]}},"geometricError":10,"content":{{"uri":"r.glb"}}}}}}"#
        );
        let mut w = GeoWorld::from_tileset(
            "https://example.invalid/root.json".to_string(),
            root.as_bytes(),
            Cartographic::from_degrees(0.0, 0.0, 0.0),
            Box::new(FailingSource::default()),
            1.0e9, // no distance cull
            256,
            4096,
        )
        .expect("tileset parses");
        let view = |frame: u64| GeoView {
            camera_ecef: DVec3::new(C + 2000.0, 0.0, 0.0),
            fov_y_radians: 1.0,
            viewport_height: 1080.0,
            max_sse: 16.0,
            cull_distance_m: 0.0,
            frame,
        };

        // Frame 1 requests r.glb; frame 2 drains the failure → reclaim + RE-REQUEST. After a single
        // round-trip the path must NOT be given up yet (it's being retried, not stranded as the bug did).
        // H5: the backoff is wall-clock now, so drive a CONTROLLED clock (no real sleeps) — jump it well past
        // the largest capped backoff each step so every eligible retry fires deterministically.
        let base = Instant::now();
        let step = Duration::from_secs(4); // > the max capped backoff (~1.5 s + jitter)
        w.set_now(base);
        w.advance_streaming(view(1));
        w.set_now(base + step);
        w.advance_streaming(view(2));
        assert_eq!(
            w.stats().fetch_failed_total,
            0,
            "a single failure retries, it does not give up"
        );

        // Run until the dead path is abandoned. With the A1 backoff a TRANSIENT failure is retried on a capped
        // exponential delay (not every step), so reaching MAX_FETCH_ATTEMPTS takes more steps than the attempt
        // count — advance until give-up (bounded), then confirm it's counted EXACTLY once and stays there, and
        // that NO pool slot leaked (the old bug stranded the entity as `Requested` forever).
        let mut f = 3u64;
        while w.stats().fetch_failed_total == 0 && f < 100_000 {
            w.set_now(base + step * f as u32);
            w.advance_streaming(view(f));
            f += 1;
        }
        assert_eq!(
            w.stats().fetch_failed_total,
            1,
            "the permanently-dead path is given up exactly once"
        );
        // A few more steps must not re-count it (it's terminal now — `select_request` skips it).
        for g in f..f + 50 {
            w.set_now(base + step * g as u32);
            w.advance_streaming(view(g));
        }
        assert_eq!(
            w.stats().fetch_failed_total,
            1,
            "give-up is counted once, not repeatedly"
        );
        assert_eq!(
            w.count_state(TileState::Free),
            256,
            "every failed fetch reclaimed its slot — none leaked (the strand bug)"
        );
        assert_eq!(w.entity_count(), 256, "no structural churn");
    }

    #[test]
    fn publish_collects_visible_resident_tiles() {
        let mut w = test_world(64, 4096);
        let near = TileBounds {
            center: DVec3::new(6_378_137.0, 0.0, 0.0),
            radius: 1.0,
        };
        let t = w.acquire_tile(7, near, 100.0, DMat4::IDENTITY).unwrap();
        w.deliver_bytes(t, Arc::from(vec![1u8, 2, 3].into_boxed_slice()));
        // In view (cull 5 km, ~800 m away) → stamped + decoded → resident; publish snapshots it.
        w.advance(GeoView {
            camera_ecef: DVec3::new(6_378_937.0, 0.0, 0.0),
            fov_y_radians: 1.0,
            viewport_height: 1080.0,
            max_sse: 16.0,
            cull_distance_m: 5_000.0,
            frame: 1,
        });
        let f = w.latest_frame();
        assert_eq!(f.frame, 1);
        assert_eq!(f.tiles.len(), 1, "the one in-view resident tile is published");
        assert_eq!(f.tiles[0].tile_key, 7);
        assert!(!f.tiles[0].geom.vertices.is_empty());
        assert!(
            f.tiles[0].clip_boxes.is_empty(),
            "a lone leaf has no coarse stand-in over it ⇒ no clip"
        );
    }

    /// Pack `n` positions into 32-byte-interleaved vertex bytes (non-position attributes zero).
    fn pack_vertices(points: &[[f32; 3]]) -> Vec<u8> {
        let mut v = vec![0u8; points.len() * 32];
        for (i, p) in points.iter().enumerate() {
            for k in 0..3 {
                v[i * 32 + k * 4..i * 32 + k * 4 + 4].copy_from_slice(&p[k].to_le_bytes());
            }
        }
        v
    }

    #[test]
    fn oriented_clip_box_hugs_a_tilted_tile() {
        // A planar quad TILTED 45° about X — the far-from-origin tile shape whose render-space AABB over-cut
        // the stand-in (the PIE jagged-reveal artifact). Place the "earth center" so the geocentric `up` at
        // the centroid IS the quad's normal: the oriented box must then be wafer-thin along `up` (the quad is
        // planar in its own ground frame), where the old axis-aligned box was 50 units fat in Z.
        let quad = pack_vertices(&[
            [0.0, 0.0, 0.0],
            [100.0, 0.0, 0.0],
            [0.0, 100.0, 100.0],
            [100.0, 100.0, 100.0],
        ]);
        let n = glam::DVec3::new(0.0, -1.0, 1.0).normalize(); // the quad's unit normal
        let centroid = glam::DVec3::new(50.0, 50.0, 50.0);
        let earth_center = centroid - n * 6.4e8; // far below along the normal ⇒ up ≈ n
        let b = oriented_clip_box(&quad, earth_center).expect("a real quad yields a box");
        let len = |a: [f32; 3]| (a[0] * a[0] + a[1] * a[1] + a[2] * a[2]).sqrt();
        let mut half_lens = [len(b.half_axes[0]), len(b.half_axes[1]), len(b.half_axes[2])];
        half_lens.sort_by(f32::total_cmp);
        assert!(
            half_lens[0] < 1e-2,
            "planar quad ⇒ wafer-thin along its own up, got {half_lens:?}"
        );
        // Ground extents: 100 across X and 100√2 across the tilted edge — half-extents 50 and ~70.7.
        assert!(
            (half_lens[1] - 50.0).abs() < 0.5,
            "ground half-extent ~50, got {half_lens:?}"
        );
        assert!(
            (half_lens[2] - 70.7).abs() < 0.5,
            "tilted ground half-extent ~70.7, got {half_lens:?}"
        );
        let c = b.center;
        assert!(
            (c[0] - 50.0).abs() < 0.5 && (c[1] - 50.0).abs() < 0.5 && (c[2] - 50.0).abs() < 0.5,
            "box centered on the quad, got {c:?}"
        );
    }

    #[test]
    fn oriented_clip_box_refuses_content_without_ground_area() {
        let earth = glam::DVec3::new(0.0, 0.0, -6.4e8);
        // A single point and a vertical line have no plan-view footprint — clipping by them would discard a
        // stripe of the stand-in over ground they don't cover.
        assert!(oriented_clip_box(&pack_vertices(&[[1.0, 2.0, 3.0]]), earth).is_none());
        assert!(
            oriented_clip_box(&pack_vertices(&[[0.0, 0.0, 0.0], [0.0, 0.0, 100.0]]), earth).is_none(),
            "a vertical line has no ground area"
        );
        assert!(oriented_clip_box(&[], earth).is_none(), "empty buffer ⇒ no box");
    }

    #[test]
    fn publish_materializes_clip_boxes_from_the_clip_childs_mesh_footprint() {
        let mut w = test_world(64, 4096);
        let near = TileBounds {
            center: DVec3::new(6_378_137.0, 0.0, 0.0),
            radius: 1.0,
        };
        let parent = w.acquire_tile(7, near, 100.0, DMat4::IDENTITY).unwrap();
        let child = w.acquire_tile(9, near, 50.0, DMat4::IDENTITY).unwrap();
        w.deliver_bytes(parent, Arc::from(vec![1u8, 2, 3].into_boxed_slice()));
        w.deliver_bytes(child, Arc::from(vec![4u8, 5, 6].into_boxed_slice()));
        // The selection said: tile 7 is a coarse stand-in with the finer tile 9 drawn over it.
        w.world.resource_mut::<ClipChildren>().0.insert(7, vec![(0u8, 9u64)]);

        // Pass 1 — the child is resident but UNTEXTURED (published hidden): it must contribute NO clip box,
        // or the stand-in would open a hole over ground nothing else is drawing yet.
        w.advance(GeoView {
            camera_ecef: DVec3::new(6_378_937.0, 0.0, 0.0),
            fov_y_radians: 1.0,
            viewport_height: 1080.0,
            max_sse: 16.0,
            cull_distance_m: 5_000.0,
            frame: 1,
        });
        let f = w.latest_frame();
        let by = |f: &GeoRenderFrame, k: u64| f.tiles.iter().find(|t| t.tile_key == k).cloned().unwrap();
        assert!(
            by(&f, 7).clip_boxes.is_empty(),
            "a hidden (untextured) clip child contributes no clip box"
        );

        // Pass 2 — the child textures: the stand-in now carries exactly its mesh-AABB box.
        w.set_textured_keys([7u64, 9].into_iter().collect());
        w.advance(GeoView {
            camera_ecef: DVec3::new(6_378_937.0, 0.0, 0.0),
            fov_y_radians: 1.0,
            viewport_height: 1080.0,
            max_sse: 16.0,
            cull_distance_m: 5_000.0,
            frame: 2,
        });
        let f = w.latest_frame();
        let (p, c) = (by(&f, 7), by(&f, 9));
        assert_eq!(p.clip_boxes.len(), 1, "the stand-in is clipped by its one shown child");
        // The box is the child's decoded mesh (the tri_vertices triangle: (0,0,0)/(1,0,0)/(0,1,0)) bounded in
        // its OWN ground frame ([`oriented_clip_box`]). At the ENU origin the geocentric `up` ≈ render +Z, so
        // the box is centered on the triangle's footprint midpoint with ground half-extents 0.5/0.5 and a
        // wafer-thin up extent — assert with tolerances (the frame is up-tilted by the ~3e-9 centroid offset
        // against the earth radius, not bit-exact axis-aligned).
        let b = &p.clip_boxes[0];
        assert!(
            (b.center[0] - 0.5).abs() < 1e-3 && (b.center[1] - 0.5).abs() < 1e-3 && b.center[2].abs() < 1e-3,
            "the box sits on the child's footprint midpoint, got {:?}",
            b.center
        );
        let len = |a: [f32; 3]| (a[0] * a[0] + a[1] * a[1] + a[2] * a[2]).sqrt();
        let mut half_lens = [len(b.half_axes[0]), len(b.half_axes[1]), len(b.half_axes[2])];
        half_lens.sort_by(f32::total_cmp);
        // GROUND half-extents are the child's exact footprint (0.5/0.5 — untouched by the slab pad). The `up`
        // half-extent is the surface sliver PLUS the attach-time stand-in pad ([`extend_clip_box_up`]):
        // parent gE 100 m → 100·100·1.5 + 1000 = 16 000 cm, so the coarser parent's deviating sheet (bounded
        // by its geometricError) falls inside the discarded slab instead of poking through over the child.
        let want_pad = 100.0f32 * 100.0 * 1.5 + 1_000.0;
        assert!(
            (half_lens[0] - 0.5).abs() < 1e-3 && (half_lens[1] - 0.5).abs() < 1e-3,
            "ground half-extents stay the child's exact 0.5/0.5 footprint, got {half_lens:?}"
        );
        assert!(
            (half_lens[2] - want_pad).abs() < 1.0,
            "up half-extent = the stand-in's gE pad (~{want_pad} cm), got {half_lens:?}"
        );
        assert!(c.clip_boxes.is_empty(), "the fine child itself is never clipped");
    }

    #[test]
    fn flash_cache_evicts_farthest_then_stalest_over_budget() {
        let g = |n: usize| {
            Arc::new(TileGeometry {
                vertices: vec![0u8; n],
                indices: Vec::new(),
                texture: Vec::new(),
                bounds_center: [0.0; 3],
                bounds_radius: 0.0,
                unlit: false,
            })
        };
        // Distance-primary: at equal staleness, the FARTHEST tile is evicted first. (key, geom, last_seen, distance)
        let mut c = FlashCache::new(1000); // low-water = 800 (80%)
        c.insert(1, g(400), 30, 9000.0); // far
        c.insert(2, g(400), 30, 100.0); // near
        assert_eq!((c.len(), c.byte_len()), (2, 800));
        c.insert(3, g(400), 30, 500.0); // 1200 > 1000 budget → trim
        c.trim(30); // now == last_seen ⇒ staleness 0 ⇒ pure distance order
        assert!(
            !c.contains_key(1),
            "the FARTHEST tile is evicted first (distance-primary)"
        );
        assert!(c.contains_key(2) && c.contains_key(3));
        assert_eq!(c.byte_len(), 800, "trimmed down to the low-water mark, not lower");

        // Staleness breaks ties: at EQUAL distance, the longest-unseen tile goes first.
        let mut c2 = FlashCache::new(1000);
        c2.insert(10, g(400), 5, 1000.0); // same distance, seen long ago
        c2.insert(11, g(400), 50, 1000.0); // same distance, seen recently
        c2.insert(12, g(400), 50, 1000.0);
        c2.trim(50);
        assert!(
            !c2.contains_key(10),
            "at equal distance, the stalest (lowest last_seen) is evicted first"
        );
        assert!(c2.contains_key(11) && c2.contains_key(12));

        // `take` moves the mesh out and discounts its bytes (the warm-restore path).
        let m = c.take(2).expect("present");
        assert_eq!(m.byte_size(), 400);
        assert_eq!((c.len(), c.byte_len()), (1, 400));
        assert!(!c.contains_key(2));
        assert!(c.take(2).is_none(), "a second take is a miss");
    }

    #[test]
    fn decode_frees_raw_bytes_on_success() {
        let mut w = test_world(8, 4096);
        let b = TileBounds {
            center: DVec3::new(6_378_137.0, 0.0, 0.0),
            radius: 1.0,
        };
        let t = w.acquire_tile(1, b, 100.0, DMat4::IDENTITY).unwrap();
        w.deliver_bytes(t, Arc::from(vec![7u8; 500].into_boxed_slice()));
        assert!(
            w.world.get::<FetchedBytes>(t).unwrap().0.is_some(),
            "bytes present before decode"
        );
        w.advance(GeoView {
            camera_ecef: DVec3::new(6_378_937.0, 0.0, 0.0),
            fov_y_radians: 1.0,
            viewport_height: 1080.0,
            max_sse: 16.0,
            cull_distance_m: 5_000.0,
            frame: 1,
        });
        assert_eq!(w.tile_state(t), Some(TileState::Resident));
        assert!(
            w.world.get::<FetchedBytes>(t).unwrap().0.is_none(),
            "the raw GLB is freed after a successful decode — a resident tile pins only its mesh"
        );
    }

    #[test]
    fn evict_deposits_decoded_mesh_into_cache() {
        let mut w = test_world(8, 0); // cap 0 → any off-screen resident tile is evicted
        let b = TileBounds {
            center: DVec3::new(6_378_137.0, 0.0, 0.0),
            radius: 1.0,
        };
        let t = w.acquire_tile(42, b, 100.0, DMat4::IDENTITY).unwrap();
        w.deliver_bytes(t, Arc::from(vec![1u8; 100].into_boxed_slice()));
        // Frame 1: in view (within the 5 km cull) → stamped → decoded → resident, spared by evict.
        w.advance(GeoView {
            camera_ecef: DVec3::new(6_378_937.0, 0.0, 0.0),
            fov_y_radians: 1.0,
            viewport_height: 1080.0,
            max_sse: 16.0,
            cull_distance_m: 5_000.0,
            frame: 1,
        });
        assert_eq!(w.tile_state(t), Some(TileState::Resident));
        assert_eq!(w.flash_cache().0, 0, "still resident — nothing cached yet");
        // Frame 2: camera jumps far (beyond the cull) → unstamped → evicted → mesh deposited, not dropped.
        w.advance(GeoView {
            camera_ecef: DVec3::new(7_000_000.0, 0.0, 0.0),
            fov_y_radians: 1.0,
            viewport_height: 1080.0,
            max_sse: 16.0,
            cull_distance_m: 5_000.0,
            frame: 2,
        });
        assert_eq!(w.tile_state(t), Some(TileState::Free), "evicted back to Free");
        let (entries, bytes) = w.flash_cache();
        assert_eq!(entries, 1, "the decoded mesh is deposited into the LRU on eviction");
        assert_eq!(
            bytes, 108,
            "cache accounts for the mesh footprint (96 vertex bytes + 12 index bytes)"
        );
    }

    #[test]
    fn warm_reentry_restores_from_cache_without_refetch_or_redecode() {
        use logi_3dtiles_io::SyntheticSource;
        const C: f64 = 6_378_137.0;
        // A single renderable leaf at the prime-meridian equator (matches the ENU origin's ECEF).
        let root = format!(
            r#"{{"asset":{{"version":"1.1"}},"geometricError":2000,
              "root":{{"boundingVolume":{{"sphere":[{C},0,0,1]}},"geometricError":10,"content":{{"uri":"r.glb"}}}}}}"#
        );
        let enu = EnuFrame::from_cartographic(&Ellipsoid::wgs84(), Cartographic::from_degrees(0.0, 0.0, 0.0));
        // Stub decoder: any non-empty bytes → a tiny valid mesh (so the cycle runs without crafting a GLB).
        let decoder = TileDecoder(Arc::new(|bytes: &[u8], _xf: DMat4| {
            Some(TileGeometry {
                vertices: tri_vertices(),
                indices: vec![0, 1, 2],
                texture: Vec::new(),
                bounds_center: [0.0; 3],
                bounds_radius: bytes.len() as f32,
                unlit: false,
            })
        }));
        let mut w = GeoWorld::from_tileset_with_decoder(
            "https://example.invalid/root.json".to_string(),
            root.as_bytes(),
            enu,
            decoder,
            Box::new(SyntheticSource::new()),
            6000.0, // max_tile_distance (select cull)
            8,      // pool size
            0,      // max_resident: cap 0 → off-screen tiles evict immediately
        )
        .expect("tileset parses");

        let view = |cam: DVec3, frame: u64| GeoView {
            camera_ecef: cam,
            fov_y_radians: 1.0,
            viewport_height: 1080.0,
            max_sse: 16.0,
            cull_distance_m: 6000.0,
            frame,
        };
        let near = DVec3::new(C + 2000.0, 0.0, 0.0); // 2 km out → within cull → selected + stamped
        let far = DVec3::new(C + 50_000.0, 0.0, 0.0); // 50 km out → beyond cull → deselected + unstamped

        // Phase 1 (cold load): near camera → select → request → drain → decode → resident.
        let mut f = 1u64;
        while w.resident_tiles() == 0 && f <= 40 {
            w.advance_streaming(view(near, f));
            f += 1;
        }
        assert_eq!(w.resident_tiles(), 1, "tile cold-loaded to resident within 40 frames");
        assert_eq!(w.stats().flash_cache_hits_total, 0, "a cold load is not a cache hit");

        // Phase 2 (eviction): far camera → off-screen → evicted → mesh deposited, index reconciled.
        for ff in f..f + 3 {
            w.advance_streaming(view(far, ff));
        }
        f += 3;
        assert_eq!(w.resident_tiles(), 0, "off-screen tile evicted");
        assert_eq!(w.flash_cache().0, 1, "evicted mesh kept in the cache for re-entry");

        // Phase 3 (warm re-entry): near again → restored straight from the cache, no fetch, no decode.
        w.advance_streaming(view(near, f));
        assert_eq!(
            w.stats().flash_cache_hits_total,
            1,
            "re-entry served from the flash cache"
        );
        assert_eq!(w.resident_tiles(), 1, "restored to resident in a single frame");
        assert_eq!(
            w.flash_cache().0,
            0,
            "the mesh moved out of the cache onto the live slot"
        );
        assert_eq!(w.in_flight(), 0, "the warm restore issued no fetch");
        assert_eq!(w.entity_count(), 8, "no structural churn through the whole cycle");
    }

    #[test]
    fn multiple_contents_publishes_every_content_of_one_tile() {
        use logi_3dtiles_io::SyntheticSource;
        const C: f64 = 6_378_137.0;
        // 3D Tiles 1.1 multiple-contents: a single renderable leaf carrying TWO contents (`contents:[a,b]`,
        // no singular `content`). A conformant client renders BOTH — so the one tile must publish two render
        // tiles, with distinct keys, riding ONE pool entity (the extra is attached, not a second tile).
        let root = format!(
            r#"{{"asset":{{"version":"1.1"}},"geometricError":2000,
              "root":{{"boundingVolume":{{"sphere":[{C},0,0,1]}},"geometricError":10,
                "contents":[{{"uri":"a.glb"}},{{"uri":"b.glb"}}]}}}}"#
        );
        let enu = EnuFrame::from_cartographic(&Ellipsoid::wgs84(), Cartographic::from_degrees(0.0, 0.0, 0.0));
        // Stub decoder: bytes → a tiny valid mesh (the cycle runs without crafting real GLB payloads).
        let decoder = TileDecoder(Arc::new(|bytes: &[u8], _xf: DMat4| {
            Some(TileGeometry {
                vertices: tri_vertices(),
                indices: vec![0, 1, 2],
                texture: Vec::new(),
                bounds_center: [0.0; 3],
                bounds_radius: bytes.len() as f32,
                unlit: false,
            })
        }));
        let mut w = GeoWorld::from_tileset_with_decoder(
            "https://example.invalid/root.json".to_string(),
            root.as_bytes(),
            enu,
            decoder,
            Box::new(SyntheticSource::new()),
            6000.0, // max_tile_distance (select cull)
            8,      // pool size
            4096,   // max_resident
        )
        .expect("tileset parses");
        let view = |frame: u64| GeoView {
            camera_ecef: DVec3::new(C + 2000.0, 0.0, 0.0), // 2 km out → within cull → selected
            fov_y_radians: 1.0,
            viewport_height: 1080.0,
            max_sse: 16.0,
            cull_distance_m: 6000.0,
            frame,
        };
        // Drive until BOTH contents have fetched (a via `decode`, b inline) + published, or a frame cap.
        let mut f = 1u64;
        while w.latest_frame().tiles.len() < 2 && f <= 60 {
            w.advance_streaming(view(f));
            f += 1;
        }
        let frame = w.latest_frame();
        assert_eq!(
            frame.tiles.len(),
            2,
            "a two-content tile publishes BOTH of its contents as distinct render tiles"
        );
        assert_ne!(
            frame.tiles[0].tile_key, frame.tiles[1].tile_key,
            "each content gets its own stable render key (a distinct section in the C++ cache)"
        );
        assert!(
            frame.tiles.iter().all(|t| !t.geom.vertices.is_empty()),
            "both contents decoded to real geometry"
        );
        assert_eq!(
            w.resident_tiles(),
            1,
            "one tile resident — its additional content rides the primary's entity, not a second slot"
        );
        assert_eq!(
            w.entity_count(),
            8,
            "no extra pool slot consumed by the additional content"
        );
    }

    #[test]
    fn hysteresis_refined_set_tracks_the_traversal() {
        use logi_3dtiles_io::SyntheticSource;
        const C: f64 = 6_378_137.0;
        // Root (gE 1000) over two leaves (gE 10): near, the root refines; far, it doesn't.
        let root = format!(
            r#"{{"asset":{{"version":"1.1"}},"geometricError":2000,
              "root":{{"boundingVolume":{{"sphere":[{C},0,0,1]}},"geometricError":1000,"refine":"REPLACE","content":{{"uri":"r.glb"}},
                "children":[
                  {{"boundingVolume":{{"sphere":[{C},0,0,1]}},"geometricError":10,"content":{{"uri":"0.glb"}}}},
                  {{"boundingVolume":{{"sphere":[{C},0,0,1]}},"geometricError":10,"content":{{"uri":"1.glb"}}}}]}}}}"#
        );
        let enu = EnuFrame::from_cartographic(&Ellipsoid::wgs84(), Cartographic::from_degrees(0.0, 0.0, 0.0));
        let decoder = TileDecoder(Arc::new(|_b: &[u8], _x: DMat4| {
            Some(TileGeometry {
                vertices: tri_vertices(),
                indices: vec![0, 1, 2],
                texture: Vec::new(),
                bounds_center: [0.0; 3],
                bounds_radius: 1.0,
                unlit: false,
            })
        }));
        let mut w = GeoWorld::from_tileset_with_decoder(
            "https://example.invalid/root.json".to_string(),
            root.as_bytes(),
            enu,
            decoder,
            Box::new(SyntheticSource::new()),
            1.0e9, // no distance cull
            64,
            4096,
        )
        .expect("tileset parses");
        let view = |cam: DVec3, frame: u64| GeoView {
            camera_ecef: cam,
            fov_y_radians: 1.0,
            viewport_height: 1080.0,
            max_sse: 16.0,
            cull_distance_m: 0.0,
            frame,
        };
        // Near (2 km): the root's SSE ≫ target → it refines into its two leaves; the cross-frame state carries
        // the single interior split (the root path `[]`) forward for next frame's dead-band + kick.
        w.advance_streaming(view(DVec3::new(C + 2000.0, 0.0, 0.0), 1));
        assert_eq!(w.refined_count(), 1, "the root refined; its split is carried forward");
        // Far (5000 km): the root's SSE ≪ target → it renders (no refine), so the refined count clears.
        w.advance_streaming(view(DVec3::new(C + 5_000_000.0, 0.0, 0.0), 2));
        assert_eq!(
            w.refined_count(),
            0,
            "far away nothing refines — the refined set empties"
        );
    }

    #[test]
    fn fast_motion_cancels_stale_in_flight_fetches() {
        use logi_3dtiles_io::SyntheticSource;
        const C: f64 = 6_378_137.0;
        // A single renderable leaf; the synthetic source delivers only after ~10 ticks, so a fetch requested
        // one frame is still outstanding the next — long enough for fast motion to strand it.
        let root = format!(
            r#"{{"asset":{{"version":"1.1"}},"geometricError":2000,
              "root":{{"boundingVolume":{{"sphere":[{C},0,0,1]}},"geometricError":10,"content":{{"uri":"r.glb"}}}}}}"#
        );
        let enu = EnuFrame::from_cartographic(&Ellipsoid::wgs84(), Cartographic::from_degrees(0.0, 0.0, 0.0));
        let decoder = TileDecoder(Arc::new(|_b: &[u8], _x: DMat4| {
            Some(TileGeometry {
                vertices: tri_vertices(),
                indices: vec![0, 1, 2],
                texture: Vec::new(),
                bounds_center: [0.0; 3],
                bounds_radius: 1.0,
                unlit: false,
            })
        }));
        let mut w = GeoWorld::from_tileset_with_decoder(
            "https://example.invalid/root.json".to_string(),
            root.as_bytes(),
            enu,
            decoder,
            Box::new(SyntheticSource::new()),
            6000.0, // 6 km select cull
            8,
            4096,
        )
        .expect("tileset parses");
        let view = |cam: DVec3, frame: u64| GeoView {
            camera_ecef: cam,
            fov_y_radians: 1.0,
            viewport_height: 1080.0,
            max_sse: 16.0,
            cull_distance_m: 6000.0,
            frame,
        };
        // Frame 1 (near): the leaf is selected → its fetch is requested + outstanding (not yet delivered).
        w.advance_streaming(view(DVec3::new(C + 2000.0, 0.0, 0.0), 1));
        assert_eq!(w.in_flight(), 1, "the near tile's fetch is outstanding");
        assert_eq!(w.count_state(TileState::Requested), 1, "its pool slot is reserved");
        // Frame 2 (jumped 5000 km away): the tile leaves the selection before arriving → cancelled, slot freed.
        w.advance_streaming(view(DVec3::new(C + 5_000_000.0, 0.0, 0.0), 2));
        assert_eq!(w.in_flight(), 0, "the now-stale fetch was pre-emptively cancelled");
        assert_eq!(w.stats().fetches_cancelled_total, 1, "the cancellation is recorded");
        assert_eq!(
            w.count_state(TileState::Free),
            8,
            "the reserved slot returned to the pool"
        );
    }

    #[test]
    fn preemption_cancels_the_lowest_priority_inflight() {
        use logi_3dtiles_io::SyntheticSource;
        const C: f64 = 6_378_137.0;
        let root = format!(
            r#"{{"asset":{{"version":"1.1"}},"geometricError":2000,
              "root":{{"boundingVolume":{{"sphere":[{C},0,0,1]}},"geometricError":10,"content":{{"uri":"r.glb"}}}}}}"#
        );
        let mut w = GeoWorld::from_tileset(
            "https://example.invalid/root.json".to_string(),
            root.as_bytes(),
            Cartographic::from_degrees(0.0, 0.0, 0.0),
            Box::new(SyntheticSource::new()),
            1.0e9,
            256,
            4096,
        )
        .expect("tileset parses");
        // Issue three PRIMARY fetches at known priorities directly via `request_one` (private — same-module
        // test). The synthetic source holds them in flight (never polled), so the in-flight set has three known
        // scores to exercise the H6 preemption mechanics on.
        let mk = |w: &mut GeoWorld, step: u32| {
            let arena = &mut w.streaming.as_mut().unwrap().arena;
            let node = arena.intern_path(&[step]);
            let key = arena.key(node);
            SelectedTile {
                path: vec![step],
                node,
                key,
                world_transform: DMat4::IDENTITY,
                content_uris: vec!["a.glb".to_string()],
                geometric_error: 100.0,
                distance: 1000.0,
                bounds_center: DVec3::ZERO,
                bounds_radius: 1.0,
                is_add: false,
                clip_children: Vec::new(),
            }
        };
        let t1 = mk(&mut w, 1);
        let t2 = mk(&mut w, 2);
        let t3 = mk(&mut w, 3);
        assert!(matches!(
            w.request_one(&t1, ContentKind::Glb, "u1".to_string(), 10.0),
            Requested::Issued(Some(_))
        ));
        assert!(matches!(
            w.request_one(&t2, ContentKind::Glb, "u2".to_string(), 50.0),
            Requested::Issued(Some(_))
        ));
        assert!(matches!(
            w.request_one(&t3, ContentKind::Glb, "u3".to_string(), 100.0),
            Requested::Issued(Some(_))
        ));
        assert_eq!(w.in_flight(), 3, "three fetches outstanding");
        let free_before = w.count_state(TileState::Free);

        // The preemption candidate is the LOWEST-priority primary (score 10).
        let (min_id, min_pri) = w.lowest_priority_inflight().expect("a primary is in flight");
        assert_eq!(min_pri, 10.0, "the score-10 fetch is the preemption candidate");

        // Preempt it: in-flight drops to 2, its pool slot returns, and the cancel is recorded — mirroring the
        // stale-cancel teardown (no leaked slot). The higher-priority fetches survive.
        w.preempt_inflight(min_id);
        assert_eq!(w.in_flight(), 2, "the lowest-priority fetch was cancelled");
        assert_eq!(
            w.count_state(TileState::Free),
            free_before + 1,
            "its pool slot returned to the pool"
        );
        assert_eq!(w.stats().fetches_cancelled_total, 1, "the preemption is recorded");
        let (_, next_min) = w.lowest_priority_inflight().expect("two remain");
        assert_eq!(
            next_min, 50.0,
            "the score-50 fetch is now the lowest — the higher ones were untouched"
        );
    }

    #[test]
    fn fetch_priority_favors_the_look_direction() {
        let cam_pos = DVec3::new(6_378_137.0, 0.0, 0.0);
        let cam_dir = DVec3::new(1.0, 0.0, 0.0); // looking radially outward (+X)
        let tile = |center: DVec3| SelectedTile {
            path: Vec::new(),
            node: logi_3dtiles_select::ROOT_NODE,
            key: logi_3dtiles_select::path_key(&[]),
            world_transform: DMat4::IDENTITY,
            content_uris: Vec::new(),
            geometric_error: 100.0,
            distance: 1000.0,
            bounds_center: center,
            bounds_radius: 1.0,
            is_add: false,
            clip_children: Vec::new(),
        };
        // Same screen-importance (equal gE + distance); only the direction relative to the camera differs.
        let ahead = tile(cam_pos + DVec3::new(1000.0, 0.0, 0.0));
        let side = tile(cam_pos + DVec3::new(0.0, 1000.0, 0.0));
        let behind = tile(cam_pos + DVec3::new(-1000.0, 0.0, 0.0));
        let (pa, ps, pb) = (
            fetch_priority(&ahead, cam_pos, cam_dir),
            fetch_priority(&side, cam_pos, cam_dir),
            fetch_priority(&behind, cam_pos, cam_dir),
        );
        assert!(
            pa > ps && ps > pb,
            "look-direction order: ahead {pa} > side {ps} > behind {pb}"
        );

        // Unknown direction (ZERO) ⇒ the view-weight is uniform, so order falls back to pure importance
        // (equal here) — i.e. the look-direction term vanishes safely.
        let z = DVec3::ZERO;
        assert_eq!(
            fetch_priority(&ahead, cam_pos, z),
            fetch_priority(&behind, cam_pos, z),
            "no camera direction ⇒ no view-direction bias"
        );
    }

    #[test]
    fn fetch_priority_grafting_boost_leads_manifests() {
        let cam_pos = DVec3::new(6_378_137.0, 0.0, 0.0);
        let cam_dir = DVec3::ZERO; // direction-agnostic — isolate the manifest boost
        let at = cam_pos + DVec3::new(1000.0, 0.0, 0.0);
        let mk = |uri: &str| SelectedTile {
            path: Vec::new(),
            node: logi_3dtiles_select::ROOT_NODE,
            key: logi_3dtiles_select::path_key(&[]),
            world_transform: DMat4::IDENTITY,
            content_uris: vec![uri.to_string()],
            geometric_error: 100.0,
            distance: 1000.0,
            bounds_center: at,
            bounds_radius: 1.0,
            is_add: false,
            clip_children: Vec::new(),
        };
        // Same screen-importance + distance; only the content KIND differs. The tree-descending manifest must
        // sort ahead of geometry so refinement reaches the fine target fast (the skip-LOD intent).
        let manifest = mk("sub/tileset.json?session=X");
        let glb = mk("0/0/0.glb");
        let pm = fetch_priority(&manifest, cam_pos, cam_dir);
        let pg = fetch_priority(&glb, cam_pos, cam_dir);
        assert!(pm > pg, "a same-distance manifest ({pm}) outranks .glb content ({pg})");
        assert!(
            (pm / pg - MANIFEST_FETCH_BOOST).abs() < 1e-9,
            "the boost is exactly {MANIFEST_FETCH_BOOST}x"
        );
    }

    #[test]
    fn frustum_aware_lod_refines_deeper_in_view_than_off_view() {
        use logi_3dtiles_io::SyntheticSource;
        const C: f64 = 6_378_137.0;
        // A 3-level tileset at the ENU origin (all tiles share the equator point, so the whole tree is in or
        // out of view together — isolating the frustum's effect from per-tile geometry).
        let root = format!(
            r#"{{"asset":{{"version":"1.1"}},"geometricError":2000,
              "root":{{"boundingVolume":{{"sphere":[{C},0,0,1]}},"geometricError":1000,"refine":"REPLACE","content":{{"uri":"r.glb"}},
                "children":[
                  {{"boundingVolume":{{"sphere":[{C},0,0,1]}},"geometricError":100,"content":{{"uri":"0.glb"}},
                    "children":[{{"boundingVolume":{{"sphere":[{C},0,0,1]}},"geometricError":10,"content":{{"uri":"00.glb"}}}}]}},
                  {{"boundingVolume":{{"sphere":[{C},0,0,1]}},"geometricError":100,"content":{{"uri":"1.glb"}},
                    "children":[{{"boundingVolume":{{"sphere":[{C},0,0,1]}},"geometricError":10,"content":{{"uri":"10.glb"}}}}]}}]}}}}"#
        );
        let enu = EnuFrame::from_cartographic(&Ellipsoid::wgs84(), Cartographic::from_degrees(0.0, 0.0, 0.0));
        let decoder = TileDecoder(Arc::new(|_b: &[u8], _x: DMat4| {
            Some(TileGeometry {
                vertices: tri_vertices(),
                indices: vec![0, 1, 2],
                texture: Vec::new(),
                bounds_center: [0.0; 3],
                bounds_radius: 1.0,
                unlit: false,
            })
        }));
        let mut w = GeoWorld::from_tileset_with_decoder(
            "https://example.invalid/root.json".to_string(),
            root.as_bytes(),
            enu,
            decoder,
            Box::new(SyntheticSource::new()),
            1.0e9, // no distance cull — isolate the frustum effect
            256,
            4096,
        )
        .expect("tileset parses");

        // Camera 4.5 km "north" of the tiles (UE −Y maps to ENU +North). At 4.5 km the mid-children sit at
        // SSE ≈ 22 px: above the 16 px in-view target (refine) but below the 32 px culled target
        // (= 16 × CULLED_SSE_FACTOR of 2.0 → stop), so in-view refines exactly one level deeper than off-view.
        let cam = [0.0, -450_000.0, 0.0];
        let up = [0.0, 0.0, 1.0];
        let (aspect, fov, vh, sse) = (16.0 / 9.0, 60f64.to_radians(), 1080.0, 16.0);

        // Looking TOWARD the tiles (UE +Y) → in the frustum, within the detail band → full SSE → refine deep.
        w.advance_from_ue(cam, [0.0, 1.0, 0.0], up, aspect, fov, vh, sse, 1);
        let toward = w.refined_count();
        // Looking 90° to the SIDE (UE +X / East) → off-frustum but NOT behind the camera → kept as a coarse
        // shell (culled SSE) for shadows + camera-turn readiness → refines shallower (NOT hard-culled).
        w.advance_from_ue(cam, [1.0, 0.0, 0.0], up, aspect, fov, vh, sse, 2);
        let side = w.refined_count();
        // Looking 180° AWAY (UE −Y) → the tiles are fully BEHIND the camera → hard-culled (audit E #5): not even
        // a coarse shell. The user's "behind ⇒ nothing".
        w.advance_from_ue(cam, [0.0, -1.0, 0.0], up, aspect, fov, vh, sse, 3);
        let behind = w.refined_count();

        assert!(
            toward > side,
            "in-view refines deeper ({toward}) than the off-view coarse shell ({side})"
        );
        assert!(
            side >= 1,
            "off-view (not behind) tiles are KEPT coarse, not hard-culled — the root still refines"
        );
        assert_eq!(
            behind, 0,
            "behind-camera tiles are hard-culled (audit E #5) — nothing refines"
        );
    }

    #[test]
    fn base_for_picks_the_nearest_grafted_ancestor() {
        let root = "https://h/root.json?session=R";
        let mut arena = logi_3dtiles_select::PathArena::new();
        let mut bases: HashMap<u32, String> = HashMap::new();
        bases.insert(arena.intern_path(&[0]), "https://h/0.json?session=A".to_string());
        bases.insert(arena.intern_path(&[0, 1]), "https://h/01.json?session=B".to_string());
        bases.insert(arena.intern_path(&[2]), "https://h/2.json?session=C".to_string()); // sibling graft

        // Intern every probe node FIRST (the walk itself only READS the arena).
        let n_root = arena.intern_path(&[]);
        let n_1 = arena.intern_path(&[1]);
        let n_0 = arena.intern_path(&[0]);
        let n_02 = arena.intern_path(&[0, 2]);
        let n_01 = arena.intern_path(&[0, 1]);
        let n_015 = arena.intern_path(&[0, 1, 5]);
        let n_012 = arena.intern_path(&[0, 1, 2]);
        // No grafted ancestor → the root base.
        assert_eq!(base_for_node(root, &bases, &arena, n_root), root);
        assert_eq!(base_for_node(root, &bases, &arena, n_1), root);
        // The one grafted ancestor on the chain.
        assert_eq!(base_for_node(root, &bases, &arena, n_0), "https://h/0.json?session=A");
        assert_eq!(base_for_node(root, &bases, &arena, n_02), "https://h/0.json?session=A");
        // The NEAREST ancestor wins ([0,1] over [0]) — deeper auth overrides shallower.
        assert_eq!(base_for_node(root, &bases, &arena, n_01), "https://h/01.json?session=B");
        assert_eq!(
            base_for_node(root, &bases, &arena, n_015),
            "https://h/01.json?session=B"
        );
        // The sibling graft [2] is not an ancestor of [0,1,2] → ignored.
        assert_eq!(
            base_for_node(root, &bases, &arena, n_012),
            "https://h/01.json?session=B"
        );
    }

    #[test]
    fn graft_external_refuses_beyond_the_depth_cap() {
        // C1 defense-in-depth: a graft at/beyond `MAX_TILE_DEPTH` is rejected before parsing — bounding a
        // hostile external-tileset chain from growing the tree without limit. The check precedes `tile_at_mut`,
        // so the (shallow) root needn't actually contain the deep path.
        let root_json = br#"{"asset":{"version":"1.1"},"geometricError":1.0,
            "root":{"boundingVolume":{"sphere":[0,0,0,1]},"geometricError":0.0}}"#;
        let mut root = parse_tileset(root_json).unwrap().root;
        let mut grafted: HashMap<u32, GraftRecord> = HashMap::new();
        let deep_path = vec![0u32; MAX_TILE_DEPTH as usize]; // length == cap ⇒ refused (`>=`)
        assert!(
            graft_external(&mut root, &mut grafted, &deep_path, 7, root_json, 0).is_err(),
            "a graft at/beyond the depth cap must be refused"
        );
        assert!(grafted.is_empty(), "a refused graft records nothing");
    }

    #[test]
    fn graft_external_refuses_unsupported_required_extension() {
        // The root-tileset `extensionsRequired` gate must also hold on the GRAFT path: an external tileset
        // requiring an extension we don't implement is refused loud (no silent wrong rendering), and the
        // placeholder tile is left un-grafted (re-fetchable).
        let root_json = br#"{"asset":{"version":"1.1"},"geometricError":1.0,
            "root":{"boundingVolume":{"sphere":[0,0,0,1]},"geometricError":0.0,
            "children":[{"boundingVolume":{"sphere":[0,0,0,1]},"geometricError":0.0}]}}"#;
        let bad_child = br#"{"asset":{"version":"1.1"},"geometricError":1.0,
            "extensionsRequired":["3DTILES_unobtainium"],
            "root":{"boundingVolume":{"sphere":[0,0,0,1]},"geometricError":0.0}}"#;
        let mut root = parse_tileset(root_json).unwrap().root;
        let mut grafted: HashMap<u32, GraftRecord> = HashMap::new();
        let err = graft_external(&mut root, &mut grafted, &[0], 7, bad_child, 0);
        assert!(err.is_err(), "an unsupported required extension must refuse the graft");
        assert!(
            format!("{:?}", err.unwrap_err()).contains("3DTILES_unobtainium"),
            "the refusal names the offending extension"
        );
        assert!(grafted.is_empty(), "a refused graft records nothing");
        assert!(root.children[0].children.is_empty(), "the placeholder stays un-grafted");
    }

    /// A source that returns a valid child tileset for the external-tileset URL (so a graft happens) and
    /// empty bytes (a failed fetch) for anything else — enough to drive a graft → cold → prune → re-graft
    /// cycle deterministically and offline.
    struct GraftingSource {
        pending: Vec<(RequestId, String)>,
        child_json: Vec<u8>,
    }
    impl TileContentSource for GraftingSource {
        fn request(&mut self, id: RequestId, url: &str, _priority: f64, _now: u64) {
            self.pending.push((id, url.to_string()));
        }
        fn poll_completed(&mut self, _now: u64, out: &mut Vec<logi_3dtiles_io::Loaded>) {
            for (id, url) in self.pending.drain(..) {
                let result = if url.contains("sub.json") {
                    Ok(self.child_json.clone()) // sniffs as a JSON tileset → grafts
                } else {
                    // A leaf .glb "fails" terminally — irrelevant to the prune lifecycle (no retry churn).
                    Err(logi_3dtiles_io::FetchError::Http {
                        status: 404,
                        retry_after: None,
                    })
                };
                out.push(logi_3dtiles_io::Loaded {
                    id,
                    result: result.map(Arc::from),
                });
            }
        }
        fn cancel(&mut self, id: RequestId) {
            self.pending.retain(|(i, _)| *i != id);
        }
        fn in_flight(&self) -> usize {
            self.pending.len()
        }
    }

    #[test]
    fn cold_grafted_subtree_is_pruned_then_re_grafted_without_churn() {
        const C: f64 = 6_378_137.0;
        // Root refines into one child whose content is an external tileset (`sub.json`).
        let root = format!(
            r#"{{"asset":{{"version":"1.1"}},"geometricError":2000,
              "root":{{"boundingVolume":{{"sphere":[{C},0,0,1]}},"geometricError":1000,"refine":"REPLACE","content":{{"uri":"r.glb"}},
                "children":[
                  {{"boundingVolume":{{"sphere":[{C},0,0,1]}},"geometricError":10,"content":{{"uri":"sub.json"}}}}]}}}}"#
        );
        // The grafted child tileset: a single leaf with a .glb.
        let child = format!(
            r#"{{"asset":{{"version":"1.1"}},"geometricError":50,
              "root":{{"boundingVolume":{{"sphere":[{C},0,0,1]}},"geometricError":10,"content":{{"uri":"leaf.glb"}}}}}}"#
        );
        let enu = EnuFrame::from_cartographic(&Ellipsoid::wgs84(), Cartographic::from_degrees(0.0, 0.0, 0.0));
        let decoder = TileDecoder(Arc::new(|_b: &[u8], _x: DMat4| None)); // decode is irrelevant here
        let src = Box::new(GraftingSource {
            pending: Vec::new(),
            child_json: child.into_bytes(),
        });
        let mut w = GeoWorld::from_tileset_with_decoder(
            "https://example.invalid/root.json".to_string(),
            root.as_bytes(),
            enu,
            decoder,
            src,
            1.0e9, // no distance cull
            64,
            4096,
        )
        .expect("tileset parses");

        let near = |frame: u64| GeoView {
            camera_ecef: DVec3::new(C + 2000.0, 0.0, 0.0), // 2 km out → root refines into the child
            fov_y_radians: 1.0,
            viewport_height: 1080.0,
            max_sse: 16.0,
            cull_distance_m: 0.0,
            frame,
        };
        // Far AND slightly moving (a parked identical view is exactly what the §3.1 re-selection gate — like
        // production's outer work gate — skips entirely; the cold-prune counts FULL steps, i.e. activity).
        let far = |frame: u64| GeoView {
            camera_ecef: DVec3::new(C + 20_000_000.0 + frame as f64, 0.0, 0.0), // far → child not visited
            fov_y_radians: 1.0,
            viewport_height: 1080.0,
            max_sse: 16.0,
            cull_distance_m: 0.0,
            frame,
        };

        // Near for a few frames: the child [0] is selected as a tileset, fetched, and grafted.
        for f in 1..=4u64 {
            w.advance_streaming(near(f));
        }
        assert_eq!(w.grafted_count(), 1, "the external child subtree should be grafted");

        // Far long enough to cross the cold threshold AND hit a prune-interval boundary → the cold graft is
        // un-grafted. (Bounded loop so a regression fails fast instead of hanging.)
        let mut f = 5u64;
        while w.grafted_count() > 0 && f < GRAFT_COLD_FRAMES + 4 * GRAFT_PRUNE_INTERVAL {
            w.advance_streaming(far(f));
            f += 1;
        }
        assert_eq!(w.grafted_count(), 0, "a cold grafted subtree must be pruned");
        assert_eq!(
            w.count_state(TileState::Free),
            64,
            "every slot under the pruned subtree was freed"
        );

        // The placeholder was restored re-fetchable: returning near re-grafts it.
        for ff in f..f + 6 {
            w.advance_streaming(near(ff));
        }
        assert_eq!(
            w.grafted_count(),
            1,
            "returning to the region re-grafts the restored placeholder"
        );
        assert_eq!(
            w.entity_count(),
            64,
            "no structural churn across graft → prune → re-graft"
        );
    }

    /// A `.subtree` with all tiles + content available and no child subtrees — constant bitstreams, so there is
    /// no binary chunk (audit S1 test fixture). The JSON is padded to an 8-byte boundary so `parse_subtree`'s
    /// `align8(json_end)` lands inside the buffer with a zero-length binary chunk.
    fn constant_subtree_bytes() -> Vec<u8> {
        let mut json: Vec<u8> =
            br#"{"tileAvailability":{"constant":1},"contentAvailability":{"constant":1},"childSubtreeAvailability":{"constant":0}}"#
                .to_vec();
        while !(24 + json.len()).is_multiple_of(8) {
            json.push(b' '); // trailing whitespace keeps the JSON valid while 8-aligning the binary chunk
        }
        let mut out = Vec::new();
        out.extend_from_slice(b"subt"); // magic
        out.extend_from_slice(&1u32.to_le_bytes()); // version 1
        out.extend_from_slice(&(json.len() as u64).to_le_bytes()); // JSON byte length
        out.extend_from_slice(&0u64.to_le_bytes()); // binary byte length (none — all constant)
        out.extend_from_slice(&json);
        out
    }

    /// Serves the implicit lane: a constant "all available" `.subtree` for any `.subtree` URL and stub GLB
    /// bytes for any content URL, recording every requested URL so the test can assert the scene fetched the
    /// subtree and then descended into implicit content.
    struct ImplicitSource {
        pending: Vec<(RequestId, String)>,
        requested: Arc<std::sync::Mutex<Vec<String>>>,
    }
    impl TileContentSource for ImplicitSource {
        fn request(&mut self, id: RequestId, url: &str, _priority: f64, _now: u64) {
            self.requested.lock().unwrap().push(url.to_string());
            self.pending.push((id, url.to_string()));
        }
        fn poll_completed(&mut self, _now: u64, out: &mut Vec<logi_3dtiles_io::Loaded>) {
            for (id, url) in self.pending.drain(..) {
                let bytes = if url.contains(".subtree") {
                    constant_subtree_bytes()
                } else {
                    vec![1u8, 2, 3] // stub GLB → the decoder turns N>0 bytes into a tiny-triangle resident mesh
                };
                out.push(logi_3dtiles_io::Loaded {
                    id,
                    result: Ok(bytes.into()),
                });
            }
        }
        fn cancel(&mut self, id: RequestId) {
            self.pending.retain(|(i, _)| *i != id);
        }
        fn in_flight(&self) -> usize {
            self.pending.len()
        }
    }

    /// End-to-end implicit tiling (audit S1): an `implicitTiling` root with NO explicit children must fetch its
    /// `.subtree`, then synthesize + stream the quadtree descendants the subtree reports available. Proves the
    /// scene's subtree fetch-lane + `SubtreeAvailability` provider + the descent through `select_tree_implicit`.
    #[test]
    fn implicit_root_fetches_its_subtree_then_streams_implicit_descendants() {
        const C: f64 = 6_378_137.0;
        // P3DT shape: the root carries `implicitTiling` + a templated content URI and no explicit `children`.
        // subtreeLevels=3 ⇒ one subtree covers levels 0..2; availableLevels=2 ⇒ refine to level 1 (4 children).
        let root = format!(
            r#"{{"asset":{{"version":"1.1"}},"geometricError":100000,
              "root":{{"boundingVolume":{{"sphere":[{C},0,0,1000]}},"geometricError":50000,"refine":"REPLACE",
                "content":{{"uri":"content/{{level}}/{{x}}/{{y}}.glb"}},
                "implicitTiling":{{"subdivisionScheme":"QUADTREE","subtreeLevels":3,"availableLevels":2,
                  "subtrees":{{"uri":"subtrees/{{level}}/{{x}}/{{y}}.subtree"}}}}}}}}"#
        );
        let enu = EnuFrame::from_cartographic(&Ellipsoid::wgs84(), Cartographic::from_degrees(0.0, 0.0, 0.0));
        // Decoder: N>0 content bytes → a tiny-triangle geometry, so a fetched implicit tile becomes resident.
        let decoder = TileDecoder(Arc::new(|bytes: &[u8], _x: DMat4| {
            (!bytes.is_empty()).then(|| TileGeometry {
                vertices: tri_vertices(),
                indices: vec![0, 1, 2],
                texture: Vec::new(),
                bounds_center: [0.0; 3],
                bounds_radius: 1.0,
                unlit: false,
            })
        }));
        let requested = Arc::new(std::sync::Mutex::new(Vec::new()));
        let src = Box::new(ImplicitSource {
            pending: Vec::new(),
            requested: requested.clone(),
        });
        let mut w = GeoWorld::from_tileset_with_decoder(
            "https://example.invalid/root.json".to_string(),
            root.as_bytes(),
            enu,
            decoder,
            src,
            1.0e9, // no distance cull
            256,
            4096,
        )
        .expect("implicit tileset parses");

        // Camera inside the root's bounding sphere → distance ≈ 0 → huge SSE → the root demands refinement,
        // which can only happen by fetching the `.subtree` (frame 1 → Pending), caching it, then descending.
        let view = |frame: u64| GeoView {
            camera_ecef: DVec3::new(C + 50.0, 0.0, 0.0),
            fov_y_radians: 1.0,
            viewport_height: 1080.0,
            max_sse: 1.0,
            cull_distance_m: 0.0,
            frame,
        };
        for f in 1..=40u64 {
            w.advance_streaming(view(f));
        }

        let urls = requested.lock().unwrap();
        assert!(
            urls.iter().any(|u| u.contains(".subtree")),
            "the implicit root's `.subtree` must be fetched (it gates all refinement)"
        );
        assert!(
            urls.iter().any(|u| u.contains("content/")),
            "implicit content tiles must be fetched once the subtree reports them available"
        );
        assert!(
            w.resident_tiles() > 1,
            "the implicit traversal must descend below the root — only {} resident",
            w.resident_tiles()
        );
    }

    /// R2 — `childSubtreeAvailability`: a parent subtree that declares NO child subtrees (`constant 0`, as
    /// `constant_subtree_bytes` does) must make the traversal PRUNE the child-subtree boundary, NOT fetch a
    /// non-existent child `.subtree`. With `subtreeLevels=2` the root subtree covers levels 0..1, so crossing to
    /// level 2 needs a child subtree the parent says doesn't exist. Without R2 the scene fetched `subtrees/2/…`
    /// (a 404 that held the region coarse for a round trip + filled `subtree_failed`); with R2 it fetches only
    /// the root subtree.
    #[test]
    fn child_subtree_availability_prunes_a_nonexistent_child_subtree() {
        const C: f64 = 6_378_137.0;
        let root = format!(
            r#"{{"asset":{{"version":"1.1"}},"geometricError":100000,
              "root":{{"boundingVolume":{{"sphere":[{C},0,0,1000]}},"geometricError":50000,"refine":"REPLACE",
                "content":{{"uri":"content/{{level}}/{{x}}/{{y}}.glb"}},
                "implicitTiling":{{"subdivisionScheme":"QUADTREE","subtreeLevels":2,"availableLevels":4,
                  "subtrees":{{"uri":"subtrees/{{level}}/{{x}}/{{y}}.subtree"}}}}}}}}"#
        );
        let enu = EnuFrame::from_cartographic(&Ellipsoid::wgs84(), Cartographic::from_degrees(0.0, 0.0, 0.0));
        let decoder = TileDecoder(Arc::new(|bytes: &[u8], _x: DMat4| {
            (!bytes.is_empty()).then(|| TileGeometry {
                vertices: tri_vertices(),
                indices: vec![0, 1, 2],
                texture: Vec::new(),
                bounds_center: [0.0; 3],
                bounds_radius: 1.0,
                unlit: false,
            })
        }));
        let requested = Arc::new(std::sync::Mutex::new(Vec::new()));
        let src = Box::new(ImplicitSource {
            pending: Vec::new(),
            requested: requested.clone(),
        });
        let mut w = GeoWorld::from_tileset_with_decoder(
            "https://example.invalid/root.json".to_string(),
            root.as_bytes(),
            enu,
            decoder,
            src,
            1.0e9,
            256,
            4096,
        )
        .expect("implicit tileset parses");
        let view = |frame: u64| GeoView {
            camera_ecef: DVec3::new(C + 50.0, 0.0, 0.0),
            fov_y_radians: 1.0,
            viewport_height: 1080.0,
            max_sse: 1.0,
            cull_distance_m: 0.0,
            frame,
        };
        for f in 1..=40u64 {
            w.advance_streaming(view(f));
        }
        let urls = requested.lock().unwrap();
        assert!(
            urls.iter().any(|u| u.contains("subtrees/0/")),
            "the root subtree must still be fetched (it gates all refinement)"
        );
        assert!(
            !urls.iter().any(|u| u.contains("subtrees/2/")),
            "R2: a child subtree the parent declares absent (childSubtreeAvailability=0) must NOT be fetched; got {:?}",
            urls.iter().filter(|u| u.contains(".subtree")).collect::<Vec<_>>()
        );
    }
}
