# Geo Pipeline Congruence + Quality Audit — 2026-06-09

Audit of the request→render geo pipeline of the Rust (`bevy_ecs`) + Unreal Engine 5.8 real-world
terrain streamer. The headline focus is **congruence** — whether the docs (ADRs / plans / research),
the in-tree comments, and the code actually agree. **48 verified findings** in total, dominated by
documentation/comment drift that survived in-place refactors (the H1 priority-heap rework, the H5
wall-clock-deadline split, the BC1→BC7 texture migration, the "Terrain"→"Geo" rename, the
ADD-refinement landing, the L2 cache-trim rework, and the FFI ABI bumps to v10). No load-bearing ABI
or layout invariant is wrong; the structural offset asserts hold. The risk is maintainer-facing:
comments and docs that actively mislead about control flow, format, eviction policy, and crate
topology. Two `high`-severity congruence items mislabel runtime behavior (a 403 that is *recoverable*
documented as terminal; BC1 labels on BC7 bytes), and one `high` smell is an entire decoder crate
(`logi_qmesh`) that advertises itself as on the render path while being unreachable.

## Breakdown

| Category | high | medium | low | Total |
|----------|------|--------|-----|-------|
| Congruence | 4 | 14 | 14 | 32 |
| Logic | 0 | 1 | 2 | 3 |
| Smell | 1 | 4 | 5 | 10 |
| Quality | 0 | 0 | 1 | 1 |
| Severity totals | **5** | **19** | **22** | **46** |
| Other (counted under stages below) | | | | **2** |
| **Grand total** | | | | **48** |

> Note: two `scene-cache-graft-implicit` congruence findings (the FlashCache struct-doc and the
> ResidentSet maintainer list) restate two `scene-schedule` findings at the same lines from the
> cache-stage perspective; both are kept here as separate verified entries, hence 48 entries vs the
> 46-row severity tally above.

---

## Congruence (docs ↔ comments ↔ code) — headline section

### Stage: scene-schedule

- [medium] `geo_world.rs:711-713` (evict header) vs `:762-783` (body) — The `evict` header says it
  resets "the oldest (by `LastSeen`, not seen this frame)", but the body evicts by a distance-primary
  score `dist * (1.0 + EVICT_STALENESS_FACTOR * stale)` (line 769) partitioned via
  `select_nth_unstable_by` over field `.3` (781) — explicitly *not* LRU; `last_seen` only feeds the
  `cache.insert` deposit timestamp (790). The inline comment (760-761) already refutes the header.
  (fix: reword the header to "`distance`-primary eviction (farthest + stalest first, score =
  distance·(1+FACTOR·staleness)), not pure LRU".)
- [low] `geo_world.rs:870-871` — Stale future-tense `GeoWorld` struct doc: "The tileset traversal
  (`select`), fetch drain, and published-frame mailbox wire in next…". All three are implemented
  (`drain_raw` / `select_request` / `GeoMailbox`/`publish`), and it names a `select` fn that does not
  exist. (fix: present-tense description naming `select_request` / `drain_raw` / `GeoMailbox`.)
- [low] `geo_world.rs:157-158` — Stale `TileDecodeFn` note: "Production wires `logi_gltf::decode_glb` +
  the ENU rebase (the next increment)". `production_decoder` (169-174) already wires exactly this and
  `from_tileset` uses it. (fix: drop "(the next increment)".)
- [low] `geo_world.rs:2743` — Stale section header "Integration / test surface (the traversal + drain
  wire to these next increment)". The production traversal/drain use `request_one`/`place_decoded`
  directly; `acquire_tile` (2747) is `pub` but only called under `#[cfg(test)]`. (fix: rename to
  "Test/integration surface (used by the test module; the production traversal uses
  request_one/place_decoded)".)
- [low] `geo_world.rs:188-190` (ResidentSet doc) vs `:2057-2082` (try_restore_from_geom) — The doc
  enumerates Resident-transition maintainers but omits `try_restore_from_geom`, which promotes via the
  shared `place_decoded` tail (insert at 2038). No correctness bug; the enumeration is incomplete.
  (fix: add `try_restore_from_geom`, or generalize to "every `place_decoded` caller plus
  `evict`/`prune_cold_grafts`".)
- [low] `geo_world.rs:340-342` (FlashCache struct doc) vs `:388-420` (trim) — Struct doc says trim
  "drops the coldest entries (lowest `last_seen`)" via an "O(n log n) sort", but the reworked body
  scores by distance·staleness (402) and runs `select_nth_unstable_by` (O(n) + O(cut·log cut)); the
  method doc (388-390) already says so. (fix: update struct doc to "drops the farthest+stalest entries
  (distance·staleness score) … in O(n) + O(cut·log cut)".)
- [low] `Docs/Research/ECS-Multithreading-for-Geo.md:7` and `ADR-0005:25` vs `Cargo.lock:113` /
  `geo_world.rs:2683` — Version drift: `Cargo.lock` resolves `bevy_ecs` to 0.19.0-rc.2 and the
  `entity_count` comment (2683) targets "bevy 0.19", but the research doc and ADR-0005 still pin
  0.18.1. Verified constraints carry over, so this is a stale pin only. (fix: bump the 0.18.1
  references to 0.19, noting constraints carry over.)

### Stage: scene-request-sched

- [high] `geo_world.rs:1644` — `fail_fetch` doc lists HTTP 403 under "Terminal — give up
  IMMEDIATELY", but code (1686) classifies 401||403 as `auth_expired` and takes the **recoverable**
  path (flag `session_refresh_pending`, brief backoff, `gave_up=false`) at 1692-1697 *before* the
  terminal `else` is reached — so a 403 is never given up. The doc also omits the auth-expired /
  P3DT-session-refresh branch entirely. Actively misleading about control flow. (fix: add an
  "Auth-expired (401/403)" bullet for the session-refresh+backoff path and remove 403 from Terminal.)
- [medium] `geo_world.rs:268` — `RETRY_BACKOFF_BASE` doc contradicts itself: the head says backoff is
  "in advance-steps (`now_tick`)", but the same comment's tail (272-275) correctly says wall-clock;
  the const is `Duration::from_millis(24)` added to an `Instant` (1689). H5 left the stale head. (fix:
  delete the "in advance-steps (`now_tick`)" clause from line 268.)
- [medium] `geo_world.rs:1649` — `fail_fetch` Transient bullet says "not re-fetched until `now_tick +
  delay`", but `now_tick` is not a param/var of `fail_fetch`; the deadline is `now + delay` where
  `now = s.now_instant` (wall-clock), gated by `s.now_instant < t` (1904). (fix: change `now_tick +
  delay` to `now_instant + delay`.)
- [medium] `geo_world.rs:1730` — `select_request` doc says it "Reuses `select_tree`.", but no
  `select_tree` exists; the call at 1770 is `select_tree_implicit`. (fix: change `select_tree` to
  `select_tree_implicit` on line 1730.)
- [medium] `geo_world.rs:934` — The `sel_state` field doc says the prior result is "fed back as
  `select_tree`'s `prev_state`", but the fn is `select_tree_implicit` (called at 1770). Param name
  `prev_state` is correct. (fix: change `select_tree`'s to `select_tree_implicit`'s on line 935.)
- [medium] `geo_world.rs:2667` — `has_pending_retry`'s doc calls the deadline `frame`-keyed
  ("advances `frame` to its retry/cooldown deadline"), but after H5 the deadlines live in
  `next_retry: HashMap<TilePath, Instant>` (916) and `last_session_refresh_at: Option<Instant>`, gated
  by `now_instant` (1904) — wall-clock Instants reached by time elapsing, not by `frame`. (fix: replace
  "`frame`-keyed retry deadline" with "wall-clock retry deadline (Instant)".)

### Stage: scene-cache-graft-implicit

- [medium] `geo_world.rs:340-342` — FlashCache struct rustdoc still describes the pre-L2 trim:
  eviction "drops the coldest entries (lowest `last_seen`)" via an "O(n log n) sort". The body (402)
  scores by distance-primary `distance * (1 + FACTOR*stale)`, uses `select_nth_unstable_by` (O(n) avg
  + O(cut log cut) on the overflow prefix only), and its method doc (388) says "farthest + stalest".
  (fix: rewrite to "evicts the farthest+stalest entries (distance-primary score,
  EVICT_STALENESS_FACTOR) via an O(n)-avg select_nth partition of only the overflow prefix".)
- [medium] `geo_world.rs:304-305` — `GraftRecord` doc says `graft_external` "saves exactly the four
  Tile fields it overwrites", but the struct has **six** `saved_*` fields (307-316:
  transform/content/contents/children/geometric_error/refine) and `graft_external` overwrites all six.
  (fix: change "the four Tile fields" to "the six Tile fields" (transform, content, contents, children,
  geometricError, refine).)
- [medium] `geo_world.rs:46-47` — `GraftRestore` type-alias doc lists a 5-element tuple "(path,
  transform, content, contents, children)", but the actual alias (48-56) is a **7-tuple** adding
  `geometric_error: f64` and `refine: Option<Refine>`, constructed at 2517-2525, destructured at 2556.
  (fix: update the doc tuple to "(path, transform, content, contents, children, geometric_error,
  refine)".)
- [medium] `geo_world.rs:919` — `subtree_cache` field doc says "(Grows over a session … LRU it if it
  ever matters.)", implying unbounded. The cache is **already** bounded FIFO past `MAX_SUBTREE_CACHE`
  via `subtree_cache_order` (921-924) + `cache_subtree()` (985-990); module comment (251-253) confirms
  it. (fix: note that `subtree_cache_order` FIFO-evicts oldest-first past `MAX_SUBTREE_CACHE`.)
- [low] `geo_world.rs:188` — ResidentSet doc lists "decode (via NewResidents), restore_cached, evict,
  prune_cold_grafts", but the Free→Resident insert now lives in shared `place_decoded` (2038), called
  by both `restore_cached` (2008) and `try_restore_from_geom` (2080, the persisted geom tier); the
  geom-tier site is unlisted. (fix: replace `restore_cached` with `place_decoded` (warm flash +
  geom-tier restore).)

### Stage: select-frustum

- [high] `logi_3dtiles_select/src/lib.rs:18` (and omission at `:8`) — Module docstring claims ADD
  refinement is still deferred ("warned + treated as REPLACE"), but ADD is fully implemented and
  unit-tested: `is_add` at 702, the ADD self-emit block 709-722, the empty-refine ADD branch 750-755,
  kick gated off for ADD at 788, `ancestor_meets_sse` gated off at 816-817, and passing tests at 1133
  and 1158. Grep confirms no `warn` / no "treat as REPLACE" code; line 8 also omits ADD. Contradicts
  code, ADR-0009:58, and tests. (fix: rewrite lines 8 and 18 to state ADD is implemented (per-tile
  mode, kick/fallback gated off for ADD) and tested; delete the "deferred" sentence.)
- [medium] `logi_3dtiles_tileset/src/lib.rs:87-88` (consumed by `select/lib.rs:345-356`) — The doc on
  `Tile::region_sphere_ecef` says it "Returns `None` for a non-region volume", but the signature
  returns a tuple (never Option) and the body memoizes unconditionally with no volume-type branch. The
  volume discrimination lives in the caller `tile_bounding_sphere_ecef`, which only invokes this inside
  the `Region(_)` arm. (fix: replace the "Returns `None`" sentence with a note that this method is only
  called for region volumes.)
- [low] `logi_3dtiles_select/src/lib.rs:109` and `:1069` — Both say an empty `prev_state` map means
  "no cross-frame kick", but the kick fires cold (`kick = !is_add && !sub.all_renderable &&
  !sub.any_rendered_last_frame`, 788; test at 1315 runs `&cold()` and asserts the root kicks). An
  empty map disables only the state-carried anti-pop; line 369 calls it the "cross-frame anti-pop"
  kick. (fix: reword both to "no hysteresis, no cross-frame anti-pop state".)
- [low] `ADR-0009:57-58` vs `logi_3dtiles_select/src/lib.rs` (whole) — ADR-0009 says the traversal
  port covers cesium-native's `skip-LOD`, but the select crate implements no skip-LOD path (grep for
  `skip.?LOD|skip_lod|skipLevel|forbidHoles|loadSiblings` → none); the only LOD-deferral is the
  show-coarse-until-complete kick (782-807). (fix: add one line noting skip-LOD was intentionally not
  ported — the kick is the chosen alternative.)

### Stage: io-fetch-cache

- [low] `logi_3dtiles_io/Cargo.toml:16` — The flume rationale comment says flume does "request fan-out
  to N workers, results drained on the owner thread", but the H1 priority-heap rework removed flume
  from the request path. flume now has one use site (`lib.rs:883` `flume::unbounded::<Loaded>()`,
  results only); requests go through `PendingQueue { heap: BinaryHeap<PrioReq> }` + Mutex + Condvar
  (848-851, 903), and `lib.rs:846-847` itself says the heap "Replaces the old flume FIFO". (fix: change
  the comment to "MPMC channel draining completed fetches (Loaded) from the worker pool to the owner
  thread"; drop the stale request-fan-out clause.)

### Stage: tileset-implicit

- [medium] `Docs/Plans/Milestone-F-3DTiles-Rust-Stack.md:314` (also `:57-58`) + `select/src/lib.rs:18-19`
  — Docs still describe ADD refinement as deferred / "warned + treated as REPLACE", but the code fully
  implements it (`tile.refine_or(parent_refine)` at 562 feeds `facts.refine`, consumed at 702 with real
  ADD branches 709/715/750-755/786-788/816-819/821-826; tests at tileset:355-356). A stale clause also
  survives in the select crate's own module doc (18-19). ADR-0009 is clean. (fix: update all three —
  Milestone-F:314, §2 "Out of scope" :57-58, and select module doc :18-19 — to "ADD refinement
  implemented (REPLACE/ADD per-tile, inherited)".)
- [low] `logi_3dtiles_tileset/src/lib.rs:130` (vs body `:147-156`, test `:325/:347`) — The `Content`
  doc says external-tileset content is detected "when the URI ends in `.json`", but the authoritative
  `is_tileset_uri` (147-156) strips query/fragment first and judges the path extension, so
  `sub/tileset.json?v=2` is classified as a tileset (test asserts `is_external_tileset()==true` at
  347). (fix: reword to "when the URI path (query/fragment stripped) has a `.json` extension".)

### Stage: decode-gltf-draco

- [medium] `ADR-0009:63-66` and `Milestone-F:141-147` vs `logi_gltf/Cargo.toml:19-21` +
  `logi_draco/Cargo.toml` + `logi_gltf/src/lib.rs:121` — Both docs say the Draco decode shim lives
  *inside* `logi_gltf`, but it was extracted to a standalone `logi_draco` crate that `logi_gltf` only
  depends on. The plan's `decode_draco(bytes, attrs) -> {pos,nrm,uv,idx}` API does not exist; the real
  entry is `logi_draco::decode_mesh(data, &DracoAttributeIds, DecodeLimits)` (logi_draco:214, called at
  logi_gltf:121). (fix: update both docs to say the shim lives in `logi_draco` (a dep of `logi_gltf`)
  with API `logi_draco::decode_mesh(...)`.)
- [medium] `ADR-0009:64-65` + `Milestone-F:218,266` vs `logi_gltf/Cargo.toml` + `logi_draco/Cargo.toml`
  — Docs say Draco is gated behind a `draco` cargo feature so a no-Draco build compiles, but no such
  feature exists (grep `feature = "draco"` → zero matches; `logi_draco` is a plain dep at
  `logi_gltf/Cargo.toml:21`; the KHR_draco branch at 99-138 has no cfg gate). Draco is unconditionally
  vendored. (fix: either remove the feature-gate claims, or actually add an optional `draco` feature +
  `#[cfg(feature="draco")]` gate to match the docs.)
- [low] `logi_gltf/src/lib.rs:25-26` — Doc refers to the rebase helper by the cross-crate path
  `logi_3dtiles_scene::rebase_gltf`, but that fn is `pub(crate)` (`streaming.rs:140`), so the symbol
  path does not resolve from outside the crate. (fix: drop the unresolvable qualifier; note the
  interleave is fused into `logi_3dtiles_scene`'s crate-internal ENU rebase.)

### Stage: decode-collada-qmesh

- [medium] `ADR-0009:51,61` vs `logi_qmesh` (no callers) — ADR-0009:51 lists `logi_qmesh` among built
  crates and :61 names "quantized-mesh-1.0" as part of the in-Rust decode stack, implying a live decode
  lane. No crate depends on `logi_qmesh`; the implemented path decodes glTF/3D-Tiles via Cesium ion
  (`ffi.rs:533` `GeoWorld::from_ion`). (fix: add a one-line note that the quantized-mesh lane is
  implemented-but-not-yet-wired (planned/optional).)
- [medium] `logi_collada/src/lib.rs:4-5,26` vs `logi_3dtiles_scene/src/lib.rs:65-96` (load_building) —
  `logi_collada` decodes and reports `up_axis` with the explicit promise that "the geodetic/handedness
  mapping to ECEF happens at placement time, not here", but the only placement consumer
  `load_building` hardcodes a fixed Z-up→Unreal-LH conversion (negate Y at 77/79, flip V at 80) and
  never reads `mesh.up_axis` — a Y-up/X-up COLLADA doc would be silently mis-oriented; `up_axis`/
  `UpAxis` are dead at the consumer. (fix: in `load_building`, branch on `mesh.up_axis` to apply the
  correct up-axis→Z-up rotation before the handedness flip, or document/assert Z_UP-only input.)

### Stage: geospatial-rebase

- [low] `logi_geospatial/src/lib.rs:107-109` — The geocenter-sentinel doc for
  `cartesian_to_cartographic` gives a geometrically wrong rationale: it says the geocenter is "one
  semi-minor axis below the prime-meridian/equator surface point", but that surface point (lon=0,
  lat=0) maps to ECEF `(SEMI_MAJOR, 0, 0)` (in-file test at 213-218), so its distance from the
  geocenter is one **semi-major** axis. The returned `-self.radii.z` sentinel is harmless; only the
  justification is wrong. (fix: reword to "an out-of-range negative height chosen as a degenerate-input
  sentinel".)

### Stage: ffi-boundary

- [high] `logi_geo_ffi/include/logi_geo_ffi.h:13-22` (vs same file `:32`, and `lib.rs:46-53`) — The
  header's top-of-file ABI banner is headlined "ABI version 8" and documents only v7/v8/v9 with no v10
  entry, yet the binding `#define LOGI_GEO_ABI_VERSION 10u` (32) and `lib.rs:53` are 10. The banner
  never describes the v9→v10 additions (cache-effectiveness telemetry + persisted decoded-geometry
  tier: `logi_globe_world_flash_cache_stats`/`_disk_cache_stats`/`_geom_cache_stats`/`_warm_cache_bytes`).
  The version banner self-contradicts the load-bearing `#define` within one file. (fix: change the
  banner headline to "ABI version 10" and append a v10 paragraph mirroring `lib.rs:46-52`.)
- [medium] `logi_geo_ffi/src/ffi.rs:1-3` — The present-tense module doc says the C-ABI world handle is
  an opaque `*mut LogiTerrainWorld`, but no such type exists (grep matches only this comment + the v2
  changelog at `lib.rs:23`). The real handles are `LogiBuilding` (141) and `LogiGlobeWorld` (299); the
  file itself (91-94) says the old streaming terrain world was retired. The comment also cites
  `runtime-model.md §3.1`, which does not exist. (fix: rewrite to reference `LogiGlobeWorld`/
  `LogiBuilding`, drop `LogiTerrainWorld`, remove the dead `runtime-model.md §3.1` citation.)
- [low] `logi_geo_ffi/src/ffi.rs:8` — The `LogiStatus` doc says the enum matches `logi_geo.h`, but the
  header is `logi_geo_ffi.h` (there is no `logi_geo.h`; every other reference in the file — 28, 346,
  353, 411 — says `logi_geo_ffi.h`). (fix: change `logi_geo.h` to `logi_geo_ffi.h` on ffi.rs:8.)

### Stage: cpp-render-actor

- [high] `LogiBakedGeoTerrainActor.h:428` (+ `.h:420-422,431-434,509-519,173` and
  `.cpp:153,695,724,727,729,784,819,899` + includes `21,23`) — BC1/PF_DXT1 comments survived the
  BC1→BC7 migration and now mislabel the on-wire format. The pipeline decodes RGBA8 and compresses to
  BC7/PF_BC7 (`.cpp:115 CompressRgbaToBC7`, `:150 CompressBlocksBC7`, `:835 CreateTransient(...,PF_BC7)`).
  Worst case: `.h:428` "Mips … tightly packed BC1/PF_DXT1" directly contradicts its sibling `.h:441`
  "tightly packed BC7/PF_BC7". (fix: replace stale labels with BC7/PF_BC7 at the listed sites; leave
  the contrastive BC7-vs-BC1 explanations at `.cpp:106,108,149,830` unchanged.)
- [medium] `logi_geo_ffi/include/logi_geo_ffi.h:13` (changelog ends `:22`) — The version prose opens
  "ABI version 8" and stops at v9, but `#define LOGI_GEO_ABI_VERSION 10u` (32), `lib.rs:53`, and the
  v10 telemetry entry points are already declared in this same header (200-203). The `#define` is
  correct, so this is stale lead-in prose. (fix: change line 13 to "ABI version 10" and append a v10
  changelog bullet mirroring `lib.rs:46-52`.)
- [medium] `ADR-0008:47` and `:70` — ADR-0008 names the flag `LOGI_TERRAIN_NANITE_LANE`, but after the
  "Geo" rename the macro is `LOGI_GEO_NANITE_LANE` (`Build.cs:109`; guards at `.cpp:34,365,1438`).
  ADR-0008:70 also cites the retired `LOGI_TERRAIN_RUNTIME_NANITE_BUILD`. ADR-0007 has a rename banner;
  ADR-0008 does not. (fix: rename to `LOGI_GEO_NANITE_LANE` at :47 and fix the :70 retired-flag
  reference.)
- [low] `LogiBakedGeoTerrainActor.cpp:74,80,94` (and mip0 comment `:187`) — The downsample helper is
  named/commented for BGRA8 ("B, G, R, A") but the decode now feeds RGBA8 (`.cpp:177
  GetRaw(ERGBFormat::RGBA,...)`, invoked on the RGBA chain at 202). Body is channel-agnostic, so no
  bug. (fix: rename `DownsampleBoxBGRA8` → `DownsampleBox` and reword 74/94/187 to "RGBA8 /
  channel-agnostic per-channel average".)
- [low] `LogiBakedGeoTerrainActor.h:173` — The `TileBuildsPerStep` UPROPERTY doc (a Details-panel
  description) still says the off-thread work is "BC1" ("the decode + mip + BC1 run OFF the game
  thread"); the off-thread compress is BC7 (`.cpp:209-216 CompressRgbaToBC7` per mip). (fix: change to
  "…+ BC7 compress run OFF the game thread".)

### Stage: cpp-render-mesh

- [low] `ADR-0007:62` vs `LogiTileRenderShared.h:39` / `Build.cs:109` — ADR-0007:62 names the Nanite
  gate `LOGI_TERRAIN_RUNTIME_NANITE_BUILD`; the shipped gate is `LOGI_GEO_NANITE_LANE` (header:39,
  Build.cs:109,114,123). Mitigated: ADR-0007's banner (:3) disclaims pre-Geo-rename names and ADR-0008:70
  retires the cited flag, but the identifier still matches no shipped symbol; ADR-0008:66 cites a third
  name `LOGI_TERRAIN_NANITE_LANE`. (fix: replace `LOGI_TERRAIN_RUNTIME_NANITE_BUILD` at ADR-0007:62 with
  a pointer at ADR-0008/0013 and the actual `LOGI_GEO_NANITE_LANE`; fix ADR-0008:66.)
- [low] `ADR-0007:60-64` vs `ADR-0013:35` / `ADR-0008:65` + `LogiTileMeshComponent.h:38-39` — ADR-0007:60-61
  frames the `FPrimitiveSceneProxy` lane as a throwaway "debug" lane and Nanite as "production".
  Shipped reality is inverted: `FLogiTileSceneProxy` is the default runtime path (ADR-0013:3,35; header
  carries no "debug" qualifier) and the Nanite lane is editor-only (ADR-0008:63-66). Standard ADR
  immutability (0007 amended forward by 0008/0013), hence low. (fix: add a one-line forward pointer at
  ADR-0007:60 noting the reframe.)

---

## Logic

- [medium] `geo_world.rs:1935` (scene-request-sched) — Two comments claim "One preemption per step"
  (1935) and `lowest_priority_inflight` runs "at most once per step (at the cap)" (2268). Both are
  wrong: the at-cap preempt block (1942-1949) sits **inside** the `for (score, lt) in &ranked` loop
  (1894) and on a winning tile calls `preempt_inflight` then falls through to `request_one` and
  continues — it only `break`s when a tile does *not* win. So preemption can fire once per qualifying
  ranked tile, and `lowest_priority_inflight` runs O(ranked) times. No correctness bug (it self-limits)
  but the stated cost misleads. (fix: reword both to "at most once per qualifying ranked tile" (cost
  O(ranked × in_flight)).)
- [low] `geo_world.rs:453-456` (scene-cache-graft-implicit) — `MAX_GEOM_RESTORES_PER_STEP` doc says it
  "bounds the SYNCHRONOUS disk reads", but the cap (incremented only on success at 1926-1929) does not
  count a `get_raw` hit whose `deserialize_geometry` fails (2075-2078 returns false uncounted).
  `get_raw` (`io/lib.rs:705`) CRC-filters bit-rot/truncation to a miss and `GEOM_FORMAT_VERSION` is
  keyed in, so the only residual trigger is a CRC-valid, same-version, magic-mismatched blob — rare,
  but in that window full file reads escape the cap. (fix: increment a read-attempt counter whenever
  `try_restore_from_geom` performs the `get_raw` read regardless of deserialize success, or soften the
  doc to "caps successful restores" and note CRC pre-filtering.)
- [low] *(see also the `select-frustum` empty-`prev_state` "no cross-frame kick" item — filed under
  Congruence above as it is a comment-vs-code mismatch, not a logic defect.)*

---

## Quality / Smells

- [high] `logi_qmesh/src/lib.rs:1-895` (entire crate) (smell) — The entire `logi_qmesh` crate is
  unreachable from any runtime path. A workspace-wide grep for every public symbol (`parse`,
  `QuantizedMeshTile`, `to_ecef_mesh`, `tile_bounds_geodetic`, `EcefMesh`, `GeodeticBounds`,
  `IndexBuffer`, `zigzag_decode`, `oct_decode`) returns only intra-crate matches plus the workspace-
  member + Cargo.lock lines; no crate depends on it. The live lane is glTF-via-ion
  (`logi_geo_ffi/src/ffi.rs:533` / `:562`); `stream_globe.rs` has zero qmesh refs. Yet the module doc
  (1-7) advertises it as on the request→render path — ~830 lines of maintained-but-dead decoder. (fix:
  either wire qmesh into a real Cesium-World-Terrain decode lane, or move it behind a feature / mark it
  experimental in its module doc.)
- [medium] `geo_world.rs:1665` (scene-request-sched) (smell) — The 4-line "reset a pool slot to Free"
  sequence (`TileState::Free`; `FetchedBytes.0=None`; `ResidentMesh.0=None`; `MultiContent.0.clear()`;
  `FreeList.push`) is hand-copied byte-identical at four sites (`fail_fetch` 1665-1669, `cancel_stale`
  2255-2259, `preempt_inflight` 2294-2299, `reconcile_index` 2461-2465) plus a guarded variant in
  `prune_cold_grafts` 2580-2593 (also removes from ResidentSet). No shared helper exists. Adding a
  sixth component or forgetting a clear means editing five places. (fix: extract a `fn free_slot(&mut
  self, e: Entity)` (with opt-in ResidentSet removal) and call it at all five sites.)
- [medium] `LogiBakedGeoTerrainActor.cpp:1785` and `:1851` (cpp-render-actor) (smell) — The per-tile
  texture-retry cap is defined twice as an independent `constexpr uint8 MaxTextureAttempts = 4;` (once
  in `ReconcileGlobeFull`, once in `DriveTileTextures`); both gate `Entry->TextureAttempts`, so
  changing one drifts from the other. (`MaxUploadRetries=3` at :935, `MaxHeld=512` at :2002 are each
  single-use.) (fix: hoist a single file-scope `static constexpr uint8 GMaxTextureAttempts = 4;`
  referenced from both sites.)
- [low] `geo_world.rs:1812` (scene-request-sched) (smell) — In the subtree-enqueue block the local
  `now` holds `view.frame` (a u64 step tick) and is passed as the `now_tick: u64` arg to
  `source.request` (1823), but elsewhere `now` denotes a wall-clock `Instant` (e.g. `let now =
  s.now_instant;` 1689). Reusing `now` for a frame tick is misleading after the H5 split where the
  tick-vs-Instant distinction is load-bearing. (fix: rename the local on 1812 to `now_tick` (or
  `frame`).)
- [low] `geo_world.rs:2533-2570` (scene-cache-graft-implicit) (smell) — `prune_cold_grafts` Phase 4
  builds `stale_inflight` (2533-2540) without the `content_index` filter that sibling `cancel_stale`
  uses, then unconditionally calls `in_flight_paths.remove(path)` for every stale id (2569), including
  1.1 additional contents (`content_index>0`) which `request_extra` never inserts (NB at 2396-2397).
  Harmless today (remove of an absent key is a no-op) but gratuitous drift from `cancel_stale`'s guarded
  teardown (2245-2248). (fix: carry `content_index` and guard the remove with `if content_index == 0`,
  or add a one-line comment that extras share the primary's path.)
- [low] `logi_geo_lod/src/lib.rs:48-54` (geospatial-rebase) (smell) — `should_refine` silently floors
  the caller's distance with `distance.max(1.0)` (51) — an undocumented magic value with no comment on
  units/purpose, even though the `screen_space_error` it wraps documents its own near-zero guard
  (`distance <= 1e-6 => INFINITY`, 13-15). The 1.0 floor means `should_refine` can never reach that
  INFINITY branch, so a reader sees two unexplained zero-distance conventions in adjacent functions.
  (The "duplicated in three callers" claim is inaccurate — only `logi_semantic/world.rs:228` also uses
  `.max(1.0)`; select and `geo_world.rs:635` floor at `.max(0.0)`.) (fix: add a one-line comment that
  it is a conservative 1-metre world-unit floor so a near-camera tile yields a large-but-finite SSE.)
- [low] `LogiTileMeshComponent.cpp:226` (GetDynamicMeshElements) vs `:305-306` (GetViewRelevance)
  (cpp-render-mesh) (smell) — `GetViewRelevance` sets `bDynamicRelevance = bWireframe`, so
  `GetDynamicMeshElements` only runs for the wireframe view, yet line 226 keeps a `bWireframe ?
  WireframeMaterialInstance : Material->GetRenderProxy()` ternary and 235 `Mesh.bWireframe =
  bWireframe`. The non-wireframe arm is unreachable, contradicting the comment at :261. Harmless
  defensive code. (The "duplicates DrawStaticElements" claim is overstated — the dynamic batch uses a
  per-frame primitive UB, the static one uses VertexFactoryUserData + LODIndex=0.) (fix: drop the dead
  arm — set `MaterialProxy = WireframeMaterialInstance` and `Mesh.bWireframe = true` unconditionally,
  optionally early-return when `!bWireframe`.)
- [low] `logi_qmesh/src/lib.rs:615-618` (decode-collada-qmesh) (quality) — Self-contradictory test
  comment: line 615 claims the run "also errors", but the code asserts
  `decode_indices_hw(...).is_ok()` and 617 says it "stays valid". Line 616 describes `[0,0,0,0,0]` but
  the code uses `[0u16,0,0,0,4]`. Tracing `[0,0,0,0,4]` at vc=4 yields in-range indices → Ok, matching
  the assertion. Comment-only defect in test code. (fix: rewrite the comment to say this is the
  in-range/valid back-reference case (delete "also errors", fix the array to `[0,0,0,0,4]`), or add the
  real OOB case it claims to test.)
- [medium] `geo_world.rs:101-107` (defs), `:634-635` (writes), `:767` (recompute) (scene-schedule)
  (smell) — `Sse` and `CamDistance` are computed and written for every live tile every step by the
  `sse_cull_lru` fused hot pass, but neither is read by any production path. Verified by grep: the only
  references are the struct defs, the spawn-init `Sse(0.0)`/`CamDistance(0.0)` (1206-1207), the two
  writes (634-635), and one test assertion (3001, reads `Sse` only); `CamDistance` has zero readers.
  `GeoRenderTile` (545-557) never reads them, the FFI crate has none, and `evict` even **re-computes**
  the distance itself (767) rather than reading the `CamDistance` just written. The length/sqrt (633) +
  `screen_space_error` (635) + two f32 writes are wasted hot-loop work; the doc-comments (101,105) lack
  the "not used by streaming" caveat that the analogous `Depth` component (76-77) carries. (fix: drop
  `Sse`/`CamDistance` from the pool and the `sse_cull_lru` query (compute inline where consumed), or if
  kept for debug, fold distance into `evict`'s use and add the "debug/telemetry-only" caveat.)

---

## Clean stages

The following pipeline stages produced **zero findings** in this audit and read as congruent between
docs, comments, and code:

- **render-publish / FFI delta-reconcile (Rust side)** — the v8 O(Δ) delta machinery (`frame_id` skip,
  added/removed lists, `tile_by_key`) carried no doc/comment drift of its own; the v10 banner gap is a
  C++/header issue filed under `ffi-boundary` and `cpp-render-actor`.
- **geospatial core math** — apart from the single cosmetic sentinel-rationale comment at
  `logi_geospatial:107-109`, the ECEF/cartographic conversions and their in-file tests
  (`equator_prime_meridian_is_semi_major_on_x`) agree with the code.

No stage produced a verified ABI-layout, offset-assert, or runtime-correctness defect: the structural
contract (the `offset_of!` / `size_of` locks referenced by the header preamble) holds; every finding
above is documentation, comment, dead-code, or duplicated-constant drift rather than a behavioral bug.
