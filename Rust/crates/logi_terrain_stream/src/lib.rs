//! The streaming engine — the heart of "streaming is the product".
//!
//! Owns: the world index, async request scheduling (via a [`Provider`]), residency (bounded
//! [`ResidencyCache`]), predictive prefetch, and **stable parent fallback**. Deterministic: the
//! synthetic provider's latency derives from the tile id, never from a clock or RNG.
//!
//! Invariants (asserted by `logi_terrain_tools`):
//! - resident bytes never exceed the budget,
//! - in-flight requests never exceed `max_in_flight`,
//! - there is always something correct to render (the root ancestor is pinned),
//! - steady-state streaming allocates nothing (all per-frame buffers are reused).

use std::collections::{BTreeMap, HashMap, HashSet};

use logi_terrain_cache::ResidencyCache;
use logi_terrain_core::{DVec3, Telemetry, TileId, Vec3};
use logi_terrain_lod::desired_level;

pub mod culling;
pub mod scheduler;
use culling::{Frustum, HorizonCuller};
use scheduler::{EvalContext, PageState, PriorityTerms, QueueClass, StreamingBudgets, QUEUE_COUNT};

/// A streamed page payload. (Phase 0: just its byte size; later, a decoded-mesh handle.)
#[derive(Clone, Copy)]
pub struct Page {
    pub size_bytes: usize,
}

/// Async content source. Implementations fetch tile pages off the main path and report completions.
pub trait Provider {
    /// Enqueue an async fetch for `id` at logical time `now`.
    fn request(&mut self, id: TileId, now: u64);
    /// Drain tiles whose fetch has completed by `now` into `out` as `(id, byte_size)`.
    fn poll_completed(&mut self, now: u64, out: &mut Vec<(TileId, usize)>);
    /// Number of outstanding fetches.
    fn in_flight(&self) -> usize;
}

#[inline]
fn splitmix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = x;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Deterministic provider with simulated network latency + jitter. Reproducible: latency derives from
/// the tile id, so the whole simulation replays identically.
pub struct SyntheticProvider {
    base_latency: u64,
    jitter: u64,
    pending: BTreeMap<u64, Vec<TileId>>,
    in_flight: usize,
}

impl SyntheticProvider {
    pub fn new(base_latency: u64, jitter: u64) -> Self {
        Self { base_latency, jitter, pending: BTreeMap::new(), in_flight: 0 }
    }

    fn latency_for(&self, id: TileId) -> u64 {
        let h = splitmix64(id.key());
        (self.base_latency + if self.jitter == 0 { 0 } else { h % self.jitter }).max(1)
    }

    /// Deterministic page size for a tile (4..8 KiB).
    pub fn page_size_for(id: TileId) -> usize {
        4096 + (splitmix64(id.key() ^ 0xABCD) as usize % 4096)
    }
}

impl Provider for SyntheticProvider {
    fn request(&mut self, id: TileId, now: u64) {
        let ready = now + self.latency_for(id);
        self.pending.entry(ready).or_default().push(id);
        self.in_flight += 1;
    }

    fn poll_completed(&mut self, now: u64, out: &mut Vec<(TileId, usize)>) {
        out.clear();
        while let Some((&key, _)) = self.pending.iter().next() {
            if key > now {
                break;
            }
            let ids = self.pending.remove(&key).unwrap();
            for id in ids {
                out.push((id, Self::page_size_for(id)));
                self.in_flight = self.in_flight.saturating_sub(1); // FIX 5: avoid usize underflow
            }
        }
    }

    fn in_flight(&self) -> usize {
        self.in_flight
    }
}

/// The logical terrain world: a quadtree over a square extent centered at the origin.
pub struct World {
    pub world_size: f64,
    pub max_level: u8,
    pub base_geometric_error: f64,
    /// Max terrain relief (meters); culling bounds enclose ±this so displaced geometry isn't culled.
    pub height_amplitude: f64,
}

impl World {
    pub fn new(world_size: f64, max_level: u8, base_geometric_error: f64) -> Self {
        Self { world_size, max_level, base_geometric_error, height_amplitude: 0.0 }
    }

    /// Set the max terrain relief (meters) so view-culling bounds enclose displaced geometry.
    pub fn set_height_amplitude(&mut self, amplitude: f64) {
        self.height_amplitude = amplitude.max(0.0);
    }

    /// Total addressable leaf tiles at the finest level.
    pub fn logical_tile_count(&self) -> u64 {
        let side = 1u64 << self.max_level;
        side * side
    }

    /// World-space edge length of a tile at `level`.
    #[inline]
    pub fn tile_size_at(&self, level: u8) -> f64 {
        self.tile_size(level)
    }

    /// World-space center `(x, z)` of `tile`.
    pub fn tile_center(&self, tile: TileId) -> (f64, f64) {
        let ts = self.tile_size(tile.level);
        let half = self.world_size * 0.5;
        let cx = -half + (tile.x as f64 + 0.5) * ts;
        let cz = -half + (tile.y as f64 + 0.5) * ts;
        (cx, cz)
    }

    /// Conservative world-space bounding sphere of a tile, for culling. Center at the tile's planar
    /// center (`y = 0`); radius covers the horizontal half-diagonal (`~0.707 * tile_size`) plus vertical
    /// relief slack. `1.2 * tile_size` is a safe over-estimate (terrain relief scales with extent), and
    /// over-estimating only *under*-culls — it never drops visible geometry.
    pub fn tile_bounds(&self, tile: TileId) -> (DVec3, f64) {
        let (cx, cz) = self.tile_center(tile);
        // Enclose the horizontal half-diagonal AND the vertical relief (±amplitude) so displaced terrain
        // is never wrongly frustum-culled. Falls back to the old 1.2*size slack when the world is flat.
        let ts = self.tile_size(tile.level);
        let horiz = ts * std::f64::consts::FRAC_1_SQRT_2; // 0.7071 * size
        let radius = (horiz * horiz + self.height_amplitude * self.height_amplitude)
            .sqrt()
            .max(ts * 1.2);
        (DVec3::new(cx, 0.0, cz), radius)
    }

    #[inline]
    fn side(&self, level: u8) -> u32 {
        // FIX L2: guard the `1u32 << level` shift (sibling of TileId::side) — `level >= 32` overflows.
        debug_assert!(level < 32, "World::side at level {} overflows the 32-bit `1u32 << level` shift", level);
        1u32 << level
    }
    #[inline]
    fn tile_size(&self, level: u8) -> f64 {
        self.world_size / self.side(level) as f64
    }

    /// The tile at `level` containing world position `(x, z)`.
    pub fn tile_for(&self, x: f64, z: f64, level: u8) -> TileId {
        let side = self.side(level);
        let ts = self.tile_size(level);
        let half = self.world_size * 0.5;
        let tx = (((x + half) / ts).floor()).clamp(0.0, (side - 1) as f64) as u32;
        let ty = (((z + half) / ts).floor()).clamp(0.0, (side - 1) as f64) as u32;
        TileId::new(level, tx, ty)
    }

    /// Fill `out` with the `(2r+1)^2` window of tiles at `level` around `(x, z)`, clamped to bounds.
    pub fn window(&self, x: f64, z: f64, level: u8, radius: u32, out: &mut Vec<TileId>) {
        out.clear();
        let c = self.tile_for(x, z, level);
        let side = self.side(level);
        let x0 = c.x.saturating_sub(radius);
        let y0 = c.y.saturating_sub(radius);
        let x1 = (c.x + radius).min(side - 1);
        let y1 = (c.y + radius).min(side - 1);
        for ty in y0..=y1 {
            for tx in x0..=x1 {
                out.push(TileId::new(level, tx, ty));
            }
        }
    }
}

/// Streaming engine configuration.
pub struct StreamConfig {
    pub budget_bytes: usize,
    pub max_in_flight: usize,
    pub window_radius: u32,
    pub sse_target_pixels: f64,
    pub fov_y_radians: f64,
    pub viewport_height: f64,
    /// How many frames of `motion` ahead to prefetch.
    pub prefetch_lead: f64,
}

/// The streaming engine: turns a camera view over a [`World`] into a bounded resident set and a
/// renderable tile list, fetching via a [`Provider`].
pub struct StreamEngine<P: Provider> {
    cfg: StreamConfig,
    cache: ResidencyCache<Page>,
    provider: P,
    in_flight: HashSet<TileId>,
    tick: u64,
    pub telemetry: Telemetry,

    // Reusable scratch — steady-state streaming allocates nothing.
    window_buf: Vec<TileId>,
    desired: Vec<TileId>,
    seen_desired: HashSet<TileId>,
    completed: Vec<(TileId, usize)>,
    evicted: Vec<TileId>,
    renderable: Vec<TileId>,
    renderable_set: HashSet<TileId>,

    // Scheduler (scheduler-design.md §6).
    budgets: StreamingBudgets,
    states: HashMap<TileId, PageState>,
    candidates: Vec<(TileId, PriorityTerms, QueueClass)>,
    candidate_seen: HashSet<TileId>,
    queue_counts: [u32; QUEUE_COUNT],
    last_top_terms: PriorityTerms,

    // Per-frame view params, taken from the live camera each frame (default to cfg until set_view).
    view_fov_y_radians: f64,
    view_viewport_width: f64,
    view_viewport_height: f64,
    view_sse_target_pixels: f64,
    view_forward: DVec3,
    view_up: DVec3,

    // View-aware selection scratch (culling.rs §2.3).
    select_stack: Vec<TileId>,
    last_select_capped: bool,
}

impl<P: Provider> StreamEngine<P> {
    pub fn new(cfg: StreamConfig, provider: P) -> Self {
        let cache = ResidencyCache::with_budget(cfg.budget_bytes);
        let budgets = StreamingBudgets::from_cache(cfg.budget_bytes, cfg.max_in_flight as u32);
        let view_fov = cfg.fov_y_radians;
        let view_vp = cfg.viewport_height;
        let view_sse = cfg.sse_target_pixels;
        let view_w = cfg.viewport_height; // aspect 1 until a real camera arrives
        Self {
            cfg,
            cache,
            provider,
            in_flight: HashSet::new(),
            tick: 0,
            telemetry: Telemetry::default(),
            window_buf: Vec::new(),
            desired: Vec::new(),
            seen_desired: HashSet::new(),
            completed: Vec::new(),
            evicted: Vec::new(),
            renderable: Vec::new(),
            renderable_set: HashSet::new(),
            budgets,
            states: HashMap::new(),
            candidates: Vec::new(),
            candidate_seen: HashSet::new(),
            queue_counts: [0; QUEUE_COUNT],
            last_top_terms: PriorityTerms::default(),
            view_fov_y_radians: view_fov,
            view_viewport_width: view_w,
            view_viewport_height: view_vp,
            view_sse_target_pixels: view_sse,
            view_forward: DVec3::default(), // zero => no view => omnidirectional window
            view_up: DVec3::new(0.0, 1.0, 0.0),
            select_stack: Vec::new(),
            last_select_capped: false,
        }
    }

    pub fn cache(&self) -> &ResidencyCache<Page> {
        &self.cache
    }
    pub fn renderable(&self) -> &[TileId] {
        &self.renderable
    }
    pub fn telemetry(&self) -> Telemetry {
        self.telemetry
    }

    pub fn budgets(&self) -> StreamingBudgets {
        self.budgets
    }
    pub fn set_budgets(&mut self, budgets: StreamingBudgets) {
        self.budgets = budgets;
        // FIX L3: apply the EXACT byte budget, not the MB-rounded field — otherwise
        // `set_budgets(budgets())` would floor a sub-MB / non-MB-aligned cache budget on every round-trip.
        self.cache.set_budget(budgets.total_cpu_cache_bytes as usize);
        self.cfg.max_in_flight = budgets.max_inflight_requests as usize;
    }
    /// Per-frame request counts by [`QueueClass`] (index = `queue as usize`).
    pub fn queue_counts(&self) -> [u32; QUEUE_COUNT] {
        self.queue_counts
    }
    /// The highest-priority candidate's term breakdown last frame ("why did this load (late)?").
    pub fn last_priority_terms(&self) -> PriorityTerms {
        self.last_top_terms
    }
    pub fn page_state(&self, tile: &TileId) -> PageState {
        self.states.get(tile).copied().unwrap_or(PageState::Unknown)
    }

    /// Re-budget the engine at runtime (memory budget + in-flight cap).
    pub fn set_budget(&mut self, budget_bytes: usize, max_in_flight: usize) {
        self.cache.set_budget(budget_bytes);
        self.cfg.max_in_flight = max_in_flight;
        self.budgets = StreamingBudgets::from_cache(budget_bytes, max_in_flight as u32);
    }

    /// Update per-frame view params from the live camera: the frustum basis (`forward`/`up`), FOV,
    /// viewport, and SSE target. A ~zero `forward` means "no camera basis" → the engine falls back to
    /// the omnidirectional window (stress harness). Positive scalars override; defaults persist otherwise.
    pub fn set_view(
        &mut self,
        forward: Vec3,
        up: Vec3,
        fov_y_radians: f64,
        viewport_width: f64,
        viewport_height: f64,
        sse_target_pixels: f64,
    ) {
        // The camera basis arrives as f32 (Unity float3); widen to f64 for planet-scale world math.
        self.view_forward = forward.as_dvec3();
        if up.length() > 1e-6 {
            self.view_up = up.as_dvec3();
        }
        if fov_y_radians > 0.0 {
            self.view_fov_y_radians = fov_y_radians;
        }
        if viewport_width > 0.0 {
            self.view_viewport_width = viewport_width;
        }
        if viewport_height > 0.0 {
            self.view_viewport_height = viewport_height;
        }
        if sse_target_pixels > 0.0 {
            self.view_sse_target_pixels = sse_target_pixels;
        }
        // FIX 6: guard against a zero/negative viewport or FOV so aspect/SSE math never divides by zero.
        if !(self.view_viewport_height > 0.0) {
            self.view_viewport_height = 1080.0;
        }
        if !(self.view_viewport_width > 0.0) {
            self.view_viewport_width = self.view_viewport_height;
        }
        if !(self.view_fov_y_radians > 0.0) {
            self.view_fov_y_radians = 1.0;
        }
    }

    /// The current desired set (visible, SSE-appropriate tiles). For tests/inspection.
    pub fn desired(&self) -> &[TileId] {
        &self.desired
    }
    /// Did the last view-aware selection hit its hard tile cap (work was dropped)? Surfaced rather than
    /// silently truncating (doctrine: no silent caps).
    pub fn last_select_capped(&self) -> bool {
        self.last_select_capped
    }

    /// A real camera basis is present (vs. the stress harness, which only moves a position).
    #[inline]
    fn has_view(&self) -> bool {
        self.view_forward.length() > 0.5
    }

    /// View-aware hierarchical selection (system-boundary §2.3): descend the quadtree from the root,
    /// pruning whole subtrees by **frustum** and **horizon**, refining by **screen-space error**. Fills
    /// `self.desired` (deduped via `self.seen_desired`) with the visible, SSE-appropriate, *mixed-level*
    /// tiles — coarse far away, fine near, nothing behind. Bounded by `MAX_SELECTED`.
    fn select_hierarchical(
        &mut self,
        world: &World,
        camera: DVec3,
        frustum: &Frustum,
        horizon: &HorizonCuller,
    ) {
        const MAX_SELECTED: usize = 8192;
        self.last_select_capped = false;
        self.select_stack.clear();
        self.select_stack.push(TileId::root());
        while let Some(node) = self.select_stack.pop() {
            let (center, radius) = world.tile_bounds(node);
            // Hierarchical cull: drop the whole subtree if its bounds are outside the frustum or below
            // the horizon.
            if !frustum.intersects_sphere(center, radius) || !horizon.is_visible(camera, center) {
                continue;
            }
            let refine = node.level < world.max_level && {
                // FIX L1: overflow-safe geometric-error halving (matches logi_terrain_lod FIX 5) —
                // `1u64 << level` would overflow past level 63; `0.5^level` cannot.
                let ge = world.base_geometric_error * 0.5f64.powi(node.level as i32);
                let dist = camera.distance(center).max(1.0);
                logi_terrain_lod::screen_space_error(
                    ge,
                    dist,
                    self.view_fov_y_radians,
                    self.view_viewport_height,
                ) > self.view_sse_target_pixels
            };
            if refine {
                for child in node.children() {
                    self.select_stack.push(child);
                }
            } else if self.seen_desired.insert(node) {
                if self.desired.len() >= MAX_SELECTED {
                    self.last_select_capped = true;
                } else {
                    self.desired.push(node);
                }
            }
        }
    }

    /// HARNESS-ONLY (the `!has_view()` stress path): uses `camera.y.abs()` as the viewing distance.
    /// Do not change this math — the deterministic stress tests depend on its byte-for-byte behavior.
    fn pick_level(&self, world: &World, camera: DVec3) -> u8 {
        let altitude = camera.y.abs().max(1.0);
        desired_level(
            world.base_geometric_error,
            altitude,
            self.view_fov_y_radians,
            self.view_viewport_height,
            self.view_sse_target_pixels,
            world.max_level,
        )
    }

    /// HARNESS-ONLY (the `!has_view()` stress path): the omnidirectional window used when no live camera
    /// basis is present. Do not change its math — the deterministic stress tests depend on it.
    fn add_desired_window(&mut self, world: &World, x: f64, z: f64, level: u8) {
        world.window(x, z, level, self.cfg.window_radius, &mut self.window_buf);
        for &id in &self.window_buf {
            if self.seen_desired.insert(id) {
                self.desired.push(id);
            }
        }
    }

    /// Advance one frame. `motion` is the per-frame camera delta (for predictive prefetch). Returns the
    /// renderable tile set (finest resident tile per desired leaf, with parent fallback).
    pub fn update(&mut self, world: &World, camera: DVec3, motion: DVec3) -> &[TileId] {
        self.tick += 1;
        self.telemetry.frames += 1;

        // 1. Ingest completed fetches into the bounded cache.
        self.provider.poll_completed(self.tick, &mut self.completed);
        for i in 0..self.completed.len() {
            let (id, bytes) = self.completed[i];
            self.cache.insert(id, Page { size_bytes: bytes }, bytes, &mut self.evicted);
            self.in_flight.remove(&id);
            self.states.insert(id, PageState::DecodedResident);
            self.telemetry.requests_completed += 1;
            for k in 0..self.evicted.len() {
                self.states.remove(&self.evicted[k]);
            }
        }

        // 2. Desired set. With a live camera basis: view-aware hierarchical selection (frustum +
        //    horizon + SSE traversal, §2.3). Without one (stress harness): the omnidirectional window
        //    + motion look-ahead. The gate keeps the deterministic stress harness byte-for-byte stable.
        self.desired.clear();
        self.seen_desired.clear();
        if self.has_view() {
            let aspect = if self.view_viewport_height > 0.0 {
                self.view_viewport_width / self.view_viewport_height
            } else {
                1.0
            };
            // Flat local world: near a small constant, far generous (SSE caps far detail, not the far
            // plane); horizon culling opts out until the globe endpoint. World is flat → y=0 plane.
            // FIX 4: far is camera-relative (origin-centered world) so an outside/orbital camera still
            // encloses the whole world; falls back to the old 2*size floor when the camera is inside.
            let world_center = DVec3::ZERO; // origin-centered world: center is the origin
            let world_diagonal = world.world_size * 1.5;
            let far = (camera.distance(world_center) + world_diagonal).max(world.world_size * 2.0);
            let frustum = Frustum::from_camera(
                camera,
                self.view_forward,
                self.view_up,
                self.view_fov_y_radians,
                aspect,
                0.5,
                far,
            );
            let horizon = HorizonCuller::flat();
            self.select_hierarchical(world, camera, &frustum, &horizon);
        } else {
            let level = self.pick_level(world, camera);
            self.add_desired_window(world, camera.x, camera.z, level);
            let ahead_x = camera.x + motion.x * self.cfg.prefetch_lead;
            let ahead_z = camera.z + motion.z * self.cfg.prefetch_lead;
            self.add_desired_window(world, ahead_x, ahead_z, level);
        }

        // 3a. Resolve the render set (finest resident per leaf, parent fallback) and collect missing
        //     candidates with a transparent priority (scheduler-design.md §6.3).
        let ctx = EvalContext {
            camera,
            motion,
            fov_y_radians: self.view_fov_y_radians,
            viewport_height: self.view_viewport_height,
            base_geometric_error: world.base_geometric_error,
        };
        self.renderable.clear();
        self.renderable_set.clear();
        self.candidates.clear();
        self.candidate_seen.clear();
        for i in 0..self.desired.len() {
            let leaf = self.desired[i];
            let mut finest_resident: Option<TileId> = None;

            for lvl in 0..=leaf.level {
                let anc = leaf.ancestor_at(lvl);
                if self.cache.contains(&anc) {
                    // FIX 2: don't promote here — promoting every resident ancestor pins coarse tiles MRU.
                    finest_resident = Some(anc);
                } else if !self.in_flight.contains(&anc) && self.candidate_seen.insert(anc) {
                    let is_fallback = finest_resident.is_none();
                    let (cx, cz) = world.tile_center(anc);
                    let terms = scheduler::evaluate(&ctx, cx, cz, anc, leaf, is_fallback);
                    let queue = scheduler::classify_queue(&terms, is_fallback);
                    self.candidates.push((anc, terms, queue));
                }
            }

            // FIX 2: promote only the finest resident tile so coarse ancestors stay evictable.
            if let Some(f) = finest_resident {
                self.cache.promote(&f);
            }

            if let Some(chosen) = finest_resident {
                if chosen != leaf {
                    self.telemetry.fallbacks_used += 1;
                }
                if self.renderable_set.insert(chosen) {
                    self.renderable.push(chosen);
                    self.states.insert(chosen, PageState::Visible);
                }
            }
        }

        // 3b. Issue requests in priority order, gated by in-flight + per-frame promote budgets (§6.4).
        // FIX 1: sort by queue class first (rank = `queue as usize`, CriticalNow=0 is most urgent), then
        // by descending score within a class, so critical fallbacks always issue before Speculative.
        self.candidates.sort_by(|a, b| {
            (a.2 as usize).cmp(&(b.2 as usize)).then_with(|| {
                b.1.total().partial_cmp(&a.1.total()).unwrap_or(std::cmp::Ordering::Equal)
            })
        });
        if let Some(top) = self.candidates.first() {
            self.last_top_terms = top.1;
        }
        self.queue_counts = [0; QUEUE_COUNT];
        let mut promoted = 0u32;
        for k in 0..self.candidates.len() {
            let (tile, _terms, queue) = self.candidates[k];
            if self.in_flight.len() >= self.budgets.max_inflight_requests as usize
                || promoted >= self.budgets.max_pages_promoted_per_frame
            {
                continue;
            }
            self.provider.request(tile, self.tick);
            self.in_flight.insert(tile);
            self.states.insert(tile, PageState::Requesting);
            self.queue_counts[queue as usize] += 1; // FIX 3: count after the budget gate = requests issued
            self.telemetry.requests_issued += 1;
            promoted += 1;
        }

        // 4. Telemetry.
        self.telemetry.resident_tiles = self.cache.len() as u32;
        self.telemetry.bytes_resident = self.cache.bytes_used() as u64;
        if self.telemetry.bytes_resident > self.telemetry.peak_bytes_resident {
            self.telemetry.peak_bytes_resident = self.telemetry.bytes_resident;
        }
        self.telemetry.cache_hits = self.cache.hits;
        self.telemetry.cache_misses = self.cache.misses;
        self.telemetry.evictions = self.cache.evictions;
        self.telemetry.in_flight = self.in_flight.len() as u32;

        &self.renderable
    }
}

#[cfg(test)]
mod view_tests {
    use super::*;

    fn engine() -> StreamEngine<SyntheticProvider> {
        StreamEngine::new(
            StreamConfig {
                budget_bytes: 64 * 1024 * 1024,
                max_in_flight: 64,
                window_radius: 6,
                sse_target_pixels: 2.0,
                fov_y_radians: 1.0,
                viewport_height: 1080.0,
                prefetch_lead: 8.0,
            },
            SyntheticProvider::new(2, 8),
        )
    }

    #[test]
    fn frustum_excludes_tiles_behind_camera() {
        let world = World::new(16384.0, 8, 4096.0);
        let mut eng = engine();
        // Look toward +X (slightly down) from above the center → −X is behind the camera.
        let forward = Vec3::new(1.0, -0.2, 0.0);
        eng.set_view(forward, Vec3::new(0.0, 1.0, 0.0), 1.0, 1920.0, 1080.0, 2.0);
        let camera = DVec3::new(0.0, 400.0, 0.0);
        let fwd = forward.as_dvec3().normalize();
        for _ in 0..30 {
            eng.update(&world, camera, DVec3::default());
        }
        let desired: Vec<TileId> = eng.desired().to_vec();
        assert!(!desired.is_empty(), "view-aware selection produced tiles");
        for t in desired {
            let (c, r) = world.tile_bounds(t);
            let ahead = (c - camera).dot(fwd);
            assert!(ahead >= -r - 1.0, "tile behind the camera leaked in: ahead={ahead}, radius={r}");
        }
    }

    #[test]
    fn omnidirectional_window_without_a_view() {
        // No set_view → the camera basis is zero → omnidirectional window (stress-harness behavior).
        let world = World::new(16384.0, 8, 4096.0);
        let mut eng = engine();
        let camera = DVec3::new(0.0, 400.0, 0.0);
        eng.update(&world, camera, DVec3::default());
        let desired = eng.desired();
        let any_plus = desired.iter().any(|&t| world.tile_bounds(t).0.x > camera.x);
        let any_minus = desired.iter().any(|&t| world.tile_bounds(t).0.x < camera.x);
        assert!(any_plus && any_minus, "window should surround the camera on all sides");
    }

    #[test]
    fn set_budgets_round_trip_is_idempotent() {
        // FIX L3: `set_budgets(budgets())` must not shrink the cache byte budget. A non-MB-aligned
        // budget previously got floored to a whole MB on every round-trip (re-derived from the rounded
        // `total_cpu_cache_mb`); the exact-bytes field keeps it stable.
        let bytes = 64 * 1024 * 1024 + 500 * 1024; // not a whole number of MB
        let cfg = StreamConfig {
            budget_bytes: bytes,
            max_in_flight: 64,
            window_radius: 6,
            sse_target_pixels: 2.0,
            fov_y_radians: 1.0,
            viewport_height: 1080.0,
            prefetch_lead: 8.0,
        };
        let mut eng = StreamEngine::new(cfg, SyntheticProvider::new(2, 8));
        assert_eq!(eng.cache().budget(), bytes, "engine starts at the exact byte budget");
        eng.set_budgets(eng.budgets());
        assert_eq!(eng.cache().budget(), bytes, "round-tripping budgets must not floor the byte budget");
        // Idempotent under repetition.
        eng.set_budgets(eng.budgets());
        assert_eq!(eng.cache().budget(), bytes);
    }

    #[test]
    fn selection_is_multi_resolution() {
        // A view-aware selection should mix levels (fine near, coarse far), not a single flat ring.
        let world = World::new(16384.0, 10, 4096.0);
        let mut eng = engine();
        eng.set_view(Vec3::new(0.2, -0.6, 1.0), Vec3::new(0.0, 1.0, 0.0), 1.0, 1920.0, 1080.0, 2.0);
        let camera = DVec3::new(0.0, 500.0, -2000.0);
        eng.update(&world, camera, DVec3::default());
        let levels: HashSet<u8> = eng.desired().iter().map(|t| t.level).collect();
        assert!(levels.len() >= 2, "expected mixed LOD levels, got {levels:?}");
    }
}
