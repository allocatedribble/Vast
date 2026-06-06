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

use logi_3dtiles_scene::{GeoRenderFrame, GeoWorld, GlobeStreamer, StreamPoll, ViewParams};
use std::time::Duration;

/// Opaque handle to a streaming globe world (boxed). Null is invalid. Owns the decoupled stream thread
/// (ADR-0012): the `bevy_ecs` `GeoWorld` advances on its own Rust thread; the game thread only pushes views
/// ([`logi_globe_world_set_view`]) and polls the latest frame ([`logi_globe_world_poll`]).
pub struct LogiGlobeWorld {
    streamer: GlobeStreamer,
    /// The latest frame polled from the stream thread — retained so its `Arc` geometry keeps backing the
    /// pointers in `frame_tiles` until the next New poll replaces it (valid-until-next-poll).
    current_frame: Option<GeoRenderFrame>,
    /// Scratch C descriptor array the returned [`LogiGlobeFrame::tiles`] points into (rebuilt each New poll).
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
    // The Geo World pre-spawns a fixed tile-entity pool; size it above the resident cap + in-flight slack so
    // it never exhausts in steady state. (Decode-failed tiles linger in the pool until a cleanup pass —
    // TODO; the headroom covers it for now.)
    match GeoWorld::from_ion(
        info.ion_asset_id,
        token,
        info.origin_longitude_deg,
        info.origin_latitude_deg,
        info.origin_height_m,
        info.workers.clamp(1, 64) as usize, // clamp: a garbage count must not spawn unbounded threads
        info.max_tile_distance_m,
        cache,
        5120, // pre-spawned tile-entity pool
        4096, // resident-tile cap (mirrors the old TilesetWorld max_decoded)
    ) {
        Ok(world) => Some(LogiGlobeWorld {
            // Hand the world to its own stream thread (ADR-0012): it advances continuously OFF the game
            // thread; the game thread only pushes views + polls. 8 ms idle park (~125 Hz when idle; a
            // `set_view` unparks it so it advances promptly on camera motion).
            streamer: GlobeStreamer::spawn(world, Duration::from_millis(8)),
            current_frame: None,
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

/// Rebuild the C descriptor array (`frame_tiles`) from the retained `current_frame`. The emitted pointers
/// index that frame's `Arc` geometry, which stays alive (valid for the renderer) until the next New poll
/// replaces `current_frame`.
fn build_globe_frame(gw: &mut LogiGlobeWorld) {
    let LogiGlobeWorld {
        current_frame,
        frame_tiles,
        ..
    } = gw;
    frame_tiles.clear();
    let Some(frame) = current_frame.as_ref() else {
        return;
    };
    frame_tiles.reserve(frame.tiles.len());
    for t in &frame.tiles {
        let g = &t.geom;
        // H5: count + stride come from the SAME buffers we emit pointers into → no OOB GPU upload.
        debug_assert!(
            g.vertices.len() % VERTEX_STRIDE == 0,
            "tile vertex buffer not a VERTEX_STRIDE multiple"
        );
        frame_tiles.push(LogiGlobeTile {
            tile_key: t.tile_key,
            vertices: g.vertices.as_ptr(),
            vertices_len: g.vertices.len() as u64,
            vertex_count: (g.vertices.len() / VERTEX_STRIDE) as u32,
            vertex_stride: VERTEX_STRIDE as u32,
            indices: g.indices.as_ptr() as *const u8,
            index_count: g.indices.len() as u32,
            _pad: 0,
            texture_jpeg: g.texture.as_ptr(),
            texture_jpeg_len: g.texture.len() as u64,
            bounds_center: g.bounds_center,
            bounds_radius: g.bounds_radius,
        });
    }
}

/// The camera view pushed to the stream thread (UE space). The forward + up + aspect + FOV build the view
/// frustum (frustum-aware streaming LOD); `dir`/`up` `[0,0,0]` ⇒ unknown (accept-all frustum, no view-priority
/// weighting). All `f64`; `#[repr(C)]`, 13 fields = 104 bytes — mirrors `LogiGlobeView` in the header.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct LogiGlobeView {
    pub camera_x_cm: f64,
    pub camera_y_cm: f64,
    pub camera_z_cm: f64,
    pub dir_x: f64,
    pub dir_y: f64,
    pub dir_z: f64,
    pub up_x: f64,
    pub up_y: f64,
    pub up_z: f64,
    pub aspect: f64,
    pub fov_y_radians: f64,
    pub viewport_height: f64,
    pub max_sse_pixels: f64,
}

/// Push the newest desired camera/view to the stream thread (newest-wins; cheap, non-blocking). The thread
/// advances against it asynchronously OFF the game thread; the renderer reads results via
/// [`logi_globe_world_poll`]. This is the producer half of the poll-based FFI (ADR-0012).
///
/// # Safety
/// `world` must be a valid handle from [`logi_globe_world_create`] (or null → `InvalidHandle`); `view` must
/// be null (→ `NullArg`) or point to a readable [`LogiGlobeView`].
#[no_mangle]
pub unsafe extern "C" fn logi_globe_world_set_view(
    world: *mut LogiGlobeWorld,
    view: *const LogiGlobeView,
) -> LogiStatus {
    guard(|| {
        let gw = match unsafe { world.as_mut() } {
            Some(w) => w,
            None => return LogiStatus::InvalidHandle,
        };
        let v = match unsafe { view.as_ref() } {
            Some(v) => v,
            None => return LogiStatus::NullArg,
        };
        gw.streamer.set_view(ViewParams {
            camera_ue_cm: [v.camera_x_cm, v.camera_y_cm, v.camera_z_cm],
            camera_dir_ue: [v.dir_x, v.dir_y, v.dir_z],
            camera_up_ue: [v.up_x, v.up_y, v.up_z],
            aspect: v.aspect,
            fov_y_radians: v.fov_y_radians,
            viewport_height: v.viewport_height,
            max_sse: v.max_sse_pixels,
        });
        LogiStatus::Ok
    })
}

/// Poll the latest render frame the stream thread has published and write it to `out_frame`. **Non-blocking:**
/// it returns the most recent complete frame immediately (or an empty frame before the first one) — it never
/// drives the streaming work, which runs on the stream thread. The frame's tile buffers are zero-copy views
/// into Rust-owned `Arc` geometry, **valid until the next `logi_globe_world_poll` on this world**. This is
/// the consumer half of the poll-based FFI (ADR-0012) — the replacement for the synchronous `step`.
///
/// # Safety
/// `world` must be a valid handle from [`logi_globe_world_create`] (or null → `InvalidHandle`); `out_frame`
/// must be null or point to writable `LogiGlobeFrame` storage (written unaligned).
#[no_mangle]
pub unsafe extern "C" fn logi_globe_world_poll(
    world: *mut LogiGlobeWorld,
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
        match gw.streamer.poll() {
            // A newer frame: retain it (its `Arc` geometry backs the pointers) and rebuild the descriptors.
            StreamPoll::New(frame) => {
                gw.current_frame = Some(frame);
                build_globe_frame(gw);
            }
            // No new frame: the previously built `frame_tiles` + retained geometry still stand (SAME).
            StreamPoll::Same => {}
            // Nothing published yet.
            StreamPoll::Empty => {
                gw.current_frame = None;
                gw.frame_tiles.clear();
            }
        }
        let origin_ecef = gw.current_frame.as_ref().map(|f| f.origin_ecef).unwrap_or([0.0; 3]);
        let out = LogiGlobeFrame {
            tiles: gw.frame_tiles.as_ptr(),
            tile_count: gw.frame_tiles.len() as u32,
            resident_tiles: gw.streamer.resident_tiles() as u32,
            in_flight: gw.streamer.in_flight() as u32,
            _pad: 0,
            origin_ecef,
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
        Some(gw) => gw.streamer.cached_bytes(),
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
