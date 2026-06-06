# Plan — Hero-Location Semantic Tile Enrichment (New Castle County, DE)

> **2026-06-05 — renamed to the Geo engine.** The terrain / real-world-earth stack is now `Rust/crates/geo/` (`logi_geo_*` plus the 3D-Tiles / geospatial / semantic crates), cdylib `logi_geo.dll`, C++ `ULogiBakedGeoSubsystem`. The `logi_terrain*` / `LogiTerrain*` / `logi_terrain.dll` names below are historical (pre-rename); see [AGENTS.md](../../AGENTS.md) §4.

**Status:** Plan (2026-06-05). **Ratified by** [ADR-0011](../Architecture/ADR-0011-Semantic-Tile-Enrichment.md).
**Builds on** Milestone F (M0 texture rail, M3 traversal, M4 glTF decode). **Supersedes** the procedural-only
M-G foliage clause of the Milestone-F plan with a *data-driven* layer (real positions where detected; seeded
procedural fill elsewhere).

**Target:** render — first-person, efficiently, deterministically — a **1 mi-radius** area of interest (AOI)
centred on **39.643178, -75.617567** (New Castle County, DE; Newark/Christiana/Bear area), reconstructed
from real-world data as **our own assets**: a ground terrain mesh, instanced trees + foliage, an animated
water asset, and param-conditioned procedural buildings. The real photoreal mesh/imagery is a
**prior/inspiration**, never shipped geometry.

**Quality bar (non-negotiable).** This plan describes **production-grade** code even where v1 deliverables
are deliberately basic. "Basic" scopes *features*, never *standards*: every line obeys the engineering
standards (§8), passes the test/CI gates (§9), meets the performance budgets (§10), and honors the
constitution (actorless, narrow batched FFI, deterministic, Bevy-authoritative). A milestone is **not done**
until its Definition of Done (§12) is green.

---

## 1. Shape of it & the load-bearing finding

```
SOURCES (fused in one geodetic/ECEF frame — logi_geospatial)
  USGS 3DEP QL2 LiDAR (LAZ)   ─┐
  NAIP aerial RGB+NIR         ─┤
  OSM (footprints/water/…)    ─┼─►  OFFLINE PYTHON ML BAKE  (Tools/logi_bake/)
  Overture/MS building heights─┤       ground · water · trees/foliage · buildings
  ESA WorldCover 10m          ─┤       (seeded · version-pinned · content-addressed · merges overrides)
  Google photoreal (prior)    ─┘
                                            │  reproducible → versioned tile store + QA report
                                            ▼
                          ENRICHED TILE STORE  (Saved/LogiTiles/<aoi>/, content-addressed)
                                            │
                                            ▼
        Rust ingest  —  logi_semantic (BakedTileSource: TileContentSource, #![forbid(unsafe_code)])
                          → logi_3dtiles_scene frame-packet assembly (RTC rebase)
                                            │  LogiFramePacket (existing rails, batched per tile)
                                            ▼
        UE render lanes (LogiBevyRuntime)            +   TILE EDITOR (ADR-0006: commands, editor-only)
          terrain: proc-mesh / editor-Nanite              move/add/delete instances → override layer
          trees+foliage: HISM (LogiInstanceBatchDraw)     validated FFI command, never a memory poke
          water: flat mesh + animated material            re-bake merges by stable id (never clobbers)
          buildings: textured non-Nanite mesh
```

**The load-bearing finding (FFI audit).** The kernel frame packet **already carries the rails this needs** —
[`LogiInstanceBatchDraw`](../../Rust/crates/logi_terrain_ffi/include/logi_terrain_ffi.h) (`mesh_handle` +
`instance_data` blob + `instance_stride` + `material_id`), the `material_page_uploads` rail, the
`attributions` rail — all present in `LogiFramePacket`, currently null on the M2 globe path. So:

- **Trees/foliage** ride the existing `instance_batches` rail → **no struct change**, just a versioned
  per-instance layout + the C++ HISM consumer. Data crosses the FFI **batched per tile**, never per instance.
- **Buildings/water** are textured/flat meshes → `LogiTerrainPatchDraw` + the **M0 texture channel** (the one
  already-planned ABI 3→4 bump, ADR-0009 §9.3). Water = a flat patch with `material_id` = animated water.
- Enriched local tiles emit the **rich `LogiFramePacket`** (the Milestone-F §3 end-state for
  `logi_3dtiles_scene`), distinct from M2's interim lean `LogiGlobeFrame`. `BakedTileSource` is just another
  `TileContentSource`; selection / cache / scheduler reuse unchanged.

**Constitution check (AGENTS.md / ADR-0005/0007).** The bake is **offline content authoring**, not gameplay
truth; enriched tiles are render/content data; Rust owns selection + placement; UE renders shells; the heavy
ML toolchain lives **outside** the `logi_terrain.dll` cdylib. Render data is **non-authoritative** —
anything gameplay depends on is promoted to the coarse deterministic representation (§5, ADR-0007).

---

## 2. The AOI & the data (concrete shopping list)

- **AOI math:** radius 1 mi = **1609.34 m**; area ≈ **8.13 km²**; bbox ≈ 3.22 km square. Centre
  `lat 39.643178, lon -75.617567`. Planar CRS = **UTM 18N / EPSG:26918 (NAD83)**; vertical **NAVD88
  (geoid18)** → **convert to WGS84 ellipsoid** before ECEF (the #1 correctness invariant — §9, §14).
- **Step 0 — confirm land cover** for the exact point (suburban mix expected: residential + commercial +
  roads + scattered woodland + ponds/wetlands). Drives which extractors carry weight; recorded in the manifest.

| Source | Gives | Res / format | Access | License (ledger) |
|---|---|---|---|---|
| **USGS 3DEP QL2 LiDAR** (Delaware Valley High Density) | heights, ground/non-ground, canopy structure, roof planes | ~0.7 m spacing, 2 pts/m², LAZ | [LidarExplorer](https://apps.nationalmap.gov/lidar-explorer/) + [AWS Open Data](https://registry.opendata.aws/usgs-lidar/) | public domain |
| **NAIP** aerial | tree crowns (DeepForest), NDVI/NDWI, facade colour | ~0.6 m RGB+NIR, GeoTIFF | USDA / cloud mirrors | public domain |
| **OpenStreetMap** | building footprints, water, roads, landuse | vector | osmnx / pyrosm | ODbL (attribution) |
| **Overture / MS / Google Open Buildings** | footprints **+ heights** at scale | 4 m heights | overturemaps CLI / DuckDB | ODbL / CDLA / CC-BY |
| **ESA WorldCover** | 11-class landcover (tree/shrub/grass/water/built-up) | 10 m | tile download | CC-BY |
| **Google photoreal** (your M2 path) | visual prior: palette, massing, facade rhythm | — | ion/Maps | **deferred — reference only** |

LiDAR is the geometric backbone; rasters + vectors disambiguate class; the photoreal mesh is colour/style
reference only. **Every input is recorded in the provenance ledger** (source, version/acquisition date,
checksum, license) in the bake manifest (§11) so a shippable open-data-only subset can be carved later.

---

## 3. Offline bake pipeline — `Tools/logi_bake/` (Python ML)

A versioned CLI package; **deterministic** (seeded; pinned model weights; no network except the explicit
acquire stage); **idempotent**; **merges artist overrides** (§7). Heavy ML stays here, off the cdylib.

**Env / libraries** (pinned + hash-locked — §8): geo I/O `rasterio`, `geopandas`, `shapely`, `pyproj`,
`osmnx`/`pyrosm`, `overturemaps`/`duckdb`; LiDAR **PDAL** (+ bindings), `laspy` (ground `filters.smrf`/CSF);
vegetation **DeepForest**, **PyCrown**, `scikit-image`; buildings **roofer**/**geoflow** or **City3D**
(external bins) else `shapely`+`trimesh`; mesh `trimesh`/`Open3D`/`pymeshlab`; AI materials (HE-6)
`diffusers`+`torch` (**StableMaterials**, pinned weights).

**A — Acquire & co-register.** Clip every source to the AOI bbox; reproject to EPSG:26918; **resolve the
vertical datum to ellipsoid** (geoid18). Cache raw downloads content-addressed under `cache/`. Validate each
input (CRS present, bounds intersect AOI, non-empty) → fail loud with provenance on bad/missing data.

**B — Ground / terrain.** PDAL ground classification → **DTM** (ground) + **DSM** (first return) rasters;
**nDSM = DSM − DTM** (height-above-ground — the key derived layer). DTM → tiled **heightfield meshes** for
the existing terrain lane, decimated to SSE per level (§4), with skirts.

**C — Water.** `OSM water ∪ WorldCover water ∪ NDWI(NAIP)` → mask → polygonize; height = min DTM around each
boundary; flatten the terrain at the waterline. Output: water polygons + heights.

**D — Trees / foliage.** CHM = nDSM ∩ vegetation(WorldCover tree/shrub ∪ NDVI). **DeepForest** (NAIP RGB)
⊕ **PyCrown** (CHM local-maxima/watershed), **fused** with a documented dedup rule → **per-tree**: stable id,
position, height, crown radius, coarse type (deciduous/conifer), **confidence**. Understory: grass/shrub/NDVI
→ **density raster** → seeded scatter weights for procedural fill between detected trees.

**E — Buildings (param-conditioned procedural).** Footprints (OSM ∪ Overture, clipped, stable id per
building) → height (nDSM zonal 90th-pct, fallback Overture/MS) → **roofer/geoflow or City3D** LOD2 massing +
roof type (or `shapely`+`trimesh` flat extrude for LOD1 in HE-3). **Param-conditioned facade** from the
NAIP/photoreal crop: dominant **colour palette** (k-means), **floor count** = height ÷ ~3 m, **window grid**
via FFT/edge frequency → facade params. Emit massing mesh + placeholder material + the param record + confidence.

**F — AI material synthesis (HE-6; the chosen path over LOD3 facade-geometry detailing).** Crop the real
texture per building/archetype → **StableMaterials** (offline, pinned weights) → tileable BC7
basecolor/normal/roughness → assign. Placeholder materials until then.

**G — Pack to "our tile data."** Per tile: terrain heightfield, water polygons+heights, tree/foliage instance
lists, building meshes + materials + params — each feature with a **stable id** + **confidence**. **On-disk:**
a versioned lean binary (`bincode`/CBOR, schema in `logi_semantic`) for the runtime, **plus** an optional
glTF / 3D Tiles 1.1 export (`EXT_mesh_gpu_instancing` + `EXT_structural_metadata`) for QGIS/CesiumJS
inspection. Content-addressed store under `Saved/LogiTiles/<aoi>/`. Emit the **manifest + QA report** (§11).

---

## 4. Tiling & LOD (the performance foundation)

- **Tiling:** a local quadtree rooted at the AOI bbox (~3.22 km), 4–5 levels → leaf ≈ 100–200 m, keyed by the
  generalised `TileKey`. The AOI is small (~few hundred leaves), so streaming is comfortable and could even
  fully preload — but it goes through the real streaming path to stay scalable + deterministic.
- **Terrain LOD:** 0.7 m posts over 8 km² ≈ 16 M samples raw — **decimate to SSE per level**; keep LOD
  **skirts** to hide cracks.
- **Trees/foliage LOD:** **HISM** with per-mesh LODs; **impostors** beyond a tuned cutoff (§10); **Nanite
  foliage** in the editor lane.
- **Buildings LOD:** mesh LODs; **merge per tile**; **non-Nanite** runtime (ADR-0008).
- **Streaming:** reuse the SSE traversal + bounded-residency LRU. `BakedTileSource` (local fs) implements
  `TileContentSource` — fully **deterministic** (kept as a deterministic source behind the trait).

---

## 5. Rust ingest + runtime — new `logi_semantic` crate

- **`logi_semantic`** (`#![forbid(unsafe_code)]`): the **versioned** tile-store + override schema; a
  `BakedTileSource: TileContentSource` reading the store on the worker pool; integration into
  `logi_3dtiles_scene` frame-packet assembly. Rebase bake-frame (ENU-local/geodetic) → Unreal render space
  via the existing `enu_pos_to_ue` (consistent with
  [streaming.rs](../../Rust/crates/logi_3dtiles_scene/src/streaming.rs)).
- **Frame-packet mapping (mostly existing rails):**
  - Ground mesh → `LogiTerrainPatchDraw` (`material_id` = terrain).
  - Water → `LogiTerrainPatchDraw` flat section (`material_id` = water → animated material). *No new ABI.*
  - Trees/foliage → `LogiInstanceBatchDraw` (**exists**): one batch per (species mesh, material); populate
    `instance_data`. Versioned per-instance layout: `pos f3 (cm, render-local) | quat f4 | scale f3 | type
    u32 | _pad u32` = **48 B**, declared in both the Rust struct and a `static_assert` on the C++ side.
  - Buildings → textured mesh: `LogiTerrainPatchDraw` + the **M0 albedo-texture channel**.
- **Error model (AGENTS.md narrow FFI):** library code returns `Result`; **no `unwrap`/`expect`/`panic!`** in
  runtime paths; the FFI seam (`logi_terrain_ffi`, the *only* `unsafe`) catches panics → `LOGI_TERRAIN_PANIC`
  (never unwinds into C++) and validates the tile-store version + checksum on load (refuses an unknown major
  version, like the ABI guard). Missing/partial tiles **degrade gracefully** (coarser fallback, logged), never crash.
- **ABI:** the only bump is the already-planned M0 texture channel (3→4); instances reuse the existing rail.
- **Gameplay boundary (ADR-0007):** `BakedTileSource` feeds the **render** packet only. Coarse deterministic
  data gameplay needs (ground height, building obstacle footprints, water route cost) is exposed separately
  through the existing coarse terrain query surface — never by the sim reading render geometry across the FFI.

---

## 6. UE render lanes (LogiBevyRuntime)

- **Terrain:** existing proc-mesh + editor-Nanite (heightfield) — unchanged.
- **Trees/foliage (new HISM lane):** consume `LogiInstanceBatchDraw` → one
  `UHierarchicalInstancedStaticMeshComponent` per species; add/update instances **per tile** (no per-instance
  FFI); LOD + impostor; Nanite foliage in the editor. **Placeholder** assets.
- **Water (new):** flat mesh + an **animated water material** (Gerstner or the UE Water plugin), bound by
  `material_id`. **Placeholder** material.
- **Buildings (new):** textured static/proc-mesh, **non-Nanite** runtime; per-tile merge; **placeholder**
  material → AI material at HE-6.
- **Standards:** Unreal conventions (`F/U/A/E`, `UPROPERTY/UCLASS`); **GC discipline** — every retained
  UObject (textures, MIDs, HISM components) held by `TStrongObjectPtr`; **no blocking IO on the game/render
  thread** (fetch/decode on the worker pool, the kernel model); editor-only code under `WITH_EDITOR`; profiled.

---

## 7. The Tile Editor (basic now; ADR-0006-compliant)

**Principle (ADR-0006).** Edit the **baked tile data** (Rust-owned truth), **not** UE actors. Edits are
**validated commands**, **never memory pokes**, **editor-only**, **zero shipping cost**. They write to a
per-tile **override layer**; the bake is **idempotent and merges overrides**, so re-running the ML never
clobbers artist work. This is the "procedural + artist cleanup" stage made concrete.

**v1 (experimentation — basic features, full standards):**
- An **Editor Mode / Editor Utility Widget**: pick a species, then **add / move / delete** tree & foliage
  instances via the standard UE transform gizmo on a single **selection proxy** (ADR-0006 §Decision-1 — one
  proxy, not one per instance).
- On commit: an FFI **command** `logi_semantic_edit_instance(tile_key, op, stable_id, xform)` —
  **editor-only**, behind a cargo feature + a separate `logi_semantic_editor_ffi.h` (the ADR-0006 hot/editor
  ABI split), **validated** against the schema in Rust — patches the Rust-owned instance list, appends to the
  per-tile override file, and re-uploads **only that tile's** instance batch. Selection is **paged/sampled**,
  never per-instance FFI from Slate paint.
- Nice-to-have v1: water-polygon vertex drag.

**Later (a more finished product — sketch only):** terrain heightfield sculpt → re-emit the tile mesh;
building geometry/param edit → mesh-or-param override. Same override-layer mechanism + command discipline.

**Override-merge spec (correctness-critical, tested in §9).** Overrides are a per-tile sidecar of operations
keyed by **stable feature id**: `{add(id, feature)}`, `{move(id, xform)}`, `{delete(id)}`, `{reclassify(id,
type)}`. The bake produces ML features (stable ids derived deterministically from position+class), then
**applies overrides last**: a `delete`/`move`/`reclassify` on an id the ML re-produces is honored; an `add`
persists regardless; a re-bake that no longer produces a moved id keeps the artist's instance (orphan-adopt).
Merge is **idempotent** and **order-independent** within a tile. No ML output ever overwrites an override.

---

## 8. Engineering standards & the quality bar

**Determinism & reproducibility (the spine).** Every stochastic step is **seeded** from the manifest seed;
model weights + every tool/lib version are **pinned + hash-locked**; the bake takes **no network input
except the explicit acquire stage**; outputs are **content-addressed**. The same (sources, seed, config,
tool version) ⇒ **byte-identical tile store** (the golden gate, §9). The runtime `BakedTileSource` is
deterministic. No `Date.now`/wall-clock/`thread_rng` anywhere in the pipeline (AGENTS.md §3.8).

**Rust** (`logi_semantic` + scene hooks): `#![forbid(unsafe_code)]` on every pure crate; the **only** `unsafe`
is the audited FFI seam in `logi_terrain_ffi`, with documented safety invariants. **No `unwrap`/`expect`/
`panic!`/`todo!`/`unimplemented!`** in library/runtime paths — `Result` + status codes; panics caught at the
seam. `cargo clippy --all-targets -- -D warnings` and `cargo fmt --check` are gates. Module-level docs state
invariants (match the existing house style). New deps justified (glam-everywhere; no `nalgebra`).

**C++** (LogiBevyRuntime): Unreal conventions; `TStrongObjectPtr` for all retained UObjects; no gameplay
authority; no blocking IO on game/render threads; editor code under `WITH_EDITOR`; const-correct; profiled
with Unreal Insights/`stat`. New modules need a from-scratch build (Live Coding can't add them).

**Python** (`Tools/logi_bake/`): **strict typing** (`pyright`/`mypy --strict`); `ruff` lint + format (gates);
**pinned, hash-locked** deps (uv lockfile); packaged + **semver-versioned**; config via a typed schema
(pydantic), no hidden globals; **structured logging** + a machine-readable QA report; `pytest` with golden
fixtures (§9). Fail **loud** with provenance on bad/missing data — never emit a silently-degraded tile.

---

## 9. Test, validation & CI gates

Mirrors the Milestone-F §9.5 rigor; nothing ships behind a red gate.

- **Per-crate unit tests:** `logi_semantic` schema round-trips (encode→decode byte-identical); rebase math
  vs known WGS84 points; the per-instance layout round-trip; the override-merge algebra.
- **Golden bake test (the pipeline regression net):** a tiny checked-in **fixture AOI** (≤ a few hundred
  metres; synthetic or a small public sample) bakes to a **checked-in golden** (feature counts, bounds,
  content hashes). Drift fails CI.
- **Determinism / replay gate:** re-baking (sources, seed, config, tool version) yields a **byte-identical**
  store (content-hash equality). This is the production gate for "reproducible."
- **FFI contract tests:** ABI version guard; `size_of` static asserts hold (Rust + C++); instance-batch
  layout round-trips Rust↔C++; **panic-across-FFI** test (inject a panic → expect `LOGI_TERRAIN_PANIC`, no
  unwind); tile-store version guard refuses an unknown major.
- **Override-merge correctness:** property tests — re-bake never loses/overwrites an override; merge is
  idempotent + order-independent; every operation kind covered (§7).
- **Data-quality acceptance** (gated per feature class against a hand-labeled validation subset of the AOI):
  tree detection **precision/recall ≥ target**; building footprint **coverage ≥ target**; **vertical-datum
  residual ≤ ~5 cm** vs a benchmark. Below target ⇒ the bake flags the tile for review, not silent shipping.
- **Performance regression gates:** the §10 budgets asserted (instance counts per tile, draw calls, resident
  bytes) where automatable; profiled otherwise.
- **Visual validation (the user's):** HE milestones render in-editor; screenshots are disabled for the
  agent — the user confirms.

**CI gate list (ADR-0006/0007 style — enforced, not aspirational):**
1. No `AActor` per tree/building/tile; no per-instance `Tick`; no per-instance FFI from Slate paint.
2. Instance/mesh data crosses the FFI **batched per tile** only.
3. Bake is **byte-reproducible** (golden hash) in CI.
4. `cargo test --workspace` green; clippy `-D warnings`; `rustfmt --check`; Python `ruff` + `pyright` +
   `pytest` green.
5. ABI `size_of` static asserts hold; ABI version bumped **iff** layout changed.
6. **Shipping build:** no `logi_semantic_editor_*` symbols; **no Python/ML/bake deps linked** into the cdylib.
7. Memory / draw-call / instance budgets (§10) not exceeded (profiled).
8. **No enrichment render data treated as gameplay authority** without promotion to the coarse deterministic
   representation (ADR-0007) — reviewed at PR.

---

## 10. Performance budgets & protocol

Profile per [Profiling-Checklist.md](../Performance/Profiling-Checklist.md); reconcile the **initial**
targets below against [Budgets.md](../Performance/Budgets.md) and ratify them (HE-7). Numbers are **gates**,
not aspirations.

| Budget | Initial target | Lever |
|---|---|---|
| Frame time (first-person, reference GPU) | **≤ 16.6 ms (60 fps)** | the rest of this table |
| Draw calls in-AOI | **≤ ~2,000/frame** | per-tile merge; HISM batching; impostors |
| Foliage instances (resident) | **10⁵–10⁶**, collapsed to a few HISM draws | HISM/ISM; **impostor** cutoff (tune ~80–150 m); Nanite foliage (editor) |
| Building meshes | merged per tile, mesh LODs, **non-Nanite** (ADR-0008) | per-tile static merge |
| Texture memory | **per-tile BC7 + byte-budget LRU** (Milestone-F risk #4) | LRU mirroring `evict_meshes` |
| Resident tile heap | **bounded LRU** (exists) | small AOI ⇒ comfortable working set |
| Streaming hitch (p99) | **< a frame** | bounded in-flight; worker-pool decode; prefetch |

Instancing is the dominant lever: suburban 8 km² ≈ tens of thousands of trees + 10⁵+ understory → HISM
collapses them to a handful of draw calls; impostors beyond the tuned cutoff; Nanite foliage in the editor.
Determinism is a perf concern too: the seeded bake makes profiling runs comparable.

---

## 11. Formats, versioning & observability

**Versioned surfaces (each refuses an unknown major, like the ABI guard):**
- **Tile-store format** — header carries `format_version`, bake-tool semver, config hash, AOI, CRS, seed.
- **Override schema** — versioned; migration is explicit, never silent drift.
- **Per-instance layout** — versioned (declared Rust + C++ `static_assert`).
- **FFI ABI** — the existing `LOGI_TERRAIN_ABI_VERSION` (M0 bump 3→4).

**Provenance ledger** — per input: source, version/acquisition date, checksum, license (§2). Lets a shippable
open-data-only subset be carved later (Google as reference) without re-architecting.

**Observability:**
- **Bake QA report** (machine-readable + human summary): per-class counts, confidence histograms, coverage
  %, rejected-feature log, **datum residuals**, override-merge log, source provenance.
- **Runtime telemetry:** extend `LogiTerrainTelemetry` with resident instance/feature counts per tile so the
  Logi telemetry panel surfaces enrichment cost.
- **Tile Editor:** every edit appended to the per-tile override file = a complete, replayable audit trail.

---

## 12. Milestone breakdown & Definition of Done

Each milestone renders something and is **not done** until its DoD is green. DoD = the named exit + **all
applicable §9 gates** (build/lint/test/golden/determinism/CI) + the §10 budgets it touches.

| # | Deliverable | Definition of Done |
|---|---|---|
| **HE-0** | AOI + tiling + acquire scripts; **terrain-only** bake (DTM → heightfield tiles) | ground streams + renders first-person; **datum residual ≤ ~5 cm validated**; `BakedTileSource` deterministic (golden + replay gates green) |
| **HE-1** | Water extraction + animated-water lane | water renders at correct height; polygon QA metrics logged; gates green |
| **HE-2** | Tree detection + **HISM instancing** (placeholder) | detected trees instanced; `LogiInstanceBatchDraw` populated + consumed **batched per tile**; tree precision/recall ≥ target; instance budget met |
| **HE-3** | Building footprints → **LOD1 param-procedural** massing + placeholder material | buildings render textured + placed; per-tile merge; footprint coverage ≥ target; draw-call budget met |
| **HE-4** | **Basic Tile Editor** (move/add/delete trees) + override persistence | an artist edits a tree; **edit survives a re-bake** (override-merge property tests green); editor-only symbols absent from Shipping (CI gate 6) |
| **HE-5** | Param-conditioned facade params + **LOD2 roofs**; understory fill | roofs + facade params drive massing; foliage density scatter; gates green |
| **HE-6** | **AI material synthesis** (StableMaterials, pinned) | generated PBR replaces placeholders; bake still byte-reproducible (pinned weights) |
| **HE-7** | **Performance pass** | §10 budgets **ratified vs Budgets.md** and met on the reference GPU; profiled per the checklist |

---

## 13. Repo layout / deliverables

- `Tools/logi_bake/` — Python package + CLI + **uv lockfile** + pinned model weights (the offline bake);
  `pytest` suite + golden fixtures.
- `Rust/crates/logi_semantic/` — `#![forbid(unsafe_code)]` schema + `BakedTileSource`; packet-assembly hooks
  in `logi_3dtiles_scene`; the M0 texture-channel ABI bump in `logi_terrain_ffi`; the **editor-only**
  `logi_semantic_editor_ffi` (cargo feature).
- C++ (`LogiBevyRuntime`) — HISM lane, animated-water lane, building textured lane; the **Tile Editor** mode
  under `WITH_EDITOR`.
- `Content/` — **placeholder** tree/foliage/water/building/material assets.
- `Saved/LogiTiles/<aoi>/` — the baked tile store + override sidecars + QA report (git-ignored build output;
  golden **fixtures** are checked in under the test tree).

---

## 14. Risks / open questions

| # | Risk | Mitigation |
|---|---|---|
| 1 | **Vertical datum** (NAVD88 geoid vs WGS84 ellipsoid) → AOI floats/sinks | geoid18 in stage A; **≤ ~5 cm residual gate** (§9) vs a benchmark; validated in HE-0 |
| 2 | ML accuracy (~75–90 %) → missed/merged trees, footprint gaps | confidence + per-class acceptance gates; the Tile Editor absorbs the rest; **budget the cleanup labor** |
| 3 | Re-bake **clobbering** artist edits | the override-merge spec (§7) + property tests (§9); ML never overwrites an override |
| 4 | **Google photoreal prior is license-deferred** | provenance ledger; AI-material route launders the look; shippable subset = open data + Google as reference only |
| 5 | Python ↔ baked-tile **boundary** is the architectural commitment | ratified by ADR-0011; heavy ML offline; cdylib stays lean; bake reproducible |
| 6 | LOD2 tools (roofer/geoflow/City3D) are external **bake-time** C++ bins | pin versions; LOD1 extrude fallback; not a runtime dep |
| 7 | Dense-foliage first-person perf | instancing + impostors + Nanite foliage (§10); HE-7 budget gate |
| 8 | Which gameplay features need **coarse promotion** (ADR-0007) | enumerate per feature (buildings/water/ground) when `logi_sim` consumes them; render-only until then |

---

## 15. Source pointers

- LiDAR: [USGS LidarExplorer](https://apps.nationalmap.gov/lidar-explorer/) ·
  [3DEP on AWS Open Data](https://registry.opendata.aws/usgs-lidar/) (Delaware Valley High Density QL2).
- Buildings: [roofer](https://github.com/3DBAG/roofer) · [geoflow building reconstruction](https://github.com/geoflow3d/gfp-building-reconstruction)
  · [City3D](https://github.com/tudelft3d/City3D) · [Point2Building](https://arxiv.org/pdf/2403.02136).
- Trees: [PyCrown](https://github.com/manaakiwhenua/pycrown) · [DeepForest](https://www.biorxiv.org/content/10.1101/2020.07.07.191551.full.pdf).
- Landcover: [ESA WorldCover v200](https://developers.google.com/earth-engine/datasets/catalog/ESA_WorldCover_v200).
- Footprints+heights: [Overture Buildings](https://docs.overturemaps.org/guides/buildings/) ·
  [Google Open Buildings 2.5D](https://blog.google/products-and-platforms/products/maps/google-open-buildings-dataset/) ·
  [MS GlobalMLBuildingFootprints](https://github.com/microsoft/GlobalMLBuildingFootprints/).
- Format: [3D Tiles Next — EXT_mesh_gpu_instancing + EXT_structural_metadata](https://cesium.com/blog/2021/11/10/introducing-3d-tiles-next/).
- AI materials: [StableMaterials](https://huggingface.co/gvecchio/StableMaterials) · [Chord (image→PBR)](https://arxiv.org/pdf/2509.09952).
- LogiUE: [ADR-0011](../Architecture/ADR-0011-Semantic-Tile-Enrichment.md) · [Milestone-F plan](Milestone-F-3DTiles-Rust-Stack.md)
  · [ADR-0006](../Architecture/ADR-0006-Editor-Inspectability.md) · [ADR-0007](../Architecture/ADR-0007-Actorless-Rust-Terrain.md)
  · [ADR-0008](../Architecture/ADR-0008-Terrain-Nanite-Lane.md).
