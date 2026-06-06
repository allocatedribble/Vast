# Milestone F — Native Rust 3D Tiles & Real-World Terrain Stack

> **2026-06-05 — renamed to the Geo engine.** The terrain / real-world-earth stack is now `Rust/crates/geo/` (`logi_geo_*` plus the 3D-Tiles / geospatial / semantic crates), cdylib `logi_geo.dll`, C++ `ULogiBakedGeoSubsystem`. The `logi_terrain*` / `LogiTerrain*` / `logi_terrain.dll` names below are historical (pre-rename); see [AGENTS.md](../../AGENTS.md) §4.

**Status:** Plan (2026-06-04). Seeds **ADR-0009** (decision + consequences).
**Author context:** synthesised from a 5-agent research pass (quantized-mesh format, cesium-native
architecture, UE 5.8 MeshTerrainMode, glTF→Unreal render shim + KML/COLLADA, cost/pricing) plus a
codebase audit of `logi_terrain_*` and inspection of the `AGI_HQ.kmz` sample.

---

## 1. Decision & the load-bearing finding

**Decision:** build a **native Rust crate set that reimplements the load-bearing parts of
[cesium-native](https://github.com/CesiumGS/cesium-native)** (3D Tiles streaming, tile selection,
glTF / quantized-mesh decode) — *not* linking the C++ cesium-native library, and *not* using the
actor-based Cesium-for-Unreal plugin. A large rewrite of the synthetic-terrain stack is accepted; no
backward-compatibility is preserved.

**Why this is tractable (the linchpin):** LogiUE **already owns Rust ports of cesium-native's hardest
runtime pieces.** A module-by-module comparison:

| cesium-native runtime piece | LogiUE today | Reuse |
|---|---|---|
| Screen-space-error LOD | `logi_terrain_lod::screen_space_error` — **byte-for-byte the same formula** | as-is |
| Frustum culling (6 inward planes) | `logi_terrain_stream::culling::Frustum` | extend to box/region |
| Horizon / globe occlusion | `culling::HorizonCuller` — **Cesium's exact scaled-space test, verbatim** | as-is |
| Byte-budgeted LRU residency (by last-used tick) | `logi_terrain_cache` | as-is |
| Transparent priority scheduler + queue classes | `logi_terrain_stream::scheduler` | extend one term |
| Async fetch/pump contract | `Provider { request / poll_completed / in_flight }` | generalize |
| Zero-copy per-tile frame packet over C ABI | `logi_terrain_scene` (`LogiFramePacket`/`LogiBufferView`) | keep layout |
| f64→f32 floating-origin rebase | `REBASE_SNAP` (2048 m) | feed it ECEF |

So this is a **generalisation-and-decode project**, not a rewrite from zero. The genuinely new work is:
**(a)** replace the *synthetic regular-quadtree world* with a real *tileset BVH + implicit tiling*;
**(b)** generalise the regular-grid traversal into cesium-native's pointer-tree traversal
(kick / skip-LOD / ancestor-fallback); **(c)** add real content decode (glTF + Draco + KTX2 + meshopt,
and quantized-mesh terrain); **(d)** add the one render-side gap — a **per-tile texture path**.

This preserves the constitution: **Rust owns terrain truth; Unreal renders; actorless; narrow C ABI.**
The selection/streaming that cesium-native would have owned in C++ now lives in Rust, on the kernel's
single owner thread.

---

## 2. Scope & non-goals

**In scope (v1):** tileset.json + implicit tiling; the selection traversal; glTF (incl. Draco/KTX2/
meshopt) + quantized-mesh decode; ECEF/geodetic/ENU geospatial; a sync IO + worker-pool model; the
per-tile texture render path; the `AGI_HQ.kmz` validation target.

**In scope (v1) — added by the 2026-06-04 decisions:** a runtime **COLLADA 1.4.1 loader**
(`logi_collada`, for the AGI_HQ sample) and a **minimal Cesium-ion asset-endpoint resolver**
(`logi_3dtiles_ion` — ion is the chosen data source).

**Out of scope (defer):** raster/imagery overlays, GeoJSON/vector, **full** ion OAuth / iTwin auth
clients (only the minimal asset-endpoint resolver is in v1), S2-cell bounding volumes, ADD-refinement
edge cases, multi-tileset weighted round-robin. (All are additive on the v1 trait surface.)

**Rejected — UE 5.8 "Mesh Terrain Mode" as a render backend.** Source-verified: it is an **editor-only
authoring mode** (`MeshTerrainMode` is an `"Type": "Editor"` module over the `MeshPartition` "MegaMesh"
data model). Its pipeline is **author → editor-compile → cook-time bake → stream pre-cooked
`ACompiledSection` actors via World Partition**. There is **no runtime ingest API** (the compiler lives
in the editor package; a forum request for a runtime-modification API got no confirmation). It is
actor-heavy + bake-time — the opposite of our runtime-streamed actorless kernel — and hits the *same*
stock-5.8 wall ADR-0008 documents (no runtime Nanite encode). **Keep our proc-mesh + Nanite lanes.**
*Borrow three ideas only:* per-section **far-field/impostor mesh** for distance LOD; the
**channel/weight-layer splat material** (Rust emits per-vertex weights → Unreal rasterises one channel
texture per tile — cleaner at runtime than their bake); and the lean custom `FMeshData` mesh format
(validates "don't always route through `FMeshDescription`").

**Foliage** is tracked separately (Milestone G / ADR-0010) — a deterministic per-tile seeded scatter;
see the prior Cesium research synthesis.

---

## 3. Crate architecture

```
NEW / GENERALISED ───────────────────────────────────────────────────────────────────────
logi_geospatial        WGS84 ellipsoid, ECEF↔geodetic, ENU, bounding-volume distance     (extends logi_terrain_geo; ~200 LOC on glam)
logi_qmesh             quantized-mesh-1.0 terrain decode (leaf codec, no FFI, no unsafe)  (Cesium World Terrain path)
logi_3dtiles_tileset   tileset.json model + serde + external-tileset graft               (thin)
logi_3dtiles_implicit  subtree binary decode, Morton/availability bitstreams             (the fiddly indexing)
logi_gltf              `gltf` crate + Draco(FFI) + KTX2/Basis + meshopt + interleave      (replaces logi_terrain_decode)
logi_collada           COLLADA 1.4.1 (`dae-parser`) → 32-byte vertex + texture           (runtime KMZ/DAE; AGI_HQ + Google-Earth assets)
logi_3dtiles_io        TileContentSource trait + ureq/gzip/disk-cache + worker pool       (generalises Provider)
logi_3dtiles_ion       minimal Cesium-ion asset-endpoint resolver (url + token)          (the chosen data source; full OAuth deferred)
logi_3dtiles_select    pointer-tree traversal (visitTile/kick/ancestorMeetsSse), cull     (generalises logi_terrain_stream)
logi_3dtiles_scene     LogiTileWorld + frame-packet assembly + buffer_view + texture rail (rewrites logi_terrain_scene; SAME repr(C) FFI)

REUSED UNCHANGED (the proven core) ───────────────────────────────────────────────────────
logi_terrain_lod       SSE math
logi_terrain_stream::culling     frustum + Cesium horizon
logi_terrain_stream::scheduler   priority terms + queue classes
logi_terrain_cache     byte-budgeted LRU
logi_terrain_core      glam (DVec3/DMat4), telemetry  (TileId → general TileKey{tileset,level,x,y,z})

RETIRED → behind a `synthetic` feature for deterministic tests / offline-no-network dev ────
logi_terrain_procgen   synthetic fractal height field
logi_terrain_decode    flat-grid displacer  (superseded by logi_gltf + logi_qmesh)
logi_terrain_pack::height   synthetic height pages
```

**The C++ renderer's *geometry* path is unchanged.** Because `logi_3dtiles_scene` keeps the exact
static-assert'd `#[repr(C)]` packet (`LogiFramePacket` == 200 B, the zero-copy `LogiBufferView`, the
floating-origin `LogiLocalFrame`), `LogiTerrainSubsystem.cpp`'s proc-mesh + Nanite lanes consume real
glTF/quantized-mesh geometry with no rework — they already take generic interleaved 32-byte vertices
(`pos f3 | normal f3 | uv f2`) + u32 indices.

---

## 4. The render side — geometry reused, texture path is the one new piece

LogiUE has **never built a runtime texture path**; that is the single render-side gap between "terrain
works" and "render a textured building / photoreal tile."

- **FFI:** add a per-tile albedo channel. The packet already carries a `material_page_uploads`
  (`LogiUploadCommand{ page_handle, data*, byte_length, slot, kind }`) array that is **currently
  null/unused** — its *shape* can carry decoded pixels. Either repurpose it or add an explicit
  `LogiTextureView { data*, width, height, format, mip_count }`, plus a per-draw material→texture
  binding (today `LogiTerrainPatchDraw.material_id` is set to 0 and never read by C++). **ABI bump** (the
  88-/200-byte size asserts lock the layout in 3 places; +1 version).
- **Decode (Rust):** transcode glTF `baseColorTexture` (KTX2/Basis → **BC7**, or pass JPEG/PNG-decoded
  RGBA8) into a texture blob alongside the vertex/index buffers; quantized-mesh terrain stays
  single-material (no texture).
- **C++ (new):** `UTexture2D::CreateTransient` (or the async `FTexturePlatformData` mip path for big
  textures) → lock/memcpy/unlock → `UpdateResource`; `UMaterialInstanceDynamic::Create` +
  `SetTextureParameterValue("BaseColor", Tex)` → `SetMaterial(0, MID)`. **Gotchas:** `SRGB=true` for
  albedo; BGRA-vs-RGBA byte order; **GC-root textures + MIDs with `TStrongObjectPtr`** (same discipline
  the Nanite mesh cache already uses — the #1 runtime-texture footgun); `CalculateExtendedBounds`.
- **Nanite caveat (ADR-0008):** stock-5.8 Nanite encode is editor-only. For a runtime *textured* mesh
  the pragmatic path is the direct `FStaticMeshRenderData` / `SetRenderData` lane (**non-Nanite**);
  Nanite stays an editor/PIE bonus. Photoreal/photogrammetry tiles therefore ride the proc-mesh lane;
  reserve Nanite for the single-material heightfield terrain.

---

## 5. Cross-cutting decisions

- **Draco = the one FFI exception.** No usable pure-Rust Draco *decoder* exists (`draco-oxide` is
  encoder-only; `draco_decoder` is itself a C++ FFI). Vendor a thin **`cc`-compiled Draco C++ decode
  shim inside `logi_gltf`** — single function `decode_draco(bytes, attrs) -> {pos,nrm,uv,idx}`,
  decode-only, plain-data in/out, on Rust worker threads, statically linked into the existing cdylib.
  It does **not** violate the FFI law (no Unreal types cross; plain data). Document it as an explicit,
  revisitable exception in ADR-0009. (`basis-universal` and `meshopt` are also C++-backed but are
  standard accepted Rust crates — not special cases.)
- **IO model: sync core + worker pool, NO tokio/async.** The kernel is single-owner-thread; the
  existing `Provider` trait is already the right shape. Generalise to a `TileContentSource`; selection
  + integration + LRU run on the owner thread; a small `std::thread`/`crossbeam` pool does
  fetch→gunzip→decode→interleave and pushes owned blobs back through a channel (the
  `thenInWorkerThread → thenInMainThread` split without futures). HTTP = **`ureq`** (sync, no tokio) +
  `flate2`; content-addressed on-disk cache under `Saved/`; `std::fs` for `file://` tilesets.
- **Geospatial = glam + ~200 LOC.** WGS84 ellipsoid, `cartographicToCartesian` / `scaleToGeodeticSurface`
  Newton iteration, ENU `LocalHorizontalCoordinateSystem` — all closed-form from cesium-native's
  `Ellipsoid.cpp`. Do **not** add `nalgebra` (would break the glam-everywhere FFI byte-identity).
  `geographiclib-rs`/`map_3d` are test-only cross-checks. Reuse `REBASE_SNAP` (ECEF coords *must* go
  through it or f32 vertices shatter at globe scale).
- **Determinism preserved.** Keep `SyntheticProvider` (derives latency from the tile id) as the
  deterministic replay/test source behind the `TileContentSource` trait; the live `ureq` provider is
  production-only. The selection's anti-thrash hysteresis (`prev_refined`, `SSE_HYSTERESIS=0.25`) — a
  LogiUE refinement cesium-native lacks — is kept; it protects the C++ Nanite re-encode lane.

---

## 6. Milestone roadmap

**M0 — Foundation (geospatial + the texture rail).**
`logi_geospatial` (ellipsoid/ECEF/ENU). Add the FFI texture channel + per-draw material binding (ABI
bump). Build the C++ runtime `UTexture2D` + `UMaterialInstanceDynamic` textured-material path with
`TStrongObjectPtr` GC rooting. *Exit:* a hand-fed textured quad renders + is GC-safe.

**M1 — `AGI_HQ` building (first pixels; proves the textured shim + geodetic placement, no streaming).**
`logi_collada` (runtime `.dae` loader on `dae-parser`: resolve `<source>` POSITION/NORMAL/TEXCOORD +
de-index `<triangles>`/`<polylist>`, honour `<up_axis>Z_UP` + `<unit>`) → the 32-byte vertex contract;
`image` crate decodes the JPEG atlas → RGBA8. Parse `doc.kml` (quick-xml) for the geodetic placement
(lon −75.597°, lat 40.039°, alt 50 m, heading 90°). Decode in Rust → interleaved verts + albedo → the
new texture channel; C++ creates the textured `SetRenderData` mesh and places it via
geodetic→ECEF→floating-origin rebase (position the component, not the vertices). *Exit:* the AGI HQ
building renders, textured + correctly placed, in the editor viewport. **This is the keystone — it
forces exactly the texture + placement work photoreal tiles will need, with one static asset.**
(`logi_gltf` lands in M4 for the streamed-tile content path.)

**M2 — Quantized-mesh terrain (real-world terrain; reuses the heightfield-adjacent pipeline).**
`logi_qmesh` (the byte-exact decoder). `logi_3dtiles_io` first cut (`ureq` + gzip + `layer.json`
discovery + `available` walk) + `logi_3dtiles_ion` (resolve **Cesium World Terrain**, asset 1, →
url + access token; thread the token onto tile requests + the attribution into the packet's
`attributions[]`). **Plus the globe coordinate path (§9.4):** camera-tracking ECEF RTC origin, the globe
camera (ECEF + altitude-adaptive), and the `FSceneViewExtension` reversed-Z adaptive-near depth. Most of
the streaming/scene/cache machinery is reused. *Exit:* Cesium-ion-streamed real-world terrain streams +
renders LOD-correct **with smooth orbital→street zoom** through the actorless kernel.

**M3 — Tileset traversal generalisation (the hard core; enables arbitrary 3D Tiles).**
`logi_3dtiles_tileset` + `logi_3dtiles_implicit`. `logi_3dtiles_select`: port `visitTile` —
per-volume distance, SSE test (reuse), REPLACE refine, **kick / skip-LOD** (render the coarse ancestor
while fine descendants stream), `ancestorMeetsSse` propagation, view-aligned load priority. Reuse
SSE/cull/cache/scheduler. Gate to REPLACE-refine quadtree tilesets first. *Exit:* an arbitrary 3D Tiles
tileset selects + streams correctly against recorded camera paths.

**M4 — Textured glTF tiles + compression (photoreal / photogrammetry).**
`logi_gltf`: add the **Draco** vendored-FFI shim + **KTX2/Basis** transcode + **meshopt**. The per-tile
texture path (M0) carries the baked albedo. *Exit:* Google photoreal / photogrammetry tilesets render
on the proc-mesh lane.

**M5 — Polish.** ADD-refinement; multi-tileset weighted round-robin; ion/iTwin auth; raster overlays;
far-field impostor LOD tier (MeshTerrainMode-borrowed); optional channel-splat multi-biome material.

**(M-G, parallel) — Foliage** per the prior synthesis (deterministic per-tile scatter → HISM/imposter
ship lane + Nanite editor lane). Independent of the tileset work.

---

## 7. Risk register

| # | Risk | Sev | Mitigation |
|---|---|---|---|
| 1 | Traversal **kick / `ancestorMeetsSse` / last-frame-render-state** fidelity — the 20% that causes pop-in/holes | **High** | Port `TileSelectionState`/`TraversalDetails` faithfully; golden-test recorded camera paths; existing hysteresis + parent-fallback are a head start |
| 2 | Draco C++ FFI build on win64/MSVC + cdylib | Med | `cc` build.rs, static-link, feature-gated; well-trodden vendoring |
| 3 | Implicit sparse-metadata **popcount/Morton** indexing edge cases | Med | The 3D-Tiles spec ships conformance fixtures; unit-test against them |
| 4 | Texture memory at scale (4096 tiles × BC7) | Med | Per-tile texture LRU + byte budget mirroring `evict_meshes`; atlas/bindless later |
| 5 | f64→f32 globe-scale precision | Low | Already solved — route ECEF through `REBASE_SNAP` |
| 6 | Determinism vs real network | Low | Keep `SyntheticProvider` as the deterministic source behind the trait |

---

## 8. Resolved decisions (2026-06-04)

1. **Draco** — ✅ **Accept the vendored C++ decode shim** (decode-only, plain-data, worker-thread,
   statically linked in `logi_gltf`). Documented exception in ADR-0009. Unblocks Draco-compressed
   photoreal/photogrammetry tiles.
2. **Synthetic terrain** — ✅ **Keep behind a `synthetic` cargo feature** (deterministic tests +
   offline/no-network dev + procedural non-georeferenced worlds). Not deleted.
3. **AGI_HQ COLLADA** — ✅ **Runtime COLLADA loader** (`logi_collada` on `dae-parser`, COLLADA 1.4.1) —
   NOT an offline bake. KMZ/DAE assets load at runtime; manual `<source>` resolve +
   `<triangles>`/`<polylist>` de-index into the 32-byte vertex contract.
4. **Data source** — ✅ **Stream from Cesium ion** (curated global terrain + OSM Buildings). Pulls a
   **minimal ion asset-endpoint resolver** (`logi_3dtiles_ion`: POST `/v1/assets/{id}/endpoint` →
   `{url, accessToken, attributions}`) forward into M2; full OAuth/iTwin auth stays deferred. (Cost: ion
   is a flat SaaS subscription; Google photoreal, if later added, is the only per-session metered,
   non-cacheable cost.)

---

## 9. Detailed design (implementation-ready)

### 9.1 Cargo workspace & build order

New crates under `Rust/crates/`, in dependency order (implement top-down):

1. `logi_terrain_core` — **extend `TileId` → `TileKey { tileset:u16, level:u8, x:u32, y:u32, z:u32 }`**
   (adds a tileset id + the octree `z`). This ripples through every map keyed on tile id — **do it
   first.** Keep glam + telemetry; `key()`/`from_key()` widen (likely a `u128` or struct key).
2. `logi_terrain_geo` (existing) — the shared 32-byte vertex contract; reused by all decoders.
3. `logi_geospatial` — WGS84 ellipsoid, geodetic↔ECEF, ENU `LocalHorizontalCoordinateSystem`. (→ core, glam.)
4. `logi_qmesh`, `logi_collada`, `logi_gltf` — content decoders (→ geo, geospatial; `logi_gltf` also →
   the Draco shim, `ktx2`, `basis-universal`, `meshopt`).
5. `logi_3dtiles_tileset` (serde) → `logi_3dtiles_implicit` (subtree binary + Morton/availability).
6. `logi_3dtiles_io` (`TileContentSource` + `ureq`/gzip/disk-cache + worker pool) → `logi_3dtiles_ion`.
7. `logi_3dtiles_select` — the traversal (→ tileset, implicit, geospatial, **+ reuses `logi_terrain_lod`,
   `logi_terrain_cache`, `logi_terrain_stream::{culling,scheduler}` UNCHANGED**).
8. `logi_3dtiles_scene` — `LogiTileWorld` + frame-packet assembly (rewrites `logi_terrain_scene`; keeps
   the `#[repr(C)]` packet contract).
9. `logi_terrain_ffi` (existing) — repointed at `logi_3dtiles_scene`; **keeps the cdylib name
   `logi_terrain.dll`** → the C++ `LogiBevyRuntime.Build.cs` linkage is unchanged.

**Feature flags:** `synthetic` (the retired procgen lane, on for tests/offline), `ion`, `draco` (gate the
C++ shim so a no-Draco build still compiles). `cargo test --workspace` runs `synthetic` for determinism.

### 9.2 The selection traversal (the M3 core — port faithfully)

Port cesium-native's `visitTile`, persisting **per-tile state across frames** (not just residency):

```rust
struct TileSelectionState { last_frame: u64, last_result: SelectionResult } // Rendered/Refined/Culled/Kicked
struct TraversalDetails   { all_renderable: bool, any_rendered_last_frame: bool, not_yet_renderable: u32 }
```

`visit(tile, frame) -> TraversalDetails`:
1. **Distance + cull.** `dist = bounding_volume.distance_to_surface(camera)` (region/box/sphere). Frustum
   + horizon cull (**reuse `culling.rs`**). Record a `culled` flag.
2. **SSE.** `sse = screen_space_error(tile.geometric_error, dist, fov, vph)` (**reuse `logi_terrain_lod`**).
   Apply the **hysteresis** (`prev_refined`, `SSE_HYSTERESIS=0.25`) — keep it (cesium-native lacks it; it
   protects the Nanite re-encode lane).
3. **Meets-SSE?** `meets = culled ? sse < culled_sse_target : sse < sse_target`. If `meets` and not
   force-refined → **render this tile**, return `{ tile.renderable, was_rendered, 0 }`.
4. **Else REFINE.** `visit_children_near_to_far` (nearest first → nearest detail loads first); fold each
   child's `TraversalDetails`.
5. **Kick** (the subtle bit): `kick = !all_renderable && !any_rendered_last_frame`;
   `will_kick = kick && (not_yet_renderable > loading_descendant_limit || tile.renderable)`. If `will_kick`
   → **render THIS tile (coarse fallback) while children keep loading** and drop the children from this
   frame's render set. Else children render (REPLACE) — or, for ADD refine, tile + children both render.
6. **`ancestor_meets_sse` propagation.** A refined tile that must keep refining (had renderable
   descendants last frame) and `!ancestor_meets_sse` → enqueue *its own* load at **Urgent** and set
   `ancestor_meets_sse=true` so deeper tiles skip the redundant Normal load.
7. **Load priority** (lower = more urgent): `(1 - dot(normalize(tile_center - cam_pos), cam_dir)) * dist`.
   Maps onto the existing `PriorityTerms` (already has the velocity-prefetch dot) — add the view-dir term.

Reuse: SSE, culling, LRU cache, scheduler queues/budgets, hysteresis. New: `TileSelectionState`,
`TraversalDetails`, the kick / `ancestor_meets_sse` flow, per-volume distance. This is **risk #1** —
golden-test it (9.5).

### 9.3 FFI / ABI delta (version 3 → 4)

The geometry packet is otherwise unchanged (the C++ geometry path is untouched). Additions:
- **`LogiTerrainPatchDraw`** gains `albedo_texture: u64` (0 = untextured terrain; nonzero = a texture
  handle). Repurpose the `_reserved` slot or widen the struct (updates the 88-byte size assert).
- **`LogiFramePacket`** gains a per-frame texture-upload array: `texture_uploads: *const LogiTextureUpload`
  + `texture_upload_count: u32`, where `LogiTextureUpload { handle:u64, data:*const u8, byte_length:u64,
  width:u32, height:u32, format:u32 /*RGBA8=0,BC7=1,BC1=2*/, mip_count:u32 }`. Updates the 200-byte assert.
- **Attribution:** wire the already-present `attributions[]` from ion's endpoint response + each tile's
  `asset.copyright` (ion/Google **require** on-screen display).
- **Camera/view (globe):** `set_camera` carries **ECEF f64** position + the ENU basis; the packet's view
  struct gains `altitude_above_ellipsoid: f64` + the Rust-recommended `near: f32` / `far: f32` (computed
  from ECEF + the ellipsoid). The C++ `FSceneViewExtension` stamps the projection from these (§9.4).
- Bump `LOGI_TERRAIN_ABI_VERSION` 3→4 in the Rust const, the C header `#define`, and re-check both
  `size_of` static asserts. The C++ load-time version guard refuses a mismatched DLL.

### 9.4 Globe coordinate, precision & depth pipeline (resolves ADR-0009 #1 — smooth orbital→street zoom)

The world IS the WGS84 ellipsoid in **ECEF f64**; there is no flat plane. Continuous orbital→street zoom
is a solved pattern (Cesium-for-Unreal / Outerra) that reuses what LogiUE already has.

```
tile content local (glTF Y-up tile-local / qmesh)
 → [Rust] tile transform chain (f64) + geodetic→ECEF         → tile_center_ECEF f64 (right-handed)
 → [Rust] O = snap(camera_ECEF)   [REBASE_SNAP, camera-tracking]
 → [Rust] (tile_center_ECEF − O) in f64  + tile ENU rotation → camera-relative f64
 → [Rust] cast to f32                                        → render-local f32  ← crosses the C ABI
 → [C++]  Y↔Z swap + ×100 (existing reflection)              → Unreal cm (Z-up, left-handed)
```

- **Precision = camera-tracking RTC (the authority).** Render origin `O` tracks the camera in ECEF (your
  `REBASE_SNAP`, fed ECEF). Vertices stay tile-local f32 (the Fix-1 tile-local meshes ARE RTC); the per-tile
  transform carries `tile_center_ECEF − O` (f64→f32). Precision scales with camera-relative distance →
  ~0.1 mm at street, sub-pixel at orbital. **UE5 LWC is a complementary safety net, NOT a substitute** —
  never render raw ECEF (Cesium keeps origin rebasing *on top of* LWC for the same reason). The large
  magnitude dies in the **f64 subtract before the f32 cast** — the rule the kernel already follows.
- **Geometry = TRUE GLOBE (falls out of M3).** Tiles are curved ECEF patches (the 3D-Tiles ellipsoidal
  tiling = the M3 tileset BVH); the SSE traversal, frustum cull, and the **already-ellipsoid-capable
  horizon culler** (`culling.rs` has Cesium's scaled-space ellipsoid-occlusion test, verbatim) all reason
  in ECEF. The flat-square `World` retires to the `synthetic` feature. So the globe's "geometry half" is
  M3 + the camera-tracking-ECEF origin — **not** a separate moving-tangent-frame effort.
- **Depth = UE 5.8 native reversed-Z float buffer + a per-frame altitude-adaptive NEAR plane.** **NOT**
  logarithmic depth (needs pixel-shader `SV_Depth` writes → disables Hi-Z/early-Z and **breaks the Nanite
  lane (ADR-0008)**; also impossible in stock 5.8 — Pixel Depth Offset is positive-only), **NOT**
  multi-frustum. With reversed-Z float the **far plane is precision-irrelevant**; near is the only knob and
  the *visible* range is bounded at any altitude: `near = clamp(k · altitude, 0.1 m, cap)` (k≈1/500–1/2000;
  clamp down if a near object is in frame), `far = horizon(altitude)` (generous/infinite-far), hysteresis
  on `near`. Set it **actorlessly** via an `FSceneViewExtensionBase` owned by the terrain `UWorldSubsystem`,
  overriding `SetupViewProjectionMatrix` per-frame from the packet's recommended `near`/`far`. One camera,
  one frustum, one depth buffer.
- **Camera = altitude-adaptive globe pawn** (intent shell, ADR-0005 — cf. Cesium `GlobeAwareDefaultPawn`/
  `CesiumFlyToComponent`): Rust returns the ENU `up`, the rebase origin, and recommended `near`/`far` in the
  frame packet; movement speed + rebase cadence scale with altitude.
- **Handedness/winding:** ECEF is right-handed; the C++ Y↔Z swap is a negative-determinant reflection into
  Unreal's front-face convention — **keep the kernel winding**. Validate against AGI_HQ + Cesium terrain.
- **Cross-scale gotchas:** drive **Sky Atmosphere**'s planet center from the same Rust ECEF origin each
  frame (else the sky swims on rebase); **scale VSM/shadow range with altitude** (empirical, no one-shot
  recipe); the reversed-Z + adaptive-near choice **preserves Nanite compatibility** (log-depth would not).
- **Gameplay authority** generalizes to **ECEF f64 / geodetic** for global agents (orbital flows) — coupled
  but separable; the render can go globe before `logi_sim` does (a regional sim maps into ECEF).

### 9.5 Test & validation strategy

- **Per-crate headless unit tests:** `logi_qmesh` decodes the `tile-opentin.terrain` fixture + round-trips
  zigzag/high-watermark/oct-decode; `logi_geospatial` checks geodetic↔ECEF vs known WGS84 points + ENU
  orthonormality; `logi_collada` loads `AGI_HQ.dae` (asserts vert/tri counts + the texture ref);
  `logi_3dtiles_implicit` checks Morton/availability vs the 3D-Tiles spec conformance fixtures.
- **Determinism preserved:** `SyntheticProvider` stays the deterministic source behind `TileContentSource`;
  the **262,144-tile streaming-stress test generalizes** (bounded memory + eviction). `cargo test
  --workspace` exit-0 stays the gate.
- **Golden traversal tests:** record a camera path (`ViewState`s) → assert the selected `TileKey` set per
  frame matches a checked-in baseline. The regression net for the kick / `ancestor_meets_sse` logic.
- **Editor visual validation (the user's):** M1 building renders; M2 terrain streams; M4 photoreal renders.
  Screenshots are disabled for the agent — the user confirms.

### 9.6 C++ render-side changes (minimal — the texture path is the only substantive one)

- **`LogiTerrainSubsystem`:** for a draw with `albedo_texture != 0`, create a `UTexture2D` from the matching
  `LogiTextureUpload` (`CreateTransient` + lock/memcpy/`UpdateResource`, or the async mip path for BC7),
  cache it (`TMap<u64, FLogiTextureEntry>` with `TStrongObjectPtr<UTexture2D>` — mirror the Nanite mesh
  cache's GC discipline), and assign a per-draw `UMaterialInstanceDynamic`
  (`SetTextureParameterValue("BaseColor", Tex)` → `SetMaterial`). Add a **texture byte-budget LRU** paralleling
  `evict_meshes`.
- **Lane choice:** textured tiles (photoreal/building) use the direct `SetRenderData` (non-Nanite) lane;
  single-material terrain keeps the proc-mesh + editor-Nanite lanes (ADR-0008).
- **Dev settings:** add `bStreamCesiumTerrain`, `CesiumIonToken`, `CesiumWorldTerrainAssetId` (default 1),
  and a `bLoadSampleKmlModel` flag for the AGI_HQ validation.
- **Build.cs:** no new C++ deps (uses existing `Engine`/`RenderCore`); the cdylib stays `logi_terrain.dll`,
  so the existing build/stage steps apply.

### 9.7 Milestone sizing & acceptance

| M | New crates / work | Effort | Acceptance |
|---|---|---|---|
| M0 | `logi_geospatial`; FFI texture channel (ABI 3→4); C++ `UTexture2D`/MID path + GC + LRU | **M** (~1–2 wk) | hand-fed textured quad renders + survives a GC; geospatial tests green |
| M1 | `logi_collada` (+`image`, `kml`/quick-xml); texture decode→upload wiring | **M** (~1–2 wk) | AGI HQ building renders textured + geodetically placed in-editor |
| M2 | `logi_qmesh`; `logi_3dtiles_io` cut; `logi_3dtiles_ion` | **M–L** (~2–3 wk) | Cesium-ion World Terrain streams + renders LOD-correct; stress test green |
| M3 | `logi_3dtiles_tileset`, `_implicit`, `_select` (traversal port) | **L–XL** (~3–5 wk) | a REPLACE-quadtree tileset selects correctly vs golden camera paths |
| M4 | `logi_gltf` (+ Draco shim, KTX2/Basis, meshopt) | **L** (~2–4 wk) | a photoreal/photogrammetry tileset renders on the proc-mesh lane |
| M5 | ADD-refine, multi-tileset round-robin, overlays, impostor LOD, splat | **L** (ongoing) | per-feature |

*(Order-of-magnitude for one engineer; M3 is the critical path.)*

## 10. Source pointers

- quantized-mesh: `CesiumGS/quantized-mesh` `README.md` (binary) + `SPECIFICATION.md` (`layer.json`).
- cesium-native traversal: `Cesium3DTilesSelection/src/TilesetSelection.cpp`,
  `TilesetContentManager.cpp`; geospatial `CesiumGeospatial/src/Ellipsoid.cpp`; implicit
  `Cesium3DTilesContent/src/SubtreeAvailability.cpp` + the 3D Tiles 1.1 ImplicitTiling spec.
- render shim: `cesium-unreal` `CesiumGltfComponent.cpp` / `CesiumTextureUtility.cpp`.
- UE Mesh Terrain (rejected): `EngineE/Plugins/Experimental/{MeshTerrainMode,MeshPartition}` @ dev-5.8.
- Rust deps: `gltf`, `ktx2`, `basis-universal`, `meshopt`, `ureq`, `flate2`, `crossbeam`, `serde`,
  `glam` (existing); Draco vendored via `cc`.
- LogiUE reused: `logi_terrain_{lod,stream(culling,scheduler),cache,scene,geo,core}`; sample
  `C:\Users\premi\Downloads\AGI_HQ.kmz` (KML + Assimp COLLADA Z-up + one diffuse atlas).
