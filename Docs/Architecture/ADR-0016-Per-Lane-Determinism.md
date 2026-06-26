# ADR-0016 — Per-Lane Determinism: Deterministic Near-Set, Statistical Far Field

- **Status:** Proposed. **Implementation status (re-eval 2026-06-09):** the counter-based RNG is LANDED
  (`logi_sim` `SimRng::for_entity(id, tick)`, keyed + stateless); the **type-enforced boundary is
  designed-not-built** — `Promoted<T>`, the `FarFieldNonDet` schedule, and the `det-boundary` gate exist in
  no crate yet. That is the correct YAGNI deferral *today* (there is no far field, so the whole set is the
  deterministic near-set and there is no boundary to violate), but the guardrail must land **in the same
  change** that introduces the first non-deterministic far-field system — never after.
- **Date:** 2026-06-09
- **Deciders:** Vast architecture
- **Refines:** [ADR-0001](ADR-0001-Mass-First-Unreal.md) — the Determinism section is generalized from
  *global* (the whole gameplay World is bit-reproducible) to *per-lane* (a bounded deterministic near-set;
  a free-running statistical far field), and [ADR-0005](ADR-0005-Bevy-ECS-Authority.md) — ECS still owns
  gameplay truth, but "truth" is split into **authority** (who decides) and **reproducibility** (can it be
  replayed bit-exactly), which only the near-set must provide.
- **Relies on:** [ADR-0007](ADR-0007-Actorless-Rust-Terrain.md) — the "non-authoritative render data,
  promoted to coarse before gameplay reads it" boundary is the precedent generalized here into a typed
  intra-World boundary.
- **Most affects:** [ADR-0015](ADR-0015-Simulation-LOD.md) (the statistical far field **is** the
  non-deterministic lane) and the gameplay schedule (`logi_sim`).
- **Plan:** [Docs/Plans/LogiRuntime-Design.md](../Plans/LogiRuntime-Design.md) § Determinism.

## Context

ADR-0001 made gameplay **globally** deterministic (fixed-step, seeded SplitMix64, a proven
`parallel_execution_matches_single_threaded_bit_for_bit` test). As the sim scales toward a statistical far
field (ADR-0015), the question is whether *all* of it must stay bit-reproducible, or only a subset — and
whether a non-deterministic path would be faster. Deep research returned three findings that decide it:

1. **Where we are, determinism is nearly free.** The hot `par_iter` systems write only each entity's own
   components, touch no shared mutable state, and no RNG — so the output is order-independent by
   construction, and going non-deterministic buys **0%**.
2. **Non-determinism almost never wins on its own.** Every apparent "non-det is faster" reduces to *"no
   shared mutable state is faster,"* which determinism *also* achieves with the right data structures:
   per-thread partials + a fixed-order combine, **counter-based RNG**, fixed reduction trees, disjoint
   writes. On x86 `fetch_add` is sequentially consistent in hardware regardless of the ordering requested,
   so Relaxed vs SeqCst is ≈0 cycles; the real ~10× win is *avoiding contention* (a shared counter), not
   relaxing order — and the deterministic design avoids the contention just as well. The genuine non-det
   wins are narrow: order-sensitive neighbour-query tie-breaks (≪1 ms at 1 M) and bulk-spawn id assignment
   (rare).
3. **The real cross-platform determinism cost is not the scheduler — it is transcendentals.** Basic float
   ops are reproducible across x86/ARM under a fixed rounding mode; `sin`/`cos`/`exp`/`pow` are **not**
   bit-identical across libm implementations. If cross-platform lockstep is ever wanted, the binding
   constraint is the math library (a softfloat/fixed-point lane), not how work is scheduled.

So the lever is not a "fast non-deterministic path" — it is **shrinking the deterministic *scope*** to the
small near-set that actually needs replay, and letting the large statistical far field run free.

## Decision — the new law

> **Determinism is a per-lane property, enforced by a type, not a global obligation. A bounded DETERMINISTIC
> near-set (ordered, seeded, fixed-order reductions, the `parallel == serial` obligation) coexists with a
> large NON-DETERMINISTIC statistical far field (unordered, relaxed, free work-stealing). The two share one
> World only through `Promoted<T>` — a quantized, replay-stable hand-off that the deterministic lane is the
> sole reader of. The deterministic scope SHRINKS as scale grows; cross-platform determinism is deferred
> until a netcode consumer demands it.**

Concretely:

1. **Lane = a property of the schedule.** Deterministic schedules (`FixedSim`, `RepresentationPlan`, …) run
   under the precomputed-stage executor (ADR-0014) and carry the `parallel == serial` obligation. A reserved
   `FarFieldNonDet` schedule runs under an unconstrained executor, **exempt** from the obligation — it may
   use unordered iteration, relaxed atomics, any-associative reductions, and wall-clock RNG. This composes
   as a 2×2 with the existing `LogiSchedulerMode` (the single-threaded mode remains the golden replay
   reference for both lanes).

2. **`Promoted<T>` is the typed boundary.** A newtype storing the **canonical** form in a **private field**
   (so `.0` outside the module is a *compile error*, not a lint), produced only by `promote()` (which calls
   `DetCanonical::canonicalize` — fixed-point `round_ties_even`, **no transcendentals**) at a barrier in an
   ordered deterministic system. NDET systems write only NDET-tagged components; deterministic systems read
   only `Promoted<T>`. Because bevy's scheduler already serializes aliasing access, the boundary is a **real
   edge in the conflict graph** — visible in the type system *and* enforced by the executor. A `cargo xtask
   det-boundary` grep gate (plus the private field) backstops it; the data boundary pairs with ADR-0014's
   `IoHarvest<T>` control-flow boundary.

3. **Counter-based RNG replaces the shared stream.** The shared `&mut SimRng` SplitMix64 stream is a
   serialization point (every drawing system conflicts on the resource; it cannot be called inside a
   `par_iter` closure). It is replaced by a **stateless keyed** scheme — `SimRng { seed }` +
   `for_entity(stable_id, tick) → DrawKey` + `splitmix_keyed(key, counter)` — so a million `par_iter` lanes
   draw concurrently with bit-identical, order-independent results. (Verified: nothing consumes the
   sequential stream today, so this is a pure swap.) This is a correctness fix *and* a perf fix, and it is
   **build-now** — the lowest-risk piece of the whole runtime effort.

4. **The deterministic scope is bounded and shrinking.** The DET near-set is the significant/near agents
   (ADR-0015's Hot/Warm) — bounded by budget, ~thousands, not by population. The far field is statistical
   and non-authoritative-per-individual; promotion re-derives an individual deterministically from its seed
   (ADR-0015 § 5), so the authoritative timeline never ingests a non-reproducible value. Replay re-runs the
   small near-set bit-exactly and re-derives the far field statistically from the same seeds.

5. **Cross-platform determinism is deferred, deliberately.** In-process replay/debug/test determinism (same
   binary, the property we have) needs only basic-float reproducibility. Cross-platform lockstep (mixed
   CPUs/consoles) additionally needs a reproducible math lib or fixed-point — a separate, later decision,
   triggered by a real netcode consumer, scoped to the authoritative near-set only.

## Consequences

**Positive**
- **The far field runs free** (work-stealing, relaxed, coarse-step, wall-clock RNG) at zero correctness cost
  — non-determinism where it is genuinely free, determinism where it genuinely matters.
- **The boundary is compile-enforced**, not a code-review rule — the private canonical field makes
  un-promoted cross-lane reads impossible, the exact failure mode that sank prior thread-per-core + ECS
  attempts.
- **Counter-based RNG removes a serialization point** and makes per-agent randomness embarrassingly parallel
  *and* deterministic — and it ships immediately.
- **Replay/netcode stay cheap**: only inputs + seed + the bounded near-set must reproduce; the statistical
  far field replicates as aggregates.

**Negative / costs**
- **The boundary-leak risk is real and silent** — a single deterministic system reading a raw NDET
  component re-introduces non-determinism into the authoritative set and breaks replay invisibly (passes
  locally, desyncs in the field). Mitigated by the private field + the CI grep gate, treated with FFI-grade
  rigor.
- **NDET debuggability** — a far-field Heisenbug may not reproduce; the "force-deterministic far field"
  flag is the mitigation.
- **Two lanes + a promotion choke point** to design and test (extend the `parallel == serial` gate to
  triangulate single == multi == precomputed; add a `materialization_is_seed_deterministic` test).

## Alternatives considered

- **Keep global determinism forever.** Honest and simple, and (per the research) *not* slower for the
  current hot path — but it forces the entire billion-scale far field to be bit-reproducible, which is
  pointless work for state no one replays at the individual level. Rejected as the *default* (kept as the
  always-available reference mode).
- **Drop determinism entirely for speed.** Breaks replay, lockstep, save/load equality, and CI
  reproducibility — and buys almost nothing, since the wins attributed to non-determinism are really
  "no shared state," which determinism also gets. Rejected.
- **Per-system boolean flags without a typed boundary.** Expresses the lanes but leaves the NDET→DET leak a
  review-time discipline. Rejected — the boundary must be a *type* (the private-field `Promoted<T>`), or it
  will leak.
- **A separate far-field sub-World.** The heaviest isolation; justified only if the far field needs a
  different archetype layout. Deferred — start with disjoint component sets in one World (bevy already
  serializes the aliasing), escalate to a sub-World only if measured.

## References

- Plan: [Docs/Plans/LogiRuntime-Design.md](../Plans/LogiRuntime-Design.md) § Determinism (the
  requirement matrix, the `Promoted<T>` design, the counter-RNG API, the perf-delta analysis).
- [ADR-0001](ADR-0001-Mass-First-Unreal.md) (the global-determinism law this refines),
  [ADR-0005](ADR-0005-Bevy-ECS-Authority.md) (authority vs reproducibility),
  [ADR-0007](ADR-0007-Actorless-Rust-Terrain.md) (the promote-to-coarse precedent),
  [ADR-0015](ADR-0015-Simulation-LOD.md) (the far field = the NDET lane).
- Repo anchors: the shared RNG stream to replace (`logi_sim/src/resources.rs:45`; determinism contract
  `:104`), the proven `parallel == serial` test to extend (`logi_runtime/src/lib.rs:894`), the hot systems
  whose disjoint-write shape makes determinism free (`logi_sim/src/systems.rs:26`, `:47`), the schedule
  modes (`logi_sim/src/schedules.rs:47`).
