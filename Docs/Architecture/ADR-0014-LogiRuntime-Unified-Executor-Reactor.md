# ADR-0014 — LogiRuntime: Unified Two-Lane Executor + Reactor

- **Status:** Proposed (spike-gated; see the plan's roadmap). **Implementation status (re-eval
  2026-06-09):** LANDED = physical-core pool sizing (both formulas), counter-based RNG, the Geo poll lane,
  the cyper h2/h3 I/O lane, SingleThreaded executors for the few-system schedules (Geo via ADR-0018;
  gameplay 2026-06-09), and a cap on the compio blocking pool. **NOT built** = the `CoreArbiter`,
  `WorkSource`/`IoHarvest<T>`, `LogiSimRunner` (gameplay still ticks synchronously on the UE game thread,
  ABI v6), LogiExec, the morsel production wiring, and the precomputed-stage executor. The Plan
  §7.1/§7.2/§7.3 is the ledger of record; this body speaks in DESIGN tense unless marked landed.
- **Date:** 2026-06-09
- **Deciders:** Vast architecture
- **Amends:** [ADR-0012](ADR-0012-Poll-Based-FFI-Decoupled-Sim-Thread.md) — the poll model is generalized
  from "one Rust-owned mutator per world" into **one runtime that owns the machine's CPU + I/O budget**,
  and ADR-0012's *gameplay* half (sim off the Unreal game thread) is **specified to complete** here
  (Decision 6 — still pending in code). Also amends [ADR-0005](ADR-0005-Bevy-ECS-Authority.md) — the single
  shared `ComputeTaskPool` becomes the **arbitrated CPU lane**. The original "narrows ADR-0004 with a
  vendored `bevy_ecs` patch" clause is **withdrawn** (re-eval 2026-06-09): the round-4 research confirmed
  `ScheduleGraph::conflicting_systems()` and the dependency DAG are `pub`, so the precomputed-stage
  executor needs **no fork** — a vendored patch is at most an optional micro-optimization, and
  [ADR-0004](ADR-0004-No-Compatibility-Code.md) is not narrowed by this ADR. (The only vendored patch in
  the tree is `cyper`, for a `TransportConfig` seam — unrelated.)
- **Relies on:** [ADR-0007](ADR-0007-Actorless-Rust-Terrain.md) — render/geo data is non-authoritative, which
  is what *permits* the I/O lane to run free of the determinism contract.
- **Most affects:** the gameplay sim (`logi_sim`/`logi_runtime`) and the Geo stream (`logi_3dtiles_*`),
  which today fan out over the shared pool, and the HTTP fetch lane (`logi_3dtiles_io`).
- **Plan:** [Docs/Plans/LogiRuntime-Design.md](../Plans/LogiRuntime-Design.md) — the full research synthesis,
  the component designs, the staged roadmap, and the decisive spikes that gate acceptance.

## Context

ADR-0012 put each world on a Rust-owned thread and fans the heavy work over a single shared
`bevy_tasks::ComputeTaskPool` (`async-executor 1.14.0`, work-stealing). That was the right first move, but
the runtime beneath it is **one general-purpose executor doing a job that is actually three jobs with
opposite optimal schedulers**, and it is missing an I/O reactor entirely:

1. **The CPU executor is priority-blind and not built for our hot loop.** `async-executor` is a reference
   work-stealing executor: freshly-spawned tasks always land on the global queue (source-verified — the
   local-push path is an unimplemented TODO), there is **no task priority**, no LIFO slot, and `par_iter`
   spawns **one task per batch**. For the gameplay sim's flat, uniform, per-frame-barriered iteration over
   (eventually) millions–billions of agents, work-stealing's random steals and per-batch task objects are
   pure coordination overhead — that workload wants a **static morsel cursor**, not a task scheduler.
   Meanwhile the medium-grained, conflict-aware *system DAG* serializes its scheduling decisions through
   `bevy_ecs`'s **single `Mutex<ExecutorState>`** (code-verified). *Source correction (re-eval 2026-06-09):*
   issue #11378 reports MTPC ~470 µs at 6000 systems and frame-rate collapse from ~1000–2000 **concurrent**
   systems — it does not state a "10–70 µs per-system wakeup" (that figure is an OS-magnitude estimate, not
   a sourced number) and our own Exp C measured the whole stock executor at 8.4 µs for N=4 systems; the
   Mutex ceiling is real but it is a *thousands-of-systems* cliff, not a near-term one.
2. **There is no I/O reactor.** The tile fetch lane is a blocking-`ureq` OS-thread pool (`logi_3dtiles_io`);
   it caps at a few-dozen blocking threads and cannot scale to the "massive concurrent connection" target,
   and it is decoupled from the frame's "harvest everything that finished" rhythm.
3. **The thread budget is wrong, today.** Both towers size the pool `available_parallelism() − 2` (logical
   cores) in two **duplicated** formulas (`resolve_worker_thread_count` / `geo_compute_threads`). *Premise
   correction (re-eval 2026-06-09 — UE source-verified):* UE 5.8 sizes its TaskGraph off **logical−2**
   (`NumberOfWorkerThreadsToSpawn` returns `NumberOfCoresIncludingHyperthreads−2` on any SMT box — the
   earlier "UE uses physical, NumberOfCores()−1" claim here was wrong). The real picture on a
   12-physical/24-logical box was therefore **two ~22-thread pools (ours + UE's) contending for 12 physical
   cores — ~2× mutual over-subscription** — plus UE's 3 always-hot named threads. The physical-core fix
   stands, but on its true grounds: don't stack a second near-logical pool on UE's, and HT siblings share
   execution ports so logical-count workers add contention, not throughput, on cache-/SIMD-bound paths.

A five-wave deep-research effort (≈25 Opus agents; see the plan) microscopically compared the options —
ntex (a custom bevy runtime), a reqwest bevy backend, work-stealing vs heartbeat (spice/chili) vs
thread-per-core (Seastar/glommio/monoio) vs morsel-driven (HyPer) vs BSP, readiness vs completion I/O
(io_uring/IOCP/`compio`), and "build our own" at every layer — and converged on a single shape.

## Decision — the new law

> **One runtime, "LogiRuntime", owns the machine's CPU and I/O budget as TWO cooperating lanes under ONE
> core arbiter, joined at the per-frame poll. We COMPOSE the two proven cores — a work-stealing CPU
> executor and a completion-based I/O reactor — and BUILD only the thin coordinator that is genuinely ours:
> the core arbiter, the typed frame-harvest hand-off, and the two CPU-regime engines. We do NOT build a
> general scheduler or a general reactor from scratch; we do NOT put CPU and I/O on one pool; we do NOT
> shard the ECS World per core.**

Concretely:

1. **The core budget — one source of truth.** *(Amended by the 2026-06-09 re-eval: the arbiter-as-TYPE is
   demoted — what this decision actually requires is (a) ONE shared `compute_threads()` formula, (b) hard
   caps on every non-pool thread source, and (c) thread priorities; a `CoreArbiter` subsystem is built only
   if the lanes measurably contend after the gameplay decouple lands. The per-frame O(1) decision below is
   the design for THAT eventual type, not a description of shipped code.)* The budget is sized from
   **physical** cores (`num_cpus::get_physical`, later `hwlocality` for NUMA/affinity), partitioning
   `cpu_sim + io + blocking_fetch + ue_reserved = usable_physical`, **reserving 3 for UE's named threads**.
   **Landed so far:** both pool formulas size `physical − 3` (still deliberately duplicated — the
   `compute_threads()` collapse lands with the LogiSimRunner work), and the compio blocking pool is capped
   (`CYPER_BLOCKING_POOL_LIMIT = 4`; its compio default was 256 — a thread bomb outside any budget).
   **Not landed:** the formula collapse, the thread priorities (compio `ABOVE_NORMAL`; blocking-fetch
   `BELOW_NORMAL` + EcoQoS → E-cores on hybrids — currently every spawned thread runs NORMAL), and any
   arbiter type. Per-frame decision when built: **O(1)** (read two atomics → two bools); never migrates
   threads to rebalance — the parked I/O driver rides idle CPU-lane cores via OS scheduling + priority.

2. **The CPU lane is two regimes, matched to two workloads, plus a priority overlay:**
   - **(a) Regime H — the flat agent loop → morsel-BSP.** A `morsel_for_each` runs *inside* a system body,
     reading SoA columns directly via the already-public `Table::get_data_slice_for::<T>()`, and spawning
     **exactly P long-lived workers that `fetch_add` one shared cursor** (zero per-morsel task objects, zero
     steals on the uniform path). Two cursors give O(1)/morsel render-priority; a GSS tail-shrink flattens
     the imbalance tail; determinism is by decomposition (disjoint row ranges + counter-RNG keyed by row +
     fixed index-shaped reduction trees). The executor and schedule are **untouched**.
   - **(b) Regime S — the system DAG → a precomputed-stage executor.** A custom `impl SystemExecutor`
     compiles the conflict graph into barrier-separated parallel stages **once** at `init`, then at `run`
     walks stages — **no `Mutex`, no per-round `can_run` scan, no per-system OS wakeup** (O(stages) + O(n)
     spawns, vs stock O(n·rounds) under one lock). Slotted via the `set_executor` call already at
     `schedules.rs:141`.
   - **(c) Priority overlay — "LogiExec".** Where gameplay + geo + I/O continuations contend on one pool, an
     **extension of `async-executor`** (vendor its ~600-LOC core, keep `async-task` untouched) adds **K=4
     priority bands** (Frame > Gameplay > Render > Background) via async-task's `metadata` hook + an O(K)
     band scan + DWRR anti-starvation + a *stealable* 1-deep LIFO slot. This is a **lane-level** priority
     overlay; it is built only when the contention is measured (YAGNI gate).

3. **The I/O lane is a completion reactor harvested at the poll.** *(The "one parked thread, no async
   executor" sentence below describes the FILE/CUSTOM-OP + cancellation MODEL, not the socket transport
   that shipped — see the Realized note: the productized lane is compio-net/cyper on its OWN runtime
   thread, draining over a channel. Exp B cont. found compio's raw socket ops are `pub(crate)`, so the
   bare-proactor shape is not the socket path.)* `compio`'s `Proactor` (IOCP now, io_uring later,
   `polling` fallback) runs on **one parked thread**, driven *without* an async executor in a
   `push → poll(frame_budget) → pop_multishot → cancel` loop, feeding **sans-IO** protocol state machines.
   Cancellation is **RST_STREAM + proactor cancel**, with the owned recv buffer **retained until the cancel
   completion** (the completion-I/O safety rule). It replaces the blocking-`ureq` pool **behind the existing
   `TileContentSource` trait**, so the synthetic/test lane and the disk cache are untouched.
   - **Realized (2026-06-09): both halves landed via `cyper` on compio — as a full compio `Runtime` on one
     dedicated owned thread (`CyperSource`), flume-drained.** The h2 lane shipped first; the
     **"quinn-proto/QUIC later" half is now also done** — enabling cyper's `http3` + `http3-altsvc` gives
     **HTTP/3 over QUIC through `compio-quic` (= `quinn-proto`, sans-IO, on the compio reactor — no tokio
     RUNTIME is constructed; the tokio crate rides only as transitive hyper-util/h2 io-compat shims — and no
     high-level `quinn` crate)**, auto-upgraded via Alt-Svc and the Google-tiles default. QUIC cancellation
     is the QUIC stream reset (the h3 analogue of RST_STREAM), same task-drop mechanism; since the re-eval,
     an h3 transport failure also **quarantines the host's Alt-Svc entry and falls back to h1/h2** (vendored
     seam), so a mid-session UDP block degrades instead of stranding the lane. The live keyed-P3DT A/B found
     h2 and h3 both **~parity with `ureq` on real bodies** (the win is cold-connect/RTT, not bandwidth);
     details + the `fetch_backend` runtime knob (ABI v11) in the plan §7.1.

4. **The frame-harvest is a typed hand-off.** A `WorkSource` trait (`drain_ready`) is implemented by **both**
   the I/O completion queue and the ECS deferred-command buffer, so the poll drains them uniformly. Anything
   the non-deterministic lanes produce crosses into the deterministic sim only as an `IoHarvest<T>` — a
   `!Send`, private-`commit_into` type consumable **only at the frame checkpoint**, making "non-deterministic
   data touched the sim mid-step" a **compile error** (this is the control-flow companion to ADR-0016's
   `Promoted<T>` data boundary).

5. **The poll loop is ADR-0012 generalized.** The Geo `run_stream` loop *is* the unified poll: UE pushes
   view/input and pulls the latest retained snapshot; the arbiter's O(1) decision gates the work; I/O
   completions drain via `WorkSource`; the deterministic sim steps `0..n` fixed sub-steps; the snapshot is
   retained. No Unreal types cross; Rust never calls Unreal.

6. **Gameplay moves off the Unreal game thread (completing ADR-0012). [PENDING — not built]** A
   `LogiSimRunner` (mirroring the existing `GlobeStreamer`) owns the gameplay `World` on a `"LogiSim"`
   thread; `logi_world_tick` splits into `push_input` (a bounded, **timestamped** SPSC, newest-wins) +
   `poll` (reads the snapshot ring). Gameplay stays fixed-step deterministic; the ABI bumps 6→7. Two
   corrections to the "mechanical mirror" sizing (re-eval 2026-06-09): (a) the `SnapshotRing` is today an
   owner-thread-only structure with **no atomics** — crossing it between a sim thread (publish) and the
   game thread (find/retire) needs a real synchronization layer, designed and property-tested, not a copy;
   (b) the Geo loop's panic policy (catch, skip the step, continue — legal because Geo is
   non-authoritative) must NOT be mirrored: a panicked fixed step in the deterministic sim hard-stops
   (replay integrity), never skip-and-continues.

```
        UNREAL GAME THREAD                         LOGIRUNTIME  (Rust-owned)
  push_view / push_input ───────────►   ┌── CORE ARBITER  (physical cores − UE 3; O(1)/frame) ──┐
  poll(latest snapshot) ◄── Arc ───────  │  CPU lane (work-stealing)        I/O lane (completion) │
  release(old) [gameplay ring]           │  ┌ Regime H: morsel-BSP ┐  ┌ compio Proactor ───────┐ │
                                         │  │ Regime S: stage-exec  │  │ 1 parked thread,        │ │
                                         │  │ + LogiExec K-bands    │  │ sans-IO rustls/h2/QUIC  │ │
                                         │  └──────────┬────────────┘  └───────────┬─────────────┘ │
                                         │   frame-harvest: WorkSource.drain_ready → IoHarvest<T>   │
                                         │   → step deterministic sim (0..n) → retain snapshot      │
                                         └───────────────────────────────────────────────────────── ┘
```

The scaling targets this serves — **millions→billions of agents** on the CPU lane and **massive concurrent
connections** on the I/O lane — are addressed by the companion ADRs: [ADR-0015](ADR-0015-Simulation-LOD.md)
(simulation-LOD bounds the per-tick active set so the CPU lane never has to touch a billion rows) and
[ADR-0016](ADR-0016-Per-Lane-Determinism.md) (the determinism scope that lets the far field run free).

## Consequences

**Positive**
- **Each workload runs on the cheapest correct mechanism** — a flat agent loop on a static morsel cursor
  (no steals, O(1) dispatch, O(P) space, SIMD-friendly), a system DAG on a lock-free precomputed walk
  (no single-`Mutex` ceiling), I/O on a completion proactor (the CQ *is* the frame-harvest queue).
- **Priority becomes expressible** without losing work-stealing's O(1) ops (K bands by partition, not a heap).
- **The thread budget becomes correct** — *(scope, re-eval 2026-06-09: what landed bounds the shared
  compute pool to `physical−3` — removing the ~2× two-near-logical-pools contention — and caps the compio
  blocking pool; the blocking-fetch workers (≤64), the cyper runtime thread, and the stream thread are not
  yet under one budget, and priorities are unset. The full guarantee arrives with the shared formula + caps
  + priorities of Decision 1, not from what is already shipped.)*
- **One CPU budget to reason about**, shared with ECS; I/O harvested once per frame, cancellable
  (RST_STREAM, O(1) any phase).
- **ADR-0012 is completed** — gameplay finally leaves the game thread; both lanes share one poll model.

**Negative / costs**
- ~~**A vendored `bevy_ecs` patch** to host the precomputed-stage executor (the conflict bitsets are
  `pub(crate)`; an external crate cannot see them).~~ **Withdrawn (re-eval 2026-06-09):** the premise was
  refuted by the round-4 research — `ScheduleGraph::conflicting_systems()` + the dependency DAG are fully
  `pub` at build time, so the executor builds with **no fork**; a vendored patch is at most an optional
  in-band micro-optimization, and the ADR-0004 narrowing this bullet justified is withdrawn with it.
- **LogiExec and the compio lane are real engineering surfaces** — the K-band executor (vendored
  async-executor core + async-task), the proactor harvest loop, and the **owned-buffer discipline**
  (completion I/O cannot free a buffer the kernel may still write — the cancellation-safety rule).
- **Several pieces are explicitly spike-gated** (morsel-vs-`par_iter`, compio-vs-tuned-ureq, whether
  LogiExec earns its place yet). The arbiter + physical-core fix is the only "build now, no spike" piece.
- **More ABI + thread lifecycle to version and join cleanly** (the `LogiSim` thread, the compio thread) —
  the same discipline ADR-0012 already established for the stream thread.

## Alternatives considered

- **Keep `async-executor` + the blocking-ureq pool (status quo).** Leaves priority unexpressed, the I/O
  lane unscalable, the thread budget over-subscribed, and the system-DAG behind the single `Mutex`. Rejected
  — it is the problem.
- **One unified pool for CPU and I/O (tokio-style).** A multi-millisecond `par_iter` blocks I/O harvest and
  cancellation mid-frame (the PostHog rayon+tokio freeze, p99 → 2.5 s), and an I/O stall steals a sim core.
  Rejected — the two tail-latency profiles are mutually destructive on shared threads.
- **Full thread-per-core (Seastar/glommio/monoio): shard the World per core.** Apache Iggy ran exactly
  this combination (thread-per-core + ECS) and the **ECS-under-TPC** half collapsed on cross-shard/
  background mutation (precision, re-eval 2026-06-09: Iggy KEPT thread-per-core and retreated to a
  shared-something left-right model — the failure is the combination, not TPC itself); it also requires
  rewriting `bevy_ecs`'s `Send` work-stealing executor and fights the per-frame determinism barrier.
  Rejected for the ECS (kept as a *later, geo-only* per-socket I/O option under NUMA).
- **ntex with a custom bevy runtime.** ntex is thread-per-core with `!Send`, reactor-owning I/O; "on bevy"
  degenerates to running ntex's own reactor on a bevy-spawned thread bridged by channels — 4–8 weeks + an
  8-crate fork against an archived reactor. Rejected (see the HTTP-library research, plan References).
- **A bevy backend for reqwest.** reqwest has no runtime-abstraction seam; a backend is a deep fork of its
  whole transport half, strictly dominated by building on hyper/compio directly. Rejected.
- **Heartbeat fork-join (spice/chili) as the CPU lane.** The right tool for *recursive* fork-join, the wrong
  tool for the *flat-uniform* agent loop (no slice support; the rustc integration regressed +35–55% and was
  abandoned). Kept only as a straggler escape-valve and for genuinely-recursive Rust lanes (BVH, tile-tree).
- **Build our own executor and reactor from scratch.** ~9–18 person-months to re-derive `async-task`'s
  audited `unsafe` and a cross-platform proactor, vs ~1.5–3 to compose them and build the coordinator. The
  coordinator (arbiter + frame-harvest + the two regime engines) is the only genuinely-ours part — that is
  where the invention budget goes.

## References

- Plan: [Docs/Plans/LogiRuntime-Design.md](../Plans/LogiRuntime-Design.md) (the research synthesis, the
  component designs with verified seams, the O-notation tables, the roadmap, and the decisive spikes).
- Companion ADRs: [ADR-0015](ADR-0015-Simulation-LOD.md) (the CPU-lane scaling answer),
  [ADR-0016](ADR-0016-Per-Lane-Determinism.md) (the determinism scope), [ADR-0017](ADR-0017-GPU-Resident-Statistical-Offload.md)
  (the deferred GPU escalation).
- [ADR-0012](ADR-0012-Poll-Based-FFI-Decoupled-Sim-Thread.md) (the poll model this generalizes; Addendum A's
  `ComputeTaskPool` unification this arbitrates), [ADR-0005](ADR-0005-Bevy-ECS-Authority.md) (gameplay
  authority), [ADR-0007](ADR-0007-Actorless-Rust-Terrain.md) (geo non-authoritative → free I/O lane),
  [ADR-0004](ADR-0004-No-Compatibility-Code.md) (the vendored-patch posture).
- Key seams (verified against `bevy_ecs 0.19.0-rc.2` + the repo; in-repo anchors are SYMBOLS — the live
  editor drifts line numbers): `Table::get_data_slice_for` (`storage/table/mod.rs:267`), `SystemExecutor` +
  `set_executor` (`schedule/executor/mod.rs:30`, `schedule/schedule.rs:544`),
  `ScheduleGraph::conflicting_systems` (pub — the no-fork executor seam), the two pool formulas
  (`logi_sim::resolve_worker_thread_count`, `geo_world::geo_compute_threads`), the `TileContentSource`
  trait (`logi_3dtiles_io/src/lib.rs`), the `GlobeStreamer` mirror (`logi_3dtiles_scene/src/stream_thread.rs`).
