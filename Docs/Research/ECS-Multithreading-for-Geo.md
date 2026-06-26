# Research — `bevy_ecs` as a Multi-Threading Orchestration Layer for Geo

> Engineering reference for ADR-0012 / the Geo async plan. Decision being supported: use **`bevy_ecs`
> (already a project dependency) as the orchestration layer for terrain's large-scale multi-threading**,
> rather than a separate `rayon` pool. Method: deep multi-source research — 21 sources fetched, 98 claims
> extracted, 25 adversarially verified (3-vote, majority-refute kills), **22 confirmed / 3 killed**.
> Primary sources: `docs.rs` for `bevy_ecs`/`bevy` 0.18.1 (the version studied; Cargo.lock now resolves
> **0.19.0-rc.2** — the verified constraints below carry over unchanged), checked-in `bevy` source
> (`par_iter.rs`, `batching.rs`, `pipelined_rendering.rs`), core-dev PRs (#4777, #6503, #16782).
>
> **This revises the earlier "ECS is not the lever for Geo" verdict.** That verdict was right about the
> *failure modes* (tiny components-per-op, archetype churn, over-fine parallelism) — and the findings below
> are exactly the constraints that avoid them. Within those constraints, `bevy_ecs` brings what `rayon`
> alone does not: one **shared** `ComputeTaskPool` (no second pool to oversubscribe cores), built-in
> **change detection**, **access-conflict scheduling**, and a unified model with gameplay.

## The verified findings (and what each means for Geo)

### 1. Multi-threaded executor = automatic, access-driven system parallelism `[3-0]`
The built-in multi-threaded executor "run[s] as many [systems] in parallel as possible," serializing two
systems **only** when one **writes** a component the other reads or writes (two reads never conflict). An
*empty* conflict list means a system is exclusive (`&mut World`) or holds `Query<EntityMut>`.
*Source: docs.rs `bevy_ecs` 0.18.1; `MultiThreadedExecutor`.*
→ **For Geo:** design systems with **disjoint component access** so independent stages (e.g. texture decode
vs. bounds compute) run concurrently for free. Avoid `&mut World` / `EntityMut` in the hot schedule.

### 2. Commands & structural changes are **barriers**, not free `[3-0]`
`Commands` and structural changes (spawn/despawn/insert/remove) are **deferred** and flushed at
`ApplyDeferred` sync points — a special exclusive-`&mut World` system that **cannot run in parallel** and
**serializes the whole schedule** at that point. The executor auto-inserts one wherever a later system
depends on the deferred effect.
*Source: docs.rs `ApplyDeferred`; DeepWiki commands; PR #16782 (Dec 2024).*
→ **For Geo (the load-bearing constraint):** **no per-tile `insert`/`remove`/`spawn`/`despawn` in the
per-frame loop.** Pre-spawn a fixed pool of tile entities; drive the pipeline by **mutating component
*fields* in place** (a `TileState` enum, an `Option<Arc<…>>` handle), never by adding/removing marker
components. Batch the rare structural churn (new tiles discovered, evicted tiles freed) into **one**
end-of-step flush, not one-per-tile.

### 3. `par_iter` batch size = `max_items / (threads × batches_per_thread)` `[3-0]`
Default `batches_per_thread = 1` and limits `1..usize::MAX`, so the default reduces to
`largest_matched_table.div_ceil(ComputeTaskPool::thread_num())` — ~one batch per pool thread, **no minimum
above 1**. `par_iter` falls back to **single-threaded for small entity counts**.
*Source: `par_iter.rs`/`batching.rs` (`calc_batch_size`); docs.rs `BatchingStrategy`; Bevy Cheat Book.*
→ **For Geo:** the pool thread count is the knob tying our parallelism to the hardware; few resident tiles
→ `par_iter` may run serial anyway (fine — overhead avoided).

### 4. The default heuristic assumes **uniform** per-item work — wrong for tiles `[2-1, direction confirmed]`
The default batch heuristic "assumes each entity has roughly the same amount of work, which may not hold in
every workload." The official example uses `BatchingStrategy::fixed(32)` for *cheap* uniform work
(vector negate). **Correct direction:** cheaper per-item work → **larger** batches (amortize scheduling);
variable/expensive per-item work → **smaller/more** batches so work-stealing balances the load.
*Source: `examples/ecs/parallel_query.rs`; docs.rs `BatchingStrategy`; Cheat Book.*
→ **For Geo:** tile decode cost varies wildly (a tiny tile vs. a dense photogrammetry tile). Use an
**explicit `BatchingStrategy` with a small batch** for decode/rebase so the pool work-steals the uneven
load; use a **large batch (or serial)** for the cheap per-tile SSE/cull pass.

### 5. THE GRANULARITY / OVERHEAD TRADE-OFF (the core) `[3-0]`
`par_iter` beats serial **only** when per-item work is expensive enough and/or item count high enough to
amortize fixed scheduling + work-stealing + cache-coherency overhead. There is "a strict minimum batch
size below which the overhead … is heavier than running the entire thing single-threaded." Over-fine
batching is a **measured net loss** — Bevy's own `check_visibility` regressed **216µs → 316µs (~46%)** from
"generating too many small tasks." The 128-element addition example is explicitly "not typically faster
than a normal Iterator."
*Source: PR #4777 (core dev james7132); `parallel_query.rs`; Cheat Book.*
→ **For Geo:** **parallelize only the expensive per-item work (decode + vertex rebase + texture decode).**
Keep cheap per-tile work (distance, SSE, cull flag, LRU stamp) **fused into one system, batched coarsely or
left serial.** Do **not** make a system (or a `par_iter`) per cheap operation.
> ⚠️ **Refuted:** the hoped-for transferable "**≥~1µs / ~10,000-cycle per task**" threshold was **killed in
> verification (1-2)** — it's from general parallel-computing literature, not `bevy_ecs`. **There is no
> magic number: size batches empirically and confirm the win on a flamegraph** (matches AGENTS.md §1B/§1E
> "measure first").

### 6. Storage: **Table (default) for hot iteration; Sparse-Set only for churny markers** `[3-0]`
Table storage (column-oriented SoA, contiguous per archetype) iterates **2–4× faster** than sparse-set and
enables SIMD (PR #14049), at the cost of slow add/remove. Sparse-set is fast add/remove, slow
(pointer-chasing) iteration.
*Source: docs.rs `Component`; Cheat Book; Bevy 0.5 notes; PR #14049.*
> ⚠️ **Refuted (0-3):** "sparse-set is the *recommended* storage for frequently added/removed components" —
> overreach. It's a **candidate to benchmark**, not a default. We keep Table (and avoid the churn that would
> tempt sparse-set — see #2).
→ **For Geo:** all tile components are **Table storage** (the default). We sidestep Table's slow-add/remove
weakness by not adding/removing in the hot loop (#2).

### 7. Archetype moves are expensive; fragmentation hurts iteration `[3-0]`
Adding/removing a Table component **copies all of that entity's Table-component data** to a new location.
Diverse/varied component sets fragment table memory → more arrays to iterate, more cache misses.
*Source: Cheat Book; Bevy 0.5 ECS-V2 notes; Flecs author (Sander Mertens).*
→ **For Geo:** keep the **tile entity's component set uniform** (every tile has the same components) so
they share one archetype and iterate dense. This is automatic if we never add/remove per-tile (#2).

### 8. Keep heavy heap **behind a handle**, not inline in a hot component `[3-0, synthesis]`
Because Table packs each component contiguously, embedding a large `Vec`/`Box`/`Arc`-of-bytes inline bloats
the per-entity stride and forces pointer-chasing during iteration. Store **small fixed-size handles**
(`Arc<TileGeometry>`, ids, indices); the heavy buffers are touched only by the systems that decode/use them.
Bonus: a handle is cheap to copy on the (rare) archetype move.
*Synthesis from the verified Table-SoA mechanics.*
→ **For Geo:** this is exactly the `Arc<TileGeometry>` we already adopted in Phase 0 — vindicated. The
geometry bytes live behind the `Arc`; the hot SSE/cull pass iterates only small fields.

### 9. Change detection skips the **body**, still scans the row `[3-0]`
`Changed<T>`/`Added<T>` are tick-based, per-system ("detects a change only if it hasn't seen it before").
**Caveat:** `Changed<T>` is **not** an `ArchetypeFilter` — a query matching N entities still **iterates all
N** (doing the cheap per-entity tick comparison); only the **expensive loop body** is skipped.
*Source: docs.rs `Changed`/`DetectChanges`; Cheat Book.*
→ **For Geo:** gate the C++ section rebuild with `Changed<TileGeometry handle>` and the decode with
"just-fetched" state, to skip recompute — but pair with a state-enum check, knowing the scan itself is O(N)
cheap, not zero.

### 10. A dedicated non-gameplay World on its **own `std::thread`**, lending idle time to the shared pool `[3-0]`
Bevy's **pipelined renderer** is the first-party precedent: the render World runs on a dedicated
`std::thread::spawn` thread (not a pool task), and **lends its idle time back to the global
`ComputeTaskPool` via `scope()`** so it doesn't burn a core while waiting.
*Source: `pipelined_rendering.rs`; PR #6503 (shipped Bevy 0.10).*
→ **For Geo:** run the Geo World on the **ADR-0012 stream thread** (a `std::thread`), advance its schedule
there, and use the **shared global `ComputeTaskPool`** for `par_iter` — exactly the pipelined-renderer
pattern. **This resolves the "unify the pools" open question:** gameplay and Geo share one pool; no `rayon`.

## What this dictates for the Geo terrain design (summary of constraints)

1. **One Geo `World` on the stream thread**, separate from gameplay's World (keeps the determinism split),
   sharing the **global `ComputeTaskPool`**.
2. **Tiles = entities; components small + Table-stored; geometry behind `Arc`.**
3. **No structural changes in the per-frame loop** — pre-spawned tile pool, in-place field mutation, batched
   end-of-step spawn/despawn (one `ApplyDeferred`).
4. **Parallelize the expensive work (decode/rebase/texture) via `par_iter` with an explicit small/variable
   `BatchingStrategy`;** fuse cheap per-tile work into one coarsely-batched/serial system.
5. **`Changed<T>` to skip recompute**, knowing it still scans (cheap) all matched rows.
6. **Tree traversal (SSE descent) stays sequential** (pointer-chasing) — it only *marks* state; the leaf
   work fans out.
7. **Measure, don't guess** the batch sizes / break-even — no transferable threshold exists.

## Open questions — must be answered empirically on target hardware

- **`ComputeTaskPool` sizing** when gameplay + Geo co-reside: how `bevy_tasks` partitions logical cores
  across Compute/IO/AsyncCompute pools, and how to cap it so Geo + gameplay + Unreal's render/RHI threads
  don't oversubscribe. (Sources confirm `par_iter` uses `ComputeTaskPool::thread_num()` but not the default
  formula.)
- **Per-tile decode break-even**: the actual µs at which `par_iter` beats serial for our decode/rebase, and
  the fixed batch size that minimizes overhead at expected tile counts — a microbenchmark, not a literature
  number.
- **Cross-World pool contention vs. gameplay determinism**: does the Geo thread lending time to the shared
  pool risk priority inversion / jitter against gameplay's fixed-step budget under load? Is a *separate*
  capped pool ever warranted over sharing the global one? (Geo is non-authoritative so its jitter is not a
  *correctness* risk, but it can steal pool time from gameplay.)
- **Structural-change avoidance, measured**: confirm the pre-spawned-pool + in-place-mutation pattern beats
  spawn/despawn-per-tile, and the cost of staging churn into one end-of-step `ApplyDeferred`.

## Sources (primary unless noted)
- `docs.rs/bevy_ecs` 0.18.1 (Component storage, change detection, executor); `MultiThreadedExecutor`,
  `ApplyDeferred`, `BatchingStrategy`, `Changed`/`DetectChanges`.
- Bevy source: `crates/bevy_ecs/src/query/par_iter.rs`, `src/batching.rs`,
  `crates/bevy_render/src/pipelined_rendering.rs`.
- Core-dev PRs: #4777 (par_iter overhead / min batch), #6503 (pipelined rendering), #16782 (auto sync
  points), #14049 (dense-iteration SIMD).
- Secondary, each corroborated by primary: Unofficial Bevy Cheat Book (storage, par-iter, change detection),
  DeepWiki Bevy source mirror, Flecs author commentary on fragmentation.
- Refuted/again: the "~1µs/10k-cycle" task threshold (Springer parallel-computing chapter — not bevy-specific).
