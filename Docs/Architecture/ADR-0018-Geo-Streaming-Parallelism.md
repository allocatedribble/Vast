# ADR-0018 — Geo Streaming Parallelism: Active-Set Reconciliation & Decoupled Decode

- **Status:** Accepted — the four build-now decisions (SingleThreaded executor, `LiveSet`, decode-off-`JustFetched`,
  settled-step `reconcile_index` skip) **landed 2026-06-09** in `logi_3dtiles_scene` (Rust-only; no FFI/ABI change;
  full `cargo test --workspace` green + clippy `-D warnings` clean, debug & release). Decoupled decode (dec. 5) and
  the writer-thread hoist (dec. 6) remain **measure-gated**. See **Implementation notes** below for the two
  judgment calls (sequential decode; the settled-step skip vs. a true-O(Δ) reverse map).
- **Date:** 2026-06-09
- **Deciders:** Vast architecture
- **Extends:** [ADR-0012](ADR-0012-Poll-Based-FFI-Decoupled-Sim-Thread.md) — ADR-0012 moved the Geo World onto
  a Rust-owned stream thread; **this ADR governs what runs ON that thread each step**, and bounds the per-step
  cost by the active working-set rather than the entity pool.
- **Refines:** [ADR-0014](ADR-0014-LogiRuntime-Unified-Executor-Reactor.md) — for the **Geo** lane the bevy
  multi-threaded *system* executor is **idle by construction** (the schedule is fully `.chain()`ed), so
  ADR-0014's precomputed-stage executor buys ~0 in Geo; the Geo CPU lever is the per-step **working-set + the
  decode handoff**, not the system DAG. Gameplay's executor story (ADR-0014) is unchanged.
- **Relies on:** [ADR-0007](ADR-0007-Actorless-Rust-Terrain.md) — Geo render data is **non-authoritative**, so
  the stage reorderings and the off-thread decode here carry **no determinism obligation** (the property that
  makes this cheap and legal).
- **Most affects:** `logi_3dtiles_scene` (`geo_world.rs`: the four schedule systems + the `advance_streaming`
  spine), and nothing in the gameplay sim.
- **Research:** [Docs/Research/2026-06-09-ECS-Round4-Pipeline-Fork-GPU-Research.md](../Research/2026-06-09-ECS-Round4-Pipeline-Fork-GPU-Research.md)
  (§2, the headline) — the 17-agent round-4 effort that found and adversarially verified this, including the
  **refutation** of the naive "full software-pipeline → 2–3×" claim.

> **Note on line anchors.** The running UE editor / Live Coding rewrites `geo_world.rs` mid-session, so the
> line numbers below drift by a few lines between reads. Trust the **symbol names**; re-grep before editing.

## Context

One Geo step is `GeoWorld::advance_streaming` (`geo_world.rs` ~1420) — a **strictly serial 7-stage method
chain** on the single stream thread:

```
drain_raw → refresh_session_if_needed → select_request → advance(schedule)
          → reconcile_index → prune_cold_grafts → bound_failure_sets
```

and `advance()` runs `(sse_cull_lru, decode, evict, publish).chain()` (~1267) — every system a barrier.

Three facts, all **verified against source** this round:

1. **The bevy multi-threaded *system* executor is genuinely unused in Geo.** Every system is `.chain()`ed, so
   nothing runs concurrently at the executor level; **100% of Geo parallelism is intra-system `par_iter`.** The
   Geo schedule is built with `Schedule::default()` (~1245), which carries bevy's **MultiThreaded** executor —
   pure per-step overhead here, with no concurrency to exploit (the spike's Exp C measured MultiThreaded as
   pure overhead at ~3 systems: 8.4 µs vs 1.0 µs single).

2. **Several per-step passes are O(pool), not O(active) — they scale with the pre-spawned entity pool
   (thousands → tens of thousands of slots), even on a settled scene where nothing changed:**
   - **`sse_cull_lru`** — `par_iter` over the **whole pool** (per-row `if Free { return }`), with **no
     `Changed` gate**. The heaviest O(pool) pass; its load-bearing output is the `LastSeen` LRU stamp that
     drives eviction.
   - **`decode`** — `par_iter` with `BatchingStrategy::fixed(DECODE_BATCH = 1)` (~602) over the **whole pool**,
     gated by `Changed<FetchedBytes>`. At batch = 1 it spawns **~one task per pooled row every step** (O(pool)
     task objects), almost all early-returning on the per-row change check — the dominant cost is the
     task-spawns, not the tick scan.
   - **`reconcile_index`** (~2541) — scans **all** of `streaming.index` every step (`for (p,&e) in &s.index`),
     early-outing **after** the full O(index) scan.

3. **The active working-set pattern already exists — but only two systems use it.** `ResidentSet`
   (`HashSet<Entity>`) + `NewResidents` (`Mutex<Vec<Entity>>`) (~186–199) let **`evict`/`publish`** iterate
   the resident working-set (O(resident)/O(visible)) instead of the pool. `sse_cull_lru` and `decode` were
   never migrated onto it.

4. **`decode` blocks the owner thread.** Its `par_iter` runs synchronously inside `advance()`, so the heavy
   Draco + `rebase_gltf` work sits on the stream-thread critical path — and a synchronous geom-cache
   `fs::write`+rename rides each decode worker (a syscall on the cold-terrain decode path; the audit comment
   already names a writer thread as "the proper fix").

The North Star tie-breaker: at a streamed planet the pool grows; **any O(pool)-per-step term is population on
the per-frame critical path**, the exact thing that must die — while the bevy *executor* is the wrong place to
look, because in Geo it is already doing nothing.

## Decision — the new law

> **The Geo step is bounded by the ACTIVE WORKING-SET, not the entity pool. Every per-tile pass is driven off a
> maintained set of live entities (resident + in-flight) or an explicit per-step transition set — never a
> `par_iter` scan of the pre-spawned pool. Index reconciliation is EVENT-DRIVEN (O(Δ)). The bevy system
> executor stays unused in Geo BY DESIGN (the schedule runs SingleThreaded); Geo parallelism is intra-pass data
> parallelism plus cross-step decode pipelining, never the system DAG. Per-step work is `O(active + Δ)`, never
> `O(pool)`. Because Geo is non-authoritative (ADR-0007), these reorderings carry no determinism obligation.**

Concretely (build-now unless marked):

1. **Run the Geo schedule SingleThreaded** — set `SingleThreadedExecutor` on the Geo `Schedule` (it is fully
   chained; MultiThreaded is pure overhead, Exp C). One line, no fork, immediate.

2. **A maintained `LiveSet` (the active entity working-set).** Extend the `ResidentSet` discipline to **all
   non-`Free` tiles**: a set updated **O(1)** at acquire (`select_request` pops a free slot) and release
   (`free_slot`). **`sse_cull_lru` iterates `LiveSet`** → **O(active)**, not O(pool). Keep `ResidentSet` for the
   resident-only consumers (`evict`/`publish`). Same debug-only drift assert as `ResidentSet` (a missed
   transition fails a test, never silently mis-renders in release).

3. **Drive `decode` off the explicit just-fetched set.** `drain_raw` already knows exactly which entities
   transitioned to `Fetched` this step (it delivers the bytes). Drive decode from that set via random-access
   `tiles.get_mut(e)` (the shape `evict`/`publish` already use) → **O(Δ_fetched)**, with **zero task-spawns on a
   no-change step** — dropping the `par_iter`-over-pool + `Changed<FetchedBytes>` trigger. Heavy decode of many
   concurrent arrivals still fans out across the pool (decision 5); a small/empty arrival set just runs inline.

4. **Event-drive `reconcile_index` → O(Δ_transitions).** Push transitioned paths into per-step queues at the
   **two** producer sites instead of rescanning `index`: eviction (sequential — `evict`/`free_slot` already know
   the freed entity) and decode-failure (decode is parallel → mirror `NewResidents` with a
   `NewDecodeFailed(Mutex<Vec<Entity>>)`, drained sequentially). `reconcile_index` then drains O(Δ) and skips
   the O(index) scan entirely on a settled step.

5. **(Measure-gated) Decouple decode off the stream-thread critical path.** Hand the embarrassingly-parallel
   decode (Draco + `rebase_gltf`) to the shared `ComputeTaskPool` as detached tasks whose `Arc<TileGeometry>`
   results land in a completion queue, **harvested at the top of the next step** — reusing the existing
   `source.poll_completed` completion-queue shape (the I/O lane's `push → poll → harvest` rhythm). An in-decode
   flag (mirroring `in_flight_paths`) prevents re-enqueuing a tile still decoding across steps; the owner thread
   still does only the cheap write (`mesh.0 = Some(arc); state = Resident; LiveSet/ResidentSet edit`).
   **Gate:** build only if a moving-camera P3DT flamegraph shows per-step decode wall-time exceeds the stream
   thread's idle park (`SHORT_PARK` + fetch RTT). Determinism-free (ADR-0007). This is the **achievable** part
   of "pipelining"; the full multi-stage pipeline is rejected (see Alternatives).

6. **(Measure-gated) Hoist the geom-cache `fs::write` off the decode worker** to a flume-fed low-priority writer
   thread — removes a syscall from the cold-terrain decode path only (warm tiles hit the `contains()` guard).

## Implementation notes (2026-06-09)

What landed, and where it diverged from the sketch above:

1. **Executor (dec. 1).** `bevy_ecs` 0.19.0-rc.2 has **no `ExecutorKind` enum / `set_executor_kind`** — the API is
   `Schedule::set_executor(SingleThreadedExecutor::new())` (a `pub const fn`, re-exported at
   `bevy_ecs::schedule`). One line in `GeoWorld::new`. The `multi_threaded` cargo feature stays on (gameplay
   still `par_iter`s); the Geo schedule simply opts its own executor down.

2. **`LiveSet` (dec. 2)** is a private `Resource(HashSet<Entity>)` maintained at exactly four sites — INSERT in
   `request_one`/`place_decoded`/`acquire_tile`, REMOVE in `free_slot`/`evict` — mirroring the existing
   `ResidentSet` discipline (`ResidentSet ⊆ LiveSet`). `sse_cull_lru` now iterates the set **sequentially**
   (random-access `tiles.get_mut`): the per-tile work is one length + div + compare, so a serial walk of the
   bounded live set beats a `par_iter` over thousands of `Free` slots. A debug-only drift assert in
   `sse_cull_lru` (full pool scan, compiled out in release) fails a test on any missed transition.

3. **Decode (dec. 3) is now SEQUENTIAL** over a `JustFetched(Vec<Entity>)` set (`drain_raw` appends; `decode`
   `std::mem::take`s it → zero work on a no-arrival step). Because it is sequential it writes `ResidentSet`
   **directly**, so the `NewResidents` mutex hop is **deleted** (decode was its only producer). This is the
   ADR's "runs inline" form; the burst fan-out is **dec. 5 (gated)**. Consequence: the cold-tile geom-cache
   `fs::write` now rides the sequential stream thread (still `contains()`-guarded + page-cache-absorbed), which
   only sharpens the **dec. 6** gate — neither is built yet.

4. **`reconcile_index` (dec. 4): the settled-step *skip*, not a true-O(Δ) drain.** The ADR's "push transitioned
   paths into per-step queues" needs the **path**, but the only two producers of a stale entry — `evict` and a
   failed `decode`, the in-schedule systems that can't reach `streaming.index` — know only the **entity**, and
   `index` is path-keyed. A genuine O(Δ) drain would therefore need an **entity→path reverse map** kept in sync
   at ~6 mutation sites: real drift surface, for the *least-material* of the three fixes. Instead `reconcile_index`
   **skips its O(index) scan entirely** when neither producer fired (gated on `GeoStats::evicted_this_step` + a
   `DecodeFailedThisStep` flag the method `take`s) — O(1) on a settled step, O(index) only on a churn step (which
   is already doing real work). This satisfies the decision's *stated* goal ("skips the O(index) scan entirely on
   a settled step"); the reverse-map true-O(Δ) is **deferred** until a churn-heavy flamegraph shows the scan dominates.
   (Verified the accounting first: `fail_fetch`/`cancel_stale`/`preempt_inflight`/`prune_cold_grafts` each remove
   their own index entry inline, so only `evict`/`decode` leave one behind.)

## Consequences

**Positive**
- **Per-step Geo cost decouples from pool size** (`O(active + Δ)`), so the same code streams a small scene and a
  planet-scale pool without the settled-scene O(pool) tax — the single change that matters at planetary scale.
- **Zero task-spawn on settled/no-change steps** (decision 3) — the common case once a region is streamed.
- **The owner thread's critical path loses the decode block** (decision 5, measure-gated), overlapping Draco +
  rebase with the next step's cheap traversal.
- **Reuses proven machinery** — `ResidentSet`, the `poll_completed` completion queue, the `free_slot` release
  path — and adds `LiveSet`/`JustFetched` in the same additive shape, not a rewrite of the working hot path.
- **No fork, no FFI** for the core decision; no determinism obligation (ADR-0007).

**Negative / costs**
- **New `LiveSet` + `JustFetched` to maintain** — more drift-risk sets in the `ResidentSet` family (mitigated by
  the same debug-assert-vs-full-scan discipline), **offset** by deleting the `NewResidents` mutex hop (sequential
  `decode` writes `ResidentSet` directly) — net a wash on moving parts.
- **Decoupled decode adds ~1–2 frames of per-tile latency**, an in-decode flag, a completion queue, and a
  thread/lifecycle to **join cleanly on shutdown** (the same discipline ADR-0012 established for the stream
  thread).
- **The win is bursty, not a steady-state multiplier.** On a settled or I/O-bound stream the decode-decouple
  win is small (the value is removing the O(pool) scaling + the decode-burst block, not a throughput ×). This
  is why decisions 5–6 are measure-gated, and the headline "2–3×" was refuted.

## Alternatives considered

- **The full 7-stage software pipeline** (traverse N+1 while decode N while publish N−1). **Rejected at the
  claimed 2–3×.** The stages share one `&mut GeoStreaming` (`index` + tree + `sel_state` + `in_flight`) with a
  tight cross-frame producer/consumer cycle (`select_request` issues fetches `drain_raw` consumes next step;
  `reconcile_index` mutates the very `index` `select_request` reads); a correct double-buffer must snapshot
  `index + tree + sel_state` (O(resident + tree-nodes)) per in-flight frame — plausibly costlier than one
  tile's decode. The thread is also I/O-park-bound during streaming (`SHORT_PARK` after every step with
  `in_flight > 0`) and decode dominates the cheap stages, so the real ceiling is ~1.1–1.4× latency-hiding.
  Decision 5 captures that achievable part **without** the snapshot cost.
- **Parallelize the tree traversal** (level-synchronous BFS front; `O(visited)` critical path → `O(depth + n/P)`).
  **Deferred.** Genuinely novel (no public 3D-Tiles runtime parallelizes the *select* walk), but traversal is
  almost certainly not the current bottleneck, the correctness surface (anti-pop kick, hysteresis dead-band,
  ADD/REPLACE graft inheritance, contiguous-suffix truncate) is subtle and stateful, and the
  `is_loaded`/`is_dead`/visibility closures must become `Send + Sync`. Build behind a feature flag with the
  existing DFS as a differential oracle, and only after a flamegraph shows `select_request` > ~1% of the stream
  frame. Insurance for the deep-tree, billions-of-tiles regime — not a current win.
- **Use ADR-0014's precomputed-stage executor for Geo.** **No benefit** — the Geo schedule is fully chained, so
  the executor is idle; the lever is the working-set, not the system DAG.
- **GPU-offload Geo decode** (the cheapest GPU target — zero PCIe readback). Out of scope here and **blocked**
  on wgpu **buffer-export** maturity (see [ADR-0017](ADR-0017-GPU-Resident-Statistical-Offload.md) + the
  research doc §6.6/§6.7); revisit as a separate decision once the interop spike passes.

## References

- Research: [Docs/Research/2026-06-09-ECS-Round4-Pipeline-Fork-GPU-Research.md](../Research/2026-06-09-ECS-Round4-Pipeline-Fork-GPU-Research.md)
  (§2 the Geo redesign; §5 the O-notation reductions; §8 the rejected full-pipeline).
- [ADR-0012](ADR-0012-Poll-Based-FFI-Decoupled-Sim-Thread.md) (the stream thread this governs),
  [ADR-0007](ADR-0007-Actorless-Rust-Terrain.md) (Geo non-authoritative → reorder-free determinism),
  [ADR-0014](ADR-0014-LogiRuntime-Unified-Executor-Reactor.md) (the runtime; the executor demotion for Geo),
  [ADR-0003](ADR-0003-Representation-LOD.md), [ADR-0011](ADR-0011-Semantic-Tile-Enrichment.md).
- Seams (`logi_3dtiles_scene/src/geo_world.rs`, symbol names — line numbers drift): `advance_streaming`, the four
  systems `sse_cull_lru`/`decode`/`evict`/`publish`, the schedule build (`set_executor(SingleThreadedExecutor::new())`
  + `.chain()`), `reconcile_index`, `drain_raw`, the working-set resources
  `LiveSet`/`JustFetched`/`ResidentSet`/`DecodeFailedThisStep`/`FreeList`, `GeoStreaming.{index,in_flight_paths}`,
  and `free_slot` (the shared release site for `LiveSet`).
