# Geo Pipeline Microscopic Audit — 2026-06-10

> **Remediation status (landed same day, build- and test-verified):** C1 (Content-Length pre-check),
> C2 (cyper heap tombstone purge), C3 (MID pool keyed by parent), C4 option (c) (in-place proxy
> material **and visibility** updates — engine source confirmed `SetVisibility` also recreated the
> proxy, so both paths now use `SetTileMaterial`/`SetTileVisible` over `SetIsDrawn_RenderThread` +
> `UpdateCachedRenderStates`), C5 (drop-primitive on index overflow), C6 (restores continue past the
> fetch cap), C7 (refresh stamps `LastSeen=0`), C8 (re-evaluated → done: statics→members; they also
> persisted across PIE sessions), C9 (publish content fingerprint → stable frame ids), §3.1
> (event-gated re-selection: full traversal only on view/orientation change, drain arrivals, retry
> deadline, decode/evict follow-up, or fetch backlog), §3.2 (PathArena interning: `SelectionState` +
> all nine scene maps keyed by `u32` node ids; canonical `path_key` single-homed in the select crate
> and computed incrementally per node), §4.1 (pool-exhaustion no longer burns the fetch budget),
> §4.7 (two-flavor `SelectedTile` build — render/coverage tiles carry no URI strings), §4.8 (comment),
> clip-box memoization (`ClipBoxMemo`, per-key, stamp-retained), `Arc<GeoRenderFrame>` mailbox
> (3×O(R) clones → O(1) lock sections through publish → stream mirror → poll → FFI), true-O(Δ)
> `reconcile_index` (Reclaim entity lists + `TileNode` reverse component — retires the deferred
> entity→path map item), prefetch every-4th-full-step interval, and `UpdateDebugTelemetry` gating.
> Verified: `cargo test --workspace` 0 failures (57 suites), `clippy --all-targets -D warnings`
> clean, `--no-default-features` io build green, `rebuild-editor.ps1 -NoLaunch` → LogiUEEditor
> **Succeeded** (editor left closed). PIE eyeball pending. Deferred with rationale: C1 full
> streaming-read wall on the cyper lane (Content-Length covers the honest case; cyper's `bytes()`
> has no incremental hook — revisit if a streaming API lands), arena growth bounding (§3.2 note),
> SIMD rebase (measure-gated, CLAUDE.md §1B).

Single-context, end-to-end hand-read of the Geo pipeline (no subagents): selection
(`logi_3dtiles_select`, `logi_3dtiles_tileset`) → fetch (`logi_3dtiles_io` ureq + cyper/h3 lanes,
`DiskCache`) → decode (`logi_gltf`, `logi_draco`, `rebase_gltf`) → orchestration
(`logi_3dtiles_scene`: `geo_world.rs`, `streaming.rs`, `stream_thread.rs`, `frustum.rs`) → FFI
(`logi_geo_ffi/ffi.rs`) → render (`LogiBakedGeoTerrainActor.cpp`, `LogiTileMeshComponent.cpp`).
Focus: correctness, quality, and the O-notation of every algorithm on the path, with
lower-order alternatives where one exists.

**Verdict.** The pipeline is in strong shape: no ABI bug, no UB at the boundary, no O(n²) on any
hot loop, indices are range-validated on both decode lanes (uncompressed + Draco), the working-set
disciplines (ADR-0018) hold as documented, and the C++ delta path is genuinely O(Δ). The findings
below are: **2 medium correctness/robustness gaps in the default (cyper/h3) fetch lane**, **1 real
memory-scaling cost on the C++ side**, **1 MID-pool churn bug introduced by the hole-clip material
swap**, and a set of concrete order-reductions — the largest being *re-selection gating* (the full
tree traversal runs at up to ~1 kHz while merely waiting on the network) and *clip-box
memoization* (identical oriented boxes recomputed from full vertex buffers every publish).

---

## 1 · Correctness / robustness findings (ranked)

### C1 — cyper/h3 lane: `MAX_TILE_BYTES` enforced only AFTER collecting the whole body (M)
`cyper_source.rs::fetch_one_h2`: `resp.bytes().await` collects the entire (gzip-inflated) body,
and only then is `body.len() > MAX_TILE_BYTES` checked. The ureq lane reads through a streaming
64 KiB-chunk wall (`take(MAX+1)`), so a hostile/misconfigured server can make it allocate at most
64 MiB + ε; the **default** lane has no such wall — bounded only by the 60 s overall timeout
(at 1 Gbps that is ~7.5 GB buffered before `TooLarge` fires). The comment acknowledges per-stream
flow control, but flow-control windows bound *in-flight* bytes, not *accumulated* bytes.
**Fix:** check `Content-Length` (when present) before the collect, and/or consume the body via
cyper's streaming reader with the same cap-as-you-go wall as `http_get_cancellable`. Until then
the asymmetry means the "hostile server" guard documented for the stack only holds on the
non-default lane.

### C2 — cyper/h3 lane: cancelled-while-pending heap entries are never purged (M)
The runtime thread's priority heap uses lazy deletion: `Cmd::Cancel` removes the id from
`pending_ids` but the `PrioReq` (with its ~150-byte URL `String`) stays in the heap until popped.
A **low-priority** dead entry (e.g. a cancelled `−1e9`-demoted prefetch) under sustained streaming
never reaches the top while higher-priority live requests keep arriving — so the heap grows for
the session's duration. This is **exactly the bug the ureq lane already fixed** (the amortized
rebuild in `HttpSource::cancel` when `heap.len() > 2·in_flight + 64`); the fix was not mirrored
when the admission heap was added to the cyper lane. **Fix:** same amortized rebuild in the
runtime loop (track a `cancelled_pending` counter; rebuild the heap dropping ids absent from
`pending_ids` when it exceeds half the heap).

### C3 — `AcquireMid` discards pooled MIDs of the "other" base — hole-clip made the pool dual-base (M, quality)
`AcquireMid` pops the pool until `Mid->Parent == Base`, **dropping every non-matching MID on the
floor** (GC reclaims). The comment's justification ("P3DT is uniform, the pool holds one base")
predates the hole-clip work: `ApplyTileClip` now swaps tiles between the opaque base and the
masked clip base and recycles *both* kinds into the one pool. Acquiring a clip MID can therefore
silently destroy several pooled opaque MIDs (and vice versa) — steady churn of UObject churn +
GC pressure that the pool exists to avoid, concentrated exactly during LOD convergence when both
kinds are being acquired. **Fix:** key the pool by parent (`TMap<UMaterialInterface*,
TArray<TStrongObjectPtr<UMaterialInstanceDynamic>>>`) or push non-matching MIDs back instead of
dropping them.

### C4 — `FLogiTilePayload` CPU copy retained for every built tile (M, memory — likely intentional, needs a decision)
`ULogiTileMeshComponent::SetTileGeometry` flat-copies the interleaved verts + indices into a
shared payload; the scene proxy uploads and releases *its* reference, but the **component keeps
its reference for the tile's whole residency**. This is load-bearing — `SetMaterial` (debug
views, the hole-clip opaque↔masked swap, texture binds via a fresh MID) recreates the proxy, and
the new proxy re-uploads from this payload; dropping it would make a material swap erase the
tile. But the cost is real: ~200–500 KB × `Tiles.Num()` (hundreds to ~2k built tiles) ≈
**100–500 MB of CPU RAM duplicating geometry that also lives in Rust's `Arc<TileGeometry>` and
in the GPU buffers**. Options, cheapest first: (a) accept + document the triple-residency
explicitly (it currently looks accidental); (b) drop the payload after upload and re-fetch
geometry by key over the FFI on proxy recreation (`logi_globe_world_tile_by_key` covers tiles in
the current frame — but NOT retained tiles absent from it, so this needs a Rust-side
"geometry-by-key" accessor backed by the resident set); (c) avoid proxy recreation for material
swaps via a custom `SetMaterial` that updates the proxy in place (render-thread command), which
also removes the re-upload hitch on every clip swap.

### C5 — `rebase_gltf` index-overflow skip corrupts topology instead of dropping the primitive (L)
`if let Some(idx) = i.checked_add(base) { indices.push(idx) }` — skipping a *single* index on
overflow shifts every subsequent index by one, garbling the whole triangle list downstream
(the C++ proxy then drops mis-ranged triangles one by one). Unreachable today (the 64 MiB fetch
cap keeps `base` far below `u32::MAX`), but if it ever fires the result is silent garbage.
Drop the primitive (or the tile) instead. One-line fix.

### C6 — `select_request` cap-break starves flash-cache restores of lower-ranked tiles (L)
In the ranked load loop, hitting the in-flight cap with nothing preemptible does `break` — which
also skips the **flash/geom restore probes** for every lower-priority tile remaining this step,
even though restores consume no in-flight budget. Under a saturated cap + camera reversal, tiles
whose meshes sit ready in the flash cache wait until the cap drains. **Fix:** on the at-cap path,
`continue` for tiles whose key is in the flash cache (one O(1) `contains_key` probe), `break`
only the fetch-issuing remainder — or run a separate restore pass before the fetch loop.

### C7 — stale `index` entries after a session refresh (L, bounded)
`apply_refreshed_root` swaps the whole tree (`s.root = root`) and clears `grafted`/`bases`, but
keeps `index`. Resident tiles under paths that no longer exist in the fresh tree (or differ
structurally under the new session) linger `Resident`-but-unselected: never published (selection-
driven `PublishSet`), never stamped, evicted only when the resident cap forces it. Bounded by
`MaxResident` so not a leak, but slots are squatted until pressure. Cheap tightening: on refresh,
mark all index entries' `LastSeen = 0` so they are first in line for eviction.

### C8 — function-`static` telemetry state in `StepAndBuildGlobe` (L)
`LastFailed`/`LastRefine`/`HeartbeatN` are function-local statics — shared across **all**
`ALogiBakedGeoTerrain` instances. With two globe actors the change-detection logs would
cross-talk. Move to instance members. (One actor in practice today.)

### Verified-clean (checked specifically, no action)
- **OOB indices cannot reach the GPU**: uncompressed lane validates `i < positions.len()`
  (logi_gltf:179), Draco lane re-checks in `copy_out` (logi_draco:313), and the C++ proxy
  re-clamps per triangle (LogiTileMeshComponent:154–163) — three independent walls.
- FFI: every struct offset-locked via `offset_of!`; every entry panic-guarded; `read_unaligned`
  discipline consistent; view sanitation (NaN/`max_sse≤0`) rejects poison at the edge.
- `DiskCache`: get/put are O(1)+O(log n), eviction unlinks outside the lock, CRC-on-read
  self-reconciles, `open` reclaim can't wander into the geom tier. Torn-write tmp-naming sound.
- `attach_clip_children` O(R) sweep is correct against the sorted invariant; the largest-first
  cap ordering does what it claims.
- Frustum plane construction and the sphere test are mathematically correct (checked edge cases).
- `subdivide_bounds` box/region math verified by hand (off(i), antimeridian unwrap, shift
  saturation).
- `oriented_clip_box` basis math correct; degenerate-extent guards sound.
- HttpSource drop/cancel/registry lifecycle: every request yields exactly one result; the
  amortized tombstone purge bounds heap+registry.
- cyper `PanicCompletion` guard restores the exactly-one-result contract on a panicking task;
  the double-`Loaded` race on cancel-after-complete is harmlessly soft-dropped by the owner.

---

## 2 · O-notation map of the pipeline (per stream step unless noted)

Symbols: **V** = visited tree nodes (cut + frontier, ~1–5 k), **d** = tree depth (~10–30),
**R** = rendered set (~100–1 k), **L** = load set, **A** = active (non-Free) entities,
**Δ** = per-step change, **F** = in-flight fetches, **Vc** = vertices of one tile (~2–10 k).

| Stage | Algorithm | Today | Notes / lower-order alternative |
|---|---|---|---|
| Selection | `select_tree_implicit` walk | O(V·d) | The d factor is path-Vec hashing + cloning per node (3 map probes + a `path.clone()` per state entry). **Path interning** (§3.2) makes it O(V). |
| Selection | render dedup + sort + clip attach | O(R·d + R log R) | Sound; sort doubles as clip-children ordering. Interning drops the d. |
| Selection | `coverage` build for loaded interior nodes | O(interior · URIs) allocs | Eagerly allocates a `SelectedTile` (path Vec + content-URI Strings) for every loaded *interior* node even when never rendered. Make lazy, or intern content URIs as `Arc<str>` in the tileset so clones are refcount bumps. |
| Per-step driver | `select_request` skip-checks | O(L·d) | 6 hash probes per load tile, each O(d). Interning → O(L). |
| Per-step driver | `bump_graft_activity` | O((R+L)·d) probes, each O(d) hash ⇒ **O((R+L)·d²) bytes hashed** | The only d² on the path (P3DT grafts at every leaf json, so `grafted` is large). Alternatives: mark graft records by interned node-id; or bump only the *longest* matching prefix and let prune check ancestors transitively. |
| Per-step driver | `cancel_stale` | O(L·d + F) | Fine. |
| Per-step driver | ranked fetch loop (decorate-sort) | O(L log L) | Fine; priority computed once per tile. |
| Per-step driver | `prefetch_along_motion` | **O(V′) second traversal every moving step** | Coarsened (V′ < V) but still a full walk. Run every Nth step (the 8×-displacement lookahead point moves slowly relative to step cadence) — ~N× cheaper for identical warming. |
| Schedule | `sse_cull_lru` | O(A) | ✓ per ADR-0018 (debug drift assert costs O(pool) in debug only). |
| Schedule | `decode` | O(Δ_fetched · decode) | ✓ working-set-driven; geom-tier `fs::write` inline is measure-gated (ADR-0018 dec. 6) — keep gated. |
| Schedule | `evict` | O(resident) only when over cap; select_nth O(c) | ✓. |
| Schedule | `publish` snapshot | O(selected) | ✓. |
| Schedule | `publish` clip materialization | **O(Σ child Vc) per step, recomputed** | `oriented_clip_box` does 2 full vertex passes per clip child *every publish step* for a pure function of immutable geometry. **Memoize per `tile_key`** (HashMap<u64, RenderClipBox>; entries dropped with the flash entry). During convergence this is ~10⁵–10⁶ redundant vertex reads/step at up-to-kHz cadence — the cheapest big win in the scene crate. Also rebuilds a key→index HashMap O(R) per clipping step; reuse a scratch map. |
| Per-step driver | `reconcile_index` | O(index) on any step with an eviction or decode-failure | Settled-step skip ✓. True O(Δ) needs the deferred entity→path reverse map; during sustained motion every step pays O(index) (~thousands of probes). Worth doing when the profiler shows it. |
| Per-step driver | `bound_failure_sets` retain | O(backoff set) | Small. |
| Graft prune | `prune_cold_grafts` | O(grafts) scan / 120 steps; cold path O((index+inflight)·roots·d) | Gated + rare. ✓ |
| Mailbox | `latest_frame` + shared store + C++ poll | **3 × O(R) clones per frame** (Arc bumps + clip-box Vec clones), two of them under the `latest` Mutex | Store `Arc<GeoRenderFrame>` in the mailbox: build once in `publish`, O(1) Arc clone under the lock on both sides. Removes ~2R atomic RMWs/step and shrinks both critical sections from O(R) to O(1). |
| Stream loop | work gate | full advance at **~1 kHz while `in_flight > 0`** | See §3.1 — the single largest order reduction available. |
| IO (ureq) | heap push/pop, cancel purge | O(log P), amortized O(P) | ✓. |
| IO (cyper) | admission pump | O(log P) | ✓ except C2 (tombstones). |
| DiskCache | get/put/evict | O(1) + O(log n); open O(N) once | ✓. |
| Decode | glTF/Draco decode, `rebase_gltf` | O(Vc) | f64 fold is the known compute-bound SIMD candidate (round-4: 3–4× available; glam f64 has no SIMD). Correct + allocation-lean as written. |
| FFI | `build_globe_frame` | O(R) per **New** poll, O(Δ) diff | ✓ (v8 design holds). |
| C++ | delta apply | O(Δ) | ✓. |
| C++ | full reconcile | O(n log n) coarsest-first, budget+deadline bounded | ✓. |
| C++ | `RefreshTileClips` | O(R) hash probes on changed frames; CRC-gated param pushes | ✓ (cheap given C9 below). |
| C++ | `EvictDeparted` | O(built) on full passes only | ✓. |
| C++ | atlas decode/BC7/mips | O(W·H) on the thread pool; uploads render-thread, budgeted | ✓. |
| C++ | `UpdateDebugTelemetry` | O(built) **every step** | Telemetry-only but unconditional; gate to panel-visible / every Nth step. |

### C9 — frame_id advances on every advance, even when the published set is identical (L/M, cheap fix)
`GeoRenderFrame.frame = view.frame` increments per advance. During a retry-wait or texture-wait
phase (loop advancing on the work gate with zero visual change), the C++ sees a "changed" frame
every poll → runs the delta path (cheap) **plus `RefreshTileClips` (O(R) probes) plus
`build_globe_frame` (O(R) rebuild) every game tick**, and Rust re-publishes + re-clones O(R)
every step. **Fix:** in `publish`, compute a cheap content fingerprint (FNV over
`(tile_key, visible, clip_box_count, clip hash)` in emit order — O(R) adds, no alloc) and keep
the previous `frame` id when unchanged; the existing `frame_id`-equality steady-state skips on
both sides of the FFI then kick in for free.

---

## 3 · The two big order reductions (creative proposals)

### 3.1 Gate re-selection on state change — O(V·d) per *event*, not per millisecond
Today `run_stream`'s work gate is `view_changed || textured_dirty || in_flight > 0 ||
has_pending_retry`, and a true gate runs the **whole** `advance_streaming` — full tree traversal,
clip post-process, schedule, publish — then parks 1 ms while anything is in flight. While merely
*waiting* on the network (in_flight high, nothing arriving this millisecond), hundreds of
traversals per second recompute an identical selection. The selection's output can only change
when one of: camera moved ≥ ε, the textured set changed, `drain_raw` delivered ≥ 1 result
(arrival/failure also frees cap slots → new fetch issuance), a retry deadline expired, an
eviction or decode-failure occurred, or a prefetch-relevant displacement accrued.

**Proposal:** split the step. Every wake: `set_now` + `drain_raw` (cheap, O(arrivals)). Run the
full `select_request` + schedule + publish only when one of the triggers above fired since the
last full step. The triggers are all already observable at zero cost (drain's `done.len()`,
`textured_dirty`, a camera-delta check, the earliest `next_retry` deadline, `evicted_this_step`,
`DecodeFailedThisStep`). Expected effect: 10–100× fewer traversals during fetch-bound streaming,
which is precisely when the stream thread is busiest — and the latency cost is zero because every
state change still triggers an immediate full step.

### 3.2 Path interning — make every per-tile map O(1) and kill the per-node allocations
`TilePath = Vec<u32>` is the key of **nine** maps/sets (`index`, `sel_state`, `decode_failed`,
`fetch_attempts`, `next_retry`, `in_flight_paths`, `grafted`, `bases`, `prefetch_paths`), and the
selector clones a path per state entry + per selected tile per frame. Every probe hashes O(d)
bytes; every entry allocates.

**Proposal:** an interning arena owned by `GeoStreaming`: `nodes: Vec<(parent: NodeId, step: u32)>`
plus one `HashMap<(NodeId, u32), NodeId>` edge map (or a `SmallVec` child table per node — child
counts are ≤ 8). A path becomes a `NodeId(u32)`. The selector threads the id alongside `path`
(push = one edge-map probe amortized O(1); pop = read the parent field), `SelectionState` becomes
`HashMap<NodeId, SelectionResult>` (u32 keys — or a flat `Vec` indexed by NodeId with a
generation tag, making prev-state lookup a bounds-checked array read), and all nine maps re-key
to u32. `tile_key` (the FFI hash) is then a per-node cached field computed once at intern time.
This removes the d factor from the traversal, `bump_graft_activity` becomes O(depth) u32 probes
(no byte hashing), and the per-frame Vec allocations vanish. It also subsumes the known
"prev-state HashMap<Vec<u32>> O(V·D)" finding from the cesium-native dive, and is the natural
substrate for the future incremental/diff selection (stable small node ids).

Sizing note: the arena grows with explored extent — bound it with the same cold-graft prune
(free a pruned subtree's ids to a free list, or epoch-compact when `prune_cold_grafts` fires).

---

## 4 · Smaller quality observations

1. **`request_one` pool-exhaustion still counts against `MAX_NEW_FETCHES_PER_STEP`** — a
   `None` (no slot, no fetch issued) increments `new_fetches`. Harmless (pool exhaustion is the
   bigger problem then) but the counter name lies; increment only on an issued fetch.
2. **`SyntheticSource::poll_completed`** is O(P) scan + sort per poll — test-only lane, fine; noted
   so nobody promotes the pattern.
3. **`doc.views().nth(bv)`** in the Draco branch is O(bv) per primitive — tiny in practice
   (P3DT has few views); `views()` is an indexed iterator so `.nth` is the idiomatic access here.
4. **`logi_globe_world_poll`'s `Same` arm** clears two Vecs per poll — fine; noted that the
   delta arrays are correctly zeroed so a frame-id-ignoring renderer can't replay a stale delta.
5. **`DrainDecodedTextures` upload budget** interacts correctly with the confirm-or-retry gate;
   the `TextureAttempts` reset on successful decode (upload-pressure ≠ decode-failure) is right.
6. **`stream_thread::poll` clones the frame under the `latest` lock** — covered by the
   `Arc<GeoRenderFrame>` change in §2 (mailbox row).
7. **Comment drift:** `make_selected`'s doc says it is "called ONLY when the tile is actually
   kept" — `decide` builds `coverage` eagerly for every *loaded* node including refining interior
   nodes that are often discarded. Fix the comment or (better) the eagerness (§2).
8. **`DecodeJpegToBgraMips` comment** says "mip 0: … BGRA8" while the code (correctly) decodes
   RGBA — stale wording from the BC1→BC7 switch, two lines above correct RGBA commentary.

---

## 5 · What I did NOT find

Hunted for and ruled out: unsorted O(n²) scans on per-step paths; unbounded growth not already
covered by a cap/prune (subtree caches, failure sets, grafts, cancel registries, decode queue —
all bounded; the two exceptions are C2 and the documented arena-growth note in §3.2); ABI
size/offset drift (locks hold, v12 tile = 104 bytes verified); double-free / use-after-free
across the FFI poll contract (valid-until-next-poll is honored everywhere, incl. the clip arena's
no-realloc reserve); panics that can cross the boundary; cache poisoning (CRC + magic + version
keys make stale/corrupt blobs clean misses); z-order/winding errors in the two coordinate
conversions (rebase reflection vs COLLADA double-flip — both correct, the latter explicitly
guarded by a test).
