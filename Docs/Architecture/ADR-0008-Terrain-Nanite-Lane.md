# ADR-0008 — Terrain render lane: editor Nanite via the engine build path, no runtime Nanite encode

- **Status:** Accepted (2026-06-03)
- **Refines:** [ADR-0007](ADR-0007-Actorless-Rust-Terrain.md) — replaces its Milestone-E plan
  ("a Nanite-compatible resource lane behind `LOGI_TERRAIN_RUNTIME_NANITE_BUILD`, off until an
  engine-source spike passes") with the spike's actual finding.
- **Relates to:** [ADR-0002](ADR-0002-Rust-Terrain-Kernel.md), [ADR-0007](ADR-0007-Actorless-Rust-Terrain.md)

## Context

Milestone E specced a **runtime** Nanite terrain lane: encode Nanite mesh resources from the Rust
kernel's per-tile frame packets each frame. The engine-source spike (UE 5.8, the **installed
launcher build** at `C:\Program Files\Epic Games\UE_5.8`) found that runtime Nanite encode is not
viable in stock 5.8:

- The Nanite **encoder** (`NaniteBuilder`) is a hard editor-only `Developer` module
  (`NaniteBuilder.Build.cs`: *"NaniteBuilder module is an editor module"*); `Engine` reaches it
  only inside `if (Target.bBuildEditor)`. `FStaticMeshRenderData::Cache` (which fills
  `NaniteResourcesPtr`) is `WITH_EDITORONLY_DATA`. `UStaticMesh::BuildFromMeshDescriptions`' runtime
  fast-path builds a **non-Nanite** mesh only.
- The runtime can **register and draw** a *pre-encoded* `Nanite::FResources`
  (`FResources::InitResources` / `NaniteStreamingManager::Add` are not editor-gated), but there is
  **no runtime API to encode one**.
- This is a launcher (installed) engine — `Build/InstalledBuild.txt` present,
  `Build/SourceDistribution.txt` absent — so engine modules **cannot be recompiled**; the encoder
  can't be dragged into a shipping runtime target without a from-source engine.
- Even with a source fork, the Nanite build is an **offline, async, DDC-cached** operation
  (ms–tens-of-ms per mesh) — unfit for per-frame streaming churn.

**But** the terrain renderer runs in the **editor and PIE-in-editor**, where `WITH_EDITOR=1` and
the Nanite builder module *is* loaded. In that context `UStaticMesh::BuildFromMeshDescriptions`
with `bFastBuild=false` falls through to `UStaticMesh::Build(true)` — the engine's own pipeline,
which produces a **real Nanite mesh** when `NaniteSettings.bEnabled`.

## Decision

**The production terrain render lane (Milestone E) is an editor-only Nanite lane built through the
engine's own `UStaticMesh` path. There is no runtime Nanite encode.**

- Per resident tile, build a transient, Nanite-enabled `UStaticMesh` from the kernel's interleaved
  vertex/index buffer views (`FMeshDescription` → `BuildFromMeshDescriptions`, `bFastBuild=false`,
  `NaniteSettings.bEnabled`), and draw it via a **pooled `UStaticMeshComponent`** under the single
  terrain host actor. Still no per-tile `AActor`; the host + pooled components are presentation
  shells (ADR-0007). Native Nanite LOD / GPU-driven culling / VSM — retires the
  `SetCastShadow(false)` hack the debug lane needs.
- The lane is **`WITH_EDITOR`-only** (the Nanite encoder is an editor module; on an installed
  engine it cannot be made to ship). Compiled behind `LOGI_GEO_NANITE_LANE`
  [correction 2026-06-09: renamed from `LOGI_TERRAIN_NANITE_LANE` in the Terrain→Geo rename; see Build.cs:109]
  (= `Target.bBuildEditor` && terrain DLL present); toggled at runtime by
  `ULogiBevyDeveloperSettings::bUseNaniteTerrainLane` (default **off**), coexisting with the
  proc-mesh debug lane (Milestone D) for live A/B.
- The build is heavy + synchronous, so it is **budgeted**: `NaniteTileBuildsPerStep` tiles built
  per kernel step, the rest deferred to later steps; `bUseHashAsGuid` for DDC reuse;
  `bSupportRayTracing` off per tile.

**For a packaged ship build**, terrain Nanite is out of scope in stock 5.8. Two future options,
each its own milestone, neither blocking the editor lane: (a) an **offline bake** — cook tiles to
Nanite `UStaticMesh` assets keyed by tile id; (b) a **fast static-relevance `FPrimitiveSceneProxy`**
(`FStaticMeshSceneProxy`-style: `bStaticRelevance` + `DrawStaticElements`) that fixes VSM without
Nanite and ships everywhere.

## Consequences

- Real Nanite terrain in editor/PIE now, via the engine's own build path — no engine fork, no
  custom Nanite encoder, no `FPrimitiveSceneProxy` re-implementation.
- Ship builds keep the proc-mesh debug lane (or a future ship lane); the Nanite lane compiles out
  (`LOGI_GEO_NANITE_LANE=0` [correction 2026-06-09: renamed from `LOGI_TERRAIN_NANITE_LANE`]), zero cost.
- Per-tile synchronous Nanite builds are heavy; budgeting trades fill-in latency (fast camera moves
  show fewer tiles until builds catch up) for editor responsiveness. If the measured per-tile cost
  is prohibitive, fall back to the fast-proxy option above.
- The `LOGI_TERRAIN_RUNTIME_NANITE_BUILD` flag/plan from ADR-0007 is **retired**: runtime Nanite
  encode is not viable in stock 5.8. [correction 2026-06-09: this was the original *runtime-encode* plan
  flag; the live *editor-only* lane this ADR ships is the separate `LOGI_GEO_NANITE_LANE` (Build.cs:109).]

This refines the Milestone-E clause of ADR-0007; ADR-0007 otherwise stands.
