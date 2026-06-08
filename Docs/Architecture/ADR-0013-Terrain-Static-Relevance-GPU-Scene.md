# ADR-0013 — Proc-mesh terrain lane: static relevance + GPU-Scene cached draw commands

- **Status:** Accepted (2026-06-08) — **implemented + verified** (renders; per-frame render-thread cost
  materially reduced, confirmed in editor/PIE).
- **Refines:** [ADR-0008](ADR-0008-Terrain-Nanite-Lane.md) — implements its future option "(b) a fast
  static-relevance `FPrimitiveSceneProxy` (`FStaticMeshSceneProxy`-style: `bStaticRelevance` +
  `DrawStaticElements`) that fixes VSM without Nanite and ships everywhere."
- **Relates to:** [ADR-0007](ADR-0007-Actorless-Rust-Terrain.md), [ADR-0012](ADR-0012-Poll-Based-FFI-Decoupled-Sim-Thread.md).

## Context

The proc-mesh terrain lane renders each resident tile through a custom per-tile `FLogiTileSceneProxy`
(`FStaticMeshVertexBuffers` + `FLocalVertexFactory` + `FDynamicMeshIndexBuffer32`; geometry de-interleaved +
uploaded on the render thread, audit T1). It used **dynamic relevance** — `GetDynamicMeshElements` rebuilt an
`FMeshBatch` **every frame for every visible tile**, a per-frame render-thread CPU cost that grows with the tile
count. ADR-0008 named a static-relevance proxy as the way to remove it (GPU-Scene cached draw commands + GPU
culling).

Converting it took **four attempts that all rendered NOTHING** — tiles built (`built=N`), invisible, with **no
`ensure`/error**. The misses, none of which were the cause: `BatchElement.PrimitiveUniformBuffer =
GetUniformBuffer()` (null for a GPU-Scene primitive); switching to `VertexFactoryUserData`; adding
`EnableGPUSceneSupportFlags()`.

**Root cause (engine-traced):** `DrawStaticElements` never set `Mesh.LODIndex`, so it defaulted to
`INDEX_NONE (-1)`. A cached static batch is gated **per-view by LOD selection** (`FLODMask::ContainsLOD`,
`SceneVisibility.cpp:1667`); `LODIndex = -1` → `bInvalidLODIndex` (`StaticMeshBatch.cpp:49`) → `GetLODIndex()`
returns -1 → `ContainsLOD(-1)` is **always false** (LODs are stored as unsigned 7-bit fields) → the command is
**cached but never marked visible** → nothing draws. It's **silent** because LOD non-selection is normal control
flow, not an error. `FStaticMeshSceneProxy` sets `LODIndex = 0` (`StaticMeshSceneProxy.cpp:831`);
`FProceduralMeshSceneProxy` (the resource shape we mirrored) is dynamic-only, so there was no `LODIndex` line to
copy. GPU-Scene was correct the whole time.

## Decision

1. **The proc-mesh terrain lane renders via STATIC relevance + GPU-Scene by default.** `DrawStaticElements`
   caches one mesh draw command per tile (immutable geometry; a content change recreates the proxy);
   `GetDynamicMeshElements` is retained only for the wireframe view. Required setup, all in
   `FLogiTileSceneProxy`:
   - **`Mesh.LODIndex = 0;`** in `DrawStaticElements` — THE fix (single-LOD tile; matches `FStaticMeshSceneProxy`).
   - `EnableGPUSceneSupportFlags()` + `bStaticElementsAlwaysUseProxyPrimitiveUniformBuffer = true` in the ctor —
     GPU-Scene-proxy hygiene (consistent GPU-Scene participation; transform/instance updates skip a full
     static-draw-list rebuild), matching `FStaticMeshSceneProxy`.
   - `BatchElement.VertexFactoryUserData = VertexFactory.GetUniformBuffer()`, **no** primitive uniform buffer —
     GPU-Scene supplies the per-primitive transform via the VF's primitive-id stream (added by
     `InitFromDynamicVertex` → `InitRHI`).
   - `GetViewRelevance`: `bStaticRelevance = !bWireframe`, `bDynamicRelevance = bWireframe`.

2. This captures GPU-Scene's **cached-command + GPU-cull** win: no per-frame render-thread `FMeshBatch` rebuild.
   **Verified rendering + materially improved performance (2026-06-08).** Tiles are unique geometry, so the win
   is cached commands + GPU culling, **not** instance merging.

3. **We do NOT build a custom GPU-Scene.** A bespoke GPU-driven path (vertex/index arena + multi-draw-indirect +
   custom GPU cull) would bypass UE's renderer — losing Lumen GI, VSM, the material system, and RT — for
   marginal gain over reusing GPU-Scene; the unique-geometry-single-draw case is Nanite (cluster GPU-driven),
   editor-encode-only at runtime (ADR-0008). Reuse the engine's GPU-Scene; do not reinvent it.

## Consequences

- The dynamic-path per-visible-tile `FMeshBatch` rebuild is **eliminated** (cached static commands + GPU
  culling) — the planetary-scale render-thread win, shipping everywhere (unlike the editor-only Nanite lane,
  ADR-0008).
- The RT BLAS opt-out (`r.LogiGeo.TerrainRayTracing`), the `rebase_gltf` micro-opts, and the FFI v8 O(Δ)
  delta-reconcile (ADR-0012) are unaffected.
- **Lesson recorded:** a custom `FPrimitiveSceneProxy` converting from dynamic to static **must** set
  `Mesh.LODIndex` (default `-1` silently LOD-culls every cached command — invisible, no error). This is not in
  any tutorial; only a per-view LOD-gate trace surfaces it. The prior batch/flag/GPU-Scene fixes all missed it
  because they were the wrong layer.
