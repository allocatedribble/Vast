<!-- Synthesis of a five-wave, ~25-Opus-agent deep-research effort (2026-06-08/09): HTTP-on-bevy options →
runtime survey → executor/reactor O-notation + build-our-own → concrete implementation-grade design. This is
the reference plan for ADR-0014..0017. -->

# LogiRuntime — Design & Plan

- **Status:** Proposed (Phase 0 is build-now; later phases are spike-gated)
- **Date:** 2026-06-09
- **Decisions recorded by:** [ADR-0014](../Architecture/ADR-0014-LogiRuntime-Unified-Executor-Reactor.md)
  (the runtime), [ADR-0015](../Architecture/ADR-0015-Simulation-LOD.md) (CPU-lane scaling),
  [ADR-0016](../Architecture/ADR-0016-Per-Lane-Determinism.md) (determinism scope),
  [ADR-0017](../Architecture/ADR-0017-GPU-Resident-Statistical-Offload.md) (the deferred GPU escalation).
- **Builds on:** [ADR-0012](../Architecture/ADR-0012-Poll-Based-FFI-Decoupled-Sim-Thread.md) (poll model),
  [ADR-0005](../Architecture/ADR-0005-Bevy-ECS-Authority.md), [ADR-0007](../Architecture/ADR-0007-Actorless-Rust-Terrain.md),
  [ADR-0003](../Architecture/ADR-0003-Representation-LOD.md).

## 1. Thesis

LogiRuntime is the runtime beneath all of Vast: the executor + reactor that must serve three workloads that
**complement each other on one machine** — `bevy_ecs` system scheduling, the million→billion-agent gameplay
sim (CPU-bound, deterministic fixed-step), and geo/network streaming (I/O-bound, async, non-authoritative).

The central finding of the research is that **there is no single optimal executor**: the three workloads want
three different schedulers, and the craft is matching each to the cheapest mechanism and unifying them under
**one core budget joined at the frame tick**. So the build-our-own answer is surgical — **compose the two
proven cores (a work-stealing CPU executor and a completion I/O reactor) and build only the thin coordinator
that is genuinely ours**: the core arbiter, the typed frame-harvest hand-off, and the two CPU-regime engines.
That is ~1.5–3 person-months of our code over inherited internals, vs ~9–18 reinventing them.

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
                                         └──────────────────────────────────────────────────────────┘
```

## 2. What was compared (the microscopic survey)

**CPU executor models** (per-op bounds; `n` = items, `P` = workers, `m` = morsel size, `T∞` = span):

| Model | Examples | per-elem overhead | coord / steal cost | cache `Q` | space | fit |
|---|---|---|---|---|---|---|
| Work-stealing (Chase-Lev) | Rayon, async-executor, Tokio | O(1) amort | **O(P·T∞)** steals | Q₁ + O(P·M·T∞/B) | O(S₁·P) | the system DAG (Regime S) |
| Heartbeat / lazy-task | spice, chili, forte | O(1+τ/H) | promotions/H | ~WS | O(S₁·P) | recursive fork-join only |
| Thread-per-core | Seastar, glommio, monoio | O(1) | none (no steal) | partition-local | O(P) | I/O servers; **fights ECS + barrier** |
| **Morsel-driven** | HyPer (Leis 2014) | **O(1)** (1 fetch-add) | **0 steals**, O(n/m) atomics | **Q₁ + O(P·m/B)** | **O(P)** | **the flat agent loop (Regime H)** |
| BSP | Valiant | O(1) | barrier O(log P) | partition-local | O(n/P) | the frame *structure* |

For a flat-uniform barriered loop `T∞ ≈ const`, so every work-conserving scheduler ties on *time* — the
decision collapses onto **coordination cost + cache misses + SIMD-ability**, where morsel-driven strictly
wins. Heartbeat is the right tool for *recursive* fork-join (its rustc integration regressed +35–55% on flat
work and was abandoned) — kept only as a straggler escape-valve and for BVH/tile-tree lanes.

**I/O reactor models:**

| Model | Examples | frame-harvest fit | Windows-now | Linux-later | cancellation |
|---|---|---|---|---|---|
| Readiness | epoll, mio, polling, async-io | poor (harvest still does the I/O) | **fakes** over IOCP (`\Device\Afd`) | epoll | drop-future |
| **Completion** | io_uring, IOCP, **compio** | **ideal — the CQ *is* the harvest queue** | **native IOCP** | io_uring | op-cancel + RST_STREAM |

The frame loop's native verb — "drain everything finished since last frame" — *is* the completion-queue
abstraction. `compio`'s `Proactor` (IOCP/io_uring/polling) is drivable without an async executor, owned-buffer
(cancellation-safe), and already carries `compio-quic` (h3) + `compio-tls` (rustls).

**Rejected at the survey stage:** ntex-on-bevy (thread-per-core `!Send`, archived reactor, 8-crate fork);
a reqwest bevy backend (no runtime seam — a deep fork); bespoke sans-io HTTP/2 (negative EV); one unified
CPU+I/O pool (head-of-line freeze); thread-per-core ECS (the Iggy collapse). Full reasoning in the memory
notes referenced in § 11.

## 3. Component designs (each: the decision · the verified seam · O-notation · the first spike)

### 3.1 The core budget — *the formula landed; the arbiter TYPE is demoted (re-eval 2026-06-09)*
- **Decision (amended):** the budget partitions `cpu_sim + io + blocking_fetch + ue_reserved =
  usable_physical`, reserving **3** for UE's Game/Render/RHI named threads, sized on **physical** cores.
  The **arbiter-as-type is demoted**: the job today is (a) ONE shared `compute_threads()` formula, (b) hard
  caps on every non-pool thread source, (c) thread priorities — a `CoreArbiter` subsystem is built only if
  the lanes measurably contend once gameplay leaves the game thread (until then `cpu_sim` has no separate
  lane to arbitrate).
- **Premise correction (UE source-verified, re-eval):** UE 5.8 sizes its TaskGraph off **LOGICAL−2**
  (`NumberOfWorkerThreadsToSpawn` → `NumberOfCoresIncludingHyperthreads−2` on SMT) — the earlier
  "`NumberOfCores()−1` = physical" claim here was **wrong**. The pre-fix situation was two ~logical-sized
  pools (ours 22 + UE's 22) on 12 physical cores ≈ **2× mutual over-subscription**, not "3–4× vs UE's
  physical pool". The physical−3 choice stands on its real grounds: don't stack a second near-logical pool
  on UE's, and HT siblings share execution ports (a sibling worker adds contention, not throughput, on the
  cache-/SIMD-bound paths).
- **Landed:** both formulas (`resolve_worker_thread_count` / `geo_compute_threads`) size `max(1,
  physical−3)` — deliberately still duplicated; the `compute_threads()` collapse lands with the
  LogiSimRunner work. The compio `spawn_blocking` AsyncifyPool is capped (`CYPER_BLOCKING_POOL_LIMIT = 4`;
  compio's default 256 was the single largest unbudgeted thread source under a streaming burst).
- **Not landed:** the shared formula, the thread priorities (compio `ABOVE_NORMAL`; blocking-fetch
  `BELOW_NORMAL` + EcoQoS) — every spawned thread currently runs NORMAL; measure-gated (set them at the
  three existing spawn sites when a contention profile shows the need, ~15 lines + a windows-sys feature).
- **Per-frame (when/if the type is built):** `on_poll()` reads two atomics → two bools — **O(1)**; no
  thread migration to rebalance.
- **Small-box note:** `physical−3` floors to 1 worker on a 4-core box (deliberate UE-first conservatism);
  decide the reserve against the real minimum player spec before shipping.

### 3.2 CPU lane — Regime H: morsel-BSP — *spike-gated (the central CPU spike)*
- **Decision:** the flat per-agent loop runs as a `morsel_for_each` *inside a system body*, not as a
  scheduler — exactly **P long-lived workers `fetch_add` one shared cursor**; zero per-morsel task objects,
  zero steals on the uniform path.
- **Seam (verified):** read SoA columns directly via the already-public `Table::get_data_slice_for::<T>()`
  (`bevy_ecs storage/table/mod.rs:267`) over `QueryState::matched_tables()`. The executor and schedule are
  **untouched**. (Rejected: replacing the executor; customizing `BatchingStrategy` — both keep
  one-task-per-batch.)
- **Structures:** `MorselCursor{ next: CachePadded<AtomicU32>, end }`, `BandCursors{ hi, bulk }` (two-cursor
  render priority — the O(1)/morsel analogue of LogiExec's K-bands), `MorselFramePlan`, `MorselPool`
  (resource), `TableView` (raw column ptrs).
- **O-notation:** dispatch `fetch_add` O(1); makespan **T₁/P + O(m·c_max)** (GSS tail-shrink on the final P
  morsels flattens the tail); atomics **O(n/m_bulk + P)**; cache **Q₁ + O(P·m/B)**.
- **Provenance precision (re-eval 2026-06-09):** the single-shared-cursor/zero-steal shape is the
  **single-socket specialization** of Leis et al. 2014 — the paper's canonical dispatcher is
  NUMA-partitioned (per-socket morsel queues, locality-preferring, cross-socket rebalance when a socket
  runs dry). Exact on one NUMA node (every target box today); the per-socket form is the S5 NUMA item.
- **Determinism by decomposition:** disjoint row ranges (free) + counter-RNG keyed by **absolute row index**
  + a **fixed, index-shaped reduction tree** (O(log(n/m)) span, never a serial fold). A `MorselReduce` trait
  carries associative reductions.
- **Required refactor:** `Transform3d` is AoS-within-a-column today (`{[f64;3],[f32;4],[f32;3]}`); split the
  hot fields into **parallel scalar columns** (`tx/ty/tz/vx/vy/vz`), hot/cold separated, cache-line +
  SIMD-width padded, so ISPC `foreach` vectorizes a stride-1 range.
- **First spike:** port **`integrate_motion` only** to a single-cursor `morsel_for_each`; assert bit-identical
  to the golden `parallel_execution_matches_single_threaded_bit_for_bit` test (`logi_runtime/src/lib.rs:894`).
  Proves the SoA seam, the cursor-beats-task-spawn claim, and determinism at once.

### 3.3 CPU lane — Regime S: precomputed-stage executor — *spike-gated*
- **Decision:** a custom `impl SystemExecutor` compiles the conflict graph into barrier-separated parallel
  stages **once** at `init`; at `run` it walks stages — **no `Mutex`, no `can_run` rescan, no per-system OS
  wakeup**. O(stages) barriers + O(n) spawns vs stock O(n·rounds) under one lock.
- **Seam (verified; CORRECTED re-eval 2026-06-09):** the `SystemExecutor` trait is 3 methods
  (`schedule/executor/mod.rs:30`), slotted via `Schedule::set_executor` (used per-phase in
  `LogiSchedules::new`). ~~Must live in-crate → a thin vendored `bevy_ecs` patch~~ — **refuted by round-4
  (§1.5/F5, [CONFIRMED])**: `ScheduleGraph::conflicting_systems()` and the dependency DAG are fully `pub`
  at build time, so the executor builds **with no fork**; the `pub(crate)` bitsets are only the in-band
  micro-opt path. The vendored-patch posture (and ADR-0014's ADR-0004 narrowing) is withdrawn.
- **Why it matters:** stock bottlenecks at ~500–1000 systems (issue #11378); simulation-LOD will push us
  there (4 bands × ~6 domains). **Order: simulation-LOD first, then this.**
- **First spike — DONE (Exp C, §6.1):** measured the stock executors at our scale. The `MultiThreaded`
  executor is **pure overhead at our current ~3 systems** (8.4 vs 1.0 µs single) and ~**2.5× the
  precomputed-stage lower bound** at 64–256 systems — but that overhead is **absolute-small** (≈0.4% of a
  33 ms frame) until *thousands* of systems. So this executor is **lower-priority than LOD + the morsel lane**,
  and the immediate **no-fork** win is to run the few/cheap-system schedules on `SingleThreaded`. When built,
  **batch** conflict-free systems into ~P tasks (one-per-system degrades at high N), and extend the
  `parallel == serial` gate to triangulate `single == multi == precomputed`.

### 3.4 CPU lane — LogiExec K-band priority — *spike-gated (YAGNI until contention)*
- **Decision:** where gameplay + geo + I/O continuations contend on one pool, **extend `async-executor`**:
  vendor its ~600-LOC work-stealing core into `crates/exec/logi_exec`, **keep `async-task` untouched** (its
  2757 LOC of audited `unsafe` is not re-derived), add **K=4 priority bands** (Frame > Gameplay > Render >
  Background).
- **Structures:** `WorkerQueues{ rings[K], next_slot[K] (stealable bounded(1)), credits[K] (DWRR) }`; priority
  rides in the task header via async-task's `metadata` hook (no side table).
- **O-notation:** `spawn` O(1); `pop` = O(K) superscalar band scan + DWRR (not a heap → keeps O(1)
  work-stealing); `steal` high-band-first O(1) typical; `drive_frame_band(budget)` O(jobs).
- **Note:** the *stealable* 1-deep LIFO slot is deliberate — the unstealable PR #22 slot deadlocks nested
  `block_on`. The likely real ceiling at extreme scale is the `active: Mutex<Slab<Waker>>` spawn-serialization
  — profile it.
- **Gate:** build only after gameplay moves off the UE thread (§3.7) and the three lanes actually contend.

### 3.5 I/O lane — compio completion reactor — *spike-gated*
- **Decision:** `compio-driver`'s `Proactor` on **one parked thread**, driven without an async executor:
  `push → poll(frame_budget) → pop_multishot → cancel`, feeding **sans-IO** protocol machines (rustls + h2
  now; quinn-proto/QUIC later). Cancellation = **RST_STREAM + proactor cancel**, with the owned recv buffer
  **retained until the cancel completion** (the completion-I/O safety rule).
- **Refined (Exp B cont., §6.1):** socket I/O over compio is **compio-net's** domain (its raw socket ops are
  `pub(crate)`, assuming a compio-managed `SharedFd<Socket>`), so the lane runs **`compio-net` (h1/h2/h3 +
  rustls) on its own dedicated Rust-owned thread, draining to the owner via a channel** — the bare `Proactor`
  above is the file/custom-op + cancellation model, not the socket transport. Same poll/harvest *shape*, one
  layer up.
- **Seam:** `CompioSource` implements the existing `TileContentSource` trait (`logi_3dtiles_io/src/lib.rs:53`)
  *identically* → one-line swap from `HttpSource`; `SyntheticSource` + `DiskCache` untouched. Buffer pool
  sized to in-flight depth (no per-request alloc).
- **First spike — DONE (Exp B, §6.1):** the bare `Proactor`, **no executor**, ran `push → poll(16ms) → pop`
  and drained 128 concurrent reads in 211 µs over **one** poll; cancelling 32 **in-flight** ops upheld the
  HARD GATE by construction (the buffer is moved into the op → never user-accessible after `cancel`). The
  *substrate* is validated on Windows/IOCP. **Still open (needs the protocol layer):** the throughput-vs-tuned-
  ureq comparison and rustls + h2 sans-IO over the proactor — until that's measured, keep ureq behind the trait.

### 3.6 The frame-harvest typed hand-off — *build-now (types are compile-time-only)*
- **Decision:** a `WorkSource` trait (`drain_ready`) implemented by **both** the I/O completion queue and the
  ECS deferred-command buffer, so the poll drains them uniformly. `IoHarvest<T>` is `!Send` with a private
  `commit_into`, consumable **only at the frame checkpoint** → "non-deterministic data touched the sim
  mid-step" is a **compile error** (the control-flow companion to ADR-0016's `Promoted<T>`).
- **The poll loop** is the geo `run_stream` generalized: push view/input → arbiter O(1) decision → drain I/O
  (budget-bounded) → stage as deferred commands → step the deterministic sim (0..n) → retain snapshot →
  return.

### 3.7 Completing ADR-0012 for gameplay — *deferred (coordinated Rust+C++ rebuild); NOT a mechanical mirror*
- A `LogiSimRunner` (shaped like `GlobeStreamer`) owns the gameplay `World` on a `"LogiSim"` thread;
  `logi_world_tick` splits into `push_input` (bounded, **timestamped** SPSC, newest-wins) + `poll` (the
  snapshot ring). Gameplay stays fixed-step deterministic; ABI 6→7.
- **Sizing corrections (re-eval 2026-06-09) — why "mechanical mirror" understates it:** (a) the
  `SnapshotRing` is owner-thread-only today (**no atomics**; correctness rests on the single-thread
  contract) — publish-from-sim-thread vs find/retire-from-game-thread needs a designed, property-tested
  synchronization layer across the publish/poll/release/reuse interleavings; (b) the Geo loop's
  catch-the-panic-and-continue policy is ADR-0007-legal for the render lane only — the deterministic sim
  must **hard-stop** on a panicked fixed step (replay integrity), so the runner needs its own failure
  protocol surfaced over the FFI; (c) the view slot's brief-Mutex shape is fine for Geo but the gameplay
  mirror must never hold a cross-thread lock across a fixed sub-step (ADR-0012 Decision 4's correction).
- **This is the load-bearing deferral of the whole runtime effort:** the deterministic sim — the workload
  the North Star scales — still runs synchronously on the UE game thread, and every later CPU win (morsel,
  SigIndex, LOD bands, the stage executor) compounds on a sim thread but is muted while the schedule blocks
  the frame. **Trigger:** before the first non-trivial gameplay system lands, schedule the decouple as one
  coordinated Rust+C++ rebuild; do not let it drift open-ended.

### 3.8 Simulation-LOD — see [ADR-0015](../Architecture/ADR-0015-Simulation-LOD.md)
The CPU-lane scaling answer: bands as archetypes (Hot/Warm/Cold/reservoir), a budget-bounded active set, an
**incremental `O(K+Δ)` significance index** (kills the O(P) scan at `systems.rs:120`), a **statistical
far-field** (Little's-law flows + depot stocks, O(#flows)), and **deterministic keyed-seed promotion** with a
conservation invariant. *First spike:* the bucketed top-K + spatial grid vs the current O(P) budget pass —
the thesis-prover, pure `logi_sim`, no FFI.

*(2026-06-11: extended by [ADR-0021](../Architecture/ADR-0021-Sim-Tiles-Temporal-Simulation-LOD.md) —
**sim-tiles**. Each band carries an explicit **dt** (multirate advance; flux settles on the coarse cadence),
the reservoir gains **hazard triggers** + a **bounded commitments ledger**, promotion across a temporal gap
is **lazy fast-forward** — O(observed), never O(elapsed) — and the significance score generalizes to a
per-cell **exported error metric**, designed before the band machinery builds. The S1 items below are that
program's floor, unchanged; the build slots live in ADR-0020's amended R-program.)*

### 3.9 Per-lane determinism — see [ADR-0016](../Architecture/ADR-0016-Per-Lane-Determinism.md)
A bounded deterministic near-set + a free non-deterministic far field, bridged by the typed `Promoted<T>`
(private canonical field → compile-time boundary). **Counter-based RNG** (replacing the shared `SimRng`
stream — zero consumers, a serialization point) is **build-now**: a correctness *and* perf fix.

### 3.10 GPU escalation — see [ADR-0017](../Architecture/ADR-0017-GPU-Resident-Statistical-Offload.md)
**Dark by default.** Rust-owned `wgpu` device, resident SoA, async Hot-set readback, shared to UE via NT
handle; the authority/reproducibility split keeps ADR-0005 intact. Built only when a profiler shows the
active-set bandwidth-bound on the branch-free bands.

## 4. The O-notation that decides the scaling

| Concern | Naive / today | LogiRuntime | why |
|---|---|---|---|
| Flat agent loop dispatch | one task/batch (`par_iter`) | **fetch_add cursor, O(n/m) atomics, O(P) space** | no task objects, no steals |
| System scheduling | single `Mutex`, O(n·rounds), per-system OS wakeup | **O(stages) barriers + O(n) spawns** | compile the schedule once |
| Significance / budget | **O(P) scan + sort every frame** | **O(K + Δ)** | incremental spatial + bucketed top-K |
| Per-tick row updates @ 10⁹ | **O(P)** = 3×10¹⁰/s (1.9 TB/s — impossible) | **O(N_active + #flows)** ≈ 10⁶–10⁷ | budget-bounded active set + aggregate far field |
| Far-field agents | O(P) integrations | **O(#flows)** ODEs | one Little's-law step per route segment |
| I/O cancellation | finish-then-discard (ureq) | **RST_STREAM O(1) any phase** | h2 stream reset over a kept-alive conn |
| Priority | none | **O(K) band scan** (not a heap) | K fixed bands by partition |
| Arbiter / frame | logical−2 (3–4× over-subscribed) | **physical−3, O(1)/frame** | one source of truth |

## 5. Roadmap (staged, measure-gated)

- **S0 — build now (no spike):** the `CoreArbiter` + physical-core fix (kills the over-subscription today);
  **counter-based RNG**; the `WorkSource`/`IoHarvest<T>` types; the `LogiSimRunner` move (completes ADR-0012
  for gameplay).
- **S1 — simulation-LOD (the 20× win):** band archetypes + `SimulationBudget`; the `O(K+Δ)` significance
  index; the statistical far field; keyed-seed promotion. *Gate:* Exp D (tick decouples from population).
  *(ADR-0021 adds: the per-cell error metric designs first; per-band dt, hazard triggers, commitments,
  fast-forward — same gate, plus an explicit latency budget on promotion catch-up, the teleport case.)*
- **S2 — the scheduler:** the precomputed-stage executor (vendored `bevy_ecs` patch). *Gate:* Exp C.
- **S3 — the two lanes:** the morsel-BSP engine for Regime H (Exp A); the compio I/O lane behind
  `TileContentSource` (Exp B); LogiExec only if contention is measured.
- **S4 — determinism scaffolding:** `Promoted<T>` + the `FarFieldNonDet` schedule + the `det-boundary` gate.
- **S5 — later/measured:** GPU-resident bands (ADR-0017 P0/P1); NUMA per-socket sharding.

## 6. Decisive experiments (each with a decision threshold)

| # | Hypothesis | Measure | Threshold |
|---|---|---|---|
| **A** | morsel cursor beats `par_iter` on the uniform loop | ns/agent, atomics vs task-spawns, LLC misses, p99 | adopt morsel if steal/scheduler overhead **>5%** on the flamegraph |
| **B** | compio frame-harvest absorbs N tiny GETs in budget; cancellation is UAF-safe | completions/frame, p99 harvest ms, **0 post-cancel buffer access** | **hard gate:** any post-cancel access = rejected; ship only if it beats the tuned ureq pool on p50 |
| **C** | precomputed-stage flattens the system-count curve | schedule µs at 10/100/1000 systems, lock % | adopt if stock's lock/wakeup is **>10%** at the real system count |
| **D** | simulation-LOD decouples tick time from population | tick ms at P ∈ {10⁶…10⁹} with fixed N_active | **the thesis-prover:** tick time must be ~flat in P |
| **E** | shared-pool decode burst jitters the gameplay tick | gameplay tick p99 during a geo decode burst | split into a capped geo pool if tick p99 **+20%** |
| **F** | physical-core budget removes UE frame hitches | UE Game/Render p99 hitches: logical−2 vs physical−3 | switch to physical-core budget if hitches drop |

### 6.1 Spike results — Exp D + Exp A (2026-06-09, release, 12-phys/24-logical dev box, 8-thread pool)

Both ran as pure-Rust `logi_sim` tests (no FFI, no editor); modules `logi_sim/src/sig_index.rs` and `logi_sim/src/morsel.rs`; all gates green + clippy `-D warnings` clean. *(2026-06-11: the Exp-D index moved up into the ECS fork as `bevy_ecs::incremental::BucketIndex` — `Rust/crates/logi_ecs/src/incremental.rs`, with the boundary-bucket partial-select fix — per ADR-0020; `logi_sim/src/sig_index.rs` is retired. The morsel spike stays in `logi_sim` pending ADR-0020 §R2.)*

**Exp D — the simulation-LOD thesis-prover (`bench_sig_index_decouples_from_population`).** Per-frame
band-selection cost, the current O(P) full rescan (`rank_topk_full`, a faithful copy of
`allocate_representation_budget`'s core) vs the incremental index, at K=4096, Δ=2000:

| N | O(P) scan | O(K+Δ) index | speedup |
|---|---|---|---|
| 10,000 | 255 µs | 41 µs | 6.2× |
| 100,000 | 1,022 µs | 87 µs | 11.7× |
| 1,000,000 | 8,511 µs | 133 µs | **64×** |

The O(P) scan grows ~linearly (≈10× per 10× N); the index is near-flat and the gap widens with N — **band
selection decouples from population, validating ADR-0015's central claim.** (The index's residual growth
41→87→133 µs is the boundary bucket's clone-and-sort, ∝ N·K/N ≈ the per-bucket density at the budget cut;
removable with finer top-bucket quantization or a partial-select — a noted refinement, not a blocker.)

**Exp A — the morsel engine (`morsel_is_bit_identical_to_serial`, `bench_morsel_vs_task_per_batch`).**
- **Correctness gate PASSED:** morsel output is **bit-identical** to the serial golden path across workers
  ∈ {1,2,3,4,8} × morsel sizes ∈ {1,17,256,4096} *and* the task-per-batch path; a 20-step run stays
  bit-identical — determinism-by-decomposition holds.
- **Dispatch A/B (1 M agents):** at fine granularity (256-row morsels → 3,907 task-per-batch tasks) the
  cursor is **2.6× faster** (1.35 vs 3.46 ns/agent); they converge at coarse granularity (few tasks). Morsel
  wins exactly where predicted — fine-grained load-balancing without per-batch task objects.
- **Incidental confirmation of the bandwidth wall (ADR-0015):** the integrate kernel is memory-bandwidth-
  bound — serial **1.57 ns/agent** vs 8-thread parallel **~0.9 ns/agent**, only **~1.7×** from 8 threads.
  More threads ≠ more throughput on a bandwidth-bound per-agent kernel — an in-vivo confirmation of why
  billions needs simulation-LOD (fewer rows touched), not just more cores.
- **Production `Table`-seam validated (`morsel_over_table_matches_par_iter_system`):** the morsel engine
  reading REAL `bevy_ecs` `Table` columns via the public `get_data_slice_for` seam — confirmed reachable from
  outside the crate (`matched_tables`/`World::storages`/`get_data_slice_for` are all `pub`, unlike the
  executor's `pub(crate)` conflict bitsets) — is **bit-identical** to the `integrate_motion` par_iter system
  on a live World. The last uncertain Exp A claim (the SoA seam works on production ECS storage, no fork),
  closed: `logi_sim::morsel::integrate_motion_morsel_world`.

**Verdict:** Exp D and Exp A both clear their thresholds, on real ECS storage. The load-bearing theses —
*band selection decouples from population* (Exp D), *morsel dispatch is correct + beats task-per-batch and the
`Table` seam works* (Exp A) — are measured, not asserted. Next: integrate `SigIndex` into `RepresentationPlan`
(maintain it from `Representation.significance`, drive tier assignment) and run `integrate_motion_morsel_world`
as an exclusive `FixedSim` system; then Exp C (precomputed-stage executor — needs the vendored `bevy_ecs`
patch, since its conflict bitsets are `pub(crate)`) and Exp B (compio I/O lane, with the cancellation gate).

**Exp C — scheduler overhead at our scale (`logi_sim/tests/sched_overhead.rs`, 2026-06-09).** Per-tick cost of
N conflict-free cheap systems under the stock executors vs a "manual single-stage scope" (the precomputed-stage
lower bound — P tasks + one barrier, no decide-loop, no per-system wakeup):

| N | SingleThreaded | MultiThreaded | manual-stage | multi ÷ stage |
|---|---|---|---|---|
| 4 | 1.0 µs | 8.4 µs | 2.1 µs | 4.0× |
| 16 | 3.8 | 7.7 | 5.8 | 1.3× |
| 64 | 15.0 | 41.5 | 14.3 | 2.9× |
| 256 | 60.4 | 140.7 | 56.0 | 2.5× |
| 1024 | 243.7 | 676.4 | 628.3 | 1.1× |

Three design-refining findings. **(1)** At our *current* ~3-system scale the `MultiThreaded` executor is pure
**overhead** — 8.4 µs vs 1.0 µs single at N=4 — confirming ADR-0012's "dormant executor" note. The immediate,
no-fork win is *use `SingleThreaded` for the few/cheap-system schedules*. **(2)** At a realistic LOD-era count
(tens–low-hundreds of systems), `MultiThreaded` runs ~**2.5× the precomputed-stage lower bound** — real
headroom the vendored executor would capture — **but in absolute terms it is small** (56–140 µs vs a 33 ms
frame ≈ 0.4%). So the precomputed-stage executor is **lower-priority than simulation-LOD + the morsel lane**,
not the urgent #11378 cliff (which is at *thousands* of systems we won't have soon). **(3)** "one task per
system" itself degrades at N=1024 (it converges with `MultiThreaded`), so the real precomputed-stage executor
must **batch** conflict-free systems into ~P tasks, not one-per-system. Net: build it — but *after* LOD and the
morsel lane — and pair it with the immediate `SingleThreaded`-for-cheap-schedules tweak.

**Exp B — compio I/O lane: frame-harvest + cancellation-safety (`logi_3dtiles_io/tests/compio_spike.rs`,
2026-06-09, Windows/IOCP).** compio 0.19 builds clean on the Windows nightly toolchain (IOCP; the `net`/
`runtime` features aren't even needed — `compio::driver::Proactor` is always available). The bare proactor,
**driven with no async executor**, ran the exact design shape — push N ops → `poll(Some(16 ms))` → `pop` — and
**drained 128 concurrent reads in 211 µs over a single `poll`** (the "drain everything finished this frame"
model, §3.5). **The HARD GATE holds by construction:** the read buffer is *moved into the op* at `push`, so
after `cancel` it is never user-accessible — `cancel` either returns it (op completed) or consumes the key and
the driver retains the buffer until the kernel finishes. All **32 cancelled ops were genuinely in-flight**,
every buffer driver-owned, and the run was clean (no UB / leak). So compio's completion proactor *is* the
frame-harvestable, cancellation-safe I/O substrate ADR-0014 specifies, on our exact platform. **Deferred (needs
the full client):** the throughput-vs-tuned-ureq comparison and the h2/TLS protocol layer (rustls + h2 sans-IO
over the proactor) — the *substrate* is validated; the *protocol layer* is the next step.

**Exp B (cont.) — the socket path + an HTTP/1.1 comparison (`logi_3dtiles_io/tests/compio_http_bench.rs`,
2026-06-09).** Reading compio's source surfaced a **design refinement**: the raw socket `Recv`/`Send` ops have
`pub(crate)` fields and assume a compio-managed `SharedFd<Socket>` — they are *compio-net's internal domain*, so
socket I/O over compio realistically goes through **compio-net, which runs on compio's thread-per-core
runtime**, *not* the bare proactor. **Therefore the I/O lane is `compio-net` (h1/h2/h3 + rustls) running on its
own dedicated Rust-owned thread, draining completed tiles to the owner via a channel** — the
"separate-runtime-on-its-own-thread" shape (ADR-0007/0012-legal), not the bare-proactor-from-the-sim-poll-loop
shape §3.5 implied. The bare proactor (Exp B) stays the model for file/custom ops + the cancellation guarantee.
A loopback HTTP/1.1 micro-comparison (200 concurrent GETs, compio-net vs the `ureq` thread-per-request pool)
found **compio-net low-variance (~80 ms, stable)** and **ureq high-variance (58–555 ms run-to-run)** — so the
*median* throughput verdict is noise-dominated and **inconclusive** at H1.1, consistent with the audit's "the
win is h2, not the I/O model at H1.1." Reliable conclusions: compio-net works + is predictable under high
concurrency; **the throughput-vs-ureq question stays genuinely open and needs the h2 multiplexing path + real
network latency** — that, plus TLS, is the actual next build step (not a loopback H1.1 bench).

**Exp B (cont. 2) — the TLS layer (`logi_3dtiles_io/tests/compio_tls.rs`, 2026-06-09).** Confirmed
**compio-tls + rustls + ring build clean on the Windows nightly toolchain** (no aws-lc-rs build dependency —
the layer directly below h2 is viable), and that a real Mozilla-root rustls `ClientConfig` + compio
`TlsConnector` **assemble** end-to-end using compio's re-exported rustls (zero version skew). So each layer of
the compio I/O lane is now individually validated: **bare-proactor frame-harvest + cancellation (Exp B) ·
compio-net sockets (cont.) · rustls/ring TLS (cont. 2).** What is *not* spike-able and remains the decisive
open question: **the live HTTP/2 handshake + a throughput-vs-ureq A/B against the real P3DT endpoint** — the h2
multiplexing win is RTT-dependent, so it can only be measured over a real network (the audit's MB/s + p50
tile-to-screen gate), as a production integration behind the `TileContentSource` trait, not a local spike.

**Exp B (LIVE) — the decisive h2-vs-ureq throughput A/B (`logi_3dtiles_io/tests/cyper_h2_bench.rs`,
2026-06-09, `#[ignore]` network test).** Over a **real network**, 64 concurrent small GETs to one h2 host
(`https://www.google.com/generate_204`, empty 204 → isolates connection/RTT/handshake overhead), **HTTP/2 via
`cyper` over compio-net+rustls vs HTTP/1.1 via the `ureq` thread-per-request pool**:

| Client | Protocol | Median (64 concurrent GETs) |
|---|---|---|
| **cyper / compio-net** | **HTTP/2.0** (ALPN-confirmed) | **56–68 ms** |
| ureq | HTTP/1.1, thread-per-request | 226–295 ms |

**h2 is a stable ~3.6–4.5× faster across runs** (vs the loopback H1.1 comparison which was noise-dominated) —
multiplexing N streams over a couple of connections hides per-request RTT and collapses ~64 TLS handshakes to
~a few. **This settles the last open decisive question: the I/O-lane win is real, large, and repeatable — but
it is an HTTP/2 win, not an I/O-model win** (consistent with the audit). Notable: **`cyper` *is* hyper 1.10's
h2 client running on compio-net+compio-tls** (the build pulls `hyper`/`h2`/`hyper-util`/`tokio-util`), so
ADR-0014's "hyper-h2" path and the "compio I/O lane" **converge** — hyper's h2 state machine over compio's
completion runtime. *Caveats (honest):* a real host but not the keyed P3DT endpoint (auth + larger Draco/JPEG
bodies would dilute the ratio toward the bandwidth-bound regime, though the handshake/RTT savings remain) —
**this caveat is now CONFIRMED: the keyed-P3DT A/B (§7.1, `tests/cyper_p3dt_bench.rs`) measured ~parity on
real 13 KiB tile bodies vs this ~7.6× on empty 204s, so the throughput win is handshake/RTT, not bandwidth;**
single machine; `ureq` here pays a cold connection per request (a warm keep-alive pool narrows it, but
concurrent H1.1 still needs ≥concurrency connections — it cannot multiplex). Production consideration: `cyper`
pulls a chunky `hyper`+`h2`+`hyper-util`+`tokio-util` graph (the io-compat bridge) — no tokio *runtime*, but a
dep-weight + ADR-0004 decision when productionized behind `TileContentSource`.

## 7. Settled vs spike-gated

- **Settled (build now, no spike):** the arbiter + `get_physical` fix; counter-based RNG;
  `WorkSource`/`IoHarvest<T>`; the `LogiSimRunner` gameplay-off-the-game-thread move.
- **Spike-gated:** morsel-vs-`par_iter` (Exp A); the `O(K+Δ)` index (Exp D — the thesis); compio-vs-tuned-ureq
  (Exp B); the precomputed-stage executor (Exp C); whether LogiExec earns its place yet (YAGNI until the
  three lanes contend); the GPU P0/P1 (wgpu-hal export maturity is the real risk).

### 7.1 Productized — landed 2026-06-09 (3 opus agents; all green + clippy `-D warnings` clean)
- **HTTP/2 I/O lane (`CyperSource`)** — `logi_3dtiles_io::CyperSource` impls `TileContentSource` (a `cyper` h2
  client on one dedicated compio-runtime thread, flume-drained, RST_STREAM cancel via task-drop, `DiskCache`
  reuse, `FetchError` classification matching `HttpSource`), **feature `cyper-h2`** — and **wired into the real
  geo stream** (`geo_world.rs::from_ion`, with graceful fallback to `HttpSource`; passthrough feature
  `logi_3dtiles_io` → `logi_3dtiles_scene` → `logi_geo_ffi`).
- **CyperSource adopted as the Google-tiles default + runtime backend knob (ABI v10→v11) — landed 2026-06-09.**
  `cyper-h2` is now **a default feature** (build `--no-default-features` for the lean ureq-only cdylib). A new
  `FetchBackend { Auto, Ureq, Cyper }` policy threads through `from_ion`: **`Auto` picks the `CyperSource` lane
  for Google P3DT hosts** (`tile.googleapis.com`) when compiled, else `ureq` — and is exposed as a runtime FFI knob,
  `LogiGlobeCreateInfo.fetch_backend` (+ a `_reserved` pad; struct 56→64, `LOGI_ABI_VERSION` 10→11, header +
  `abi_layout`/`globe_contract` + the C++ `ELogiFetchBackend` UPROPERTY all updated). A zero-init create-info ⇒
  `Auto` ⇒ the new default. *Verified:* geo crates check/clippy/test green on **both** default (`cyper-h2`) and
  `--no-default-features`; the v11 offset-locks + layout tests pass; the release `logi_geo` cdylib builds. (C++
  not rebuild-verified — needs `rebuild-editor.ps1`; the ABI bump is fail-safe: a stale v10 editor's
  `IsTerrainAbiOk` mismatch skips the lane rather than mis-decoding.)
- **The live keyed P3DT A/B — measured 2026-06-09 (`tests/cyper_p3dt_bench.rs`, real `tile.googleapis.com`).**
  The deferred decisive measurement, run against the keyed endpoint on **real Draco/JPEG tile bodies** (not the
  empty-204 proxy). Headline: **the ~4× is a handshake/RTT artifact that does NOT survive on real bodies.**
  | Workload | h2 (cyper) | ureq h1.1 (best case) | h2 ÷ h1.1 |
  |---|---|---|---|
  | empty-204 proxy, 64 concurrent | 11.7 ms | 89.8 ms | **~7.6× faster** |
  | real P3DT tiles (13 KiB avg), 32 concurrent | 77–81 ms | 74–87 ms | **~parity** (0.93–1.03×, 2 runs) |
  The entire h2 win is connection-setup/RTT collapse; over warm keep-alive connections fetching real bodies, h2
  and ureq's *best case* (unbounded thread-per-request) tie. **Adopting it as the Google default is still
  justified — but on structural grounds, not throughput:** (1) the bench gives ureq its best case; the
  *production* `HttpSource` caps at `workers` (16), so a 32-tile burst is 2 serial waves on ureq vs one
  multiplexed wave on h2; (2) at hundreds of concurrent tiles h2 multiplexes over a few connections where
  thread-per-request needs hundreds of threads/sockets; (3) the handshake collapse is a large win on cold
  starts / teleports / session refresh. The `fetch_backend` knob makes reverting to `Ureq` (or `Auto`→ureq) a
  one-field change if the structural case doesn't hold up in the full streamed pipeline.
- **HTTP/3 over QUIC adopted for Google tiles (compio-quic / quinn-proto — NO tokio) — landed 2026-06-09.**
  User-directed "quinn"/QUIC, realised on the compio lane exactly as ADR-0014 §3.5 envisioned ("quinn-proto/QUIC
  later"): enabling cyper's `http3` + `http3-altsvc` makes the existing `CyperSource` client negotiate **HTTP/3
  over QUIC** through `compio-quic` (= `quinn-proto`, the sans-IO QUIC state machine, driven on the compio
  completion reactor) — **no tokio RUNTIME (the tokio crate rides only as transitive hyper-util/h2 io-compat shims), no high-level `quinn` crate, no second runtime.** Alt-Svc auto-upgrade: the
  first request per host is an h2 bootstrap → Google's `Alt-Svc: h3=":443"` (confirmed live by probe) is cached on
  the long-lived client → every later request to that host is h3; h2 stays the fallback for non-h3 hosts. The
  `FetchBackend::CyperH2` variant was renamed `Cyper` (FFI value 2 unchanged → **no ABI bump**; C++
  `ELogiFetchBackend::Cyper`). *Live h3 A/B* (`tests/cyper_p3dt_bench.rs`, real keyed P3DT, protocol probed
  post-Alt-Svc-warmup = **HTTP/3.0**): h3 90–110 ms vs ureq 95 ms on 13 KiB bodies = **~parity** (0.96× / 1.15×,
  2 runs) — same as h2. h3 reliably engages, but like h2 its win is cold-connect/RTT + no-HoL-blocking, **not**
  warm-burst bandwidth on small bodies. *Verified:* check/clippy(`-D warnings`)/test green on default + 
  `--no-default-features`; v11 ABI locks hold (the rename preserved offsets + values); release `logi_geo` cdylib
  builds with the full h3 stack (compio-quic + quinn-proto + h3; no tokio runtime — the crate is a transitive io-compat shim). C++ not rebuild-verified.
- **Counter-based RNG** — `logi_sim` `SimRng` is now stateless + keyed (`for_entity(id,tick) → DrawKey`),
  callable inside `par_iter`, replacing the serialization-point stream (zero prior consumers; determinism test
  re-locked to the keyed contract).
- **Physical-core pool budget** — both pools (`logi_sim::resolve_worker_thread_count` +
  `geo_world::geo_compute_threads`) now size off `num_cpus::get_physical() − 3` (was logical − 2), removing the
  ~3–4× UE over-subscription (22 → 9 workers on a 12-phys/24-logical box).
- *Verified:* workspace builds (default/shipping), geo cdylib builds `--features cyper-h2`, affected-crate tests
  green (io 21 / runtime 13 incl. the determinism test / sim 11 / scene 36), clippy clean default + feature.

### 7.2 Deferred — with reasons (blocked or premature, not skipped)
- ~~**Adopt `CyperSource` as the default + the live P3DT A/B**~~ — **DONE 2026-06-09 (see §7.1):** flipped to the
  Google-tiles default behind the runtime `fetch_backend` knob (ABI v11); **and the lane was upgraded to HTTP/3
  over QUIC** (compio-quic / quinn-proto, no tokio runtime — the user-directed "quinn"/QUIC). The live keyed
  `tile.googleapis.com` A/B is measured for both h2 and h3 — both come back **~parity on real 13 KiB bodies**
  (the ~4–7.6× is an empty-204 handshake artifact), so the adoption rests on the structural case (capped
  production pool / connection economy at scale / cold-connect + QUIC handshake), not warm-burst throughput.
  *Open follow-ups:* (a) an A/B of the **full streamed pipeline** (`from_ion` Ureq vs Cyper over a sustained run —
  tiles-resident / MB/s over T s, via the v11 knob) to confirm the structural win beyond a 32-tile burst; (b) a
  deeper-LOD (larger-body) sweep; (c) a **lossy-network** test (QUIC's no-HoL-blocking advantage only shows under
  packet loss, which a clean dev link doesn't exercise). All are measurement-only — no new code (drive `from_ion`
  with each `FetchBackend`).
- ~~**`SigIndex` → `RepresentationPlan`**~~ — **DONE 2026-06-12 (ADR-0020 R1 wiring).** As decided below
  (re-eval 2026-06-09; round-4 §1.1 concurred): `logi_sim::SignificanceIndex` (persistent resource over
  the fork's `BucketIndex`; slot = entity index; `ensure_slots`/`remove`/`occupied` added to the fork
  module) and `allocate_representation_budget` re-shaped to **evict-on-`RemovedComponents` (O(Δ_removed))
  → upsert-feed (O(P) iterate / O(Δ) structural) → quantized top-K (O(k log k + boundary)) → exact
  re-rank of the selected set (O(k log k))** — the per-frame O(P) snapshot + `select_nth` is gone. The
  behavioral acceptance (quantized boundary membership; exact `(significance, stable_key)` tiering within
  the selected set) is locked by `budget_selection_is_quantized_top_k_on_a_live_world` +
  `budget_index_tracks_churn_despawn_and_reuse` on a live World through the real system (not the spike
  copy); in-system debug drift-assert (`occupied == population`, the LiveSet discipline); both
  determinism gates re-run green (in-crate golden + cross-process). *Remaining (R1 cont.):* the
  write-time change-detection feed (fork-interior) shrinks the upsert pass to O(Δ); the spatial-grid
  significance recompute stays the separate S1 item.
- **morsel engine + `Transform3d` SoA refactor** — morsel on today's AoS layout is bit-identical with no perf
  gain (bandwidth-bound); the SIMD payoff needs the SoA split, whose blast radius (every `Transform3d`
  consumer + the FFI snapshot + render-extract) is a dedicated PR.
- **`LogiSimRunner` (gameplay off the game thread)** — ABI 6→7 + C++ subsystem changes that can't be
  built/tested here and would break the running editor's ABI; needs a coordinated Rust + C++ rebuild.
- **Precomputed-stage executor** — needs the vendored `bevy_ecs` patch (fork); Exp C showed it's low-priority
  (overhead is absolute-small until *thousands* of systems).

### 7.3 Re-evaluation rectifications — landed 2026-06-09 (microscopic runtime re-eval; all Rust-only, no ABI/C++)

A 13-agent mapped + 10-dimension review of this runtime effort (verified inline; full report
[Docs/Audit/2026-06-09-Runtime-Scheduler-Reactor-Reevaluation.md](../Audit/2026-06-09-Runtime-Scheduler-Reactor-Reevaluation.md))
confirmed the trajectory and landed these fixes (workspace 245 tests green, clippy `-D warnings` clean,
default + `--no-default-features`):

- **Stream-thread panic containment** — `run_stream`'s step body is `catch_unwind`-guarded: a poisoned
  step increments `step_panics` (rate-limited stderr) and the loop continues, instead of the thread dying
  silently with `running=true` while C++ polls `Same` forever. (FFI counter exposure rides the next bump.)
- **compio blocking pool capped** — `CYPER_BLOCKING_POOL_LIMIT = 4` (default was 256): the disk-cache
  `spawn_blocking` lane can no longer thread-bomb outside the core budget under a streaming burst.
- **h3→h2 fallback + Alt-Svc quarantine** (vendored cyper) — an h3 transport failure on a cache-routed
  GET/HEAD evicts + quarantines the host's h3 entry (5 min) and retries once over h1/h2; a mid-session UDP
  block now degrades to h2 instead of stranding streaming until the 24 h Alt-Svc TTL.
- **M4 prefetch made real** — `prefetch_paths` now ages by `retain(in_flight_paths.contains)` instead of a
  wholesale clear that cancelled every speculative fetch ~2 advances (~2–16 ms) after issue; prefetches
  survive their RTT, and H6 preemption remains the stale-speculation relief valve.
- **Geo step convergence holes closed** — a Glb-requested fetch that sniffs as a tileset reclaims its slot
  + index entry (was a permanent `Requested` ghost); a fetched-but-unparseable manifest (and the reverse
  Glb-on-manifest mismatch) is marked terminal in `decode_failed` (was an unbounded 1 ms-cadence re-fetch
  loop); expired `next_retry` deadlines are purged in `bound_failure_sets` (was: one abandoned transient
  failure kept the stream thread re-traversing a settled scene at ~125 Hz forever); the 401 backoff floor
  is `SESSION_REFRESH_COOLDOWN` (was 24 ms of doomed re-401s).
- **ureq lane tombstone purge** — cancelled-but-buried heap entries (e.g. score −1e9 prefetches) are
  reclaimed by an amortized rebuild in `cancel`; heap + cancel registry stay bounded under sustained
  motion. A panicking cyper fetch task now emits its one result via an unwind guard (was: a leaked budget
  slot + in-flight entry).
- **Determinism gate made real** — new cross-process `logi_runtime/tests/parallel_determinism.rs`: the
  serial arm runs in a child process pinned to a 1-thread pool (asserted), the parallel arm on a 4-thread
  pool (asserted), full per-tick snapshot-stream digests compared bit-for-bit. The in-crate gate is
  re-scoped as a smoke test (the process-global pool OnceLock made it degrade to comparing identical
  configurations).
- **Exp C landed for gameplay** — all gameplay phases run the `SingleThreaded` system executor in both
  modes (the MT executor was pure overhead at 1–2 systems/phase; intra-system `par_iter` is unaffected);
  the modes now differ only in pool width, which is exactly what the determinism gate exercises.
- **Telemetry + guards** — `decoded_this_step` re-wired (the dec. 5 gate signal; orphaned by the
  sequential-decode rewrite); `_reserved` create-info field `debug_assert`ed to 0; the per-step
  `base_url` clone hoisted into the rare refresh-enqueue branch.
- **Docs reconciled** — ADR-0012 status scoped (Geo half built, gameplay pending, ABI still 6); ADR-0014
  arbiter de-tensed + demoted to formula-plus-caps (type only on measured contention), UE logical−2
  premise corrected (the "3–4×" was really ~2× two-near-logical-pools), proactor-shape pointer added,
  vendored-`bevy_ecs` premise withdrawn (no fork needed — `conflicting_systems()` is pub), #11378 figures
  re-sourced, Iggy precision (kept TPC, dropped ECS-under-TPC), "no tokio" tightened to "no tokio
  RUNTIME"; ADR-0016 status marks the type boundary designed-not-built; morsel single-socket provenance
  noted; SigIndex governance resolved above.

**Deferred with explicit gates (not skipped):** thread priorities (~15 lines at the 3 spawn sites; set when
a contention profile shows the need); the clip-child AABB memo in `publish` (transition-heavy flamegraph);
the ureq `Drop` deadline-join (only if PIE-exit hitches on a black-holed network are ever observed; the
docstring now states the connect/stall-phase bound honestly); the small-box reserve policy (min-spec
decision); auth-failure session-generation gating (the cooldown floor landed; generations if 401 storms
ever reappear); upstreaming the cyper `TransportConfig` + fallback seams as a PR (shrinks the fork to
nothing on merge — needs the user's go-ahead to publish).

## 8. Open questions

- The morsel `m_bulk` auto-tuning policy (must change schedule, never outputs); change-detection cost at
  morsel scale.
- The reservoir slot-indexing for fractional flow rates (deterministic Poisson thinning vs integer tokens);
  promotion pop-in vs predictive prefetch.
- Barrier granularity in the precomputed executor (one `scope()` per stage vs one scope + atomic countdown)
  if a schedule grows to hundreds of thin stages.
- The run-condition-skips-dependents corner in the precomputed executor (the highest-risk correctness case).
- compio IOCP multishot fidelity; h2 vs quinn-proto first; the two-GPU-device VRAM/context cost (ADR-0017).
- Editor-vs-packaged target flips UE's core math ~2.5× → the FFI create-info needs an "is-game" bit.

## 9. References

- ADRs: [0014](../Architecture/ADR-0014-LogiRuntime-Unified-Executor-Reactor.md),
  [0015](../Architecture/ADR-0015-Simulation-LOD.md), [0016](../Architecture/ADR-0016-Per-Lane-Determinism.md),
  [0017](../Architecture/ADR-0017-GPU-Resident-Statistical-Offload.md); builds on
  [0012](../Architecture/ADR-0012-Poll-Based-FFI-Decoupled-Sim-Thread.md),
  [0005](../Architecture/ADR-0005-Bevy-ECS-Authority.md), [0007](../Architecture/ADR-0007-Actorless-Rust-Terrain.md),
  [0003](../Architecture/ADR-0003-Representation-LOD.md), [0004](../Architecture/ADR-0004-No-Compatibility-Code.md).
- Verified seams: `Table::get_data_slice_for` (`bevy_ecs storage/table/mod.rs:267`), `SystemExecutor` +
  `set_executor` (`schedule/executor/mod.rs:30`, `schedule/schedule.rs:544`), the two pool formulas
  (`logi_sim/src/schedules.rs:83`, `logi_3dtiles_scene/src/geo_world.rs:1162`), `TileContentSource`
  (`logi_3dtiles_io/src/lib.rs:53`), the O(P) budget scan (`logi_sim/src/systems.rs:120`), the shared RNG
  stream (`logi_sim/src/resources.rs:45`), the `parallel == serial` gate + snapshot ring
  (`logi_runtime/src/lib.rs:894`, `:209`), the `GlobeStreamer` mirror (`logi_3dtiles_scene/src/stream_thread.rs`).
- External baselines: `async-executor 1.14.0` / `async-task 4.7.1` (the CPU core to extend), `compio` /
  `compio-quic` / `compio-tls` (the I/O core), HyPer morsel-driven (Leis et al. 2014), bevy issues #11378 /
  #8304 (the single-`Mutex` ceiling), the PostHog rayon+tokio post-mortem (one-pool failure), the Apache Iggy
  thread-per-core+ECS retreat.
- Project memory: `runtime-architecture-research.md` (the full three-round synthesis + the round-by-round
  findings), `http-library-bevy-runtime-research.md` (the I/O-substrate decision that became compio).
