# Geo pipeline end-to-end deep audit — 2026-06-09 (evening)

**Scope.** The entire Geo pipeline — request → fetch (ureq + cyper/h3) → cache (disk/geom/flash) →
decode (glTF/Draco) → select → scene ECS → FFI (ABI v12) → C++ render — plus the enrichment lane,
the ADR/plan/audit doc surface, tests/benches, and the vendored cyper. Audited the working tree
(HEAD `0f59017` + the uncommitted hole-clip / h3-lane / ADR-0018 work).

**Method.** A 25-reader Opus workflow (every pipeline stage + congruence/vision/architecture/
hot-path-O/error-path lenses), each finding piped to an independent adversarial Opus verifier
(84 agents, 217 raw findings → 9 dups dropped → 52 refuted → 156 "confirmed"), plus an 8-probe
lifecycle wave (shutdown, PIE re-init, cache versioning, secret hygiene: **all four that ran came
back clean**, with detailed evidence). Mid-session API rate-limiting killed the second probe wave
and the result-persistence path, so the remediation phase was completed **manually**: every finding
acted on (and every "high") was re-verified against the code by hand before any edit.

**The meta-finding.** Hand re-verification refuted **12 of the 18 machine-"confirmed" highs** —
verifiers repeatedly accepted *fabricated evidence* (quoted code that does not exist in the tree:
a `clip.remove(&evicted_key)` line, an unbounded `while` Newton loop, a `(Abi >> 16)` major-only
check, an L1-√3 cull, a `TODO sparse` arm). Machine-verified findings below the "high" bar should
be treated as *leads*, not facts. Everything in §1/§2 of this report was verified by direct read.

---

## 1. Fixed this session (landed + gated)

**Gates: `cargo test --workspace` 0 failures · `cargo clippy --workspace --all-targets -D warnings`
clean · `cargo fmt` · `rebuild-editor.ps1` → C++ build **Succeeded**, editor relaunched.**

### Logic / behavior
- **The one failing test in the tree** (`publish_materializes_clip_boxes_…`): the shared test stub
  decoder emitted a degenerate 1-vertex mesh (32 zero bytes, plus out-of-bounds `indices [0,1,2]`)
  that the (correct, newer) `positive_extents >= 2` footprint guard rightly rejects. Fixed the
  fixture, not the guard: a shared `tri_vertices()` honest triangle (96 B, two positive extents —
  exactly the flat-terrain case the guard admits) across all 8 stub sites; byte-accounting assert
  updated (44 → 108). `geo_world.rs` tests.
- **`graft_external` now enforces `extensionsRequired`** like the root path (fail-loud,
  `StreamError`) — a grafted external tileset could previously smuggle an unsupported required
  extension past the root-only gate. + regression test. `geo_world.rs:3199`.
- **cyper/h3 lane: 60 s overall request timeout** (`compio::time::timeout` around the whole
  exchange), lane-congruent with ureq's `HTTP_OVERALL_TIMEOUT`. QUIC's 30 s idle timeout only reaps
  a *dead* connection — our own 10 s keep-alive holds a stalled-but-alive stream's connection warm
  indefinitely, so a server that stopped sending mid-body could previously hang a fetch (and its
  in-flight slot) forever. Elapsed ⇒ transient `Transport` error (same backoff semantics).
  `cyper_source.rs`.
- **`parse_tileset` validates `asset.version`** against the 3D Tiles 1.x family (fail-loud,
  ADR-0004); dropped the parse-only dead `tileset_version` field. `logi_3dtiles_tileset`.
- **`subdivide_bounds` antimeridian unwrap** for region roots (mirrors `bounding_sphere_ecef`) — a
  wrapped implicit root no longer synthesizes longitudinally inverted children. `logi_3dtiles_tileset`.
- **glTF TRS rotation normalized** (finite/non-degenerate guard → else identity): spec requires unit
  quaternions, exporters drift, and a hostile zero quat would NaN the node matrix. `logi_gltf`.
- **Frustum input clamp + NaN guard**: clamp the *input* angle exactly like
  `logi_geo_lod::tan_half_fov_y` (past π, raw `tan(fov/2)` flips negative → degenerate sliver
  frustum) and return accept-all on non-finite fov/aspect. `frustum.rs`.

### C++ render lane (`LogiBakedGeoTerrainActor`, rebuilt + verified)
- **`ApplyTileClip` records the APPLIED clip count, not the requested one** — with
  `GlobeClipMaterial` unset, the old code left `bWasClip` stuck true and re-ran the base-swap branch
  (a fresh MID acquire) **every poll** for every would-clip tile.
- **Bit-identical clip sets skip the param push** (CRC over the OBB bytes, new `FTileEntry.ClipHash`)
  — a stand-in waiting for more children no longer re-pushes 17 render-thread params per poll.
- **Clip param `FName`s built once** (static table) instead of `FString::Printf` + FName-registry
  round trips ×16 per clipping tile per call.
- **Deleted the dead `FadingOut`/`HoldFadingOut` hold-the-departed-parent subsystem** (zero callers
  — superseded by the Rust texture gate + geometric hole-clip, as `RecycleByKey`'s comment already
  records) incl. the `GlobeParentHoldSec` UPROPERTY and `HoldRemaining` field (ADR-0004).

### Congruence / docs / hygiene (the drift the audit was asked to kill)
- The stale **"HTTP/2 CyperSource, ~4× confirmed"** narrative swept everywhere it survived:
  `logi_3dtiles_io` crate docs (×2 blocks), `from_ion` doc, FFI v11 changelog, `logi_geo_ffi.h`
  `fetch_backend` comment — all now say HTTP/3-over-QUIC via Alt-Svc (h2 bootstrap/fallback) and the
  *measured* framing (cold-connect/RTT win, ~parity warm; adoption structural).
- **`logi_geo_ffi.h` banner is now version-less** ("the number lives in `LOGI_GEO_ABI_VERSION`
  only") — it had rotted v10→v12 once already; this kills the recurrence class.
- `http_agent` keep-alive comment: stale worker cap 32 → the real 64, "above" → "at".
- Cargo.toml `windows-gro` comment now records the **revert** (spiked, null result, deliberately
  absent; do not re-enable without a new measurement) instead of claiming it was kept.
- **`third_party/cyper/PATCHES.md` created** (upstream, why vendored, the two `LogiUE patch:` seams,
  re-vendor procedure).
- `fetch_priority` block comment rewritten to the real math (`gE/d²` × lobe ∈ [0.55, 1.0] × graft
  boost); flash-trim "purely by distance" → distance × size-nudge; stream-thread "two mutexes" →
  three; `MAX_CLIP_BOXES_PER_TILE` docstring now states the honest `4^skip` overflow bound + the
  union-box escape hatch; FFI telemetry accessors carry the documented no-panic convention (×4);
  panic-guard string "logi-terrain" → logi_geo; `LOGI_GLOBE_TILE_FLAG_HIDDEN` documented as advisory
  (our renderer derives the same state locally).
- `make_selected` no longer recomputes `max_axis_scale` per kept tile — the scaled geometric error is
  computed once in `visit` and threaded through (formula single-homed).
- Tests/benches: the loopback A/B spike is `#[ignore]`d (it ran ~400 threads on default `cargo
  test` and asserts only completion); its baked-in "observed" timings labeled spike-era;
  `webpki-roots` moved to dev-dependencies (production cdylib carried a root store only a spike test
  reads); **new v12 read-back validation** in `globe_contract`'s live test (clip pointer/arena/cap/
  pad contracts, count-0 ⇒ NULL).

## 2. Audited and confirmed BY DESIGN (no action — the audit re-derived the rationale)
- **Stream-thread continue-after-panic** (`stream_thread.rs`): explicitly designed + re-evaluated
  2026-06-09 — Geo is non-authoritative; thread-death = silent permanent freeze; `step_panics` +
  rate-limited stderr; the comment already warns the deterministic sim must NOT copy the shape.
- **SingleThreaded Geo executor / reconcile settled-skip** (ADR-0018, intentional).
- **LiveSet drift assert debug-only** — the documented §1E trade (release does not pay O(pool)).
- **Flash-restored tiles re-enter hidden-until-textured** — the anti-pop texture gate, not a bug.
- **`RefreshTileClips`** is steady-state-skipped AND 0⇒0-skipped (the wave-1 "every retained tile
  every poll" claim was wrong on both counts; the real residuals are fixed above).
- **Telemetry FFI accessors guard-free**: atomic loads only; the no-panic convention is documented.
- **Poll is synchronous** on the game thread (Mutex hand-off inside `GlobeStreamer::poll`) — no
  fence needed; `assert_single_consumer` enforces the contract in debug.
- The selector's single path-sort (consolidates two sorts; determinism = future-diff precondition).

## 3. Notable refuted-finding ledger (fabricated/stale evidence — kept so future audits don't re-chase)
ClipChildren evict sweep (the resource is `clear()`+rebuilt every pass); geom-tier cache
OOM-on-truncation (deserializer checks magic/64 MiB cap/exact length, CRC-named files, *and* the
format version is folded into the cache key); io Condvar lost-wake (check+wait under one lock);
preemption double-decrement (owner removes once; late results soft-dropped — documented); FFI
frame_id/arena fence; select L1-√3 cull (**already fixed in a prior session** — the cesium-deep-dive
\#1 item is done); "nearest-four" clip sort (path-order is real and the ffi.h comment was correct);
GLB `chunk_len+8` u32 wrap (checked_add present); silent sparse-accessor arm (upstream `gltf` reader
applies sparse); unbounded geodetic Newton loop (`for _ in 0..100`); h3-lane 4xx-retried-forever
(both lanes share `FetchError::is_transient`); cyper gzip missing (vendored `decompression.rs` +
accept); key-bearing bench panics (bare `expect`s, env-gated); header inset-0.94 paragraph (doesn't
exist); C++ major-only ABI check (exact `!=` match); single-mip BC7 (full mip chain exists);
clip-only `MarkRenderStateDirty` gap (`SetMaterial` dirties; MID params update via proxy);
ADR-0018 "references dead consts" (it's the problem-statement section; the Decision section is
current).

## 4. Open items (verified real; deliberately not landed tonight)
| Item | Where | Why deferred |
|---|---|---|
| Corrupt atlas `?`-aborts the whole tile (geometry discarded too) | `logi_gltf` image decode | Behavior choice: a geometry-only tile stays hidden forever behind the texture gate anyway; splitting tiers needs design. |
| Prev-cut map keyed by `Vec<u32>` paths (alloc per visited node) | `select` (~987) | The known cesium-deep-dive #2; mechanical but touches the traversal core — wants its own measured pass (u64 morton keys exist already). |
| `sse_cull_lru` pays `tan()` per tile | `geo_world.rs:823` | Per-step constant — hoist with `tan_half_fov_y` plumbing; quick win for a perf pass. |
| Idle retry-pending steps republish identical frames under new ids; `set_view` unparks every frame | `stream_thread.rs` | Micro-cost; frame counter is load-bearing for eviction — change only with profiling. |
| Semantic/aux lows: store-open trusts manifest level (>27 aliasing), `read_tile_entry` ignores header id, `content_id` doc overclaim, roundtrip test misses `error_metric`, `aabb_to_sphere` re-implemented in `scene/lib.rs:105`, draco `build.rs` compiles encoder TUs + `rerun-if-changed` misses `.cc` | enrichment/aux lane | Cold offline lane; batch into the next enrichment session. |
| `logi_sim` keyed-RNG pub API has zero callers; `num_cpus` Cargo comment claims false facts | gameplay crate | Outside Geo; the RNG API is likely the concurrent session's WIP — flagged, not deleted. |
| Audit-report staleness sweep + remaining congruence lenses (vision/arch/hot-path-O tables; multi-globe, FFI-input-validation, texture-edge, C++ failure-UX probes) | docs + cross-cutting | The four probes/10 lenses lost to rate-limiting; the highest-risk ones (shutdown, PIE re-init, cache versioning, secrets) DID run and came back clean. |

## 4b. Deferred-items pass (same evening, follow-up session) — recheck → re-evaluate → act

Every deferred list in `Docs/` was re-verified against the (again-moved) working tree before acting.

**Landed (gated as §5):**
- **`Sse`/`CamDistance` components deleted** — the in-code note already called them "a vestigial pair —
  droppable"; nothing read them, and the `Sse` write paid a full `screen_space_error` (incl. `tan()`) per
  live tile per step. Their removal subsumes this report's "hoist the tan()" item: the call is gone, and
  `sse_cull_lru` is now pure distance + LRU stamp (the one load-bearing output).
- **COLLADA `up_axis` now enforced** (Tier-C #3): `load_building` hard-codes the Z_UP conversion, so a
  non-Z_UP document is refused loud (`SceneError::UnsupportedUpAxis`) instead of silently sideways. + test.
- **`load_building` bounding sphere re-uses the single-homed `logi_geo_mesh::aabb_to_sphere`** (it had
  re-implemented DUP-5 inline).
- **FFI view-input sanitation** (closes the dead `ffi-input-validation` probe): `set_view` drops a view
  with any non-finite field, `max_sse <= 0`, or `viewport_height <= 0` (newest-wins ⇒ the last good view
  stays; dev builds assert). The bad shapes were fetch-storm degradations, not crashes.
- **`BakedWorld::open` validates manifest tile levels** (< 28) — `TileId::key`'s 28-bit packing was only
  debug-asserted, so a corrupt manifest could silently alias distinct tiles in release.
- **`logi_semantic::TileId::file_name` doc** corrected (leaf name, no `tiles/` prefix).
- **Multi-globe contract documented** (closes the dead `multi-globe` probe): everything is per-handle;
  the one shared resource is a common `cache_dir` (correct-but-thrashy — distinct dirs recommended).
  Documented on `logi_globe_world_create` in the header.
- **gltf whole-tile-abort on a corrupt image documented as deliberate** (salvaged geometry would bloom a
  flat placeholder OVER the photographic coarse parent — strictly worse).
- **Draco docs corrected** (Tier-C #4): the `draco` cargo feature was never built and is not wanted
  (P3DT is 100 % Draco) — ADR-0009's correction note itself had perpetuated the claim; Milestone-F
  annotated; `build.rs` now states the dir-granular sweep's encoder-TU ride-along is accepted (linker
  drops unreferenced objects).
- **`logi_qmesh` disposition resolved** (Tier-C #1): KEEP — Milestone-F names it the Cesium-World-Terrain
  lane; the crate header now carries the parked status so audits stop re-flagging it.
- **Deferred-optimizations doc reconciled:** its TODO-1 (resident-set scans) was shipped by ADR-0018 →
  entry deleted per the doc's own rule; TODO-2 (path interning) **deferral upheld** and cross-referenced
  to the cesium-dive #2 finding, with cesium-native's state-on-the-node shape recorded as the preferred
  fix if the profiling signal ever appears.

**Refuted on recheck:** the geo_pack roundtrip already asserts `error_metric` survival (roundtrip.rs:144);
`build.rs` rerun-if-changed narrowing is a documented trade; the 2026-06-05 audit's deferred list (DUP-4,
M21-remainder) is settled won't-do-by-design.

**Still open after this pass:** the select prev-cut interning (profile-gated, see the deferred doc); idle
republish/unpark micro-costs (profile-gated); `read_tile_entry` ignoring the in-file header id +
`content_id` doc overclaim (cold-lane polish); the `texture-edges` and `cpp-failure-ux` probes (C++-heavy,
still unprobed); redb/fjall embedded-store spike (research-gated, own ADR); PIE eyeball of the clip
material.

## 4c. PIE artifact follow-up (late evening): the mixed-LOD patchwork + hole-clip re-evaluation

A PIE screenshot (high-altitude oblique view) showed jagged polygon-edged sharp/blur patches, a thin
diagonal gap seam, and same-distance regions at wildly different LODs. Root causes found and fixed:

1. **Axis-aligned clip boxes over-cut tilted tiles.** The hole-clip boxes were the child mesh's
   render-space AABB; far from the ENU origin a tile is tilted (curvature), so the AABB covered far more
   than its footprint — the stand-in was discarded over ground the child does not draw (jagged reveals),
   and the P3DT edge skirt inflated the AABB sideways (the gap ring). **Fixed:** `oriented_clip_box` —
   bounds in the child's own ground frame (geocentric `up` + ground basis). The C++ material's per-axis
   test was already fully general; only the producer was axis-aligned. The skirt now inflates only the
   `up` extent, which never widens the footprint cut — this retires the deferred "clip-edge skirt" item.
2. **Cap-4 in path order under skip-level stand-ins.** While converging at altitude a stand-in's
   immediate rendered descendants are grandchildren (up to `4^skip`); only 4 arbitrary ones got clipped —
   the rest drew coarse-over-fine as persistent blur patches. **Fixed:** cap raised to **8** on both
   sides (selector + `LOGI_CLIP_MAX` + the 8-box HLSL), candidates ordered **largest-footprint-first**
   (`bounds_radius`), so overflow now drops the least-visible slivers.
3. **Publish emitted stale finer tiles the selection no longer wanted.** `sse_cull_lru` LRU-stamps every
   in-range live tile (eviction warmth), and `publish` gated on that stamp — so after the camera receded,
   finer tiles from the previous cut kept rendering (resident + in range) with NO clip pairing possible
   (clips pair only selection tiles): persistent un-clippable fine-vs-coarse z-fight patchwork — exactly
   "the wrong tile rendered for the camera view". **Fixed:** `PublishSet` — publish now draws exactly the
   selection's render + prepare sets (O(selected)); stale tiles stay RESIDENT (instant warm re-entry) but
   no longer draw. Foundation/test worlds without a selection keep the legacy all-resident snapshot.

**Knob map (re-evaluated; all congruent with intent, one to tune):**
| Knob | Value | Meaning / verdict |
|---|---|---|
| `max_sse` (C++ DetailTargetPx, **instance** override) | 4 px | SSE target — aggressive-sharp; remember the `.h` default is inert, the instance carries it. |
| `DETAIL_BAND_M` | 25 km | **THE concentration knob**: full detail only within 25 km of the camera; beyond → coarse shell. At >25 km altitude the WHOLE view is intentionally the coarse shell — if the screenshot's altitude should look sharper, raise this (fetch volume grows ~quadratically). |
| `CULLED_SSE_FACTOR` | 2.0 | Coarse-shell quality = `max_sse × 2` (off-view + beyond-band). |
| `LOD_HYSTERESIS` | 0.15 | Coarsen dead-band — anti flip-flop; sane. |
| `FRUSTUM_MARGIN_RAD` | 0.18 (~10°) | Full-detail margin past screen edge; sane. |
| `MAX_CLIP_BOXES_PER_TILE` / `LOGI_CLIP_MAX` | **8** (was 4) | Hole-clip cap; per-pixel cost rides stand-ins only. |
| `FETCH_LOBE_MIN/SPAN`, `gE/d²`, `CONCENTRATE_BOOST` | — | Fetch ORDER only (convergence pattern, not the converged result). |
| flash/disk budgets, in-flight cap, C++ build/evict/texture budgets | instance | Convergence SPEED; check the L1 telemetry (`flash/disk/geom_cache_stats`) + `failed_counts`/`refine_failures` for stuck-blurry regions before tuning. |

**Verification:** Rust workspace tests green (41 scene incl. 2 new oriented-box tests + the rewritten
publish-clip test), clippy `-D warnings` clean, editor rebuilt. **PIE eyeball pending** — the three fixes
target the screenshot's three artifact classes (jagged reveals + gap seam → oriented boxes; blur-over-sharp
patches → cap 8 largest-first; persistent sharp patches over a coarsened cut → selection-driven publish).

## 5. Verification status
- Rust: full workspace tests green (incl. the previously-failing clip test + 2 new tests), clippy
  `-D warnings` clean, fmt applied.
- C++: `rebuild-editor.ps1` → Rust staged, `UnrealEditor-LogiBevyRuntime.dll` build **Succeeded**,
  editor relaunched (20:03). Runtime PIE eyeball of the clip-material path still pending (as before).
- A concurrent session was active in this tree during the audit (files reformatted/extended
  mid-flight); all edits here were re-read-before-write and re-gated after.
