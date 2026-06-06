# LogiUE — Deep Quality / Logic / Duplication Audit

**Date:** 2026-06-05
**Method:** Six parallel microscopic audits (terrain kernel, glTF/mesh/IO decode, 3D-Tiles selection + geospatial + semantic, core sim/runtime/FFI, the C++ `LogiBevy` bridge, and a whole-repo cross-cutting pass for duplication / dependency hygiene / docs drift). Every in-scope source file was read in full and checked against [`AGENTS.md`](../../AGENTS.md).
**Builds on:** [`2026-06-Codebase-Audit.md`](2026-06-Codebase-Audit.md) (prior terrain/render-focused round). Status of its open items is noted under *Cross-cutting → Duplication*.

> **Path note:** This audit was performed against the **pre-Geo-rename** layout, so paths below read `Rust/crates/logi_terrain_*` and `Rust/crates/logi_<crate>`. As of 2026-06-05 that stack was renamed to the **Geo** engine and moved under `Rust/crates/geo/`: `logi_terrain_{pack,procgen,lod,ffi} → logi_geo_*` (cdylib `logi_terrain.dll → logi_geo.dll`; C++ `ULogiBakedTerrainSubsystem → ULogiBakedGeoSubsystem`); the 3D-Tiles / geospatial / semantic crates kept their names under `geo/`. To open a finding today, map `Rust/crates/<x>` → `Rust/crates/geo/<x>` (and `logi_terrain_* → logi_geo_*`).

---

## Executive summary

The codebase is, on the whole, **disciplined and architecture-respecting**. The hard law of [`AGENTS.md`](../../AGENTS.md) holds on every load-bearing path: gameplay truth lives only in Bevy Components/Resources/Systems; no Unreal types cross into Rust; Rust never calls Unreal; the FFI is **batched** (one world-level snapshot, never per-entity); `unsafe` is concentrated in the three boundary crates (11 non-FFI crates carry `#![forbid(unsafe_code)]`); and determinism (seeded RNG, sorted-before-emit, no wall-clock in the step) is taken seriously. Copy/alloc density is genuinely low for the size.

The findings are therefore mostly **localized correctness, robustness, and duplication** issues rather than systemic rot. The themes that recur across crates:

1. **FFI panic safety is incomplete.** The runtime ABI guards every entry point with `catch_unwind`, but the **baked-terrain lane** and the **editor lane** do not — a panic there unwinds across the C ABI (**undefined behavior**), and both modules' own docs claim the opposite. *(CR1, H1)*
2. **The 32-byte interleaved vertex format is the single biggest duplication.** Its layout + interleave routine is reimplemented in **5 crates** with the stride `32` as a scattered magic literal, and two FFI lanes derive `vertex_count` from a *different* source than the buffer they hand out — a latent OOB GPU upload. *(H5, H12)*
3. **Untrusted-input robustness in the streaming/IO layer.** Unbounded HTTP body reads (OOM/DoS), no request timeout (worker-thread starvation), and several "decode error → silently degenerate/empty" paths. *(H8, H9, H10, M13–M18)*
4. **A constitutional drift + one real violation.** `AGENTS.md` §4 describes a crate layout that no longer exists, and `bevy_ecs` default features pull `wgpu-types`/`bevy_reflect` into the graph — forbidden by rule 10 / ADR-0004. *(H3, H4)*
5. **Dormant features wired end-to-end except the last step** (actor rotation/scale, animation bridge, `AnimationDrive` insertion, `SimRng` consumption) read as oversights and are landmines for the next contributor. *(M1, M2, M23, M26)*

Counts: **1 Critical, 13 High, ~30 Medium, ~30 Low**, plus the cross-cutting duplication/dependency/docs items. No memory-safety defect, no truth-ownership leak, and no determinism break on the gameplay hot path were found.

---

## Remediation status (2026-06-05)

Per the follow-up directive ("do all others"), these findings were fixed this session and verified — `cargo build` + full test suite + `clippy -D warnings` + `fmt`, **all green** across the now-21-crate workspace.

- **Fixed (Rust, verified):** CR1; H1, H5, H6, H7, H8, H9, H10, H13; M3–M20, M23–M30; the Low-severity items; `bevy_app` removed (DEP-2). A new shared `logi_geo_mesh` crate single-homes the vertex layout + `interleave` (H12/DUP-1), the ENU↔UE convention on the streaming path (M21/DUP-3), `VERTEX_STRIDE`, and AABB→sphere (DUP-5). `logi_3dtiles_implicit` documented as intentionally staged (DEP-1). H4 (the AGENTS.md crate map) was fixed during the Geo rename.
- **Fixed (C++ bridge — needs `rebuild-editor.ps1` to compile-verify; not buildable headless):** H11, M1, M2, M3, M4, and the C++ Lows.
- **Won't fix (by decision):** **H2** (rendering needs no determinism — non-authoritative, ADR-0007); **H3** (`bevy_ecs` default features kept — rule 10 / ADR-0004 amended to permit the type-only transitive `wgpu-types`).
- **Also fixed (M22 / DUP-2):** the SSE refine criterion + hysteresis dead-band is now single-homed in `logi_geo_lod` (`RefineParams` + `should_refine`); both the baked quadtree selector (`logi_semantic`) and the 3D-Tiles pointer-tree selector (`logi_3dtiles_select`) descend by it — baked with hysteresis (0.25), 3D-Tiles stateless (0.0). The two tree-*walks* stay separate by design: they traverse structurally different trees (an indexed quadtree vs a pointer tree threading a world transform + streaming fallback), so a single generic walker would add indirection for no correctness gain — only the duplicated *decision* was merged. Behavior preserved (selection tests green; 2 new engine tests added).
- **Deferred (recommend separate, reviewed changes):**
  - **DUP-4** — FNV-1a / SplitMix64 dedup is low value and feeds cache / content-addressing identity, so extraction carries cache-invalidation risk out of proportion to the gain.
  - **M21 (remainder)** — `logi_semantic`'s in-place byte transform and the M1 building path keep their local f32 ENU math on purpose (reusing the f64 scalar fns would shift rounding in non-authoritative render data).

---

## Critical

### CR1 — Baked-terrain FFI lane can unwind a panic across the C ABI (UB)
**Architecture / FFI-safety · `Rust/crates/logi_terrain_ffi/src/ffi_baked.rs:90,167,221,258,305`**
Unlike the globe lane in `ffi.rs` (which wraps every body in `guard(...)`/`catch_unwind`), **none** of the baked-world entry points (`logi_baked_world_create`, `_create_procedural`, `_step`, `_select`, `_get_frame`) have a panic guard — yet the module doc (`ffi_baked.rs:9`) claims "no panic unwinds across the boundary." Any panic inside `logi_semantic` (allocation failure, a slice index, a `debug_assert`, an `unwrap` in the TIN/meshing path) unwinds straight across `extern "C"` into the C++ caller, which is undefined behavior. The `LogiStatus::Panic` (`-3`) contract is entirely defeated for this lane. `panic = "abort"` only applies to the `release` profile (`Rust/Cargo.toml:51`); editor/dev/test builds unwind.
**Fix:** Factor the `catch_unwind(AssertUnwindSafe(...))` guard from `ffi.rs` into a shared `guard_i32`/`guard_ptr` helper and wrap every baked entry-point body, mapping `Err` → `LOGI_PANIC`/`LOGI_INTERNAL` (creators return null).

---

## High

### H1 — Editor FFI entry points lack `catch_unwind` (can unwind into C++)
**FFI-safety · `Rust/crates/logi_ffi/src/editor.rs:37,76`**
`logi_editor_query_entities` / `logi_editor_set_debug_pin` are the only `extern "C"` fns in `logi_ffi` not wrapped in `catch_unwind`, despite the crate doc (`lib.rs:6`) promising "panics never cross the boundary." `query_entities` runs `world.query::<…>()` + `.collect()` (`logi_runtime/src/lib.rs:494`) — all of which can panic. The editor ABI is precisely a non-`release` build, so `panic = "abort"` does not protect it.
**Fix:** Wrap both bodies in `catch_unwind(AssertUnwindSafe(...))` like the runtime fns.

### H2 — Non-deterministic render-set ordering in tile selection
**Determinism · `Rust/crates/logi_3dtiles_select/src/lib.rs:133,146`**
`Selection.render` is built from `HashMap<Vec<u32>, Rc<SelectedTile>>` via `render.into_values().collect()` with no sort. `HashMap` iteration order is randomized per process, so the render list differs every run for identical inputs — breaking the byte-and-order-identical replay the sibling `logi_3dtiles_io` deliberately preserves (it sorts completion order), and making the ADR-0009 golden-camera-path tests flaky.
**Fix:** Collect into a `Vec` and `sort_by(path)` before returning (or accumulate in a `BTreeMap`). Consider sorting `load` too.
**Decision (2026-06-05): WON'T FIX.** Rendering requires no determinism — the Geo engine is non-authoritative (ADR-0007), so render/tile-selection order never affects gameplay replay or snapshot equality. The "breaks replay" framing applies only to the *sim*, which is already deterministic. Left as-is by design.

### H3 — `wgpu-types` / `bevy_reflect` enter the dependency graph (violates AGENTS.md rule 10)
**Architecture / Dependencies · `Rust/Cargo.toml:42`, `Rust/Cargo.lock`**
`bevy_ecs = "0.18.1"` is declared with **default features**, which enable `bevy_reflect`, pulling `wgpu-types 27.0.1` (+ `web-sys`/`js-sys`) into the build graph. [`AGENTS.md`](../../AGENTS.md) §10 / ADR-0004 explicitly forbid `wgpu`. No gameplay component derives `Reflect` (all use plain `#[derive(Component)]`), so the feature is almost certainly unused.
**Fix:** Set `bevy_ecs = { version = "0.18.1", default-features = false }` in `[workspace.dependencies]`, re-enable only what builds. Verify the workspace still compiles; if `bevy_ecs 0.18` hard-requires reflect, document the exception in ADR-0004 rather than leaving the rule silently broken.

### H4 — The constitution's own crate map is wrong (AGENTS.md drift)
**Docs / Architecture · `AGENTS.md:79,109,114`**
§4 "Where things live" lists a `Rust/crates/logi_terrain` crate that **does not exist** (terrain is `logi_terrain_pack/procgen/lod/ffi`); lists `Source/LogiCore` + `Source/LogiTerrain` C++ modules that were already removed; omits the entire 3D-Tiles + semantic tower; and cites only ADR-0001–0005 though ADR-0006–0011 govern current work. A new agent reading only AGENTS.md (as it instructs) gets a pre-Milestone-F mental model.
**Fix:** Rewrite §4 from the real `Rust/Cargo.toml` member list (post-Geo-rename), drop the dead `Source/*` row, and add ADR-0006–0011 to §1 + the ADR list. *(Handled in this session's AGENTS.md pass.)*

### H5 — Vertex `count`/`stride` open-coded and sourced inconsistently → latent OOB GPU upload
**Logic / Copy · `Rust/crates/logi_terrain_ffi/src/ffi.rs:183-186,425-443`**
The building + globe lanes hardcode `vertex_stride: 32` and `vertex_count: m.vertices.len() / 32` instead of the shared `logi_semantic::VERTEX_STRIDE` (which `ffi_baked.rs:130` uses correctly). Worse, `logi_globe_world_step` sets `vertices`/`vertices_len` from `tile_mesh(...)` but `vertex_count`/`index_count` from a **separate** `RenderTileInfo` query — they agree today only by coincidence; any future re-decode between the two reads hands C++ a length from one buffer and a count from another → out-of-bounds upload. `len() / 32` also silently floors a corrupt non-multiple-of-32 buffer.
**Fix:** One `pub const VERTEX_STRIDE`; derive `vertex_count`/`index_count` from the *same* `m` buffer whose pointer is emitted; `debug_assert!(len % VERTEX_STRIDE == 0)`.

### H6 — `Package::page_bytes` can panic OOB on a crafted/truncated package
**Logic · `Rust/crates/logi_terrain_pack/src/package.rs:178`**
`&bytes[start..start + desc.stored_len as usize]` raw-indexes using a `PageDescriptor` whose fields are `pub` on a `Copy` struct, so a caller can hand back a tweaked descriptor; `start = desc.offset as usize` also truncates `u64`→`usize` on 32-bit and can wrap on add. The directory invariant is enforced only inside `parse`, not by the type.
**Fix:** Make `page_bytes` total: `self.bytes.get(start..start.checked_add(len)?)` returning `Option`, and/or make the descriptor fields read-only so only `parse` can mint them.

### H7 — `desired_level` contradicts its own doc ("coarsest" vs finest-required)
**Logic / Docs · `Rust/crates/logi_terrain_lod/src/lib.rs:21-40`**
The doc says it returns "the coarsest level whose SSE is within target," but higher level = **finer** here (geometric error halves each level), and the loop returns the finest level required — the companion test `coarser_sse_target_picks_lower_level` encodes the opposite convention from the word "coarsest." A foot-gun for anyone mapping levels↔bands.
**Fix:** Make doc, name, and the test's direction agree (state "higher level = finer; returns the finest level required to bring SSE within target").

### H8 — `http_get` reads an unbounded response body (OOM / DoS)
**Robustness / Architecture · `Rust/crates/logi_3dtiles_io/src/lib.rs:171`**
`resp.into_reader().read_to_end(&mut bytes)` has no size cap; tile content URIs are explicitly attacker-influenced (per the `resolve_url` comment). A hostile/misconfigured server can stream gigabytes and OOM the process. (The ion resolver's `into_string()` caps ~10 MB; this geometry path does not.)
**Fix:** `.take(MAX_TILE_BYTES)` before `read_to_end`; pre-size from a sane `Content-Length`; treat a cap hit as a fetch error.

### H9 — No HTTP timeout → a stalled server pins a worker thread forever
**Robustness · `Rust/crates/logi_3dtiles_io/src/lib.rs:172`, `Rust/crates/logi_3dtiles_ion/src/lib.rs:91`**
Both use `ureq::get(...).call()` with no agent/timeout; ureq applies no overall timeout by default. A server that accepts then stalls blocks that worker indefinitely; a few stuck hosts starve the whole fetch pool and streaming silently stops.
**Fix:** A shared `ureq::Agent` with connect/read/write (+ overall) timeouts, reused across requests (also gains connection pooling).

### H10 — Blocking network reachable from the kernel path (`from_ion`)
**Architecture · `Rust/crates/logi_3dtiles_scene/src/streaming.rs:136`**
`from_ion` does two blocking round-trips; the doc says "call off the render thread" but nothing enforces it, and combined with H9 a slow endpoint blocks world construction unboundedly.
**Fix:** Ensure the only kernel-reachable constructor is the non-blocking `new(bytes)`; move `from_ion`'s fetches onto the worker pool, or mark it test/tools-only.

### H11 — ISM `HighWater`/`FreeSlots`/`EntityToSlot` desync if the ISM is GC'd while its bucket survives
**Logic / Lifetime (C++) · `Plugins/LogiBevy/Source/LogiBevyRuntime/Private/LogiBevyInstancedRenderer.cpp:71-114,165`**
The bucket holds the ISM as `TWeakObjectPtr`. When it goes stale, `GetOrCreateComponent` makes a fresh 0-instance ISM but does **not** reset `HighWater`/`FreeSlots`, and `EntityToSlot` still maps live entities to now-nonexistent slots. The `bIsNew` invariant ("`AddInstance` index == `Slot`") breaks → `UpdateInstanceTransform`/`SetCustomData` index out of range (only `checkSlow` catches it, debug-only).
**Fix:** On *replacement*, reset that bucket's `HighWater=0` + `FreeSlots`, purge its `EntityToSlot` entries; or make the ISM a strong ref (the host actor already GC-roots it, so the weak ptr buys nothing).

### H12 — The 32-byte interleaved vertex format is reimplemented in 5 crates
**Duplication · `logi_gltf/src/lib.rs:54`, `logi_collada/src/lib.rs:45`, `logi_semantic/src/mesh.rs:18,91`, `logi_3dtiles_scene/src/{lib.rs:44,streaming.rs:483}`**
Four independent definitions of the `pos f3 | normal f3 | uv f2` layout and three near-identical `interleave`/`push_vertex` helpers; stride `32` is a magic literal in 5+ files (incl. the FFI structs). Adding a tangent/color touches all of them with no compiler help. This also generates ~27 infallible `try_into().unwrap()` byte-slice idioms in `logi_semantic`.
**Fix:** A small `logi_geo_mesh` (or `geo/`-level) crate owning `VERTEX_STRIDE`, `interleave(...)`, and a vertex read/write accessor; all decoders + FFI depend on it. **Highest-value consolidation in the repo** — also subsumes the ENU→UE duplication (M21) and the bounds-sphere duplication.

### H13 — `logi_world_get_render_snapshot` lifetime contract is self-inconsistent + no owner-thread guard
**FFI-safety / Lifetime · `Rust/crates/logi_ffi/src/lib.rs:312-394`**
The fn doc says "valid until the next tick"; the struct doc + `SnapshotRing` say the slot survives until retired or reused (≥2 ticks). These disagree, so C++ retain/release can't be written correctly. The read path also deliberately omits the owner-thread check that every mutating fn has, so a non-owner reader racing the owner's next `publish()` (which reallocates a slot's `Vec`) is prevented only by unenforced prose.
**Fix:** Unify the lifetime wording to the real contract (valid until `logi_buffer_release` or slot reuse); add an owner-thread debug-assert on the read path.

---

## Medium

| # | Area | Location | Problem | Fix |
|---|---|---|---|---|
| M1 | Logic (C++) | `LogiBevyActorPool.cpp:132` | `ApplyActorUpdate` applies only `SetActorLocation`; rotation+scale from `transform_3x4` are dropped, so the *most visible* (actor-tier) entities render axis-aligned/unit-scaled. | Reuse `WorldTransformFrom3x4` (extract from the renderer) and `SetActorTransform`. |
| M2 | Logic (C++) | `LogiBevyAnimationBridge.cpp:5` | The anim bridge stores params in a `TMap` and stops; AnimInstance push + budget-allocator registration are TODO. The whole `LogiAnimationParams` channel is discarded. | Wire to pooled skeletal AnimInstance + budget allocator, or drop the map until it exists. |
| M3 | Lifetime (C++) | `LogiBevyActorPool.cpp:138` | `EntityToActor.FindOrAdd` inserts the key *before* acquire; if `AcquireActor` returns null the entity stays as a dead entry forever and re-spawns every frame. | Acquire into a local; only `Add` on success. |
| M4 | Dependencies | `Rust/Cargo.toml:25` | `logi_3dtiles_implicit` is an **orphan** — built, tested, depended on by nothing (no `path=` edge anywhere; not in `Build.cs`). Dead weight on every build/clippy/CI; drifts from `tileset::ImplicitTiling`. | Wire into `logi_3dtiles_scene` (intended consumer) or drop from `[workspace] members` until Milestone-F implicit streaming lands. |
| M5 | Copy / Dup | `logi_semantic/src/mesh.rs:49,68` | `to_ue_cm` clones the full vertex buffer **and** the index `Vec` (indices pass through unchanged) → double peak geometry alloc during world build; flagged by 3 agents. | Add `into_ue_cm(self)` that mutates `vertices` in place and **moves** `indices`. |
| M6 | Determinism / Copy | `logi_terrain_ffi/src/ffi_baked.rs:272` | Per-frame `HashSet` alloc in `logi_baked_world_select`; also `BakedWorld::select` carries hysteresis state, so it is **not** a pure function of inputs (render-only). | Return selected slice/indices directly or reuse a scratch set; document "render-only, not for replay." |
| M7 | Logic | `logi_geospatial/src/lib.rs:101` | `cartesian_to_cartographic` normalizes a ~zero vector at/near the geocenter → NaN lat/lon with no error; poisons every derived ENU frame. | Guard the degenerate case (return a sentinel `Cartographic` or `Option`). |
| M8 | Copy | `logi_3dtiles_select/src/lib.rs:199` | `child_path = path.clone()` per child in the hot traversal — O(nodes·depth) `Vec<u32>` allocs, despite the header claiming per-node allocation is avoided. | Single shared path stack: `push`/`pop`, clone only at keep-points. |
| M9 | Copy | `logi_3dtiles_select/src/lib.rs:91` | `Region::bounding_sphere_ecef` allocates an 8-elem `Vec<DVec3>` per region tile per frame. | Use a `[DVec3;8]` / single-pass center+radius. |
| M10 | Copy | `logi_qmesh/src/lib.rs:426` | `to_ecef_mesh` clones the whole index buffer via `to_u32()` (pure clone in the U32 arm) then re-walks it. | `IndexBuffer::iter_u32()` lazy iterator; feed normals + triangle filter from it. |
| M11 | Copy | `logi_gltf/src/lib.rs:153` | `decode_glb` `.to_vec()`s every embedded image out of the blob; copied again downstream. | Retain the blob (`Arc<[u8]>`/`bytes::Bytes`) and have `EmbeddedImage` borrow an offset+len. |
| M12 | Copy | `logi_3dtiles_scene/src/streaming.rs:402` | `rebase_decoded` byte-bashes each vertex's 8 f32s one at a time — third pass over data gltf already decoded then interleaved. | Expose typed attrs from `logi_gltf` / `bytemuck::cast_slice` whole vertices. |
| M13 | Logic | `logi_collada/src/lib.rs:69` | `read_n` returns `[0,0,0]` for out-of-range indices → silently degenerate (origin-collapsed) mesh, no error, though `BadSource` exists. | Distinguish "absent attribute" (default) from "index OOB of a present source" (error). |
| M14 | Logic | `logi_3dtiles_scene/src/streaming.rs:388` | `bounds_radius` uses `f32::MAX`/`MIN` sentinels; an all-NaN/degenerate mesh yields a ~`f32::MAX` radius that drives culling/placement. | Use `±INFINITY` sentinels + a finiteness guard after the loop. |
| M15 | Copy | `logi_3dtiles_scene/src/streaming.rs:236` | `evict_decoded` clones+sorts **all** non-live candidate paths every overflow step (often to drop 1). | `select_nth_unstable_by_key`, or an LRU index like `DiskCache`. |
| M16 | Logic | `logi_3dtiles_io/src/lib.rs:234` | Disk-cache key = FNV-1a(path) with no stored URL → a hash/path collision silently serves another tile's bytes. | Store+verify the URL, or use a 128-bit hash. |
| M17 | Logic | `logi_3dtiles_scene/src/streaming.rs:365` | `graft` swallows external-tileset parse failures, but the caller records `bases.insert(...)` unconditionally → subtree silently never attaches and is never retried. | Return `bool`/`Result`; only insert base on success; surface the error. |
| M18 | Logic | `logi_3dtiles_scene/src/streaming.rs:459` | `is_tileset_uri` is extension-only; extensionless/query tileset endpoints (common) get misrouted to `decode_glb` and dropped. | Sniff bytes (`{` vs glTF magic) instead of trusting `.json`. |
| M19 | Perf | `logi_semantic/src/store.rs:98`, `world.rs:61` | `BakedWorld::open_with` is O(n²): `read_tile(id)` does a linear `manifest.tiles.iter().find` per tile. | Read from `&ManifestTile` directly or build a `HashMap<u64,usize>` index once. |
| M20 | Architecture | `logi_semantic/src/store.rs:130` | `BakedTileSource::request` does synchronous `fs::read` on the owner thread, violating the off-thread `TileContentSource` contract. | Move the read to `poll_completed`/a worker, or document it as a blocking local source. |
| M21 | Duplication | `logi_3dtiles_scene/src/streaming.rs:498`, `logi_semantic/src/mesh.rs:48`, `logi_3dtiles_scene/src/lib.rs:57` | ENU→Unreal conversion (×100, negate-North) — the most safety-critical coord convention — hand-copied in 3 places. | One module: `enu_pos_to_ue`/`enu_dir_to_ue`/`ue_cm_to_enu` (+ in-place variant). Fold into the H12 crate. |
| M22 | Duplication | `logi_semantic/src/world.rs:156`, `logi_3dtiles_select/src/lib.rs:126` | Two SSE tree-selection traversals with divergent features (semantic has hysteresis, no cull; 3dtiles has cull+fallback, no hysteresis). | Promote a generic refine engine into `logi_terrain_lod` (it owns the SSE math) parameterized over a tree-walker + predicates. |
| M23 | Dead / Determinism | `logi_sim/src/resources.rs:33`, inserted `logi_runtime/src/lib.rs:211` | `SimRng` is created from the seed but **never read** by any system; the first RNG need may reach for `thread_rng` (a rule-8 break) because the sanctioned path isn't exercised. | Add a trivial consumer + a same-seed/diff-seed replay test; doc it as the only sanctioned randomness. |
| M24 | Dead / Dup | `logi_render_extract/src/lib.rs:384` | `build_ffi_batches` is `pub`, documented as *the* marshaller, and **never called** — the runtime re-implements the same `unsafe` pointer-baking inline (`logi_runtime/src/lib.rs:144`). | Delete it, or have `publish` call it (removes the duplicated `unsafe`). |
| M25 | Quality | `logi_sim/src/schedules.rs:77` | 4 of 7 schedules (`input_ingest`, `fixed_pre_sim`, `fixed_post_sim`, `maintenance`) are empty but `run` every step; the `input_ingest` schedule does nothing while a same-named *method* does the work. | Move `ingest_input` into a real system in the schedule, or skip empty `run`s; document reserved phases. |
| M26 | Logic | components `logi_sim/src/components.rs:61`, spawn `logi_runtime/src/lib.rs:304` | `AnimationDrive` is never inserted on spawned entities → the actor animation lane always emits defaults (inert end-to-end). | Add to the spawn bundle (or an attach-on-tier system), or document the lane as dormant. |
| M27 | Copy | `logi_runtime/src/lib.rs:496` | `editor_query_entities` scans the population **twice** (count + collect) plus a throwaway `Vec`. | Push directly into `editor_query_buf`; get `total` from an O(1) count if available. |
| M28 | Logic | `logi_ffi/src/lib.rs:360` | `get_render_snapshot` rejects any non-latest `snapshot_id` even though the ring holds 3 live slots and `logi_buffer_release` takes arbitrary ids — the API promises random access it doesn't provide. | Look the id up across `ring.slots`, or narrow the API/docs to "0=latest only." |
| M29 | Docs (C++) | `LogiBevySchemaExporter.cpp:29` | Exported JSON keys (`high_res_actors`, …) don't match the Rust `WorldConfig`/`LogiInitDesc` field names → silent bind failure when a loader is added; a parallel struct already drifting. | Generate from the Rust types, or align key names to the `#[repr(C)]`/serde names now. |
| M30 | Logic | `logi_qmesh/src/lib.rs:254,326` | `decode_indices_u16` casts the running `u32` index to `u16`; a malformed tile claiming `vertex_count==65536` can drive `highest` to 65536 and silently alias to 0 (wrong-but-plausible mesh). | Keep indices `u32`; reject/clamp any index `>= vertex_count` rather than truncating. |

---

## Low (condensed by theme)

**Quality / docs in code**
- Stale **"Unity"/"DllImport"** doc-comments throughout `logi_terrain_ffi` (`lib.rs:1,17,19`, `Cargo.toml:9`) — this is an Unreal project; the ABI consumer is the C++ bridge. *(cross-cutting DOC-4)*
- Dead references to **deleted crates** (`logi_terrain_scene`/`stream`/`world`) in live comments: `logi_render_extract/src/lib.rs:25` (cites a `REBASE_SNAP` constant in a crate that no longer exists), `logi_terrain_procgen/src/lib.rs:50`, `logi_runtime/src/lib.rs:213,717`, `logi_semantic/src/world.rs:34`, `logi_terrain_ffi/src/ffi.rs:280`. *(DOC-5/6)*
- `logi_terrain_ffi.h:4` header comment claims it mirrors the deleted `logi_terrain_scene`. *(L2)*
- `LogiStatus::Internal`/`AbiMismatch` (`ffi.rs:16`) are documented in the header but never emitted → misleads C++ error handling. *(L1)*
- `Compression` enum has one `None` variant; `page_bytes` ignores compression, so a future non-`None` descriptor would silently return compressed bytes — add a `debug_assert`/explicit error. *(`logi_terrain_pack/src/page.rs:81`)*
- `WGS84_FLATTENING` (`logi_geospatial/src/lib.rs:18`) is dead; `geodetic_surface_normal` doc overstates correctness for off-surface points (`:84`). *(L2/L3)*
- Misleading/stale TODOs: `LogiBevyInstancedRenderer.h:40` ("wired next" — already wired); behavior-affecting TODOs at `LogiBevyActorPool.cpp:146`, anim bridge. *(C++ L11)*

**Copy / alloc (cold paths)**
- `height_page` allocates an all-`false` `holes` `Vec` of n² for procedural tiles that never have holes (`logi_terrain_procgen/src/lib.rs:132`). *(L5)*
- `LoadObject<UStaticMesh>(".../Cube")` on every new bucket/actor (`LogiBevyInstancedRenderer.cpp:101`, `LogiBevyActorPool.cpp:82`) — cache once. *(C++ L6)*
- `interleave` does 8 separate 4-byte `extend_from_slice` per vertex (`logi_gltf/src/lib.rs:54`, `logi_collada/src/lib.rs:45`). *(L1)*

**Robustness (untrusted input, fail-soft)**
- `oct_decode` defaults missing normal bytes to "up" instead of validating `oct.len() >= 2·vc` (`logi_qmesh/src/lib.rs:428`). *(L2)*
- `tile_bounds_geodetic` accepts hostile `z` → zero-area bounds (`logi_qmesh/src/lib.rs:371`). *(L3)*
- `DiskCache::open` seeds LRU from filesystem `mtime` (non-monotonic) and silently drops unreadable shards (`logi_3dtiles_io/src/lib.rs:261`). *(L5)*
- `LastFfiError()` writes `Buf[N]` trusting the FFI return length — 1-byte stack overflow if it ever returns `buf_len` (`LogiGlobeSubsystem.cpp:91`). *(C++ L4)*

**FFI-safety (defense-in-depth; benign on x86-64)**
- Unaligned `reinterpret_cast<const float*/uint32*>` of `Vec<u8>` FFI buffers in `LogiTileRenderShared.cpp:80,86,115,138,153` — the building lane already uses `Memcpy` for this reason; the shared core is inconsistent. *(C++ L1)*
- Index buffers read trusting `index_count` with no `indices_len` to clamp against (`LogiTileRenderShared.cpp:88`, `LogiBuildingSubsystem.cpp:144`). *(C++ L2)*
- `meshing_mode_from` / create-info pointers use `&*info` (alignment-assuming) instead of `read_unaligned` like the globe/building lanes (`ffi_baked.rs:73,173`). *(M4)*
- Globe camera passed as f64 but baked camera truncated to `f32` (`LogiBakedTerrainSubsystem.cpp:267` vs `LogiGlobeSubsystem.cpp:310`) — planetary-precision inconsistency. *(C++ L3)*

**Determinism / correctness (latent)**
- `TileId::children`/`key` overflow unguarded (`logi_semantic/src/lib.rs:62`) — prior audit L5, still open. Add `debug_assert!(level < 28)`.
- `allocate_id` uses plain `+=` (can wrap at extreme scale) while the rest of the file uses `wrapping_add` (`logi_runtime/src/lib.rs:264`). *(L4)*
- `Region` bounding sphere is wrong for antimeridian-crossing regions (`logi_3dtiles_select/src/lib.rs:80`) — conservative (over-refines), not unsafe. *(L1)*
- `Refine::Add` is silently treated as REPLACE in selection (`logi_3dtiles_select/src/lib.rs:198`) — known v1 gap; an ADD-root tileset renders holes with no warning. *(L4)*
- `with_seed` test helper is `pub` (not `#[cfg(test)]`), inviting non-canonical terrain (`logi_sim/src/terrain.rs:53`). *(L6)*

**Magic numbers / tuning**
- Significance falloff `1.0e-4` and route-cost clamps `20.0`/`0.05` are bare literals in gameplay-tuning math (`logi_sim/src/systems.rs:48`, `terrain.rs:83`). *(L2)*
- `custom_data: [0.0;8]` shipped per instance every frame, always zero (`logi_render_extract/src/lib.rs:311`); editor outliner hardcodes a 500-row cap with no paging (`LogiBevyEditorSubsystem.cpp:192`). *(L1, C++ L8)*

**Game-thread hitches (C++)**
- Globe per-tile JPEG decode + texture upload runs synchronously in `Tick` (`LogiGlobeSubsystem.cpp:348`) — move decode to a worker, keep only `UpdateResource` on the game thread. *(C++ L9)*

---

## Cross-cutting

### Duplication (consolidation targets, by value)
1. **32-byte vertex format + interleave** — 5 crates (**H12**). Extract a `geo/`-level mesh crate. Folds in #2, #3, and ~27 infallible unwraps.
2. **ENU↔Unreal conversion** — 3 crates (**M21**).
3. **AABB→bounding-sphere** — 4 sites (`logi_semantic/src/mesh.rs:326`, `logi_3dtiles_scene/src/{lib.rs:90,streaming.rs:441}`).
4. **FNV-1a 64** ×3 (`logi_terrain_pack/src/page.rs:43`, `logi_3dtiles_io/src/lib.rs:234`, `logi_terrain_ffi/src/ffi.rs:290`) + **SplitMix64** ×2 (`logi_3dtiles_io/src/lib.rs:41`, `logi_sim/src/resources.rs:44`) — a `logi_hash` leaf; drift here silently breaks cache identity.
5. **SSE tree traversal** ×2 (**M22**).

*Prior-audit status:* D1 (SSE ×2) and D2 (C++ camera/section/Nanite ×3) **fixed**; D3 (traversals) and D4 (ENU→UE) **still open**; D5 (LRU) largely mooted by deletions.

### Dependency hygiene
- **H3** `wgpu-types`/`bevy_reflect` via default features — *violation*.
- **M4** orphan `logi_3dtiles_implicit`.
- `bevy_app` declared in `[workspace.dependencies]` but used by no crate — delete (`Rust/Cargo.toml:44`).
- Graph is otherwise a clean DAG, no cycles, no version drift; workspace deps consistently used.

### Docs drift
- **H4** AGENTS.md §4 crate map + ADR list (the big one).
- DOC-4/5/6 stale "Unity"/deleted-crate references in `logi_terrain_ffi` and other comments.

### Workspace-wide patterns (healthy)
- `unwrap/expect/panic`: 97 total, **none on the gameplay hot path** (`logi_sim`/`logi_runtime`/`logi_render_extract` `src` are clean; the rest are tests or infallible `try_into` idioms).
- `unsafe`: 106 hits, **concentrated in the 3 FFI crates**; 11 crates `#![forbid(unsafe_code)]`; each `unsafe fn` has a `# Safety` doc + `#![deny(unsafe_op_in_unsafe_fn)]`.
- `#[allow]`: 4, all surgical/justified (CI gates `clippy -D warnings`). No crate-level dead-code suppression.
- `TODO`: 6, all benign (the `voxel.rs` set is feature-gated WIP).

---

## What was verified correct (so the next reviewer can skip it)
- **Geodesy/SSE/Morton/RTIN math**: ECEF↔geodetic round-trips, ENU handedness (`E×N=U`), Cesium SSE formula + FOV/zero-distance clamps, 2D/3D Morton spread masks (21- and 32-bit), implicit `bit_index` level offsets, the martini/RTIN parent-error split and CCW winding flip — all derived and confirmed.
- **Determinism** (core crates): no wall-clock in the step (frame Δ feeds only the step accumulator, with a cross-frame-rate equality test); no `thread_rng`; all `HashMap`/`HashSet` outputs sorted before emit; NaN sanitized before `partial_cmp`. Structural changes go through controlled phases, never mid-iteration.
- **Architecture law**: no Unreal types in Rust; no Rust→Unreal calls; gameplay truth only in Components/Resources/Systems; **all FFI is world-level/batched** (verified no per-entity ABI call in any C++ loop); FFI buffer ownership is correct (Rust owns/frees, C++ copies out before `destroy`/next `step`); ABI size-locks via `const _`; ABI-version checked at DLL load for both DLLs; GC-root fix for the asset registry is in place.
- **CRC32** matches zlib; `.lst` store encode/decode is bounds-checked + version-gated; COLLADA indexing is safe given `dae-parser`'s parse-time validation.

---

## Prioritized remediation
1. **CR1 + H1** — add `catch_unwind` to the baked + editor FFI lanes (close the UB the docs claim is already closed).
2. **H3** — `bevy_ecs default-features = false` (constitutional violation, likely one line).
3. ~~**H2** — sort the tile-selection render set~~ → **Won't fix** (rendering needs no determinism; the Geo engine is non-authoritative, ADR-0007).
4. **H5 / H6** — single-source vertex count/stride; make `page_bytes` bounds-safe.
5. **H8 / H9** — cap HTTP body size; add request timeouts.
6. **H11 / M1 / M2** — fix the ISM-GC desync; apply actor rotation/scale; wire (or remove) the anim bridge.
7. **H4** — fix the AGENTS.md crate map + ADR list *(done this session)*.
8. **H12 + M21** — extract the shared `geo/` mesh crate (vertex format + interleave + ENU→UE); the single highest-leverage cleanup.
9. **M4 / `bevy_app`** — resolve the orphan crate and the unused workspace dep.

*No files were modified by the audit. Findings are advisory; fixes are tracked separately.*
