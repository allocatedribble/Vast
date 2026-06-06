//! The C-ABI entry points (runtime-model.md §3.1). Every call is wrapped so a Rust panic becomes a
//! status code / null instead of unwinding across FFI (UB). The world handle is an opaque
//! `*mut LogiTerrainWorld` (a boxed pointer); null is invalid.

use std::cell::RefCell;
use std::panic::{self, AssertUnwindSafe};

/// Status codes returned across the boundary. Matches `LogiStatus` (int32) in `logi_geo.h`.
#[repr(i32)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LogiStatus {
    Ok = 0,
    NullArg = -1,
    InvalidHandle = -2,
    Panic = -3,
    /// Reserved for a generic internal failure. Defined for the C header's completeness but **not
    /// currently emitted** by any lane (failures surface as `Panic`, `NullArg`, `InvalidHandle`, or a
    /// null handle); kept so C++ error handling can switch on the full documented set.
    Internal = -4,
    /// Reserved for a future create-time ABI check. Today the handshake is caller-driven — call
    /// [`logi_geo_abi_version`] and compare it before any other call (no create fn validates a version
    /// field yet, so this status is **not currently returned**).
    AbiMismatch = -5,
}

thread_local! {
    static LAST_ERROR: RefCell<String> = const { RefCell::new(String::new()) };
}

pub(crate) fn set_last_error(msg: impl Into<String>) {
    LAST_ERROR.with(|e| *e.borrow_mut() = msg.into());
}

fn guard(f: impl FnOnce() -> LogiStatus) -> LogiStatus {
    match panic::catch_unwind(AssertUnwindSafe(f)) {
        Ok(status) => status,
        Err(_) => {
            set_last_error("panic caught at logi-terrain FFI boundary");
            LogiStatus::Panic
        }
    }
}

/// Shared `extern "C"` panic guard for status-returning entry points: runs `f`, and if it unwinds,
/// records the boundary error and returns `panic_code` (e.g. `LOGI_GEO_PANIC = -3`) instead of letting
/// the panic cross the C ABI (UB). Used by the baked/procedural lane, which returns raw `i32` codes
/// rather than [`LogiStatus`].
pub(crate) fn guard_i32(panic_code: i32, f: impl FnOnce() -> i32) -> i32 {
    match panic::catch_unwind(AssertUnwindSafe(f)) {
        Ok(code) => code,
        Err(_) => {
            set_last_error("panic caught at logi-geo FFI boundary");
            panic_code
        }
    }
}

/// Shared `extern "C"` panic guard for pointer-returning constructors: runs `f`, and if it unwinds,
/// records the boundary error and returns null instead of letting the panic cross the C ABI (UB).
pub(crate) fn guard_ptr<T>(f: impl FnOnce() -> *mut T) -> *mut T {
    match panic::catch_unwind(AssertUnwindSafe(f)) {
        Ok(ptr) => ptr,
        Err(_) => {
            set_last_error("panic caught at logi-geo FFI boundary");
            std::ptr::null_mut()
        }
    }
}

/// Returns the ABI version. Unreal must verify this matches before any other call.
#[no_mangle]
pub extern "C" fn logi_geo_abi_version() -> u32 {
    crate::LOGI_ABI_VERSION
}

// The old streaming synthetic-terrain world (`the retired world-ABI symbol *` over `the retired render-scene crate`'s
// `LogiTerrainWorld`) was RETIRED in the one-engine merge: the baked + procedural lanes (`ffi_baked`,
// `logi_baked_world_create`/`_create_procedural`/`_select`) are the single terrain engine now. The
// shared `logi_geo_abi_version` (above) + `logi_geo_last_error_message` (below) stay — the globe
// and building lanes in this same DLL use them.

/// Copies the last error message (UTF-8) into `buf` and returns the number of message bytes written
/// (not counting the terminator). A trailing NUL is written **when there is room** (`written < buf_len`),
/// so a caller may treat `buf` as a C string; if the message exactly fills `buf_len` it is left
/// un-terminated and the caller must use the returned length.
///
/// # Safety
/// `buf` must be null or point to at least `buf_len` writable bytes.
#[no_mangle]
pub unsafe extern "C" fn logi_geo_last_error_message(buf: *mut u8, buf_len: usize) -> usize {
    if buf.is_null() || buf_len == 0 {
        return 0;
    }
    LAST_ERROR.with(|e| {
        let msg = e.borrow();
        let bytes = msg.as_bytes();
        let mut n = bytes.len().min(buf_len);
        // Back off to a UTF-8 char boundary so we never copy a split codepoint.
        while n > 0 && !msg.is_char_boundary(n) {
            n -= 1;
        }
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), buf, n) };
        // Write a trailing NUL when the buffer has a spare byte, so C consumers can read it as a string.
        if n < buf_len {
            unsafe { *buf.add(n) = 0 };
        }
        n
    })
}

// ----------------------------------------------------------------------------------------------------
// M1 — AGI_HQ building (ADR-0009, Milestone F). A one-shot textured mesh decoded from a `.dae` + `.jpg`
// on disk by `logi_3dtiles_scene` and transformed into Unreal render space. Additive surface (the
// terrain ABI is unchanged); the C++ side builds a textured proc-mesh from the returned zero-copy views.

use logi_3dtiles_scene::{load_building, BuildingMesh, Placement};
// Single source of truth for the interleaved-vertex stride (pos f3 | normal f3 | uv f2), shared with the
// baked lane (`ffi_baked`) and every mesh decoder. Avoids open-coding `32` / `len()/32` here (H5).
use logi_semantic::VERTEX_STRIDE;
use std::ffi::{c_char, CStr};

/// Opaque handle to a loaded building (boxed). Null is invalid.
pub struct LogiBuilding {
    mesh: BuildingMesh,
}

/// Geodetic placement (degrees + metres); mirrors `LogiPlacement` in the header.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct LogiPlacement {
    pub longitude_deg: f64,
    pub latitude_deg: f64,
    pub height_m: f64,
    pub heading_deg: f64,
}

/// Zero-copy views into a loaded building's Rust-owned buffers (valid until `logi_building_destroy`).
#[repr(C)]
pub struct LogiBuildingData {
    pub vertices: *const u8,
    pub vertices_len: u64,
    pub vertex_count: u32,
    pub vertex_stride: u32,
    pub indices: *const u8, // `index_count` little-endian u32 indices
    pub index_count: u32,
    pub _pad: u32,
    pub texture_jpeg: *const u8,
    pub texture_jpeg_len: u64,
    pub bounds_center: [f32; 3],
    pub bounds_radius: f32,
    pub ecef_origin: [f64; 3],
}

fn building_load_impl(
    dae_path: *const c_char,
    jpg_path: *const c_char,
    placement: *const LogiPlacement,
) -> Option<LogiBuilding> {
    if dae_path.is_null() || jpg_path.is_null() || placement.is_null() {
        return None;
    }
    let dae_path = unsafe { CStr::from_ptr(dae_path) }.to_str().ok()?;
    let jpg_path = unsafe { CStr::from_ptr(jpg_path) }.to_str().ok()?;
    let pl = unsafe { placement.read_unaligned() };
    let dae_xml = std::fs::read_to_string(dae_path).ok()?;
    let jpg = std::fs::read(jpg_path).ok()?;
    let placement = Placement {
        longitude_deg: pl.longitude_deg,
        latitude_deg: pl.latitude_deg,
        height_m: pl.height_m,
        heading_deg: pl.heading_deg,
    };
    let mesh = load_building(&dae_xml, jpg, &placement).ok()?;
    Some(LogiBuilding { mesh })
}

/// Loads a building from a `.dae` + `.jpg` on disk at `placement`. Returns null on failure. Free with
/// `logi_building_destroy`.
///
/// # Safety
/// `dae_path` and `jpg_path` must be null or valid NUL-terminated C strings; `placement` must be null or
/// point to a readable `LogiPlacement` (read unaligned). The returned handle is owned by the caller.
#[no_mangle]
pub unsafe extern "C" fn logi_building_load(
    dae_path: *const c_char,
    jpg_path: *const c_char,
    placement: *const LogiPlacement,
) -> *mut LogiBuilding {
    let result = panic::catch_unwind(AssertUnwindSafe(|| building_load_impl(dae_path, jpg_path, placement)));
    match result {
        Ok(Some(b)) => Box::into_raw(Box::new(b)),
        _ => {
            set_last_error("logi_building_load: failed to read or decode the building");
            std::ptr::null_mut()
        }
    }
}

/// Writes zero-copy views of the building's geometry + texture into `out`.
///
/// # Safety
/// `building` must be a valid handle from [`logi_building_load`] (or null → `InvalidHandle`). `out` must be
/// null or point to writable `LogiBuildingData` storage (written unaligned); its pointers borrow the
/// building's buffers, valid until [`logi_building_destroy`].
#[no_mangle]
pub unsafe extern "C" fn logi_building_get_data(
    building: *const LogiBuilding,
    out: *mut LogiBuildingData,
) -> LogiStatus {
    guard(|| {
        let b = match unsafe { building.as_ref() } {
            Some(b) => b,
            None => return LogiStatus::InvalidHandle,
        };
        if out.is_null() {
            return LogiStatus::NullArg;
        }
        let m = &b.mesh;
        // H5: derive count + stride from the SAME buffer (`m`) whose pointer we emit, via the shared
        // VERTEX_STRIDE — never a separate source or an open-coded `32` that can drift OOB on the GPU.
        debug_assert!(
            m.vertices.len() % VERTEX_STRIDE == 0,
            "vertex buffer not a VERTEX_STRIDE multiple"
        );
        let data = LogiBuildingData {
            vertices: m.vertices.as_ptr(),
            vertices_len: m.vertices.len() as u64,
            vertex_count: (m.vertices.len() / VERTEX_STRIDE) as u32,
            vertex_stride: VERTEX_STRIDE as u32,
            indices: m.indices.as_ptr() as *const u8,
            index_count: m.indices.len() as u32,
            _pad: 0,
            texture_jpeg: m.texture_jpeg.as_ptr(),
            texture_jpeg_len: m.texture_jpeg.len() as u64,
            bounds_center: m.bounds_center,
            bounds_radius: m.bounds_radius,
            ecef_origin: m.ecef_origin,
        };
        unsafe { out.write_unaligned(data) };
        LogiStatus::Ok
    })
}

/// Destroys a building created by `logi_building_load`.
///
/// # Safety
/// `building` must be null or a handle from [`logi_building_load`] that has not already been destroyed.
#[no_mangle]
pub unsafe extern "C" fn logi_building_destroy(building: *mut LogiBuilding) {
    if building.is_null() {
        return;
    }
    let _ = panic::catch_unwind(AssertUnwindSafe(|| unsafe {
        drop(Box::from_raw(building));
    }));
}

// ----------------------------------------------------------------------------------------------------
// Streaming globe — Google Photorealistic 3D Tiles (ADR-0009, Milestone F). A live streaming world: it
// resolves a Cesium ion asset, fetches + decodes glTF tiles on a worker pool, and rebases each tile into
// Unreal render space (cm, Z-up LH) at a fixed ENU origin. `step` returns a multi-tile frame whose tile
// buffers are **zero-copy views into Rust-owned memory, valid until the next `step`** (the terrain-lane
// contract). The C++ side caches one proc-mesh section per `tile_key` and rebuilds only new keys.

use logi_3dtiles_scene::TilesetWorld;

/// Opaque handle to a streaming globe world (boxed). Null is invalid. Holds the world + a scratch frame
/// list whose buffer the returned [`LogiGlobeFrame::tiles`] points into.
pub struct LogiGlobeWorld {
    world: TilesetWorld,
    frame_tiles: Vec<LogiGlobeTile>,
}

/// Parameters for [`logi_globe_world_create`]; mirrors `LogiGlobeCreateInfo` in the header. The ion token
/// is passed separately (a C string) and is not retained past the resolve.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct LogiGlobeCreateInfo {
    pub ion_asset_id: u64,
    pub origin_longitude_deg: f64,
    pub origin_latitude_deg: f64,
    pub origin_height_m: f64,
    /// Spatial cull radius (metres) around the camera; caps the streamed extent.
    pub max_tile_distance_m: f64,
    pub workers: u32,
    pub _reserved: u32,
    /// Persistent on-disk tile-cache budget in bytes; `0` disables the cache. The cache directory is the
    /// separate `cache_dir` argument to `logi_globe_world_create`.
    pub cache_budget_bytes: u64,
}

/// One tile to draw this frame — zero-copy views into Rust-owned buffers (valid until the next `step`).
/// Geometry is in Unreal render space (cm, Z-up LH); the texture is raw encoded bytes (JPEG/PNG).
#[repr(C)]
pub struct LogiGlobeTile {
    /// Stable per-tile key (hash of the tile path) for C++ section caching across frames.
    pub tile_key: u64,
    pub vertices: *const u8,
    pub vertices_len: u64,
    pub vertex_count: u32,
    pub vertex_stride: u32, // = 32 (pos f3 | normal f3 | uv f2)
    pub indices: *const u8, // `index_count` little-endian u32 indices
    pub index_count: u32,
    pub _pad: u32,
    pub texture_jpeg: *const u8,
    pub texture_jpeg_len: u64,
    pub bounds_center: [f32; 3],
    pub bounds_radius: f32,
}

/// The frame returned by [`logi_globe_world_step`]: a pointer to the world's scratch tile array (valid
/// until the next `step`) + counts + the ENU origin's WGS84 ECEF (metres).
#[repr(C)]
pub struct LogiGlobeFrame {
    pub tiles: *const LogiGlobeTile,
    pub tile_count: u32,
    pub resident_tiles: u32,
    pub in_flight: u32,
    pub _pad: u32,
    pub origin_ecef: [f64; 3],
}

// Compile-time ABI locks: a field edit that changes any boundary struct's size fails the build HERE,
// mirroring the runtime checks in `tests/globe_contract.rs` and the `const _` guards in
// `the retired render-scene crate` (the M1/globe structs previously had only the runtime test).
const _: () = {
    assert!(std::mem::size_of::<LogiPlacement>() == 32);
    assert!(std::mem::size_of::<LogiBuildingData>() == 96);
    assert!(std::mem::size_of::<LogiGlobeCreateInfo>() == 56);
    assert!(std::mem::size_of::<LogiGlobeTile>() == 80);
    assert!(std::mem::size_of::<LogiGlobeFrame>() == 48);
};

/// FNV-1a over the tile path's little-endian bytes — a stable per-tile key for C++ caching.
fn hash_path(path: &[u32]) -> u64 {
    let mut h: u64 = 0xCBF2_9CE4_8422_2325;
    for &seg in path {
        for b in seg.to_le_bytes() {
            h ^= b as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01B3);
        }
    }
    h
}

fn globe_create_impl(
    info: *const LogiGlobeCreateInfo,
    ion_token: *const c_char,
    cache_dir: *const c_char,
) -> Option<LogiGlobeWorld> {
    if info.is_null() || ion_token.is_null() {
        set_last_error("logi_globe_world_create: null info or token");
        return None;
    }
    let info = unsafe { info.read_unaligned() };
    let token = match unsafe { CStr::from_ptr(ion_token) }.to_str() {
        Ok(t) => t,
        Err(_) => {
            set_last_error("logi_globe_world_create: ion token is not valid UTF-8");
            return None;
        }
    };
    // Optional persistent disk cache; a cache-open failure degrades to no-cache (the globe still streams).
    let cache = build_globe_cache(info.cache_budget_bytes, cache_dir);
    match TilesetWorld::from_ion(
        info.ion_asset_id,
        token,
        info.origin_longitude_deg,
        info.origin_latitude_deg,
        info.origin_height_m,
        info.workers.clamp(1, 64) as usize, // clamp: a garbage count must not spawn unbounded threads
        info.max_tile_distance_m,
        cache,
    ) {
        Ok(world) => Some(LogiGlobeWorld {
            world,
            frame_tiles: Vec::new(),
        }),
        Err(e) => {
            set_last_error(format!("logi_globe_world_create: {e:?}"));
            None
        }
    }
}

/// Build the optional persistent disk cache from the FFI budget + dir. Returns `None` (no cache) when the
/// budget is 0, the dir is null/empty, or the cache can't be opened — the last case logs but still streams.
fn build_globe_cache(budget: u64, cache_dir: *const c_char) -> Option<std::sync::Arc<logi_3dtiles_io::DiskCache>> {
    if budget == 0 || cache_dir.is_null() {
        return None;
    }
    let dir = unsafe { CStr::from_ptr(cache_dir) }.to_str().ok()?;
    if dir.is_empty() {
        return None;
    }
    match logi_3dtiles_io::DiskCache::open(dir, budget) {
        Ok(c) => Some(std::sync::Arc::new(c)),
        Err(e) => {
            set_last_error(format!(
                "globe disk cache open '{dir}': {e} (streaming without a cache)"
            ));
            None
        }
    }
}

/// Creates a streaming globe world: resolves the ion asset, fetches the root tileset, and centres the ENU
/// frame on the given origin. `cache_dir` (a UTF-8 path, or null/empty) + `info.cache_budget_bytes` enable
/// the persistent on-disk tile cache. **Blocking** (one endpoint resolve + one root fetch) — call off the
/// render thread. Returns null on failure (see `logi_geo_last_error_message`). Free with
/// `logi_globe_world_destroy`.
///
/// # Safety
/// `info` must be null or point to a readable `LogiGlobeCreateInfo` (read unaligned); `ion_token` must be
/// null or a valid NUL-terminated C string; `cache_dir` must be null or a valid NUL-terminated C string.
/// The returned handle is owned by the caller.
#[no_mangle]
pub unsafe extern "C" fn logi_globe_world_create(
    info: *const LogiGlobeCreateInfo,
    ion_token: *const c_char,
    cache_dir: *const c_char,
) -> *mut LogiGlobeWorld {
    let result = panic::catch_unwind(AssertUnwindSafe(|| globe_create_impl(info, ion_token, cache_dir)));
    match result {
        Ok(Some(w)) => Box::into_raw(Box::new(w)),
        Ok(None) => std::ptr::null_mut(), // globe_create_impl already set a specific last_error
        Err(_) => {
            set_last_error("logi_globe_world_create: panic at FFI boundary");
            std::ptr::null_mut()
        }
    }
}

/// Advances the globe world one step using an **Unreal-space** camera position (cm; ENU origin at Unreal
/// `(0,0,0)`) and writes the frame to `out_frame`. The frame's tile buffers are zero-copy views valid
/// until the next `step` on this world.
///
/// # Safety
/// `world` must be a valid handle from [`logi_globe_world_create`] (or null → `InvalidHandle`). `out_frame`
/// must be null or point to writable `LogiGlobeFrame` storage (written unaligned); the frame's tile buffers
/// borrow Rust-owned memory valid only until the next `step` on this world.
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn logi_globe_world_step(
    world: *mut LogiGlobeWorld,
    camera_x_cm: f64,
    camera_y_cm: f64,
    camera_z_cm: f64,
    fov_y_radians: f64,
    viewport_height: f64,
    max_sse_pixels: f64,
    out_frame: *mut LogiGlobeFrame,
) -> LogiStatus {
    guard(|| {
        let gw = match unsafe { world.as_mut() } {
            Some(w) => w,
            None => return LogiStatus::InvalidHandle,
        };
        if out_frame.is_null() {
            return LogiStatus::NullArg;
        }
        let frame = gw.world.step_from_ue_camera(
            [camera_x_cm, camera_y_cm, camera_z_cm],
            fov_y_radians,
            viewport_height,
            max_sse_pixels,
        );
        // Rebuild the scratch tile array; its pointers index the world's decoded buffers (heap-stable
        // until that tile is re-decoded/evicted on a later step — i.e. valid until the next `step`).
        let mut tiles = Vec::with_capacity(frame.tiles.len());
        for rt in &frame.tiles {
            if let Some(m) = gw.world.tile_mesh(&rt.path) {
                // H5: count + stride come from the SAME `m` buffers we emit pointers into (not the
                // separate `RenderTileInfo`, which can disagree after a re-decode) → no OOB GPU upload.
                debug_assert!(
                    m.vertices.len() % VERTEX_STRIDE == 0,
                    "tile vertex buffer not a VERTEX_STRIDE multiple"
                );
                tiles.push(LogiGlobeTile {
                    tile_key: hash_path(&rt.path),
                    vertices: m.vertices.as_ptr(),
                    vertices_len: m.vertices.len() as u64,
                    vertex_count: (m.vertices.len() / VERTEX_STRIDE) as u32,
                    vertex_stride: VERTEX_STRIDE as u32,
                    indices: m.indices.as_ptr() as *const u8,
                    index_count: m.indices.len() as u32,
                    _pad: 0,
                    texture_jpeg: m.texture.as_ptr(),
                    texture_jpeg_len: m.texture.len() as u64,
                    bounds_center: rt.bounds_center,
                    bounds_radius: rt.bounds_radius,
                });
            }
        }
        gw.frame_tiles = tiles;
        let out = LogiGlobeFrame {
            tiles: gw.frame_tiles.as_ptr(),
            tile_count: gw.frame_tiles.len() as u32,
            resident_tiles: gw.world.resident_tiles() as u32,
            in_flight: gw.world.in_flight() as u32,
            _pad: 0,
            origin_ecef: frame.origin_ecef,
        };
        unsafe { out_frame.write_unaligned(out) };
        LogiStatus::Ok
    })
}

/// Bytes currently resident in the world's persistent disk cache (0 if none configured). Telemetry only.
///
/// # Safety
/// `world` must be null or a valid handle from [`logi_globe_world_create`].
#[no_mangle]
pub unsafe extern "C" fn logi_globe_world_cached_bytes(world: *const LogiGlobeWorld) -> u64 {
    match unsafe { world.as_ref() } {
        Some(gw) => gw.world.cached_bytes(),
        None => 0,
    }
}

/// Destroys a globe world created by `logi_globe_world_create`.
///
/// # Safety
/// `world` must be null or a handle from [`logi_globe_world_create`] that has not already been destroyed.
#[no_mangle]
pub unsafe extern "C" fn logi_globe_world_destroy(world: *mut LogiGlobeWorld) {
    if world.is_null() {
        return;
    }
    let _ = panic::catch_unwind(AssertUnwindSafe(|| unsafe {
        drop(Box::from_raw(world));
    }));
}
