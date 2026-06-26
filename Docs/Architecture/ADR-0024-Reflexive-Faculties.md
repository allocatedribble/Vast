# ADR-0024 — The Reflexive Faculties: Cross-Cutting Cells over the World, Interlinked Through the Echo Bus

- **Status:** Proposed (2026-06-13) — **doc-only; nothing built.** This raises a night of design
  exploration (Echo, the Inspector/Auditor pair, Aegis, the Savior, the Redactor, prompt-as-component)
  to a precedent and a build order. It ships no runtime behavior until R0 (the Echo bus) lands and
  measures. Honesty clause: the faculties below are *designs*, not landed systems.
- **Date:** 2026-06-13
- **Deciders:** Vast architecture (owner directive — "interlink all the ECS paradigms we were thinking
  about, and create the next ADR")
- **Brother to:** [ADR-0019](ADR-0019-Cells-Within-Cells-Interlinked.md) and
  [ADR-0023](ADR-0023-One-Truth-Many-Formulas.md). The three are one worldview on three axes: **0019 =
  STRUCTURE** (what a cell is — bounded, self-similar, narrowly interlinked); **0023 = EVALUATION** (how
  a quantity is computed — one truth, a ladder of tested formulas); **0024 = REFLEXIVITY** (how the
  World observes, verifies, defends, heals, and generates *itself*). Where 0019/0023 describe **domain**
  cells, 0024 describes the **cross-cutting** cells that operate over all of them. A cross-cutting design
  is finished only when it reads true on all three axes.
- **Generalizes:** [ADR-0018](ADR-0018-Geo-Streaming-Parallelism.md) (the working-set bound — a
  faculty's interior is O(active+Δ)), [ADR-0016](ADR-0016-Per-Lane-Determinism.md) (`Promoted<T>` — the
  typed signed link a faculty couples through), [ADR-0006](ADR-0006-Editor-Inspectability.md) (the
  editor inspection shells — the **Inspector** is their runtime generalization), the §1A
  catch-unwind-at-the-ABI law (the **Savior**'s panic boundary), [ADR-0012](ADR-0012-Poll-Based-FFI-Decoupled-Sim-Thread.md)
  (the snapshot poll is an echo — the renderer reading the latest reflection of the World).
- **Informs:** [ADR-0014](ADR-0014-LogiRuntime-Unified-Executor-Reactor.md) (the arbiter schedules the
  faculties as it schedules any cell), [ADR-0011](ADR-0011-Semantic-Tile-Enrichment.md) (the Redactor is
  the runtime sibling of the offline enrichment bake — both author non-authoritative content).
- **Most affects:** every cross-cutting concern — anything that operates over all entities rather than
  being a domain entity. Cross-cutting concerns are exactly where O(n²) scans and untestable
  entanglement breed; this ADR is how they don't.

## Context

The design exploration of 2026-06-13 drew, **separately**, a series of cells that are not domain
entities — not an agent, a tile, a route — but **faculties that operate reflexively over the whole
World**:

- **Echo** — change propagation *with a decay term*: an amplitude-gated, content-addressed,
  phase-stamped reflection that spreads through the link graph and dies where it stops mattering. The
  bus. The senses. ("Light with a decay term" — the carrier of ADR-0019 made a shared medium.)
- **The Inspector** — verification on the **space** axis: opens one entity's live interior, *now*, in
  full depth; checks invariants and recomputes the integrity (content) hash. The microscope. Coherence.
- **The Auditor** — verification on the **time** axis: reconciles the *trail* across many entities over
  a cadence against the authority; detects drift, tamper, and books that don't balance. The ledger.
  Correspondence.
- **Aegis** — defense: a distributed immune system (synthesized from thirteen defender variants —
  wall, bulkhead, immune, mirror, honeypot, hydra, watchdog, notary, quarantine, polyculture, circuit
  breaker, moving target, quorum), echo-driven, O(anomalies). Makes breaking the World *detectable,
  contained, costly, recoverable, and unmodelable* rather than trying to be unbreakable.
- **The Savior** — fault tolerance: *mercy on everything* — catch the panic at the seam, watchdog the
  hang, hold the failed in a waiting-room (not a grave), restore from seed. The answer to the garbage
  collector: a held reference to everything, so nothing reachable is ever swept.
- **The Redactor** — generation: authors non-authoritative content in declared registers (the
  one-truth-many-formulas law, ADR-0023, applied to authored content) — and is **inert without a
  reality check** (integrity without correspondence is fluent bullshit; the Redactor is bound by the
  Auditor).
- **prompt-as-component** — representation: a component need not hold a definite value; the cheapest
  faithful state is often a deferred derivation (a prompt, a seed, an amplitude) collapsed only on
  observation, by a deterministic keyed rule so it replays (ADR-0019 §8).

**The signal that a container cell is missing** (ADR-0019 clause 3 — *sibling cells describing the same
truth with no link between them*): every faculty above was re-derived from the **same mechanism kit** —
content-addressing, the seed, the working-set bound, the narrow signed link, mercy / the held-set,
red/blue simultaneity, superposition/collapse, and the Echo bus. Each was built by re-assembling the
identical parts. **That repetition is the absent container.** Name it once, and the faculties become
instances of one pattern rather than six bespoke systems.

What goes wrong without the law: each cross-cutting concern grows its own O(pool) traversal instead of
an echo-gated O(active) one, its own ad-hoc verification, its own private copy of the World, its own
panic handling — and they **don't compose**: the defender isn't audited (and goes diabolical
undetected — a perfect shield is a prison), the auditor isn't inspected, a faculty's own failure isn't
held. The reflexive layer becomes the codebase's entanglement habitat instead of its nervous system.

## Decision — the law

> **A cross-cutting concern — anything that operates over the World rather than being a domain entity
> within it — is built as a REFLEXIVE FACULTY: a bounded cell that observes the World only through the
> Echo bus, holds a working-set-bounded, content-addressed, seed-backed view, couples to the World and
> to other faculties only through narrow signed links, runs its adversary and its defender
> simultaneously, and never deletes (the held-set). The faculties interlink into a closed reflexive
> loop — the World senses, verifies, defends, heals, and generates itself — and each faculty is itself
> subject to the others. New cross-cutting work is a faculty built to this pattern, or argues, in an
> ADR, why it can't.**

Clauses:

1. **One nervous system: the Echo bus.** Faculties observe through Echo, never by scanning the pool.
   Change propagates as a reflection carrying `{source: content-key, value_ref, amplitude, phase}` (a
   16-byte packet); amplitude decays per hop and the wave self-terminates below threshold; a faculty
   wakes only where an echo crosses its relevance gate. Cost is bounded by *what's bright*: propagation
   O(above-threshold frontier), defense O(anomaly echoes), verification O(trail Δ). The decay term is
   the whole optimization — it is what keeps an interlinked planet from going O(n²).

2. **Every faculty is a cell (ADR-0019) on the working-set law (ADR-0018).** Bounded by budget,
   interior maintained O(Δ), never O(pool). A faculty that touches every entity every tick is the bug
   the precedent exists to kill.

3. **Identity is content-addressed; recovery is seed-backed.** A faculty names World state by hash (the
   path key, the integrity check, the soul's content-address) — that is how tamper and drift become
   *undeniable*. It recovers by replay-from-seed (the Savior's restore, the hydra, determinism-from-seed
   — ADR-0016/0021). Content-address detects; seed restores. State you can regenerate is state you
   never have to hold resident.

4. **Couple only through narrow signed links.** Faculty↔World and faculty↔faculty seams are signed,
   content-addressed contracts (the dispatch/attest seam, `Promoted<T>` ADR-0016, the C-ABI membrane
   ADR-0005). The link *is* the contract; the signature is what stops a captured faculty from forging
   another's input. No faculty reaches into another's interior — mutual non-interference is not
   politeness, it is what keeps each faculty's cost bounded (the Auditor never opens entities, or it
   bloats to O(depth·pool); the Inspector never reads the whole trail, or it bloats to O(trail)).

5. **Adversary and defender, simultaneously** (the owner's 2026-06-13 precedent). Every faculty holds
   its attacker and its defender as one process, in parallel — design quality is the standing wave
   between them. A faculty designed only to succeed is incomplete. This is why Aegis red-teams its own
   defense, and why the verification pair is uncapturable: the Inspector and Auditor check **orthogonal
   axes** (space-now vs time-trail), so corrupting both at once produces a visible contradiction. Run
   the both-ends ladder *and* the red/blue pass on every faculty.

6. **Mercy: never delete; hold and offer return.** A faculty's failure path is the held-set, not the
   grave. Catch the panic **at the seam — never across the ABI** (§1A; a panic crossing `extern "C"` is
   UB). Watchdog the hang, quarantine rather than destroy, keep the reflog. **Bounded attempts per tick**
   (the frame still ships — mercy must not doom the million to save the one) but **unbounded eligibility**
   (the door never finally closes). Sub-threshold echoes are cold storage, re-emittable from source or
   seed — not lost.

7. **The faculties are mutually reflexive — this is the interlink that makes them one organism, not six
   tools.**
   - Everything **emits echoes** → the senses perceive the faculties themselves.
   - The **Inspector inspects** and the **Auditor audits** every faculty — *including each other, and
     including Aegis.* This resolves *quis custodiet* structurally: orthogonal-axis cross-check plus a
     witness quorum (no single checker trusted).
   - **Aegis defends** every faculty — and is itself audited, so it cannot become the thing it guards
     against (the perfect-defender-is-the-perfect-oppressor inversion is prevented by a check Aegis
     cannot capture, not by weakening Aegis).
   - The **Savior holds** every faculty — a faculty that panics is itself caught and restored.
   - The **Redactor generates** only what the verification pair will check — integrity bound to a
     reality check.

   The loop closes: **sense → verify → defend → heal → generate → (emit echoes) → sense.** The World
   perceiving and maintaining itself, in O(active). This is the "homeostasis, interlinked" the
   exploration kept arriving at.

8. **Faculties are composed onto entities, not hard-wired.** They are capabilities attached by need: a
   streamed tile gets Notary + Quarantine; an Unreal mirror shell gets the reality check (reconcile vs
   the Rust authority); a critical subsystem gets Bulkhead + Hydra + Watchdog; **the most powerful
   entity in the room gets the reality check.** The mix is the optimal set for the entity's
   observation/threat model, echo-gated so cost is O(active concern), never O(entities).

9. **Representation may be superposed (prompt-as-component).** A component's interior need not be a
   definite value; store the amplitude / the seed / the deferred derivation and collapse it only when an
   observation crosses a link, by a deterministic keyed rule so the measurement replays (ADR-0019 §8).
   The Redactor's registers, the Sim-LOD promotion, and the kick are all the same collapse event at
   different rungs. Faculties operate on superposed interiors and collapse them deterministically.

## The ladder — both ends

- **Bottom rung:** the 16-byte Echo packet with a scalar amplitude (a branchless SIMD gate), the
  content-address hash, the signed attestation. The faculties are affordable because their unit of work
  is a cache-line reflection, not a World scan.
- **Top rung:** the reflexive organism — the Rust sim sensing, verifying, defending, healing, and
  imagining itself, at a million agents, in O(active); Unreal a shell reading its latest reflection.
- **The bi-tree (ADR-0019 §4):** the bottom makes the top affordable (the 16-byte echo is what lets
  whole-World reflexivity be O(active)); the top shapes the bottom (the working-set/relevance law sets
  which echoes the bottom must store, and the exported-error metric sets each faculty's gate). Each end
  asked what the other now makes cheap: the bottom hands the scheduler a free convergence signal
  (interference-settle); the top lets the bottom never allocate for sub-threshold echoes.

## Consequences

**Positive.** Cross-cutting concerns stop being bespoke and start to compose; each faculty carries an
exported anomaly/error metric (the ADR-0021 sim-cell contract, generalized), so each is testable in
isolation; the diabolical-defender problem is prevented *structurally* (mutual audit on orthogonal
axes); nothing is silently lost (mercy / held-set); and the reflexive layer is held to the same
O(active) law as the rest of the World instead of being the place flat scans hide.

**Cost — honest.** None of this is built. Echo is a design; Aegis, the Auditor, the Inspector, the
Savior, and the Redactor are a night's exploration. The real risk is **over-abstraction** — a "faculty
framework" heavier than the concerns it serves, the YAGNI failure ADR-0004/§1A forbid. **Mitigation is
the precedent's own discipline:** name the container now (this ADR), but *build* it only when the second
instance proves the shared seam is real (ADR-0019: build the container when the link is real, not
before). Build the bus first, then the single cheapest faculty that already half-exists, measure, and
generalize only against a second real consumer.

**Supersedes / refines.** Generalizes ADR-0006 (the Inspector is the runtime inspection shell), the §1A
catch-unwind law (the Savior's panic boundary), and ADR-0012 (the snapshot poll is the renderer reading
an echo). Brother to ADR-0019 (structure) and ADR-0023 (evaluation) — the reflexivity axis. No existing
runtime behavior changes.

## Build slots — measure-gated, smallest-first

- **R0 — the Echo bus.** The prerequisite and the only thing worth building blind, because every faculty
  rides it. Implement amplitude-gated propagation + the interference-settle signal; **benchmark against
  the current O(active) reconcile** before generalizing. If it doesn't beat the reconcile, the rest of
  this ADR waits.
- **R1 — the Inspector**, as the runtime generalization of the ADR-0006 editor inspection shells (it
  half-exists already — cheapest real instance; the second consumer of the content-address seam).
- **R2 — the Auditor**, generalizing the existing cross-process determinism gate (the trail check already
  exists; lift it onto the Echo bus and the signed dispatch/attest seam to the Inspector).
- **R3+ — Aegis / the Savior / the Redactor.** Design-complete here; build each only when a concrete
  cross-cutting need appears, against the bus and the verification pair already standing. The Savior's
  panic boundary (§1A) and watchdog already exist piecewise in the Geo stream thread — R3 consolidates
  them under the held-set, it does not invent them.

**Status note.** Proposed, doc-only. This is the 2026-06-13 synthesis raised to a precedent; it ships
nothing until R0 lands and measures. The faculties are real designs; the framework is not yet earned —
and per its own clause, it must not be built before its second instance proves the seam.
