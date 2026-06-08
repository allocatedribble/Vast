# ADR-0012 — Poll-Based FFI / Decoupled Sim Thread

- **Status:** Accepted
- **Date:** 2026-06-06
- **Deciders:** Vast architecture
- **Amends:** [ADR-0005](ADR-0005-Bevy-ECS-Authority.md) — the single-owner-thread invariant
  (`is_owner_thread`, the historical §11 runtime rule) is **relocated off the Unreal game thread**, and the
  FFI *advance* call (`logi_world_tick`, `logi_*_world_step`) becomes a **non-blocking poll**. The
  one-directional, *Unreal-pulls-a-snapshot, never-mutates* contract of [AGENTS.md](../../AGENTS.md) §1/§1C
  is **preserved and in fact completed** — see Context.
- **Relies on:** [ADR-0007](ADR-0007-Actorless-Rust-Terrain.md) — render data is non-authoritative; this
  is what *permits* the Geo engine to run fully async with no determinism obligation.
- **Most affects:** [ADR-0009](ADR-0009-Native-Rust-3DTiles-Stack.md) (the streaming 3D-Tiles stack).
- **Plan:** [Docs/Plans/Async-Sim-Decoupling-and-Geo-Performance.md](../Plans/Async-Sim-Decoupling-and-Geo-Performance.md)
  — Phase 0 is **gated on this ADR being accepted.**

## Context

Both engines today advance **synchronously on the Unreal game thread**:

- Gameplay: `ULogiBevySubsystem::TickSimulation` (TG_PrePhysics) calls `logi_world_tick`, which runs the
  whole schedule **and** publishes the render snapshot — on the game thread.
- Geo: `ALogiBakedGeoTerrain::Tick` (an `AActor::Tick`, ~30 Hz) calls `logi_*_world_step`, which runs the
  entire drain → glTF-decode → per-vertex f64 rebase → SSE traversal → evict **inline, on the game thread**.

This has three problems, all rooted in *who drives the loop*:

1. **All heavy CPU work lands on the frame thread.** A burst of completed tile fetches decodes and rebases
   synchronously inside one `step`, hitching the frame. The single most expensive work in the project runs
   on the one thread that must not stall.
2. **The "Rust is parallel" goal is unrealized.** Only HTTP fetch is off-thread. Decode, rebase,
   traversal, and the gameplay schedule are serial on the game thread; the gameplay `MultiThreaded`
   executor is even spun up but dormant (no `par_iter`, ≤1 system per phase) — pure coordination overhead.
3. **It half-contradicts §1C.** §1C already says *"Unreal pulls one batched per-frame snapshot (§3.9); it
   does not reach in and mutate."* The snapshot read honors that — but the **advance** that produces it is
   still *driven by* the game thread. The game thread therefore bears the full simulation cost while §1C's
   intent (Unreal as a pure consumer) is only half-met.

Two facts make the fix safe and asymmetric:

- **Geo is non-authoritative** (ADR-0007; audit H2 "rendering needs no determinism", won't-fix). It is free
  to run fully async, at its own cadence, with unconstrained intra-step parallelism.
- **Gameplay must stay deterministic** (North Star §0; ADR-0001 Determinism, preserved by ADR-0005). Its
  decoupling must preserve fixed-step + input-order, or replay/snapshot equality breaks.

And the machinery largely exists: the gameplay [`SnapshotRing`](../../Rust/crates/logi_runtime/src/lib.rs)
already implements a retained, latest-state handoff (3 slots; a publish fence that only overwrites a slot
that is *retired and not the current latest*; `find(0)` = latest; `logi_buffer_release` to retire; keeps up
to 3 live so a mid-read consumer is never clobbered). **Geo has none of it** — its `tile_mesh` pointers are
valid only "until the next `step`", a contract that cannot survive an async producer.

## Decision — the new law

> **The FFI advance call becomes a poll. Unreal, on its game thread, only *pushes* the latest view/input
> and *pulls* the latest published snapshot — it never drives, and never blocks on, the simulation or
> streaming loop. Each world is advanced by exactly one Rust-owned thread; Vast-rs is internally as parallel
> as its determinism contract allows.**

Concretely:

1. **Poll, not step.** The per-frame FFI surface, callable only from the Unreal game thread, is three cheap
   non-blocking categories:
   - **Push (producer):** `set_view(camera, fov, viewport, max_sse)` for Geo (newest-wins, a seqlock slot);
     `push_input(UnrealFrameInput)` for gameplay (a bounded SPSC queue, drained deterministically).
   - **Poll (consumer):** `poll(&out)` returns the **latest complete** snapshot with a `NEW`/`SAME`/`EMPTY`
     status, so an unchanged frame is a no-op the C++ side skips rebuilding.
   - **Release (consumer):** `logi_buffer_release(id)` retires the slot.

2. **One Rust-owned mutator per world.** The owner thread **relocates** from the game thread to a Rust sim
   thread (gameplay) and a Rust stream thread (Geo). The single-mutator invariant is **preserved**, not
   removed: `is_owner_thread()` is re-pinned to that thread, and it remains the only thing that mutates the
   `World` / `TilesetWorld`.

3. **Internally parallel.** A bounded, core-aware job pool (reuse `bevy_tasks::ComputeTaskPool`; unify the
   ad-hoc geo threads into it) fans out the embarrassingly-parallel work — per-tile decode, per-vertex
   rebase, procgen, and gameplay `par_iter`. **Geo: unrestricted. Gameplay: parallel only where systems are
   conflict-correct and the result stays deterministic.**

4. **The cross-thread FFI surface is a closed, race-free set.** Exactly `{set_view, push_input, poll,
   release}` cross threads, each via a **lock-free seqlock / ring** or a **bounded SPSC queue** — *not*
   shared mutable state. §1C's "no shared mutable state across the boundary → no data race" therefore still
   holds: the producer and consumer touch disjoint, fenced slots, never the same mutable datum.

5. **Retained, refcounted handoff — no use-after-free across the ABI.** Geo holds its per-tile geometry as
   **`Arc<TileGeometry>`**: a polled frame clones the `Arc`s of the tiles it references, so eviction merely
   drops the *world's* reference and the buffer is freed only when the last frame holding it is released. The
   `Arc` clone is the lifetime guarantee, so Geo's cross-thread slot is a single **latest-wins mailbox**
   (`Mutex<Option<frame>>`, no `release`); gameplay, whose ordered deterministic snapshot stream needs
   retained intermediates, uses the **`SnapshotRing`** (≥3 slots, publish/retire fence, `logi_buffer_release`).
   (The `Arc` also deletes the audit's M11 texture-copy: the decoder produces one shared `Arc<[u8]>`.) The FFI
   still hands raw pointers into those buffers — zero-copy across the boundary, with the `Arc` guaranteeing
   lifetime.

6. **Determinism split.** Gameplay advances on a **fixed-timestep** loop on its sim thread, draining a
   **timestamped** input queue at each step boundary, so a recorded input sequence replays bit-identically
   regardless of wall-clock; the render side may interpolate between the two latest snapshots for smoothness.
   Geo runs **free** (latest-view-wins, no fixed step, no replay obligation).

```
            UNREAL GAME THREAD                         RUST-OWNED THREADS
  set_view / push_input  ───────────────►  ┌─ stream thread (owns TilesetWorld) ── fans → job pool
  poll_latest  ◄──── Arc handoff ─────────  └─ sim thread   (owns World, fixed-step) ─ fans → job pool
  release(old_id) [gameplay ring only] ──►       (HTTP fetch pool unchanged)
   builds only tiles whose id changed          Geo: latest-wins mailbox; gameplay: SnapshotRing
```

## Consequences

**Positive**
- **Game-thread FFI cost drops to O(visible)** — push a small POD, read a snapshot, release. No decode,
  rebase, or traversal on the frame thread.
- **Heavy work is parallel and off-frame** — decode/rebase/procgen fan across the job pool; the frame thread
  is decoupled from simulation cost. Directly removes the dominant visual-hang source.
- **§1C is completed, not violated** — Unreal becomes a pure consumer; the only mutator is one Rust thread.
- **`SAME`-status polls eliminate redundant rebuilds** — most frames the tile/instance set is unchanged.
- **Sim cadence decouples from frame rate** — a slow sim re-presents the last snapshot; a fast sim drops
  intermediates (already counted as `dropped_frames`).

**Negative / costs**
- **A retain/lifetime layer to build for Geo** (`Arc<TileGeometry>` + the ring) and to **property-test**
  across the publish/poll/release/evict interleaving. This is the real new engineering surface.
- **Sim/stream-thread lifecycle** to manage: start on world-create, signalled stop + **clean join** on
  destroy / PIE end / Live Coding reload — no detached threads leaking across editor sessions.
- **Determinism now depends on input timestamping**, not call timing — golden/replay tests must drive the
  input queue, and parallel ECS execution must be proven conflict-correct (keep `SingleThreadedDeterministic`
  for replay).
- **One more ABI surface to version and profile** (`set_view`/`push_input`/`poll`/`release`), and
  cross-thread debugging is harder than a single synchronous call.

## Alternatives considered

- **Keep the synchronous `step` (status quo).** Simplest, but the heavy work stays on the game thread →
  hitches, and "Vast-rs as parallel as possible" is unreachable. Rejected — it is the problem.
- **Run the heavy work on Unreal's task graph (C++ threads calling into Rust).** Would force the Rust side
  to be thread-safe for arbitrary external callers and shatter the single-owner invariant. Rejected —
  keep exactly one Rust owner per world; Unreal stays a consumer (ADR-0005 §1).
- **Mutex the shared world for cross-thread access.** Contention and priority inversion on the game thread
  (the one place we must never block). Rejected in favor of lock-free latest-state handoff.
- **Epoch/generation reclamation instead of `Arc`.** Workable, but adds manual bookkeeping and a
  reclamation race surface. `Arc` is simpler, allocation-stable, and its per-tile-per-frame refcount traffic
  is negligible. Chose `Arc`.
- **Decouple Geo only; leave gameplay synchronous.** Half the win — the gameplay tick still hitches at
  scale. Chose to decouple both, with the determinism split (§ Decision 6) that lets gameplay stay
  replay-safe.

## Addendum A (2026-06-06) — `bevy_ecs` is the Geo orchestration layer (Geo crates gain a `bevy_ecs` dep)

Decision 3 ("internally parallel … reuse `bevy_tasks::ComputeTaskPool`") is made concrete: **terrain's
intra-step multi-threading is orchestrated by `bevy_ecs` — not `rayon`.** The Geo stream thread owns a
**dedicated `bevy_ecs::World`** (the state ADR-0012 already puts under the stream thread) and runs a Geo
`Schedule`; tiles are entities and the heavy per-tile work fans out via `Query::par_iter` over the **shared
global `ComputeTaskPool`**. Design + the verified constraints: [the plan §2A](../Plans/Async-Sim-Decoupling-and-Geo-Performance.md)
and [Docs/Research/ECS-Multithreading-for-Geo.md](../Research/ECS-Multithreading-for-Geo.md).

**The boundary change this records:** the Geo crates (`Rust/crates/geo/*`) were deliberately **bevy-free**
(only `logi_sim`/`logi_runtime` used `bevy_ecs`). The orchestration World gives `logi_3dtiles_scene` (and
thus `logi_geo.dll`) a `bevy_ecs` + `bevy_tasks` dependency. This is **consistent with ADR-0005's "one ECS /
one truth language"** — Geo is part of the simulation that feeds the renderer — and it stays within
ADR-0012's threading contract: the Geo `World` is a **separate** World from gameplay's (preserving the
fixed-step-vs-free-running determinism split, Decision 6), owned by the stream thread, lending idle time
back to the shared pool via `scope()` (Bevy's pipelined-renderer pattern).

**Why ECS over `rayon` (supersedes the earlier "rayon job-tier" alternative):** `bevy_ecs` is already a
dependency, so reusing it means **one shared `ComputeTaskPool`** (gameplay + Geo) rather than a second
`rayon` pool that would oversubscribe cores — it directly resolves the plan's pool-unification open item.
It also brings change detection (`Changed<T>` to skip unchanged tiles) and access-conflict system
scheduling for free. This **revises the original review's "ECS isn't the lever for Geo" verdict**: that was
right about the *failure modes* (per-op components, archetype churn, over-fine parallelism), and the
research's constraints are exactly what avoid them —
1. **No structural changes in the hot loop** (spawn/despawn/insert/remove are `ApplyDeferred` barriers):
   pre-spawned tile-entity pool + in-place field mutation; churn batched into one end-of-step flush.
2. **Small, Table-stored components; heavy geometry behind `Arc`** (the Phase-0 `Arc<TileGeometry>` retain).
3. **Right-sized systems**: the expensive `decode` is the `par_iter` beneficiary (small `BatchingStrategy`,
   work-stealing the variable tile cost); cheap per-tile work is **fused** into one coarsely-batched system —
   never a `par_iter` per cheap op (a measured net loss).
4. **Batch sizes tuned empirically** (no transferable µs threshold survived verification).

**Consequence / cost:** `logi_geo.dll` grows (links `bevy_ecs`/`bevy_tasks`); the Geo World sharing the
global pool introduces a **cross-World contention** risk against gameplay's fixed-step budget — Geo's own
jitter is harmless (non-authoritative) but it can steal pool time from gameplay, so it must be measured
under load, with a separate capped pool as the fallback (plan §7).

## References

- Plan: [Async-Sim-Decoupling-and-Geo-Performance.md](../Plans/Async-Sim-Decoupling-and-Geo-Performance.md).
- Research: [ECS-Multithreading-for-Geo.md](../Research/ECS-Multithreading-for-Geo.md) (the verified `bevy_ecs`
  multithreading constraints Addendum A rests on).
- [ADR-0005](ADR-0005-Bevy-ECS-Authority.md) (§1 boundary, amended owner-thread), [ADR-0007](ADR-0007-Actorless-Rust-Terrain.md)
  (render non-authoritative — permits async Geo), [ADR-0009](ADR-0009-Native-Rust-3DTiles-Stack.md) (the
  streaming stack), [ADR-0004](ADR-0004-No-Compatibility-Code.md) (no compatibility code — `step` is replaced outright by `poll`, not bridged).
- [AGENTS.md](../../AGENTS.md) §0 (determinism), §1 (the one-directional boundary), §1C (no shared mutable
  state; Unreal pulls a batched snapshot), §3.9 (the snapshot-extract contract).
- `logi_runtime` `SnapshotRing` / `is_owner_thread` — the existing retained-handoff precedent this generalizes.
