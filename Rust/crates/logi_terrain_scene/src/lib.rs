//! The `LogiTerrainWorld` runtime object + the C-ABI frame-packet contract (runtime-model.md §3).
//!
//! `LogiTerrainWorld` wraps the streaming engine, owns a small frame ring, and assembles a [`LogiFramePacket`]
//! each `step`. Geometry is referenced by [`LogiBufferHandle`] and fetched zero-copy via a
//! [`LogiBufferView`] (the bytes stay Rust-owned). Field order in every `#[repr(C)]` struct is the
//! contract — mirror it byte-for-byte in `include/logi_terrain.h` and the Unity bindings.

use std::collections::{HashMap, HashSet};

use logi_terrain_core::{DVec3, LogiBoundingSphere, Telemetry, TileId, Vec3};
use logi_terrain_decode::height_mesh;
use logi_terrain_geo::{self as geo, MeshData};
use logi_terrain_procgen::{TerrainHeights, DEFAULT_AMPLITUDE_M};
use logi_terrain_stream::{StreamConfig, StreamEngine, SyntheticProvider, World as StreamWorld};

/// Index buffer element format.
pub mod index_format {
    pub const U16: u32 = 0;
    pub const U32: u32 = 1;
}

/// Buffer-view kind.
pub mod buffer_kind {
    pub const VERTEX: u32 = 0;
    pub const INDEX: u32 = 1;
}

pub type LogiFrameId = u64;
pub type LogiBufferHandle = u64;

#[inline]
fn tile_buffer_handle(tile: TileId, kind: u32) -> LogiBufferHandle {
    // FIX 6: the `<< 1` steals the top bit for `kind`; a key with bit 63 set would silently lose it
    // (and `buffer_view`'s `>> 1` could not recover the tile). Catch such keys in debug builds.
    debug_assert!(
        tile.key() < (1u64 << 63),
        "tile key {} overflows handle on << 1",
        tile.key()
    );
    (tile.key() << 1) | (kind as u64)
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct LogiCameraState {
    pub position: DVec3, // f64 world (planet-scale)
    pub forward: Vec3,   // f32 unit direction (↔ Unity float3)
    pub up: Vec3,        // f32 unit direction
    pub fov_y_radians: f32,
    pub viewport_width: f32,
    pub viewport_height: f32,
    pub sse_target_pixels: f32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct LogiStreamingBudget {
    pub memory_budget_bytes: u64,
    pub max_in_flight: u32,
    pub _reserved: u32,
}

impl LogiStreamingBudget {
    pub fn defaults() -> Self {
        Self {
            memory_budget_bytes: 64 * 1024 * 1024,
            max_in_flight: 512,
            _reserved: 0,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct LogiWorldCreateInfo {
    pub world_size: f64,
    pub budget: LogiStreamingBudget,
    pub max_level: u32,
    pub _reserved: u32,
}

impl LogiWorldCreateInfo {
    pub fn defaults() -> Self {
        // max_level is the close-up DETAIL CEILING: the finest tile is world_size / 2^max_level, with
        // HEIGHT_PAGE_RES (17) samples per edge. At 16384 m / 2^8 = 64 m tiles -> 4 m sample spacing —
        // ample from altitude, coarse for a ground-level view. Raising it adds finer tiles (more detail)
        // at the cost of more resident tiles/memory; lowering it is coarser + cheaper. Clamped to <= 20.
        Self {
            world_size: 16_384.0,
            budget: LogiStreamingBudget::defaults(),
            max_level: 8,
            _reserved: 0,
        }
    }
}

/// Floating-origin rebase. `origin` is the world point Unity should treat as its local zero.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct LogiLocalFrame {
    pub origin: DVec3, // f64 world point that Unity treats as its local zero
}

/// A zero-copy view into Rust-owned buffer memory (valid for the world's lifetime).
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct LogiBufferView {
    pub data: *const u8,
    pub byte_length: u64,
    pub element_stride: u32,
    pub kind: u32,
}

/// One terrain patch to draw. Geometry is referenced by handle (resolve with `acquire_buffer_view`).
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct LogiTerrainPatchDraw {
    pub patch_handle: u64,
    pub vertex_buffer: LogiBufferHandle,
    pub index_buffer: LogiBufferHandle,
    pub bounds: LogiBoundingSphere,
    pub local_position: Vec3, // f32, relative to local_frame.origin (↔ Unity float3) — no C# conversion
    pub geometric_error: f32,
    pub lod: u32,
    pub vertex_count: u32,
    pub vertex_stride: u32,
    pub index_count: u32,
    pub index_format: u32,
    pub material_id: u32,
    pub _reserved: u32,
}

/// A "page must be resident this frame" upload command (material/height pages).
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct LogiUploadCommand {
    pub page_handle: u64,
    pub data: *const u8,
    pub byte_length: u64,
    pub slot: u32,
    pub kind: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct LogiInstanceBatchDraw {
    pub mesh_handle: LogiBufferHandle,
    pub instance_data: *const u8,
    pub instance_count: u32,
    pub instance_stride: u32,
    pub material_id: u32,
    pub _reserved: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct LogiDecalBatchDraw {
    pub data: *const u8,
    pub count: u32,
    pub material_id: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct LogiColliderCommand {
    pub patch_handle: u64,
    pub data: *const u8,
    pub byte_length: u64,
    pub kind: u32,
    pub _reserved: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct LogiAttributionEntry {
    pub text: *const u8,
    pub text_len: u32,
    pub source_id: u32,
}

/// Streaming health/perf counters carried on every [`LogiFramePacket`].
///
/// FIX 3: these fields mix two semantics — a HUD must not treat them uniformly:
///
/// - CUMULATIVE-since-start (monotonic totals, only ever grow): `frames`, `cache_hits`, `cache_misses`,
///   `evictions`, `requests_issued`, `requests_completed`, `fallbacks_used`. For per-frame rates, diff
///   against the previous frame's value.
/// - INSTANTANEOUS gauges (current value this frame, can go up or down): `resident_tiles`,
///   `bytes_resident`, `in_flight`.
/// - `peak_bytes_resident` is a running HIGH-WATER MARK (max of `bytes_resident` over all frames) —
///   monotonic non-decreasing, but not a "since-start sum".
///
/// The C++ telemetry panel reads resident_tiles/in_flight/bytes (gauges) + peak (high-water) and
/// evictions/fallbacks/requests_completed (cumulative TOTALS, not per-frame).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct LogiTerrainTelemetry {
    pub frames: u64,
    pub bytes_resident: u64,
    pub peak_bytes_resident: u64,
    pub cache_hits: u64,
    pub cache_misses: u64,
    pub evictions: u64,
    pub requests_issued: u64,
    pub requests_completed: u64,
    pub fallbacks_used: u64,
    pub resident_tiles: u32,
    pub in_flight: u32,
}

impl From<Telemetry> for LogiTerrainTelemetry {
    fn from(t: Telemetry) -> Self {
        Self {
            frames: t.frames,
            bytes_resident: t.bytes_resident,
            peak_bytes_resident: t.peak_bytes_resident,
            cache_hits: t.cache_hits,
            cache_misses: t.cache_misses,
            evictions: t.evictions,
            requests_issued: t.requests_issued,
            requests_completed: t.requests_completed,
            fallbacks_used: t.fallbacks_used,
            resident_tiles: t.resident_tiles,
            in_flight: t.in_flight,
        }
    }
}

/// The per-frame packet — the only thing Unity needs each frame (pointers + counts, no Rust collections).
/// Pointer fields are grouped first (8-aligned), then counts — see `logi_terrain.h`.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct LogiFramePacket {
    pub frame_id: u64,
    pub local_frame: LogiLocalFrame,

    pub terrain_patches: *const LogiTerrainPatchDraw,
    pub material_page_uploads: *const LogiUploadCommand,
    pub height_page_uploads: *const LogiUploadCommand,
    pub instance_batches: *const LogiInstanceBatchDraw,
    pub decal_batches: *const LogiDecalBatchDraw,
    pub collider_commands: *const LogiColliderCommand,
    pub attributions: *const LogiAttributionEntry,

    pub terrain_patch_count: u32,
    pub material_page_upload_count: u32,
    pub height_page_upload_count: u32,
    pub instance_batch_count: u32,
    pub decal_batch_count: u32,
    pub collider_command_count: u32,
    pub attribution_count: u32,
    pub _pad: u32,

    pub telemetry: LogiTerrainTelemetry,
}

/// ABI layout locks — these sizes are the contract mirrored by `include/logi_terrain.h` and the Unity
/// bindings. A glam layout change (or a stray field edit) breaks the build here, not at runtime.
const _: () = {
    use std::mem::size_of;
    assert!(size_of::<LogiBoundingSphere>() == 16);
    assert!(size_of::<LogiCameraState>() == 64);
    assert!(size_of::<LogiLocalFrame>() == 24);
    assert!(size_of::<LogiTerrainPatchDraw>() == 88);
    // FIX 5: lock the pointer-bearing structs that actually cross the FFI (pointers = 8 bytes on the
    // win64 target). Computed against logi_terrain_ffi/include/logi_terrain_ffi.h (do not edit there).
    // LogiBufferView: ptr(8) + u64(8) + u32(4) + u32(4) = 24.
    assert!(size_of::<LogiBufferView>() == 24);
    // LogiFramePacket: u64(8) + LogiLocalFrame(24) + 7*ptr(56) + 8*u32(32) + LogiTerrainTelemetry(80) = 200.
    // (LogiTerrainTelemetry = 9*u64(72) + 2*u32(8) = 80; no tail padding anywhere, align 8.)
    assert!(size_of::<LogiTerrainTelemetry>() == 80);
    assert!(size_of::<LogiFramePacket>() == 200);
};

// ----------------------------------------------------------------------------------------------------

const VERTEX_STRIDE: u32 = geo::VERTEX_STRIDE as u32;
const HEIGHT_PAGE_RES: u16 = 17; // FIX 7: fixed N (=17) samples per tile edge regardless of level/size
                                 // Resident-tile budget. Must stay >= the kernel's MAX_SELECTED so a single frame's selection never
                                 // forces live-tile eviction. This is THE single resident cap for the whole pipeline: the C++ renderer's
                                 // MaxSections / MaxNaniteTiles MUST be >= this (it holds every tile the kernel keeps resident). Keep all
                                 // three (here, stream MAX_SELECTED, and LogiTerrainSubsystem.cpp) in sync — they describe one budget.
const MESH_CACHE_CAP: usize = 4096; // decoded per-tile meshes (count cap)
                                    // Byte budget for the decoded-mesh cache — bounds memory regardless of count, since tiles vary in
                                    // geometry (and skirts add ~50% indices). Evicted together with MESH_CACHE_CAP, whichever binds first.
const MESH_BYTE_BUDGET: usize = 128 * 1024 * 1024; // 128 MiB of decoded mesh
                                                   // Terrain relief (amplitude) is the SHARED canonical constant `logi_terrain_procgen::DEFAULT_AMPLITUDE_M`
                                                   // (250 m) — the SAME value the sim's gameplay sampler uses. Do not re-declare a local copy here.
                                                   // FIX 2: frame ring depth. Max in-flight frames the host may hold; it MUST release a frame within RING
                                                   // steps of receiving it, else step() reuses the slot and the host reads silently-stale data. The C++
                                                   // host releases each frame the SAME tick it acquires it (1 in flight), so 4 is ample headroom (was 8).
const RING: usize = 4;

/// A decoded, displaced per-tile mesh (procgen height page -> displaced grid). Unique per tile, so it
/// cannot be shared per-level like the old flat template.
struct TileMesh {
    vertices: Vec<u8>,
    indices: Vec<u8>,
    vertex_count: u32,
    index_count: u32,
    min_elevation: f32,
    max_elevation: f32,
    last_used: u64,
}

struct FrameSlot {
    frame_id: LogiFrameId,
    local_frame: LogiLocalFrame,
    patches: Vec<LogiTerrainPatchDraw>,
    telemetry: LogiTerrainTelemetry,
}

impl FrameSlot {
    fn new() -> Self {
        Self {
            frame_id: 0,
            local_frame: LogiLocalFrame::default(),
            patches: Vec::new(),
            telemetry: LogiTerrainTelemetry::default(),
        }
    }
}

/// The opaque runtime world object (runtime-model.md §3.1).
pub struct LogiTerrainWorld {
    stream_world: StreamWorld,
    engine: StreamEngine<SyntheticProvider>,
    heights: TerrainHeights,
    tile_meshes: HashMap<TileId, TileMesh>,
    // Running byte total of `tile_meshes` (sum of each mesh's vertices+indices len), maintained on
    // insert/remove so evict_meshes' steady-state guard is O(1) instead of re-summing every frame.
    mesh_bytes: usize,
    // Reused eviction scratch (no per-call alloc on the churn path): the pinned-tile set + the
    // sorted-by-age candidate list.
    evict_pinned: HashSet<TileId>,
    evict_candidates: Vec<(u64, TileId, usize)>,
    camera: LogiCameraState,
    prev_camera: Option<DVec3>,
    next_frame_id: LogiFrameId,
    renderable: Vec<TileId>,
    frames: Vec<FrameSlot>,
}

impl LogiTerrainWorld {
    pub fn new(info: LogiWorldCreateInfo) -> Self {
        let max_level = info.max_level.min(20) as u8;
        // base_geometric_error = the terrain relief: a root tile (flat) deviates from the true surface
        // by ~the amplitude, so that is the right coarsest-level error. Tie it to the SHARED
        // DEFAULT_AMPLITUDE_M (was world_size/64 = 256, which only coincided with the relief at this one
        // world size and would silently mis-tune the LOD pyramid if world_size changed).
        let mut stream_world = StreamWorld::new(info.world_size, max_level, DEFAULT_AMPLITUDE_M);
        stream_world.set_height_amplitude(DEFAULT_AMPLITUDE_M);

        let budget = if info.budget.memory_budget_bytes > 0 {
            info.budget.memory_budget_bytes as usize
        } else {
            64 * 1024 * 1024
        };
        let cfg = StreamConfig {
            budget_bytes: budget,
            max_in_flight: info.budget.max_in_flight.max(1) as usize,
            window_radius: 6,
            sse_target_pixels: 8.0, // refinement target across the whole view; 2 over-refined massively
            fov_y_radians: 60.0_f64.to_radians(),
            viewport_height: 1080.0,
            prefetch_lead: 6.0,
        };
        let engine = StreamEngine::new(cfg, SyntheticProvider::new(2, 8));

        // The CANONICAL shared field — identical to the sim's gameplay sampler (logi_sim Terrain::canonical),
        // so agents sit on the terrain that is drawn. Never hardcode a seed or amplitude here.
        let heights = TerrainHeights::canonical();

        Self {
            stream_world,
            engine,
            heights,
            tile_meshes: HashMap::new(),
            mesh_bytes: 0,
            evict_pinned: HashSet::new(),
            evict_candidates: Vec::new(),
            camera: LogiCameraState::default(),
            prev_camera: None,
            next_frame_id: 1,
            renderable: Vec::new(),
            frames: (0..RING).map(|_| FrameSlot::new()).collect(),
        }
    }

    pub fn set_camera(&mut self, camera: LogiCameraState) {
        self.camera = camera;
    }

    pub fn set_budget(&mut self, budget: LogiStreamingBudget) {
        self.engine.set_budget(
            budget.memory_budget_bytes as usize,
            budget.max_in_flight.max(1) as usize,
        );
    }

    /// Ensure a decoded, displaced mesh exists for `tile` (procgen height page -> displaced grid).
    /// Crack-free across same-level neighbours (edge-inclusive sampling). Cached; LRU-bounded by
    /// [`Self::evict_meshes`].
    fn ensure_mesh(&mut self, tile: TileId, frame: u64) {
        if let Some(m) = self.tile_meshes.get_mut(&tile) {
            m.last_used = frame;
            return;
        }
        let (cx, cz) = self.stream_world.tile_center(tile);
        let ts = self.stream_world.tile_size_at(tile.level);
        let page = self
            .heights
            .height_page(cx - ts * 0.5, cz - ts * 0.5, ts, HEIGHT_PAGE_RES);
        let (min_elevation, max_elevation) = (page.min_elevation, page.max_elevation);
        let MeshData {
            vertices,
            indices,
            vertex_count,
            index_count,
        } = height_mesh(&page, ts as f32);
        self.mesh_bytes += vertices.len() + indices.len(); // keep the running byte total in sync (see evict_meshes)
        self.tile_meshes.insert(
            tile,
            TileMesh {
                vertices,
                indices,
                vertex_count,
                index_count,
                min_elevation,
                max_elevation,
                last_used: frame,
            },
        );
    }

    /// Evict least-recently-used tile meshes when over the cap (current-frame tiles carry the newest
    /// stamp, so they survive). Allocates only on overflow, not in steady state.
    ///
    /// FIX 1: NEVER evict a mesh whose `last_used == self.next_frame_id - 1` — that is the frame just
    /// built, and its tiles are live in the patch list. Evicting one would make the patch loop hit
    /// `None => continue` and silently drop drawable geometry. The buffer_view lifetime depends on this:
    /// the C++ host holds zero-copy pointers into a tile's mesh bytes for the whole duration of the frame
    /// (resolved via `buffer_view`), so the bytes must outlive the frame. We therefore only trim
    /// strictly-older entries; if that is not enough to get under the cap, we leave the cap exceeded for
    /// this frame rather than freeing memory the host is still reading.
    ///
    /// FIX M4: the just-built frame is not the only one the host may still be reading. The frame ring
    /// holds up to `RING` (=4) un-released frames; each holds a `patches` list that references tile
    /// meshes by handle, and the host resolves those handles to zero-copy mesh pointers via
    /// `buffer_view` for as long as it holds the frame. Pinning only the current frame could evict a
    /// mesh that an *outstanding* (older, still-unreleased) ring frame references, dangling the host's
    /// pointer. So we pin the union of tiles referenced by EVERY live ring slot (`frame_id != 0`) — in
    /// addition to the current-frame stamp pin — and never evict any of them.
    fn evict_meshes(&mut self) {
        // O(1) steady-state guard: `mesh_bytes` is maintained incrementally on insert/remove (mirroring
        // logi_terrain_cache::ResidencyCache::bytes_used) instead of re-summing every resident mesh each
        // frame. Bounded by BOTH the count cap and the byte budget; whichever binds first.
        if self.tile_meshes.len() <= MESH_CACHE_CAP && self.mesh_bytes <= MESH_BYTE_BUDGET {
            return;
        }
        let live = self.next_frame_id.wrapping_sub(1); // frame just built; its tiles are pinned
                                                       // Tiles referenced by any still-live ring frame: the host may hold zero-copy pointers into their
                                                       // mesh bytes until it releases that frame, so they must outlive eviction (FIX M4). Reuse the
                                                       // scratch set — no per-call allocation on the churn path.
        self.evict_pinned.clear();
        for slot in &self.frames {
            if slot.frame_id == 0 {
                continue; // released/empty ring slot
            }
            for patch in &slot.patches {
                // `patch_handle` is the tile's `key()` (see `step`). from_key masks to 62 bits, so a
                // handle that ever carried high bits (e.g. the bit-63-stealing buffer handle) would pin
                // the WRONG tile — assert the clean round-trip so future encoding drift trips in debug.
                debug_assert!(
                    TileId::from_key(patch.patch_handle).key() == patch.patch_handle,
                    "patch_handle is not a clean TileId::key() round-trip",
                );
                self.evict_pinned.insert(TileId::from_key(patch.patch_handle));
            }
        }
        // Candidates = un-pinned, strictly-older entries (carry each one's byte size for the byte budget).
        // mem::take the scratch Vec so the filter closure can borrow the other self fields; restore it
        // (capacity preserved → reused next call) at the end.
        let mut candidates = std::mem::take(&mut self.evict_candidates);
        candidates.clear();
        candidates.extend(
            self.tile_meshes
                .iter()
                .filter(|(t, m)| m.last_used != live && !self.evict_pinned.contains(t))
                .map(|(t, m)| (m.last_used, *t, m.vertices.len() + m.indices.len())),
        );
        candidates.sort_unstable_by_key(|(lu, _, _)| *lu);
        // Evict oldest-first until BOTH budgets are met (or only pinned/live entries remain — then a budget
        // can stay exceeded this frame, intentional: never drop a mesh the host is still/maybe reading).
        let mut count = self.tile_meshes.len();
        let mut bytes = self.mesh_bytes;
        for &(_, t, sz) in &candidates {
            if count <= MESH_CACHE_CAP && bytes <= MESH_BYTE_BUDGET {
                break;
            }
            self.tile_meshes.remove(&t);
            count -= 1;
            bytes = bytes.saturating_sub(sz);
        }
        self.mesh_bytes = bytes;
        // Observability (audit): if we exit STILL over budget it must be because every remaining mesh is
        // pinned by a live ring frame the host may be reading (FIX M4) or is the just-built frame (FIX 1) —
        // never because the loop quit early with an evictable (un-pinned, non-live) mesh left. This makes
        // the intentional over-cap distinguishable from an eviction-logic regression in tests, instead of
        // the single pipeline-wide resident cap being silently exceeded.
        debug_assert!(
            (self.tile_meshes.len() <= MESH_CACHE_CAP && self.mesh_bytes <= MESH_BYTE_BUDGET)
                || self
                    .tile_meshes
                    .iter()
                    .all(|(t, m)| m.last_used == live || self.evict_pinned.contains(t)),
            "evict_meshes left the mesh cache over budget while an evictable (un-pinned, non-live) mesh remained"
        );
        self.evict_candidates = candidates; // restore the scratch (now with capacity) for reuse
    }

    /// Advance one frame; returns the new frame id (assembled into the ring).
    pub fn step(&mut self, _delta_seconds: f64) -> LogiFrameId {
        let camera_pos = self.camera.position;
        let motion = match self.prev_camera {
            Some(p) => camera_pos - p,
            None => DVec3::ZERO,
        };
        self.prev_camera = Some(camera_pos);

        // Drive selection from the LIVE camera: frustum basis (forward/up) + FOV / viewport / user SSE.
        // This feeds both the LOD picker and the view-aware frustum/horizon/SSE traversal (§2.2, §2.3).
        self.engine.set_view(
            self.camera.forward,
            self.camera.up,
            self.camera.fov_y_radians as f64,
            self.camera.viewport_width as f64,
            self.camera.viewport_height as f64,
            self.camera.sse_target_pixels as f64,
        );
        self.renderable.clear();
        {
            let r = self.engine.update(&self.stream_world, camera_pos, motion);
            self.renderable.extend_from_slice(r);
        }
        let frame_id = self.next_frame_id;
        self.next_frame_id += 1;
        let idx = (frame_id as usize) % RING;

        // Decode a displaced mesh per renderable tile (cached, LRU-bounded).
        for i in 0..self.renderable.len() {
            self.ensure_mesh(self.renderable[i], frame_id);
        }
        self.evict_meshes();

        // FIX 4: removed obsolete "origin stays 0 for now" paragraph — rebase is active (see below).
        // Floating-origin rebase (now ACTIVE): snap to a coarse grid near the camera so Unreal renders the
        // proc-mesh near its host actor with small float coords. Absolute planetary world coords (km from
        // origin) give huge bounds + LWC precision loss and the mesh fails to draw. The C++ side places the
        // host actor at this origin and emits vertices relative to it; on a grid crossing it rebuilds.
        // Terrain rebase grid: 2048 m. Deliberately COARSE — terrain tiles are large and their f32
        // vertices tolerate a far origin, so a wide grid means rare host-actor moves + section rebuilds.
        // (Contrast the AGENT rebase grid, logi_render_extract::ORIGIN_GRID_CM = 256 m, which is FINE
        // because small per-agent transforms need tight f32 precision near the camera. Different needs,
        // different grids — intentional, not an inconsistency.)
        const REBASE_SNAP: f64 = 2048.0;
        let origin = DVec3::new(
            (camera_pos.x / REBASE_SNAP).round() * REBASE_SNAP,
            0.0,
            (camera_pos.z / REBASE_SNAP).round() * REBASE_SNAP,
        );
        self.frames[idx].frame_id = frame_id;
        self.frames[idx].local_frame = LogiLocalFrame { origin };
        self.frames[idx].patches.clear();

        for i in 0..self.renderable.len() {
            let tile = self.renderable[i];
            let (cx, cz) = self.stream_world.tile_center(tile);
            let ts = self.stream_world.tile_size_at(tile.level);
            // FIX L1: overflow-safe geometric-error halving (matches logi_terrain_lod FIX 5) —
            // `1u64 << level` would overflow past level 63; `0.5^level` cannot.
            let ge = self.stream_world.base_geometric_error * 0.5f64.powi(tile.level as i32);
            let m = match self.tile_meshes.get(&tile) {
                Some(m) => m,
                None => continue,
            };
            // Camera-relative rebase happens HERE, in Rust (f64 → f32), so Unity gets a ready-to-use float3.
            let local = (DVec3::new(cx, 0.0, cz) - origin).as_vec3();
            // Bounds enclose the tile's actual relief, centered at its mean elevation.
            let mean_y = (m.min_elevation + m.max_elevation) * 0.5;
            let half_v = ((m.max_elevation - m.min_elevation) * 0.5).max(0.0);
            let horiz = (ts * std::f64::consts::FRAC_1_SQRT_2) as f32;
            let patch = LogiTerrainPatchDraw {
                patch_handle: tile.key(),
                // Per-tile geometry handle: displaced terrain can't share a per-level template mesh.
                vertex_buffer: tile_buffer_handle(tile, buffer_kind::VERTEX),
                index_buffer: tile_buffer_handle(tile, buffer_kind::INDEX),
                bounds: LogiBoundingSphere {
                    center: local + Vec3::new(0.0, mean_y, 0.0),
                    radius: (horiz * horiz + half_v * half_v).sqrt(),
                },
                local_position: local,
                geometric_error: ge as f32,
                lod: tile.level as u32,
                vertex_count: m.vertex_count,
                vertex_stride: VERTEX_STRIDE,
                index_count: m.index_count,
                index_format: index_format::U32,
                material_id: 0,
                _reserved: 0,
            };
            self.frames[idx].patches.push(patch);
        }

        self.frames[idx].telemetry = LogiTerrainTelemetry::from(self.engine.telemetry());
        frame_id
    }

    /// Build the frame packet for `frame_id` (must be a live frame in the ring).
    pub fn frame_packet(&self, frame_id: LogiFrameId) -> Option<LogiFramePacket> {
        let idx = (frame_id as usize) % RING;
        let slot = &self.frames[idx];
        if slot.frame_id != frame_id || frame_id == 0 {
            return None;
        }
        Some(LogiFramePacket {
            frame_id,
            local_frame: slot.local_frame,
            terrain_patches: slot.patches.as_ptr(),
            material_page_uploads: std::ptr::null(),
            height_page_uploads: std::ptr::null(),
            instance_batches: std::ptr::null(),
            decal_batches: std::ptr::null(),
            collider_commands: std::ptr::null(),
            attributions: std::ptr::null(),
            terrain_patch_count: slot.patches.len() as u32,
            material_page_upload_count: 0,
            height_page_upload_count: 0,
            instance_batch_count: 0,
            decal_batch_count: 0,
            collider_command_count: 0,
            attribution_count: 0,
            _pad: 0,
            telemetry: slot.telemetry,
        })
    }

    /// Resolve a buffer handle to a zero-copy view of Rust-owned bytes.
    pub fn buffer_view(&self, handle: LogiBufferHandle) -> Option<LogiBufferView> {
        let tile = TileId::from_key(handle >> 1);
        let kind = (handle & 1) as u32;
        let mesh = self.tile_meshes.get(&tile)?;
        Some(if kind == buffer_kind::VERTEX {
            LogiBufferView {
                data: mesh.vertices.as_ptr(),
                byte_length: mesh.vertices.len() as u64,
                element_stride: VERTEX_STRIDE,
                kind: buffer_kind::VERTEX,
            }
        } else {
            LogiBufferView {
                data: mesh.indices.as_ptr(),
                byte_length: mesh.indices.len() as u64,
                element_stride: 4,
                kind: buffer_kind::INDEX,
            }
        })
    }

    /// Release a frame (frees its ring slot for reuse).
    pub fn release_frame(&mut self, frame_id: LogiFrameId) {
        let idx = (frame_id as usize) % RING;
        if self.frames[idx].frame_id == frame_id {
            self.frames[idx].frame_id = 0;
        }
    }
}
