# ADR-0015 — Simulation-LOD: Planetary-Scale Agent Simulation

- **Status:** Proposed (spike-gated) — **the thesis-proving spike has since LANDED** (Exp D,
  2026-06-09: band selection 64× faster than the O(P) scan at 1 M and near-flat in P; the index now
  lives in the ECS fork as `bevy_ecs::incremental::BucketIndex` per
  [ADR-0020](ADR-0020-LogiEcs-Incremental-World-Cell.md) R1). **Extended by
  [ADR-0021](ADR-0021-Sim-Tiles-Temporal-Simulation-LOD.md)** (2026-06-11): time becomes a second,
  explicit refinement axis (a cell's dt is a resolution, frame → day → never), the reservoir gains
  hazard triggers + a bounded commitments ledger, promotion across a temporal gap is lazy
  fast-forward, and this ADR's bands become instances of the sim-cell contract. Entity count is
  unbounded — one million agents is the floor, not the ceiling.
- **Date:** 2026-06-09
- **Deciders:** Vast architecture
- **Extends:** [ADR-0003](ADR-0003-Representation-LOD.md) — the unified significance score now drives a
  **second, deeper** LOD that governs *whether an entity is simulated at all*, not just how it is drawn.
- **Relies on:** [ADR-0005](ADR-0005-Bevy-ECS-Authority.md) (Bevy ECS owns gameplay truth — this is how that
  truth scales) and [ADR-0016](ADR-0016-Per-Lane-Determinism.md) (the statistical far field is the
  non-deterministic lane; promotion crosses the typed boundary).
- **Most affects:** the gameplay sim (`logi_sim`/`logi_runtime`) and its consumer, [ADR-0014](ADR-0014-LogiRuntime-Unified-Executor-Reactor.md)
  (the CPU lane runs a bounded active set, never a billion rows; the system-count growth this causes is why
  ADR-0014's precomputed-stage executor exists).
- **Plan:** [Docs/Plans/LogiRuntime-Design.md](../Plans/LogiRuntime-Design.md) § Simulation-LOD.

## Context

The North Star is millions — ultimately **billions** — of meaningfully-simulated agents on a streamed
planet. A flat ECS that touches every entity every tick **cannot** reach that, and the wall is not the
scheduler:

- At 10⁹ agents × 30 Hz × ~80 B touched/agent, the per-tick memory traffic is **~1.9–3.6 TB/s**, against a
  desktop DRAM ceiling of **~50–70 GB/s sustained** — off by **20–40×**. No executor change, no SIMD trick,
  no work-stealing heuristic closes a 20× *bandwidth* gap. The CPU "touch-everything" ceiling is
  **~10–30 M** fully-simulated agents, set by the memory bus.
- The existing `Representation`/`allocate_representation_budget` machinery (ADR-0003) is a **render**-LOD: it
  decides *how to draw* an entity that already exists as an ECS row. It presupposes the row. What is missing
  — and what makes billions tractable — is a **simulation**-LOD that decides whether the row **exists at
  all**, or only as a number inside an aggregate.
- A latent cost compounds this: `allocate_representation_budget` does an **O(P) scan + partial-sort every
  frame** (`systems.rs:120`), and `compute_significance` is **O(P · cameras)** — both put population `P` on
  the per-tick critical path, the very thing that must die.

## Decision — the new law

> **You cannot, and must not, touch every entity every tick. The active full-fidelity set is bounded by a
> BUDGET, not by population. The far field is simulated as STATISTICAL AGGREGATES (distributions and flows),
> not individuals. The billions exist as coarse-bucket state, materialized into ECS rows only within the
> significance horizon, deterministically from a keyed seed. Per-tick work is `O(N_active + #flows + K + Δ)`
> — never `O(P)`.**

Concretely:

1. **Bands are archetypes, not fields.** `Hot` / `Warm` / `Cold` are **zero-sized marker components**, so
   Bevy places each band in a **distinct archetype** — a `Query<…, With<Hot>>` iterates only Hot tables, so
   skipping a band is an **O(1) archetype skip with zero per-entity branches**. `Dormant` is **not an ECS
   row at all** — dormant agents live in the far-field reservoir (a dormant *archetype* would still cost the
   memory wall: 80 B × 10⁹ = 80 GB). Bands transition under **Schmitt-trigger margins + minimum-dwell
   hysteresis**, both pure functions of replay-stable state.

2. **The active set is budget-bounded.** A `SimulationBudget` caps `Hot + Warm + Cold` at `N_active`
   (~10⁶–10⁷) **regardless of `P`**, sitting above the existing `RepresentationBudget` (which now governs
   render-tier only *within* that active set). Per-tick row-updates collapse to `Hot + Warm/k +
   Cold/period` — a fixed ~10⁶ order, mathematically independent of population. The 10⁹ problem becomes the
   (already-feasible) 10⁶ problem plus a cheap aggregate.

3. **Significance becomes incremental — `O(K + Δ)`, killing the O(P) scan.** A coarse **spatial grid**
   recomputes significance only in cells near a camera (`O(N_near)`), and a **bucketed top-K** structure
   (≈256 buckets, intrusive sets) is maintained incrementally — an agent moves buckets only when its
   significance crosses a boundary (`O(Δ)`) — and band selection walks buckets top-down (`O(K + Δ)`). The
   deep tail (bucket 0, holding ~all of `P`) is never iterated. Determinism holds: buckets are integer
   quantiles of (deterministic) significance; *boundary* ties within one quantum resolve by ascending
   entity index (deterministic, replay-stable), and the selected set is then re-ranked **exactly** by
   `(significance desc, stable_key asc)` before tiers are assigned — quantization decides only who
   crosses the budget boundary. *(Landed 2026-06-12 for the representation budget: ADR-0020 R1 —
   `SignificanceIndex` + the re-shaped `allocate_representation_budget`; the spatial grid and the
   write-time O(Δ) feed remain open.)*

4. **The far field is distributions, not individuals — `O(#flows)`.** ~10³–10⁵ aggregate **flow** and
   **stock** entities advanced by macroscopic laws: a route segment carrying 10⁶ couriers is **one** ODE
   (`in_transit = λ · length / speed`, Little's law `L = λW`), not 10⁶ integrations; depots integrate
   inflow − outflow with stock conservation; diffuse movement uses a per-cell Markov density field. Cost
   scales with the **logistics graph** (routes/depots), never with the agent count it represents.

5. **Promotion/demotion is deterministic and conserving.** When the significance horizon reaches reservoir
   population, individuals **materialize** from a **keyed seed** —
   `hash(global_seed, cell, route, slot, tick)` over the project's sanctioned SplitMix64 — so replay and
   netcode hold (two peers materialize identically without syncing the agents; only the aggregate `λ`/seed
   replicate). Demotion folds residual state back into the aggregate. The invariant
   **`reservoir_count + materialized_count == total_population`** holds every tick (a debug assert in the
   deterministic reference mode). This is the gameplay analogue of Geo's coarse↔fine tile promotion
   (ADR-0007). Materialization is **demand-driven by significance but supply-capped by the budget** — a full
   budget degrades to statistical/instanced rendering, never an unbounded spawn storm.

6. **Time-slicing is replay-stable.** Warm agents update `1/k` per tick, partitioned by
   `stable_id % k == tick % k` (a pure function of replay state), integrated with the **correct elapsed dt**
   `(tick − last_full_tick) · dt_base` (exact for linear motion). Order-independence (each agent writes only
   its own components) is preserved, so the `parallel == serial` obligation still holds.

## Consequences

**Positive**
- **Per-tick cost decouples from population.** Tick time depends on `N_active + #flows + K + Δ`, so the same
  code runs a 10⁶ city and a 10⁹ planet — the single change that makes "billions" literally runnable.
- **The O(P)-per-frame scans die** (the significance index is `O(K+Δ)`); the deep tail is never iterated.
- **It reuses what exists** — ADR-0003 significance, the `RepresentationBudget` pattern, the spawn/despawn
  id bookkeeping, the Geo promotion precedent — as additive layers, not a rewrite of the working hot path.
- **The far field is naturally non-deterministic and non-authoritative-per-individual**, which is exactly
  the cheap NDET lane (ADR-0016) — so the determinism scope *shrinks* as scale grows.

**Negative / costs**
- **A whole new layer** (bands, the spatial+bucket index, the flow/stock far-field, the promotion/demotion
  machinery, the budget), and the **promotion boundary** is genuinely subtle (keyed-seed determinism,
  conservation, hysteresis vs visible pop-in).
- **It multiplies system count** (4 bands × ~6 domains = 24+, a mature game → hundreds), which is precisely
  what pushes `bevy_ecs`'s single-`Mutex` executor toward its ~1000-system knee — so this ADR **requires**
  ADR-0014's precomputed-stage executor to land alongside it. *Order: simulation-LOD first (the 20× win),
  then the scheduler fix.*
- **Far-field fidelity is statistical** — a far-field bug may not reproduce as an individual; acceptable
  only because no authoritative gameplay reads an un-promoted far-field individual (ADR-0016's boundary),
  with a "force-deterministic far field" debug flag retained.

## Alternatives considered

- **Brute-force flat ECS to billions.** Dies at the ~10–30 M memory-bandwidth wall regardless of scheduler.
  Rejected — it is the wall this ADR exists to go around.
- **GPU-simulate everything.** Raises the branch-free ceiling but breaks replay (GPU floats), can't read a
  billion rows back per frame, and is premature. Deferred to [ADR-0017](ADR-0017-GPU-Resident-Statistical-Offload.md),
  gated on a measured bandwidth-bound profile — and even then only for the Warm/Cold statistical bands.
- **A `Dormant` archetype the executor skips.** Skipping is O(1) *time*, but the rows still cost O(P)
  *memory* (the 80 GB wall) and structural churn. Rejected — dormancy is *non-existence* (the reservoir),
  which is the strongest possible skip.
- **A field-based band tag (`if band == Hot`).** Forces every system to load the component and branch —
  O(P) branches, defeating the purpose. Rejected in favor of archetype-per-band.

## References

- Plan: [Docs/Plans/LogiRuntime-Design.md](../Plans/LogiRuntime-Design.md) § Simulation-LOD (the data
  structures, the `O(K+Δ)` index, the flow model, the keyed-seed promotion, and the first spike).
- [ADR-0003](ADR-0003-Representation-LOD.md) (the significance score this extends),
  [ADR-0016](ADR-0016-Per-Lane-Determinism.md) (the NDET far field + `Promoted<T>`),
  [ADR-0014](ADR-0014-LogiRuntime-Unified-Executor-Reactor.md) (the precomputed-stage executor this
  necessitates), [ADR-0007](ADR-0007-Actorless-Rust-Terrain.md) (the coarse↔fine promotion precedent).
- Repo anchors: the O(P) budget scan — **killed 2026-06-12** (`allocate_representation_budget` now ranks
  against `SignificanceIndex`, ADR-0020 R1; `compute_significance` remains the O(P·cameras) term the
  spatial grid will bound), the significance + tie-break
  (`:67`, `:100`), the budget resource (`logi_sim/src/resources.rs:79`), the sanctioned RNG to key
  materialization (`:45`), spawn/despawn + id bookkeeping (`logi_runtime/src/lib.rs:320`, `:370`, `:295`),
  the `Simulated`/`Representation`/`Agent.route` components (`logi_sim/src/components.rs:103`, `:52`, `:90`).
