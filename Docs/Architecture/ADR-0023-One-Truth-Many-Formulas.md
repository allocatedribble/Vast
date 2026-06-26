# ADR-0023 — One Truth, Many Formulas: the Resolution-Ladder Precedent

- **Status:** Accepted (2026-06-12) — **owner-decreed precedent, brother to ADR-0019.** First
  enforcement landed the same day in `logi_sky` (the fidelity tiers + the cross-tier agreement gate).
- **Date:** 2026-06-12
- **Deciders:** Vast architecture (owner directive, decreed while scoping LogiSky: the atmosphere must
  support different formulas — quality levels, precisions, approximations, up to exact — and "our
  system must work seamlessly at any scale").
- **Brother to:** [ADR-0019](ADR-0019-Cells-Within-Cells-Interlinked.md). The two are the same
  worldview on two axes: **0019 governs the STRUCTURE axis** (every abstraction = bounded self-similar
  cells, narrowly interlinked); **0023 governs the EVALUATION axis** (every quantity = one truth,
  evaluated by a ladder of formulas at declared resolutions). A design is finished only when it reads
  true on both axes.
- **Generalizes (sim side, already law):** ADR-0003 (representation LOD), ADR-0015 (whether an entity
  is simulated at all is a resolution), ADR-0021 (the timestep is a resolution, frame → day → never).
  This ADR extends the same law to **rendering and every other continuously-evaluated model**, starting
  with the atmosphere (ADR-0022).

## Context

The North Star is continuous zoom — orbit to a single character on foot — at a million-plus agents. No
single formula serves a quantity across that range: the evaluation that is exact and affordable on one
rung is unaffordable or needlessly precise on another. The codebase already lives this on the
simulation side (significance bands, the statistical far field, seed-and-fast-forward time), and the
2026-06-12 LogiSky work surfaced it on the rendering side unprompted: the transmittance integral exists
as an exact numerical form (the test oracle), a baked LUT (the runtime tier), and — to come — cheaper
analytic approximations for distant/secondary evaluations. The owner decreed the pattern precedent:
formula families are not an atmosphere quirk, they are how every model ships.

What goes wrong without the law: tiers grow private constants and drift apart (two truths); error
bounds are asserted, not tested; tier switches pop (the artifact class the kick/promotion discipline
exists to kill); and some scale ends up covered by *no* valid tier — the system "works" only at the
scales someone happened to test.

## Decision — the law

> **Every continuously-evaluated quantity is defined ONCE — an authoritative parameter set, in Rust —
> and evaluated by a FAMILY of formulas at declared (cost, error) resolutions, from an exact/reference
> form down through baked and approximate tiers. Tiers derive from the one truth, state and TEST their
> error bounds against the reference, hand off seamlessly inside overlapping validity domains, and
> together cover every scale the North Star spans. New evaluated models follow this precedent or argue,
> in an ADR, why they can't.**

Clauses:

1. **One truth.** All tiers of a family derive from the same parameter set (the model cell's
   authoritative state). A tier may *cache* derived data (a LUT, a fit, a table) but never *own*
   constants the other tiers can't see — private constants are how two truths are born. Parameter
   changes invalidate every derived tier through one generation counter.
2. **Declared, tested error.** Each tier declares its validity domain and its error bound versus the
   reference tier — and the bound is enforced by an **agreement test**, not asserted in a comment.
   Where the reference itself has a closed form on a subdomain, that closed form is the oracle of the
   oracle (the `logi_sky` zenith-transmittance golden test is the canonical example).
3. **The reference tier always exists in code.** The exact (or highest-fidelity numerical) form is
   implemented even when it never ships at runtime: it is the test oracle, the offline bake source, and
   the debugging arbiter. "Too slow to ship" is not "not worth writing" — it is *why* the cheap tiers
   can be trusted.
4. **Seamless handoff.** Tier selection is driven by scale/budget/exported error (the ADR-0015/0021
   machinery); switches happen inside overlapping validity domains where the divergence is below the
   declared tolerance. A visible pop or a sim discontinuity at a tier boundary is a bug of the same
   class as a kick/promotion pop (ADR-0019 clause 7) — the handoff is part of the family's contract,
   not the caller's problem.
5. **Scale-free coverage.** The family's domains must jointly cover the full scale range the system
   spans (orbit → on-foot; frame → never). A scale with no valid tier is a design hole found by the
   ladder (ADR-0019 clause 3), not a runtime surprise.
6. **Structure meets evaluation.** Each tier is a cell (bounded: its domain + budget + error); the
   handoff is a link; the exported error metric is the link's contract (the same exported-error law
   ADR-0021 demands of sim-cells). Run ADR-0019's design test on the family exactly as on any other
   abstraction.
7. **The tier set is a graded, observer-indexed alphabet** (the quantum re-evaluation, owner-prompted
   2026-06-12; ADR-0019 clauses 8–9 are the structural side of this coin). Fidelity is not a bit
   (low/high): a family is an *ordered alphabet* of resolutions, sized by the radix-economy argument
   like any alphabet (clause 9 there), and **which letter manifests is chosen by the observer** —
   camera distance, budget, error tolerance — the way a measurement basis picks the observable. The
   reference tier plays the un-collapsed state: it carries the full information every cheaper
   observation coarse-grains. Corollary: tier selection belongs to the OBSERVATION (the link crossing),
   never baked into the cell — one quantity may legitimately collapse to different letters for
   different observers in the same frame (the near camera's exact aerial perspective and a far probe's
   analytic haze are both correct, simultaneously).

## First enforcement (2026-06-12, `logi_sky`)

- The **reference tier** is `transmittance_to_top` (direct numerical integration, step count explicit).
- Its own oracle: the **closed-form zenith golden test** (exponential constituents integrate to βᵢHᵢ
  exactly; ozone tent to β·half-width) — clause 2 applied to the reference itself.
- The **baked tier** is the Bruneton-parameterized LUT + the CPU sampler `sample_transmittance_lut`;
  the **agreement gate** asserts the sampled tier stays within its declared bound of the reference
  across its domain (`TRANSMITTANCE_LUT_MAX_REL_ERROR`).
- One generation counter invalidates all derived tiers on parameter change (clause 1, already in
  `SkyState`). Analytic cheap tiers (e.g. Chapman-function transmittance for distant/secondary
  evaluations) slot in as new family members with their own declared bounds — the seam is reserved in
  [ADR-0022](ADR-0022-LogiSky-Rust-Modeled-Atmosphere.md) law 6 and the LogiSky plan.

## Consequences

- **Positive:** approximations become safe to ship (their error is a tested number, not a vibe); the
  exact form stops being "someday" code and becomes load-bearing as the oracle; scale holes surface at
  design time; quality knobs ("low/high/cinematic") fall out of the family for free instead of being
  bolted on per feature.
- **Negative / costs:** every family carries reference-tier maintenance + agreement tests (mitigated:
  the reference is usually the *simplest* implementation, just slow); domain/bound bookkeeping is real
  API surface. A family must earn its tiers — YAGNI binds: two tiers (reference + one shipped) is a
  complete family until a measured need adds a third (ADR-0019's "a cell must earn its membrane").
- **Anti-pattern named:** shipping ONLY an approximation with no in-repo reference is now a violation —
  that is how formula drift and untestable tuning enter a codebase.

## Alternatives considered

- **Per-feature quality enums without the law** (each renderer/sim picks its own approach): rejected —
  that is the status quo everywhere else and it produces exactly the private-constants drift and
  untested bounds this ADR forbids.
- **Always render/simulate exactly:** rejected by the North Star — no exact evaluation of scattering or
  a million agents fits any budget at every scale; exactness lives in the reference tier and the tests.
- **Folding this into ADR-0019 as another clause:** rejected by the owner's framing ("a brother") and by
  cohesion — 0019 is about *what an abstraction is*; 0023 is about *how a quantity is computed*. They
  cross-reference instead.

## References

- First instance: `Rust/crates/geo/logi_sky` (fidelity tiers + agreement gate; ADR-0022 — whose scope
  is ALL atmospheric rendering, amended same day).
- Sim-side instances already in force: ADR-0003 / ADR-0015 / ADR-0021.
- The structure axis: ADR-0019 (the design test runs on formula families too).
