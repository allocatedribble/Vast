//! The C-ABI entry points. Every call is wrapped so a Rust panic becomes a status code / null instead
//! of unwinding across FFI (UB). The handles are opaque boxed pointers — `*mut LogiGlobeWorld` (the
//! streamed-globe lane) and `*mut LogiBuilding` (the M1 building lane); null is invalid. (The old
//! streaming `LogiTerrainWorld` was retired in the one-engine merge.)

use std::cell::RefCell;
use std::panic::{self, AssertUnwindSafe};

/// Status codes returned across the boundary. Matches `LogiStatus` (int32) in `logi_geo_ffi.h`.
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
            set_last_error("panic caught at the logi_geo FFI boundary");
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

use logi_3dtiles_scene::{
    FetchBackend, GeoRenderFrame, GeoWorld, GlobeStreamer, StreamPoll, ViewParams, DEFAULT_MAX_RESIDENT,
    DEFAULT_TILE_POOL_SIZE,
};
use std::collections::{HashMap, HashSet};
#[cfg(debug_assertions)]
use std::thread::ThreadId;
use std::time::Duration;

/// FFI fetch-worker-count clamp: a garbage `info.workers` must not spawn unbounded OS threads, nor 0. The max
/// MUST match the C++ `GlobeFetchWorkers` UPROPERTY ClampMax and the ureq per-host idle-pool size in
/// `logi_3dtiles_io` (the pool must be ≥ the worker count so every worker keeps a warm keep-alive connection).
const MIN_FETCH_WORKERS: u32 = 1;
const MAX_FETCH_WORKERS: u32 = 64;

/// Idle park for the Rust-owned stream thread (ADR-0012): when the camera is still and nothing is in flight it
/// sleeps this long (~125 Hz) instead of spinning; a `set_view` unparks it so it advances promptly on motion.
const GEO_IDLE_PARK: Duration = Duration::from_millis(8);

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
    /// The latest frame polled from the stream thread — retained (an `Arc` share of the stream mailbox's
    /// frame, O(1) to take) so its geometry keeps backing the pointers in `frame_tiles` until the next New
    /// poll replaces it (valid-until-next-poll).
    current_frame: Option<std::sync::Arc<GeoRenderFrame>>,
    /// Scratch C descriptor array the returned [`LogiGlobeFrame::tiles`] points into (rebuilt each New poll).
    frame_tiles: Vec<LogiGlobeTile>,
    /// Hole-clip arena: the per-tile clip OBBs that [`LogiGlobeTile::clip_boxes`] pointers index into. One flat
    /// `Vec` for the whole frame (reserved to the exact total before the build loop, so appending never
    /// reallocates and the handed-out pointers stay valid until the next New poll — the same contract as the
    /// `Arc` geometry behind `frame_tiles`). Empty most frames (only coarse stand-ins contribute boxes).
    clip_arena: Vec<LogiClipBox>,
    /// Delta scratch (planetary-scale O(Δ) reconcile). `prev_keys` is the set of tile keys handed to the renderer
    /// on the LAST New poll; each New poll diffs the current frame against it to fill `added_indices` (indices into
    /// `frame_tiles` whose key is new) and `removed_keys` (keys that departed), so the renderer applies O(Δ) changes
    /// instead of re-scanning the whole visible set every frame. `key_to_index` is the current frame's key→index map
    /// — it dedups keys while diffing AND backs [`logi_globe_world_tile_by_key`] (a targeted texture re-drive).
    prev_keys: HashSet<u64>,
    key_to_index: HashMap<u64, u32>,
    added_indices: Vec<u32>,
    removed_keys: Vec<u64>,
    /// Planet-true sky anchor (ABI v15), computed once at create from the world's ENU origin: the WGS84
    /// earth center in UE render-space cm + the origin's geocentric surface radius in km. Read by
    /// [`logi_globe_world_sky_anchor`] so the renderer can anchor the SkyAtmosphere / VolumetricCloud planet
    /// model to the streamed globe (geodesy stays single-homed in Rust).
    sky_anchor: ([f64; 3], f64),
    /// The ENU frame of this world's origin — the SAME geodesy every streamed vertex was rebased with
    /// (single-homed); the sky lane uses it to convert the camera/sun between UE render space and ECEF.
    enu: logi_geospatial::EnuFrame,
    /// The LogiSky cell (ADR-0022 S2): atmosphere parameters + lazily-baked LUTs + the event-gated
    /// per-frame derivation. Lives here (game-thread cadence) because the sky is camera-coupled and the
    /// camera arrives via `set_view`; the ECS-resident move is S3+ (see the plan's S2 design deltas).
    sky: logi_sky::SkyState,
    /// The last camera pushed via `set_view` (UE cm) — the sky frame derives from it. `None` until the
    /// first view (the sky-frame call then reports "no frame yet").
    last_camera_ue_cm: Option<[f64; 3]>,
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
    /// resident set. `0` ⇒ the engine default ([`DEFAULT_FLASH_CACHE_BYTES`]). (Was a reserved pad field; it
    /// occupies the same 4 bytes, so the ABI size is unchanged.)
    pub flash_cache_budget_mb: u32,
    /// Persistent on-disk tile-cache budget in bytes; `0` disables the cache. The cache directory is the
    /// separate `cache_dir` argument to `logi_globe_world_create`.
    pub cache_budget_bytes: u64,
    /// Runtime tile-fetch backend knob (ABI v11): `0` = Auto (the [`CyperSource`] lane — HTTP/3 over QUIC via
    /// Alt-Svc, h2 fallback — for Google P3DT when the `cyper-h2` feature is compiled, else `ureq`
    /// `HttpSource`), `1` = force `ureq` `HttpSource` (HTTP/1.1), `2` = force the `CyperSource` lane (HTTP/3).
    /// Maps via [`FetchBackend::from_ffi`]. A zero-initialised create-info gets `Auto` — i.e. the new default
    /// (QUIC for Google tiles); forcing a value drives the live ureq-vs-h3 A/B. If `cyper-h2` was not compiled
    /// in, a Cyper request degrades gracefully to `ureq` (logged).
    pub fetch_backend: u32,
    /// Reserved (alignment pad to the struct's 8-byte alignment; future knob). Must be `0`.
    pub _reserved: u32,
}

/// [`LogiGlobeTile::flags`] bit 0: the tile's material is unlit (`KHR_materials_unlit`) — the renderer should
/// shade it emissive (it bakes its own lighting; P3DT bakes real-world sun/shadow into the albedo) and not
/// apply scene lighting on top. Mirrors `LOGI_GLOBE_TILE_FLAG_UNLIT` in `logi_geo_ffi.h`.
pub const LOGI_GLOBE_TILE_FLAG_UNLIT: u32 = 1;

/// `LogiGlobeTile.flags` bit 1: a "prepare" tile — geometry-resident but NOT yet textured. The renderer must
/// BUILD it (so it can decode the atlas) yet keep it HIDDEN: the selector's texture gate is still drawing the
/// coarse parent over its footprint, and showing the untextured tile would flash gray. Once the renderer
/// textures it and reports it via [`logi_globe_world_set_textured_keys`], the gate promotes it (next frame this
/// bit clears and it shows). Mirrors `LOGI_GLOBE_TILE_FLAG_HIDDEN` in `logi_geo_ffi.h`. ADVISORY for a renderer
/// that — like ours — already derives the same state locally (a tile stays invisible until its own atlas-confirm
/// lifecycle completes, `FadeT`/bloom); it is the authoritative mirror for a renderer without one.
pub const LOGI_GLOBE_TILE_FLAG_HIDDEN: u32 = 2;

/// One **hole-clip** oriented bounding box in Unreal render space (cm) — the geometric primitive the renderer
/// tests each fragment of a coarse stand-in tile against, discarding fragments inside any of the tile's boxes
/// so the coarse mesh never overlaps the finer tiles drawn over it (no z-fight, no Pixel Depth Offset). It is a
/// center plus three half-extent axis vectors; a point `p` is inside iff `|(p − center)·âᵢ| ≤ |aᵢ|` along each
/// axis `aᵢ`. Mirrors `LogiClipBox` in `logi_geo_ffi.h`. POD (`[f32;12]`), 48 bytes, align 4.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct LogiClipBox {
    pub center: [f32; 3],
    pub half_axes: [[f32; 3]; 3],
}

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
    /// **Geometric hole-clip** (ABI v12): zero-copy view of this tile's clip OBBs (valid until the next `poll`,
    /// like the geometry pointers). Non-null with `clip_box_count > 0` ONLY for a coarse REPLACE stand-in drawn
    /// over finer tiles that have loaded+textured; the renderer then selects the masked clip material and discards
    /// this tile's fragments inside any box. Null / 0 for the overwhelming majority of tiles — they render opaque
    /// with no clip.
    pub clip_boxes: *const LogiClipBox,
    pub clip_box_count: u32,
    /// Alignment pad to the struct's 8-byte alignment (the `clip_boxes` pointer). Always 0.
    pub _pad_clip: u32,
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

    // LogiGlobeCreateInfo — u64 + 4 × f64 + 2 × u32 + u64 + 2 × u32 (align 8), size 64 (was 56 before v11:
    // the `fetch_backend` runtime knob + a `_reserved` pad were appended; 64 is 8-aligned so no tail pad).
    assert!(std::mem::size_of::<LogiGlobeCreateInfo>() == 64);
    assert!(offset_of!(LogiGlobeCreateInfo, ion_asset_id) == 0);
    assert!(offset_of!(LogiGlobeCreateInfo, origin_longitude_deg) == 8);
    assert!(offset_of!(LogiGlobeCreateInfo, origin_latitude_deg) == 16);
    assert!(offset_of!(LogiGlobeCreateInfo, origin_height_m) == 24);
    assert!(offset_of!(LogiGlobeCreateInfo, max_tile_distance_m) == 32);
    assert!(offset_of!(LogiGlobeCreateInfo, workers) == 40);
    assert!(offset_of!(LogiGlobeCreateInfo, flash_cache_budget_mb) == 44);
    assert!(offset_of!(LogiGlobeCreateInfo, cache_budget_bytes) == 48);
    assert!(offset_of!(LogiGlobeCreateInfo, fetch_backend) == 56);
    assert!(offset_of!(LogiGlobeCreateInfo, _reserved) == 60);

    // LogiClipBox — [f32;3] center + [[f32;3];3] half-axes = 12 × f32 (align 4), size 48.
    assert!(std::mem::size_of::<LogiClipBox>() == 48);
    assert!(offset_of!(LogiClipBox, center) == 0);
    assert!(offset_of!(LogiClipBox, half_axes) == 12);

    // LogiGlobeTile — same shape as LogiBuildingData up to texture, then a depth (u32) + geometric_error (f32)
    // tail (v7), then the v12 hole-clip tail: a `clip_boxes` ptr (8) + `clip_box_count` (u32) + a `_pad_clip`
    // (u32) to the struct's 8-byte alignment. size 104 (was 88 before v12, 80 before v7).
    assert!(std::mem::size_of::<LogiGlobeTile>() == 104);
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
    assert!(offset_of!(LogiGlobeTile, clip_boxes) == 88);
    assert!(offset_of!(LogiGlobeTile, clip_box_count) == 96);
    assert!(offset_of!(LogiGlobeTile, _pad_clip) == 100);

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

    // LogiGlobeTelemetry (v17) — 26 × u64 (align 8), size 208; offsets are mechanical (index × 8).
    assert!(std::mem::size_of::<LogiGlobeTelemetry>() == 208);
    assert!(offset_of!(LogiGlobeTelemetry, resident_tiles) == 0);
    assert!(offset_of!(LogiGlobeTelemetry, in_flight) == 8);
    assert!(offset_of!(LogiGlobeTelemetry, produced_frames) == 16);
    assert!(offset_of!(LogiGlobeTelemetry, step_panics) == 24);
    assert!(offset_of!(LogiGlobeTelemetry, free_slots) == 32);
    assert!(offset_of!(LogiGlobeTelemetry, max_resident) == 40);
    assert!(offset_of!(LogiGlobeTelemetry, evicted_total) == 48);
    assert!(offset_of!(LogiGlobeTelemetry, pool_exhausted_total) == 56);
    assert!(offset_of!(LogiGlobeTelemetry, decode_failed_total) == 64);
    assert!(offset_of!(LogiGlobeTelemetry, fetch_failed_total) == 72);
    assert!(offset_of!(LogiGlobeTelemetry, graft_failed_total) == 80);
    assert!(offset_of!(LogiGlobeTelemetry, url_resolve_failed_total) == 88);
    assert!(offset_of!(LogiGlobeTelemetry, fetches_cancelled_total) == 96);
    assert!(offset_of!(LogiGlobeTelemetry, session_refreshes_total) == 104);
    assert!(offset_of!(LogiGlobeTelemetry, flash_hits) == 112);
    assert!(offset_of!(LogiGlobeTelemetry, flash_misses) == 120);
    assert!(offset_of!(LogiGlobeTelemetry, flash_bytes) == 128);
    assert!(offset_of!(LogiGlobeTelemetry, disk_hits) == 136);
    assert!(offset_of!(LogiGlobeTelemetry, disk_misses) == 144);
    assert!(offset_of!(LogiGlobeTelemetry, disk_bytes) == 152);
    assert!(offset_of!(LogiGlobeTelemetry, hold_kick) == 160);
    assert!(offset_of!(LogiGlobeTelemetry, hold_dead) == 168);
    assert!(offset_of!(LogiGlobeTelemetry, hold_loading) == 176);
    assert!(offset_of!(LogiGlobeTelemetry, hold_leaf_under_sse) == 184);
    assert!(offset_of!(LogiGlobeTelemetry, hold_pending_subtree) == 192);
    assert!(offset_of!(LogiGlobeTelemetry, fetch_skip_grafted) == 200);
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
    // `_reserved` is documented "must be 0" but was never checked: surface a mis-filled struct (stale C++
    // header vs DLL within a matching ABI version) in dev builds at create time, where the per-field
    // `offset_of!` locks can't see VALUES (re-eval 2026-06-09). Zero release cost; promote to a hard
    // `set_last_error` + reject when `_reserved` is ever repurposed into a real knob.
    debug_assert!(
        info._reserved == 0,
        "LogiGlobeCreateInfo._reserved must be 0 (got {}) — mis-filled create struct?",
        info._reserved
    );
    let token = match unsafe { CStr::from_ptr(ion_token) }.to_str() {
        Ok(t) => t,
        Err(_) => {
            set_last_error("logi_globe_world_create: ion token is not valid UTF-8");
            return None;
        }
    };
    // THE optional persistent disk cache — one cell, two tiers (bytes + decoded geometry), one budget
    // (`cache_budget_bytes`, split inside the cell). A cache-open failure degrades to no-cache / bytes-only
    // (the globe still streams). The tiers flow to their runtime owners below: bytes → the fetch source,
    // geometry → the Geo World's restore path.
    let disk = build_globe_disk_cache(info.cache_budget_bytes, cache_dir);
    let (cache, geom_cache) = match &disk {
        Some(cell) => (Some(std::sync::Arc::clone(&cell.bytes)), cell.geom.clone()),
        None => (None, None),
    };
    // The Geo World pre-spawns a fixed tile-entity pool; size it above the resident cap + in-flight slack so
    // it never exhausts in steady state. (Decode-failed tiles linger in the pool until a cleanup pass —
    // TODO; the headroom covers it for now.)
    match GeoWorld::from_ion(
        info.ion_asset_id,
        token,
        info.origin_longitude_deg,
        info.origin_latitude_deg,
        info.origin_height_m,
        info.workers.clamp(MIN_FETCH_WORKERS, MAX_FETCH_WORKERS) as usize,
        // Runtime fetch-backend knob (ABI v11): 0=Auto (Cyper/QUIC lane for Google P3DT), 1=ureq, 2=Cyper(h3).
        FetchBackend::from_ffi(info.fetch_backend),
        info.max_tile_distance_m,
        cache,
        DEFAULT_TILE_POOL_SIZE, // pre-spawned tile-entity pool (see the const for the sizing rationale)
        DEFAULT_MAX_RESIDENT,   // resident-tile cap (mainly bounds the off-view shell; pool above is the ceiling)
    ) {
        Ok(mut world) => {
            // Apply the actor's in-memory flash-cache budget (MiB → bytes) before the world moves to its
            // stream thread; `0` keeps the engine default.
            if info.flash_cache_budget_mb > 0 {
                world.set_flash_cache_budget(info.flash_cache_budget_mb as usize * 1024 * 1024);
            }
            // Install the persisted decoded-geometry tier (audit M3), if enabled (a `geom` subdir of the cache dir).
            world.set_geom_cache(geom_cache);
            // Planet-true sky anchor (ABI v15): the WGS84 earth center in UE render space under this
            // world's ENU origin + the origin's geocentric radius — same kernel geodesy as the rebase.
            let ellipsoid = logi_geospatial::Ellipsoid::wgs84();
            let enu = logi_geospatial::EnuFrame::from_cartographic(
                &ellipsoid,
                logi_geospatial::Cartographic::from_degrees(
                    info.origin_longitude_deg,
                    info.origin_latitude_deg,
                    info.origin_height_m,
                ),
            );
            let sky_anchor = {
                let c = logi_geo_mesh::enu_pos_to_ue(enu.ecef_to_local(glam::DVec3::ZERO));
                let (cx, cy, cz) = (f64::from(c[0]), f64::from(c[1]), f64::from(c[2]));
                ([cx, cy, cz], (cx * cx + cy * cy + cz * cz).sqrt() / 100_000.0)
            };
            Some(LogiGlobeWorld {
                sky_anchor,
                enu,
                sky: logi_sky::SkyState::new(logi_sky::AtmosphereParams::earth()),
                last_camera_ue_cm: None,
                // Hand the world to its own stream thread (ADR-0012): it advances continuously OFF the game
                // thread; the game thread only pushes views + polls. `GEO_IDLE_PARK` governs the idle cadence.
                streamer: GlobeStreamer::spawn(world, GEO_IDLE_PARK),
                current_frame: None,
                frame_tiles: Vec::new(),
                clip_arena: Vec::new(),
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

/// Build THE persistent disk cache — ONE cell ([`logi_3dtiles_io::TileDiskCache`], ADR-0019): one root
/// (`cache_dir`), one total budget (`info.cache_budget_bytes`, split evenly across the byte + geometry
/// tiers inside the cell), aggregate telemetry at the membrane. Returns `None` (no disk caching at all)
/// when the budget is 0, the dir is null/empty, or the BYTE tier can't open; a geometry-tier-only failure
/// degrades inside the cell (bytes-only, re-decoding on miss) and is logged here.
fn build_globe_disk_cache(budget: u64, cache_dir: *const c_char) -> Option<logi_3dtiles_io::TileDiskCache> {
    if budget == 0 || cache_dir.is_null() {
        return None;
    }
    let dir = unsafe { CStr::from_ptr(cache_dir) }.to_str().ok()?;
    if dir.is_empty() {
        return None;
    }
    match logi_3dtiles_io::TileDiskCache::open(dir, budget) {
        Ok((cell, geom_error)) => {
            if let Some(e) = geom_error {
                set_last_error(format!(
                    "globe geom cache open '{dir}/geom': {e} (streaming without the geometry tier)"
                ));
            }
            Some(cell)
        }
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
        clip_arena,
        prev_keys,
        key_to_index,
        added_indices,
        removed_keys,
        ..
    } = gw;
    frame_tiles.clear();
    clip_arena.clear();
    added_indices.clear();
    removed_keys.clear();
    key_to_index.clear();
    let Some(frame) = current_frame.as_ref() else {
        prev_keys.clear(); // no frame retained → the next New poll starts a fresh delta baseline (all-added)
        return;
    };
    frame_tiles.reserve(frame.tiles.len());
    // Reserve the hole-clip arena to the EXACT total up front, so the per-tile `clip_boxes` pointers taken below
    // index a Vec that never reallocates during the build loop — they stay valid until the next New poll, exactly
    // like the `Arc` geometry pointers. (Almost always 0 — only coarse stand-ins carry boxes.)
    clip_arena.reserve(frame.tiles.iter().map(|t| t.clip_boxes.len()).sum());
    for t in &frame.tiles {
        let g = &t.geom;
        // H5: count + stride come from the SAME buffers we emit pointers into → no OOB GPU upload.
        debug_assert!(
            g.vertices.len() % VERTEX_STRIDE == 0,
            "tile vertex buffer not a VERTEX_STRIDE multiple"
        );
        // Append this tile's clip OBBs to the (pre-reserved → non-reallocating) arena, then hand out a pointer to
        // its slice. `null`/0 for the common no-clip tile; the renderer reads `clip_box_count` boxes from the ptr.
        let clip_start = clip_arena.len();
        for b in &t.clip_boxes {
            clip_arena.push(LogiClipBox {
                center: b.center,
                half_axes: b.half_axes,
            });
        }
        let clip_box_count = t.clip_boxes.len() as u32;
        let clip_boxes = if clip_box_count > 0 {
            // SAFETY: `clip_start <= clip_arena.len()` and the reserve above guarantees no reallocation, so this
            // points at a valid run of `clip_box_count` elements that lives until the next New poll.
            unsafe { clip_arena.as_ptr().add(clip_start) }
        } else {
            core::ptr::null()
        };
        frame_tiles.push(LogiGlobeTile {
            tile_key: t.tile_key,
            vertices: g.vertices.as_ptr(),
            vertices_len: g.vertices.len() as u64,
            vertex_count: (g.vertices.len() / VERTEX_STRIDE) as u32,
            vertex_stride: VERTEX_STRIDE as u32,
            indices: g.indices.as_ptr() as *const u8,
            index_count: g.indices.len() as u32,
            // bit 0 UNLIT (material), bit 1 HIDDEN ("prepare" tile — built but not shown until textured; the
            // texture gate is still drawing its coarse parent). `visible` is the Rust-side render-vs-prepare bit.
            flags: (if g.unlit { LOGI_GLOBE_TILE_FLAG_UNLIT } else { 0 })
                | (if t.visible { 0 } else { LOGI_GLOBE_TILE_FLAG_HIDDEN }),
            texture_jpeg: g.texture.as_ptr(),
            texture_jpeg_len: g.texture.len() as u64,
            bounds_center: g.bounds_center,
            bounds_radius: g.bounds_radius,
            depth: t.depth,
            geometric_error: t.geometric_error,
            clip_boxes,
            clip_box_count,
            _pad_clip: 0,
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
        // Boundary sanitation (FFI-edge probe, 2026-06-09): a non-finite field or a degenerate target must
        // not reach the selector. The failure shapes differ and neither is a crash, but the bad ones are
        // expensive: a NaN camera zeroes every distance via `f64::max` (NaN.max(0) == 0) → SSE explodes →
        // over-refine storm; `max_sse <= 0` fails the SSE test everywhere → descend-to-depth-cap fetch storm
        // (bounded by the caps, but pure waste). `viewport_height <= 0` is meaningless. A bad view is
        // DROPPED — newest-wins means the last good view simply stays current (frozen-but-stable) — and
        // asserted in dev builds so the caller bug is loud (the guard maps the panic to `LogiStatus::Panic`).
        let finite = [
            v.camera_x_cm,
            v.camera_y_cm,
            v.camera_z_cm,
            v.dir_x,
            v.dir_y,
            v.dir_z,
            v.up_x,
            v.up_y,
            v.up_z,
            v.aspect,
            v.fov_y_radians,
            v.viewport_height,
            v.max_sse_pixels,
        ]
        .iter()
        .all(|f| f.is_finite());
        if !finite || v.max_sse_pixels <= 0.0 || v.viewport_height <= 0.0 {
            debug_assert!(
                false,
                "logi_globe_world_set_view: non-finite view field, max_sse <= 0, or viewport_height <= 0 — view dropped"
            );
            return LogiStatus::Ok; // release: keep the last good view rather than poison selection
        }
        // The sky lane derives from the same camera the selector streams against (one observer, one truth).
        gw.last_camera_ue_cm = Some([v.camera_x_cm, v.camera_y_cm, v.camera_z_cm]);
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

/// Report the COMPLETE set of tile keys the renderer has fully TEXTURED (geometry was already resident; this
/// adds the texture-ready bit). The stream thread's selector treats a tile as renderable — eligible to replace
/// its coarse parent — only when it is BOTH geometry-resident AND in this set, so the coarse parent stays drawn
/// (real imagery) until the fine tile is textured: LOD loading never shows a gray placeholder. Newest-wins —
/// pass the WHOLE current set whenever it changes (a dropped update just costs one extra frame of coarse cover,
/// never gray). Producer half of the texture gate; single-consumer (game thread), like `set_view`/`poll`.
///
/// # Safety
/// `world` must be a valid handle from [`logi_globe_world_create`] (or null → `InvalidHandle`); `keys` must be
/// null (→ treated as the empty set) or point to `count` readable `u64`s.
#[no_mangle]
pub unsafe extern "C" fn logi_globe_world_set_textured_keys(
    world: *mut LogiGlobeWorld,
    keys: *const u64,
    count: u32,
) -> LogiStatus {
    guard(|| {
        let gw = match unsafe { world.as_mut() } {
            Some(w) => w,
            None => return LogiStatus::InvalidHandle,
        };
        gw.assert_single_consumer(); // debug-only: poll/set_view/set_textured_keys must stay on one thread
        let keys: &[u64] = if keys.is_null() || count == 0 {
            &[]
        } else {
            // Read as a slice; the keys are plain u64 with no alignment concern beyond `u64` (the caller's
            // TArray<uint64> is 8-aligned). Empty when null/zero so the set simply clears.
            unsafe { std::slice::from_raw_parts(keys, count as usize) }
        };
        gw.streamer.set_textured_keys(keys);
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
        // The per-poll header counters ride the v16 telemetry block (one brief lock; the full block is
        // available via `logi_globe_world_telemetry`).
        let t = gw.streamer.telemetry();
        let out = LogiGlobeFrame {
            tiles: gw.frame_tiles.as_ptr(),
            added_indices: gw.added_indices.as_ptr(),
            removed_keys: gw.removed_keys.as_ptr(),
            tile_count: gw.frame_tiles.len() as u32,
            added_count: gw.added_indices.len() as u32,
            removed_count: gw.removed_keys.len() as u32,
            resident_tiles: t.resident_tiles as u32,
            in_flight: t.in_flight as u32,
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

/// The Geo streaming lane's TELEMETRY BLOCK (v16, reshaped at v17 to the TWO-cache model) — every
/// streaming counter crossing the C ABI as ONE `#[repr(C)]` struct, replacing the v10–v15 accessor sprawl
/// (`_cached_bytes`, `_failed_counts`, `_refine_failures`, `_flash/_disk/_geom_cache_stats`,
/// `_pool_stats`, `_evict_stats`, `_hold_stats`, `_warm_cache_bytes` — all DELETED, ADR-0004 no-compat).
/// Field-for-field mirror of the scene's `GeoTelemetry` (same names, same order); every field is u64, so
/// the layout is mechanical (offset = index × 8, size = 26 × 8 = 208). Filled by
/// [`logi_globe_world_telemetry`].
#[repr(C)]
#[derive(Clone, Copy)]
pub struct LogiGlobeTelemetry {
    /// Resident decoded tiles after the last step.
    pub resident_tiles: u64,
    /// Outstanding/undrained tile fetches.
    pub in_flight: u64,
    /// Frames published into the stream mailbox (`produced - polled` ≈ intentional latest-wins skips).
    pub produced_frames: u64,
    /// Stream-thread steps that panicked and were caught (the lane keeps running). A rapid climb = a
    /// deterministically-bad tile/input. First mirrored at v16 (previously waited for "the next bump").
    pub step_panics: u64,
    /// Pool pressure: free slots now / the LRU cap / cumulative Resident→Free reclaims / cumulative
    /// admissions dropped for want of a slot. `pool_exhausted_total` climbing while `evicted_total` stays
    /// flat at a full pool = the 2026-06-11 starvation-stall signature (eviction dead, streamer starved).
    pub free_slots: u64,
    pub max_resident: u64,
    pub evicted_total: u64,
    pub pool_exhausted_total: u64,
    /// Give-ups — why a region stays blurry: bytes didn't parse (terminal) / bytes never arrived after
    /// retries (dead URL, network, expired session) / a child manifest fetched but never grafted (its
    /// subtree never appears) / a content URI never resolved against its base (never even fetched).
    pub decode_failed_total: u64,
    pub fetch_failed_total: u64,
    pub graft_failed_total: u64,
    pub url_resolve_failed_total: u64,
    /// In-flight fetches pre-emptively cancelled because their tile left the selection (fast camera
    /// motion) — bandwidth saved from already-stale tiles. First crossed the ABI at v16.
    pub fetches_cancelled_total: u64,
    /// Google P3DT session refreshes (root re-fetched for a fresh `?session=…` after a 401/403). Climbing
    /// on a long session is NORMAL; climbing alongside `fetch_failed_total` means the refresh isn't
    /// recovering (bad key / network). First crossed the ABI at v16.
    pub session_refreshes_total: u64,
    /// The TWO caches (v17, owner directive + ADR-0019): hit-rate = hits/(hits+misses); `*_bytes` =
    /// occupancy. `flash` = the in-memory cell (decoded-geometry LRU). `disk` = the disk cell's MEMBRANE
    /// aggregate over its two internal tiers (raw bytes + persisted decoded geometry): a hit is ANY tier
    /// serving (a geometry restore that skipped fetch+decode, or cached bytes that skipped the network);
    /// a miss is a request that ultimately needed the network.
    pub flash_hits: u64,
    pub flash_misses: u64,
    pub flash_bytes: u64,
    pub disk_hits: u64,
    pub disk_misses: u64,
    pub disk_bytes: u64,
    /// The selection X-RAY: why is each coarser-than-SSE region held there? SNAPSHOT of the last full
    /// selection step (see `logi_3dtiles_select::HoldCounts`). A stuck-coarse SCAR = `hold_leaf_under_sse`
    /// or `fetch_skip_grafted` sustained while the pipeline is quiescent; `hold_kick`/`hold_loading` are
    /// normal during convergence and fall to zero at settle.
    pub hold_kick: u64,
    pub hold_dead: u64,
    pub hold_loading: u64,
    pub hold_leaf_under_sse: u64,
    pub hold_pending_subtree: u64,
    pub fetch_skip_grafted: u64,
}

/// Fill `out` with the streaming lane's WHOLE telemetry block (v17) — one call per HUD/bench sample in
/// place of the ten deleted per-counter accessors. Returns `size_of::<LogiGlobeTelemetry>()` (208) on
/// success, `0` for a null world/out; the caller should check the return against its own `sizeof` as a
/// layout handshake.
///
/// # Safety
/// `world` must be null or a valid handle from [`logi_globe_world_create`]; `out` must be null or point to
/// writable [`LogiGlobeTelemetry`] storage (written unaligned).
#[no_mangle]
pub unsafe extern "C" fn logi_globe_world_telemetry(
    world: *const LogiGlobeWorld,
    out: *mut LogiGlobeTelemetry,
) -> usize {
    // no-panic: a brief mutex lock + flat field copies — cannot panic, so no `catch_unwind` (the same
    // documented convention as the other no-panic telemetry entry points). A future edit that adds
    // fallible work must add a guard (an unwind across the C ABI is UB).
    let (Some(gw), false) = (unsafe { world.as_ref() }, out.is_null()) else {
        return 0;
    };
    let t = gw.streamer.telemetry();
    let mirrored = LogiGlobeTelemetry {
        resident_tiles: t.resident_tiles,
        in_flight: t.in_flight,
        produced_frames: t.produced_frames,
        step_panics: t.step_panics,
        free_slots: t.free_slots,
        max_resident: t.max_resident,
        evicted_total: t.evicted_total,
        pool_exhausted_total: t.pool_exhausted_total,
        decode_failed_total: t.decode_failed_total,
        fetch_failed_total: t.fetch_failed_total,
        graft_failed_total: t.graft_failed_total,
        url_resolve_failed_total: t.url_resolve_failed_total,
        fetches_cancelled_total: t.fetches_cancelled_total,
        session_refreshes_total: t.session_refreshes_total,
        flash_hits: t.flash_hits,
        flash_misses: t.flash_misses,
        flash_bytes: t.flash_bytes,
        disk_hits: t.disk_hits,
        disk_misses: t.disk_misses,
        disk_bytes: t.disk_bytes,
        hold_kick: t.hold_kick,
        hold_dead: t.hold_dead,
        hold_loading: t.hold_loading,
        hold_leaf_under_sse: t.hold_leaf_under_sse,
        hold_pending_subtree: t.hold_pending_subtree,
        fetch_skip_grafted: t.fetch_skip_grafted,
    };
    unsafe { out.write_unaligned(mirrored) };
    std::mem::size_of::<LogiGlobeTelemetry>()
}

/// Planet-true SKY ANCHOR (ABI v15): fills `out` with up to `len` of
/// `[earth_center_x_cm, earth_center_y_cm, earth_center_z_cm, geocentric_radius_km]` — the WGS84 earth
/// center in UE render space under this world's ENU origin, and the origin's geocentric surface radius.
/// The renderer anchors the SkyAtmosphere planet model with these (`PlanetCenterAtComponentTransform` at
/// the center, `BottomRadius` = the radius; the VolumetricCloud layer follows the atmosphere's planet), so
/// the sky/cloud shells track the streamed WGS84 globe instead of a 6,360 km sphere pinned at the UE world
/// origin — the 2026-06-11 "settled-shot collapse" (clouds engulfing station cameras, detached limb bands,
/// ±15 km latitude-varying misplacement). Constant per world (computed at create). Returns the count
/// written (4, or 0 for a null handle/out).
///
/// # Safety
/// `world` must be null or a valid handle from [`logi_globe_world_create`]; `out` must be null or valid for
/// `len` writes.
#[no_mangle]
pub unsafe extern "C" fn logi_globe_world_sky_anchor(world: *const LogiGlobeWorld, out: *mut f64, len: usize) -> usize {
    // no-panic: field reads + bounded writes — cannot panic, so no `catch_unwind` (the same documented
    // convention as `logi_globe_world_telemetry`). A future fallible edit must add a guard (FFI unwind = UB).
    let (Some(gw), false) = (unsafe { world.as_ref() }, out.is_null()) else {
        return 0;
    };
    let ([cx, cy, cz], r_km) = gw.sky_anchor;
    let vals = [cx, cy, cz, r_km];
    let n = vals.len().min(len);
    for (i, v) in vals.iter().take(n).enumerate() {
        unsafe { out.add(i).write(*v) };
    }
    n
}

/// The LogiSky per-frame block (ABI v18 — ADR-0022 S2): everything camera/sun-dependent the renderer
/// needs, derived in Rust from the SAME camera `set_view` last pushed (one observer, one truth) plus the
/// atmosphere parameters the shader evaluates with. Per ADR-0023 §7 the block carries the UN-COLLAPSED
/// family — NO quality/tier field crosses here; the observing pass picks its formula per view. Positions/
/// directions are UE render space (the same ENU→UE map every streamed vertex used); the sun is the S2
/// zenith-follow policy (computed in Rust — sun truth gains a real model in S3).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct LogiSkyFrame {
    /// The WGS84 earth center in UE render space (cm) — the AP pass's ray∩ellipsoid anchor (= the v15 anchor).
    pub earth_center_ue_cm: [f64; 3],
    /// Geodetic (ellipsoid) camera altitude in metres — the LUT altitude coordinate (ADR-0022 law 3).
    pub camera_altitude_m: f64,
    /// Sun direction (unit, UE render space, pointing AT the sun).
    pub sun_dir_ue: [f64; 3],
    /// Geodetic zenith at the camera (unit, UE render space).
    pub up_ue: [f64; 3],
    pub cos_sun_zenith: f64,
    /// cos of zenith→horizon at this altitude (negative below horizontal) — the sky-view horizon split.
    pub horizon_cos: f64,
    pub bottom_radius_m: f64,
    pub top_radius_m: f64,
    /// LUT generation this frame was derived against — re-fetch + re-upload the LUTs iff it changed.
    pub lut_generation: u64,
    // --- Atmosphere parameters the shader's in-scatter evaluation needs (f32 — shader-bound) ---------
    pub rayleigh_scattering_per_m: [f32; 3],
    pub rayleigh_scale_height_m: f32,
    pub mie_scattering_per_m: f32,
    pub mie_absorption_per_m: f32,
    pub mie_g: f32,
    pub mie_scale_height_m: f32,
    pub ozone_absorption_per_m: [f32; 3],
    pub ozone_center_m: f32,
    pub ozone_half_width_m: f32,
    pub _pad: f32,
}

/// Zero-copy views of the LogiSky LUTs (ABI v18): flat RGBA `f32` rows, baked LAZILY on first call
/// (ADR-0019 §8 — the un-evaluated state collapses on observation) and STABLE until the world is
/// destroyed (no parameter-set entry exists in S2, so the generation can only change with one). The
/// renderer uploads each buffer as one flat texture copy and re-uploads only when `generation` moves.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct LogiSkyLuts {
    pub transmittance: *const f32,
    /// Float count (= width × height × 4 RGBA).
    pub transmittance_len: u64,
    pub transmittance_width: u32,
    pub transmittance_height: u32,
    pub multi_scattering: *const f32,
    pub multi_scattering_len: u64,
    pub multi_scattering_width: u32,
    pub multi_scattering_height: u32,
    pub generation: u64,
}

/// Fill `out` with the LogiSky per-frame block, derived (event-gated in Rust — a parked camera costs two
/// compares) from the last `set_view` camera under the S2 zenith-sun policy. Returns
/// `size_of::<LogiSkyFrame>()` (176) on success, `0` for a null world/out OR before the first `set_view`
/// (no camera ⇒ no sky frame yet). Single-consumer (game thread), like `poll`/`set_view`.
///
/// # Safety
/// `world` must be null or a valid handle from [`logi_globe_world_create`]; `out` must be null or point
/// to writable [`LogiSkyFrame`] storage (written unaligned).
#[no_mangle]
pub unsafe extern "C" fn logi_globe_world_sky_frame(world: *mut LogiGlobeWorld, out: *mut LogiSkyFrame) -> usize {
    let result = panic::catch_unwind(AssertUnwindSafe(|| {
        let (Some(gw), false) = (unsafe { world.as_mut() }, out.is_null()) else {
            return 0;
        };
        gw.assert_single_consumer();
        let Some(cam_ue) = gw.last_camera_ue_cm else {
            return 0; // no view pushed yet — the renderer keeps whatever sky it has
        };
        let cam_ecef = gw.enu.local_to_ecef(logi_geo_mesh::ue_cm_to_enu(cam_ue));
        // S2 sun policy: zenith-follow — `derive` maps a zero sun to the geodetic up (local solar noon,
        // matching the imagery's baked ~noon lighting). S3 replaces this with a real sun model.
        let f = gw.sky.derive(cam_ecef, glam::DVec3::ZERO);
        // ECEF direction → UE render space: rotate into ENU, then negate North (the same convention as
        // `enu_dir_to_ue`, kept in f64 here).
        let to_ue = |d: glam::DVec3| {
            let v = gw.enu.ecef_to_enu * d;
            [v.x, -v.y, v.z]
        };
        let p = *gw.sky.params();
        let frame = LogiSkyFrame {
            earth_center_ue_cm: gw.sky_anchor.0,
            camera_altitude_m: f.camera_altitude_m,
            sun_dir_ue: to_ue(f.sun_dir_ecef),
            up_ue: to_ue(f.up_ecef),
            cos_sun_zenith: f.cos_sun_zenith,
            horizon_cos: f.horizon_cos,
            bottom_radius_m: f.bottom_radius_m,
            top_radius_m: f.top_radius_m,
            lut_generation: f.lut_generation,
            rayleigh_scattering_per_m: p.rayleigh_scattering_per_m.map(|v| v as f32),
            rayleigh_scale_height_m: p.rayleigh_scale_height_m as f32,
            mie_scattering_per_m: p.mie_scattering_per_m as f32,
            mie_absorption_per_m: p.mie_absorption_per_m as f32,
            mie_g: p.mie_g as f32,
            mie_scale_height_m: p.mie_scale_height_m as f32,
            ozone_absorption_per_m: p.ozone_absorption_per_m.map(|v| v as f32),
            ozone_center_m: p.ozone_center_m as f32,
            ozone_half_width_m: p.ozone_half_width_m as f32,
            _pad: 0.0,
        };
        unsafe { out.write_unaligned(frame) };
        std::mem::size_of::<LogiSkyFrame>()
    }));
    match result {
        Ok(n) => n,
        Err(_) => {
            set_last_error("panic caught in logi_globe_world_sky_frame");
            0
        }
    }
}

/// Fill `out` with zero-copy views of the LogiSky LUTs, baking them on first call (a one-off CPU cost on
/// parameter generations, never per frame). Returns `size_of::<LogiSkyLuts>()` (56) on success, `0` for a
/// null world/out. The pointed-to buffers stay valid until `logi_globe_world_destroy`. Single-consumer.
///
/// # Safety
/// `world` must be null or a valid handle from [`logi_globe_world_create`]; `out` must be null or point
/// to writable [`LogiSkyLuts`] storage (written unaligned). The written pointers borrow Rust-owned
/// buffers valid until the world is destroyed.
#[no_mangle]
pub unsafe extern "C" fn logi_globe_world_sky_luts(world: *mut LogiGlobeWorld, out: *mut LogiSkyLuts) -> usize {
    let result = panic::catch_unwind(AssertUnwindSafe(|| {
        let (Some(gw), false) = (unsafe { world.as_mut() }, out.is_null()) else {
            return 0;
        };
        gw.assert_single_consumer();
        let luts = gw.sky.luts(); // lazy bake (ADR-0019 §8): first observation collapses the derivation
        let view = LogiSkyLuts {
            transmittance: luts.transmittance.as_ptr(),
            transmittance_len: luts.transmittance.len() as u64,
            transmittance_width: logi_sky::TRANSMITTANCE_W as u32,
            transmittance_height: logi_sky::TRANSMITTANCE_H as u32,
            multi_scattering: luts.multi_scattering.as_ptr(),
            multi_scattering_len: luts.multi_scattering.len() as u64,
            multi_scattering_width: logi_sky::MULTI_SCATTERING_W as u32,
            multi_scattering_height: logi_sky::MULTI_SCATTERING_H as u32,
            generation: luts.generation,
        };
        unsafe { out.write_unaligned(view) };
        std::mem::size_of::<LogiSkyLuts>()
    }));
    match result {
        Ok(n) => n,
        Err(_) => {
            set_last_error("panic caught in logi_globe_world_sky_luts");
            0
        }
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
