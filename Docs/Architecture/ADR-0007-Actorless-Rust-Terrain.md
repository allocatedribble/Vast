# ADR-0007 — Actorless Rust terrain for Unreal

> **2026-06-05 — renamed to the Geo engine.** The terrain / real-world-earth stack is now `Rust/crates/geo/` (`logi_geo_*` plus the 3D-Tiles / geospatial / semantic crates), cdylib `logi_geo.dll`, C++ `ULogiBakedGeoSubsystem`. The `logi_terrain*` / `LogiTerrain*` / `logi_terrain.dll` names below are historical (pre-rename); see [AGENTS.md](../../AGENTS.md) §4.

- **Status:** Accepted (2026-06-02)
- **Supersedes (in part):** [ADR-0005](ADR-0005-Bevy-ECS-Authority.md) — the clause
  that listed terrain host `AActor`s as an allowed Unreal shell zone.
- **Relates to:** [ADR-0002](ADR-0002-Rust-Terrain-Kernel.md) (Rust terrain kernel),
  [Docs/Research/Actorless-Representation.md](../Research/Actorless-Representation.md)

## Context

ADR-0005 generalized terrain truth into Rust but still permitted a terrain *host
actor* as a shell. At planetary scale (a 262,144-tile logical world under a hard
memory budget, streamed by screen-space error) a per-tile `AActor` — or even one
big terrain actor owning state — is the wrong model: it can't scale, it leaks
truth into Unreal, and it fights World Partition / Nanite streaming. The prior
Unity Logi terrain kernel already proved the correct split (Rust owns streaming,
memory, decode, LOD, telemetry; the engine owns only renderer-facing resources).

## Decision

**Terrain uses no `AActor` per tile and no terrain `AActor` as runtime authority.**

- **Rust owns** terrain truth: planet/world index, tile & page IDs, provider
  adapters (native + Cesium), file/HTTP/cache IO, decode, mesh-voxel storage,
  memory arenas, compressed + decoded caches, tile priority, LOD/SSE selection,
  predictive prefetch, parent fallback, collision-proxy planning, telemetry, and
  source attribution. It runs as a **dedicated Rust kernel/resource adjacent to
  Bevy** — *not* as Bevy ECS entities (packed arrays/arenas/page tables, different
  memory & IO behavior than gameplay agents).
- **Unreal owns** only renderer-facing shells: a `UWorldSubsystem` /
  `UEditorSubsystem`, bounded `UPrimitiveComponent` shells + `FPrimitiveSceneProxy`
  (debug lane) and a Nanite resource bridge (production lane), materials, and GPU
  upload mirrors — the minimum needed to participate in **Nanite, Lumen, Virtual
  Shadow Maps, editor selection, and viewport display**.

**"No terrain Actors" ≠ "no Unreal renderer objects."** Unreal still needs
UObjects/components/proxies/GPU resources to enter the renderer, editor, Lumen,
and shadow systems. These are **presentation shells**, not terrain truth.

**Frame lifecycle (Unreal asks, Rust answers — Rust never calls Unreal):**
`set views → tick → get frame packet → upload/submit resident window → release
frame`. The packet describes only the small resident window Unreal should
upload/render; Unreal never holds the whole planet tree.

**Bevy integration is read-only and coarse.** Bevy sees terrain via a resource
handle (`Arc<LogiTerrainHandle>`), querying coarse deterministic data
(`height_cm`, `route_cost`, `material_class`, `coarse_collision`). High-resolution
render terrain is non-authoritative for gameplay unless promoted into a coarse
deterministic representation (resolve before vehicles/pathing depend on it).

## Consequences

- Planet-scale terrain with a hard memory/in-flight budget, owned end-to-end by
  Rust; Unreal receives only the visible cloth.
- The prior Unity terrain kernel (core/geo/pack/cache/decode/stream/lod/voxel/
  scene/ffi/tools) ports into `Rust/crates/logi_terrain_*` (Milestone C) with its
  streaming stress test intact.
- Two render lanes (Milestones D/E): a debug `FPrimitiveSceneProxy` lane to prove
  actorless streaming, then a Nanite-compatible resource lane for production
  (behind `LOGI_TERRAIN_RUNTIME_NANITE_BUILD`, off until an engine-source spike
  passes). Mesh-voxel pages decode to mesh geometry so they ride the same
  Nanite/Lumen/VSM path.
- CI gates: no terrain `AActor` created; no per-tile `Tick`; no terrain data in
  component properties; memory/in-flight/GPU-mirror budgets never exceeded.

This ADR is the law for all terrain work (Milestones C–G).
