//! Additive C ABI for the **baked semantic-tile world** (ADR-0011, Milestone HE-0).
//!
//! Mirrors the globe API's shape: open a baked store, then hand back every tile's mesh in Unreal render
//! space (cm, Z-up LH) as **zero-copy** views into Rust-owned memory, valid until `destroy`. HE-0 serves
//! the whole AOI statically (no camera-driven streaming yet — that lands in a later HE), so a single
//! `get_frame` is enough; the C++ side caches one proc-mesh section per `tile_key`.
//!
//! No Unreal types cross; Rust never calls Unreal (AGENTS.md §1). Errors are status codes / null — no
//! panic unwinds across the boundary.

use logi_geospatial::{Cartographic, Ellipsoid};
use logi_semantic::{within_cull_distance, BakedWorld, MeshingMode, TerrainHeights, VERTEX_STRIDE};
use std::ffi::{c_char, CStr};

/// Status codes (mirror `logi_terrain_ffi.h`: OK=0, NULL_ARG=-1, INTERNAL=-4).
const LOGI_OK: i32 = 0;
const LOGI_NULL_ARG: i32 = -1;

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
    pub _pad: u32,
    pub bounds_center: [f32; 3], // Unreal render space (cm)
    pub bounds_radius: f32,
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
    /// Per-step distance-culled view into `tiles` (rebuilt each `logi_baked_world_step`).
    frame_scratch: Vec<LogiBakedTile>,
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

/// # Safety
/// `info` must be null or point to a valid `LogiBakedCreateInfo`.
unsafe fn meshing_mode_from(info: *const LogiBakedCreateInfo) -> MeshingMode {
    if info.is_null() {
        return MeshingMode::Uniform;
    }
    let info = unsafe { &*info };
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
    if store_dir_utf8.is_null() {
        return std::ptr::null_mut();
    }
    let dir = match unsafe { CStr::from_ptr(store_dir_utf8) }.to_str() {
        Ok(s) => s,
        Err(_) => return std::ptr::null_mut(),
    };
    let mode = unsafe { meshing_mode_from(info) };
    let skirt_depth_m = if info.is_null() {
        0.0
    } else {
        unsafe { (*info).skirt_depth_m }.max(0.0)
    };
    let world = match BakedWorld::open_with(dir, mode, skirt_depth_m) {
        Ok(w) => w,
        Err(_) => return std::ptr::null_mut(),
    };
    let tiles = build_tile_views(&world);
    Box::into_raw(Box::new(LogiBakedWorld {
        world,
        tiles,
        frame_scratch: Vec::new(),
    }))
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
            _pad: 0,
            bounds_center: t.mesh.bounds_center,
            bounds_radius: t.mesh.bounds_radius,
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

/// Create a **procedural** terrain world (no store on disk): generate the SSE-LOD pyramid from the
/// canonical fractal field — the one-engine procedural lane (the old `logi_terrain_scene`'s role). The
/// handle is otherwise identical to a store-backed one (same `step`/`select`/`get_frame`/`destroy`).
/// Returns null on a null `info` or a build failure. Blocking (meshes the whole bounded pyramid) — call
/// off the render thread.
///
/// # Safety
/// `info` must be null or point to a valid `LogiBakedProceduralInfo`.
#[no_mangle]
pub unsafe extern "C" fn logi_baked_world_create_procedural(
    info: *const LogiBakedProceduralInfo,
) -> *mut LogiBakedWorld {
    if info.is_null() {
        return std::ptr::null_mut();
    }
    let info = unsafe { &*info };
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
        Err(_) => return std::ptr::null_mut(),
    };
    let tiles = build_tile_views(&world);
    Box::into_raw(Box::new(LogiBakedWorld {
        world,
        tiles,
        frame_scratch: Vec::new(),
    }))
}

/// Camera-driven frame: like `get_frame` but distance-culled to tiles within `max_distance_cm` of the
/// camera (Unreal cm; `<= 0` = no cull). The returned tiles point into the world's owned buffers, valid
/// until `destroy` or the next `step`/`get_frame` on this world. HE-0 has one tile level, so this is a
/// distance cull, not SSE-LOD (a baked tile pyramid is a later-HE upgrade).
///
/// # Safety
/// `world` must be a live handle from [`logi_baked_world_create`]; `out` a valid writable pointer.
#[no_mangle]
pub unsafe extern "C" fn logi_baked_world_step(
    world: *mut LogiBakedWorld,
    camera_x_cm: f32,
    camera_y_cm: f32,
    camera_z_cm: f32,
    max_distance_cm: f32,
    out: *mut LogiBakedFrame,
) -> i32 {
    if world.is_null() || out.is_null() {
        return LOGI_NULL_ARG;
    }
    let handle = unsafe { &mut *world };
    let cam = [camera_x_cm, camera_y_cm, camera_z_cm];
    handle.frame_scratch.clear();
    for t in &handle.tiles {
        if within_cull_distance(t.bounds_center, t.bounds_radius, cam, max_distance_cm) {
            handle.frame_scratch.push(*t);
        }
    }
    let frame = LogiBakedFrame {
        tiles: handle.frame_scratch.as_ptr(),
        tile_count: handle.frame_scratch.len() as u32,
        _pad: 0,
        origin_ecef: handle.world.origin_ecef(),
    };
    unsafe { *out = frame };
    LOGI_OK
}

/// SSE-LOD frame: selects the coarse-far / fine-near tile set by descending the baked **pyramid** and
/// refining by screen-space error (reusing the kernel's `logi_terrain_lod`). `fov_y_radians`,
/// `viewport_height`, and `sse_target_px` drive the LOD; a single-level store renders everything. The
/// returned tiles point into owned buffers, valid until `destroy` or the next step/select/get_frame.
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
    if world.is_null() || out.is_null() {
        return LOGI_NULL_ARG;
    }
    let handle = unsafe { &mut *world };
    let selected: std::collections::HashSet<u64> = handle
        .world
        .select(
            [camera_x_cm, camera_y_cm, camera_z_cm],
            fov_y_radians as f64,
            viewport_height as f64,
            sse_target_px as f64,
        )
        .iter()
        .map(|t| t.key())
        .collect();
    handle.frame_scratch.clear();
    for t in &handle.tiles {
        if selected.contains(&t.tile_key) {
            handle.frame_scratch.push(*t);
        }
    }
    let frame = LogiBakedFrame {
        tiles: handle.frame_scratch.as_ptr(),
        tile_count: handle.frame_scratch.len() as u32,
        _pad: 0,
        origin_ecef: handle.world.origin_ecef(),
    };
    unsafe { *out = frame };
    LOGI_OK
}

/// Fill `out` with the render frame (all resident tiles + the ECEF origin). The tile pointers are valid
/// until [`logi_baked_world_destroy`].
///
/// # Safety
/// `world` must be a live handle from [`logi_baked_world_create`]; `out` must be a valid writable pointer.
#[no_mangle]
pub unsafe extern "C" fn logi_baked_world_get_frame(world: *const LogiBakedWorld, out: *mut LogiBakedFrame) -> i32 {
    if world.is_null() || out.is_null() {
        return LOGI_NULL_ARG;
    }
    let handle = unsafe { &*world };
    let frame = LogiBakedFrame {
        tiles: handle.tiles.as_ptr(),
        tile_count: handle.tiles.len() as u32,
        _pad: 0,
        origin_ecef: handle.world.origin_ecef(),
    };
    unsafe { *out = frame };
    LOGI_OK
}

/// Destroy a baked world (frees its meshes; any previously returned frame pointers dangle afterwards).
///
/// # Safety
/// `world` must be a handle from [`logi_baked_world_create`], not destroyed before, and not used after.
#[no_mangle]
pub unsafe extern "C" fn logi_baked_world_destroy(world: *mut LogiBakedWorld) {
    if !world.is_null() {
        drop(unsafe { Box::from_raw(world) });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn null_args_are_handled() {
        // create(null) → null; get_frame(null,..) → NULL_ARG; destroy(null) → no-op.
        assert!(unsafe { logi_baked_world_create(std::ptr::null(), std::ptr::null()) }.is_null());
        let mut frame = LogiBakedFrame {
            tiles: std::ptr::null(),
            tile_count: 0,
            _pad: 0,
            origin_ecef: [0.0; 3],
        };
        assert_eq!(
            unsafe { logi_baked_world_get_frame(std::ptr::null(), &mut frame) },
            LOGI_NULL_ARG
        );
        assert_eq!(
            unsafe { logi_baked_world_step(std::ptr::null_mut(), 0.0, 0.0, 0.0, 1.0, &mut frame) },
            LOGI_NULL_ARG
        );
        assert!(unsafe { logi_baked_world_create_procedural(std::ptr::null()) }.is_null());
        unsafe { logi_baked_world_destroy(std::ptr::null_mut()) };
    }
}
