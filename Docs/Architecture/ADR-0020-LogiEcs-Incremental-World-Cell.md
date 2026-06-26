# ADR-0020 — logi_ecs: the Adopted ECS Fork — the World as an Incremental, Interlinked Cell

- **Status:** Accepted (2026-06-11) — **owner directive**, with R0 + the first divergences landed the
  same day (see *Implementation status*).
- **Date:** 2026-06-11
- **Deciders:** Vast architecture (owner directive)
- **Amends:** [ADR-0014](ADR-0014-LogiRuntime-Unified-Executor-Reactor.md) — its re-eval clause
  *withdrew* a bevy_ecs fork because the precomputed-stage executor didn't need one; that reasoning
  stands, but this ADR forks on **new grounds** (the incremental-World program needs interior access
  the public seams cannot give). Extends [ADR-0004](ADR-0004-No-Compatibility-Code.md)'s vendored-
  exception precedent (cyper, Draco) to full adoption: the ECS is now **ours**.
- **Applies:** [ADR-0019](ADR-0019-Cells-Within-Cells-Interlinked.md) to the ECS itself, and is the
  implementation substrate for [ADR-0015](ADR-0015-Simulation-LOD.md) (budget-bounded World,
  far-field reservoir) and [ADR-0014](ADR-0014-LogiRuntime-Unified-Executor-Reactor.md) Regimes H/S.
- **Extended by:** [ADR-0021](ADR-0021-Sim-Tiles-Temporal-Simulation-LOD.md) (2026-06-11, same day)
  — the R-program now implements the **two-axis sim-tile law**: R3/R4/R5 re-scoped below (temporal
  bands, hazard triggers, the commitments ledger, lazy fast-forward, flux + error links), with the
  per-cell error metric designed first under R3.
- **Most affects:** `Rust/crates/logi_ecs` (+ `logi_ecs_macros`), every bevy_ecs consumer
  (`logi_sim`, `logi_runtime`, `logi_render_extract`, `logi_3dtiles_scene`).

## Context

The 2026-06-11 deep audit of bevy_ecs and our extensions concluded: the library's *interior* is
already cells-within-cells (`World → Archetype → Table → Column → row`) and clean; what it lacks is
any hierarchy **above the row set** — no notion of population that isn't a row, no incremental
top-K, no spatial containment, no nested worlds. Every wall on the path to "entity counts the world
has never comprehended" is a **missing container cell**, not a defective inner one: the O(P)
significance scan, the O(P) budget ranking, the flat extract, the u32 row cap, the single-socket
bandwidth wall.

Until now we composed around the public seams (`set_executor`, `Table::get_data_slice_for`, change
detection). The owner directive of 2026-06-11 changes the posture: **fork bevy_ecs into the project
as logi-ecs and refactor the World itself** — think of it *interactively, incrementally*: the World
should be a **cell**, interlinked. Several program steps genuinely need the interior:
write-time index maintenance hooks (value-change links, which no component hook exposes), making
`ParamSet` well-formed without rustc's name-keyed grandfather clause, in-band morsel/stage
execution, and shedding subsystems we will never ship.

## Decision — the law

> **The ECS is ours: `bevy_ecs` 0.19.0-rc.2 is adopted as `logi_ecs` (with `logi_ecs_macros`),
> pinned at the fork point, every divergence ledgered in `crates/logi_ecs/FORK.md`. The refactor
> direction is ADR-0019 applied to the World itself: the World is a CELL — bounded by budget, never
> population; its interior maintained INCREMENTALLY, O(Δ) per step, never by flat sweeps; and
> INTERLINKED — coupled to sibling Worlds, to the far-field reservoir, and to the renderer only
> through narrow, explicit, typed links. Physical layout may stay flat (ADR-0019 §4); the
> abstraction may not.**

### The program (R-series; FORK.md is the ledger of record)

- **R0 — fork lands green.** Package identity `logi_ecs`; consumers untouched via the `bevy_ecs`
  dependency key; upstream's full test suite stays green at every divergence. **Landed 2026-06-11**
  (882 upstream tests + new regression tests pass), with two same-day divergences: D-place-clamp
  (the audited `place`/`place_most_recent` boundary bugs, both impls) and D-incremental (below).
- **R1 — the incremental substrate.** `incremental::BucketIndex` (the proven Exp-D O(K+Δ) selector,
  moved in-band with the boundary-bucket partial-select fix) — **landed**. **The budget wiring
  landed 2026-06-12** (the audit's #1): `logi_sim::SignificanceIndex` (persistent resource, slot =
  entity index) + `allocate_representation_budget` re-shaped to evict-on-`RemovedComponents` /
  upsert-feed / quantized top-K / exact re-rank of the selected set — the per-frame O(P)
  rank-and-sort is gone (acceptance: quantized boundary + exact tiering within the selected set,
  locked by `budget_selection_is_quantized_top_k_on_a_live_world`; both determinism gates re-run
  green). Next: feed it from change detection at write time (the in-band O(Δ) link that shrinks
  the remaining O(P) upsert pass to O(Δ)).
- **R2 — the morsel lane in-band.** `logi_sim::morsel`'s proven Table-seam engine becomes a safe
  first-class API (ADR-0014 Regime H); the external `unsafe` column-pointer work retires.
- **R3 — band cells + budget.** Hot/Warm/Cold as archetype cells with Schmitt-trigger links;
  `SimulationBudget` bounds the World (ADR-0015 §1–2). Dormant is *non-existence* — the far-field
  reservoir (flows/stocks) lives beside the World, linked by keyed-seed `Promoted<T>`
  materialization (ADR-0015 §4–5, ADR-0016). **Re-scoped by ADR-0021:** *step 0* is the per-cell
  **error metric + its block-shaped export** (ADR-0021 §4 — designed before any band machinery
  builds; rides R1's score plumbing); each band then carries its **dt** (multirate advance, flux
  settled on the coarse cadence), the reservoir gains **hazard triggers** (keyed-RNG, replay-stable
  endogenous promotion) and a **bounded commitments ledger** (observation continuity), and
  materialization is **lazy fast-forward** (trajectory + conditioned seed — O(observed), never
  O(elapsed)) with the per-quantity conservation assert.
- **R4 — the membrane.** The cell-link shapes (latest-wins mailbox, delta channel, versioned POD
  block, snapshot ring) standardized where the towers can share them. **Re-scoped by ADR-0021:**
  plus the two links it makes load-bearing — the conserving **flux link** (accumulate-fine /
  settle-coarse) and the per-cell **error export**.
- **R5 — the shard fabric.** The container above the World: many `LogiWorld` cells (the
  `LogiEntityId.shard` field has named this since the schema was written), interlinked by the SAME
  demote-to-flow / promote-on-arrival link as R3, one rung up. Dissolves the u32 row cap and the
  single-socket bandwidth wall. Gated on a single World's budget actually saturating a socket.
  **Rationale strengthened by ADR-0021 §7:** cross-shard links carry the coarser endpoint's cadence
  — a cell's tick rate *is* its latency tolerance, which is what lets the fabric span real networks
  without lockstep.
- **R6 — executor cells in-band** (precomputed stages) when the system count demands it (the
  measured Exp-C gate stands).
- **D-paramset.** Make `ParamSet`/`SystemParam` well-formed under the tightened solver rules, then
  flip the crate symbol name to `logi_ecs` (see *Naming constraint*).
- **Shedding.** Subsystems the towers never use are cut once their absence is test-pinned
  (ADR-0004's no-compat law applies to the fork's interior).

### Naming constraint (until D-paramset)

The crate **symbol** stays `bevy_ecs`: rustc ships a compatibility hack for bevy's `ParamSet`
implied-bounds pattern **keyed on the crate name** — verified empirically 2026-06-11 (identical
sources + toolchain: compiles as `bevy_ecs`, fails E0477 on every `ParamSet<…>` system signature as
`logi_ecs`). Package, directory, and workspace identity are `logi_ecs`; consumers keep
`use bevy_ecs::…` via the dependency key, which also keeps the derive macros' manifest-key path
resolution working.

## Consequences

**Positive**

- **The interior is reachable.** Write-time incremental hooks, in-band executors, honest fixes
  (D-place-clamp shipped the day the fork landed — upstream's bug, our tree, fixed) instead of
  composing around what we cannot see.
- **The scale program has a home.** R1–R5 are exactly the missing container cells the audit named;
  each lands as a ledgered divergence with upstream's test suite as the regression floor.
- **The fork can shrink.** Upstream carries subsystems we never ship; ours doesn't have to.

**Negative / costs**

- **We own ~30k lines of audited `unsafe`.** No more free upstream fixes; every future bevy_ecs
  improvement we want is a deliberate cherry-pick. Mitigations: pinned fork point, minimal-diff
  discipline per divergence, the 882-test upstream suite + our gates green at every step, the
  FORK.md ledger.
- **A standing naming asymmetry** (package `logi_ecs`, symbol `bevy_ecs`) until D-paramset lands.
- **Tooling drift risk:** new nightlies may break upstream patterns we inherited (the ParamSet hack
  is the proof). The fork makes us responsible for those fixes.

## Alternatives considered

- **Keep composing around public seams (the ADR-0014 re-eval posture).** Right while the needs were
  executor-shaped (the seams sufficed); wrong for the incremental-World program — value-change
  hooks, ParamSet well-formedness, and shedding are interior work. Superseded by owner directive.
- **Vendor with `[patch.crates-io]` under the upstream name (the cyper shape).** Right for
  minimal-diff patch forks; wrong for an adopted, diverging crate — the patch table misleads about
  intent and blocks the identity rename. Rejected.
- **Rename every consumer to `use logi_ecs::` now.** Blocked by the rustc ParamSet hack (symbol
  must stay `bevy_ecs`) and would still need a macro-utils fork for derive path resolution; queued
  behind D-paramset instead of half-landing.

## References

- Ledger: `Rust/crates/logi_ecs/FORK.md` (divergences D-identity, D-place-clamp, D-incremental;
  queue). First code: `Rust/crates/logi_ecs/src/incremental.rs`,
  `src/relationship/relationship_source_collection.rs` (fix + regression test).
- The audit that named the missing cells: 2026-06-11 session (bevy_ecs + extensions deep audit);
  round-4 research `Docs/Research/2026-06-09-ECS-Round4-Pipeline-Fork-GPU-Research.md` (the Exp-D
  spike, the withdrawn-fork finding this ADR supersedes on new grounds).
- Companions: [ADR-0019](ADR-0019-Cells-Within-Cells-Interlinked.md) (the shape law),
  [ADR-0015](ADR-0015-Simulation-LOD.md) (budget/reservoir), [ADR-0016](ADR-0016-Per-Lane-Determinism.md)
  (`Promoted<T>`), [ADR-0014](ADR-0014-LogiRuntime-Unified-Executor-Reactor.md) (Regimes H/S),
  [ADR-0004](ADR-0004-No-Compatibility-Code.md) (vendored-exception precedent; no-compat inside the fork).
