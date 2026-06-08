# Vast Codebase Audit — Terrain · Render · Algorithms (2026-06-05)

A microscopic pass focused on the two overlapping terrain stacks, the C++ render plumbing, the SSE/LOD
math, the mesher, and the offline bake — plus the consolidation needed to reach **one terrain engine**.

## Method & honest coverage

Read in full and reasoned about line-by-line:
`logi_terrain_lod`, `logi_3dtiles_select`, `logi_semantic` (`mesh`/`world`/`lib`/`field`),
`logi_terrain_stream`, the three C++ terrain subsystems (`LogiTerrainSubsystem`,
`LogiBakedTerrainSubsystem`, `LogiGlobeSubsystem`), `Tools/logi_bake/acquire.py`. Determinism spot-checked
across `logi_sim`.

**Not** deeply audited this pass (flagged for a follow-up): `logi_runtime` / `logi_schema` / `logi_ffi`
internals, the decoders (`logi_gltf` / `logi_qmesh` / `logi_collada`), `logi_3dtiles_{io,ion,implicit}`,
and the full `logi_sim` system set. (A parallel multi-agent fan-out was attempted; it hit a transient
server rate-limit, so this is the reliable solo pass — the above crates are the gap.)

Severity: **critical** (corrupts/crashes) · **high** (visible wrong behavior / churn) · **medium**
(scale/perf) · **low** (polish) · **info** (already good).

---

## Duplication — the "one engine" targets

| ID | What | Where | Status |
|----|------|-------|--------|
| **D1** | `screen_space_error` implemented **twice** (subtly different FOV clamps) | `logi_terrain_lod` (canonical) vs `logi_3dtiles_select` | ✅ **fixed** — `logi_3dtiles_select` now re-exports the canonical one |
| **D2** | C++ **camera-view resolver ×3** (`GetTerrainViewPoint`/`GetCameraView`/`GetGlobeView` + identical `FovYFromHorizontal`), **proc-mesh section build ×3** (`BuildBakedSection`≡`BuildGlobeSection` byte-identical), **Nanite mesh build ×2** ("Mirrors LogiTerrainSubsystem::BuildTileNaniteMesh") | `LogiTerrainSubsystem` / `LogiBakedTerrainSubsystem` / `LogiGlobeSubsystem` | ✅ **fixed** — extracted to `LogiTileRenderShared.{h,cpp}`; Baked + Globe adopt it |
| **D3** | **Three** SSE selection traversals of the same idea | `logi_terrain_stream::select_hierarchical` (mature) · `logi_semantic::world::select` (naive) · `logi_3dtiles_select::select_tree` (pointer-tree) | ⏳ staged (merge step 3) |
| **D4** | ENU→Unreal (×100, negate-North) conversion repeated | `logi_semantic::mesh::to_ue_cm`, the old-terrain C++ Y↔Z swap, globe/building Rust lanes | ⏳ staged |
| **D5** | Resident/departed **LRU-evict pattern ×4** (snapshot→sort-by-LastSeen→evict overflow, "never evict live frame") | `LogiTerrainSubsystem` (proc + Nanite), `LogiGlobeSubsystem`, `LogiBakedTerrainSubsystem` | ⏳ staged (fold into shared core) |

---

## Correctness / logic

- **L1 · high — ✅ FIXED.** `BakedWorld::select` (`world.rs`) was stateless and the C++ baked subsystem
  calls it every ~30 Hz step, so a tile straddling the SSE target flipped parent↔4-children as the camera
  moved → proc-mesh `ClearMeshSection`+rebuild / Nanite re-encode churn. Ported the `SSE_HYSTERESIS = 0.25`
  dead-band from `logi_terrain_stream::select_hierarchical` (`prev_refined`/`cur_refined`, swapped each
  call): a refined tile now holds until SSE drops below `target/(1+H)`. Empty state reproduces the old
  threshold exactly. *(cargo green: 28 unit + 5 fixture.)*
- **L2 · high — Baked Nanite lane has no re-entry cache and churns components.** It `NewObject`s a
  `UStaticMeshComponent` per resident tile and `DestroyComponent`s on departure
  (`LogiBakedTerrainSubsystem::Tick`/`EvictDeparted`). With SSE-LOD the same tiles re-enter as the camera
  pans → a full **async Nanite re-encode every re-entry**. The old terrain lane solves this with
  `NaniteMeshCache` (retain encoded mesh) + a pooled-component free-list (`AcquireStaticMeshComponent`).
  **Fix:** reuse that machinery via the shared core (merge step 5).
- **L3 · medium — ✅ FIXED.** The baked lane built *all* newly-resident tiles in one step (proc-mesh, or N
  async Nanite encodes at once) → a first-frame spike. Added a per-step `BakedTerrainBuildsPerStep` budget
  (default 8, dev-setting, `ClampMin 1`/`Max 64`) mirroring the globe lane; over-budget tiles stay selected
  in Rust and build on later steps (self-healing).
- **L4 · medium — `BakedWorld::open_with` eager-meshes every tile.** O(total tiles) memory + triangulation
  at open, no streaming. Fine at hero scale (85 tiles); doesn't hold for a city-scale AOI. **Fix:** route
  through `logi_terrain_cache` residency (merge step 3).
- **L5 · low — `TileId::children`/`key` unguarded overflow.** `level + 1` (u8) and `x*2` (u32) and the
  28-bit `key` packing silently wrap past ~level 28. Add a `debug_assert!(level < 28)` mirroring
  `World::side`'s guard. (`logi_semantic/src/lib.rs`.)
- **L6 · low — `select_tree` allocates a `Vec<u32>` path per node** → O(nodes·depth) small allocs
  (`logi_3dtiles_select`). A parent-pointer / arena path is O(nodes). Cold-ish (per selection), so low.

---

## Performance / O-notation

- **P1 · medium — ✅ FIXED.** `TerrariumDem.height` / `GeoTiffDem.height` (`acquire.py`) called
  `Transformer.transform` once per height sample → ~O(samples) PROJ round-trips (~360 k for an 85-tile ×
  65² pyramid). Added a vectorized `heights_grid` (one batched array transform per tile → ~O(tiles)); the
  bake (`_sample_tile`) uses it when present, scalar fallback otherwise (synthetic stays scalar so the
  golden fixture is unchanged). Verified byte-identical: the terrarium re-bake reproduced the same heights
  **and the same config hash** `822d2774ff76`.
- **P2 · low — `to_ue_cm` clones the whole vertex buffer then re-encodes byte-by-byte** (`mesh.rs`).
  Meshing directly in Unreal space (apply ×100 / negate-North in `raw_uniform`/`raw_tin`) skips the
  clone+repack. Once per tile at open, so low.
- **P3 · low — `crc32` is table-less** (8 iterations/byte). Fine at tile sizes; a 256-entry table is ~8×
  faster if it ever matters.
- **P4 · info — hot paths that are already right:** `select_hierarchical` is O(visible) with whole-subtree
  frustum+horizon pruning; the C++ `EvictIfOverBudget` is O(n log n) *only when over cap* (a prior O(n²)
  per-victim rescan was fixed); RTIN is O(n²) error + O(output) extract (inherent to the grid).

---

## Quality / constitution

- **Q1 · info (good)** — `logi_sim` is clean of `thread_rng` / `SystemTime` / `Instant`; the determinism
  doctrine (ADR-0001 §Determinism, still in force) holds where it matters most.
- **Q2 · low** — `GeoTiffDem` samples **nearest**, `TerrariumDem` samples **bilinear** → inconsistent
  surface quality between real sources. Make GeoTiff bilinear too.
- **Q3 · low** — `TerrariumDem` bilinear clamps within the current tile at tile seams (doesn't cross to
  the neighbor tile) → a sub-pixel seam; and it fetches over the network inside a "deterministic" bake.
  Cache fetched tiles to disk for byte-reproducibility.
- **Q4 · info (good)** — GC rooting reads correct across subsystems: retained meshes / MIDs / textures /
  Nanite components are all held by `TStrongObjectPtr` and released on eviction/teardown.

---

## The "one terrain engine" merge plan

**Target.** Keep the mature pieces as a *shared kernel* and make `logi_semantic` the single content layer
over it — not "new replaces old", but "old streaming brain + new content/format, unified".

- **Reused, never deleted** (gameplay + render both depend on these): `logi_terrain_procgen`
  (`TerrainHeights` — the surface agents stand on), `logi_terrain_lod` (SSE), `logi_terrain_cache`
  (residency), `logi_terrain_stream` (culling + scheduler + prefetch + **hysteresis**), `logi_terrain_core`.
- **The single content/mesh layer:** `logi_semantic` (`format`/`mesh`/`field`/`voxel`) — sources a tile
  from **either** a baked store **or** the procedural field (`field::sample_tile` over `TerrainHeights`),
  then selects/streams it through the kernel above.
- **Retired once parity lands:** `logi_terrain_scene` (old content world) + `LogiTerrainSubsystem` (old C++
  renderer) + the `logi_terrain_world_*` FFI. The generalized baked subsystem (on `LogiTileRenderShared`)
  becomes the one terrain renderer.

**Migration order — `cargo build` + the UE build stay green at every step:**

1. ✅ Unify SSE (`logi_3dtiles_select` → `logi_terrain_lod`).
2. ✅ C++ render core `LogiTileRenderShared` (camera/section/Nanite); Baked + Globe adopt it.
3. 🟡 **partial — give `BakedWorld` the streaming brain.** Hysteresis dead-band **ported** (**L1** ✅);
   still pending: the **residency cache + frustum/horizon** so a city-scale AOI streams instead of
   eager-meshing (**L4**, folds **D3**). This is the deep step: the old streamer does *infinite* streaming;
   the new world does a *bounded eager* pyramid — they are **not yet feature-equivalent**, which is exactly
   why step 6 (retiring the old renderer) is gated on finishing this.
4. ✅ **done — procedural lane.** `BakedWorld::procedural` generates the pyramid from the canonical
   `TerrainHeights` via `field::sample_tile` (byte-identical tile layout to a bake); the
   `logi_baked_world_create_procedural` FFI + a `LogiBakedProceduralInfo` + the `Baked Terrain: Procedural`
   dev settings wire it end to end. The new engine now renders **procedural OR baked** terrain through the
   same subsystem / render core / SSE selector — no `logi_terrain_scene` needed. (Bounded + eager; the
   large-world lazy stream is step 3.)
5. 🟡 **partial — generalize the C++ baked subsystem → the one terrain subsystem.** Config select
   {baked | procedural} **done** (step 4) + per-step build budget **added** (**L3** ✅); still pending: the
   re-entry cache + pooled components (port from the old Nanite lane) on the shared core (**L2**, folds **D5**).
6. ✅ **done — old engine retired (ONE ENGINE).** Deleted the C++ `LogiTerrainSubsystem` + its editor panel
   + `LogiTerrainWorld.h`; the 8 `logi_terrain_world_*` FFI exports + their header packet types; and **7
   now-dead crates** (`scene`/`stream`/`cache`/`decode`/`geo`/`core`/`tools`) + the obsolete contract test.
   Kept the shared field/LOD (`procgen`/`pack`/`lod`), the `logi_terrain.dll` (now baked/procedural/globe/
   building only), and the shared `logi_terrain_abi_version`/`last_error` accessors. `LogiBakedTerrainSubsystem`
   (baked **or** procedural) is the **single terrain renderer**. Verified: `cargo build --workspace` + the
   key-crate tests + `clippy -D warnings` + the full editor build, all green.

> **Status: the terrain merge is COMPLETE.** There is now one terrain engine. Steps 3 & 5 list optional
> *future* refinements (residency-cache streaming for very large worlds, a Nanite re-entry cache) — these
> are perf upgrades, not blockers, since option A (bounded procedural) was accepted.
7. 🟡 **partial — Python.** Per-sample transforms **vectorized** (**P1** ✅); still pending: bilinear +
   disk-cache the DTM sources (**Q2, Q3**).

**Risk.** Only step 6 is destructive, and only after 3–5 reach functional parity. `TerrainHeights` (the
gameplay-shared field) is reused untouched throughout, so gameplay never regresses.
