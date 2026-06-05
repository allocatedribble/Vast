//! The C-ABI boundary crate. Compiles to `logi_terrain.{dll,so,dylib}` — the library Unity loads.
//!
//! Thin by design: exposes the stable C ABI in `include/logi_terrain_ffi.h` for the **baked + procedural
//! terrain** lanes (`ffi_baked` — the one terrain engine), the **globe** + **building** lanes (`ffi`), and
//! the shared ABI-version / last-error accessors. (The old streaming `LogiTerrainWorld` from
//! `logi_terrain_scene` was retired in the one-engine merge.)
//!
//! The exported `extern "C"` entry points take raw pointers from the C caller and are therefore declared
//! `unsafe`; each pointer use is wrapped in an explicit `unsafe` block so it is individually auditable.
#![deny(unsafe_op_in_unsafe_fn)]

pub mod ffi;
// HE-0 (ADR-0011): additive C ABI for the baked semantic-tile world. Separate module; does not touch the
// terrain `LogiFramePacket` ABI (v3) — new symbols only, like the globe/building additions.
pub mod ffi_baked;

/// ABI version of the C boundary. Unity must verify `logi_terrain_abi_version()` matches before any other call.
/// v2 = the `LogiTerrainWorld` runtime model (was v1 `logi_kernel_*`).
/// v3 = glam-typed boundary: f32 local render data (Vec3/Mat4 ↔ Unity float3/float4x4), f64 origin only.
pub const LOGI_ABI_VERSION: u32 = 3;
