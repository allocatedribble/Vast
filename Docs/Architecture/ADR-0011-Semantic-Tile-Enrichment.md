# ADR-0011 — Data-Driven Semantic Tile Enrichment (offline bake)

> **2026-06-05 — renamed to the Geo engine.** The terrain / real-world-earth stack is now `Rust/crates/geo/` (`logi_geo_*` plus the 3D-Tiles / geospatial / semantic crates), cdylib `logi_geo.dll`, C++ `ULogiBakedGeoSubsystem`. The `logi_terrain*` / `LogiTerrain*` / `logi_terrain.dll` names below are historical (pre-rename); see [AGENTS.md](../../AGENTS.md) §4.

- **Status:** Accepted (2026-06-05)
- **Implements:** [ADR-0007](ADR-0007-Actorless-Rust-Terrain.md) — the data-driven *content* layer over the
  actorless terrain/render lanes; and the foliage scope once reserved for ADR-0010 (see **Numbering**).
- **Builds on:** [ADR-0009](ADR-0009-Native-Rust-3DTiles-Stack.md) — the native Rust 3D Tiles stack
  (streaming / decode / render path) that enrichment feeds.
- **Relates to:** [ADR-0005](ADR-0005-Bevy-ECS-Authority.md) (Bevy ECS authority),
  [ADR-0006](ADR-0006-Editor-Inspectability.md) (the Tile Editor's command-not-poke, editor-only,
  zero-shipping-cost discipline), [ADR-0008](ADR-0008-Terrain-Nanite-Lane.md) (runtime non-Nanite
  textured lane).
- **Plan:** [Docs/Plans/Hero-Location-Semantic-Enrichment.md](../Plans/Hero-Location-Semantic-Enrichment.md).

## Context

ADR-0009 gives LogiUE a native Rust path to stream real-world 3D Tiles (Cesium ion; Google Photorealistic
3D Tiles). That content is **inadequate for first-person fidelity** and **carries no object semantics**:

- Google/Cesium photoreal tiles are **fused photogrammetry meshes** — cars, trees, water, and buildings are
  baked into one continuous textured surface (parked cars melt into the ground; tree canopies float) at a
  resolution that reads acceptably from the air and **falls apart at street level**. There is **no
  per-object metadata** — the 3D Tiles metadata extensions exist, but the photoreal producers do not
  populate them.
- We want **our own art** standing on the ground: our trees/foliage, our animated water, our buildings —
  not a low-resolution photogrammetric crust.
- The **procedural-only** foliage scatter once earmarked for ADR-0010 (Milestone-F §2) is insufficient: it
  places plausible-random vegetation, not the **real** trees, water, and buildings of a real place.

What we need: turn real-world data — the photoreal mesh as a *prior*, plus LiDAR, aerial imagery,
OSM/Overture footprints, and landcover — into **our own enriched tile data** (ground surface, instanced
trees/foliage, water surfaces, param-procedural buildings), rendered with our assets, at first-person
quality, **efficiently and deterministically**, without breaking the constitution.

The hard questions this ADR settles: *where* the (necessarily ML-heavy) extraction runs, *what* owns the
result, and *how* it stays inside the actorless / narrow-FFI / deterministic / Bevy-authority laws.

## Decision

**Semantic enrichment is an offline content-authoring bake. Its output is render/content data consumed by
the existing ADR-0009 streaming path — never runtime gameplay truth, and never an in-process ML dependency.**

1. **Offline bake, lean runtime.** Extraction (ground / water / trees / buildings) runs in an **offline
   Python/ML toolchain** (`Tools/logi_bake/`) that emits a **versioned enriched tile store**. The runtime
   `logi_terrain.dll` only **consumes** baked tiles; it links **no ML, no Python, no torch**. The external
   ML toolchain is an **accepted build-time / content-authoring dependency** — the same class as offline
   asset authoring or the vendored Draco decode shim (ADR-0009 §5): it produces plain data, runs off-engine,
   and **no Unreal types cross and Rust never calls Unreal**. Honors AGENTS.md §3.9–10 (narrow FFI, lean
   runtime, no compatibility code) and §0 (the runtime stays scalable to planet/agent scale).

2. **Output rides existing rails — actorless, batched.** Enriched tiles emit the kernel `LogiFramePacket`:
   trees/foliage on the existing `LogiInstanceBatchDraw` rail (**batched per tile — no per-instance FFI
   chatter**, AGENTS.md §3.9); ground/water/buildings as textured `LogiTerrainPatchDraw` sections (water = a
   flat section bound to the animated-water material). **No `AActor` per tree/building/tile**, no per-instance
   `Tick` (ADR-0007). The only ABI change is the already-planned M0 texture channel (ADR-0009 §9.3).

3. **Real data is a prior, not shipped geometry.** Buildings are **param-conditioned procedural** (footprint
   + height + roof + facade parameters *extracted from* the real texture) finished with **AI material
   synthesis**. We do **not** ship the photogrammetry mesh, and we do **not** do LOD3 facade-geometry
   detailing in this phase.

4. **The Tile Editor obeys ADR-0006.** In-editor edits (move/add/delete instances now; terrain/building
   geometry later) are **validated commands** (`logi_semantic_edit_*`) into a persisted **override layer** —
   never memory pokes, never per-instance FFI from Slate paint, **editor-only** (behind a cargo feature + a
   separate editor header), **zero shipping cost**. The bake is **idempotent and merges overrides** keyed by
   stable id, so re-running the ML never clobbers artist work.

5. **Deterministic and reproducible.** The bake is **seeded, version-pinned, and content-addressed** →
   **byte-reproducible** given (sources, seed, config, tool version). The runtime `BakedTileSource` is a
   deterministic `TileContentSource` (kept behind the trait alongside `SyntheticProvider`). Determinism is
   preserved end-to-end (AGENTS.md §3.8).

6. **Gameplay truth stays Bevy / coarse (ADR-0007).** Enriched render data is **non-authoritative**.
   Anything gameplay depends on — building collision/footprints, water as route cost, ground height for
   pathing — must be **promoted into the coarse deterministic representation** owned in Rust (the ADR-0007
   "resolve before vehicles/pathing depend on it" rule). The render layer may be richer than the sim; the
   sim never reads raw render geometry across the FFI.

## Consequences

- **First-person fidelity from open real-world data, as our own art** — sidesteps both the resolution and
  the licensing problems of shipping photoreal tiles. The photoreal mesh is reference only; geometry derives
  from open LiDAR/OSM/Overture; a **provenance ledger** in the bake manifest keeps per-input licensing
  auditable even though productionization is deferred.
- **The constitution holds:** actorless (ADR-0007); narrow, batched, one-directional FFI; Rust owns
  selection + placement; Unreal renders shells; determinism preserved; the runtime stays lean (ML is
  offline). The Bevy `World` remains the sole gameplay authority.
- **New surfaces to own + test:** the `Tools/logi_bake/` toolchain, the `logi_semantic` crate, a **versioned
  tile-store format**, and a **versioned override schema** — each under the standards + gates the plan
  enumerates (§8–§10): typed/linted/pinned Python; `#![forbid(unsafe_code)]` Rust outside the single FFI
  seam; golden-bake reproducibility; FFI contract tests; CI gates mirroring ADR-0006/0007.
- **Costs:** the bake is offline and slow (acceptable for one hero AOI; planet-scale bake is open); artist
  cleanup must be budgeted — ML accuracy is ~75–90 %, not perfect, and the Tile Editor is the safety valve.
- **Foliage is now data-driven:** the procedural-only scatter reserved for ADR-0010 is **subsumed** — real
  tree positions where detected, seeded procedural fill elsewhere.

## Open questions

1. **Vertical-datum accuracy is a correctness invariant.** Source LiDAR is NAVD88 (geoid); the globe is
   WGS84 ellipsoid. The bake converts via a geoid model and validates against a benchmark; define + test the
   acceptance threshold (target ≤ ~5 cm vertical residual). A wrong datum floats/sinks the whole AOI.
2. **Scaling the bake beyond one AOI** — a single offline run today; region/planet scale needs a
   tiled/distributed bake + a coverage strategy. Deferred.
3. **Which gameplay features need coarse promotion** (buildings as obstacles? water as route cost? ground
   height for couriers?) — coupled to `logi_sim`, separable from render bring-up.
4. **Licensing productionization** — deferred; the provenance ledger lets a shippable open-data-only subset
   be carved later (Google as on-screen reference) without re-architecting.

## Numbering

ADR-0010 was reserved (Milestone-F §2 — "Foliage — Milestone G / ADR-0010") for a **procedural** foliage
decision that was never authored. This ADR **supersedes that scope before it was written**: foliage becomes
one feature class of data-driven enrichment. ADR-0010 is intentionally left unused to preserve existing
references; no separate foliage ADR will be written.

This ADR is the law for the semantic-enrichment work; ADR-0007/0008/0009 otherwise stand.
