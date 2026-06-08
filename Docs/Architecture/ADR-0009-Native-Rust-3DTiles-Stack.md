# ADR-0009 — Native Rust OGC 3D Tiles & real-world terrain stack

> **2026-06-05 — renamed to the Geo engine.** The terrain / real-world-earth stack is now `Rust/crates/geo/` (`logi_geo_*` plus the 3D-Tiles / geospatial / semantic crates), cdylib `logi_geo.dll`, C++ `ULogiBakedGeoSubsystem`. The `logi_terrain*` / `LogiTerrain*` / `logi_terrain.dll` names below are historical (pre-rename); see [AGENTS.md](../../AGENTS.md) §4.

- **Status:** Accepted (2026-06-04)
- **Implements:** [ADR-0007](ADR-0007-Actorless-Rust-Terrain.md) — the "provider adapters (native +
  **Cesium**)" it names; the LOD/SSE/streaming it assigns to Rust now also covers OGC 3D Tiles tilesets.
- **Relates to:** [ADR-0005](ADR-0005-Bevy-ECS-Authority.md) (Bevy ECS authority),
  [ADR-0008](ADR-0008-Terrain-Nanite-Lane.md) (editor-only Nanite lane),
  [ADR-0002](ADR-0002-Rust-Terrain-Kernel.md) (Rust terrain kernel).
- **Plan:** [Docs/Plans/Milestone-F-3DTiles-Rust-Stack.md](../Plans/Milestone-F-3DTiles-Rust-Stack.md).

## Context

Vast needs **real-world terrain + buildings** (and optionally photorealistic city meshes) — i.e.
OGC **3D Tiles** / Cesium content. Two off-the-shelf paths were researched and rejected as the
*authority*:

- **Cesium for Unreal (the plugin)** is actor-based end to end — one `ACesium3DTileset` actor, a
  `USceneComponent`/`UStaticMeshComponent` tree per tile/primitive, and a separate `ACesiumGeoreference`
  actor that owns the world origin. It is a parallel renderer with its own origin authority →
  **violates ADR-0005/0007** (no per-tile actors; Rust owns placement).
- **Linking the C++ `cesium-native` library** as a render-only tile source ("approach B1") is viable
  and keeps Unreal actorless, but it puts tile *selection* + *decode* in C++ and adds a heavy C++ build
  dependency (vcpkg, ~30 libs).

The user chose a **native Rust reimplementation** "for better performance and efficiency," accepting a
large rewrite and no backward-compatibility.

**The finding that makes this tractable:** `logi_terrain_*` already contains Rust ports of
cesium-native's *hardest runtime pieces* — `logi_terrain_lod::screen_space_error` is **byte-for-byte**
Cesium's SSE; `logi_terrain_stream::culling` already implements Cesium's **exact scaled-space
horizon-occlusion test** plus inward-plane frustum culling; `logi_terrain_cache` is the byte-budgeted
LRU; `scheduler.rs` is the priority/queue model; `logi_terrain_scene` is the zero-copy per-tile
frame-packet C ABI; and `REBASE_SNAP` is the f64→f32 floating-origin rebase. So a Rust-native stack is
**generalisation + decode, not a from-scratch port.**

UE 5.8's experimental **Mesh Terrain Mode** was evaluated as a render backend and rejected: source
inspection (`Engine/Plugins/Experimental/{MeshTerrainMode,MeshPartition}` @ dev-5.8) shows it is an
**editor-only authoring mode** — author → editor-compile → cook-time bake → stream pre-cooked
`ACompiledSection` World-Partition actors. There is **no runtime ingest API**; it is actor-heavy and
hits the same stock-5.8 wall as ADR-0008 (no runtime Nanite encode). It cannot serve runtime-streamed
Rust tiles.

## Decision

**Build a native Rust 3D Tiles + real-world-terrain stack that reuses the existing kernel core and
feeds the existing actorless render lanes over the unchanged frame-packet ABI (geometry) plus a new
per-tile texture channel.**

- **Crates** (full DAG in the plan): `logi_geospatial`, `logi_qmesh`,
  `logi_3dtiles_{tileset,implicit,select,io,ion,scene}`, `logi_gltf`, `logi_collada`. **Reuse unchanged:**
  `logi_terrain_{lod, cache}`, `logi_terrain_stream::{culling, scheduler}`, `logi_terrain_core` (with
  `TileId` → a general `TileKey`). **Retire** `logi_terrain_{procgen, decode, pack::height}` **behind a
  `synthetic` feature** (kept for deterministic tests, offline dev, and procedural non-georeferenced
  worlds — not deleted).
- **Tile selection lives in Rust:** port cesium-native's pointer-tree `visitTile` traversal
  (kick / skip-LOD / `ancestorMeetsSse` / REPLACE+ADD refine), generalising the regular-quadtree
  `select_hierarchical`. SSE / culling / cache / scheduler are reused; the existing anti-thrash
  hysteresis is kept.
- **Content decode in Rust:** glTF (the `gltf` crate) + KTX2/Basis + meshopt + quantized-mesh-1.0;
  COLLADA 1.4.1 (`dae-parser`) for KML/DAE assets.
- **Draco is the one accepted FFI exception:** a vendored, `cc`-built, **decode-only** C++ shim inside
  `logi_gltf` (plain-data in/out, runs on Rust worker threads, **no Unreal types cross**). Gated behind a
  `draco` feature. Revisit when a production pure-Rust decoder exists. (`basis-universal`/`meshopt` are
  standard C++-backed Rust crates, not special cases.)
- **IO is synchronous — no tokio.** A `TileContentSource` trait (generalises the existing `Provider`)
  drives selection + integration + LRU on the **single owner thread**; a small `std::thread`/`crossbeam`
  worker pool does fetch → gunzip → decode → interleave and pushes owned blobs back. HTTP = `ureq`;
  `flate2` gzip; content-addressed disk cache. `SyntheticProvider` stays the deterministic test source.
- **Data source = Cesium ion** via a **minimal** `logi_3dtiles_ion` asset-endpoint resolver
  (POST `/v1/assets/{id}/endpoint` → `{url, accessToken, attributions}`); full OAuth/iTwin is deferred.
- **Render:** the C++ *geometry* path is unchanged. Add a per-tile **texture** path — an FFI texture
  channel (ABI **3→4**) and a C++ `UTexture2D` + `UMaterialInstanceDynamic` lane with `TStrongObjectPtr`
  GC rooting and a texture byte-budget LRU. Runtime textured meshes use the direct `SetRenderData`
  (**non-Nanite**) lane; single-material terrain keeps the proc-mesh + editor-Nanite lanes.
- **Mesh Terrain Mode is not a render backend.** Keep the proc-mesh + editor-Nanite lanes; borrow only
  its **far-field/impostor LOD** and **channel-splat material** ideas.

## Consequences

- **The constitution holds:** Rust owns terrain truth (selection + decode + placement); Unreal renders;
  actorless; narrow C ABI. No C++ cesium-native dependency, no Cesium actors, no competing georeference —
  Rust keeps the floating origin via `REBASE_SNAP`.
- **The rewrite is bounded** (~the kernel's hard half is reused) — generalisation + decode, not
  from-scratch. The cdylib name (`logi_terrain.dll`) and the C++ `LogiBevyRuntime.Build.cs` linkage are
  unchanged.
- **The traversal port is the critical-path risk** (the kick / `ancestorMeetsSse` / last-frame-render
  interactions cause pop-in/holes) → mitigated by faithful `TileSelectionState`/`TraversalDetails`
  porting + **golden camera-path tests**.
- **One FFI exception (Draco) is accepted and documented.**
- **New render capability + an ABI bump (3→4):** per-tile textures. Textured meshes are non-Nanite at
  runtime — the ADR-0008 wall (no runtime Nanite encode in stock 5.8) stands.
- **Cost:** Cesium ion is a flat SaaS subscription; Google photoreal (if later added) is the only
  per-session-metered, **non-cacheable** cost (its licence forbids caching, and EU/EEA billing is
  blocked post-Jul-2025).
- **Determinism** is preserved by keeping `SyntheticProvider` behind the `TileContentSource` trait; live
  network streaming is non-deterministic and is render-only (gameplay truth stays on the deterministic
  coarse representation, per ADR-0007).

## Open questions

1. **Globe / orbital→street zoom — RESOLVED (2026-06-04).** The world is a **true-globe WGS84 ECEF f64**
   model; there is no flat plane. **Precision** = camera-tracking RTC (the existing `REBASE_SNAP`, fed
   ECEF; UE5 LWC is a complementary safety net, not a substitute). **Geometry** = the ellipsoidal
   3D-Tiles tiling (the tileset BVH) + the already-ellipsoid-capable horizon culler, all reasoning in
   ECEF — the flat-square `World` retires to the `synthetic` feature. **Depth** = UE 5.8 native reversed-Z
   float buffer + a per-frame altitude-adaptive near plane, set actorlessly via an `FSceneViewExtensionBase`
   (`SetupViewProjectionMatrix`) — **NOT** logarithmic depth (it needs pixel-shader depth writes →
   disables Hi-Z and **breaks the Nanite lane**, and is impossible in stock 5.8) and **NOT** multi-frustum;
   with reversed-Z float the far plane is precision-irrelevant and `near = clamp(k·altitude, 0.1 m, cap)`
   is the only knob. **Camera** = an altitude-adaptive globe pawn (intent shell); Rust ships the ENU up +
   rebase origin + recommended near/far in the frame packet. **Gotchas:** Sky Atmosphere's planet center
   must track the rebased origin; VSM/shadow range scales with altitude (empirical). Full design in the
   plan §9.4. **Gameplay authority** generalizes to ECEF f64 for global agents — coupled but separable.
2. **Texture memory at scale.** Per-tile BC7 textures need a byte-budget LRU mirroring `evict_meshes`;
   atlas/bindless is a later optimisation.
3. **Photoreal licence.** If Google Photorealistic 3D Tiles are added, the no-cache + EU/EEA-billing
   restrictions apply and must be surfaced to product.

This **implements** ADR-0007's named Cesium provider; ADR-0007 and ADR-0008 otherwise stand.
