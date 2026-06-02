//! The C-ABI boundary crate. Compiles to `logi_terrain.{dll,so,dylib}` — the library Unity loads.
//!
//! Thin by design (ADR-0001): it wraps the `LogiTerrainWorld` runtime object from `logi_terrain_scene` and
//! exposes the stable C ABI in `include/logi_terrain.h`. See `runtime-model.md` for the object model.

pub mod ffi;

/// ABI version of the C boundary. Unity must verify `logi_terrain_abi_version()` matches before any other call.
/// v2 = the `LogiTerrainWorld` runtime model (was v1 `logi_kernel_*`).
/// v3 = glam-typed boundary: f32 local render data (Vec3/Mat4 ↔ Unity float3/float4x4), f64 origin only.
pub const LOGI_ABI_VERSION: u32 = 3;
