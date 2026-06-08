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

// Lock the status enum's ABI (the offset_of! guards below cover the structs, but not this enum): it must be a
// 4-byte int32 with the exact discriminants the C header mirrors as `#define LOGI_GEO_*`. A reorder, a width
// change, or a renumber that diverges from `logi_geo_ffi.h` then fails the build instead of silently drifting.
const _: () = {
    assert!(core::mem::size_of::<LogiStatus>() == 4);
    assert!(LogiStatus::Ok as i32 == 0);
    assert!(LogiStatus::NullArg as i32 == -1);
    assert!(LogiStatus::InvalidHandle as i32 == -2);
    assert!(LogiStatus::Panic as i32 == -3);
    assert!(LogiStatus::Internal as i32 == -4);
    assert!(LogiStatus::AbiMismatch as i32 == -5);
};

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
    // no-panic: a single `const` field read — cannot panic, so no `catch_unwind` is needed. If a future
    // edit makes the body fallible, wrap it in `guard`/`catch_unwind` (a panic across the C ABI is UB).
    crate::LOGI_ABI_VERSION
}

// The old streaming synthetic-terrain world was RETIRED in the one-engine merge: the baked + procedural
// lanes (`ffi_baked`, `logi_baked_world_create`/`_create_procedural`/`_select`) are the single terrain
// engine now. The shared `logi_geo_abi_version` (above) + `logi_geo_last_error_message` (below) stay —
// the globe and building lanes in this same DLL use them.

/// Copies the last error message (UTF-8) into `buf` and returns the number of message bytes written
/// (not counting the terminator). A trailing NUL is written **when there is room** (`written < buf_len`),
/// so a caller may treat `buf` as a C string; if the message exactly fills `buf_len` it is left
/// un-terminated and the caller must use the returned length.
///
/// # Safety
/// `buf` must be null or point to at least `buf_len` writable bytes.
#[no_mangle]
pub unsafe extern "C" fn logi_geo_last_error_message(buf: *mut u8, buf_len: usize) -> usize {
    // no-panic: under the documented safety contract this only reads a thread-local string and does a
    // bounded `copy_nonoverlapping` into the caller's buffer (length clamped to `buf_len`, backed off to a
    // char boundary) — no indexing/slicing/alloc that can panic. Kept unguarded to avoid `catch_unwind`
    // overhead on the error-reporting path; a future edit that adds fallible work must add a guard.
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
// Unreal render space (cm, Z-up LH) at a fixed ENU origin. `poll` returns a multi-tile frame whose tile
// buffers are **zero-copy views into Rust-owned memory, valid until the next `poll`** (the terrain-lane
// contract). The C++ side caches one proc-mesh section per `tile_key` and rebuilds only new keys.

use logi_3dtiles_scene::{GeoRenderFrame, GeoWorld, GlobeStreamer, StreamPoll, ViewParams};
use std::collections::{HashMap, HashSet};
#[cfg(debug_assertions)]
use std::thread::ThreadId;
use std::time::Duration;

/// Opaque handle to a streaming globe world (boxed). Null is invalid. Owns the decoupled stream thread
/// (ADR-0012): the `bevy_ecs` `GeoWorld` advances on its own Rust thread; the game thread only pushes views
/// ([`logi_globe_world_set_view`]) and polls the latest frame ([`logi_globe_world_poll`]).
///
/// **Single-consumer invariant.** The handle is NOT `Sync` for the FFI's purposes: exactly one thread (the
/// Unreal game thread) may call [`logi_globe_world_set_view`] / [`logi_globe_world_poll`] on a given world —
/// those mutate it through `&mut` with no internal lock. (The actual streaming work runs on the streamer's
/// own thread behind a channel; that is not this invariant.) A `debug_assert!` in those two entry points
/// pins the first caller's thread and fires on any cross-thread reuse in debug builds; it compiles out in
/// release, so the contract is the caller's to keep.
pub struct LogiGlobeWorld {
    streamer: GlobeStreamer,
    /// The latest frame polled from the stream thread — retained so its `Arc` geometry keeps backing the
    /// pointers in `frame_tiles` until the next New poll replaces it (valid-until-next-poll).
    current_frame: Option<GeoRenderFrame>,
    /// Scratch C descriptor array the returned [`LogiGlobeFrame::tiles`] points into (rebuilt each New poll).
    frame_tiles: Vec<LogiGlobeTile>,
    /// Delta scratch (planetary-scale O(Δ) reconcile). `prev_keys` is the set of tile keys handed to the renderer
    /// on the LAST New poll; each New poll diffs the current frame against it to fill `added_indices` (indices into
    /// `frame_tiles` whose key is new) and `removed_keys` (keys that departed), so the renderer applies O(Δ) changes
    /// instead of re-scanning the whole visible set every frame. `key_to_index` is the current frame's key→index map
    /// — it dedups keys while diffing AND backs [`logi_globe_world_tile_by_key`] (a targeted texture re-drive).
    prev_keys: HashSet<u64>,
    key_to_index: HashMap<u64, u32>,
    added_indices: Vec<u32>,
    removed_keys: Vec<u64>,
    /// The first thread to call `poll`/`set_view` on this world (lazily pinned). Debug-only single-consumer
    /// guard (see the type doc); `None` until the first consumer call. Gated to debug builds so it adds no
    /// field/size in release. Carries no ABI weight regardless — the handle is boxed and never crosses the C
    /// boundary by value.
    #[cfg(debug_assertions)]
    consumer_thread: Option<ThreadId>,
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
    /// In-memory decoded-tile (mesh+texture) cache budget in **MiB** — the warm-restore LRU in front of the
    /// resident set. `0` ⇒ the engine default ([`DEFAULT_MESH_CACHE_BYTES`]). (Was a reserved pad field; it
    /// occupies the same 4 bytes, so the ABI size is unchanged.)
    pub mesh_cache_budget_mb: u32,
    /// Persistent on-disk tile-cache budget in bytes; `0` disables the cache. The cache directory is the
    /// separate `cache_dir` argument to `logi_globe_world_create`.
    pub cache_budget_bytes: u64,
}

/// [`LogiGlobeTile::flags`] bit 0: the tile's material is unlit (`KHR_materials_unlit`) — the renderer should
/// shade it emissive (it bakes its own lighting; P3DT bakes real-world sun/shadow into the albedo) and not
/// apply scene lighting on top. Mirrors `LOGI_GLOBE_TILE_FLAG_UNLIT` in `logi_geo_ffi.h`.
pub const LOGI_GLOBE_TILE_FLAG_UNLIT: u32 = 1;

/// One tile to draw this frame — zero-copy views into Rust-owned buffers (valid until the next `poll`).
/// Geometry is in Unreal render space (cm, Z-up LH); the texture is raw encoded bytes (JPEG/PNG).
/// `Copy` is a bitwise copy of the (borrowed) pointers — used by [`logi_globe_world_tile_by_key`] to hand a
/// single tile back out; the copied pointers carry the same valid-until-next-poll contract as the array.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct LogiGlobeTile {
    /// Stable per-tile key (hash of the tile path) for C++ section caching across frames.
    pub tile_key: u64,
    pub vertices: *const u8,
    pub vertices_len: u64,
    pub vertex_count: u32,
    pub vertex_stride: u32, // = 32 (pos f3 | normal f3 | uv f2)
    pub indices: *const u8, // `index_count` little-endian u32 indices
    pub index_count: u32,
    /// Per-tile render flags (repurposes the former alignment pad — same offset/size). Bit 0 =
    /// [`LOGI_GLOBE_TILE_FLAG_UNLIT`]. Other bits reserved (0).
    pub flags: u32,
    pub texture_jpeg: *const u8,
    pub texture_jpeg_len: u64,
    pub bounds_center: [f32; 3],
    pub bounds_radius: f32,
    /// Refinement tree depth (root = 0; higher = more refined). Debug-viz / telemetry only — the renderer
    /// ignores it for normal drawing. Mirrors the tile's `path.len()` in the selector.
    pub depth: u32,
    /// 3D Tiles geometric error at this tile (metres; lower = finer). Debug-viz / telemetry only.
    pub geometric_error: f32,
}

/// The frame returned by [`logi_globe_world_poll`]: a pointer to the world's scratch tile array (valid until the
/// next `poll`) + the per-poll DELTA against the previously polled frame + counts + the ENU origin's WGS84 ECEF.
///
/// **Delta (ADR-0012, planetary scale).** `added_indices` lists the indices into `tiles` whose `tile_key` is new
/// since the last poll; `removed_keys` lists the keys that departed. `frame_id` is the render frame's monotonic id
/// (0 = empty) — equal to the previous poll's `frame_id` ⇒ nothing changed (the renderer skips reconcile entirely).
/// The full `tiles` array is still provided (for the first frame / a large-burst resync); both delta arrays are
/// zero-copy views into world scratch, valid until the next `poll`.
#[repr(C)]
pub struct LogiGlobeFrame {
    pub tiles: *const LogiGlobeTile,
    /// Indices into `tiles` whose key is NEW vs the previous poll (`added_count` entries).
    pub added_indices: *const u32,
    /// Tile keys that DEPARTED since the previous poll (`removed_count` entries; not present in `tiles`).
    pub removed_keys: *const u64,
    pub tile_count: u32,
    pub added_count: u32,
    pub removed_count: u32,
    pub resident_tiles: u32,
    pub in_flight: u32,
    pub _pad: u32,
    /// Monotonic render-frame id (0 = no frame yet). Unchanged across polls ⇒ identical scene ⇒ skip reconcile.
    pub frame_id: u64,
    pub origin_ecef: [f64; 3],
}

// Compile-time ABI locks for the globe/building lanes. `include/logi_geo_ffi.h` is HAND-MAINTAINED (it is
// NOT cbindgen-generated), so a total-size guard alone is not enough: a field *reorder* or *retype* that
// preserves the total size would silently mis-decode every frame and corrupt memory across the boundary.
// We therefore lock BOTH the total size AND every field's byte offset via `core::mem::offset_of!`. A field
// edit that diverges from the checked-in header now fails the build HERE (and mirrors the runtime checks in
// `tests/globe_contract.rs` / the field table in `tests/abi_layout.rs`). The expected offsets are the
// `#[repr(C)]` layout of the structs below cross-checked field-by-field against the header's struct order.
//
// To re-derive an offset after an intentional ABI change: sum each prior field's size rounded up to the
// next field's alignment (pointers/u64/f64 = 8, u32/f32 = 4, [f32;N] = 4-aligned, [f64;N] = 8-aligned),
// then round the struct's total up to its max field alignment. Bump LOGI_ABI_VERSION and the header to match.
use core::mem::offset_of;
const _: () = {
    // LogiPlacement — 4 × f64 (align 8), size 32.
    assert!(std::mem::size_of::<LogiPlacement>() == 32);
    assert!(offset_of!(LogiPlacement, longitude_deg) == 0);
    assert!(offset_of!(LogiPlacement, latitude_deg) == 8);
    assert!(offset_of!(LogiPlacement, height_m) == 16);
    assert!(offset_of!(LogiPlacement, heading_deg) == 24);

    // LogiBuildingData — mixed ptr/u64/u32 + [f32;3]/f32 + [f64;3] (align 8), size 96.
    assert!(std::mem::size_of::<LogiBuildingData>() == 96);
    assert!(offset_of!(LogiBuildingData, vertices) == 0);
    assert!(offset_of!(LogiBuildingData, vertices_len) == 8);
    assert!(offset_of!(LogiBuildingData, vertex_count) == 16);
    assert!(offset_of!(LogiBuildingData, vertex_stride) == 20);
    assert!(offset_of!(LogiBuildingData, indices) == 24);
    assert!(offset_of!(LogiBuildingData, index_count) == 32);
    assert!(offset_of!(LogiBuildingData, _pad) == 36);
    assert!(offset_of!(LogiBuildingData, texture_jpeg) == 40);
    assert!(offset_of!(LogiBuildingData, texture_jpeg_len) == 48);
    assert!(offset_of!(LogiBuildingData, bounds_center) == 56);
    assert!(offset_of!(LogiBuildingData, bounds_radius) == 68);
    assert!(offset_of!(LogiBuildingData, ecef_origin) == 72);

    // LogiGlobeCreateInfo — u64 + 4 × f64 + 2 × u32 + u64 (align 8), size 56. `mesh_cache_budget_mb`
    // occupies the 4 bytes that used to be a reserved pad, so the ABI size is unchanged.
    assert!(std::mem::size_of::<LogiGlobeCreateInfo>() == 56);
    assert!(offset_of!(LogiGlobeCreateInfo, ion_asset_id) == 0);
    assert!(offset_of!(LogiGlobeCreateInfo, origin_longitude_deg) == 8);
    assert!(offset_of!(LogiGlobeCreateInfo, origin_latitude_deg) == 16);
    assert!(offset_of!(LogiGlobeCreateInfo, origin_height_m) == 24);
    assert!(offset_of!(LogiGlobeCreateInfo, max_tile_distance_m) == 32);
    assert!(offset_of!(LogiGlobeCreateInfo, workers) == 40);
    assert!(offset_of!(LogiGlobeCreateInfo, mesh_cache_budget_mb) == 44);
    assert!(offset_of!(LogiGlobeCreateInfo, cache_budget_bytes) == 48);

    // LogiGlobeTile — same shape as LogiBuildingData up to texture, then a depth (u32) + geometric_error
    // (f32) tail for debug-viz/telemetry; no trailing ecef (align 8), size 88 (was 80 before v7).
    assert!(std::mem::size_of::<LogiGlobeTile>() == 88);
    assert!(offset_of!(LogiGlobeTile, tile_key) == 0);
    assert!(offset_of!(LogiGlobeTile, vertices) == 8);
    assert!(offset_of!(LogiGlobeTile, vertices_len) == 16);
    assert!(offset_of!(LogiGlobeTile, vertex_count) == 24);
    assert!(offset_of!(LogiGlobeTile, vertex_stride) == 28);
    assert!(offset_of!(LogiGlobeTile, indices) == 32);
    assert!(offset_of!(LogiGlobeTile, index_count) == 40);
    assert!(offset_of!(LogiGlobeTile, flags) == 44);
    assert!(offset_of!(LogiGlobeTile, texture_jpeg) == 48);
    assert!(offset_of!(LogiGlobeTile, texture_jpeg_len) == 56);
    assert!(offset_of!(LogiGlobeTile, bounds_center) == 64);
    assert!(offset_of!(LogiGlobeTile, bounds_radius) == 76);
    assert!(offset_of!(LogiGlobeTile, depth) == 80);
    assert!(offset_of!(LogiGlobeTile, geometric_error) == 84);

    // LogiGlobeFrame — 3 ptr + 6 × u32 + u64 + [f64;3] (align 8), size 80 (was 48 before v8 added the per-poll
    // delta lists `added_indices`/`removed_keys` + their counts + the `frame_id`).
    assert!(std::mem::size_of::<LogiGlobeFrame>() == 80);
    assert!(offset_of!(LogiGlobeFrame, tiles) == 0);
    assert!(offset_of!(LogiGlobeFrame, added_indices) == 8);
    assert!(offset_of!(LogiGlobeFrame, removed_keys) == 16);
    assert!(offset_of!(LogiGlobeFrame, tile_count) == 24);
    assert!(offset_of!(LogiGlobeFrame, added_count) == 28);
    assert!(offset_of!(LogiGlobeFrame, removed_count) == 32);
    assert!(offset_of!(LogiGlobeFrame, resident_tiles) == 36);
    assert!(offset_of!(LogiGlobeFrame, in_flight) == 40);
    assert!(offset_of!(LogiGlobeFrame, _pad) == 44);
    assert!(offset_of!(LogiGlobeFrame, frame_id) == 48);
    assert!(offset_of!(LogiGlobeFrame, origin_ecef) == 56);

    // LogiGlobeView — 13 × f64 (align 8), size 104. (Defined below; the offset refs are order-independent.)
    assert!(std::mem::size_of::<LogiGlobeView>() == 104);
    assert!(offset_of!(LogiGlobeView, camera_x_cm) == 0);
    assert!(offset_of!(LogiGlobeView, camera_y_cm) == 8);
    assert!(offset_of!(LogiGlobeView, camera_z_cm) == 16);
    assert!(offset_of!(LogiGlobeView, dir_x) == 24);
    assert!(offset_of!(LogiGlobeView, dir_y) == 32);
    assert!(offset_of!(LogiGlobeView, dir_z) == 40);
    assert!(offset_of!(LogiGlobeView, up_x) == 48);
    assert!(offset_of!(LogiGlobeView, up_y) == 56);
    assert!(offset_of!(LogiGlobeView, up_z) == 64);
    assert!(offset_of!(LogiGlobeView, aspect) == 72);
    assert!(offset_of!(LogiGlobeView, fov_y_radians) == 80);
    assert!(offset_of!(LogiGlobeView, viewport_height) == 88);
    assert!(offset_of!(LogiGlobeView, max_sse_pixels) == 96);
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
        24576, // pre-spawned tile-entity pool (> resident cap + in-flight + decode-failed slack). Raised from
        // 10240: at the finer 6px Detail Target the in-view working set is ~1.8× the 8px set, which
        // already sat at ~9.8k — 95% of the old pool, the hard wall that silently dropped tiles into
        // coarse holes. FIRST tuning step; verify the live `resident` lands comfortably below this.
        18432, // resident-tile cap — raised with the finer 6px Detail Target so the deeper LOD's larger working
               // set fits without the LRU evicting still-wanted near tiles. In-view tiles are stamped every
               // frame (never evictable), so this mainly bounds the off-view shell; the pool above is the ceiling.
    ) {
        Ok(mut world) => {
            // Apply the actor's in-memory mesh-cache budget (MiB → bytes) before the world moves to its
            // stream thread; `0` keeps the engine default.
            if info.mesh_cache_budget_mb > 0 {
                world.set_mesh_cache_budget(info.mesh_cache_budget_mb as usize * 1024 * 1024);
            }
            Some(LogiGlobeWorld {
                // Hand the world to its own stream thread (ADR-0012): it advances continuously OFF the game
                // thread; the game thread only pushes views + polls. 8 ms idle park (~125 Hz when idle; a
                // `set_view` unparks it so it advances promptly on camera motion).
                streamer: GlobeStreamer::spawn(world, Duration::from_millis(8)),
                current_frame: None,
                frame_tiles: Vec::new(),
                prev_keys: HashSet::new(),
                key_to_index: HashMap::new(),
                added_indices: Vec::new(),
                removed_keys: Vec::new(),
                #[cfg(debug_assertions)]
                consumer_thread: None,
            })
        }
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

impl LogiGlobeWorld {
    /// Debug-only single-consumer guard: pins the first thread to reach a consumer entry point and asserts
    /// every later `poll`/`set_view` comes from that same thread (see the type's doc-comment). This only
    /// catches an accidental cross-thread call in dev/tests; the contract stays the caller's to uphold.
    #[cfg(debug_assertions)]
    #[inline]
    fn assert_single_consumer(&mut self) {
        let me = std::thread::current().id();
        let first = *self.consumer_thread.get_or_insert(me);
        debug_assert_eq!(
            first, me,
            "LogiGlobeWorld is single-consumer: poll/set_view must be called from one thread (ADR-0012)"
        );
    }

    /// Release build: the single-consumer guard compiles to nothing (no field, no work).
    #[cfg(not(debug_assertions))]
    #[inline(always)]
    fn assert_single_consumer(&mut self) {}
}

/// Rebuild the C descriptor array (`frame_tiles`) from the retained `current_frame` AND compute the per-poll
/// delta (`added_indices` / `removed_keys`) against the previously emitted frame (`prev_keys`). The emitted
/// pointers index that frame's `Arc` geometry, which stays alive (valid for the renderer) until the next New
/// poll replaces `current_frame`. Called only on a New poll, so `prev_keys` tracks exactly the frame the
/// renderer last consumed — the diff is correct even when the stream thread superseded intermediate frames.
fn build_globe_frame(gw: &mut LogiGlobeWorld) {
    let LogiGlobeWorld {
        current_frame,
        frame_tiles,
        prev_keys,
        key_to_index,
        added_indices,
        removed_keys,
        ..
    } = gw;
    frame_tiles.clear();
    added_indices.clear();
    removed_keys.clear();
    key_to_index.clear();
    let Some(frame) = current_frame.as_ref() else {
        prev_keys.clear(); // no frame retained → the next New poll starts a fresh delta baseline (all-added)
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
            flags: if g.unlit { LOGI_GLOBE_TILE_FLAG_UNLIT } else { 0 },
            texture_jpeg: g.texture.as_ptr(),
            texture_jpeg_len: g.texture.len() as u64,
            bounds_center: g.bounds_center,
            bounds_radius: g.bounds_radius,
            depth: t.depth,
            geometric_error: t.geometric_error,
        });
    }
    // Diff vs the previously emitted frame. `key_to_index.insert(..).is_none()` is true only for the FIRST
    // occurrence of a key — multiple-contents tiles share a tile_key, and the C++ section cache keys on it, so we
    // dedup to the first occurrence (matching the renderer's "one component per key"). added = first-seen keys not
    // in prev; removed = prev keys absent from the current frame.
    key_to_index.reserve(frame_tiles.len());
    for (i, t) in frame_tiles.iter().enumerate() {
        if key_to_index.insert(t.tile_key, i as u32).is_none() && !prev_keys.contains(&t.tile_key) {
            added_indices.push(i as u32);
        }
    }
    for &k in prev_keys.iter() {
        if !key_to_index.contains_key(&k) {
            removed_keys.push(k);
        }
    }
    // prev := current keys, for the next poll's diff (reuse `prev_keys`' allocation).
    prev_keys.clear();
    prev_keys.extend(key_to_index.keys().copied());
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
        gw.assert_single_consumer(); // debug-only: poll/set_view must stay on one thread (ADR-0012)
        if view.is_null() {
            return LogiStatus::NullArg;
        }
        // Read unaligned into an aligned local: the C caller's struct pointer may not be 8-byte aligned, and a
        // reference deref (`as_ref`) would be UB on a misaligned `LogiGlobeView`. Matches the discipline every
        // other inbound boundary struct uses (LogiGlobeCreateInfo / LogiPlacement / the baked infos).
        let v = unsafe { view.read_unaligned() };
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
        gw.assert_single_consumer(); // debug-only: poll/set_view must stay on one thread (ADR-0012)
        if out_frame.is_null() {
            return LogiStatus::NullArg;
        }
        match gw.streamer.poll() {
            // A newer frame: retain it (its `Arc` geometry backs the pointers), rebuild the descriptors, and
            // compute the delta vs the frame the renderer last consumed.
            StreamPoll::New(frame) => {
                gw.current_frame = Some(frame);
                build_globe_frame(gw);
            }
            // No new frame: the previously built `frame_tiles` + retained geometry still stand (SAME). The
            // `frame_id` below is unchanged, so the renderer skips its reconcile; clear the delta so the counts
            // read 0 and a renderer that ignores `frame_id` still can't re-apply a stale delta.
            StreamPoll::Same => {
                gw.added_indices.clear();
                gw.removed_keys.clear();
            }
            // Nothing published yet. Reset everything, including the delta baseline.
            StreamPoll::Empty => {
                gw.current_frame = None;
                gw.frame_tiles.clear();
                gw.added_indices.clear();
                gw.removed_keys.clear();
                gw.key_to_index.clear();
                gw.prev_keys.clear();
            }
        }
        let origin_ecef = gw.current_frame.as_ref().map(|f| f.origin_ecef).unwrap_or([0.0; 3]);
        // Monotonic frame id (0 = empty). The renderer compares it to its last-applied id: equal ⇒ identical
        // scene ⇒ skip the reconcile entirely (the dominant camera-still case).
        let frame_id = gw.current_frame.as_ref().map(|f| f.frame).unwrap_or(0);
        let out = LogiGlobeFrame {
            tiles: gw.frame_tiles.as_ptr(),
            added_indices: gw.added_indices.as_ptr(),
            removed_keys: gw.removed_keys.as_ptr(),
            tile_count: gw.frame_tiles.len() as u32,
            added_count: gw.added_indices.len() as u32,
            removed_count: gw.removed_keys.len() as u32,
            resident_tiles: gw.streamer.resident_tiles() as u32,
            in_flight: gw.streamer.in_flight() as u32,
            _pad: 0,
            frame_id,
            origin_ecef,
        };
        unsafe { out_frame.write_unaligned(out) };
        LogiStatus::Ok
    })
}

/// Look up a single tile in the CURRENT polled frame by its `tile_key`, writing a zero-copy view into `out_tile`.
/// Returns `Ok` if the key is present in the frame last returned by `poll` (its buffers stay valid until the next
/// `poll`), or `InvalidHandle` for a null world / a key not in the current frame (`NullArg` for a null `out_tile`).
/// Lets the renderer re-drive a SPECIFIC tile's atlas decode (a bounded texture retry) without re-scanning the
/// whole frame — the delta path never visits already-resident tiles, so this is how it reaches one by key. Single-
/// consumer (the game thread), like `poll`/`set_view`.
///
/// # Safety
/// `world` must be null or a valid handle from [`logi_globe_world_create`]; `out_tile` must be null or point to
/// writable [`LogiGlobeTile`] storage (written unaligned). The written pointers borrow Rust-owned geometry, valid
/// until the next [`logi_globe_world_poll`] on this world.
#[no_mangle]
pub unsafe extern "C" fn logi_globe_world_tile_by_key(
    world: *mut LogiGlobeWorld,
    tile_key: u64,
    out_tile: *mut LogiGlobeTile,
) -> LogiStatus {
    guard(|| {
        let gw = match unsafe { world.as_mut() } {
            Some(w) => w,
            None => return LogiStatus::InvalidHandle,
        };
        gw.assert_single_consumer(); // debug-only: poll/set_view/tile_by_key must stay on one thread (ADR-0012)
        if out_tile.is_null() {
            return LogiStatus::NullArg;
        }
        match gw
            .key_to_index
            .get(&tile_key)
            .copied()
            .and_then(|i| gw.frame_tiles.get(i as usize))
        {
            Some(t) => {
                unsafe { out_tile.write_unaligned(*t) };
                LogiStatus::Ok
            }
            None => LogiStatus::InvalidHandle, // key not in the current frame (evicted / never present)
        }
    })
}

/// Bytes currently resident in the world's persistent disk cache (0 if none configured). Telemetry only.
///
/// # Safety
/// `world` must be null or a valid handle from [`logi_globe_world_create`].
#[no_mangle]
pub unsafe extern "C" fn logi_globe_world_cached_bytes(world: *const LogiGlobeWorld) -> u64 {
    // no-panic: a null check + one atomic load — cannot panic, so no `catch_unwind`. A future edit that
    // adds fallible work here must add a guard (an unwind across the C ABI is UB).
    match unsafe { world.as_ref() } {
        Some(gw) => gw.streamer.cached_bytes(),
        None => 0,
    }
}

/// Cumulative tiles that GAVE UP loading, packed `(decode_failed << 32) | fetch_failed`. Telemetry to
/// distinguish the two reasons a tile stays stuck at its coarse (blurry) ancestor: **fetch_failed** = the
/// bytes never arrived (dead URL / network error / expired P3DT session, after bounded retries);
/// **decode_failed** = bytes arrived but didn't parse (an unsupported format such as Draco/KTX2, or corrupt
/// content). A steady zero on both means stuck tiles are still in flight, not given up. `0` for a null handle.
///
/// # Safety
/// `world` must be null or a valid handle from [`logi_globe_world_create`].
#[no_mangle]
pub unsafe extern "C" fn logi_globe_world_failed_counts(world: *const LogiGlobeWorld) -> u64 {
    // no-panic: a null check + two atomic loads + mask/shift — cannot panic, so no `catch_unwind`. A
    // future edit that adds fallible work here must add a guard (an unwind across the C ABI is UB).
    match unsafe { world.as_ref() } {
        // Mask BOTH lanes: an unmasked high counter that ever exceeds 32 bits would spill into the low
        // lane and corrupt it. C++ unpacks as `decode_failed = v >> 32`, `fetch_failed = v & 0xFFFFFFFF`.
        Some(gw) => ((gw.streamer.decode_failed() & 0xFFFF_FFFF) << 32) | (gw.streamer.fetch_failed() & 0xFFFF_FFFF),
        None => 0,
    }
}

/// Cumulative tiles stuck coarse because **refinement couldn't descend** into them, packed
/// `(graft_failed << 32) | url_resolve_failed`. Unlike [`logi_globe_world_failed_counts`] (a fetch/decode
/// failure of a tile we DID try to load), these are tiles never even fetched: **graft_failed** = a child
/// `tileset.json` manifest fetched but didn't parse/graft (its subtree never appears); **url_resolve_failed**
/// = a content URI couldn't be resolved against its base. Both were SILENT before — a climbing count is the
/// cause of a region that stays blurry at any SSE target. `0` for a null handle.
///
/// # Safety
/// `world` must be null or a valid handle from [`logi_globe_world_create`].
#[no_mangle]
pub unsafe extern "C" fn logi_globe_world_refine_failures(world: *const LogiGlobeWorld) -> u64 {
    // no-panic: a null check + two atomic loads + mask/shift — cannot panic, so no `catch_unwind`. A
    // future edit that adds fallible work here must add a guard (an unwind across the C ABI is UB).
    match unsafe { world.as_ref() } {
        // Mask BOTH lanes (see logi_globe_world_failed_counts): a >32-bit high counter must not spill into
        // the low lane. C++ unpacks as `graft_failed = v >> 32`, `url_resolve_failed = v & 0xFFFFFFFF`.
        Some(gw) => {
            ((gw.streamer.graft_failed() & 0xFFFF_FFFF) << 32) | (gw.streamer.url_resolve_failed() & 0xFFFF_FFFF)
        }
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
