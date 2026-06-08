//! C ABI for the **baked semantic-tile world** (ADR-0011, Milestone HE-0) and the procedural terrain lane.
//!
//! Mirrors the globe API's shape: open a baked store (or build a procedural pyramid), then hand back each
//! tile's mesh in Unreal render space (cm, Z-up LH) as **zero-copy** views into Rust-owned memory, valid
//! until `destroy`. `select` returns the SSE-LOD tile set for the camera; the C++ side caches one proc-mesh
//! section per `tile_key`.
//!
//! No Unreal types cross; Rust never calls Unreal (AGENTS.md §1). Errors are status codes / null — no
//! panic unwinds across the boundary.

use crate::ffi::{guard_i32, guard_ptr, set_last_error, LogiStatus};
use logi_geospatial::{Cartographic, Ellipsoid};
use logi_semantic::{BakedWorld, MeshingMode, TerrainHeights, VERTEX_STRIDE};
use std::ffi::{c_char, CStr};

// The baked lane returns raw `i32` codes (not [`LogiStatus`]), but the *values* have a single source of
// truth: the `LogiStatus` enum in `ffi.rs` (whose discriminants the header mirrors as `LOGI_GEO_*`). We
// derive the three the baked lane uses from it, so they can never drift from the enum or the header.
const LOGI_OK: i32 = LogiStatus::Ok as i32;
const LOGI_NULL_ARG: i32 = LogiStatus::NullArg as i32;
/// A Rust panic was caught at this entry point (mapped here instead of unwinding across the C ABI — UB).
const LOGI_PANIC: i32 = LogiStatus::Panic as i32;

/// One baked tile for the renderer: a 32-byte-vertex mesh (`pos f3 | normal f3 | uv f2`, Unreal cm) +
/// `u32` indices, both zero-copy views valid until [`logi_baked_world_destroy`].
#[repr(C)]
#[derive(Clone, Copy)]
pub struct LogiBakedTile {
    /// Stable key (`TileId::key`) — the C++ section-cache key.
    pub tile_key: u64,
    pub vertices: *const u8,
    pub vertices_len: u64,
    pub vertex_count: u32,
    pub vertex_stride: u32, // == 32
    pub indices: *const u8, // index_count little-endian u32s
    pub index_count: u32,
    /// Pyramid level / refinement depth (root = 0; higher = more refined). Debug-viz / telemetry only —
    /// the renderer ignores it. (Occupies the former `_pad` slot, so the head layout is unchanged.)
    pub depth: u32,
    pub bounds_center: [f32; 3], // Unreal render space (cm)
    pub bounds_radius: f32,
    /// 3D Tiles geometric error at this tile (metres; lower = finer). Debug-viz / telemetry only.
    pub geometric_error: f32,
}

/// One frame's render list (HE-0: the whole AOI). `tiles` points into the world's owned array, valid
/// until `destroy`; `origin_ecef` is the ENU origin's WGS84 ECEF (for globe / sky-atmosphere placement).
#[repr(C)]
pub struct LogiBakedFrame {
    pub tiles: *const LogiBakedTile,
    pub tile_count: u32,
    pub _pad: u32,
    pub origin_ecef: [f64; 3],
}

/// Opaque handle: owns the [`BakedWorld`] plus the prebuilt `LogiBakedTile` array whose pointers reference
/// the world's mesh buffers (stable for the handle's lifetime — `Vec` data does not move when the handle
/// is boxed/moved).
pub struct LogiBakedWorld {
    world: BakedWorld,
    tiles: Vec<LogiBakedTile>,
    /// The selected view into `tiles` (rebuilt each `logi_baked_world_select`).
    frame_scratch: Vec<LogiBakedTile>,
    /// Reused membership set for `logi_baked_world_select` — cleared and refilled each call so the SSE
    /// path allocates no fresh `HashSet` per frame (M6).
    select_scratch: hashbrown::HashSet<u64>,
}

/// Creation parameters — the **meshing toggle** + skirt. `meshing_mode`: 0 = uniform grid, 1 = adaptive
/// TIN (using `tin_max_error_m`, metres). `skirt_depth_m` (0 = none) adds a downward edge wall hiding
/// cross-tile cracks. A null `info` defaults to uniform, no skirt.
#[repr(C)]
pub struct LogiBakedCreateInfo {
    pub meshing_mode: u32,
    pub tin_max_error_m: f32,
    pub skirt_depth_m: f32,
}

/// Derive the meshing mode from an already-read (aligned, local) create-info value.
fn meshing_mode_from(info: &LogiBakedCreateInfo) -> MeshingMode {
    match info.meshing_mode {
        1 => MeshingMode::Tin {
            max_error_m: info.tin_max_error_m.max(0.0),
        },
        _ => MeshingMode::Uniform,
    }
}

/// Open a baked tile store at `store_dir_utf8` and build every tile's render mesh with the meshing mode in
/// `info` (null → uniform). Returns null on a null/non-UTF-8 path or an open/decode failure (call off the
/// render thread — it reads + triangulates).
///
/// # Safety
/// `store_dir_utf8` must be a valid NUL-terminated UTF-8 C string (or null); `info` must be null or point
/// to a valid `LogiBakedCreateInfo`.
#[no_mangle]
pub unsafe extern "C" fn logi_baked_world_create(
    store_dir_utf8: *const c_char,
    info: *const LogiBakedCreateInfo,
) -> *mut LogiBakedWorld {
    // CR1: a panic anywhere below (open/decode/triangulate) becomes null, never an unwind across the ABI.
    guard_ptr(|| {
        if store_dir_utf8.is_null() {
            set_last_error("logi_baked_world_create: null store_dir");
            return std::ptr::null_mut();
        }
        let dir = match unsafe { CStr::from_ptr(store_dir_utf8) }.to_str() {
            Ok(s) => s,
            Err(_) => {
                set_last_error("logi_baked_world_create: store_dir is not valid UTF-8");
                return std::ptr::null_mut();
            }
        };
        // M3/M4: read the C-supplied struct ONCE, unaligned, into an aligned local (the globe/building
        // lanes' discipline) — never deref a possibly-unaligned `*const` field-by-field.
        let info = if info.is_null() {
            None
        } else {
            Some(unsafe { info.read_unaligned() })
        };
        let mode = info.as_ref().map(meshing_mode_from).unwrap_or(MeshingMode::Uniform);
        let skirt_depth_m = info.as_ref().map(|i| i.skirt_depth_m.max(0.0)).unwrap_or(0.0);
        let world = match BakedWorld::open_with(dir, mode, skirt_depth_m) {
            Ok(w) => w,
            Err(e) => {
                set_last_error(format!("logi_baked_world_create: {e:?}"));
                return std::ptr::null_mut();
            }
        };
        let tiles = build_tile_views(&world);
        Box::into_raw(Box::new(LogiBakedWorld {
            world,
            tiles,
            frame_scratch: Vec::new(),
            select_scratch: hashbrown::HashSet::new(),
        }))
    })
}

/// Build the zero-copy `LogiBakedTile` view array for a world's meshes (pointers reference the world's
/// owned buffers, stable for the handle's lifetime). Shared by the store + procedural create paths.
fn build_tile_views(world: &BakedWorld) -> Vec<LogiBakedTile> {
    world
        .tiles()
        .iter()
        .map(|t| LogiBakedTile {
            tile_key: t.id.key(),
            vertices: t.mesh.vertices.as_ptr(),
            vertices_len: t.mesh.vertices.len() as u64,
            vertex_count: t.mesh.vertex_count() as u32,
            vertex_stride: VERTEX_STRIDE as u32,
            indices: t.mesh.indices.as_ptr() as *const u8,
            index_count: t.mesh.indices.len() as u32,
            depth: t.id.level as u32,
            bounds_center: t.mesh.bounds_center,
            bounds_radius: t.mesh.bounds_radius,
            geometric_error: t.geometric_error,
        })
        .collect()
}

/// Procedural-world creation parameters: build the terrain from the canonical fractal field (the surface
/// gameplay shares) instead of a baked store. `seed == 0` ⇒ the canonical gameplay field; else a custom
/// world. The AOI is the `2·radius_m` square at the origin (lon/lat degrees, height metres), a pyramid of
/// levels `0..=leaf_level` at `grid_n²` samples/tile. Meshing fields mirror [`LogiBakedCreateInfo`].
#[repr(C)]
pub struct LogiBakedProceduralInfo {
    pub seed: u64,
    pub origin_lon_deg: f64,
    pub origin_lat_deg: f64,
    pub origin_height_m: f64,
    pub radius_m: f64,
    pub leaf_level: u32,
    pub grid_n: u32,
    pub meshing_mode: u32,
    pub tin_max_error_m: f32,
    pub skirt_depth_m: f32,
}

// Compile-time ABI locks (M3) for the baked lane — the same discipline the globe lane has in `ffi.rs`.
// `include/logi_geo_ffi.h` is HAND-MAINTAINED (NOT cbindgen-generated), and the C++ side reads these with
// `read_unaligned` into the matching header structs, so a silent size *or* field-offset drift would
// mis-decode every frame and corrupt memory. We lock BOTH the total size AND every field's byte offset via
// `core::mem::offset_of!`; the expected offsets are the `#[repr(C)]` layout below, cross-checked
// field-by-field against the header's struct order.
use core::mem::offset_of;
const _: () = {
    // LogiBakedTile — u64 + ptr/u64/u32 + depth(u32) + [f32;3]/f32 + geometric_error(f32) (align 8), size 72
    // (was 64 before v7: `depth` reuses the old `_pad` slot, `geometric_error` is a new f32 tail). Same head
    // as LogiGlobeTile but no texture fields (baked tiles carry no per-tile texture).
    assert!(std::mem::size_of::<LogiBakedTile>() == 72);
    assert!(offset_of!(LogiBakedTile, tile_key) == 0);
    assert!(offset_of!(LogiBakedTile, vertices) == 8);
    assert!(offset_of!(LogiBakedTile, vertices_len) == 16);
    assert!(offset_of!(LogiBakedTile, vertex_count) == 24);
    assert!(offset_of!(LogiBakedTile, vertex_stride) == 28);
    assert!(offset_of!(LogiBakedTile, indices) == 32);
    assert!(offset_of!(LogiBakedTile, index_count) == 40);
    assert!(offset_of!(LogiBakedTile, depth) == 44);
    assert!(offset_of!(LogiBakedTile, bounds_center) == 48);
    assert!(offset_of!(LogiBakedTile, bounds_radius) == 60);
    assert!(offset_of!(LogiBakedTile, geometric_error) == 64);

    // LogiBakedFrame — ptr + 2 × u32 + [f64;3] (align 8), size 40.
    assert!(std::mem::size_of::<LogiBakedFrame>() == 40);
    assert!(offset_of!(LogiBakedFrame, tiles) == 0);
    assert!(offset_of!(LogiBakedFrame, tile_count) == 8);
    assert!(offset_of!(LogiBakedFrame, _pad) == 12);
    assert!(offset_of!(LogiBakedFrame, origin_ecef) == 16);

    // LogiBakedCreateInfo — u32 + 2 × f32 (align 4), size 12.
    assert!(std::mem::size_of::<LogiBakedCreateInfo>() == 12);
    assert!(offset_of!(LogiBakedCreateInfo, meshing_mode) == 0);
    assert!(offset_of!(LogiBakedCreateInfo, tin_max_error_m) == 4);
    assert!(offset_of!(LogiBakedCreateInfo, skirt_depth_m) == 8);

    // LogiBakedProceduralInfo — u64 + 4 × f64 + 3 × u32 + 2 × f32 (align 8), size 64 (60 bytes + 4 tail pad).
    assert!(std::mem::size_of::<LogiBakedProceduralInfo>() == 64);
    assert!(offset_of!(LogiBakedProceduralInfo, seed) == 0);
    assert!(offset_of!(LogiBakedProceduralInfo, origin_lon_deg) == 8);
    assert!(offset_of!(LogiBakedProceduralInfo, origin_lat_deg) == 16);
    assert!(offset_of!(LogiBakedProceduralInfo, origin_height_m) == 24);
    assert!(offset_of!(LogiBakedProceduralInfo, radius_m) == 32);
    assert!(offset_of!(LogiBakedProceduralInfo, leaf_level) == 40);
    assert!(offset_of!(LogiBakedProceduralInfo, grid_n) == 44);
    assert!(offset_of!(LogiBakedProceduralInfo, meshing_mode) == 48);
    assert!(offset_of!(LogiBakedProceduralInfo, tin_max_error_m) == 52);
    assert!(offset_of!(LogiBakedProceduralInfo, skirt_depth_m) == 56);
};

/// Create a **procedural** terrain world (no store on disk): generate the SSE-LOD pyramid from the
/// canonical fractal field — the one-engine procedural lane. The handle is otherwise identical to a
/// store-backed one (same `select`/`destroy`). Returns null on a null `info` or a build failure. Blocking
/// (meshes the whole bounded pyramid) — call off the render thread.
///
/// # Safety
/// `info` must be null or point to a valid `LogiBakedProceduralInfo`.
#[no_mangle]
pub unsafe extern "C" fn logi_baked_world_create_procedural(
    info: *const LogiBakedProceduralInfo,
) -> *mut LogiBakedWorld {
    // CR1: a panic in the procedural mesh build becomes null, never an unwind across the ABI.
    guard_ptr(|| {
        if info.is_null() {
            set_last_error("logi_baked_world_create_procedural: null info");
            return std::ptr::null_mut();
        }
        // M3/M4: read the C-supplied struct ONCE, unaligned, into an aligned local.
        let info = unsafe { info.read_unaligned() };
        // seed 0 → the canonical field gameplay stands on, so the rendered terrain matches the sim surface.
        let field = if info.seed == 0 {
            TerrainHeights::canonical()
        } else {
            TerrainHeights::new(info.seed)
        };
        let c = Ellipsoid::wgs84().cartographic_to_cartesian(Cartographic::from_degrees(
            info.origin_lon_deg,
            info.origin_lat_deg,
            info.origin_height_m,
        ));
        let mode = match info.meshing_mode {
            1 => MeshingMode::Tin {
                max_error_m: info.tin_max_error_m.max(0.0),
            },
            _ => MeshingMode::Uniform,
        };
        let leaf_level = info.leaf_level.min(10) as u8; // bounded-eager: cap the pyramid depth
        let grid_n = info.grid_n.clamp(2, 513);
        let world = match BakedWorld::procedural(
            &field,
            [c.x, c.y, c.z],
            info.radius_m.max(1.0),
            leaf_level,
            grid_n,
            mode,
            info.skirt_depth_m.max(0.0),
        ) {
            Ok(w) => w,
            Err(e) => {
                set_last_error(format!("logi_baked_world_create_procedural: {e:?}"));
                return std::ptr::null_mut();
            }
        };
        let tiles = build_tile_views(&world);
        Box::into_raw(Box::new(LogiBakedWorld {
            world,
            tiles,
            frame_scratch: Vec::new(),
            select_scratch: hashbrown::HashSet::new(),
        }))
    })
}

/// SSE-LOD frame: selects the coarse-far / fine-near tile set by descending the baked **pyramid** and
/// refining by screen-space error (reusing the kernel's `logi_geo_lod`). `fov_y_radians`,
/// `viewport_height`, and `sse_target_px` drive the LOD; a single-level store renders everything. The
/// returned tiles point into owned buffers, valid until `destroy` or the next `select` on this world.
///
/// **Render-only — not for replay.** `BakedWorld::select` carries per-frame hysteresis state, so its
/// output is a function of the *camera history*, not the camera alone; never feed it into a deterministic
/// snapshot/replay path (use the gameplay field in `logi_sim` for that).
///
/// # Safety
/// `world` must be a live handle from [`logi_baked_world_create`]; `out` a valid writable pointer.
#[no_mangle]
pub unsafe extern "C" fn logi_baked_world_select(
    world: *mut LogiBakedWorld,
    camera_x_cm: f32,
    camera_y_cm: f32,
    camera_z_cm: f32,
    fov_y_radians: f32,
    viewport_height: f32,
    sse_target_px: f32,
    out: *mut LogiBakedFrame,
) -> i32 {
    // CR1: panic → LOGI_PANIC instead of unwinding across the C ABI.
    guard_i32(LOGI_PANIC, || {
        if world.is_null() || out.is_null() {
            return LOGI_NULL_ARG;
        }
        let handle = unsafe { &mut *world };
        // `select` returns an owned Vec (its borrow of `handle.world` ends here); fold its keys into the
        // reused `select_scratch` membership set so this frame allocates no fresh HashSet (M6).
        let selected_ids = handle.world.select(
            [camera_x_cm, camera_y_cm, camera_z_cm],
            fov_y_radians as f64,
            viewport_height as f64,
            sse_target_px as f64,
        );
        handle.select_scratch.clear();
        handle.select_scratch.extend(selected_ids.iter().map(|t| t.key()));
        handle.frame_scratch.clear();
        for t in &handle.tiles {
            if handle.select_scratch.contains(&t.tile_key) {
                handle.frame_scratch.push(*t);
            }
        }
        let frame = LogiBakedFrame {
            tiles: handle.frame_scratch.as_ptr(),
            tile_count: handle.frame_scratch.len() as u32,
            _pad: 0,
            origin_ecef: handle.world.origin_ecef(),
        };
        unsafe { out.write_unaligned(frame) };
        LOGI_OK
    })
}

/// Destroy a baked world (frees its meshes; any previously returned frame pointers dangle afterwards).
///
/// # Safety
/// `world` must be a handle from [`logi_baked_world_create`], not destroyed before, and not used after.
#[no_mangle]
pub unsafe extern "C" fn logi_baked_world_destroy(world: *mut LogiBakedWorld) {
    if world.is_null() {
        return;
    }
    // CR1: a panic in a destructor must not unwind across the ABI (mirrors the globe/building destroy).
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        drop(Box::from_raw(world));
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn null_args_are_handled() {
        // create(null) → null; select(null,..) → NULL_ARG; destroy(null) → no-op.
        assert!(unsafe { logi_baked_world_create(std::ptr::null(), std::ptr::null()) }.is_null());
        let mut frame = LogiBakedFrame {
            tiles: std::ptr::null(),
            tile_count: 0,
            _pad: 0,
            origin_ecef: [0.0; 3],
        };
        assert_eq!(
            unsafe { logi_baked_world_select(std::ptr::null_mut(), 0.0, 0.0, 0.0, 1.0, 1080.0, 16.0, &mut frame) },
            LOGI_NULL_ARG
        );
        assert!(unsafe { logi_baked_world_create_procedural(std::ptr::null()) }.is_null());
        unsafe { logi_baked_world_destroy(std::ptr::null_mut()) };
    }

    /// CR1: a panic inside a baked entry point must be caught and mapped to a status code / null — never
    /// allowed to unwind across the C ABI (which is UB and would abort the test runner). We exercise the
    /// shared guards that wrap every baked entry point with a closure that panics.
    #[test]
    fn panic_in_a_baked_path_returns_a_code_not_an_abort() {
        // Status-returning lanes (select) → LOGI_PANIC.
        let code = guard_i32(LOGI_PANIC, || panic!("boom in a baked status path"));
        assert_eq!(code, LOGI_PANIC);
        // Pointer-returning lanes (create/_create_procedural) → null.
        let ptr: *mut LogiBakedWorld = guard_ptr(|| panic!("boom in a baked create path"));
        assert!(ptr.is_null());
    }

    /// L8 + H5: the procedural lane builds a real world end-to-end through the guarded ABI, and every tile
    /// it hands back has `vertices_len == vertex_count * VERTEX_STRIDE` (so a C++ GPU upload can't run OOB).
    #[test]
    fn procedural_world_roundtrip_has_consistent_vertex_layout() {
        let info = LogiBakedProceduralInfo {
            seed: 0, // canonical field
            origin_lon_deg: -73.9857,
            origin_lat_deg: 40.7484,
            origin_height_m: 0.0,
            radius_m: 500.0,
            leaf_level: 1,
            grid_n: 8,
            meshing_mode: 0, // uniform
            tin_max_error_m: 0.0,
            skirt_depth_m: 0.0,
        };
        let world = unsafe { logi_baked_world_create_procedural(&info) };
        assert!(!world.is_null(), "procedural build should succeed");

        // `select` with a tiny SSE target refines to the whole AOI; every tile's buffer length must match
        // count * stride.
        let mut frame: LogiBakedFrame = unsafe { std::mem::zeroed() };
        assert_eq!(
            unsafe { logi_baked_world_select(world, 0.0, 0.0, 0.0, 60f32.to_radians(), 1080.0, 1.0, &mut frame) },
            LOGI_OK
        );
        assert!(frame.tile_count > 0, "procedural world should emit tiles");
        assert!(!frame.tiles.is_null());
        let tiles = unsafe { std::slice::from_raw_parts(frame.tiles, frame.tile_count as usize) };
        for t in tiles {
            assert_eq!(t.vertex_stride, VERTEX_STRIDE as u32);
            assert_eq!(
                t.vertices_len,
                t.vertex_count as u64 * VERTEX_STRIDE as u64,
                "vertices_len must equal vertex_count * stride"
            );
        }

        unsafe { logi_baked_world_destroy(world) };
    }
}
