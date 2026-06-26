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
/// v9 = invisible LOD loading (the texture gate). New entry point `logi_globe_world_set_textured_keys` (the
///      renderer reports which tiles it has textured); the selector treats a tile as renderable only when it is
///      geometry-resident AND textured, so the coarse parent holds until the fine tile is textured — no gray
///      placeholder. `LogiGlobeTile.flags` gains bit 1 = `LOGI_GLOBE_TILE_FLAG_HIDDEN` (a built-but-not-yet-shown
///      "prepare" tile). Byte layout UNCHANGED (new function + reused flags bit); bumped because the renderer now
///      MUST call `set_textured_keys` — a stale C++ that doesn't would render nothing. Render-lane only.
/// v10 = cache-effectiveness telemetry + the persisted decoded-geometry tier. New ADDITIVE entry points
///       `logi_globe_world_flash_cache_stats` / `_disk_cache_stats` / `_geom_cache_stats` (each packed
///       `(hits<<32)|misses`) + `_warm_cache_bytes` (packed `(flash_mib<<32)|geom_mib`) so the host can observe
///       hit-rates + occupancy and tune the budgets (audit L1). The geometry tier (audit M3) persists decoded
///       `TileGeometry` under `<cache_dir>/geom` keyed by `(origin, tile_key)`, so a cold revisit skips the
///       Draco decode — enabled transparently when the disk cache is on (no new create-info field). Byte layout
///       of every existing struct UNCHANGED (new functions only); bumped to advertise the new surface. Render-lane.
/// v11 = runtime tile-fetch backend knob. `LogiGlobeCreateInfo` gains `fetch_backend` (u32: 0=Auto, 1=ureq,
///       2=CyperSource) + a `_reserved` pad (56 → 64). The `CyperSource` lane — HTTP/3 over QUIC via Alt-Svc
///       auto-upgrade (compio-quic = quinn-proto), h2 over compio-net + rustls as bootstrap/fallback — is the
///       DEFAULT for Google tiles (`Auto`); the knob forces ureq vs cyper for live A/Bs. Adopted on structural
///       grounds (connection economy, 1-RTT connect): the keyed-P3DT A/B measured the win as cold-connect/RTT
///       collapse with ~parity on warm real-body bursts. A zero-init create-info gets `Auto`. Affects only the
///       fetch lane (non-authoritative, ADR-0007); no sim-determinism effect.
/// v12 = geometric hole-clip. `LogiGlobeTile` gains a `clip_boxes` (*const LogiClipBox) + `clip_box_count`
///       (u32) + `_pad_clip` (u32) tail (88 → 104), plus a new `LogiClipBox` POD (center + 3 half-axis vectors,
///       render-space cm). A coarse REPLACE stand-in carries the OBBs of the finer tiles loaded over it; the
///       renderer selects a masked material and discards the coarse fragments inside them, so coarse + fine tile
///       the ground edge-to-edge with no coplanar overlap (replaces the failed Pixel Depth Offset; keeps
///       SkyAtmosphere's scene depth clean). Empty for almost every tile. Render-lane only — no sim-determinism effect.
/// v13 = pool-pressure telemetry (additive, no layout change): `logi_globe_world_pool_stats`
///       (`(free_slots << 32) | max_resident`) + `logi_globe_world_evict_stats`
///       (`(evicted_total << 32) | pool_exhausted_total`). Added for the 2026-06-11 starvation stall —
///       eviction dead at a planet-scale cull distance left the pool 100% Resident and every admission
///       silently dropped; these expose the signature (free=0, exhausted climbing, evicted flat).
/// v14 = the selection X-ray (additive): `logi_globe_world_hold_stats(world, out, len)` fills up to 6 u32s —
///       [kick, dead, loading, leaf_under_sse, pending_subtree, fetch_skip_grafted], a SNAPSHOT of the last
///       full selection step's hold reasons. Built to attribute the stuck-coarse "scars" (a region pinned
///       coarser than SSE wants with a quiescent pipeline): leaf_under_sse / fetch_skip_grafted sustained at
///       quiescence is the scar; kick/loading are normal convergence and fall to zero at settle.
/// v15 = the planet-true sky anchor (additive): `logi_globe_world_sky_anchor(world, out, len)` fills up to
///       4 f64s — [earth_center_x_cm, earth_center_y_cm, earth_center_z_cm, geocentric_radius_km] under the
///       world's ENU origin. The renderer anchors SkyAtmosphere/VolumetricCloud to the streamed WGS84 globe
///       with these (the 2026-06-11 settled-shot collapse: a 6,360 km sphere pinned at the UE origin put
///       cloud/AP shells ±15 km off the terrain, varying by latitude). Geodesy stays single-homed in Rust.
/// v16 = the UNIFIED telemetry block (breaking, ADR-0004 no-compat): one `LogiGlobeTelemetry` POD (29 × u64,
///       size 232) filled by `logi_globe_world_telemetry(world, out)`, REPLACING the ten per-counter
///       accessors (`_cached_bytes`, `_failed_counts`, `_refine_failures`, `_flash/_disk/_geom_cache_stats`,
///       `_pool_stats`, `_evict_stats`, `_hold_stats`, `_warm_cache_bytes` — all deleted). The block crosses
///       each seam (GeoStats → stream handoff → C ABI) WHOLE, so adding a counter is one field per mirror +
///       an offset lock, not a new entry point + three hand-mirror edits + an ABI bump per counter. First
///       crossings: `step_panics` (caught stream-step panics — the silent-hiccup signal),
///       `fetches_cancelled_total`, `session_refreshes_total`. Render-lane only — no sim-determinism effect.
/// v17 = the TWO-cache model (breaking; owner directive 2026-06-12 + ADR-0019). `LogiGlobeTelemetry` drops
///       its `geom_*` fields (29 → 26 × u64, 232 → 208): the disk cache is ONE cell (`TileDiskCache` — one
///       root, ONE total budget split across its byte + decoded-geometry tiers) whose `disk_*` fields are
///       now the cell's MEMBRANE aggregate (hit = any tier served; miss = the network was needed;
///       bytes = both tiers). Per-tier detail stays inside the cell. Pairs with the same-day flash fix
///       (every dropped resident mesh now deposits into the in-memory cell, not just LRU evictions) and
///       the `geom` budget moving from a hardcoded 4 GiB const to half the one disk budget. Render-lane only.
pub const LOGI_ABI_VERSION: u32 = 17;
