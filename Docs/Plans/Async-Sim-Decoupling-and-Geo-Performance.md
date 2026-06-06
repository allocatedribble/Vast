# Plan — Async Logi-rs: Poll-Based FFI, Decoupled Sim/Stream Threads, and Geo Performance

> Status: **active.** [ADR-0012](../Architecture/ADR-0012-Poll-Based-FFI-Decoupled-Sim-Thread.md) accepted;
> Phase 0 (Geo decouple + poll FFI) **landed and editor-verified**; the Phase-1 rebase matrix-fold landed.
> Companion to [Docs/Audit/2026-06-05-Deep-Quality-Audit.md](../Audit/2026-06-05-Deep-Quality-Audit.md)
> (correctness pass) and **[Docs/Research/ECS-Multithreading-for-Geo.md](../Research/ECS-Multithreading-for-Geo.md)**
> (the `bevy_ecs`-orchestration research Phase 1 is built on).
>
> **Parallelism decision (revised 2026-06-06):** terrain's multi-threading is orchestrated by **`bevy_ecs`
> (already a project dependency) — NOT `rayon`.** A dedicated Geo `World` on the stream thread runs a
> schedule whose heavy systems `par_iter` over **tile entities** on the **shared global `ComputeTaskPool`**
> (gameplay + Geo share one pool → no second pool, no core oversubscription). This revises the original
> review's "ECS isn't the lever for Geo" verdict: with small Table components, geometry behind `Arc`, **zero
> structural churn in the hot loop**, and **right-sized systems**, ECS brings what `rayon` alone does not
> (one pool, change detection, access-conflict scheduling, a unified model with gameplay). The rationale and
> the hard constraints are in the research doc; the design is **§2A**.
>
> **Architectural implication:** amends ADR-0005's boundary (owner thread off the game thread) — ADR-0012.
> The Geo crates gain a `bevy_ecs`/`bevy_tasks` dependency for the orchestration World (they were
> deliberately bevy-free); this is consistent with ADR-0005's "one ECS / one truth language" and should be
> recorded as an **ADR-0012 addendum** before Phase 1 lands.

## Governing principle (the directive this plan encodes)

> Unreal communicates with Logi-rs **only on the Unreal game thread**, through **cheap, non-blocking**
> calls. Logi-rs is **as multi-threaded and parallel as the determinism contract allows**. The FFI
> **`step` functions become `poll` functions**: Unreal pushes the latest view/input and pulls the
> **latest** published simulation/terrain update — it never drives, and never blocks on, the heavy work.

Everything below follows from inverting *who drives the loop*. Today the game thread calls `step()` and
the heavy work (decode, rebase, traversal, mesh build) runs synchronously inside that call. After this
plan, a **Rust-owned thread** runs the loop continuously and **publishes** snapshots; the game thread is
a pure producer (push view/input) and consumer (poll latest, release). The FFI boundary stays exactly
where ADR-0005 puts it — Unreal→Rust, narrow C ABI, no Rust→Unreal calls — but the call *shape* changes
from "do the work now" to "hand me the newest result."

---

## 0. Before / after at a glance

| | **Today (synchronous `step`)** | **After (poll + decoupled threads)** |
|---|---|---|
| Who advances the World/TilesetWorld | the **game thread**, inside the FFI call | a **Rust-owned sim/stream thread** |
| Game-thread FFI cost | the entire drain→decode→rebase→select→evict (geo) / schedule run (gameplay) | **O(1)** push-view + O(visible) snapshot read |
| Heavy CPU work location | game thread (causes frame hitches) | sim/stream thread + a job pool (off the frame) |
| Geometry handoff | pointers "valid until next `step`" (fragile) | retained, refcounted slots; up to 3 live (ring) |
| Geo parallelism | HTTP fetch only | fetch **+ decode + rebase + mesh-prep**, all parallel |
| Cadence | locked to whoever calls `step` (~30 Hz actor tick) | sim runs at its own rate; render polls at frame rate |
| Determinism (gameplay) | fixed-step, on game thread | fixed-step, on sim thread, timestamped input (preserved) |

The gameplay engine is **already 70% of the way** to the "after" column: it has the 3-slot
[`SnapshotRing`](../../Rust/crates/logi_runtime/src/lib.rs) (publish/retire fence, `find(0)`=latest,
keeps 3 live so a mid-read consumer is never clobbered) and `logi_buffer_release`. Its only gap is that
the *advance* (`logi_world_tick`) still runs on the game thread. **Geo has none of this** — building the
ring/retain layer for Geo is the larger lift.

---

## 1. The FFI contract: `step` → `poll`

Three call categories, all non-blocking, all callable only from the Unreal game thread:

### 1a. Push the latest view / input (producer; cheap)
```
// Geo: only the NEWEST camera matters → a seqlock/atomic-published slot (no queue).
logi_globe_world_set_view(handle, camera_ue_cm[3], fov_y, viewport_h, max_sse);

// Gameplay: inputs must be ordered + deterministic → a bounded SPSC queue, drained at the
// next fixed-step boundary and recorded for replay. Stamp with the intended sim tick.
logi_world_push_input(handle, &UnrealFrameInput);
```
`set_view` writes a small POD into a double-buffered/seqlock slot the stream thread reads each iteration.
`push_input` enqueues into an SPSC ring the sim thread drains at step boundaries. Neither blocks; neither
touches the `World`.

### 1b. Poll the latest published frame (consumer; cheap, retained)
```
// Returns the most-recent COMPLETE frame. `status` distinguishes:
//   NEW     — a newer frame than your last poll (rebuild changed tiles)
//   SAME    — no new frame since last poll (C++ reuses its built sections — no work)
//   EMPTY   — nothing published yet
logi_globe_world_poll(handle, &out_frame /* id + tile descriptor span */) -> status;
logi_world_poll(handle, &out_snapshot /* id + instance/actor/despawn spans */) -> status;
```
`poll` is the renamed/repurposed read side. It is `SnapshotRing::find(0)` (latest) + mark-retained, plus
a generation check so an unchanged frame returns `SAME` and the C++ side **skips rebuilding** (this is
the "fewer lookups / fewer rebuilds" win — most frames the tile set is unchanged). The returned
descriptors are pointers into **retained** Rust buffers (see §3).

### 1c. Release a retained frame (consumer; cheap)
```
logi_buffer_release(handle, frame_id);   // already exists for gameplay; Geo adopts the same
```
Marks the ring slot free so the producer can reuse it. The producer's publish fence
(`acquire_free_slot`: retired **and** not the current latest) already guarantees it never overwrites a
buffer the consumer holds.

**Migration shim:** keep `logi_*_world_step(view) -> frame` as a thin wrapper = `set_view(view);
advance_once(); poll()` so the new path can land incrementally and be A/B'd against the old synchronous
behavior behind a flag, then retired (ADR-0004: no permanent compatibility code — the shim is temporary
and tracked).

---

## 2. Internal threading model of Logi-rs (after)

```
            UNREAL GAME THREAD                         RUST-OWNED THREADS
  ┌───────────────────────────────┐
  │ per frame (~120 Hz):          │        ┌──────────────────────────────────────────┐
  │  set_view / push_input  ──────┼──────► │ GEO STREAM THREAD — owns a bevy_ecs World  │
  │  poll_latest  ◄───────────────┼──────  │  loop: run the Geo schedule (§2A)          │
  │  release(old_id)              │  ring  │   select → drain → DECODE(par_iter) →       │
  │                               │        │   sse/cull(fused) → evict → publish        │
  │  (renderer builds only the    │        │   tiles are entities; heavy systems        │
  │   tiles whose id changed)     │        │   par_iter on the shared pool ──┐          │
  └───────────────────────────────┘        │      └── fans out to ──┐                   │
                                           └──────────────────────────┼─────────────────┘
  ┌───────────────────────────────┐        ┌──────────────────────────▼─────────────────┐
  │ poll_latest (gameplay) ◄──────┼──ring──│ SIM THREAD (owns gameplay World)            │
  │ push_input ───────────────────┼──────► │  fixed-step loop @ 30 Hz (deterministic):   │
  └───────────────────────────────┘        │   drain input queue (timestamped)           │
                                           │   run schedule (par-safe ECS systems)       │
                                           │   extract_render_snapshot → publish ring     │
                                           └──────────────────────────┬─────────────────┘
                                                                      │ fans out to
                    ┌─────────────────────────────────────────────────▼──────────────┐
                    │ SHARED bevy_tasks ComputeTaskPool (gameplay + Geo — ONE pool)   │
                    │  • Geo `decode` system par_iter (glTF/qmesh decode + rebase)    │
                    │  • gameplay par_iter   • procgen   — NO separate rayon pool      │
                    └─────────────────────────────────────────────────────────────────┘
                    ┌─────────────────────────────────────────────────────────────────┐
                    │ HTTP FETCH POOL (existing crossbeam workers) — blocking GET + DiskCache │
                    └─────────────────────────────────────────────────────────────────┘
```

Rules that hold across the model:
- **Exactly one mutator per world** — the §11 invariant is *relocated*, not removed. The owner is now
  the sim/stream thread; `is_owner_thread()` is re-pinned to it at thread start. The cross-thread FFI
  surface is exactly `{set_view, push_input, poll, release}`, each lock-free or bounded-queue.
- **No Rust→Unreal calls** (ADR-0005) — poll is an Unreal→Rust *pull*. Unchanged.
- **One bounded job pool, core-aware** — geo decode and gameplay `par_iter` share it; clamp to
  `cores − reserved` so the two engines + Unreal's render/RHI threads don't oversubscribe (resolves the
  existing `TODO(perf)` on logical-vs-physical cores in `schedules.rs`, and unifies the geo pool that is
  currently a separate ad-hoc `std::thread` set).

---

## 2A. Terrain as an ECS — the orchestration layer for large multi-threading

The Geo stream thread (§2) does not advance a hand-rolled `TilesetWorld`; it **owns a dedicated
`bevy_ecs::World` and runs a Geo `Schedule`** on it. Tiles are entities; the heavy per-tile work fans out
over the **shared global `ComputeTaskPool`** via `Query::par_iter`. This reuses the ECS we already ship (no
`rayon`, one pool) and brings change detection + access-conflict scheduling for free. Every choice below is
bound by a verified constraint from [the research](../Research/ECS-Multithreading-for-Geo.md) — get any of
them wrong and ECS becomes *slower* than the serial loop.

**World & thread.** A **separate** Geo `World` (not the gameplay World — preserves the determinism split),
created and ticked on the ADR-0012 stream thread — exactly Bevy's **pipelined-renderer pattern** (a dedicated
`std::thread` that lends its idle time back to the shared `ComputeTaskPool` via `scope()`, research §10). Geo
is non-authoritative (ADR-0007), so its schedule needs **no determinism** — the multi-threaded executor runs
free.

**Entities = tiles, from a pre-spawned pool.** One entity per tile slot. The per-frame loop performs **no
structural changes** — spawn/despawn/insert/remove are `ApplyDeferred` barriers that serialize the whole
schedule (research §2/§7). Tiles are taken from a **pre-spawned free-list** and "born"/"evicted" by
**resetting component *fields***, never by `spawn`/`despawn` or add/remove markers. Genuinely new slots (pool
growth) are batched into **one end-of-step `ApplyDeferred`**, never one-per-tile. Result: all tiles share one
archetype → dense, fragmentation-free iteration.

**Components — small, Table-stored, heavy data behind `Arc`** (research §6/§7/§8). Every tile carries the
same set (uniform archetype); the hot pass touches only small fields:

| Component | ~Bytes | Role |
|---|---|---|
| `TileId(u64)` | 8 | interned path identity (cache-friendly key; the `Vec<u32>` path lives in a side `Resource`) |
| `TileBounds { center: DVec3, radius: f64 }` | 32 | SSE + culling |
| `GeometricError(f64)`, `TileTransform(DMat4)` | 8+128 | refine decision + the rebase-fold input |
| `TileState` (enum `Free`/`Selected`/`Requested`/`Fetched`/`Resident`) | 1 | drives which system touches it — **a field, never a marker component** |
| `Sse(f32)`, `CamDistance(f32)`, `LastSeen(u64)` | 16 | per-frame scratch (refine + LRU) |
| `FetchedBytes(Option<Arc<[u8]>>)` | 8 | raw bytes between fetch and decode — a **handle**, not inline |
| `TileGeometry(Option<Arc<TileGeometryData>>)` | 8 | the decoded mesh — **`Arc` handle; bytes never sit in the hot row** |

The multi-kilobyte geometry is reached **only** by the decode/publish systems through the `Arc` (this is the
Phase-0 retain, now doubling as the storage-layout win).

**Systems — few, right-sized** (research §5, the granularity rule). Schedule order below; the executor also
runs any with disjoint component access concurrently:

1. **`select`** — *sequential*. Descends the tileset tree (pointer-chasing → not `par_iter`-able) marking
   each visited tile's `TileState`/`Sse`. Cheap; sets fields only. Owns the tree as a `Resource`.
2. **`drain_fetches`** — *sequential, cheap*. Moves completed HTTP results (the existing crossbeam worker
   pool, unchanged) into the matching tile's `FetchedBytes` and flips `TileState = Fetched`. Field mutation
   only — no structural change.
3. **`decode` — the parallel beneficiary.** `par_iter_mut` over `Query<…, (With<Fetched>, Changed<FetchedBytes>)>`:
   each task does the **whole expensive unit** — `decode_glb`/`qmesh::parse` + the folded vertex rebase — and
   writes an `Arc<TileGeometryData>`, flipping to `Resident`. **Explicit `BatchingStrategy` with a small
   batch**: tile cost is highly variable (a tiny tile vs. a dense photogrammetry tile), so small batches let
   the pool **work-steal** the uneven load — the uniform-work default heuristic is wrong here (research §4).
   This is where the cores actually get used.
4. **`sse_cull_lru`** — the **fused cheap pass**: one system computing `CamDistance` + `Sse` + visibility +
   `LastSeen` for all candidate tiles in a **single coarse-batched `par_iter`** (or serial when few tiles).
   *Not* four tiny systems and *not* a `par_iter` per cheap op — that is a measured net loss (research §5).
5. **`evict`** — *sequential*. Picks LRU victims past the cap, drops their `TileGeometry` `Arc` (frees the
   buffer once no published frame holds it), resets the slot to `Free`. **No despawn.**
6. **`publish`** — builds the render-frame snapshot from `Query<…, With<Resident>>` ∩ visible, gated by
   `Changed<TileGeometry>` so unchanged tiles are skipped, and writes it into the §3 mailbox the game thread
   polls. (`Changed` still *scans* matched rows cheaply but skips the build body — research §9.)

**Why this is "decently-sized" (the overhead guard).** The only `par_iter` that must clearly win is
`decode` — each task is ~ms of independent work, so scheduling overhead is amortized hundreds-fold. All the
cheap work is **fused** into `sse_cull_lru` and coarsely batched. There is **no transferable µs threshold**
(the "~1µs/task" rule was *refuted* in verification, research §5) — the `decode` and `sse_cull_lru` batch
sizes are **tuned on a flamegraph** against real tile counts (AGENTS.md §1B "measure first"). Inter-system
parallelism is a bonus: e.g. texture decode could be a separate disjoint-access system that runs concurrently
with mesh rebase.

**What stays out of the ECS.** The tileset *tree* (a grafting pointer tree) and the HTTP worker pool stay as
they are — the ECS orchestrates *tile work*, not tree topology or the network. `select` reads the tree
(`Resource`) and writes tile components; that seam is the bridge. The Phase-0 `GlobeStreamer` mailbox/ring
(§3) is unchanged — only the *internals* of how the stream thread advances change (an ECS schedule replaces
the hand-rolled `advance`).

---

## 3. The handoff & lifetime safety (the critical correctness work)

The whole plan rests on the consumer reading Rust-owned geometry on a *different* thread than the producer
mutating it. Get this wrong and you get a use-after-free across the C ABI. Two mechanisms, both already
proven on the gameplay side:

### 3a. Ring slots with a publish/retire fence (have it for gameplay; build it for geo)
Reuse the `SnapshotRing` design: N≥3 slots; `publish` writes into a slot that is **retired and not the
current latest**; `poll` serves the latest and the consumer `release`s when done. The ring keeps up to N
frames live, so a consumer still reading frame *k* is never clobbered when *k+1* publishes; over-fast
producers drop intermediate frames (counted as `dropped_frames` — already instrumented). For render,
dropping intermediates is correct: only the latest matters.

### 3b. Refcounted tile geometry (new for geo — the harder half)
Geo's per-tile vertex/index/texture buffers are large and have independent lifetimes (a tile can stay
resident across many frames, then be evicted while a slightly-stale polled frame still points at it).
The current "valid until next `step`" contract cannot survive async. Fix: store tile geometry as
**`Arc<TileGeometry>`** (`Arc<[u8]>` vertices / `Arc<[u32]>` indices / `Arc<[u8]>` texture). The published
`GlobeFrame` holds `Arc` clones of exactly the tiles it references; `evict_decoded` just drops the
*world's* `Arc`, and the buffer is freed only when the last frame referencing it is released. Benefits:
- **Correct by construction** — no epoch bookkeeping, no manual reclamation race.
- **Subsumes audit item M11** — the glTF texture `B`-byte copy disappears: the decoder produces an
  `Arc<[u8]>` once and everyone shares it.
- The FFI still hands raw `*const u8` into the `Arc`'s buffer (zero-copy across the boundary); the `Arc`
  just guarantees the buffer outlives the polled frame.

(Alternative considered: an epoch/generation counter where the stream thread refuses to reclaim buffers
tagged with an epoch still held. Rejected — `Arc` is simpler, allocation-stable, and the refcount traffic
is per-tile-per-frame, i.e. hundreds of atomic ops/frame, negligible.)

---

## 4. Cadence, determinism, and interpolation

The user's "as parallel as possible" must be reconciled with AGENTS.md / ADR-0005's gameplay-determinism
law. The split:

- **Geo / terrain = unrestricted parallel.** Render data is non-authoritative (ADR-0007), needs no
  determinism (audit H2, won't-fix). The stream thread runs free at its own cadence, fans decode/rebase
  across the job pool with no ordering constraint, and selects against the newest camera each iteration.
  This is where the directive applies fully.

- **Gameplay = decoupled but deterministic.** The sim thread runs a **fixed-timestep** loop
  (`fixed_dt = 1/hz`, default 30 Hz — the `accumulator`/`fixed_dt` already in `LogiWorld`), draining a
  **timestamped input queue** at each step boundary so a recorded input sequence replays bit-identically
  regardless of wall-clock. Parallel ECS execution is allowed **only when systems are conflict-correct**;
  keep `SingleThreadedDeterministic` for golden/replay tests and `BoundedParallel` for production (both
  modes already exist in `schedules.rs`). Moving the loop off the game thread does **not** by itself
  change determinism — the step count and input order do, and both are preserved.

- **Render-side smoothing (optional, gameplay).** Because the renderer polls at frame rate (e.g. 120 Hz)
  but the sim publishes at 30 Hz, the C++ side can **interpolate** between the two latest published
  snapshots for smooth motion (publish the previous + current transform; lerp by `accumulator`
  fraction). Geo tiles change slowly — latest-only is fine, with an optional cross-fade on LOD swap.

---

## 5. How the prior audit's findings re-rank under this model

The inversion changes *why* and *how urgently* each earlier finding matters:

| Prior finding | Re-ranked under async poll |
|---|---|
| Move decode/rebase off the game thread (was #1 hang fix) | **Done** — geo runs on the `GlobeStreamer` thread; decode is off-frame. Remaining: *parallelize* decode per-tile via the `bevy_ecs` Geo World's `decode` system on the shared `ComputeTaskPool` (§2A, Phase 1). |
| `resolved`-set rebuild clones every resident path/frame ([streaming.rs:184](../../Rust/crates/geo/logi_3dtiles_scene/src/streaming.rs:184)) | No longer a *frame-hitch* (it runs on the stream thread), but still caps **sim throughput** → keep, lower urgency. |
| FFI double-probe + per-frame path re-hash ([ffi.rs:472,480](../../Rust/crates/geo/logi_geo_ffi/src/ffi.rs:472)) | Now on the stream thread; fold into the publish step (memoize key on the tile). Throughput, not hangs. |
| DiskCache re-reads every hit from disk ([io.rs:378](../../Rust/crates/geo/logi_3dtiles_io/src/lib.rs:378)) | **Unchanged value** — add in-memory hot tier. Async makes it *easier* (the stream thread can prefetch/warm without blocking anyone). |
| Priority + preemptive fetch ([streaming.rs:359](../../Rust/crates/geo/logi_3dtiles_scene/src/streaming.rs:359)) | **Higher value** — the stream thread now always selects against the freshest camera, so a priority queue pays off immediately on fast motion. |
| f32 + matrix-fold rebase (~3–6×) | **Matrix-fold DONE** (f64-exact, ~½ the matrix ops, tested) — it is now the per-task body of the `decode` system, so it compounds with the `par_iter` fan-out. f32-SIMD deferred (precision-validate first). |
| Incremental cross-frame traversal | Unchanged value; complements the priority queue. |
| **New, introduced by this plan** | Geo ring + `Arc` retain (§3); input/view queues (§1a); sim-thread lifecycle/shutdown (§7); the ADR-0012 contract change (§9). |

---

## 6. Revised phased roadmap

Re-sequenced so the enabling inversion lands first and everything else rides on it.

**Phase 0 — Decouple + poll (the foundation).** ✅ **Landed + editor-verified for Geo** (`Arc` retain,
`FrameRing`, `GlobeStreamer` thread, `set_view`/`poll` FFI, C++ consumer, editor rebuild). The **gameplay**
sim-thread half is still pending (lower value today — the gameplay sim is trivial; determinism-sensitive).
- Spawn the sim thread (gameplay) and stream thread (geo) at `world_create`; relocate `is_owner_thread`.
- Split `logi_world_tick` into: sim-thread `advance+publish` loop, and game-thread `poll`. Gameplay
  reuses its existing ring; **build the geo ring + `Arc<TileGeometry>` retain** (§3b).
- Add `set_view` / `push_input` producers (§1a) and the `poll`/`release` consumers (§1b/c).
- Land the temporary `step`-shim for A/B.
- Sim-thread lifecycle: clean start on create, signalled stop + join on destroy / PIE end (§7).
- *Exit criterion:* game-thread FFI time is O(visible) with zero decode/traversal on it; replay golden
  tests still bit-identical.

**Phase 1 — Orchestrate the off-thread work as a `bevy_ecs` Geo World** (§2A; built on
[the research](../Research/ECS-Multithreading-for-Geo.md)).
- ✅ *Done:* `rebase_decoded` matrix-fold — one folded affine `DMat4` per primitive (f64-exact, ~½ the
  per-vertex matrix ops), guarded by a unit test. (The f32-SIMD variant is deferred — precision-validate
  first; the fold is the safe win.)
- Stand up the Geo `World` + `Schedule` on the stream thread, **replacing the hand-rolled `advance`**, and
  share the global `ComputeTaskPool` via `scope()` (pipelined-renderer pattern). Geo crates take a
  `bevy_ecs`/`bevy_tasks` dependency (ADR-0012 addendum).
- Tiles = **pre-spawned entities**, small Table components, geometry behind `Arc` (Phase-0 retain reused);
  **no structural changes in the loop** — free-list + in-place field mutation, churn batched into one
  end-of-step `ApplyDeferred`.
- The **`decode` system** `par_iter_mut`s the heavy decode + folded rebase with an explicit **small**
  `BatchingStrategy` (variable tile cost → work-steal); the **cheap per-tile work fuses** into one
  coarsely-batched `sse_cull_lru` system; `decode`/`publish` gated by `Changed<T>` (§2A).
- **Tune batch sizes on a flamegraph** — no magic threshold (research §5). Microbenchmark the per-tile decode
  break-even and the resident-tile counts on target hardware.
- Parallelize the C++ proc-mesh / Nanite build (UE `Async`/task graph); keep the per-step build budget as a
  smoothing cap, not a throughput cap.

**Phase 2 — Cache + streaming quality (kills residual hitches & pop).**
- Two-tier cache: in-memory byte (and/or decoded-mesh) LRU in front of `DiskCache`; stop re-decoding on
  re-entry near the working-set boundary; move eviction file-deletes off the index mutex.
- Priority + preemptive fetch: bounded priority queue keyed by screen-space importance; actually cancel
  stale in-flight on fast motion.
- Incremental cross-frame traversal (cesium-style kick / `ancestorMeetsSse`) + stateful LOD hysteresis on
  the streaming selector (the dead-band already exists in `logi_geo_lod::should_refine`).

**Phase 3 — Throughput cleanups (sim-thread headroom).**
- Persistent `in_flight_paths` set → delete the per-iteration `resolved` rebuild.
- Collapse the FFI double-probe; memoize tile keys; reuse descriptor buffers.
- `base_for` prefix trie (O(L·G)→O(L·D)); baked `select` O(visible) via index + dedupe child probes.
- Codec constant-factors: procgen divide-elimination, `Package::find` map, qmesh alloc dedupe,
  `interleave` 32-byte write.

**Phase 4 — Gameplay parallelism (make the dormant executor earn its keep).**
- Make the hot ECS systems (`integrate_motion`, `compute_significance`, budget passes) conflict-correct
  and `par_iter_mut` them — **or** default to `SingleThreadedDeterministic` until agent population
  justifies the pool, so the `MultiThreaded` executor stops costing coordination overhead for nothing.

---

## 7. Risks & invariants to preserve

- **Determinism (gameplay)** — fixed step + timestamped input + conflict-safe parallel systems; keep the
  single-threaded mode for golden tests. The decoupling must not let wall-clock affect step count.
- **Lifetime safety across the FFI** — `Arc` retain + the ring publish/retire fence; never hand a pointer
  whose buffer can be freed before `release`. Property-test the publish/poll/release/evict interleaving.
- **§11 owner rule relocates, not removed** — exactly one mutator per world (the sim/stream thread); the
  cross-thread surface is only `{set_view, push_input, poll, release}`. Assert owner identity in debug.
- **No Rust→Unreal calls** (ADR-0005) — preserved; poll is a pull.
- **Editor / PIE lifecycle** — sim/stream threads must start on world-create and **join cleanly** on stop;
  no detached threads leaking across PIE sessions or surviving a Live Coding reload. (HTTP workers already
  exit on source-drop; the sim/stream threads need the same `AtomicBool running` + channel-close + join.)
- **Back-pressure** — bounded input queue and bounded in-flight fetches; the ring drops (counts)
  intermediate frames rather than growing unbounded.
- **One home per shared contract** (AGENTS.md §1A) — the geo ring should reuse, not re-implement, the
  gameplay ring's slot/fence logic; factor the ring into a shared place if it's duplicated.
- **No structural changes in the Geo hot loop** (research §2/§7) — pre-spawned tile pool + in-place field
  mutation; spawn/despawn/insert/remove are `ApplyDeferred` barriers, so any churn is staged into one
  end-of-step flush. `TileState` is an enum *field*, never an add/removed marker component.
- **Cross-World pool contention** (research open question) — the Geo World lending idle time to the *shared*
  `ComputeTaskPool` could steal pool time from gameplay's fixed-step budget (jitter / priority inversion).
  Geo's own jitter is harmless (non-authoritative), but gameplay's is not — **measure under load**; a
  separate capped pool for Geo is the fallback if sharing starves gameplay.

---

## 8. Concrete FFI sketch (illustrative)

```rust
// ---- producer (game thread → stream thread): newest-wins, no queue ----
#[no_mangle] pub extern "C" fn logi_globe_world_set_view(
    h: *mut GlobeHandle, cam: [f64;3], fov_y: f64, vh: f64, max_sse: f64,
) { /* publish into a seqlock View slot; stream thread reads it each loop iter */ }

// ---- consumer (game thread): O(visible), retained, generation-checked ----
#[repr(C)] pub struct LogiPollResult { pub status: u32, pub frame_id: u64,
    pub tiles: *const LogiGlobeTile, pub tile_count: u32 }
#[no_mangle] pub extern "C" fn logi_globe_world_poll(
    h: *mut GlobeHandle, out: *mut LogiPollResult,
) -> u32 { /* ring.find(0) + retain; SAME if id unchanged → C++ skips rebuild */ }

#[no_mangle] pub extern "C" fn logi_buffer_release(h: *mut GlobeHandle, frame_id: u64) { /* ring.retire */ }
```
```cpp
// ---- C++ game thread, per frame ----
World->set_view(Camera, Fov, ViewportH, MaxSse);          // cheap
LogiPollResult R; uint32 St = World->poll(&R);            // cheap, latest
if (St == NEW) { BuildOrReuseSectionsFor(R.tiles, R.tile_count); LastId = R.frame_id; }
// St == SAME → nothing to do; reuse existing sections (no lookups, no rebuild)
World->release(PrevFrameId); PrevFrameId = R.frame_id;
```
The stream thread, meanwhile, loops independently: read view → drain/decode (job pool) → select →
request → evict → `publish`, at whatever rate the work allows.

---

## 9. ADR implication

This relocates the single-owner thread (`is_owner_thread`, the historical §11 runtime rule) off the Unreal
game thread and redefines the FFI from synchronous `step` to `set_view + poll + release`. That is a
first-class architecture decision, ratified as
**[ADR-0012 — Poll-Based FFI / Decoupled Sim Thread](../Architecture/ADR-0012-Poll-Based-FFI-Decoupled-Sim-Thread.md)**
(Proposed): it amends ADR-0005's boundary (AGENTS.md §1/§1C — which already says *Unreal pulls a snapshot
and does not mutate*; this completes that intent), and notes that ADR-0007's non-authoritative-render clause
is what *permits* Geo to run fully async while gameplay stays fixed-step deterministic. Phase 0 should not
merge before that ADR is accepted.
