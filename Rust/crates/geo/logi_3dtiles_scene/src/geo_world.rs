//! The Geo **ECS orchestration layer** (ADR-0012 Addendum A; design = the plan §2A; constraints =
//! `Docs/Research/ECS-Multithreading-for-Geo.md`). Terrain tiles are `bevy_ecs` **entities**; the Geo
//! stream thread runs this `Schedule` and the heavy per-tile work fans out via `Query::par_iter` over the
//! **shared global `ComputeTaskPool`**. This is the full orchestration layer: the entity/component model,
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
//! - **Right-sized systems.** [`decode`] is the `par_iter` beneficiary (heavy per-tile work, **small** batch
//!   so the pool work-steals the variable tile cost). The cheap per-tile work is **fused** into one
//!   coarsely-batched [`sse_cull_lru`] system — never a `par_iter` per cheap op (a measured net loss).
//! - **Change detection** gates [`decode`] (`Changed<FetchedBytes>`) so unchanged tiles skip the work body.

use crate::frustum::ViewFrustum;
use crate::streaming::{is_tileset_uri, sniff_content_kind, tile_at_mut, ContentKind, StreamError, TileGeometry};
use bevy_ecs::batching::BatchingStrategy;
use bevy_ecs::prelude::*;
use bevy_tasks::{ComputeTaskPool, TaskPoolBuilder};
use glam::{DMat4, DVec3};
use hashbrown::{HashMap, HashSet};
use logi_3dtiles_implicit::{parse_subtree, Subtree};
use logi_3dtiles_io::{http_get, resolve_url, DiskCache, FetchError, HttpSource, RequestId, TileContentSource};
use logi_3dtiles_select::{
    select_tree_implicit, AvailabilityProvider, ImplicitContext, SelectParams, SelectedTile, SelectionResult,
    SelectionState, TileAvailability, Visibility, MAX_TILE_DEPTH,
};
use logi_3dtiles_tileset::{parse_tileset, Content, Refine, Tile};
use logi_geo_mesh::ue_cm_to_enu;
use logi_geospatial::{Cartographic, EnuFrame};
use std::collections::VecDeque;
use std::sync::Arc;

/// A tile's child-index path from the root — the stable identity keying the tree ↔ entity map.
type TilePath = Vec<u32>;

/// One placeholder restore queued by [`GeoWorld::prune_cold_grafts`]: `(path, transform, content, contents,
/// children)` — the [`GraftRecord`] fields re-applied to un-graft a cold subtree's placeholder tile.
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

/// Per-frame screen-space error (pixels).
#[derive(Component, Clone, Copy, Debug)]
pub struct Sse(pub f32);

/// Per-frame camera distance (metres, clamped ≥ 0).
#[derive(Component, Clone, Copy, Debug)]
pub struct CamDistance(pub f32);

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
/// parallel-decode path — so the single-content case (every Google P3DT tile, and the overwhelming majority of
/// tilesets) is **byte-identical** to before and this vec stays EMPTY, leaving the hot path untouched. Each
/// ADDITIONAL content (`[1..]`) rides this same entity: it is fetched with a `content_index` tag, inline-decoded
/// when it lands (the rare path — off the parallel decoder), and parked here. [`publish`] emits one render tile
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
/// decode failure — it never panics). An `Arc<dyn Fn>` so the [`decode`] `par_iter` shares one impl across
/// every worker thread. Production wires `logi_gltf::decode_glb` + the ENU rebase (the next increment);
/// tests wire a stub.
pub type TileDecodeFn = Arc<dyn Fn(&[u8], DMat4) -> Option<TileGeometry> + Send + Sync>;

/// The pluggable per-tile decode, as a resource the [`decode`] system reads.
#[derive(Resource, Clone)]
pub struct TileDecoder(pub TileDecodeFn);

/// The **production** [`TileDecoder`]: real glTF decode (`logi_gltf::decode_glb`) + the ENU rebase
/// ([`crate::streaming::rebase_gltf`]), sharing one `EnuFrame` across the parallel [`decode`] workers. A
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
/// Maintained at every `Resident` transition — `decode` (via [`NewResidents`]), `restore_cached`, `evict`,
/// `prune_cold_grafts` — and drift-checked against a full scan in debug builds (`evict`), so a missed
/// transition fails tests rather than silently dropping renders in release.
#[derive(Resource, Default)]
struct ResidentSet(HashSet<Entity>);

/// Entities promoted to `Resident` by the parallel `decode` this step. `decode` is `par_iter`, so it can't
/// touch [`ResidentSet`] directly; it pushes here (a tiny per-step set under a mutex — only this step's
/// arrivals), and the sequential `evict` drains this into [`ResidentSet`] before using it.
#[derive(Resource, Default)]
struct NewResidents(std::sync::Mutex<Vec<Entity>>);

/// Default decoded-mesh LRU budget (bytes). Sized as a re-entry buffer in front of the resident set: large
/// enough that a camera reversing direction restores meshes instead of re-decoding, small enough to bound
/// RAM. Tune on a flamegraph / RAM headroom (plan §6, Phase 1); override per-world with
/// [`GeoWorld::set_mesh_cache_budget`].
pub const DEFAULT_MESH_CACHE_BYTES: usize = 384 * 1024 * 1024;

/// Consecutive failed fetches a tile path is retried before it's given up. A network fetch (unlike decode)
/// can fail transiently — a dropped connection, a 5xx, an expired session — so a single failure must NOT be
/// terminal (that was the bug: a failed fetch stranded the tile as `Requested` forever AND leaked its pool
/// slot, so its region showed a coarse ancestor — "low quality" — permanently). Each failure frees the slot
/// and re-selects the path; only after this many consecutive misses is it abandoned, so a genuinely dead URL
/// (a persistent 404 / removed tile) can't storm the network or leak slots without bound. A success resets
/// the count. ~4 rides out a brief outage while still converging quickly on a truly dead tile.
const MAX_FETCH_ATTEMPTS: u32 = 4;

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

/// Backoff for a TRANSIENT fetch failure (audit A1), in advance-steps (`now_tick`). The stream loop advances
/// at most ~once/ms (`SHORT_PARK`), so these are ~tens-to-hundreds of ms — enough to stop the sub-millisecond
/// retry STORM the old code caused (it dropped a failed path and re-selected it the very next iteration with
/// no delay, so a wave of 429s would hammer an already-overloaded server). Doubles per attempt; a per-path
/// jitter de-synchronizes a herd of tiles that all fail together. Tick-based (never wall-clock) so the
/// deterministic streaming tests stay reproducible.
const RETRY_BACKOFF_BASE_TICKS: u64 = 24;

/// Upper bound on the stream loop's step rate (`SHORT_PARK` = 1 ms ⇒ ≤ ~1000 steps/s), used to convert a
/// server `Retry-After` (wall seconds) into a tick floor. Over-estimating the rate makes us wait AT LEAST as
/// long as the server asked (never less) before retrying — the point of honoring the header.
const APPROX_TICKS_PER_SEC: u64 = 1000;

/// Cap on a `Retry-After`-derived wait so a hostile/huge header value can't pin a path out of streaming for an
/// absurd time (it still retries eventually, bounded by [`MAX_FETCH_ATTEMPTS`]). 60 s of steps.
const RETRY_AFTER_MAX_TICKS: u64 = 60 * APPROX_TICKS_PER_SEC;

/// Minimum advance steps between Google P3DT **session refreshes**. A P3DT session (the `?session=…` the root
/// tileset mints) has a TTL; once it expires EVERY new tile/subtree fetch returns 401/403 at once. One refresh
/// (re-fetch the root → fresh session for the whole tree) fixes the wave, so this cooldown collapses the 401
/// storm into a single blocking root re-fetch instead of one per failed tile. ~2 s of steps.
const SESSION_REFRESH_COOLDOWN: u64 = 2000;

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
/// saves exactly the four `Tile` fields it overwrites; [`GeoWorld::prune_cold_grafts`] restores them, which
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

struct MeshCacheEntry {
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

/// A byte-budgeted LRU of **decoded** tile geometry, keyed by `tile_key`, that outlives slot eviction. When
/// the traversal re-selects a recently-evicted tile (camera reversing, orbiting, or jitter at the working-set
/// boundary), [`GeoWorld::select_request`] restores its mesh straight onto a pool slot — **no re-fetch and no
/// re-decode** (decode is the costly step the audit flagged for re-entry hitching). It holds an [`Arc`], so a
/// published render frame can still own a copy after the cache drops it. Eviction is **amortized**: a `trim`
/// is a no-op until total bytes cross `max_bytes`, then it drops the coldest entries (lowest `last_seen`)
/// down to a low-water mark, so the O(n log n) sort is paid only once per ~20% of cache turnover.
#[derive(Resource)]
pub struct MeshCache {
    entries: HashMap<u64, MeshCacheEntry>,
    bytes: usize,
    max_bytes: usize,
}

impl MeshCache {
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
        let entry = MeshCacheEntry {
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
                                                             // Score each entry (higher = evict first), then drop from the top until back to the low-water mark.
        let mut scored: Vec<(u64, f32)> = self
            .entries
            .iter()
            .map(|(&k, e)| {
                let stale = now.saturating_sub(e.last_seen) as f32;
                (k, e.distance * (1.0 + EVICT_STALENESS_FACTOR * stale))
            })
            .collect();
        scored.sort_unstable_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal)); // hottest-to-evict first
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

/// Per-step telemetry (read by the stream thread for the mailbox / diagnostics).
#[derive(Resource, Clone, Copy, Debug, Default)]
pub struct GeoStats {
    pub resident: u32,
    pub decoded_this_step: u32,
    pub evicted_this_step: u32,
    /// Cumulative tiles whose payload failed to decode — terminal (never re-requested). A dataset-health
    /// signal: steady zero is healthy; a climbing count means corrupt/unsupported tiles in the stream.
    pub decode_failed_total: u32,
    /// Cumulative warm re-entries served from the decoded-mesh LRU (`MeshCache`) — a tile re-selected after
    /// eviction whose mesh was restored with **no re-fetch and no re-decode**. The cache's effectiveness.
    pub mesh_cache_hits_total: u32,
    /// Cumulative in-flight fetches **pre-emptively cancelled** because their tile left the selection before
    /// arriving (fast camera motion). Bandwidth that would otherwise have been spent on already-stale tiles.
    pub fetches_cancelled_total: u32,
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
}

/// A published render frame — the `Resident` tiles in view this step, geometry pinned via `Arc`. Built by
/// [`publish`] into [`GeoMailbox`]; the stream thread mirrors it to the cross-thread handoff (ADR-0012 §3).
#[derive(Clone, Default)]
pub struct GeoRenderFrame {
    pub frame: u64,
    pub origin_ecef: [f64; 3],
    pub tiles: Vec<GeoRenderTile>,
}

/// The latest published render frame (the schedule's output). Read via [`GeoWorld::latest_frame`].
#[derive(Resource, Default)]
struct GeoMailbox(GeoRenderFrame);

/// Batch size for the cheap, near-uniform [`sse_cull_lru`] pass — **large** so scheduling overhead is
/// amortized over many trivial items (research §4/§5). Tune on a flamegraph.
const SSE_BATCH: usize = 1024;
/// Batch size for the heavy, highly-variable [`decode`] work — **small** so the pool work-steals uneven
/// tile costs rather than stranding a fat tile in one batch (research §4/§5). Tune on a flamegraph.
const DECODE_BATCH: usize = 1;
/// LOD hysteresis dead-band fraction for the streaming selector: a tile refined last frame holds its split
/// until SSE drops below `max_sse / (1 + LOD_HYSTERESIS)`. ~15% keeps a camera dwelling on a threshold from
/// flip-flopping parent↔children frame to frame (anti-pop) without a visible LOD-too-coarse lag. Tunable.
const LOD_HYSTERESIS: f64 = 0.15;
/// Off-view (`Visibility::Offscreen`) tiles refine to `max_sse × CULLED_SSE_FACTOR` — a coarse shell kept
/// resident (shadow casters + camera-turn readiness) at a fraction of the fetch/decode of full detail.
/// Mirrors cesium's `culledScreenSpaceError` (64) vs `maximumScreenSpaceError` (16). Tunable.
/// 2.0 (was 4.0): the coarse shell is only half as coarse, so off-view tiles a camera turn reveals pop in
/// far less jarringly (already near full detail), and any on-screen sliver just beyond the frustum margin is
/// much sharper — directly attacks the "coarse parent at a massive seam" the user reported at view edges.
const CULLED_SSE_FACTOR: f64 = 2.0;
/// Angular slack (radians) added to the view frustum's half-angles for the in-view test, so tiles at the
/// screen edge classify in-view (full detail) instead of flipping to the coarse shell — no LOD seam crawling
/// the screen border, and a buffer against camera rotation. 0.18 rad (≈10°, was 0.12/≈7°): a wider full-detail
/// margin so the sharp/coarse boundary sits comfortably outside the visible frame, not at its very edge.
const FRUSTUM_MARGIN_RAD: f64 = 0.18;

// ---- Systems (few, right-sized) --------------------------------------------------------------------

/// The **fused cheap pass**: distance + SSE + LRU stamp for every live tile, in one coarse-batched
/// `par_iter` (not four tiny systems, not a `par_iter` per op). Free slots are skipped.
fn sse_cull_lru(
    view: Res<GeoView>,
    mut tiles: Query<(
        &TileState,
        &TileBounds,
        &GeometricError,
        &mut Sse,
        &mut CamDistance,
        &mut LastSeen,
    )>,
) {
    let (cam, fov, vh, frame, cull) = (
        view.camera_ecef,
        view.fov_y_radians,
        view.viewport_height.max(1.0),
        view.frame,
        view.cull_distance_m,
    );
    tiles
        .par_iter_mut()
        .batching_strategy(BatchingStrategy::fixed(SSE_BATCH))
        .for_each(|(state, bounds, ge, mut sse, mut dist, mut seen)| {
            if *state == TileState::Free {
                return;
            }
            let d = ((cam - bounds.center).length() - bounds.radius).max(0.0);
            dist.0 = d as f32;
            sse.0 = logi_3dtiles_select::screen_space_error(ge.0, d, fov, vh) as f32;
            // LRU stamp ONLY for in-view tiles (within the cull radius); off-screen tiles keep their old
            // `LastSeen` and become evictable. `cull <= 0` ⇒ no cull, everything is in view.
            if cull <= 0.0 || d <= cull {
                seen.0 = frame;
            }
        });
}

/// The **parallel beneficiary**: decode each just-fetched tile's bytes into geometry, in parallel, with a
/// **small** batch (work-stealing the variable per-tile cost). `Changed<FetchedBytes>` skips tiles whose
/// bytes didn't change since this system last ran (the body is skipped; the scan is cheap — research §9).
fn decode(
    decoder: Res<TileDecoder>,
    new_residents: Res<NewResidents>,
    mut tiles: Query<
        (
            Entity,
            &mut FetchedBytes,
            &TileTransform,
            &mut ResidentMesh,
            &mut TileState,
        ),
        Changed<FetchedBytes>,
    >,
) {
    let decode_fn = &decoder.0;
    // Stage 1: par_iter can't touch the `ResidentSet` resource, so newly-`Resident` entities are collected
    // here (a tiny per-step set — this step's arrivals only) and drained into `ResidentSet` by `evict`.
    let new_residents = &new_residents.0;
    tiles
        .par_iter_mut()
        .batching_strategy(BatchingStrategy::fixed(DECODE_BATCH))
        .for_each(|(e, mut bytes, xf, mut mesh, mut state)| {
            if *state != TileState::Fetched {
                return;
            }
            // Decode in a scope so the borrow of `bytes` ends before we clear it below.
            let decoded = match bytes.0.as_deref() {
                Some(raw) => decode_fn(raw, xf.0),
                None => return,
            };
            if let Some(geom) = decoded {
                mesh.0 = Some(Arc::new(geom));
                *state = TileState::Resident;
                new_residents.lock().unwrap().push(e); // record the Fetched→Resident promotion (Stage 1)
                                                       // The raw GLB is dead once decoded — drop it so a resident tile pins only its mesh, not also
                                                       // its (often larger) source bytes. Skips ~100 KB–1 MB of dead weight per resident tile.
                bytes.0 = None;
            }
            // A decode failure leaves the tile in `Fetched` with its bytes — `reconcile_index` frees the slot
            // and marks it terminal; we never panic across the decode (the decoder returns `Option`).
        });
}

/// **Sequential** LRU eviction: when resident tiles exceed the cap, reset the oldest (by `LastSeen`, not
/// seen this frame) back to `Free`. The decoded mesh is **deposited into [`MeshCache`]** (not dropped) so a
/// camera returning to the tile restores it without re-fetch/-decode; the slot returns to the free-list.
/// **No despawn** (no structural change). Runs after the parallel passes.
// Bevy system params (Res/ResMut/Query) count as fn args to clippy; the +2 over the limit are Stage 1's
// `ResidentSet`/`NewResidents`. Splitting them into a struct param would only obscure the system signature.
#[allow(clippy::too_many_arguments)]
fn evict(
    view: Res<GeoView>,
    cap: Res<MaxResident>,
    mut free: ResMut<FreeList>,
    mut stats: ResMut<GeoStats>,
    mut cache: ResMut<MeshCache>,
    mut resident_set: ResMut<ResidentSet>,
    new_residents: Res<NewResidents>,
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
    // Stage 1: drain the entities `decode` just promoted to `Resident` (par_iter collected them) into the
    // maintained `ResidentSet`, so the count + candidate scan run over the resident WORKING SET (O(resident))
    // instead of the whole pre-spawned pool (O(pool)) every step.
    {
        let mut nr = new_residents.0.lock().unwrap();
        for e in nr.drain(..) {
            resident_set.0.insert(e);
        }
    }
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
    // Over cap → collect eviction candidates (resident AND not stamped this frame), iterating ONLY the
    // resident set. Each carries its camera distance (ECEF m) + score = distance·(1 + FACTOR·staleness) —
    // evict the FARTHEST + STALEST first (distance-primary; `EVICT_STALENESS_FACTOR`), not merely the oldest.
    let cam = view.camera_ecef;
    let mut candidates: Vec<(Entity, u64, f32, f32)> = Vec::new(); // (entity, last_seen, distance_m, score)
    for &e in resident_set.0.iter() {
        if let Ok((_, _, bounds, state, seen, _, _)) = tiles.get(e) {
            if *state == TileState::Resident && seen.0 != view.frame {
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
            resident_set.0.remove(&e); // Stage 1: Resident→Free
            stats.evicted_this_step += 1;
        }
    }
    cache.trim(view.frame); // amortized: only sorts/drops when the batch pushed the cache over budget
                            // Report the resident count AFTER this step's eviction (the live working set).
    stats.resident = resident as u32 - stats.evicted_this_step;
}

/// **Sequential** publish: snapshot the `Resident` tiles in view this step (`LastSeen == frame`) into the
/// render frame the stream thread polls — geometry pinned via `Arc`, keyed by the stable `TileId`. Reuses
/// the mailbox's `Vec` (clear + push) so there's no per-step allocation.
fn publish(
    view: Res<GeoView>,
    enu: Res<GeoEnu>,
    mut mailbox: ResMut<GeoMailbox>,
    resident_set: Res<ResidentSet>,
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
    let frame = &mut mailbox.0;
    frame.frame = view.frame;
    frame.origin_ecef = enu.0.origin_ecef.to_array();
    frame.tiles.clear();
    // Stage 1: iterate the resident working set (O(resident)) not the whole pool (O(pool)); publish those
    // in view this step. (`tiles.get` confirms the live state — the set is the candidate list, not the truth.)
    for &e in resident_set.0.iter() {
        if let Ok((state, id, seen, mesh, extra, depth, ge)) = tiles.get(e) {
            if *state == TileState::Resident && seen.0 == view.frame {
                // `depth`/`geometric_error` ride along for the Geo debug-viz + telemetry tools (white→red
                // quality heatmaps); they don't affect what or how the renderer draws.
                let geometric_error = ge.0 as f32;
                if let Some(geom) = &mesh.0 {
                    frame.tiles.push(GeoRenderTile {
                        tile_key: id.0,
                        geom: Arc::clone(geom),
                        depth: depth.0,
                        geometric_error,
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
                        });
                    }
                }
            }
        }
    }
}

// ---- The Geo World ---------------------------------------------------------------------------------

/// A `bevy_ecs` World of pre-spawned tile entities + the Geo schedule, owned by the stream thread. One
/// [`advance`](Self::advance) runs the schedule once (the heavy systems fan out over the shared
/// `ComputeTaskPool`). The tileset traversal (`select`), fetch drain, and published-frame mailbox wire in
/// next; for now the public surface is enough to drive + test the entity/component/system core.
pub struct GeoWorld {
    world: World,
    schedule: Schedule,
    /// The sequential streaming machinery (tree + fetcher + path→entity index), driven by `drain_raw` +
    /// `select_request` BEFORE the schedule. `None` for the foundation [`new`](Self::new) (schedule-only,
    /// for tests / externally-driven entities); `Some` for [`from_tileset`](Self::from_tileset).
    streaming: Option<GeoStreaming>,
}

/// The growing 3D-Tiles tree (external `.json` tilesets graft into it as the camera descends) + the async
/// fetch source + the path→entity index. Owned by the stream thread; touched only by the sequential
/// `drain_raw`/`select_request` methods (the tree-walk is pointer-chasing — inherently sequential).
struct GeoStreaming {
    base_url: String,
    ellipsoid: logi_geospatial::Ellipsoid,
    root: Tile,
    /// Grafted external subtrees: path → the placeholder's pre-graft state + last-active step ([`GraftRecord`]).
    /// Membership marks "already grafted, don't re-fetch"; the record lets [`GeoWorld::prune_cold_grafts`]
    /// un-graft a cold subtree (the bound on this map + the tree + `bases`, which otherwise only grow).
    grafted: HashMap<TilePath, GraftRecord>,
    /// Per-grafted-subtree base URL (carries the inherited auth query down each external subtree).
    bases: HashMap<TilePath, String>,
    source: Box<dyn TileContentSource + Send>,
    in_flight: HashMap<RequestId, InFlight>,
    in_flight_paths: HashSet<TilePath>,
    next_id: RequestId,
    max_in_flight: usize,
    max_tile_distance: f64,
    /// path → the pool entity backing that tile (in-flight or resident). The reverse of an entity's tile.
    index: HashMap<TilePath, Entity>,
    /// Paths whose payload failed to decode. Decode is deterministic over the fetched bytes, so failure is
    /// terminal: this set makes `select_request` skip them permanently — freeing the pool slot on failure
    /// (see `reconcile_index`) without the re-fetch→re-fail busy-loop that merely clearing `index` would cause.
    decode_failed: HashSet<TilePath>,
    /// Consecutive failed-FETCH count per path (network errors, not decode). Unlike `decode_failed`, a fetch
    /// failure is transient, so the path is freed + re-selected + re-fetched; only once the count reaches
    /// [`MAX_FETCH_ATTEMPTS`] is it treated as terminal (`select_request` skips it). A successful fetch clears
    /// the entry. Keying the terminal state here (rather than in `decode_failed`) keeps the two failure modes —
    /// and their telemetry — honestly separated.
    fetch_attempts: HashMap<TilePath, u32>,
    /// path → the earliest `now_tick` at which a TRANSIENT-failed path may be re-selected (audit A1 backoff).
    /// `select_request` skips a path while `now_tick < next_retry[path]`, so a rate-limited (429/5xx) tile is
    /// retried after a capped exponential delay instead of every step. Cleared on success / give-up / prune.
    next_retry: HashMap<TilePath, u64>,
    /// Decoded `.subtree` files (audit S1 — implicit tiling): resolved subtree URL → its availability
    /// bitstreams. The `SubtreeAvailability` provider answers the implicit traversal from this; a miss
    /// enqueues a fetch. (Grows over a session — subtree entries are tiny; LRU it if it ever matters.)
    subtree_cache: HashMap<String, Subtree>,
    /// FIFO insertion order of `subtree_cache` keys, so it can be evicted oldest-first past
    /// [`MAX_SUBTREE_CACHE`] — bounds the implicit-tiling cache over a long session while keeping the current
    /// view's recently fetched subtrees (which were inserted most recently).
    subtree_cache_order: VecDeque<String>,
    /// In-flight `.subtree` fetches: RequestId → (resolved URL, child_count) so `drain_raw` decodes + caches
    /// the result. Separate from `in_flight` (tiles) — subtrees aren't tiles (no entity / `TilePath`).
    in_flight_subtrees: HashMap<RequestId, (String, u32)>,
    /// Resolved URLs of the in-flight `.subtree` fetches (mirrors `in_flight_subtrees`' values) for O(1) dedup
    /// in `select_request`, instead of an O(in-flight) linear value scan over `in_flight_subtrees`.
    in_flight_subtree_urls: HashSet<String>,
    /// `.subtree` URLs that failed terminally (404 / malformed) — answered `Unavailable` (prune the region)
    /// instead of re-pending forever. Transient fetch failures are simply re-fetched next walk.
    subtree_failed: HashSet<String>,
    /// The **previous** `select_request`'s per-tile [`SelectionResult`] — the cross-frame traversal state fed
    /// back as `select_tree`'s `prev_state`. It drives BOTH the hysteresis dead-band (a tile `Refined` last
    /// frame holds its split, [`LOD_HYSTERESIS`]) AND the anti-pop **kick** (a fresh, not-yet-loaded
    /// refinement renders the coarse ancestor instead of holes, switching to fine atomically once loaded).
    sel_state: SelectionState,
    /// Camera **forward** direction in ECEF (unit; `ZERO` = unknown ⇒ no view-direction weighting). Set per
    /// frame by [`advance_from_ue`](GeoWorld::advance_from_ue); read by `select_request`'s fetch-priority sort
    /// (plan §9.2 step 7) so tiles in the look direction load first. Lives here (not on the per-frame
    /// `GeoView`) because only the streaming selector needs it — the schedule systems cull by distance.
    cam_dir_ecef: DVec3,
    /// The camera view frustum in ECEF, rebuilt each frame by [`advance_from_ue`](GeoWorld::advance_from_ue)
    /// (`Default` = accept-all, for the headless `advance`/`advance_streaming` test path). `select_request`
    /// classifies each tile against it: in-view → full SSE, off-view-but-in-range → coarse `culled` SSE
    /// (frustum-aware LOD). Reduces fetch/decode by streaming a coarse shell for what the camera isn't facing.
    frustum: ViewFrustum,
    /// Set when a tile/subtree fetch returns 401/403 (a Google P3DT **expired session**). Consumed by
    /// [`GeoWorld::refresh_session_if_needed`], which re-fetches the root tileset for a fresh `?session=…`.
    session_refresh_pending: bool,
    /// The advance step of the last session refresh — gates the next one by [`SESSION_REFRESH_COOLDOWN`] so a
    /// 401 storm (every in-flight tile fails at once when the session expires) triggers ONE root re-fetch.
    last_session_refresh_frame: u64,
}

impl GeoStreaming {
    /// Cache a decoded `.subtree`, FIFO-evicting the oldest past [`MAX_SUBTREE_CACHE`] so the implicit-tiling
    /// cache stays bounded over a long fly-through. The current view's subtrees were inserted most recently, so
    /// they survive; only old far-away regions are dropped (re-fetched if the camera revisits them).
    fn cache_subtree(&mut self, url: String, subtree: Subtree) {
        // Only fetched-on-a-miss URLs reach here, so a key is new (insert returns `None`) → record its order.
        if self.subtree_cache.insert(url.clone(), subtree).is_none() {
            self.subtree_cache_order.push_back(url);
        }
        while self.subtree_cache.len() > MAX_SUBTREE_CACHE {
            let Some(old) = self.subtree_cache_order.pop_front() else {
                break;
            };
            self.subtree_cache.remove(&old);
        }
    }

    /// Record a terminally-failed `.subtree` URL, clearing the set wholesale past [`MAX_SUBTREE_FAILED`]
    /// (self-healing — a still-absent subtree simply re-fails and re-adds, paced by the in-flight cap).
    fn mark_subtree_failed(&mut self, url: String) {
        if self.subtree_failed.len() >= MAX_SUBTREE_FAILED {
            self.subtree_failed.clear();
        }
        self.subtree_failed.insert(url);
    }
}

/// One outstanding fetch: enough to route the result back to the tree (tileset graft) or the entity (GLB).
struct InFlight {
    path: TilePath,
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
    /// The implicit root's fetch base + the grafted-subtree bases, to resolve the (relative) subtree template.
    base_url: &'a str,
    bases: &'a HashMap<TilePath, String>,
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
    fn covering(&self, ctx: &ImplicitContext, level: u32, x: u32, y: u32, z: u32) -> Option<Covering> {
        let st_levels = ctx.tiling.subtree_levels.max(1);
        let child_count = ctx.tiling.subdivision_scheme.child_count();
        let root_level = (level / st_levels) * st_levels;
        let down = level - root_level; // levels from the covering subtree's root down to this tile
        let (sx, sy, sz) = (x >> down, y >> down, z >> down); // subtree-root coords at root_level
        let (lx, ly, lz) = (x - (sx << down), y - (sy << down), z - (sz << down)); // local coords in subtree
        let template = ctx.tiling.subtrees.substitute(root_level, sx, sy, sz);
        let base = base_for(self.base_url, self.bases, ctx.root_path);
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
    fn child_subtree_present(&self, ctx: &ImplicitContext, cov: &Covering) -> Option<bool> {
        let st_levels = ctx.tiling.subtree_levels.max(1);
        if cov.root_level < st_levels {
            return None; // the root subtree has no parent to consult — it always "exists"
        }
        let mask = 1u32.checked_shl(st_levels)?.checked_sub(1)?; // st_levels >= 32 ⇒ skip (fetch as before)
        let parent_root_level = cov.root_level - st_levels;
        let (psx, psy, psz) = (cov.sx >> st_levels, cov.sy >> st_levels, cov.sz >> st_levels);
        let (clx, cly, clz) = (cov.sx & mask, cov.sy & mask, cov.sz & mask);
        let parent_template = ctx.tiling.subtrees.substitute(parent_root_level, psx, psy, psz);
        let base = base_for(self.base_url, self.bases, ctx.root_path);
        let parent_url = resolve_url(base, &parent_template).ok()?;
        let parent = self.cache.get(&parent_url)?; // parent not cached ⇒ can't tell ⇒ fetch as before
        Some(parent.is_child_subtree_available(clx, cly, clz))
    }
}

impl AvailabilityProvider for SubtreeAvailability<'_> {
    fn tile_availability(&mut self, ctx: &ImplicitContext, level: u32, x: u32, y: u32, z: u32) -> TileAvailability {
        let Some(cov) = self.covering(ctx, level, x, y, z) else {
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
                if let Some(false) = self.child_subtree_present(ctx, &cov) {
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

    fn content_availability(&mut self, ctx: &ImplicitContext, level: u32, x: u32, y: u32, z: u32) -> bool {
        // Only called for already-`Available` tiles, so the covering subtree is cached. Content index 0 (P3DT
        // tiles carry a single content); a tile may exist as a pure interior node with no content.
        let Some(cov) = self.covering(ctx, level, x, y, z) else {
            return false;
        };
        self.cache
            .get(&cov.url)
            .map(|st| st.is_content_available(0, cov.llevel, cov.lx, cov.ly, cov.lz))
            .unwrap_or(false)
    }
}

/// Worker-thread budget for the shared `ComputeTaskPool` — mirrors `logi_sim::resolve_worker_thread_count`
/// (kept in lockstep here rather than coupling the geo tower to the gameplay tower): clamp to
/// `max(1, logical_cores - 2)` so the pool never grabs all cores and starves Unreal's game/render/RHI
/// threads. Geo and gameplay share ONE global pool via `get_or_init`; both build it BOUNDED, so whichever
/// initializes first, Unreal keeps its headroom.
fn geo_compute_threads() -> usize {
    const UNREAL_RESERVED_THREADS: usize = 2;
    let logical = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
    logical.saturating_sub(UNREAL_RESERVED_THREADS).max(1)
}

impl GeoWorld {
    /// Pre-spawn `pool_size` tile entities (all `Free`, one uniform archetype), build the schedule, and
    /// ensure the **shared**, BOUNDED global `ComputeTaskPool` exists (joins the gameplay pool if it ran first).
    pub fn new(pool_size: usize, enu: EnuFrame, decoder: TileDecoder, max_resident: usize) -> Self {
        // Share the one global ComputeTaskPool (research §10), BOUNDED so geo never oversubscribes Unreal —
        // `get_or_init` joins gameplay's pool if it ran first; both towers build it bounded (no more race to
        // an unbounded pool). Required before the schedule's `decode` `par_iter` runs (multi_threaded feature).
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
        world.insert_resource(MeshCache::new(DEFAULT_MESH_CACHE_BYTES));

        let mut free = Vec::with_capacity(pool_size);
        for _ in 0..pool_size {
            let e = world
                .spawn((
                    TileId(0),
                    TileBounds {
                        center: DVec3::ZERO,
                        radius: 0.0,
                    },
                    GeometricError(0.0),
                    Depth(0),
                    TileTransform(DMat4::IDENTITY),
                    TileState::Free,
                    Sse(0.0),
                    CamDistance(0.0),
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
        world.insert_resource(NewResidents::default());

        let mut schedule = Schedule::default();
        // Cheap fused pass → parallel decode → sequential evict → publish the visible resident set. (The
        // sequential tileset traversal + fetch drain run as `GeoWorld` methods before the schedule — they
        // mutate the tree/io and acquire entities; the schedule is the per-entity + parallel work.)
        schedule.add_systems((sse_cull_lru, decode, evict, publish).chain());

        Self {
            world,
            schedule,
            streaming: None,
        }
    }

    /// Advance one step against `view`: run the schedule once.
    pub fn advance(&mut self, view: GeoView) {
        self.world.insert_resource(view);
        self.schedule.run(&mut self.world);
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
        world.streaming = Some(GeoStreaming {
            base_url,
            ellipsoid,
            root: tileset.root,
            grafted: HashMap::new(),
            bases: HashMap::new(),
            source,
            in_flight: HashMap::new(),
            in_flight_paths: HashSet::new(),
            next_id: 1,
            max_in_flight: 128,
            max_tile_distance,
            index: HashMap::new(),
            decode_failed: HashSet::new(),
            fetch_attempts: HashMap::new(),
            next_retry: HashMap::new(),
            subtree_cache: HashMap::new(),
            subtree_cache_order: VecDeque::new(),
            in_flight_subtrees: HashMap::new(),
            in_flight_subtree_urls: HashSet::new(),
            subtree_failed: HashSet::new(),
            sel_state: SelectionState::new(),
            cam_dir_ecef: DVec3::ZERO,
            frustum: ViewFrustum::default(),
            session_refresh_pending: false,
            last_session_refresh_frame: 0,
        });
        Ok(world)
    }

    /// Resolve a Cesium **ion** asset → fetch its root tileset → build a streaming Geo World on a worker-pool
    /// [`HttpSource`]. **Blocking** (ion resolve + root fetch) — call off the render thread (the FFI does, on
    /// the create path). `cache` is the optional persistent tile cache; `pool_size`/`max_resident` size the
    /// entity pool + resident cap.
    #[allow(clippy::too_many_arguments)]
    pub fn from_ion(
        asset_id: u64,
        token: &str,
        longitude_deg: f64,
        latitude_deg: f64,
        height_m: f64,
        workers: usize,
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
        let source: Box<dyn TileContentSource + Send> = Box::new(HttpSource::new(workers, cache));
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

    /// One **streaming** step (the full pipeline): drain completed fetches → run the tileset traversal
    /// (acquire/request) → run the schedule (parallel decode, evict, publish). The drain + traversal are
    /// sequential (pointer-chasing); the schedule fans out.
    pub fn advance_streaming(&mut self, view: GeoView) {
        self.drain_raw(view.frame);
        self.refresh_session_if_needed(view.frame); // drain may have flagged an expired P3DT session
        self.select_request(view);
        self.advance(view);
        self.reconcile_index();
        self.prune_cold_grafts(view.frame);
        self.bound_failure_sets();
    }

    /// Re-fetch the root tileset for a FRESH Google P3DT `?session=…` when [`fail_fetch`](Self::fail_fetch)
    /// flagged an expired session (a 401/403). The session the root minted has a TTL; once it lapses, every new
    /// tile/subtree fetch 401s and (without this) dies permanently, freezing the planet at the coarse LOD it
    /// reached while the session was valid. Re-fetching the root (its URL still carries the long-lived `?key=`)
    /// mints a new session baked into fresh content/subtree templates; resetting the session-tainted streaming
    /// state then makes the traversal re-descend + re-fetch everything not yet resident with the new session.
    /// Blocking (one `http_get` on the stream thread) but rare and cooldown-gated; resident tiles keep rendering
    /// throughout (their geometry is session-independent and the entity pool/index are preserved).
    fn refresh_session_if_needed(&mut self, frame: u64) {
        let (pending, last, base_url) = match self.streaming.as_ref() {
            Some(s) => (
                s.session_refresh_pending,
                s.last_session_refresh_frame,
                s.base_url.clone(),
            ),
            None => return,
        };
        // Cooldown: a session expiry 401s every in-flight tile at once; collapse that storm into ONE refresh.
        if !pending || (last != 0 && frame.saturating_sub(last) < SESSION_REFRESH_COOLDOWN) {
            return;
        }
        // Re-fetch + re-parse the root. On failure, leave `pending` set (retry next eligible frame) but stamp
        // `last` so the cooldown still throttles a persistently-failing refresh (e.g. a genuinely bad key).
        let fresh_root = http_get(&base_url).ok().and_then(|bytes| parse_tileset(&bytes).ok());
        let s = self.streaming.as_mut().unwrap();
        s.last_session_refresh_frame = frame;
        let Some(tileset) = fresh_root else {
            return; // network/parse failure — keep `pending`, try again after the cooldown
        };
        s.root = tileset.root; // fresh content/subtree templates carry the fresh session
                               // Reset every session-tainted bit so the traversal re-descends with the new session. URL-keyed caches
                               // (subtree_cache/_failed/_urls) hold stale-session keys; the path-keyed failure sets are cleared so tiles
                               // killed under the old session get a fresh chance. `index` + the entity pool + in-flight fetches are kept
                               // (resident tiles keep rendering; old in-flight results route by id and are harmless).
        s.subtree_cache.clear();
        s.subtree_cache_order.clear();
        s.subtree_failed.clear();
        s.in_flight_subtree_urls.clear();
        s.decode_failed.clear();
        s.fetch_attempts.clear();
        s.next_retry.clear();
        s.grafted.clear();
        s.bases.clear();
        s.session_refresh_pending = false;
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
        if s.decode_failed.len() > MAX_DEAD_PATHS {
            s.decode_failed.clear();
        }
        if s.fetch_attempts.len() > MAX_DEAD_PATHS {
            s.fetch_attempts.clear();
            s.next_retry.clear();
        }
    }

    /// Drain completed fetches: graft external tilesets into the tree, or hand a GLB's bytes to its entity
    /// (`FetchedBytes` + `Fetched`) — **no decode here**; the parallel `decode` system does it. `now_tick`
    /// is the monotonic step counter the source's completion schedule uses (ignored by the real HTTP source,
    /// which drains its channel; used by the deterministic `SyntheticSource` in tests).
    fn drain_raw(&mut self, now_tick: u64) {
        let mut done = Vec::new();
        match self.streaming.as_mut() {
            Some(s) => s.source.poll_completed(now_tick, &mut done),
            None => return,
        }
        for loaded in done {
            // Subtree lane (audit S1): a fetched `.subtree` → decode + cache so the implicit traversal can
            // descend below its implicit root next pass. Routed by RequestId before the tile lane (ids are
            // unique across both). Malformed / 404 → terminal (`subtree_failed`) so we don't re-pend forever;
            // a transient failure is left uncached (re-pended next walk, paced by the fetch round-trip).
            {
                let s = self.streaming.as_mut().unwrap();
                if let Some((url, child_count)) = s.in_flight_subtrees.remove(&loaded.id) {
                    s.in_flight_subtree_urls.remove(&url);
                    match loaded.result {
                        Ok(bytes) => match parse_subtree(&bytes, child_count) {
                            Ok(subtree) => s.cache_subtree(url, subtree),
                            Err(_) => s.mark_subtree_failed(url),
                        },
                        Err(e) if !e.is_transient() => s.mark_subtree_failed(url),
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
                // Only the primary content (`content_index == 0`) owns the path's in-flight bookkeeping; a 1.1
                // additional content shares the path and must not clear it out from under the primary.
                if inflight.content_index == 0 {
                    s.in_flight_paths.remove(&inflight.path);
                }
                inflight
            };
            // Typed outcome (audit A1/A10): an `Err` carries the failure CLASS (404 terminal vs 429/5xx/
            // transport transient) so `fail_fetch` can abandon at once or back off + retry; an `Ok` — even an
            // empty body — is a success. The old code collapsed every outcome to "empty bytes ⇒ failure".
            let bytes = match loaded.result {
                Ok(b) => b,
                Err(e) => {
                    self.fail_fetch(inflight, e, now_tick);
                    continue;
                }
            };
            // Fetch succeeded → clear any prior consecutive-failure count + retry backoff for this path (the
            // primary owns that per-path state; an extra's success must not reset the primary's failure count).
            if inflight.content_index == 0 {
                let s = self.streaming.as_mut().unwrap();
                s.fetch_attempts.remove(&inflight.path);
                s.next_retry.remove(&inflight.path);
            }
            // A 1.1 additional content (`content_index > 0`) rides the primary's entity and takes the inline
            // decode path — it never grafts (an extra is renderable GLB, not a nested tileset) and never touches
            // the primary slot's `FetchedBytes`/state. Route it here, before the primary's graft/GLB dispatch.
            if inflight.content_index > 0 {
                if let Some(e) = inflight.entity {
                    self.store_extra_content(e, &inflight.path, inflight.content_index, &bytes);
                }
                continue;
            }
            let kind = sniff_content_kind(&bytes).unwrap_or(inflight.kind);
            match kind {
                ContentKind::Tileset => {
                    let grafted_ok = {
                        let s = self.streaming.as_mut().unwrap();
                        let ok = graft_external(&mut s.root, &mut s.grafted, &inflight.path, &bytes, now_tick).is_ok();
                        if ok {
                            // Descendants inherit this subtree's fetch URL (carries the P3DT `?session=…`).
                            s.bases.insert(inflight.path, inflight.url);
                        }
                        ok
                    };
                    if !grafted_ok {
                        // The manifest fetched but didn't graft (parse error / target gone): its subtree never
                        // appears, so that region is stuck coarse. Count it (was silent) — see `graft_failed_total`.
                        self.world.resource_mut::<GeoStats>().graft_failed_total += 1;
                    }
                }
                ContentKind::Glb => {
                    if let Some(e) = inflight.entity {
                        let arc_bytes: Arc<[u8]> = Arc::from(bytes.into_boxed_slice());
                        let mut ent = self.world.entity_mut(e);
                        ent.get_mut::<FetchedBytes>().unwrap().0 = Some(arc_bytes);
                        *ent.get_mut::<TileState>().unwrap() = TileState::Fetched;
                    }
                }
            }
        }
    }

    /// 3D Tiles 1.1 multiple-contents: a tile's ADDITIONAL content (`content_index > 0`) has landed — decode it
    /// **inline** (off the parallel `decode` system — this is the rare path, not the per-frame P3DT hot loop) and
    /// park the mesh in the entity's [`MultiContent`] part. Guards against a recycled slot: if the entity has been
    /// re-tasked to another tile since this extra was requested (its `TileId` no longer matches the primary key),
    /// the stale result is dropped. A decode failure / missing part simply leaves the content undrawn.
    fn store_extra_content(&mut self, e: Entity, path: &[u32], content_index: u32, bytes: &[u8]) {
        // Stale-slot guard: the entity must still back this very tile (primary key == hash of `path`).
        let primary_key = tile_key(path);
        if self.world.get::<TileId>(e).map(|t| t.0) != Some(primary_key) {
            return;
        }
        let key = content_key(path, content_index);
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

    /// Handle a failed fetch, CLASSIFIED by the source (audit A1). Always reclaims the reserved GLB slot, then
    /// decides the path's fate by failure class:
    /// * **`Cancelled`** — the owner already moved on (the tile left the view; see `cancel_stale`): free the
    ///   slot, don't count it, don't bump attempts. Nothing to retry.
    /// * **Terminal** (`!is_transient`: 404/403/400, bad URL, over-cap body): give up IMMEDIATELY — mark the
    ///   path at the attempt cap so `select_request` skips it permanently, and count it once. Retrying a tile
    ///   the server says doesn't exist only wastes bandwidth + a worker slot (the old code burned all 4
    ///   attempts on a dead 404).
    /// * **Transient** (429/5xx/transport): re-selectable, but on a BACKOFF — set `next_retry` so the path is
    ///   not re-fetched until `now_tick + delay` (capped exponential by attempt + per-path jitter, floored by
    ///   the server's `Retry-After`). After [`MAX_FETCH_ATTEMPTS`] consecutive transient misses it too becomes
    ///   terminal.
    ///
    /// The old code treated every failure as a no-delay transient retry, so a wave of 429s became a sub-ms
    /// retry storm that deepened the throttling AND falsely abandoned merely-rate-limited tiles after 4 fast
    /// failures. (It still reclaims the slot the pre-A1 fix added — never strands the entity as `Requested`.)
    fn fail_fetch(&mut self, inflight: InFlight, err: FetchError, now_tick: u64) {
        // A 1.1 ADDITIONAL content (`content_index > 0`) rides the primary's slot and the primary owns the
        // path's retry state — so a failed extra frees nothing and touches no per-path counters. It just doesn't
        // draw (its `MultiContent` part keeps `mesh = None`); the tile still renders content[0] and the rest.
        if inflight.content_index > 0 {
            return;
        }
        // Return the reserved GLB slot to the pool (a tileset fetch has no entity, so nothing to free here).
        if let Some(e) = inflight.entity {
            *self.world.get_mut::<TileState>(e).unwrap() = TileState::Free;
            self.world.get_mut::<FetchedBytes>(e).unwrap().0 = None;
            self.world.get_mut::<ResidentMesh>(e).unwrap().0 = None;
            self.world.get_mut::<MultiContent>(e).unwrap().0.clear();
            self.world.resource_mut::<FreeList>().0.push(e);
        }
        // A cancel is not a failure: the slot is freed above; leave attempts/backoff untouched and don't count.
        if matches!(err, FetchError::Cancelled) {
            return;
        }
        let path_key = tile_key(&inflight.path);
        let retry_after_ticks = err
            .retry_after()
            .map(|d| {
                d.as_secs()
                    .saturating_mul(APPROX_TICKS_PER_SEC)
                    .min(RETRY_AFTER_MAX_TICKS)
            })
            .unwrap_or(0);
        // A 401/403 on a P3DT tile is almost always an EXPIRED SESSION (the `?session=…` the root minted has a
        // TTL), NOT a permanently-dead URL. Treat it as RECOVERABLE: flag a session refresh (re-fetch the root
        // for a fresh session — `refresh_session_if_needed`) and merely BACK OFF this path instead of marking it
        // terminal. Without this, every fetch issued after the TTL dies forever → the planet freezes at the
        // coarse LOD it reached while the session was valid (the "tiles stuck / failing to load" symptom).
        let auth_expired = matches!(err, FetchError::Http { status, .. } if status == 401 || status == 403);
        let gave_up = {
            let s = self.streaming.as_mut().unwrap();
            // Drop the stale index entry so the path is re-selectable (the entity it named is now Free).
            s.index.remove(&inflight.path);
            if auth_expired {
                s.session_refresh_pending = true;
                // Brief backoff so this path doesn't re-spam the still-stale session before the refresh lands.
                let delay = RETRY_BACKOFF_BASE_TICKS.max(retry_after_ticks);
                s.next_retry
                    .insert(inflight.path.clone(), now_tick.saturating_add(delay));
                false // recoverable once the session refreshes — not a give-up
            } else if err.is_transient() {
                let n = s.fetch_attempts.entry(inflight.path.clone()).or_insert(0);
                *n += 1;
                if *n >= MAX_FETCH_ATTEMPTS {
                    // Out of retries → terminal: the attempt cap (≥ MAX) makes `select_request` skip it; clear
                    // the pending backoff so the terminal marker, not the timer, governs.
                    s.next_retry.remove(&inflight.path);
                    true
                } else {
                    // Back off before the next attempt: capped exponential by attempt count + a per-path jitter
                    // (de-synchronizes a herd that failed together), floored by the server's `Retry-After`.
                    let shift = (*n - 1).min(6);
                    let exp = RETRY_BACKOFF_BASE_TICKS.saturating_mul(1u64 << shift);
                    let jitter = path_key % RETRY_BACKOFF_BASE_TICKS;
                    let delay = exp.max(retry_after_ticks).saturating_add(jitter);
                    s.next_retry
                        .insert(inflight.path.clone(), now_tick.saturating_add(delay));
                    false
                }
            } else {
                // Terminal (4xx≠429 / bad URL / over-cap): give up at once. Mark at the cap so it's skipped
                // permanently; no backoff timer needed.
                s.fetch_attempts.insert(inflight.path.clone(), MAX_FETCH_ATTEMPTS);
                s.next_retry.remove(&inflight.path);
                true
            }
        };
        if gave_up {
            self.world.resource_mut::<GeoStats>().fetch_failed_total += 1;
        }
    }

    /// Run the SSE traversal over the (growing) tree: stamp visible resident tiles into this frame's working
    /// set, and acquire + request a fetch for each selected-but-unloaded tile. Reuses `select_tree`.
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
            let s = self.streaming.as_ref().unwrap();
            // `is_loaded` probes the path→entity index + the world directly (both immutable borrows, so they
            // coexist with the walk's `&s.root`). The old per-frame "resident set" rebuild — iterate the whole
            // index, clone every resident path into a `HashSet` — was a pure allocation tax each frame (§1E).
            let world = &self.world;
            let params = SelectParams {
                camera_position: cam,
                fov_y_radians: view.fov_y_radians,
                viewport_height: view.viewport_height.max(1.0),
                max_screen_space_error: view.max_sse,
                culled_screen_space_error: view.max_sse * CULLED_SSE_FACTOR,
                hysteresis: LOD_HYSTERESIS,
            };
            let max_d = s.max_tile_distance;
            let frustum = s.frustum; // Copy out so the visibility closure borrows neither `s` nor `self`.
                                     // Implicit tiling (audit S1): answer the traversal's subtree-availability queries from the decoded
                                     // cache; a miss is recorded in `needed_subtrees` (fetched below) and answered `Pending`, so the
                                     // walk holds the coarse ancestor until the `.subtree` arrives. Explicit tilesets never invoke it.
            let mut provider = SubtreeAvailability {
                cache: &s.subtree_cache,
                failed: &s.subtree_failed,
                base_url: &s.base_url,
                bases: &s.bases,
                needed: &mut needed_subtrees,
            };
            select_tree_implicit(
                &s.root,
                &s.ellipsoid,
                &params,
                &mut |p| {
                    s.index
                        .get(p)
                        .is_some_and(|&e| world.get::<TileState>(e) == Some(&TileState::Resident))
                },
                // is_dead (R1): content decode gave up, or the fetch hit the retry cap. `select_request` already
                // PERMANENTLY skips these, so without this signal the traversal kept treating them as "still
                // loading" → a dead fine tile pinned its REPLACE parent as a coarse stand-in forever, blurring
                // the whole region over its loaded siblings. Treating it as ABSENT lets the siblings refine sharp
                // (the dead tile's footprint falls back to coarse only if its WHOLE sibling set is dead).
                &mut |p| {
                    s.decode_failed.contains(p) || s.fetch_attempts.get(p).is_some_and(|&n| n >= MAX_FETCH_ATTEMPTS)
                },
                &mut |c, r| {
                    // Distance hard-culls (the far plane). Off-frustum tiles are NOT hard-culled by direction —
                    // they're kept `Offscreen` (coarse SSE) so the engine can still cast their shadows from its
                    // larger-than-view shadow frustum, and a camera turn finds them already coarse-loaded.
                    if (cam - c).length() - r >= max_d {
                        Visibility::Culled
                    } else if frustum.contains_sphere(c, r) {
                        Visibility::Visible
                    } else {
                        Visibility::Offscreen
                    }
                },
                &s.sel_state,
                &mut provider,
            )
        };
        // Enqueue the `.subtree` files the walk needs (they gate refinement, so fetch them ahead of tile
        // content). Deduped vs in-flight + terminally-failed, bounded by the same in-flight cap as tiles.
        if !needed_subtrees.is_empty() {
            let now = view.frame;
            let s = self.streaming.as_mut().unwrap();
            for (url, child_count) in needed_subtrees {
                if s.source.in_flight() >= s.max_in_flight {
                    break;
                }
                if s.subtree_failed.contains(&url) || s.in_flight_subtree_urls.contains(&url) {
                    continue; // already terminally failed, or a fetch is already in flight (O(1) dedup)
                }
                let id = s.next_id;
                s.next_id += 1;
                s.source.request(id, &url, now);
                s.in_flight_subtree_urls.insert(url.clone());
                s.in_flight_subtrees.insert(id, (url, child_count));
            }
        }
        // Stamp visible (render) tiles so evict spares them + publish includes them.
        for rt in &sel.render {
            let e = self.streaming.as_ref().unwrap().index.get(&rt.path).copied();
            if let Some(e) = e {
                if let Some(mut ls) = self.world.get_mut::<LastSeen>(e) {
                    ls.0 = view.frame;
                }
            }
        }
        // Keep each rendered/loading tile's grafted ancestors "warm" so `prune_cold_grafts` only reclaims
        // subtrees the camera has truly left — never one with a live descendant this frame. Runs before
        // `sel.load` is consumed below; O(depth) per tile, no allocation.
        {
            let frame = view.frame;
            let s = self.streaming.as_mut().unwrap();
            for t in sel.render.iter().chain(sel.load.iter()) {
                bump_graft_activity(&mut s.grafted, &t.path, frame);
            }
        }
        // Preemptive cancel: any in-flight fetch whose tile is no longer selected (the camera moved on before
        // it arrived) is wasted bandwidth — cancel it and reclaim its slot, freeing in-flight budget for the
        // tiles we DO want now. `wanted` borrows `sel.load`, so scope it before we sort `sel.load` below.
        {
            let wanted: HashSet<&[u32]> = sel.load.iter().map(|t| t.path.as_slice()).collect();
            self.cancel_stale(&wanted);
        }
        // Priority: when the in-flight budget saturates, fetch the tiles that matter most first. The score
        // blends three terms (plan §9.2 step 7): screen importance `geometric_error / distance` (closer /
        // coarser-fallback tiles first — coarse stand-ins lead their own fine detail), times a view-direction
        // weight so a tile in the look direction outranks one of equal importance off to the side or behind.
        // `cam_dir == ZERO` (no direction known, e.g. the headless test path) makes the weight uniform → pure
        // screen-importance order. Sorted **descending** (highest score first).
        //
        // Decorate-sort: each tile's `fetch_priority` (a normalize + dot + div) is evaluated ONCE here, not
        // twice per comparison inside the comparator — O(n) score evals instead of O(n·log n) (§1E). The
        // f64 key is precomputed and finite (importance is finite, the weight ∈ [0.1, 1.1]), so the
        // comparator is a bare `partial_cmp` with a NaN-safe fallback.
        let cam_pos = view.camera_ecef;
        let cam_dir = self.streaming.as_ref().unwrap().cam_dir_ecef;
        let mut ranked: Vec<(f64, SelectedTile)> = std::mem::take(&mut sel.load)
            .into_iter()
            .map(|t| (fetch_priority(&t, cam_pos, cam_dir), t))
            .collect();
        ranked.sort_unstable_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        // For each selected-but-unloaded tile: skip if already accounted for, else try a warm restore from
        // the decoded-mesh cache, else (cold) enqueue a fetch.
        for (_, lt) in &ranked {
            // Already known / in-flight / grafted / terminally-bad → nothing to do.
            {
                let s = self.streaming.as_ref().unwrap();
                if s.index.contains_key(&lt.path)
                    || s.grafted.contains_key(&lt.path)
                    || s.in_flight_paths.contains(&lt.path)
                    || s.decode_failed.contains(&lt.path)
                    || s.fetch_attempts.get(&lt.path).is_some_and(|&n| n >= MAX_FETCH_ATTEMPTS)
                    // Transient-failure backoff (audit A1): not yet time to retry this rate-limited/5xx path.
                    || s.next_retry.get(&lt.path).is_some_and(|&t| view.frame < t)
                {
                    continue;
                }
            }
            // Warm re-entry: the decoded mesh is still cached → restore it directly (no fetch, no decode).
            // This does NOT consume an in-flight fetch slot, so it's tried *before* the outstanding-fetch cap.
            let key = tile_key(&lt.path);
            if self.world.resource::<MeshCache>().contains_key(key) {
                self.restore_cached(lt, key, view.frame);
                continue;
            }
            // Cold: resolve the content URL and enqueue a fetch (subject to the outstanding-fetch cap).
            {
                let s = self.streaming.as_ref().unwrap();
                if s.source.in_flight() >= s.max_in_flight {
                    break; // outstanding-fetch cap reached this step
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
                resolve_url(base_for(&s.base_url, &s.bases, &lt.path), uri)
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
            if let Some(e) = self.request_one(lt, kind, full) {
                if lt.content_uris.len() > 1 {
                    self.request_extra(e, lt);
                }
            }
        }
        // Carry this pass's per-tile results forward as next frame's `prev_state` (drives the kick + the
        // hysteresis dead-band). Move it in — the prior state map is dropped.
        self.streaming.as_mut().unwrap().sel_state = std::mem::take(&mut sel.state);
    }

    /// Warm re-entry: restore a decoded mesh straight from [`MeshCache`] onto a fresh pool slot, skipping
    /// both the fetch and the decode. Caller guarantees `key` is present in the cache and `lt.path` is not
    /// already indexed. A no-op (mesh left cached) when the pool is momentarily exhausted — back-pressure that
    /// resolves as eviction frees slots. The restored tile is `Resident`, stamped `LastSeen = frame`, so it
    /// is spared by `evict` and snapshotted by `publish` this same step.
    fn restore_cached(&mut self, lt: &SelectedTile, key: u64, frame: u64) {
        // Need a free slot *and* the cached mesh. Secure the slot FIRST (pop, not check-then-pop), so the pop
        // can never race a concurrent drain into an `unwrap` panic; if none is free, leave the mesh cached and
        // retry later. Then take the mesh — and on a (shouldn't-happen) cache miss, return the slot to the pool
        // so a popped-but-unused entity is never leaked out of the free list.
        let Some(e) = self.world.resource_mut::<FreeList>().0.pop() else {
            return;
        };
        let Some(geom) = self.world.resource_mut::<MeshCache>().take(key) else {
            self.world.resource_mut::<FreeList>().0.push(e);
            return;
        };
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
            // `FetchedBytes` is already `None` on every free slot (decode clears it; evict/reclaim never set
            // it), so leave it untouched — avoids a spurious `Changed<FetchedBytes>` that `decode` would scan.
        }
        self.streaming.as_mut().unwrap().index.insert(lt.path.clone(), e);
        self.world.resource_mut::<ResidentSet>().0.insert(e); // Stage 1: Free→Resident (warm restore)
        self.world.resource_mut::<GeoStats>().mesh_cache_hits_total += 1;
        // The warm cache holds only content[0]; a multiple-contents tile must re-fetch its additional contents
        // so they render again alongside the restored primary (no-op for single-content tiles).
        if lt.content_uris.len() > 1 {
            self.request_extra(e, lt);
        }
    }

    /// Pre-emptively cancel in-flight fetches whose tile is no longer in this frame's selection — the
    /// fast-motion case where the camera leaves a region before its tiles arrive. `wanted` is the set of
    /// still-selected unloaded paths (`sel.load`); any in-flight path outside it is stale: cancel the fetch,
    /// drop its `in_flight`/`index` bookkeeping, and return its reserved GLB slot to the pool. This frees
    /// in-flight budget for the tiles we actually want now (paired with the priority sort in the caller).
    fn cancel_stale(&mut self, wanted: &HashSet<&[u32]>) {
        // Phase 1 — collect the stale fetches (immutable borrow; `wanted` borrows the caller's `sel.load`).
        // `content_index` distinguishes the primary (owns the slot + the path's bookkeeping) from a 1.1
        // additional content riding the same entity — so phase 3 frees the slot exactly once (no double-free).
        let stale: Vec<(RequestId, TilePath, Option<Entity>, u32)> = {
            let s = self.streaming.as_ref().unwrap();
            s.in_flight
                .iter()
                .filter(|(_, inf)| !wanted.contains(inf.path.as_slice()))
                .map(|(&id, inf)| (id, inf.path.clone(), inf.entity, inf.content_index))
                .collect()
        };
        if stale.is_empty() {
            return;
        }
        // Phase 2 — cancel each fetch + drop its streaming bookkeeping (one `streaming` borrow). Only the
        // primary touches `index`/`in_flight_paths` (extras never inserted there); guarding keeps it honest.
        {
            let s = self.streaming.as_mut().unwrap();
            for (id, path, _, content_index) in &stale {
                s.source.cancel(*id);
                s.in_flight.remove(id);
                if *content_index == 0 {
                    s.in_flight_paths.remove(path);
                    s.index.remove(path);
                }
            }
        }
        // Phase 3 — return each reserved GLB slot to the pool ONCE, via its primary content only (a tileset
        // fetch has no entity; an extra rides the primary's slot and must not free it again).
        for (_, _, entity, content_index) in &stale {
            if let (Some(e), 0) = (entity, content_index) {
                *self.world.get_mut::<TileState>(*e).unwrap() = TileState::Free;
                self.world.get_mut::<FetchedBytes>(*e).unwrap().0 = None;
                self.world.get_mut::<ResidentMesh>(*e).unwrap().0 = None;
                self.world.get_mut::<MultiContent>(*e).unwrap().0.clear();
                self.world.resource_mut::<FreeList>().0.push(*e);
            }
        }
        self.world.resource_mut::<GeoStats>().fetches_cancelled_total += stale.len() as u32;
    }

    /// Acquire a pool entity for a GLB tile (none for an external tileset — it grafts) and enqueue its fetch
    /// for content `[0]`. Returns the reserved entity (so the caller can attach this tile's ADDITIONAL contents
    /// via [`request_extra`](Self::request_extra)), or `None` for a tileset graft / when the pool is exhausted.
    fn request_one(&mut self, lt: &SelectedTile, kind: ContentKind, full: String) -> Option<Entity> {
        let entity = if kind == ContentKind::Glb {
            let Some(e) = self.world.resource_mut::<FreeList>().0.pop() else {
                return None; // pool exhausted — skip (grows at a step boundary later)
            };
            let key = tile_key(&lt.path);
            let mut ent = self.world.entity_mut(e);
            *ent.get_mut::<TileId>().unwrap() = TileId(key);
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
        let s = self.streaming.as_mut().unwrap();
        let id = s.next_id;
        s.next_id += 1;
        s.source.request(id, &full, 0);
        if let Some(e) = entity {
            s.index.insert(lt.path.clone(), e);
        }
        s.in_flight_paths.insert(lt.path.clone());
        s.in_flight.insert(
            id,
            InFlight {
                path: lt.path.clone(),
                kind,
                url: full,
                entity,
                content_index: 0,
            },
        );
        entity
    }

    /// 3D Tiles 1.1 multiple-contents: enqueue the tile's ADDITIONAL contents (`content_uris[1..]`) onto the
    /// SAME entity `e` reserved by [`request_one`](Self::request_one). Each gets a [`MultiContent`] part keyed by
    /// [`content_key`] (filled when the fetch lands, in [`drain_raw`](Self::drain_raw)) and an in-flight fetch
    /// tagged with its `content_index` (so the result routes to the part, not the primary slot). External-tileset
    /// extras are not supported (a tile's extra contents are renderable GLB content, not nested tilesets) and are
    /// skipped. Respects the outstanding-fetch cap — any extra not issued this step is simply re-tried next pass
    /// (the primary already gates refinement; a late extra just pops in). No-op for single-content tiles.
    fn request_extra(&mut self, e: Entity, lt: &SelectedTile) {
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
                resolve_url(base_for(&s.base_url, &s.bases, &lt.path), &uri)
            };
            let Ok(full) = resolved else {
                self.world.resource_mut::<GeoStats>().url_resolve_failed_total += 1;
                continue;
            };
            // Park an (unfilled) part for this content so `publish` knows it exists; the mesh lands on arrival.
            let key = content_key(&lt.path, content_index);
            if let Some(mut mc) = self.world.get_mut::<MultiContent>(e) {
                if mc.0.iter().all(|p| p.key != key) {
                    mc.0.push(ExtraContent { key, mesh: None });
                }
            }
            let s = self.streaming.as_mut().unwrap();
            let id = s.next_id;
            s.next_id += 1;
            s.source.request(id, &full, 0);
            // NB: extras do NOT touch `index`/`in_flight_paths`/retry state — the primary (content[0]) owns the
            // path's bookkeeping; an extra only routes its bytes back to the entity it shares.
            s.in_flight.insert(
                id,
                InFlight {
                    path: lt.path.clone(),
                    kind: ContentKind::Glb,
                    url: full,
                    entity: Some(e),
                    content_index,
                },
            );
        }
    }

    /// Reconcile the path→entity `index` with the post-schedule entity states — the one place a tile leaves
    /// the index. The `evict`/`decode` systems mutate entity *state* but can't touch `streaming` (they're
    /// `bevy_ecs` systems), so a single O(index) scan here (after [`advance`], off the game thread) settles
    /// both exits an entry can take:
    ///
    /// - **Evicted** (now `Free`): `evict` deposited the mesh into [`MeshCache`] and freed the slot; drop the
    ///   stale `index` entry so the traversal can re-select the path — a **warm restore** from the cache
    ///   ([`restore_cached`](Self::restore_cached)) or, if it aged out of the cache, a re-fetch. Without this,
    ///   `select_request`'s `index.contains_key` skip would keep an evicted tile out forever (the re-entry
    ///   bug the mesh cache would otherwise be unable to serve).
    /// - **Decode-failed** (still `Fetched` after `decode` ran on its `Changed` bytes): decode is pure over
    ///   the fetched bytes, so failure is **terminal** — drop the entry, record the path in `decode_failed`
    ///   (never re-requested → no re-fetch→re-fail busy-loop), and free the slot.
    ///
    /// Live entries (`Selected`/`Requested`/`Resident`) stay. Real work happens only on the (rare) frames
    /// with evictions or failures; otherwise it's a single cheap scan.
    fn reconcile_index(&mut self) {
        if self.streaming.is_none() {
            return;
        }
        // Phase 1 — classify index entries by entity state (disjoint immutable borrows of `index` + `world`).
        let mut evicted: Vec<TilePath> = Vec::new();
        let mut failed: Vec<(TilePath, Entity)> = Vec::new();
        {
            let s = self.streaming.as_ref().unwrap();
            for (p, &e) in &s.index {
                match self.world.get::<TileState>(e) {
                    Some(TileState::Free) => evicted.push(p.clone()),
                    Some(TileState::Fetched) => failed.push((p.clone(), e)),
                    _ => {} // Selected / Requested / Resident → live, keep it
                }
            }
        }
        if evicted.is_empty() && failed.is_empty() {
            return;
        }
        // Phase 2 — update `streaming`: drop both classes' entries; mark the failures terminally bad.
        {
            let s = self.streaming.as_mut().unwrap();
            for p in &evicted {
                s.index.remove(p);
            }
            for (p, _) in &failed {
                s.index.remove(p);
                s.decode_failed.insert(p.clone());
            }
        }
        // Phase 3 — free the decode-failed slots (evicted slots were already freed by `evict`).
        for (_, e) in &failed {
            *self.world.get_mut::<TileState>(*e).unwrap() = TileState::Free;
            self.world.get_mut::<FetchedBytes>(*e).unwrap().0 = None;
            self.world.get_mut::<ResidentMesh>(*e).unwrap().0 = None;
            self.world.get_mut::<MultiContent>(*e).unwrap().0.clear();
            self.world.resource_mut::<FreeList>().0.push(*e);
        }
        if !failed.is_empty() {
            self.world.resource_mut::<GeoStats>().decode_failed_total += failed.len() as u32;
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
        // tree only at the shallowest cold path of each cold subtree (restoring it drops the rest wholesale).
        let cold_roots: Vec<TilePath> = {
            let s = self.streaming.as_ref().unwrap();
            let mut cold: Vec<TilePath> = s
                .grafted
                .iter()
                .filter(|(_, rec)| frame.saturating_sub(rec.last_active) > GRAFT_COLD_FRAMES)
                .map(|(p, _)| p.clone())
                .collect();
            if cold.is_empty() {
                return;
            }
            cold.sort_unstable_by_key(|p| p.len());
            let mut roots: Vec<TilePath> = Vec::new();
            for p in cold {
                if !roots.iter().any(|r| path_has_prefix(&p, r)) {
                    roots.push(p);
                }
            }
            roots
        };
        let under = |p: &[u32]| cold_roots.iter().any(|r| path_has_prefix(p, r));
        // Phase 2 — gather each cold root's saved placeholder (immutable), then restore it into the tree
        // (mutable). Restoring `children` drops the grafted subtree's nodes, including any nested grafts'.
        let restores: Vec<GraftRestore> = {
            let s = self.streaming.as_ref().unwrap();
            cold_roots
                .iter()
                .filter_map(|rp| {
                    s.grafted.get(rp).map(|rec| {
                        (
                            rp.clone(),
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
        let stale_inflight: Vec<(RequestId, TilePath)> = {
            let s = self.streaming.as_ref().unwrap();
            s.in_flight
                .iter()
                .filter(|(_, inf)| under(&inf.path))
                .map(|(&id, inf)| (id, inf.path.clone()))
                .collect()
        };
        let mut to_free: HashSet<Entity> = {
            let s = self.streaming.as_ref().unwrap();
            s.index.iter().filter(|(p, _)| under(p)).map(|(_, &e)| e).collect()
        };
        {
            let s = self.streaming.as_ref().unwrap();
            for inf in s.in_flight.values().filter(|inf| under(&inf.path)) {
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
            for (id, path) in &stale_inflight {
                s.source.cancel(*id);
                s.in_flight.remove(id);
                s.in_flight_paths.remove(path);
            }
            s.index.retain(|p, _| !under(p));
            s.grafted.retain(|p, _| !under(p)); // removes the cold roots + any nested grafts
            s.bases.retain(|p, _| !under(p));
            s.decode_failed.retain(|p| !under(p));
            s.fetch_attempts.retain(|p, _| !under(p));
            s.next_retry.retain(|p, _| !under(p));
        }
        // Phase 5 — return every freed pool slot to the free-list (reset fields, never despawn).
        for e in to_free {
            if let Some(mut st) = self.world.get_mut::<TileState>(e) {
                *st = TileState::Free;
            }
            if let Some(mut fb) = self.world.get_mut::<FetchedBytes>(e) {
                fb.0 = None;
            }
            if let Some(mut rm) = self.world.get_mut::<ResidentMesh>(e) {
                rm.0 = None;
            }
            if let Some(mut mc) = self.world.get_mut::<MultiContent>(e) {
                mc.0.clear(); // 1.1 multiple-contents: drop a pruned tile's additional contents (no-op if single)
            }
            self.world.resource_mut::<ResidentSet>().0.remove(&e); // Stage 1: a pruned cold tile may be Resident
            self.world.resource_mut::<FreeList>().0.push(e);
        }
    }

    /// Per-step telemetry (resident count, decoded/evicted this step).
    pub fn stats(&self) -> GeoStats {
        *self.world.resource::<GeoStats>()
    }

    /// The latest published render frame (cloned out — `Arc` bumps, plus a small key list). The stream
    /// thread mirrors this into the cross-thread handoff after each [`advance`](Self::advance).
    pub fn latest_frame(&self) -> GeoRenderFrame {
        self.world.resource::<GeoMailbox>().0.clone()
    }

    /// Advance one **streaming** step from an Unreal-space camera (cm): convert to ECEF via the ENU frame,
    /// set the LRU-cull to the streaming `max_tile_distance`, and run the full pipeline at `frame`. This is
    /// what the stream thread feeds each iteration.
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
    ) {
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
        });
    }

    /// Resident decoded tiles after the last step (telemetry).
    pub fn resident_tiles(&self) -> usize {
        self.stats().resident as usize
    }

    /// Outstanding/undrained fetches — the stream thread's "work pending" signal (telemetry).
    pub fn in_flight(&self) -> usize {
        self.streaming.as_ref().map(|s| s.source.in_flight()).unwrap_or(0)
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
    pub fn mesh_cache(&self) -> (usize, usize) {
        let c = self.world.resource::<MeshCache>();
        (c.len(), c.byte_len())
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
        self.streaming.as_ref().map(|s| s.grafted.len()).unwrap_or(0)
    }

    /// Override the decoded-mesh LRU byte budget (default [`DEFAULT_MESH_CACHE_BYTES`]). Trims immediately if
    /// the new budget is below current occupancy. For tuning / tests that want to force cache eviction.
    pub fn set_mesh_cache_budget(&mut self, max_bytes: usize) {
        let mut c = self.world.resource_mut::<MeshCache>();
        c.max_bytes = max_bytes;
        c.trim(0); // no live frame here ⇒ staleness term is 0, so a budget-cut trim evicts purely by distance
    }

    // ---- Integration / test surface (the traversal + drain wire to these next increment) ----

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
        let mut ent = self.world.entity_mut(e);
        *ent.get_mut::<TileId>().unwrap() = TileId(id);
        *ent.get_mut::<TileBounds>().unwrap() = bounds;
        *ent.get_mut::<GeometricError>().unwrap() = GeometricError(geometric_error);
        *ent.get_mut::<TileTransform>().unwrap() = TileTransform(transform);
        *ent.get_mut::<TileState>().unwrap() = TileState::Selected;
        Some(e)
    }

    /// Deliver fetched bytes to a tile (the drain system's job once wired): set `FetchedBytes` + flip to
    /// `Fetched`. The `Changed<FetchedBytes>` gate then admits it to [`decode`].
    pub fn deliver_bytes(&mut self, tile: Entity, bytes: Arc<[u8]>) {
        let mut ent = self.world.entity_mut(tile);
        ent.get_mut::<FetchedBytes>().unwrap().0 = Some(bytes);
        *ent.get_mut::<TileState>().unwrap() = TileState::Fetched;
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
    grafted: &mut HashMap<TilePath, GraftRecord>,
    path: &[u32],
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
    grafted.insert(path.to_vec(), record);
    Ok(())
}

/// Whether `path` is `prefix` itself or a descendant of it (`prefix` is a prefix of `path`) — the
/// subtree-membership test `prune_cold_grafts` uses to drop everything under a cold graft root.
fn path_has_prefix(path: &[u32], prefix: &[u32]) -> bool {
    path.len() >= prefix.len() && path[..prefix.len()] == *prefix
}

/// Bump `last_active = frame` for every grafted ancestor on `path` (including `path` itself), so a subtree
/// with any rendered/loading descendant this frame is kept warm. `O(depth)` hashmap probes, no allocation:
/// look up `&path[..k]` directly (`Vec<u32>: Borrow<[u32]>`, same trick as [`base_for`]).
fn bump_graft_activity(grafted: &mut HashMap<TilePath, GraftRecord>, path: &[u32], frame: u64) {
    for k in 0..=path.len() {
        if let Some(rec) = grafted.get_mut(&path[..k]) {
            rec.last_active = frame;
        }
    }
}

/// The base URL a tile's content URI resolves against: the nearest grafted-ancestor subtree URL (longest
/// path-prefix in `bases`), else the root `base_url`. Propagates inherited auth (P3DT `?session=…`) downward.
///
/// Probes `path`'s prefixes **longest-first** in `bases` and returns the first hit — `O(depth)` map lookups
/// (depth ≈ tileset levels, tiny). The previous full scan was `O(|bases|)`, and `bases` grows with every
/// grafted subtree over a streaming session, so the scan degraded without bound (plan §6, Phase 3 / §1E).
fn base_for<'a>(base_url: &'a str, bases: &'a HashMap<TilePath, String>, path: &[u32]) -> &'a str {
    (1..=path.len())
        .rev()
        .find_map(|k| bases.get(&path[..k]).map(String::as_str))
        .unwrap_or(base_url)
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
/// the §9.2 step-7 look-direction priority. Three terms: **screen importance** `geometric_error / distance`
/// (closer tiles + coarse fallbacks — whose error is larger — lead their own fine detail), times a
/// **view-direction weight** `0.6 + 0.5·align` where `align = dot(dir_to_tile, cam_dir) ∈ [-1, 1]` (ahead
/// ≈ 1.1×, to the side ≈ 0.6×, behind ≈ 0.1× — never zero, so importance still orders tiles when the
/// camera direction is unknown, `cam_dir == ZERO ⇒ align 0 ⇒ uniform 0.6×), times a **grafting boost** so a
/// tree-descending manifest leads same-distance content ([`MANIFEST_FETCH_BOOST`]).
fn fetch_priority(t: &SelectedTile, cam_pos: DVec3, cam_dir: DVec3) -> f64 {
    let importance = t.geometric_error / t.distance.max(1.0);
    let align = (t.bounds_center - cam_pos).normalize_or_zero().dot(cam_dir);
    let graft_boost = if t.content_uris.first().is_some_and(|u| is_tileset_uri(u)) {
        MANIFEST_FETCH_BOOST
    } else {
        1.0
    };
    importance * (0.6 + 0.5 * align) * graft_boost
}

/// Stable per-tile key: FNV-1a over the path's little-endian bytes. Used as the entity's `TileId`, the
/// `MeshCache` key, and (published as `LogiGlobeTile.tile_key`) the C++ section-cache key.
///
/// COLLISION BUDGET (audit G6 — accepted): this is a 64-bit hash, so two distinct tile paths *could* collide
/// and alias each other's `MeshCache` / C++ section entry. The probability is astronomically small (the
/// birthday bound over the few thousand simultaneously-live tiles is ~1e-13), so it is accepted rather than
/// paid for. Eliminating it would mean keying those caches by the full path — but `evict` (which populates
/// `MeshCache`) sees only the entity, not its path, so that needs a per-entity path component (a `Vec<u32>` on
/// every pool slot); deferred until/unless a collision is ever observed. (There is NO `ffi::hash_path` — the
/// cross-boundary contract is simply "the raw FNV-1a `u64`", verified by neither side; see the FFI audit.)
fn tile_key(path: &[u32]) -> u64 {
    let mut h: u64 = 0xCBF2_9CE4_8422_2325;
    for &seg in path {
        for b in seg.to_le_bytes() {
            h ^= b as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01B3);
        }
    }
    h
}

/// Distinct, stable render key for content `content_index` of a (multiple-contents) tile. Content `0` returns
/// the tile's primary key UNCHANGED — so a single-content tile (every P3DT tile) keeps the exact same key and
/// `MeshCache` entry as before — while each additional content extends the same FNV-1a hash with its index,
/// giving the C++ section cache a globally-unique key per rendered sub-mesh (same collision profile as `tile_key`).
fn content_key(path: &[u32], content_index: u32) -> u64 {
    let h = tile_key(path);
    if content_index == 0 {
        return h;
    }
    let mut h = h;
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

    fn test_world(pool: usize, max_resident: usize) -> GeoWorld {
        let e = Ellipsoid::wgs84();
        let enu = EnuFrame::from_cartographic(&e, Cartographic::from_degrees(-73.9857, 40.7484, 0.0));
        // Stub decoder: turns N bytes into a 1-vertex geometry whose radius encodes the byte count, so a
        // test can prove decode ran in parallel + transitioned state — without crafting a real GLB.
        let decoder = TileDecoder(Arc::new(|bytes: &[u8], _xf: DMat4| {
            Some(TileGeometry {
                vertices: vec![0u8; 32],
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
        // The live tile got a finite SSE and was stamped this frame; a Free slot stays at the default 0.
        assert!(w.world.get::<Sse>(t).unwrap().0 > 0.0);
        assert_eq!(w.world.get::<LastSeen>(t).unwrap().0, 7);
    }

    #[test]
    fn decode_runs_in_parallel_and_transitions_state() {
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
        // Every fetched tile decoded (par_iter over the batch) and is now resident with a mesh handle.
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
        fn request(&mut self, id: RequestId, _url: &str, _now: u64) {
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

    /// A source that returns HTTP 401 for every fetch — a Google P3DT **expired session**.
    #[derive(Default)]
    struct SessionExpiredSource {
        pending: Vec<RequestId>,
    }
    impl TileContentSource for SessionExpiredSource {
        fn request(&mut self, id: RequestId, _url: &str, _now: u64) {
            self.pending.push(id);
        }
        fn poll_completed(&mut self, _now: u64, out: &mut Vec<logi_3dtiles_io::Loaded>) {
            for id in self.pending.drain(..) {
                out.push(logi_3dtiles_io::Loaded {
                    id,
                    result: Err(logi_3dtiles_io::FetchError::Http {
                        status: 401,
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
        // The refresh re-fetches the root from this base; an instantly-refused port keeps the test off the real
        // network (the content 401s are mocked by the source — only the refresh's root re-fetch is a real call).
        let mut w = GeoWorld::from_tileset(
            "http://127.0.0.1:1/root.json".to_string(),
            root.as_bytes(),
            Cartographic::from_degrees(0.0, 0.0, 0.0),
            Box::new(SessionExpiredSource::default()),
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
        for f in 1..=100u64 {
            w.advance_streaming(view(f));
        }
        assert_eq!(
            w.stats().fetch_failed_total,
            0,
            "a 401/403 must NOT permanently give up the tile — it is a recoverable session expiry"
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
        w.advance_streaming(view(1));
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
    }

    #[test]
    fn mesh_cache_evicts_farthest_then_stalest_over_budget() {
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
        let mut c = MeshCache::new(1000); // low-water = 800 (80%)
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
        let mut c2 = MeshCache::new(1000);
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
        assert_eq!(w.mesh_cache().0, 0, "still resident — nothing cached yet");
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
        let (entries, bytes) = w.mesh_cache();
        assert_eq!(entries, 1, "the decoded mesh is deposited into the LRU on eviction");
        assert_eq!(
            bytes, 44,
            "cache accounts for the mesh footprint (32 verts + 12 index bytes)"
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
                vertices: vec![0u8; 32],
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
        assert_eq!(w.stats().mesh_cache_hits_total, 0, "a cold load is not a cache hit");

        // Phase 2 (eviction): far camera → off-screen → evicted → mesh deposited, index reconciled.
        for ff in f..f + 3 {
            w.advance_streaming(view(far, ff));
        }
        f += 3;
        assert_eq!(w.resident_tiles(), 0, "off-screen tile evicted");
        assert_eq!(w.mesh_cache().0, 1, "evicted mesh kept in the cache for re-entry");

        // Phase 3 (warm re-entry): near again → restored straight from the cache, no fetch, no decode.
        w.advance_streaming(view(near, f));
        assert_eq!(
            w.stats().mesh_cache_hits_total,
            1,
            "re-entry served from the mesh cache"
        );
        assert_eq!(w.resident_tiles(), 1, "restored to resident in a single frame");
        assert_eq!(
            w.mesh_cache().0,
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
                vertices: vec![0u8; 32],
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
        // Drive until BOTH contents have fetched (a on the parallel path, b inline) + published, or a frame cap.
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
                vertices: vec![0u8; 32],
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
                vertices: vec![0u8; 32],
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
    fn fetch_priority_favors_the_look_direction() {
        let cam_pos = DVec3::new(6_378_137.0, 0.0, 0.0);
        let cam_dir = DVec3::new(1.0, 0.0, 0.0); // looking radially outward (+X)
        let tile = |center: DVec3| SelectedTile {
            path: Vec::new(),
            world_transform: DMat4::IDENTITY,
            content_uris: Vec::new(),
            geometric_error: 100.0,
            distance: 1000.0,
            bounds_center: center,
            bounds_radius: 1.0,
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
            world_transform: DMat4::IDENTITY,
            content_uris: vec![uri.to_string()],
            geometric_error: 100.0,
            distance: 1000.0,
            bounds_center: at,
            bounds_radius: 1.0,
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
                vertices: vec![0u8; 32],
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

        // Looking TOWARD the tiles (UE +Y) → in the frustum → full SSE → refine to the leaves.
        w.advance_from_ue(cam, [0.0, 1.0, 0.0], up, aspect, fov, vh, sse, 1);
        let toward = w.refined_count();
        // Looking AWAY (UE −Y) → behind the camera → Offscreen → coarse culled SSE → refine shallower.
        w.advance_from_ue(cam, [0.0, -1.0, 0.0], up, aspect, fov, vh, sse, 2);
        let away = w.refined_count();

        assert!(
            toward > away,
            "in-view refines deeper ({toward}) than off-view coarse shell ({away})"
        );
        assert!(
            away >= 1,
            "off-view tiles are KEPT (coarse), not hard-culled — the root still refines"
        );
    }

    #[test]
    fn base_for_picks_the_longest_grafted_ancestor_prefix() {
        let root = "https://h/root.json?session=R";
        let mut bases: HashMap<TilePath, String> = HashMap::new();
        bases.insert(vec![0], "https://h/0.json?session=A".to_string());
        bases.insert(vec![0, 1], "https://h/01.json?session=B".to_string());
        bases.insert(vec![2], "https://h/2.json?session=C".to_string()); // a sibling graft, never an ancestor of [0,..]

        // No grafted ancestor → the root base.
        assert_eq!(base_for(root, &bases, &[]), root);
        assert_eq!(base_for(root, &bases, &[1]), root);
        // The one grafted ancestor on the path.
        assert_eq!(base_for(root, &bases, &[0]), "https://h/0.json?session=A");
        assert_eq!(base_for(root, &bases, &[0, 2]), "https://h/0.json?session=A");
        // The LONGEST prefix wins ([0,1] over [0]) — deeper auth overrides shallower.
        assert_eq!(base_for(root, &bases, &[0, 1]), "https://h/01.json?session=B");
        assert_eq!(base_for(root, &bases, &[0, 1, 5]), "https://h/01.json?session=B");
        // The sibling graft [2] is not an ancestor of [0,1,2] → ignored.
        assert_eq!(base_for(root, &bases, &[0, 1, 2]), "https://h/01.json?session=B");
    }

    #[test]
    fn graft_external_refuses_beyond_the_depth_cap() {
        // C1 defense-in-depth: a graft at/beyond `MAX_TILE_DEPTH` is rejected before parsing — bounding a
        // hostile external-tileset chain from growing the tree without limit. The check precedes `tile_at_mut`,
        // so the (shallow) root needn't actually contain the deep path.
        let root_json = br#"{"asset":{"version":"1.1"},"geometricError":1.0,
            "root":{"boundingVolume":{"sphere":[0,0,0,1]},"geometricError":0.0}}"#;
        let mut root = parse_tileset(root_json).unwrap().root;
        let mut grafted: HashMap<TilePath, GraftRecord> = HashMap::new();
        let deep_path = vec![0u32; MAX_TILE_DEPTH as usize]; // length == cap ⇒ refused (`>=`)
        assert!(
            graft_external(&mut root, &mut grafted, &deep_path, root_json, 0).is_err(),
            "a graft at/beyond the depth cap must be refused"
        );
        assert!(grafted.is_empty(), "a refused graft records nothing");
    }

    /// A source that returns a valid child tileset for the external-tileset URL (so a graft happens) and
    /// empty bytes (a failed fetch) for anything else — enough to drive a graft → cold → prune → re-graft
    /// cycle deterministically and offline.
    struct GraftingSource {
        pending: Vec<(RequestId, String)>,
        child_json: Vec<u8>,
    }
    impl TileContentSource for GraftingSource {
        fn request(&mut self, id: RequestId, url: &str, _now: u64) {
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
                out.push(logi_3dtiles_io::Loaded { id, result });
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
        let far = |frame: u64| GeoView {
            camera_ecef: DVec3::new(C + 20_000_000.0, 0.0, 0.0), // far → root renders coarse, child not visited
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
        fn request(&mut self, id: RequestId, url: &str, _now: u64) {
            self.requested.lock().unwrap().push(url.to_string());
            self.pending.push((id, url.to_string()));
        }
        fn poll_completed(&mut self, _now: u64, out: &mut Vec<logi_3dtiles_io::Loaded>) {
            for (id, url) in self.pending.drain(..) {
                let bytes = if url.contains(".subtree") {
                    constant_subtree_bytes()
                } else {
                    vec![1u8, 2, 3] // stub GLB → the decoder turns N>0 bytes into a 1-vertex resident mesh
                };
                out.push(logi_3dtiles_io::Loaded { id, result: Ok(bytes) });
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
        // Decoder: N>0 content bytes → a 1-vertex geometry, so a fetched implicit tile becomes resident.
        let decoder = TileDecoder(Arc::new(|bytes: &[u8], _x: DMat4| {
            (!bytes.is_empty()).then(|| TileGeometry {
                vertices: vec![0u8; 32],
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
                vertices: vec![0u8; 32],
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
