# Plan — Hero-Location Semantic Tile Enrichment (New Castle County, DE)

**Status:** Plan (2026-06-05). **Builds on** Milestone F (M0 texture rail, M3 traversal, M4 glTF decode).
**Seeds** a future ADR-0011 (Data-Driven Semantic Enrichment). **Supersedes** the procedural-only
M-G foliage clause of the Milestone-F plan with a *data-driven* enrichment layer (real positions where
detected; seeded procedural fill elsewhere).

**Target:** render — first-person, efficiently — a **1 mi-radius** area of interest (AOI) centred on
**39.643178, -75.617567** (New Castle County, DE; Newark/Christiana/Bear area), reconstructed from
real-world data as **our own assets**: a ground terrain mesh, instanced trees + foliage, an animated
water asset, and param-conditioned procedural buildings. The real photoreal mesh/imagery is a
**prior/inspiration**, never shipped geometry.

---

## 1. Shape of it & the load-bearing finding

```
SOURCES (fused in one geodetic/ECEF frame — logi_geospatial)
  USGS 3DEP QL2 LiDAR (LAZ)   ─┐
  NAIP aerial RGB+NIR         ─┤
  OSM (footprints/water/…)    ─┼─►  OFFLINE PYTHON ML BAKE  (Tools/logi_bake/)
  Overture/MS building heights─┤       ground · water · trees/foliage · buildings
  ESA WorldCover 10m          ─┤
  Google photoreal (prior)    ─┘            │  (seeded, idempotent, merges artist overrides)
                                            ▼
                          ENRICHED TILE STORE  (Saved/LogiTiles/, content-addressed)
                                            │
                                            ▼
        Rust ingest  —  logi_semantic (BakedTileSource: TileContentSource)
                          → logi_3dtiles_scene frame-packet assembly (RTC rebase)
                                            │  LogiFramePacket (existing rails)
                                            ▼
        UE render lanes (LogiBevyRuntime)            +   TILE EDITOR (ADR-0006 shells)
          terrain: proc-mesh / editor-Nanite              move/add/delete instances
          trees+foliage: HISM (LogiInstanceBatchDraw)     edits → per-tile override layer
          water: flat mesh + animated material            re-bake merges (never clobbers)
          buildings: textured non-Nanite mesh
```

**The load-bearing finding (FFI audit).** The kernel frame packet **already carries the rails this needs**
— [`LogiInstanceBatchDraw`](../../Rust/crates/logi_terrain_ffi/include/logi_terrain_ffi.h) (`mesh_handle` +
`instance_data` blob + `instance_stride` + `material_id`), the `material_page_uploads` rail, the
`attributions` rail — all present in `LogiFramePacket`, **currently null on the M2 globe path**. So:

- **Trees/foliage** ride the existing `instance_batches` rail → **no struct change**, just a documented
  per-instance layout + the C++ HISM consumer.
- **Buildings/water** are *textured/flat meshes* → ride `LogiTerrainPatchDraw` + the **M0 texture channel**
  (the one already-planned ABI 3→4 bump in Milestone-F §9.3). Water = a flat patch with `material_id` =
  the animated-water material; no new ABI.
- The enriched local tiles emit the **rich `LogiFramePacket`** (the kernel packet with the instance/upload
  rails — the Milestone-F §3 end-state for `logi_3dtiles_scene`), distinct from M2's interim lean
  `LogiGlobeFrame`. A `BakedTileSource` is just another `TileContentSource`; selection / cache / scheduler
  reuse unchanged.

**Constitution check.** The bake is **offline content authoring**, not gameplay truth; enriched tiles are
render/content data; Rust owns selection + placement; UE renders shells (ADR-0005/0007). The heavy ML
toolchain lives **outside** the `logi_terrain.dll` cdylib — the runtime only *consumes* baked tiles.

---

## 2. The AOI & the data (concrete shopping list)

- **AOI math:** radius 1 mi = **1609.34 m**; area ≈ **8.13 km²**; bbox ≈ 3.22 km square. Centre
  `lat 39.643178, lon -75.617567`. Planar processing CRS = **UTM 18N / EPSG:26918 (NAD83)**; vertical
  **NAVD88 (geoid18)** → **convert to WGS84 ellipsoid** before ECEF (the #1 co-registration gotcha — §11).
- **Step 0 — confirm land cover** for the exact point (suburban mix expected: residential + commercial +
  roads + scattered woodland + ponds/wetlands). Drives which extractors matter.

| Source | Gives | Res / format | Access |
|---|---|---|---|
| **USGS 3DEP QL2 LiDAR** (Delaware Valley High Density) | heights, ground/non-ground, canopy structure, roof planes | ~0.7 m spacing, 2 pts/m², LAZ | [LidarExplorer](https://apps.nationalmap.gov/lidar-explorer/) + [AWS Open Data](https://registry.opendata.aws/usgs-lidar/) (free) |
| **NAIP** aerial | tree crowns (DeepForest), NDVI/NDWI, facade colour | ~0.6 m RGB+NIR, GeoTIFF | USDA / cloud mirrors |
| **OpenStreetMap** | building footprints, water, roads, landuse | vector | osmnx / pyrosm |
| **Overture / MS / Google Open Buildings** | footprints **+ heights** at scale | 4 m heights | overturemaps CLI / DuckDB |
| **ESA WorldCover** | 11-class landcover (tree/shrub/grass/water/built-up) | 10 m | tile download |
| **Google photoreal** (your M2 path) | visual prior: palette, massing, facade rhythm | — | **license-deferred (§11)** |

LiDAR is the geometric backbone; rasters + vectors disambiguate class; the photoreal mesh is colour/style
reference only.

---

## 3. Offline bake pipeline — `Tools/logi_bake/` (Python ML)

A CLI package (pinned env via **uv**/conda; **seeded**; **idempotent**; **merges artist overrides** — §7).
Heavy ML stays here, off the cdylib. Stages:

**Env / libraries**
- Geo I/O: `rasterio`, `geopandas`, `shapely`, `pyproj`, `osmnx`/`pyrosm`, `overturemaps`/`duckdb`.
- LiDAR: **PDAL** (+ python bindings), `laspy`; ground filter `filters.smrf`/CSF.
- Vegetation: **DeepForest** (RGB crown detector), **PyCrown** (CHM local-maxima/watershed), `scikit-image`.
- Buildings: **roofer**/**geoflow** or **City3D** (footprint + point cloud → LOD2; external bins), else
  `shapely`+`trimesh` extrude for LOD1.
- Mesh ops: `trimesh`, `Open3D`, `pymeshlab`.
- AI materials (deferred): `diffusers`+`torch` (**StableMaterials**).

**A — Acquire & co-register.** Clip every source to the AOI bbox; reproject to EPSG:26918; resolve the
vertical datum to ellipsoid. Cache raw downloads under `Tools/logi_bake/cache/`.

**B — Ground / terrain.** PDAL ground classification → **DTM** (ground) + **DSM** (first-return) rasters;
**nDSM = DSM − DTM** (height-above-ground — the key derived layer). DTM → tiled **heightfield meshes** for
the existing terrain lane (decimated to SSE per level — §4).

**C — Water.** `OSM water ∪ WorldCover water ∪ NDWI(NAIP)` → mask → polygonize (`rasterio.features` +
`shapely`); height = min DTM around each boundary; flatten the terrain mesh at the waterline. Output: water
polygons + heights.

**D — Trees / foliage.** CHM = nDSM ∩ vegetation(WorldCover tree/shrub ∪ NDVI). **DeepForest** on NAIP RGB
⊕ **PyCrown** on the CHM (fuse detections) → **per-tree**: position, height, crown radius, coarse type
(deciduous/conifer from crown shape + NDVI). Understory: grass/shrub/NDVI → **density raster** → seeded
scatter weights for the procedural fill between detected trees.

**E — Buildings (param-conditioned procedural).** Footprints (OSM ∪ Overture, clipped) → height (nDSM
zonal 90th-pct, fallback Overture/MS) → **roofer/geoflow or City3D** for LOD2 massing + roof type (or
`shapely`+`trimesh` flat extrude for LOD1 in HE-3). **Param-conditioned facade** from the NAIP/photoreal
texture crop per footprint: dominant **colour palette** (k-means), **floor count** = height ÷ ~3 m, **window
grid** via FFT/edge frequency → facade params. Emit massing mesh + placeholder material + the param record.

**F — AI material synthesis (deferred to HE-6; the recommended choice over facade-geometry detailing).**
Crop the real texture per building/archetype → **StableMaterials** (diffusers, offline) → tileable
basecolor/normal/roughness (BC7) → assign to the generated mesh. Until then, placeholder materials.
**We do NOT do LOD3 facade-geometry detailing during experimentation** (window/door geometry is high-effort,
high-poly, premature). Param-procedural massing + AI materials gives the look at a fraction of the cost.

**G — Pack to "our tile data."** Per tile: terrain heightfield, water polygons+heights, tree/foliage
instance lists, building meshes + materials + params. **On-disk:** a lean binary (`bincode`/CBOR) for the
runtime, **plus** an optional **glTF / 3D Tiles 1.1 export** with `EXT_mesh_gpu_instancing` +
`EXT_structural_metadata` for inspection in QGIS/CesiumJS during dev. Content-addressed store under
`Saved/LogiTiles/<aoi>/`. A manifest records AOI, CRS, tiling, **seed**, source provenance + attributions,
and the override-merge log.

---

## 4. Tiling & LOD (the performance foundation)

- **Tiling:** a local quadtree rooted at the AOI bbox (~3.22 km), 4–5 levels → leaf ≈ 100–200 m, keyed by
  the generalised `TileKey`. The AOI is small (~few hundred leaves), so streaming is comfortable and could
  even fully preload — but it goes through the real streaming path to stay scalable.
- **Terrain LOD:** 0.7 m posts over 8 km² ≈ 16 M samples raw — **decimate to SSE per level**; keep the LOD
  **skirts** (already in the terrain lane) to hide cracks.
- **Trees/foliage LOD:** **HISM** with per-mesh LODs; **impostors** beyond ~80–150 m; **Nanite foliage** in
  the editor lane.
- **Buildings LOD:** mesh LODs; **merge per tile**; **non-Nanite** runtime (ADR-0008).
- **Streaming:** reuse the SSE traversal + bounded-residency LRU. `BakedTileSource` (local fs) implements
  `TileContentSource` — fully **deterministic** (keep it as the deterministic test source too).

---

## 5. Rust ingest + runtime — new `logi_semantic` crate

- **`logi_semantic`**: schema structs mirroring the bake blob; `BakedTileSource: TileContentSource` reads the
  store on the worker pool; feeds `logi_3dtiles_scene`'s frame-packet assembly. Rebase bake-frame
  (ENU-local/geodetic) → Unreal render space via the existing `enu_pos_to_ue` (consistent with
  [streaming.rs](../../Rust/crates/logi_3dtiles_scene/src/streaming.rs)).
- **Frame-packet mapping (mostly existing rails):**
  - Ground mesh → `LogiTerrainPatchDraw` (`material_id` = terrain).
  - Water → `LogiTerrainPatchDraw` flat section (`material_id` = water → animated material). *No new ABI.*
  - Trees/foliage → `LogiInstanceBatchDraw` (**exists**): one batch per (species mesh, material); populate
    `instance_data`. Proposed per-instance layout (`instance_stride`): `pos f3 (cm, render-local) | quat f4
    | scale f3 | type u32 | _pad u32` = **48 B** (can shrink to 24 B for foliage). Documented layout, not a
    struct change.
  - Buildings → textured mesh: `LogiTerrainPatchDraw` + the **M0 albedo-texture channel**.
- **ABI:** the only bump is the **already-planned M0 texture channel** (3→4, Milestone-F §9.3). Instances
  reuse the existing rail.

---

## 6. UE render lanes (LogiBevyRuntime)

- **Terrain:** existing proc-mesh + editor-Nanite (heightfield) — unchanged.
- **Trees/foliage (new HISM lane):** consume `LogiInstanceBatchDraw` → one
  `UHierarchicalInstancedStaticMeshComponent` per species; add/update instances per tile; LOD + impostor;
  Nanite foliage in the editor. **Placeholder** tree/grass/shrub assets.
- **Water (new):** flat mesh + an **animated water material** (Gerstner or the UE Water plugin), bound by
  `material_id`. **Placeholder** water material.
- **Buildings (new):** textured static/proc-mesh, **non-Nanite** runtime; per-tile merge; **placeholder**
  material → AI material at HE-6.
- **GC discipline:** `TStrongObjectPtr` for textures / MIDs / HISM components (the runtime-texture footgun
  the plan already calls out).

---

## 7. The Tile Editor (basic now; their explicit ask)

**Principle (ADR-0006):** edit the **baked tile data** (Rust-owned truth), **not** UE actors. Edits write to
a **per-tile override layer**; the bake is **idempotent and merges overrides** on re-run, so re-running the
ML never clobbers artist work. This *is* the "procedural + artist cleanup" stage made concrete.

**v1 (experimentation — basic):**
- An **Editor Mode / Editor Utility Widget**: pick a species, then click to **add / move / delete** tree &
  foliage instances; standard UE transform gizmo on a single **selection proxy** (ADR-0006).
- On commit: FFI `logi_semantic_edit_instance(tile_key, op, index, xform)` → patch the Rust-owned instance
  list + append to the per-tile override file; re-upload **only that tile's** instance batch.
- Nice-to-have v1: water-polygon vertex drag.

**Later (a more finished product — sketch only):** terrain heightfield sculpt → re-emit the tile mesh;
building geometry / param edit → mesh-or-param override. Same override-layer mechanism.

**Persistence / merge:** overrides are a sidecar per tile (adds/moves/deletes keyed by a **stable instance
id**); the bake applies ML output, then overrides, then re-packs. Deterministic and replayable.

---

## 8. Milestone breakdown (each renders something)

| # | Deliverable | Exit criteria |
|---|---|---|
| **HE-0** | AOI + tiling + acquire scripts; **terrain-only** bake (DTM → heightfield tiles) | the ground surface streams + renders first-person via the existing lane; `BakedTileSource` deterministic |
| **HE-1** | Water extraction + animated-water lane | ponds/streams render with the placeholder water material at the right height |
| **HE-2** | Tree detection + **HISM instancing** (placeholder asset) | detected trees render as instances; `LogiInstanceBatchDraw` populated + consumed; LOD/impostor |
| **HE-3** | Building footprints → **LOD1 param-procedural** massing + placeholder material (textured lane) | buildings render textured + correctly placed; per-tile merge |
| **HE-4** | **Basic Tile Editor** (move/add/delete trees) + override persistence + re-bake merge | an artist moves/deletes a tree; edit survives a re-bake |
| **HE-5** | Param-conditioned facade params + **LOD2 roofs** (roofer/geoflow); understory foliage fill | roofs + facade params drive massing; foliage density scatter |
| **HE-6** | **AI material synthesis** (StableMaterials) for buildings | generated PBR materials replace placeholders |
| **HE-7** | **Performance pass** | LOD/impostor/texture-LRU/draw-call budgets met; profiled (§9) |

---

## 9. Performance plan (the "efficiently and performantly" requirement, made explicit)

- Budgets per [Docs/Performance/Budgets.md](../Performance/Budgets.md); profile per
  [Profiling-Checklist.md](../Performance/Profiling-Checklist.md).
- **Instancing is the dominant lever:** suburban 8 km² ≈ tens of thousands of trees + 100k+ understory →
  HISM collapses them to a handful of draw calls; **impostors** beyond ~100 m; Nanite foliage (editor).
- **Terrain:** Nanite (editor) / decimated proc-mesh (runtime) + skirts.
- **Buildings:** merged per tile, mesh LODs, non-Nanite.
- **Textures:** per-tile BC7 + **byte-budget LRU** (Milestone-F risk #4).
- **Streaming:** bounded-residency LRU (exists); small AOI → comfortable working set.
- **Determinism:** seeded bake; deterministic runtime (`BakedTileSource` behind the trait).

---

## 10. Repo layout / deliverables

- `Tools/logi_bake/` — Python package + CLI + pinned env (the offline bake).
- `Rust/crates/logi_semantic/` — schema + `BakedTileSource`; packet-assembly hooks in `logi_3dtiles_scene`;
  the M0 texture-channel ABI bump in `logi_terrain_ffi`.
- C++ (`LogiBevyRuntime`) — HISM lane, animated-water lane, building textured lane, **Tile Editor** mode
  (+ an editor module if needed).
- `Content/` — **placeholder** tree/foliage/water/building/material assets.
- `Saved/LogiTiles/<aoi>/` — the baked tile store + override sidecars.

---

## 11. Risks / open questions

| # | Risk | Mitigation |
|---|---|---|
| 1 | **Vertical datum** mismatch (NAVD88 geoid vs WGS84 ellipsoid) → everything floats/sinks | apply the geoid model in stage A; validate a known benchmark; cross-check against LiDAR-vs-ellipsoid |
| 2 | ML accuracy (WorldCover ~75%; missed/merged trees; footprint gaps) | the Tile Editor absorbs it; **budget the cleanup labor** — it's the cost of "procedural + cleanup" |
| 3 | Re-bake **clobbering** artist edits | idempotent override merge keyed by stable id (§7); never re-derive over an override |
| 4 | **Google photoreal as a prior is license-deferred** | the AI-material route launders the look into non-derivative materials; a shippable build uses Google as on-screen reference only and derives geometry from LiDAR/OSM/Overture |
| 5 | Python ↔ baked-tile **boundary** is the architectural commitment | heavy ML stays offline; cdylib stays lean; bake is reproducible + seeded |
| 6 | LOD2 tools (roofer/geoflow/City3D) are external C++ **bake-time** bins | fine offline; pin versions; fall back to LOD1 extrude if a build is unavailable |
| 7 | Dense-foliage first-person perf | instancing + impostors + Nanite foliage (§9); HE-7 budget pass |

---

## 12. Source pointers

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
- LogiUE: [Milestone-F plan](Milestone-F-3DTiles-Rust-Stack.md) · [ADR-0007](../Architecture/ADR-0007-Actorless-Rust-Terrain.md)
  · [ADR-0006](../Architecture/ADR-0006-Editor-Inspectability.md) · [ADR-0008](../Architecture/ADR-0008-Terrain-Nanite-Lane.md).
