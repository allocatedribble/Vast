//! Additive C ABI for the **baked semantic-tile world** (ADR-0011, Milestone HE-0).
//!
//! Mirrors the globe API's shape: open a baked store, then hand back every tile's mesh in Unreal render
//! space (cm, Z-up LH) as **zero-copy** views into Rust-owned memory, valid until `destroy`. HE-0 serves
//! the whole AOI statically (no camera-driven streaming yet — that lands in a later HE), so a single
//! `get_frame` is enough; the C++ side caches one proc-mesh section per `tile_key`.
//!
//! No Unreal types cross; Rust never calls Unreal (AGENTS.md §1). Errors are status codes / null — no
//! panic unwinds across the boundary.

use logi_semantic::{BakedWorld, MeshingMode, VERTEX_STRIDE};
use std::ffi::{c_char, CStr};

/// Status codes (mirror `logi_terrain_ffi.h`: OK=0, NULL_ARG=-1, INTERNAL=-4).
const LOGI_OK: i32 = 0;
const LOGI_NULL_ARG: i32 = -1;

/// One baked tile for the renderer: a 32-byte-vertex mesh (`pos f3 | normal f3 | uv f2`, Unreal cm) +
/// `u32` indices, both zero-copy views valid until [`logi_baked_world_destroy`].
#[repr(C)]
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
}

/// Creation parameters — the **meshing toggle**. `meshing_mode`: 0 = uniform grid, 1 = adaptive TIN
/// (using `tin_max_error_m`, metres). A null `info` defaults to uniform.
#[repr(C)]
pub struct LogiBakedCreateInfo {
    pub meshing_mode: u32,
    pub tin_max_error_m: f32,
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
    let world = match BakedWorld::open_with(dir, mode) {
        Ok(w) => w,
        Err(_) => return std::ptr::null_mut(),
    };
    let tiles: Vec<LogiBakedTile> = world
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
        .collect();
    Box::into_raw(Box::new(LogiBakedWorld { world, tiles }))
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
        unsafe { logi_baked_world_destroy(std::ptr::null_mut()) };
    }
}
