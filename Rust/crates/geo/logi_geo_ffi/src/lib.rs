//! The C-ABI boundary crate. Compiles to `logi_geo.{dll,so,dylib}` — the cdylib the C++ bridge delay-loads.
//!
//! Thin by design: exposes the stable C ABI in `include/logi_geo_ffi.h` for the **baked + procedural
//! terrain** lanes (`ffi_baked` — the one terrain engine), the **globe** + **building** lanes (`ffi`), and
//! the shared ABI-version / last-error accessors.
//!
//! The exported `extern "C"` entry points take raw pointers from the C caller and are therefore declared
//! `unsafe`; each pointer use is wrapped in an explicit `unsafe` block so it is individually auditable.
#![deny(unsafe_op_in_unsafe_fn)]

/// mimalloc backs every Rust allocation inside `logi_geo.dll`. It cuts fragmentation and speeds up
/// multithreaded alloc/free on the tile streaming/decode/mesh hot paths versus the system CRT allocator,
/// and it governs *only* this DLL's Rust heap — Unreal keeps its own allocator. Safe because the FFI hands
/// Unreal pointer+len views into Rust-owned buffers (§1A), never ownership, so nothing is freed across it.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

pub mod ffi;
// ADR-0011: C ABI for the baked semantic-tile world (and the procedural terrain lane).
pub mod ffi_baked;

/// ABI version of the C boundary. The C++ bridge must verify `logi_geo_abi_version()` matches before any other call.
/// v2 = the `LogiTerrainWorld` runtime model (was v1 `logi_kernel_*`).
/// v3 = glam-typed boundary: f32 local render data (Vec3/Mat4 ↔ Unreal FVector/FMatrix), f64 origin only.
/// v4 = `logi_globe_world_set_view`/`_step` carry the camera **forward** direction (3 f64) for the
///      view-direction fetch priority (plan §9.2 step 7).
/// v5 = globe `set_view` takes a `LogiGlobeView` struct (camera basis + aspect) for the view frustum
///      (frustum-aware streaming LOD, §9.2); the deprecated `logi_globe_world_step` shim is removed.
/// v6 = `LogiGlobeTile`'s former alignment pad becomes a `flags` field (same offset/size, bit 0 =
///      `LOGI_GLOBE_TILE_FLAG_UNLIT`, `KHR_materials_unlit`). Byte layout unchanged; bumped because C++ now
///      reads those 4 bytes as flags rather than ignoring them.
/// v7 = `LogiGlobeTile` + `LogiBakedTile` gain `depth` (u32, refinement tree depth / pyramid level, root = 0)
///      and `geometric_error` (f32, metres) for the Geo debug-visualisation + telemetry tools. The globe tile
///      grows 80 → 88; the baked tile reuses its former `_pad` slot for `depth` and appends `geometric_error`
///      (64 → 72). Render-lane only (non-authoritative, ADR-0007) — no effect on sim determinism.
/// v8 = `LogiGlobeFrame` carries a per-poll DELTA for O(Δ) planetary-scale reconcile: `added_indices`
///      (*u32 into `tiles`) + `removed_keys` (*u64) with their counts, plus a monotonic `frame_id` (0 = empty)
///      so the renderer skips reconcile when the scene is unchanged (48 → 80). New entry point
///      `logi_globe_world_tile_by_key` (targeted texture re-drive). Render-lane only — no sim-determinism effect.
pub const LOGI_ABI_VERSION: u32 = 8;
