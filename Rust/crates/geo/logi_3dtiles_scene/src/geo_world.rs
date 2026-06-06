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
use logi_3dtiles_io::{http_get, resolve_url, DiskCache, HttpSource, RequestId, TileContentSource};
use logi_3dtiles_select::{select_tree, SelectParams, SelectedTile, SelectionResult, SelectionState, Visibility};
use logi_3dtiles_tileset::{parse_tileset, Tile};
use logi_geo_mesh::ue_cm_to_enu;
use logi_geospatial::{Cartographic, EnuFrame};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// A tile's child-index path from the root — the stable identity keying the tree ↔ entity map.
type TilePath = Vec<u32>;

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
#[derive(Component, Default)]
pub struct ResidentMesh(pub Option<Arc<TileGeometry>>);

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

/// Default decoded-mesh LRU budget (bytes). Sized as a re-entry buffer in front of the resident set: large
/// enough that a camera reversing direction restores meshes instead of re-decoding, small enough to bound
/// RAM. Tune on a flamegraph / RAM headroom (plan §6, Phase 1); override per-world with
/// [`GeoWorld::set_mesh_cache_budget`].
pub const DEFAULT_MESH_CACHE_BYTES: usize = 384 * 1024 * 1024;

struct MeshCacheEntry {
    geom: Arc<TileGeometry>,
    bytes: usize,
    last_seen: u64,
}

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

    /// Insert/refresh a decoded mesh keyed by `tile_key`. Does **not** trim — callers batch inserts (one per
    /// evicted tile this step) then call [`trim`](Self::trim) once, so the sort is amortized over the batch.
    fn insert(&mut self, key: u64, geom: Arc<TileGeometry>, last_seen: u64) {
        let bytes = geom.byte_size();
        if let Some(old) = self.entries.insert(key, MeshCacheEntry { geom, bytes, last_seen }) {
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

    /// Amortized batch eviction: a no-op under budget; over budget, drop the coldest entries (lowest
    /// `last_seen`) down to an 80% low-water mark so the next ~20% of inserts won't re-trigger the sort.
    fn trim(&mut self) {
        if self.bytes <= self.max_bytes {
            return;
        }
        let low_water = self.max_bytes - self.max_bytes / 5; // 80%
        let mut by_age: Vec<(u64, u64)> = self.entries.iter().map(|(&k, e)| (k, e.last_seen)).collect();
        by_age.sort_unstable_by_key(|&(_, ls)| ls); // coldest first
        for (k, _) in by_age {
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
}

/// One tile in a published Geo render frame: its stable key (for C++ section caching) + a pinned share of
/// its geometry. Holding the `Arc` keeps the buffers alive for the renderer even after eviction.
#[derive(Clone)]
pub struct GeoRenderTile {
    pub tile_key: u64,
    pub geom: Arc<TileGeometry>,
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
const CULLED_SSE_FACTOR: f64 = 4.0;
/// Angular slack (radians, ≈7°) added to the view frustum's half-angles for the in-view test, so tiles at
/// the very screen edge classify in-view (full detail) instead of flipping to the coarse shell — no LOD seam
/// crawling the screen border, and a buffer against one frame of camera rotation.
const FRUSTUM_MARGIN_RAD: f64 = 0.12;

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
    mut tiles: Query<(&mut FetchedBytes, &TileTransform, &mut ResidentMesh, &mut TileState), Changed<FetchedBytes>>,
) {
    let decode_fn = &decoder.0;
    tiles
        .par_iter_mut()
        .batching_strategy(BatchingStrategy::fixed(DECODE_BATCH))
        .for_each(|(mut bytes, xf, mut mesh, mut state)| {
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
                // The raw GLB is dead once decoded — drop it so a resident tile pins only its mesh, not also
                // its (often larger) source bytes. Skips ~100 KB–1 MB of dead weight per resident tile.
                bytes.0 = None;
            }
            // A decode failure leaves the tile in `Fetched` with its bytes — `reclaim_failed` frees the slot
            // and marks it terminal; we never panic across the decode (the decoder returns `Option`).
        });
}

/// **Sequential** LRU eviction: when resident tiles exceed the cap, reset the oldest (by `LastSeen`, not
/// seen this frame) back to `Free`. The decoded mesh is **deposited into [`MeshCache`]** (not dropped) so a
/// camera returning to the tile restores it without re-fetch/-decode; the slot returns to the free-list.
/// **No despawn** (no structural change). Runs after the parallel passes.
fn evict(
    view: Res<GeoView>,
    cap: Res<MaxResident>,
    mut free: ResMut<FreeList>,
    mut stats: ResMut<GeoStats>,
    mut cache: ResMut<MeshCache>,
    mut tiles: Query<(Entity, &TileId, &mut TileState, &LastSeen, &mut ResidentMesh)>,
) {
    // Count resident; collect eviction candidates (resident AND not stamped this frame).
    let mut resident = 0usize;
    let mut candidates: Vec<(Entity, u64)> = Vec::new();
    for (e, _, state, seen, _) in tiles.iter() {
        if *state == TileState::Resident {
            resident += 1;
            if seen.0 != view.frame {
                candidates.push((e, seen.0));
            }
        }
    }
    stats.resident = resident as u32;
    stats.evicted_this_step = 0;
    if resident <= cap.0 {
        return;
    }
    let overflow = resident - cap.0;
    let n = overflow.min(candidates.len());
    if n == 0 {
        return;
    }
    // Partition so the `n` oldest are first (O(len) average, no full sort — mirrors the streaming evictor).
    if n < candidates.len() {
        candidates.select_nth_unstable_by_key(n, |(_, ls)| *ls);
    }
    for (e, _) in candidates.into_iter().take(n) {
        if let Ok((_, id, mut state, seen, mut mesh)) = tiles.get_mut(e) {
            *state = TileState::Free;
            // Deposit the decoded mesh in the LRU instead of dropping it — the warm re-entry path. The `Arc`
            // also keeps any published frame's copy alive; the cache merely holds one more reference.
            if let Some(geom) = mesh.0.take() {
                cache.insert(id.0, geom, seen.0);
            }
            free.0.push(e);
            stats.evicted_this_step += 1;
        }
    }
    cache.trim(); // amortized: only sorts/drops when the batch pushed the cache over budget
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
    tiles: Query<(&TileState, &TileId, &LastSeen, &ResidentMesh)>,
) {
    let frame = &mut mailbox.0;
    frame.frame = view.frame;
    frame.origin_ecef = enu.0.origin_ecef.to_array();
    frame.tiles.clear();
    for (state, id, seen, mesh) in tiles.iter() {
        if *state == TileState::Resident && seen.0 == view.frame {
            if let Some(geom) = &mesh.0 {
                frame.tiles.push(GeoRenderTile {
                    tile_key: id.0,
                    geom: Arc::clone(geom),
                });
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
    grafted: HashSet<TilePath>,
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
}

impl GeoWorld {
    /// Pre-spawn `pool_size` tile entities (all `Free`, one uniform archetype), build the schedule, and
    /// ensure the **shared** global `ComputeTaskPool` exists (joins the gameplay pool if already created).
    pub fn new(pool_size: usize, enu: EnuFrame, decoder: TileDecoder, max_resident: usize) -> Self {
        // Share the one global ComputeTaskPool (research §10) — `get_or_init` joins gameplay's if it ran first.
        ComputeTaskPool::get_or_init(|| TaskPoolBuilder::new().thread_name("LogiCompute".to_string()).build());

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
                    TileTransform(DMat4::IDENTITY),
                    TileState::Free,
                    Sse(0.0),
                    CamDistance(0.0),
                    LastSeen(0),
                    FetchedBytes::default(),
                    ResidentMesh::default(),
                ))
                .id();
            free.push(e);
        }
        world.insert_resource(FreeList(free));

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
        let ellipsoid = logi_geospatial::Ellipsoid::wgs84();
        let mut world = Self::new(pool_size, enu, decoder, max_resident);
        world.streaming = Some(GeoStreaming {
            base_url,
            ellipsoid,
            root: tileset.root,
            grafted: HashSet::new(),
            bases: HashMap::new(),
            source,
            in_flight: HashMap::new(),
            in_flight_paths: HashSet::new(),
            next_id: 1,
            max_in_flight: 64,
            max_tile_distance,
            index: HashMap::new(),
            decode_failed: HashSet::new(),
            sel_state: SelectionState::new(),
            cam_dir_ecef: DVec3::ZERO,
            frustum: ViewFrustum::default(),
        });
        Ok(world)
    }

    /// Resolve a Cesium **ion** asset → fetch its root tileset → build a streaming Geo World on a worker-pool
    /// [`HttpSource`]. **Blocking** (ion resolve + root fetch) — call off the render thread (the FFI does, on
    /// the create path). Mirrors `TilesetWorld::from_ion` (which retires with that struct). `cache` is the
    /// optional persistent tile cache; `pool_size`/`max_resident` size the entity pool + resident cap.
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

    /// One **streaming** step (the full pipeline, replacing `TilesetWorld::advance`): drain completed
    /// fetches → run the tileset traversal (acquire/request) → run the schedule (parallel decode, evict,
    /// publish). The drain + traversal are sequential (pointer-chasing); the schedule fans out.
    pub fn advance_streaming(&mut self, view: GeoView) {
        self.drain_raw(view.frame);
        self.select_request(view);
        self.advance(view);
        self.reconcile_index();
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
            let inflight = {
                let s = self.streaming.as_mut().unwrap();
                let Some(inflight) = s.in_flight.remove(&loaded.id) else {
                    continue;
                };
                s.in_flight_paths.remove(&inflight.path);
                inflight
            };
            if loaded.bytes.is_empty() {
                continue; // failed fetch
            }
            let kind = sniff_content_kind(&loaded.bytes).unwrap_or(inflight.kind);
            match kind {
                ContentKind::Tileset => {
                    let s = self.streaming.as_mut().unwrap();
                    if graft_external(&mut s.root, &mut s.grafted, &inflight.path, &loaded.bytes).is_ok() {
                        // Descendants inherit this subtree's fetch URL (carries the P3DT `?session=…`).
                        s.bases.insert(inflight.path, inflight.url);
                    }
                }
                ContentKind::Glb => {
                    if let Some(e) = inflight.entity {
                        let bytes: Arc<[u8]> = Arc::from(loaded.bytes.into_boxed_slice());
                        let mut ent = self.world.entity_mut(e);
                        ent.get_mut::<FetchedBytes>().unwrap().0 = Some(bytes);
                        *ent.get_mut::<TileState>().unwrap() = TileState::Fetched;
                    }
                }
            }
        }
    }

    /// Run the SSE traversal over the (growing) tree: stamp visible resident tiles into this frame's working
    /// set, and acquire + request a fetch for each selected-but-unloaded tile. Reuses `select_tree`.
    fn select_request(&mut self, view: GeoView) {
        if self.streaming.is_none() {
            return;
        }
        let cam = view.camera_ecef;
        // `is_loaded` set (owned) so the walk borrows neither the world nor `streaming` mutably.
        let resident: HashSet<TilePath> = {
            let s = self.streaming.as_ref().unwrap();
            s.index
                .iter()
                .filter(|(_, &e)| self.world.get::<TileState>(e) == Some(&TileState::Resident))
                .map(|(p, _)| p.clone())
                .collect()
        };
        // Walk the tree → render (with the cross-frame anti-pop kick: coarse ancestor instead of a hole-y
        // partial refinement) + load (to fetch). Last frame's `sel_state` drives the kick AND the hysteresis
        // dead-band so a camera on an SSE threshold can't flip a tile parent↔children every frame.
        let mut sel = {
            let s = self.streaming.as_ref().unwrap();
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
            select_tree(
                &s.root,
                &s.ellipsoid,
                &params,
                &mut |p| resident.contains(p),
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
            )
        };
        // Stamp visible (render) tiles so evict spares them + publish includes them.
        for rt in &sel.render {
            let e = self.streaming.as_ref().unwrap().index.get(&rt.path).copied();
            if let Some(e) = e {
                if let Some(mut ls) = self.world.get_mut::<LastSeen>(e) {
                    ls.0 = view.frame;
                }
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
        let cam_pos = view.camera_ecef;
        let cam_dir = self.streaming.as_ref().unwrap().cam_dir_ecef;
        sel.load.sort_unstable_by(|a, b| {
            fetch_priority(b, cam_pos, cam_dir)
                .partial_cmp(&fetch_priority(a, cam_pos, cam_dir))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        // For each selected-but-unloaded tile: skip if already accounted for, else try a warm restore from
        // the decoded-mesh cache, else (cold) enqueue a fetch.
        for lt in &sel.load {
            // Already known / in-flight / grafted / terminally-bad → nothing to do.
            {
                let s = self.streaming.as_ref().unwrap();
                if s.index.contains_key(&lt.path)
                    || s.grafted.contains(&lt.path)
                    || s.in_flight_paths.contains(&lt.path)
                    || s.decode_failed.contains(&lt.path)
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
            let resolved = {
                let s = self.streaming.as_ref().unwrap();
                if s.source.in_flight() >= s.max_in_flight {
                    break; // outstanding-fetch cap reached this step
                }
                let Some(uri) = lt.content_uris.first() else {
                    continue;
                };
                let kind = if is_tileset_uri(uri) {
                    ContentKind::Tileset
                } else {
                    ContentKind::Glb
                };
                match resolve_url(base_for(s, &lt.path), uri) {
                    Ok(full) => (full, kind),
                    Err(_) => continue,
                }
            };
            self.request_one(lt, resolved.1, resolved.0);
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
        // Need a free slot *and* the cached mesh; if no slot is free, leave the mesh cached and retry later.
        if self.world.resource::<FreeList>().0.is_empty() {
            return;
        }
        let Some(geom) = self.world.resource_mut::<MeshCache>().take(key) else {
            return;
        };
        let e = self.world.resource_mut::<FreeList>().0.pop().unwrap();
        {
            let mut ent = self.world.entity_mut(e);
            *ent.get_mut::<TileId>().unwrap() = TileId(key);
            *ent.get_mut::<TileBounds>().unwrap() = TileBounds {
                center: lt.bounds_center,
                radius: lt.bounds_radius,
            };
            *ent.get_mut::<GeometricError>().unwrap() = GeometricError(lt.geometric_error);
            *ent.get_mut::<TileTransform>().unwrap() = TileTransform(lt.world_transform);
            *ent.get_mut::<TileState>().unwrap() = TileState::Resident;
            *ent.get_mut::<LastSeen>().unwrap() = LastSeen(frame);
            ent.get_mut::<ResidentMesh>().unwrap().0 = Some(geom);
            // `FetchedBytes` is already `None` on every free slot (decode clears it; evict/reclaim never set
            // it), so leave it untouched — avoids a spurious `Changed<FetchedBytes>` that `decode` would scan.
        }
        self.streaming.as_mut().unwrap().index.insert(lt.path.clone(), e);
        self.world.resource_mut::<GeoStats>().mesh_cache_hits_total += 1;
    }

    /// Pre-emptively cancel in-flight fetches whose tile is no longer in this frame's selection — the
    /// fast-motion case where the camera leaves a region before its tiles arrive. `wanted` is the set of
    /// still-selected unloaded paths (`sel.load`); any in-flight path outside it is stale: cancel the fetch,
    /// drop its `in_flight`/`index` bookkeeping, and return its reserved GLB slot to the pool. This frees
    /// in-flight budget for the tiles we actually want now (paired with the priority sort in the caller).
    fn cancel_stale(&mut self, wanted: &HashSet<&[u32]>) {
        // Phase 1 — collect the stale fetches (immutable borrow; `wanted` borrows the caller's `sel.load`).
        let stale: Vec<(RequestId, TilePath, Option<Entity>)> = {
            let s = self.streaming.as_ref().unwrap();
            s.in_flight
                .iter()
                .filter(|(_, inf)| !wanted.contains(inf.path.as_slice()))
                .map(|(&id, inf)| (id, inf.path.clone(), inf.entity))
                .collect()
        };
        if stale.is_empty() {
            return;
        }
        // Phase 2 — cancel each fetch + drop its streaming bookkeeping (one `streaming` borrow).
        {
            let s = self.streaming.as_mut().unwrap();
            for (id, path, _) in &stale {
                s.source.cancel(*id);
                s.in_flight.remove(id);
                s.in_flight_paths.remove(path);
                s.index.remove(path);
            }
        }
        // Phase 3 — return any reserved GLB slot to the pool (a tileset fetch has no entity).
        for (_, _, entity) in &stale {
            if let Some(e) = entity {
                *self.world.get_mut::<TileState>(*e).unwrap() = TileState::Free;
                self.world.get_mut::<FetchedBytes>(*e).unwrap().0 = None;
                self.world.get_mut::<ResidentMesh>(*e).unwrap().0 = None;
                self.world.resource_mut::<FreeList>().0.push(*e);
            }
        }
        self.world.resource_mut::<GeoStats>().fetches_cancelled_total += stale.len() as u32;
    }

    /// Acquire a pool entity for a GLB tile (none for an external tileset — it grafts) and enqueue its fetch.
    fn request_one(&mut self, lt: &SelectedTile, kind: ContentKind, full: String) {
        let entity = if kind == ContentKind::Glb {
            let Some(e) = self.world.resource_mut::<FreeList>().0.pop() else {
                return; // pool exhausted — skip (grows at a step boundary later)
            };
            let key = tile_key(&lt.path);
            let mut ent = self.world.entity_mut(e);
            *ent.get_mut::<TileId>().unwrap() = TileId(key);
            *ent.get_mut::<TileBounds>().unwrap() = TileBounds {
                center: lt.bounds_center,
                radius: lt.bounds_radius,
            };
            *ent.get_mut::<GeometricError>().unwrap() = GeometricError(lt.geometric_error);
            *ent.get_mut::<TileTransform>().unwrap() = TileTransform(lt.world_transform);
            *ent.get_mut::<TileState>().unwrap() = TileState::Requested;
            *ent.get_mut::<LastSeen>().unwrap() = LastSeen(0);
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
            },
        );
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
            self.world.resource_mut::<FreeList>().0.push(*e);
        }
        if !failed.is_empty() {
            self.world.resource_mut::<GeoStats>().decode_failed_total += failed.len() as u32;
        }
    }

    /// Per-step telemetry (resident count, decoded/evicted this step).
    pub fn stats(&self) -> GeoStats {
        self.world.resource::<GeoStats>().copied_or_default()
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

    /// Total tile-entity count — **invariant across `advance`** (the pool never spawns/despawns in the loop).
    pub fn entity_count(&self) -> usize {
        self.world.entities().len() as usize
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

    /// Override the decoded-mesh LRU byte budget (default [`DEFAULT_MESH_CACHE_BYTES`]). Trims immediately if
    /// the new budget is below current occupancy. For tuning / tests that want to force cache eviction.
    pub fn set_mesh_cache_budget(&mut self, max_bytes: usize) {
        let mut c = self.world.resource_mut::<MeshCache>();
        c.max_bytes = max_bytes;
        c.trim();
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
/// composed transform replace the placeholder's), so the next traversal descends into it. Mirrors
/// `TilesetWorld::graft` (which retires with that struct once `GlobeStreamer` is rewired to `GeoWorld`).
fn graft_external(
    root: &mut Tile,
    grafted: &mut HashSet<TilePath>,
    path: &[u32],
    json_bytes: &[u8],
) -> Result<(), StreamError> {
    let child = parse_tileset(json_bytes).map_err(|e| StreamError::Tileset(e.to_string()))?;
    let ext = child.root;
    let tile = tile_at_mut(root, path)
        .ok_or_else(|| StreamError::Tileset(format!("graft target {path:?} no longer exists")))?;
    let composed = tile.matrix() * ext.matrix();
    tile.transform = Some(composed.to_cols_array());
    tile.content = ext.content;
    tile.contents = ext.contents;
    tile.children = ext.children;
    grafted.insert(path.to_vec());
    Ok(())
}

/// The base URL a tile's content URI resolves against: the nearest grafted-ancestor subtree URL (longest
/// path-prefix in `bases`), else the root `base_url`. Propagates inherited auth (P3DT `?session=…`) downward.
fn base_for<'a>(s: &'a GeoStreaming, path: &[u32]) -> &'a str {
    let mut best = s.base_url.as_str();
    let mut best_len = 0usize;
    for (p, url) in &s.bases {
        if p.len() > best_len && p.len() <= path.len() && path[..p.len()] == p[..] {
            best = url;
            best_len = p.len();
        }
    }
    best
}

/// Fetch-priority **score** for ordering the per-step load queue (higher = fetch first), the blend behind
/// the §9.2 step-7 look-direction priority. Two terms: **screen importance** `geometric_error / distance`
/// (closer tiles + coarse fallbacks — whose error is larger — lead their own fine detail), times a
/// **view-direction weight** `0.6 + 0.5·align` where `align = dot(dir_to_tile, cam_dir) ∈ [-1, 1]` (ahead
/// ≈ 1.1×, to the side ≈ 0.6×, behind ≈ 0.1× — never zero, so importance still orders tiles when the
/// camera direction is unknown, `cam_dir == ZERO ⇒ align 0 ⇒ uniform 0.6×).
fn fetch_priority(t: &SelectedTile, cam_pos: DVec3, cam_dir: DVec3) -> f64 {
    let importance = t.geometric_error / t.distance.max(1.0);
    let align = (t.bounds_center - cam_pos).normalize_or_zero().dot(cam_dir);
    importance * (0.6 + 0.5 * align)
}

/// Stable per-tile key (FNV-1a over the path's little-endian bytes) — the entity's `TileId` and the C++
/// section-cache key. (Matches `ffi::hash_path` so the FFI can key sections off the published `tile_key`.)
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

/// The ENU rebase frame, as a resource the schedule reads (e.g. [`publish`] for the frame's ECEF origin).
#[derive(Resource, Clone, Copy)]
struct GeoEnu(EnuFrame);

/// Small helper: `GeoStats` is `Copy + Default`, so reading it can't fail meaningfully.
trait CopiedOrDefault {
    fn copied_or_default(&self) -> GeoStats;
}
impl CopiedOrDefault for GeoStats {
    fn copied_or_default(&self) -> GeoStats {
        *self
    }
}

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
        // show: each drained tile's synthetic (non-glTF) payload fails `decode`, so `reclaim_failed` frees
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
    fn mesh_cache_lru_evicts_coldest_over_budget() {
        let g = |n: usize| {
            Arc::new(TileGeometry {
                vertices: vec![0u8; n],
                indices: Vec::new(),
                texture: Vec::new(),
                bounds_center: [0.0; 3],
                bounds_radius: 0.0,
            })
        };
        let mut c = MeshCache::new(1000); // low-water = 800 (80%)
        c.insert(1, g(400), 10); // coldest
        c.insert(2, g(400), 20);
        assert_eq!((c.len(), c.byte_len()), (2, 800));
        c.insert(3, g(400), 30); // 1200 > 1000 budget
        c.trim(); // drop coldest (key 1) until <= 800
        assert!(
            !c.contains_key(1),
            "the coldest entry (lowest last_seen) is evicted first"
        );
        assert!(c.contains_key(2) && c.contains_key(3));
        assert_eq!(c.byte_len(), 800, "trimmed down to the low-water mark, not lower");
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

        // Camera 3 km "north" of the tiles (UE −Y maps to ENU +North). At 3 km the mid-children sit at
        // SSE ≈ 31 px: above the 16 px in-view target (refine) but below the 64 px culled target (stop).
        let cam = [0.0, -300_000.0, 0.0];
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
}
