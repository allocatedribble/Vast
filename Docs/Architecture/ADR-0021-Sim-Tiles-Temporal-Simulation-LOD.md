# ADR-0021 — Sim-Tiles: Temporal Simulation-LOD and the Sim-Cell Contract

- **Status:** Accepted (2026-06-11) — **owner directive** (the goal evolution decreed 2026-06-11,
  recorded in project memory the same day). This ADR is direction + contract law: its build items
  slot into [ADR-0020](ADR-0020-LogiEcs-Incremental-World-Cell.md)'s R-program (amended the same
  day) — it spawns no workstream outside that program.
- **Date:** 2026-06-11
- **Deciders:** Vast architecture (owner directive)
- **Extends:** [ADR-0015](ADR-0015-Simulation-LOD.md) — the band machinery gains a second, explicit
  refinement axis (**time**), and band / reservoir / budget become instances of one governing cell
  shape; [ADR-0003](ADR-0003-Representation-LOD.md) — the unified significance score generalizes
  into a per-cell, **exported error metric**; [ADR-0019](ADR-0019-Cells-Within-Cells-Interlinked.md)
  — the ladder's top opens: "the shipped game at a million agents" is the **floor rung** (it remains
  the tie-breaker test), no longer the cell where the climb closes.
- **Relies on:** [ADR-0016](ADR-0016-Per-Lane-Determinism.md) — determinism-from-seed is reframed as
  this program's **compression scheme** (it is what makes "don't tick at all" legal);
  [ADR-0020](ADR-0020-LogiEcs-Incremental-World-Cell.md) — the fork is where the substrate lands;
  [ADR-0014](ADR-0014-LogiRuntime-Unified-Executor-Reactor.md) — one arbiter spends one budget
  across the cut.
- **Most affects:** `logi_sim` / `logi_runtime` / `logi_ecs` (the amended R-program); much later and
  still gated, the shard fabric (R5).
- **Plan:** [ADR-0020](ADR-0020-LogiEcs-Incremental-World-Cell.md) § R-series (amended 2026-06-11) +
  [Docs/Plans/LogiRuntime-Design.md](../Plans/LogiRuntime-Design.md) §3.8 / §5.

## Context

The owner goal evolved on 2026-06-11: *"as many entities as we can throw numbers at"* — never all at
the same quality or resolution — with the algorithmic shape decreed by analogy: **"like 3D Tiles, but
for pure simulation processing"**, over timesteps from frames to seconds to days, on anything from
one CPU to many interconnected machines. The analogy is load-bearing, not decorative; it maps
one-to-one onto machinery this codebase has already built and shipped once:

| 3D Tiles (Geo, shipped) | Sim-Tiles (this ADR) |
|---|---|
| geometric error per tile | error metric per sim-cell (what its coarseness costs) |
| screen-space error vs the camera | significance vs observers + hazard mass (ADR-0003 generalized) |
| the selected cut of the tile tree | the set of cells actively advanced, each at its own dt |
| REPLACE refinement | promotion: a statistical cell replaced by individuals (ADR-0015 §5) |
| implicit tiling (availability from a rule) | unmaterialized state, derivable from seed on demand |
| the kick (coarse parent holds until children ready) | aggregate stands in until promotion completes (ADR-0019 §6) |
| tile streaming / cache / eviction | the shard fabric + snapshot/seed reconstruction (R5) |

Two facts ground it. First, the **spatial axis is already law and already measured**: ADR-0015's
budget-bounded bands and statistical far field, with the Exp-D thesis-prover landed (band selection
64× faster than the O(P) scan at 1 M and near-flat in P; the index now lives in the fork as
`bevy_ecs::incremental::BucketIndex`). Second, the **temporal axis already exists in embryo** —
Warm's 1/k time-slicing, Cold's periodic update, the flow ODEs — but as per-band special cases with
no named resolution and no coupling rule. This ADR promotes time to a first-class refinement axis
and names the cell contract that both axes (and any later axis, e.g. cognition fidelity) plug into.

Prior art — the shape is forty years proven; the synthesis-as-one-principle for agent simulation is
ours: Berger–Oliger adaptive mesh refinement **subcycles** (finer grids take more, smaller
timesteps, with flux corrected at coarse/fine seams — production-standard in computational physics);
multirate ODE integration; discrete-event simulation advances at *event* resolution, not tick
resolution (Jefferson's virtual time for the distributed case); Barnes–Hut/FMM N-body replaces
distant subtrees with one aggregate term under an opening-angle test — a literal error-metric-driven
simulation cut.

## Decision — the law

> **Simulation fidelity has two refinement axes: WHAT exists (spatial/population — ADR-0015) and
> WHEN it advances (temporal — a cell's timestep is a resolution: frame → second → day → never). The
> unit is the SIM-CELL: bounded in space, population, timestep band, and compute budget; carrying an
> explicit, exported ERROR METRIC; coupled to siblings and parents only through typed, conserving
> flux links; obligated to conservation, observation-continuity, and replay-stable hazard triggers.
> The cheapest tick is the one never run: unobserved state is a seed plus an aggregate trajectory,
> FAST-FORWARDED on observation — determinism-from-seed is the compression scheme, not a constraint.
> Per-step work is O(cut + Δ); the entity count itself is unbounded — one million agents is the
> floor the tie-breaker tests, never a ceiling a design may bake in.**

Concretely:

1. **Time is a banded resolution.** Each band carries a dt: Hot advances every fixed step; Warm at
   1/k (already in-tree, ADR-0015 §6); Cold at seconds; flows at minutes→hours; seed-only at
   *never*. A cell's dt is selected by the same budget/significance machinery that selects its band,
   under the same Schmitt-trigger + minimum-dwell hysteresis (ADR-0015 §1). The world's per-wall-
   second cost is `Σ_cells cost(cell) / dt(cell)` — the budget bounds this *sum*, so the temporal
   axis multiplies the spatial win: most of the world is not merely simulated coarsely, per unit
   time it barely runs at all.

2. **Multirate coupling happens at the link, on the coarse cadence.** A fine cell and a coarse
   neighbor exchange through a typed **flux link** that accumulates at the fine cadence and settles
   at the coarse cadence (the subcycling rule). The link is the single home of in-transit quantity,
   so flux conserves by construction; no cell reads another's interior at a cadence finer than the
   owner's band — the link is the contract (ADR-0019 §2).

3. **The strongest temporal rung is "never": lazy fast-forward.** Promotion across a temporal gap
   does **not** tick through the gap. `state(t_now)` is reconstructed as the aggregate trajectory
   evaluated at `t_now` (closed-form / ODE) plus a keyed-seed sampling of the microstate
   *conditioned on the cell's commitments* (§6) — `O(observed)`, never `O(elapsed)`. Catch-up runs
   under an explicit latency budget, coarse-first then refining — the kick shape (ADR-0019 §6) at
   the sim rung. This is why ADR-0016 is a *reliance* of the goal evolution, not a casualty: state
   you can regenerate is state you don't store; ticks you can replay are ticks you can defer.

4. **The error metric is the keystone — design it before building any band machinery.** ADR-0003's
   significance generalizes to one scalar per sim-cell: the expected observable divergence of
   staying coarse over the cell's next interval, weighted by observation (cameras, player
   interaction, gameplay reads) and hazard mass (§5). Requirements: **(a)** cheap to maintain
   incrementally — it feeds the same `BucketIndex` machinery, cells being slots one rung above
   agents; **(b)** **exported** per cell in the unified-telemetry block shape (the `GeoTelemetry`
   precedent), so the cut is observable and the arbiter's spend is auditable; **(c)** comparable
   across axes — refining dt, population, or (later) cognition compete in one budget on
   error-per-cost. First implementation: ADR-0003's score as-is for agent cells, aggregated up the
   hierarchy for coarse cells. This scalar is the SSE of the program; everything downstream of it is
   engineering this codebase has already practiced.

5. **Promotion is observer-driven AND event-driven — one mechanism, both replay-stable.** Aggregates
   carry **hazard rates** for rare events whose consequences are observable (the riot, the market
   crash, the bankruptcy) — a pure mean field is too smooth, and a world that cannot surprise beyond
   the player's bubble reads as dead. Hazard draws use the counter-based keyed RNG (ADR-0016 §3)
   keyed by `(cell, coarse_tick)`, so endogenous promotion is a pure function of seed + key:
   replay-stable by construction even in the free far-field lane. The kick ≈ promotion identity
   (ADR-0019 §6) is hereby law, with two drivers for the same swap.

6. **Demotion carries commitments.** When individuals fold back into an aggregate, observed facts
   that must survive re-materialization (the shopkeeper the player met, the truck the player loaded)
   are retained as a **bounded commitments ledger** on the cell; promotion samples the microstate
   *conditioned on* the ledger. The ledger is budgeted and evicted lowest-significance-first — an
   evicted commitment is the world forgetting, deliberately, under budget. ADR-0015 §5's
   conservation invariant extends per conserved quantity (population, money, goods) and stays
   debug-asserted in the deterministic reference mode.

7. **Distribution inherits temporal tolerance.** A cell's cadence **is** its latency tolerance: a
   minutes-band cell exchanges flux at minutes cadence and does not care about cross-machine (later,
   cross-datacenter) latency; only frame-band bubbles need tight locality, and they are small by
   construction and local to their observer. R5's shard fabric therefore partitions along the
   hierarchy, with cross-shard links carrying the **coarser endpoint's cadence** — the same
   demote-to-flow / promote-on-arrival link, one rung up (ADR-0020 R5, rationale strengthened, gate
   unchanged). One CPU and N machines run the same algorithm; the budget moves the cut, not the code
   shape.

8. **Two contract clauses recorded now, built later — cheap now, brutal to retrofit.**
   **(a) Forkability:** a World forks from snapshot + seed as a first-class operation (replay plus
   divergent input — instances, ensembles, counterfactuals). Contract-shape requirement: all cell
   state lives in World state + seed + commitments — never ambient or process-global.
   **(b) Foreign interiors:** the sim-cell contract is a *trait*, not a schedule — advance(band dt),
   error(), flux ports, promote/demote, (seed, commitments) serialization — and nothing in it may
   assume the interior is bevy systems. Both clauses are YAGNI-guarded: they constrain the shape of
   what gets built anyway, and build no speculative machinery (ADR-0019: a cell must earn its
   membrane).

### Where it lands — the amended R-program (ADR-0020 remains the program of record)

- **R1 (unchanged, still first):** wire `allocate_representation_budget`
  (`logi_sim/src/systems.rs:110`) onto `BucketIndex` + the write-time feed. The queued acceptance
  decision (quantized vs exact top-K) and determinism-gate re-locks are unaffected by this ADR.
- **R3, step 0 — the error metric (§4):** the per-cell scalar + its block-shaped export, designed
  *before* any new band machinery builds; rides R1's score plumbing (same scalar, one aggregation
  rung up).
- **R3 (re-scoped):** bands carry **dt** (multirate advance, flux settled on the coarse cadence);
  the reservoir gains **hazard triggers** (§5) and the **commitments ledger** (§6); materialization
  becomes **lazy fast-forward** (§3) with the per-quantity conservation assert.
- **R4 (re-scoped):** the membrane adds the two link shapes this ADR makes load-bearing — the
  conserving **flux link** and the per-cell **error export**.
- **R5 (rationale strengthened):** cross-shard links carry the coarser endpoint's cadence (§7); the
  socket-saturation gate stands.
- **Sequence law unchanged:** simulation-LOD before scheduler work (ADR-0015); every step
  measure-gated by an Exp-D-style thesis-prover, with promotion catch-up gaining an explicit latency
  budget gate (the teleport case).

## Consequences

**Positive**

- **Tick cost decouples from population AND from elapsed unobserved time.** The spatial axis bounds
  the cut; the temporal axis bounds `Σ cost/dt`; the "never" rung makes the unobserved world nearly
  free (a seed and a trajectory). This is the only path consistent with "as many entities as we can
  throw numbers at."
- **One mechanism, fewer specials.** Warm 1/k, Cold periodic, flow ODEs, keyed-seed promotion, and
  the Geo kick become instances of one banded-cell + flux-link shape; a future cognition-fidelity
  axis plugs into the same budget without new law.
- **Distribution stops being a rewrite.** The hierarchy is the partition, cadence is the latency
  tolerance, and scaling from one CPU to many machines moves the budget, not the architecture.
- **The cut is observable from day one.** The error export gives the sim what geo-bench's sidecars
  gave Geo — the discipline that root-caused four streaming bugs in two days arrives with the
  substrate, not after it.

**Negative / costs**

- **More contract per cell** — flux links, hazard rates, the commitments ledger are each a new
  invariant surface; the ledger is by nature an unbounded-growth risk and must be budgeted/evicted
  from its first commit.
- **Multirate bugs are subtle** (flux drift, seam leakage across cadences); the conservation asserts
  in the deterministic reference mode are mandatory, not optional.
- **Fast-forward correctness rests entirely on the determinism discipline** (ADR-0016): any ambient
  state breaks reconstruction silently — the forkability clause (§8a) exists to keep that surface at
  zero.
- **A mis-designed error metric starves the wrong cells.** It must be tunable, its spend auditable
  (the export), and its first version is deliberately the proven ADR-0003 score — invention happens
  above a measured baseline.

## Alternatives considered

- **One global dt with update-skipping only (Warm 1/k everywhere).** Already in-tree in embryo;
  leaves minutes/hours-scale aggregates ticking at frame cadence for nothing and supplies no
  coupling rule. Subsumed as one band of this ADR's axis.
- **Pure discrete-event simulation for everything.** Right for the middle bands (an event queue is
  adaptive temporal resolution for free) and adoptable *inside* a cell; wrong as the organizing
  principle — the observed bubble is genuinely frame-clocked (the renderer's cadence) and the far
  field is cheaper as closed-form trajectories than as event storms. Kept as an intra-band
  mechanism.
- **Tick-through catch-up on promotion (no fast-forward).** `O(elapsed)` work at the worst possible
  moment (a teleport); fails any latency budget. The Geo lane already met exactly this bug shape
  ("slow blurry converge" on teleport). Rejected.
- **Global lockstep across shards.** Re-couples the planet at the finest cadence and destroys the
  latency tolerance the bands grant. Rejected — ADR-0016's per-lane scope generalizes per-cell
  instead.

## References

- Owner decree + conversation record: project memory `sim-tiles-temporal-lod-goal.md` (2026-06-11).
- Prior art: Berger & Oliger 1984 (AMR + subcycling); multirate ODE integration; Jefferson 1985
  (virtual time / Time Warp — the distributed DES case); Barnes & Hut 1986 (the opening-angle
  aggregate cut). The synthesis as one organizing principle for agent simulation is this project's.
- In-tree precedent: Exp D
  ([Docs/Plans/LogiRuntime-Design.md](../Plans/LogiRuntime-Design.md) §6.1 — 64× at 1 M, near-flat
  in P), `bevy_ecs::incremental::BucketIndex` (`Rust/crates/logi_ecs/src/incremental.rs`), the O(P)
  pass it replaces (`logi_sim/src/systems.rs:110`), the kick / graft-link discipline (ADR-0019 §6).
- Companions: [ADR-0015](ADR-0015-Simulation-LOD.md) (the spatial axis),
  [ADR-0016](ADR-0016-Per-Lane-Determinism.md) (the compression scheme),
  [ADR-0019](ADR-0019-Cells-Within-Cells-Interlinked.md) (the shape law + the ladder),
  [ADR-0020](ADR-0020-LogiEcs-Incremental-World-Cell.md) (the program of record),
  [ADR-0014](ADR-0014-LogiRuntime-Unified-Executor-Reactor.md) (the arbiter),
  [ADR-0003](ADR-0003-Representation-LOD.md) (the score the metric generalizes),
  [ADR-0017](ADR-0017-GPU-Resident-Statistical-Offload.md) (a Warm/Cold residency option,
  unchanged).
