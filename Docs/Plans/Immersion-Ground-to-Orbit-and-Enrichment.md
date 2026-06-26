# Plan — World-Class Immersion: Ground↔Orbit Substrate + Real-World Enrichment

**Status:** Plan / draft (2026-06-09). **Lineage:** the successor to and superset of
[Hero-Location-Semantic-Enrichment.md](Hero-Location-Semantic-Enrichment.md) (HE-0..HE-7) — that plan
is the *inspiration* and its milestones fold into Pillar B here. **Builds on** Milestone F (the native
Rust 3D Tiles stack, [ADR-0009](../Architecture/ADR-0009-Native-Rust-3DTiles-Stack.md)),
[ADR-0007](../Architecture/ADR-0007-Actorless-Rust-Terrain.md) (actorless), and
[ADR-0011](../Architecture/ADR-0011-Semantic-Tile-Enrichment.md) (offline enrichment bake).

**Author context:** synthesised from a 5-stream opus research pass this session — (A) an enrichment
pipeline state audit, (B) a streaming/rendering fitness audit against the two requirements, plus deep
external research on (C) seamless ground↔orbit planet rendering, (D) generative/upsampling content
models, and (E) runtime aesthetic render systems in UE 5.8. Findings are cited inline (`file:line` for
code, URLs for external).

---

## 0. Thesis (one paragraph)

The terrain **streamer is canonical and correct** — it is a faithful Rust port of cesium-native's SSE
traversal (kick / ancestor-fallback / loading-descendant-limit), and it never stalls the game thread.
But it is **not yet fit for the two headline requirements**, for two structural reasons the audit pins
down: (1) ground fidelity **hard-caps at the photoreal tile's leaf resolution** and there is **no
collision mesh**, so a player on foot has neither crisp ground nor anything to stand on; and (2) an
instant ground→orbit teleport **converges slowly and blurrily** (re-roots traversal + bounded per-step
fetch + a static, non-camera-tracking origin) rather than presenting an instant coarse-complete planet.
The fix is **two composable pillars**: **Pillar A** hardens the substrate (always-resident coarse
levels, destination prefetch, real camera-tracking origin rebasing, a **near-field detail + collision
layer**), and **Pillar B** is the enrichment bake — the **visual models** that turn real-world data into
our own assets (procedurally-amplified terrain, animated water, wind-reactive trees, up-sampled
buildings) that the near-field layer renders at first-person quality. Pillar A must lead: it is both the
structural blocker for "walk anywhere" + "teleport without stutter" **and** the rail Pillar B rides.

---

## 1. The bar (non-negotiable requirements)

1. **Walk-anywhere, world-class ground.** First/third-person on foot, anywhere on Earth, sees
   realistic, crisp, stable near-field terrain — not a gray photogrammetric crust. AAA visual quality.
2. **Instant ground→orbit, zero stutter.** In debug, the camera teleports from any ground point to high
   orbit and back. The terrain renderer must **not hitch** and must always present a *complete* (if
   coarse) world that **sharpens** — never a hole, never a freeze — across ~7 orders of scale.
3. **The constitution holds** ([AGENTS.md](../../AGENTS.md), ADR-0005/0007): Rust owns truth; UE renders
   shells; actorless; narrow batched one-directional FFI; deterministic near-set; heavy ML offline;
   scalable to a streamed planet and a million agents.

---

## 2. Where we are (condensed audit — this session)

### 2A. Enrichment pipeline = **terrain-only** (Pillar B is ~10% built)

| Milestone | State | Evidence |
|---|---|---|
| **HE-0** terrain bake + deterministic store | **Done, extended** | `Tools/logi_bake/` runs (synthetic + real Terrarium/GeoTIFF DEM, datum gate ≤5 cm, golden+replay gates); `logi_semantic` decodes `.lst` heightfield → uniform/RTIN mesh + skirts (`format.rs`, `mesh.rs`) |
| **HE-1..HE-7** water / trees / buildings / editor / facades / AI-materials / perf | **Not started** | No water/tree/building/override types in `logi_semantic` (`lib.rs:29-33`); `LogiInstanceBatchDraw` exists **only in docs** (zero code hits); baked FFI carries geometry only — *"no texture fields"* (`ffi_baked.rs:190`); the `vector`/`trees`/`materials` Python extras are declared but **no module imports them** |

Two structural gaps beyond "unwritten features": **(i)** `BakedTileSource` is a real `TileContentSource`
(`store.rs:104-157`) but is **NOT wired into the live `logi_3dtiles_scene` streaming kernel** — the
baked lane uses a separate eager-mesh `BakedWorld` + its own FFI (`ffi_baked.rs`), bypassing the
residency streamer; **(ii)** there are **no enrichment rails in the FFI** (no instance batch, no texture
channel, no patch struct) — so even a finished bake has nothing to cross on today.

### 2B. Streaming/rendering = **canonical streamer, wrong shape for the two requirements**

The streamer mirrors cesium-native deliberately (same SSE formula, same kick / skip-LOD /
ancestor-fallback, byte-budgeted LRU; Milestone-F §1). The audit verified, against current code:

- **Verdict R1 (walk-around ground): NO at the limit.** Detail caps at the leaf tile's resolution —
  nothing synthesizes finer geometry/texture (no clipmap, no virtual-texture detail layer). And tiles
  are **`NoCollision`** (`LogiTileMeshComponent.cpp:414`): a pawn has nothing to stand on.
- **Verdict R2 (instant ground→orbit, zero stutter): NO at the extreme.** The game thread never stalls
  (async decode/upload, O(Δ) poll), but a one-frame teleport **re-roots traversal** (`geo_world.rs:1843`)
  and issues only `MAX_NEW_FETCHES_PER_STEP=32` cold fetches/step (`geo_world.rs:477`), so the
  destination **streams in over seconds, coarse→fine**. The travel-vector prefetch keys on
  `cam − prev_camera` (`geo_world.rs:2225`) and is **defeated by a discontinuous jump**. The one true
  stutter source is the **synchronous `fs::write` of decoded geometry on the stream thread**
  (`geo_world.rs:719`).
- **Precision drift (load-bearing).** The origin is documented as a *"camera-tracking floating origin"*
  (`logi_geospatial/src/lib.rs:4`, `logi_qmesh/src/lib.rs:7`) but the as-built `EnuFrame.origin_ecef` is
  **pinned at construction** (`world.rs:39-41`) with **no rebase/recenter mutator anywhere** (verified by
  grep). f64→f32 math is otherwise robust (the 0.76 m quilt bug is fixed). Consequence: far from the
  hero origin, f32 render coords grow large → jitter; a true planet-walk needs the rebasing the comments
  *claim* but the code lacks.
- **Per-step fixed costs that lengthen convergence:** `sse_cull_lru` is an **O(pool=24,576)** float scan
  with no `Changed` gate (`geo_world.rs:634`); `reconcile_index` is **O(index)/step** (`geo_world.rs:2550`);
  `DECODE_BATCH=1` (`geo_world.rs:602`). (Same findings as
  [ECS-Round4 research](../Research/2026-06-09-ECS-Round4-Pipeline-Fork-GPU-Research.md).)

**External confirmation that our shape is right but incomplete.** The canonical cesium-native selection
algorithm we mirror prevents pop/holes precisely via `ancestorMeetsSse` (keep showing detail while
zooming out), **kicking** (render the parent until *all* children load), `loadingDescendantLimit` (show
intermediate detail, never nothing), and `forbidHoles` (newly-visible areas load at Normal priority)
([cesium-native selection algorithm](https://cesium.com/learn/cesium-native/ref-doc/selection-algorithm-details.html)).
We implement the first three. What guarantees *instant* coverage on a teleport is the **always-resident
coarse level + destination prefetch** — which we lack.

---

## 3. Shape of the solution — two pillars

```
                          ┌──────────────────────────────────────────────────────────┐
   REAL-WORLD DATA        │  PILLAR B — ENRICHMENT BAKE  (offline, Tools/logi_bake/)   │
   DEM · LiDAR · NAIP ───►│  VISUAL MODELS (deterministic, version-pinned, content-    │
   OSM/Overture · photoreal│  addressed):                                              │
                          │   terrain : DEM super-res → erosion amplification          │
                          │   water   : extract extent/height/flow                     │
                          │   trees   : detect position/height/species → asset id      │
                          │   building: footprint+height → massing → facade grammar →  │
                          │             image→PBR material laundering ("up-sample")     │
                          └───────────────┬──────────────────────────────────────────-┘
                                          │ versioned enriched tile store (per region)
                                          ▼
   ┌──────────────────────────────────────────────────────────────────────────────────┐
   │  PILLAR A — GROUND↔ORBIT SUBSTRATE  (Rust kernel + UE render lanes, runtime)        │
   │                                                                                    │
   │  FAR layer  : streamed photoreal 3D Tiles (existing) — orbit→mid, non-authoritative│
   │  NEAR layer : enriched / detail layer — crisp ground + collision + our assets      │
   │                                                                                    │
   │  • always-resident coarse pyramid   • destination prefetch on big view deltas      │
   │  • camera-tracking origin rebasing  • RVT-cached ground detail + collision         │
   │  • bounded per-step work (off-thread fs)   • atmosphere/depth coherence            │
   └──────────────────────────────────────────────────────────────────────────────────┘
                                          │ LogiFramePacket (existing rails + NEW enrichment rails, batched per tile)
                                          ▼
                            UE 5.8 render shells (actorless, instanced/proxy)
```

**The load-bearing finding:** the photoreal stream stays the **far/establishing** layer (it is great
from the air and is *reference quality* up close at best). World-class **on-foot** fidelity comes from a
**separate near-field layer** — RVT detail materials + our enriched assets + a coarse-promoted collision
mesh — composited over (or replacing) the photoreal crust near the player. This is the only path that
both **breaks the leaf-resolution cap** and **gives the pawn something to stand on**, and it is already
the sanctioned direction (ADR-0011 enrichment + ADR-0007 coarse-for-gameplay). Pillar A builds the rail;
Pillar B fills it with content.

---

## 4. Pillar A — Ground↔Orbit Substrate (substrate milestones, **S-series**)

The cheap, high-leverage fixes first; the structural near-field layer next. All pure-Rust kernel work
unless noted; all inside the constitution.

### S-1 — No-stutter traversal (the teleport guarantee)
- **Pin the coarsest N levels permanently resident** (never LRU-evict above a depth/error threshold).
  Then *any* teleport instantly shows a complete coarse planet that sharpens — converting "blurry
  arrival over seconds" into "instant coarse-complete, then sharpen." This is the production
  "always keep a coarse level resident so you never show a hole" principle
  ([cesium-native](https://cesium.com/learn/cesium-native/ref-doc/selection-algorithm-details.html)).
- **Destination prefetch on large view deltas.** When the view delta exceeds a threshold (a teleport),
  run the coarse lookahead traversal **at the new camera pose** and pre-warm its column — *not* a linear
  `cam − prev` extrapolation (which a jump defeats, `geo_world.rs:2225`). Small change, directly attacks
  the convergence latency.
- **Hoist the decode `fs::write` off the stream thread** (`geo_world.rs:719`) to a writer thread — the
  one real stream-thread stutter under a teleport burst (also flagged by
  [ECS-Round4](../Research/2026-06-09-ECS-Round4-Pipeline-Fork-GPU-Research.md)).
- **Gate the O(pool) per-step scans:** add a `Changed`/dirty gate to `sse_cull_lru` (`geo_world.rs:634`)
  and bound `reconcile_index` (`geo_world.rs:2550`); raise `DECODE_BATCH`. Shortens the published-frame
  cadence during fast traversal.
- **Adaptive per-step fetch budget** keyed to "is the camera moving fast / did it just teleport" so the
  destination column fills in a few steps, while steady-state stays bounded.

### S-2 — Precision for a true planet-walk (camera-tracking origin)
- **Implement the camera-tracking origin rebasing the comments already promise.** Add a `recenter(ecef)`
  that re-folds `EnuFrame.origin_ecef` toward the camera past a snap threshold (the Milestone-F
  `REBASE_SNAP` idea, ~2 km) and re-rebases resident tile geometry. This is the canonical RTE /
  relative-to-center scheme for virtual globes (Cozzi & Ring, *3D Engine Design for Virtual Globes*;
  Cesium's `CesiumOriginShiftComponent`
  [Cesium origin rebasing](https://cesium.com/learn/unreal/unreal-placing-objects/)). Keeps f32 render
  coords small **everywhere**, so both sub-meter ground and orbit are precise simultaneously. Fully
  inside "Rust owns truth" (the rebase is a Rust transform; UE just receives small-coordinate meshes).
- **Confirm depth handling across the range.** UE's reversed-Z float depth already distributes precision
  well; verify no z-fighting at the transition. A **logarithmic depth** vertex transform is the fallback
  Outerra uses to span mm→Mm on a 24-bit buffer if reversed-Z proves insufficient
  ([Outerra log-z](https://outerra.blogspot.com/2009/08/logarithmic-z-buffer.html)) — but origin
  rebasing is the primary lever; reach for log-z only if measured z-fighting demands it.

### S-3 — The near-field detail + collision layer (the walk-around-ground fix)
- **RVT-cached ground detail.** Write the streamed terrain's shading to a Runtime Virtual Texture
  (basecolor / world-space normal / roughness), then blend **our high-detail materials** (detail
  normal/albedo fading in by distance, macro variation, triplanar on slopes, parallax) on top, with
  Pixel-Depth-Offset edge fades — the standard "near detail, far macro" recipe that beats the photoreal
  leaf resolution and is largely free per-pixel at view time
  ([UE RVT](https://dev.epicgames.com/documentation/unreal-engine/runtime-virtual-texturing-in-unreal-engine)).
- **Collision via coarse promotion (ADR-0007).** Promote the near tiles' geometry into a coarse
  deterministic collision representation in Rust (or generate a simplified collision mesh per near tile)
  and feed UE a collidable proxy. Without this, "on foot" is impossible. This is the ADR-0007
  "resolve before vehicles/pathing depend on it" rule made concrete.
- **The seam to Pillar B.** Where an enriched region exists, the near layer renders **our** assets
  (S-3's RVT + the Pillar-B terrain mesh, trees, water, buildings); where it doesn't, it falls back to
  the photoreal mesh + procedural detail. One layer, two content sources, chosen per tile.

### S-4 — Atmosphere & visual coherence across the transition
- Sky Atmosphere with planet `Ground Radius` + froxel aerial perspective; keep AP consistent ground→orbit.
  The "quilt" we hit was **depth-binned AP froxel banding** off the f32 per-tile transform (now fixed);
  if residual banding appears at altitude, the doc-level mitigations are `r.SkyAtmosphere...FastApplyOnOpaque=0`
  (per-pixel AP) and per-pixel transmittance on the sun
  ([UE Sky Atmosphere](https://dev.epicgames.com/documentation/unreal-engine/sky-atmosphere-component-in-unreal-engine)).
  Do **not** fix this by toggling scene lighting (prior guidance) — fix geometry/AP.

**S-series exit:** a debug teleport ground→orbit→ground shows an **instant coarse-complete planet that
sharpens with no hitch**, the near layer is **crisp and collidable** under the player, and precision
holds far from the hero origin. This is the minimum bar for "walk anywhere + travel without stutter,"
and it is achievable **before any new enrichment content** (S-3 ships with procedural detail + photoreal
fallback).

---

## 5. Pillar B — Real-World Enrichment bake (visual-model milestones, **E-series**)

The HE-0..HE-7 plan stands as the per-feature spec; this re-sequences it onto the S-series rails and
upgrades the *models* with the research. New FFI rails needed (the HE gap): an **instance-batch rail**
(`LogiInstanceBatchDraw`: mesh handle + per-instance blob, batched per tile) and a **per-tile texture
channel** (ground/water/building albedo+normal+rough). These are additive ABI bumps.

### E-1 — Terrain visual model: DEM super-res → erosion amplification (the "generate procedural terrain from real terrain")
- **Two-stage amplifier** (deterministic, bakeable): **(a)** deep **DEM super-resolution** (terrain-feature-aware
  CNN, e.g. *TfaSR*, or a detrend-residual net) to lift coarse real DEM toward ~10 m; **(b)** **procedural
  multi-scale hydraulic/thermal erosion seeded by the real DEM** (*Terrain Amplification using Multi-Scale
  Erosion*, SIGGRAPH 2024; Cordonnier et al.) to synthesize sub-meter gullies/scree/rock that **hold up
  at first-person range** — this is the strongest determinism + close-range story in the research.
  ([erosion amplification](https://dl.acm.org/doi/10.1145/3658200)) Add **implicit 3D features** only
  surgically for overhangs/cliffs/arches a heightfield can't express. Diffusion terrain stays a
  hero/texture tool, not the primary conditioner (stochastic, harder to make deterministic).
- Output → the near-layer ground mesh + landcover-conditioned material blend weights (drive S-3's RVT).

### E-2 — Water: extract → our animated water shader
- **Extraction (bake):** OSM/Overture water polygons as prior, refined by a U-Net / SAM+CLIP water mask;
  vectorize → polygons; sample shoreline height from the DEM; classify still/flowing; derive flow vectors
  from DEM D8/D-infinity. Output `{polygon, height, still|flowing, flow_field}`.
- **Render (the user's explicit ask — see §7).** Single Layer Water shading model + Gerstner-WPO +
  quadtree continuous-LOD mesh, fed the per-tile `{height, flow}` as static instance data.

### E-3 — Trees: detect → our wind-reactive assets
- **Detection (bake):** DeepForest (RGB crowns, ~70%) ⊕ PyCrown (LiDAR CHM) fused; coarse type
  (broadleaf/conifer/palm), height, crown radius, confidence. **Deterministic asset substitution:**
  `asset = library[type][hash(tile,idx) % N]`, scaled to measured height. Understory → a density field
  (don't detect individual plants — unsolved).
- **Render (§7):** HISM per species per tile (static transforms at stream-in); **wind entirely in-shader
  from a global wind field** (Pivot Painter 2 / SimpleGrassWind in WPO) — **never** per-frame instance
  data writes (measured 85→45 fps on 20k instances). Impostor last-LOD.

### E-4 — Buildings: footprint + height + photoreal texture → **up-sampled model** (the explicit pipeline)
End-to-end, offline, deterministic-as-possible (pin weights/seed/sampler):
1. **Massing:** footprint (OSM/Overture) + height + LiDAR → **roofer** (3DBAG production engine) or
   **City3D** → watertight **LoD2** mesh with real roof shape. ([roofer](https://github.com/3DBAG/roofer),
   [City3D](https://github.com/tudelft3d/City3D))
2. **Facade parse:** rectified photoreal crop → window/floor/balcony layout (DeepFacade / DeepWindows).
3. **Inverse-procedural facade:** layout → **FacAID** (transformer → editable split-grammar program,
   re-renderable at any resolution) — *keeps the real structure/rhythm*; optional **Pro-DG**
   (diffusion-guided) for appearance edits. ([FacAID](https://arxiv.org/html/2406.01829v2))
4. **Material laundering:** crop → **delight** (MaterIA, strip baked sun/shadow) → **StableMaterials**
   (image→tileable 4K PBR, seamless) → BC7; **clamp basecolor hue to the real crop** so "keep the real
   color" holds. ([StableMaterials](https://gvecchio.com/stablematerials/))
5. **Assemble & address:** execute grammar on the LoD2 massing → clean parametric facade + PBR, keyed by
   content hash, every model/weight/seed in the manifest.
- **Render (§7):** **non-Nanite** proxy + HISM (runtime Nanite encode is editor-only — ADR-0008 is
  correct), interior-cubemap fake interiors for windows, trim-sheet facades, octahedral impostors at
  distance. *Honesty:* full LoD3 from imagery is frontier; expect plausible-not-measured facades.

### E-5 — Editor + override layer (HE-4)
- The ADR-0006 Tile Editor: validated `logi_semantic_edit_*` commands → per-tile override sidecar,
  editor-only, merged idempotently by stable id on re-bake. (Unchanged from the Hero plan §7.)

### E-6 — Planet-scale strategy: **on-demand regional bake**
- The Hero plan baked one AOI. "Anywhere on Earth" needs an **on-demand, region-tiled bake** triggered
  by where the player goes (hero-location-first, then expand along the travel path), with aggressive
  content-hash caching. Cheap deterministic stages (DEM-SR, erosion, segmentation, D8) can run wide;
  expensive stochastic stages (4K material diffusion, facade diffusion) are **region-gated to the
  player's vicinity**. This is a real compute budget (GPU-days+) — model it explicitly. Likely warrants
  an ADR amendment (see §8).

---

## 6. The visual-model stack (reference table, from research stream D)

| Domain | Recommended offline model(s) | Input → output | Deterministic? | Holds at 1st-person? | Ceiling |
|---|---|---|---|---|---|
| Terrain | DEM-SR (TfaSR/detrend) → **multi-scale erosion** (SIGGRAPH'24) → implicit features (surgical) | coarse DEM(+img) → sub-m mesh + material weights | **Yes** (erosion = pure sim) | **Best-in-class** | plausibility-bound (good) |
| Buildings | **roofer**/City3D (massing) → **FacAID**/Pro-DG (facade) → StableMaterials (PBR) | footprint+height+crop → clean LoD2 + PBR | massing yes; diffusion needs pinning | massing/material yes; LoD3 frontier | LoD3 facade is frontier |
| Trees | DeepForest ⊕ PyCrown; coarse-type classifier | RGB+CHM → per-tree table → asset id | **Yes** (single-pass + hash) | via our assets | species fine-grain weak; understory unsolved |
| Water | OSM prior + U-Net/SAM-CLIP mask; D8 flow | imagery/vectors → {poly,height,flow} | **Yes** | n/a (shader does look) | flow plausible not measured |
| Materials | **MaterIA delight → StableMaterials** | photoreal crop → tileable 4K PBR | needs seed/weight pin | yes | SR hallucinates (acceptable) |
| Future runtime | **NVIDIA NTC** (neural texture compression) | offline-encode → tiny GPU decoder | encode offline | — | the one plausible runtime neural lane |

Full citations: research streams D & E (this session). Frontier/avoid-on-critical-path: full LoD3,
understory detection, robust delighting under hard shadows, Gaussian-splat→clean-mesh.

---

## 7. Runtime render-tech choices (from research stream E)

- **Water shader (explicit ask):** **Single Layer Water** shading model (the *material*, not the Water
  plugin's actors — actors fight actorless) on our own quadtree-CLOD meshes; **Gerstner sum in World
  Position Offset** (cheap, deterministic; reserve FFT for a hero ocean band); SW Lumen reflections with
  roughness faked via normal detail (**SLW roughness is ignored under Lumen — forced mirror**, verified
  two sources); depth-based shoreline foam; rivers = nonzero flow vector, lakes = calm, ocean = larger
  amplitude. Per-tile `{height, flow, class}` as **static** custom primitive data.
  ([SLW](https://dev.epicgames.com/documentation/en-us/unreal-engine/single-layer-water-shading-model-in-unreal-engine),
  [Water meshing CLOD](https://dev.epicgames.com/documentation/en-us/unreal-engine/water-meshing-system-and-surface-rendering-in-unreal-engine))
- **Wind vegetation (explicit ask):** one **global wind field** (Material Parameter Collection / flow
  texture) read by every foliage material in WPO; **HISM** per species (static transforms); Nanite only
  for **hero close-up trees as opaque geometry** (Nanite foliage is Experimental in 5.7, net-loss on
  grass cards). The decisive rule: **animate in-shader, never via per-frame instance-data writes.**
- **Ground detail:** RVT (S-3) + distance detail/macro + triplanar + POM on hero surfaces.
- **Buildings:** non-Nanite proxy + HISM + interior-cubemap windows + trim sheets + impostors.
- **Lighting/atmosphere:** **Software Lumen** (cost independent of instance count; HW RT wants <100k
  instances — wrong for a streamed planet); **Virtual Shadow Maps** with WPO-disable-distance + Static
  invalidation on foliage (measured 11 ms→1.4 ms); Sky Atmosphere + Volumetric Clouds (one global
  component, actorless-friendly).
- **Actorless conflicts & workarounds:** Water plugin (use SLW *model* on instanced meshes, not its
  actors); runtime Nanite encode (editor-only → non-Nanite runtime, Nanite reserved for pre-baked
  libraries); per-frame ISM updates (slow → static data + in-shader animation); RVT writers must be
  static (write terrain only, keep wind foliage out). Custom `FPrimitiveSceneProxy` is the bridge for
  Rust-owned geometry (our static-relevance terrain proxy already is one).

---

## 8. Constitution impact — ADRs to write / amend

1. **ADR (new): Layered terrain — far photoreal + near-field detail/enrichment lane.** Establishes the
   §3 two-layer model, the per-tile content-source choice, and the coarse-promoted collision (extends
   ADR-0007/0011). *Load-bearing — write first.*
2. **ADR (amend ADR-0007/0009 or new): Camera-tracking origin rebasing.** S-2 changes the origin from
   static to camera-tracking; document the RTE scheme and the determinism implications (the rebase must
   be deterministic). Also reconcile the **docs-vs-code drift** (comments claim camera-tracking; code
   pins) — a congruence fix.
3. **ADR (amend ADR-0011): On-demand regional / planet-scale bake.** ADR-0011 assumed one offline AOI;
   E-6 makes the bake player-driven and region-tiled, with compute-budget and caching policy.
4. **FFI ABI:** additive bumps for the **instance-batch rail** and **per-tile texture channel** (E-series).

No change dodges the constitution: ML stays offline, runtime stays lean, FFI stays narrow/batched/one-way,
Rust keeps truth, near-field geometry promotes to coarse for gameplay.

---

## 9. Performance budgets (first-person ground, 60 fps = 16.6 ms; ratify vs [Budgets.md](../Performance/Budgets.md))

| Budget | Target | Lever |
|---|---|---|
| Software Lumen (GI+refl+fog) | ≤ 4 ms | SW RT; foliage roughness→0; MaxRoughnessToTrace clamp (~1.8 ms swing) |
| Vegetation raster (Nanite VisBuffer) | < 4 ms | opaque hero trees; non-Nanite grass cards; impostor last-LOD |
| Virtual Shadow Maps | bounded | WPO-disable-distance + Static invalidation (11→1.4 ms measured) |
| Water | cheap vs translucency | SLW pass + quadtree CLOD (verts halve per ring) + flat far mesh |
| Draw calls / memory | HISM ≈ 1 draw/species; ~64 B/instance | instancing + impostors |
| Teleport stutter (p99) | **0 hitch; coarse-complete < 1 frame** | always-resident coarse + destination prefetch + off-thread fs |
| Streaming convergence to full detail | seconds, monotone sharpen | adaptive fetch budget; pinned coarse never regresses |

---

## 10. Milestone roadmap & Definition of Done

**Sequence: S-1 → S-2 → S-3 (substrate, unblocks both requirements) → E-1.. (content).** S-3 ships with
procedural detail + photoreal fallback, so "walk anywhere + travel without stutter" is demonstrable
*before* the ML bake matures.

| # | Deliverable | DoD |
|---|---|---|
| **S-1** | Pinned coarse pyramid + destination prefetch + off-thread decode write + gated O(pool) scans | Debug teleport ground↔orbit shows **instant coarse-complete, no hitch**; profiled; `cargo test`+clippy green |
| **S-2** | Camera-tracking origin rebasing (RTE); depth verified | No jitter/z-fighting far from hero origin (validated at 3 locations + orbit); docs/code congruent; determinism preserved |
| **S-3** | Near-field detail (RVT) + collision-by-promotion + photoreal fallback | Player walks on crisp, collidable ground anywhere; near detail beats leaf resolution; budgets met |
| **S-4** | Atmosphere/depth coherence across transition | No quilt/banding ground→orbit; per-pixel AP only where needed |
| **E-1** | Terrain visual model (DEM-SR → erosion) → near-layer mesh + materials | Enriched terrain renders first-person; deterministic golden+replay; datum ≤5 cm |
| **E-2** | Water extract + SLW/Gerstner/CLOD shader | Water renders at correct height/flow; performant on many tiles |
| **E-3** | Tree detect + wind HISM (global field) | Detected trees as our wind-reactive assets, batched per tile; instance budget met |
| **E-4** | Building up-sample (massing→facade→PBR) | Buildings keep real layout/color, crisp facades + materials; non-Nanite; draw budget met |
| **E-5** | Tile Editor + override merge | Artist edit survives re-bake (property tests); editor-only symbols absent from Shipping |
| **E-6** | On-demand regional bake | Player travels into an unbaked region → bake triggers/caches; planet-scale path proven |
| **E-7** | Performance pass | §9 budgets ratified and met on reference GPU |

Each milestone obeys the Hero plan §8 standards and §9 CI gates (forbid-unsafe outside the FFI seam;
no `unwrap`/`panic` in runtime; batched-per-tile FFI; byte-reproducible bake; ABI static-asserts;
no enrichment render data treated as gameplay authority without coarse promotion).

---

## 11. Risks

| # | Risk | Mitigation |
|---|---|---|
| 1 | Origin rebasing introduces a per-rebase frame cost / determinism subtlety | snap threshold (~2 km) so rebases are rare; deterministic transform; test orbit↔ground↔orbit |
| 2 | Near-field + photoreal **seam** (color/height mismatch at the layer boundary) | blend zone; promote photoreal height as the base the near layer detail-displaces; RVT shared shading |
| 3 | Collision-by-promotion cost / accuracy on foot | coarse simplified mesh per near tile; budget; ADR-0007 promotion rule |
| 4 | Bake determinism for diffusion stages (materials, facades) | pin weights+sampler+seed in a bake container; treat the **baked artifact** (content-addressed) as truth, not the model |
| 5 | Planet-scale bake compute (GPU-days+) | region-gate expensive stochastic stages to the player's vicinity; run cheap deterministic stages wide; cache by hash |
| 6 | ML accuracy (trees ~70%, LoD3 frontier) | confidence + per-class gates; Tile Editor absorbs cleanup; coarse type not fine species |
| 7 | UE5 atmosphere/Lumen/VSM pitfalls at planet scale | SW Lumen; VSM WPO-disable + Static invalidation; per-pixel AP only where needed |
| 8 | Photoreal licensing for shipped enriched output | provenance ledger (ADR-0011); open-data subset carve; Google as on-screen reference only |

---

## 12. Immediate next steps

1. **Write ADR #1 (layered terrain — far photoreal + near-field detail/collision lane)** — it gates the
   whole plan's shape.
2. **Land S-1** (pinned coarse + destination prefetch + off-thread decode write) — sub-week, pure Rust,
   immediately demonstrable on the debug teleport, no new content needed.
3. **Land S-2** (camera-tracking origin rebasing) — fixes the docs/code drift and unblocks walk-anywhere
   precision.
4. **Prototype S-3** (RVT detail + a collision proxy) on the existing photoreal stream — proves the
   near-field win before the ML bake.
5. In parallel, **spike E-1** (DEM-SR → erosion) on the hero AOI as the first visual model, feeding S-3.

---

## 13. Source pointers

- This session's research streams (A enrichment-state, B streaming-fitness, C ground↔orbit, D content
  models, E render systems) — findings cited inline.
- Internal: [Hero-Location plan](Hero-Location-Semantic-Enrichment.md) ·
  [Milestone-F](Milestone-F-3DTiles-Rust-Stack.md) · [ADR-0007](../Architecture/ADR-0007-Actorless-Rust-Terrain.md) ·
  [ADR-0008](../Architecture/ADR-0008-Terrain-Nanite-Lane.md) · [ADR-0009](../Architecture/ADR-0009-Native-Rust-3DTiles-Stack.md) ·
  [ADR-0011](../Architecture/ADR-0011-Semantic-Tile-Enrichment.md) ·
  [ECS-Round4 research](../Research/2026-06-09-ECS-Round4-Pipeline-Fork-GPU-Research.md).
- External (load-bearing): [cesium-native selection algorithm](https://cesium.com/learn/cesium-native/ref-doc/selection-algorithm-details.html) ·
  [Cesium origin rebasing](https://cesium.com/learn/unreal/unreal-placing-objects/) ·
  [Outerra log-z](https://outerra.blogspot.com/2009/08/logarithmic-z-buffer.html) ·
  [UE Single Layer Water](https://dev.epicgames.com/documentation/en-us/unreal-engine/single-layer-water-shading-model-in-unreal-engine) ·
  [UE Runtime Virtual Texturing](https://dev.epicgames.com/documentation/unreal-engine/runtime-virtual-texturing-in-unreal-engine) ·
  [UE Sky Atmosphere](https://dev.epicgames.com/documentation/unreal-engine/sky-atmosphere-component-in-unreal-engine) ·
  [Terrain Amplification w/ Multi-Scale Erosion (SIGGRAPH'24)](https://dl.acm.org/doi/10.1145/3658200) ·
  [roofer](https://github.com/3DBAG/roofer) · [FacAID](https://arxiv.org/html/2406.01829v2) ·
  [StableMaterials](https://gvecchio.com/stablematerials/) · [NVIDIA NTC](https://research.nvidia.com/labs/rtr/neural_texture_compression/).
