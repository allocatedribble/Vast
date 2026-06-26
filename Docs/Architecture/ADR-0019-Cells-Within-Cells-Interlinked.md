# ADR-0019 — Cells Within Cells, Interlinked: the Structural Precedent

- **Status:** Accepted (2026-06-11) — **owner-decreed precedent**, with first enforcement **landed the same
  day** (the three Geo fixes under *Context → First enforcement*). This ADR is the precedent's
  constitutional home; the `CLAUDE.md` section of the same name is its operational mirror (this document
  governs on any divergence).
- **Date:** 2026-06-11
- **Deciders:** Vast architecture (owner directive)
- **Generalizes:** [ADR-0005](ADR-0005-Bevy-ECS-Authority.md) (the narrow, one-directional C ABI — the
  outermost cell membrane), [ADR-0015](ADR-0015-Simulation-LOD.md) (cells bounded by **budget**, never
  population), [ADR-0016](ADR-0016-Per-Lane-Determinism.md) (`Promoted<T>` — the typed link between
  determinism cells), [ADR-0018](ADR-0018-Geo-Streaming-Parallelism.md) (per-step work bounded by the
  active working-set — the cell's live interior). None of those change; this ADR names the shape they
  already share so new work can be held to it.
- **Informs:** [ADR-0014](ADR-0014-LogiRuntime-Unified-Executor-Reactor.md) (the arbiter is the
  not-yet-built **container cell** for the runtime's self-spawned thread cells) and
  [ADR-0007](ADR-0007-Actorless-Rust-Terrain.md)'s promote-to-coarse clause (the not-yet-built
  **Geo→gameplay link** — coarse height/collision). Both were independently re-derived by this
  precedent's ladder, which is part of the evidence it finds real structure.
- **Most affects:** every new abstraction, and any refactor of an entangled one. This ADR has no runtime
  behavior of its own.

## Context

The strongest designs in this codebase already share one shape: **bounded, self-similar units nested
hierarchically, coupled only through narrow, explicit links** — "cells within cells, interlinked" (the
baseline-test line the owner decreed as precedent on 2026-06-11). Verified instances, bottom rung to top:

- **Identity is containment.** A tile's 64-bit key is computed by *extending its parent's FNV state* one
  path segment at a time (`logi_3dtiles_select::PathArena` / `fnv_extend`), and that single key interlinks
  the whole stack unbroken: entity `TileId` → `FlashCache` key → FFI `tile_key` → the C++ section cache.
- **Data cells nest five deep.** Planet → tileset tree → grafted external tilesets (attachable/detachable
  whole, `Grafts` + `prune_cold_grafts`) → implicit `.subtree` availability cells → tiles → primitives →
  vertices (the 32-byte interleaved cell, linked by indices).
- **Compute cells own their state exclusively.** Game thread ⟷ stream thread ⟷ fetch workers ⟷ decode
  workers ⟷ render thread — every boundary a typed mailbox (newest-wins view, latest-wins
  `Arc<GeoRenderFrame>`, priority heaps, `TQueue`), never shared mutable interiors.
- **Membranes are versioned and self-describing at every scale.** The geometry blob's magic + exact-length
  header (`LGM3`) and the version-locked, offset-asserted C ABI are the *same membrane shape* at two rungs
  five orders of magnitude apart.
- **Subtree summaries, not interiors, cross upward.** The selection traversal folds each subtree to a tiny
  `TraversalDetails`; the kick renders a parent cell as the stand-in until *all* child cells are ready,
  then swaps atomically.
- **Cell autonomy beat sibling coupling where both were tried.** Skirts seal each tile's own boundary
  (no neighbor links — cesium's conclusion too); hole-clip, the one sanctioned sibling-reach, was the
  pipeline's most bug-prone seam until it got its own per-frame refresh discipline (`RefreshTileClips`).

The precedent was first recorded in `CLAUDE.md` — but `CLAUDE.md` states it does not override `AGENTS.md`,
and the precedent's own enforcement clause routes exceptions through ADRs. Per the constitution's preamble
("exceptions are documents, not code comments"), a constraint with architectural force must *be* an ADR.
This is it.

### First enforcement (landed 2026-06-11, same day as the decree)

A full pipeline review under the precedent found three violations; all were fixed and verified
(workspace tests + clippy + fmt green; editor rebuild Succeeded):

1. **`GeoStreaming` was a flat ~30-field mega-cell** whose cross-field invariants were the pipeline's
   proven bug habitat (the graft-skip scar, the rio session wedge, the prefetch de-protection — all
   multi-field invariant breaks inside the bag). Decomposed into five cells with their invariants
   documented on the cell: `Grafts`, `FailureLedger`, `SubtreeStreams`, `Prefetcher`, `SessionLane`
   (`logi_3dtiles_scene/src/geo_world.rs`).
2. **Telemetry crossed three seams as a hand-mirrored sprawl** (~25 atomics, ten packed FFI accessors;
   every new counter cost three mirror edits and an ABI bump). Replaced by one POD block crossing each
   seam whole — `GeoTelemetry` / `LogiGlobeTelemetry`, ABI v16, ten accessors deleted (ADR-0004
   no-compat). Adding a counter is now one field per mirror plus an offset lock, not a new entry point.
3. **The editor stream view was linked to an unstable cell** (`GCurrentLevelEditingViewportClient` — the
   focus cursor, possibly an ortho pane), with the zenith sun keyed off it. Re-bound to a perspective
   level viewport (`LogiTileRenderShared.cpp`).

The review's ladder also *re-derived* ADR-0014's arbiter ("Geo's threads are sibling cells with no
container") and *diagnosed* the 2026-06-11 settled-shot sky collapse as a missing link (two sibling cells —
SkyAtmosphere's planet model and the streamed WGS84 globe — describing the same planet with no link
between them; fixed by the v15 `sky_anchor`, which exports the planet truth from its single home in Rust).

## Decision — the law

> **Every abstraction is designed as cells within cells, interlinked: bounded, self-similar units nested
> hierarchically, coupled only through narrow, explicit links. Hold every design at its lowest rung and its
> highest rung in parallel, then use both to draw the cell one level above the current highest; repeat until
> the climb closes at the North-Star cell (the shipped game at a million agents). An abstraction is settled
> when the same cells-interlinked shape reads true at every rung; a rung where it doesn't is the next piece
> of work. New work follows this precedent or argues, in an ADR, why it can't.**

Concretely:

1. **Cells are bounded.** A cell owns its invariants, its budget, and its working set — bounded by budget
   or active set, never by population (ADR-0015/0018). Its invariants are documented *on the cell* (the
   struct/module), not scattered across call sites. Nesting is the mechanism that turns `O(n)` into
   `O(log n)` / `O(active)`; a flat sweep over a planet or a million agents does not ship (§1E).
2. **Links are narrow, explicit, and the whole contract.** Sanctioned link shapes, all proven in-tree:
   the trait seam (`TileContentSource`, `AvailabilityProvider`), the injected function
   (`TileDecodeFn`), the immutable shared artifact (`Arc<TileGeometry>` — the lifetime *is* the
   contract), the latest-/newest-wins mailbox, the versioned self-describing POD block (geom blob, the
   C ABI structs, `LogiGlobeTelemetry`), and key-derived identity single-homed across rungs
   (`path_key`). Everything behind a link stays private; a design whose cells reach into each other's
   interiors is entangled — redraw it.
3. **The ladder is the design procedure.** Run the lens at both ends before building; a missing container
   shows up as sibling cells describing the same truth with no link between them (the sky/globe planet
   model was exactly this). Name the cell, its container, and its links *before* the implementation.
4. **The bi-tree optimization rule** (owner addition, 2026-06-12). The hierarchy is a tree with two ends,
   and optimization runs at **both ends at once — each end optimized FOR the opposite end, simultaneously
   and independently**:
   - *Shape the bottom for the top.* The byte layout, the key encoding, the instruction sequence are
     chosen so the containing cells' operations become trivial: the 32-byte interleaved vertex exists so
     the render-thread upload is one flat memcpy; the incremental FNV path key exists so every map above
     it probes O(1); the flash deposit on `free_slot` (2026-06-12) is a bottom-end `Arc` hand-off shaped
     for the top-end revisit pattern.
   - *Shape the top for the bottom.* The architecture, the schedule, the link shapes are chosen so leaf
     work stays cheap: the poll-FFI exists so per-tile work never waits on the game thread (ADR-0012);
     the working-set law exists so per-step instruction count is O(active), never O(pool) (ADR-0018);
     the one-budget disk cell exists so each tier's trim loop stays a local LRU.
   - *Simultaneously, independently.* The two passes are independent lanes (§1E): optimizing one end
     must not serialize on the other — they meet only at the link between them, and that independence is
     exactly what a well-drawn link buys. A change at one end that forces re-opening the opposite end's
     interior is the signal that the link is mis-drawn, not a reason to fuse the passes. An optimization
     is finished only when BOTH ends have been asked "what does the opposite end now make cheap that
     wasn't before?" — one-ended optimization leaves the other half of the win on the table.
5. **Flat storage is not a flat abstraction.** Memory layout may flatten for the instruction-count law —
   one archetype, pre-spawned pools, field-flips instead of structural churn — while the hierarchy lives
   in identity and data (PathArena, the tileset tree). This precedent governs **abstractions and links**,
   never physical layout; the Geo World is the proof the two laws compose.
6. **Cross-hierarchy links carry their own refresh discipline.** A link that cuts across containment (a
   parent's output depending on its *children's* state — hole-clip) cannot ride a containment-shaped
   protocol (a delta that never revisits the parent); it must be re-evaluated per frame or carry an
   explicit invalidation. Budget for that discipline when the feature forces such a link — or redesign
   for cell autonomy (skirts) and pay the redundancy instead.
7. **Self-similar patterns are shared property.** The *aggregate-stands-in-until-gated-atomic-swap*
   pattern is one abstraction at two rungs: the Geo kick (coarse parent holds until children are
   textured) and ADR-0015's promotion (statistical flow holds until an individual is promoted through
   ADR-0016's `Promoted<T>`). Design changes to either must consult the other; future LOD/kick research
   reads as work on both.
8. **Superposed interiors, collapse at the membrane** (the quantum re-evaluation, owner-prompted
   2026-06-12). A cell's interior state need not be a definite letter: the cheapest faithful
   representation is often a *distribution over an alphabet* — a statistical aggregate (ADR-0015's far
   field), a seed (ADR-0021's determinism-from-seed), an unevaluated derivation (`logi_sky`'s lazy LUT
   bake) — held un-collapsed until an observation crosses a link, then collapsed by a **deterministic
   keyed rule** (counter-based RNG, ADR-0016) so the measurement is replayable. Clause 7 is this
   clause's special case: the kick's texture gate and `Promoted<T>` are measurement operators.
   Conserving flux links (ADR-0021) make collapses *correlated* — a promoted individual debits the flow
   it leaves; entanglement-shaped bookkeeping is what keeps distributed truth conserved. **The analogy's
   limit, stated so it cannot be cargo-culted:** these are classical distributions — no phase, no
   interference, no quantum speedup is claimed. The harvested wins are *compression* (store the
   distribution, not the members) and *deferred choice* (collapse late, at the link, with the observer's
   full context). Designers gain a third option beyond "store the value" and "compute on demand":
   **store the amplitude.**
9. **Alphabets are a design axis; the bit is the degenerate case.** A cell's state space and a link's
   vocabulary are *alphabets* to be sized deliberately — `TileState` is a five-letter alphabet, a tile's
   identity is a *word* over the child-index alphabet (the incremental path key), and the availability
   bitstreams are the *grammar* deciding which words are in the tree's language. Radix economy is the
   sizing test: a larger alphabet shortens words (shallower hierarchies, fewer hops, fewer link
   crossings) at a higher per-letter cost — so every branching factor (quadtree 4 / octree 8), band
   count, and tier count is a radix **choice to argue at design time**, not a default to inherit. When a
   boolean appears in a design, ask what alphabet it is the stunted form of.
   **Radix is not abstraction is not simplicity** (owner-prompted sharpening, 2026-06-12): three axes,
   never to be conflated. *Radix* is meaning-preserving encoding (binary vs octal carry identical
   information; base 2 in silicon is physical radix economy, not fundamentality). *Abstraction* is a
   semantic quotient — each rung's sentence denotes an equivalence class of realizations one rung down
   (collapsed distinctions that don't matter) while adding invariants unstatable below; a rung's sentence
   is therefore a clause-8 superposition over the rung beneath, and **the toolchain is its deterministic
   collapse** (the compiler as measurement operator, optimization as choosing the cheapest class member,
   the superscalar still collapsing at run time). *Simplicity* is not a property of the artifact but a
   RELATION between a rung and its observer — each rung must be the simplest language for ITS observer
   (the 32-byte vertex for the GPU, `SelectedTile` for the scene, the sidecar for the human), with the
   relocated complexity living in the link between them (the bi-tree rule's meeting point). Rungs exist
   because observers' working sets are bounded: a tower of languages is Simulation-LOD for meaning —
   ADR-0015's budget law applied to cognition. The C ABI is the house's worked example: a deliberately
   designed language between two towers, whose entry points are an alphabet, whose POD structs are a
   grammar, and whose version bumps are language reforms.

## Consequences

**Positive**

- **The bug habitat shrinks.** Multi-field invariants live inside one membrane with the fields they
  govern — the precise failure mode of the `GeoStreaming` bag (three shipped bugs) becomes a compile-time
  borrow/visibility question instead of a discipline question.
- **Seams stop widening.** The telemetry case quantifies it: per-counter accessors cost a new entry
  point + three mirror edits + an ABI bump *per counter*; the block costs one field per mirror. Narrow
  links keep marginal cost flat as the system grows.
- **Missing architecture is findable before it bites.** The ladder turned "weird settled-shot fog" into
  "two cells, no link" — and independently re-derived two ADRs that already existed. The design test is
  cheap and runs on paper.
- **The chain is consistent.** ADR-0005/0015/0016/0018 become instances of one named law, so the next
  ADR in their family starts from the shape instead of rediscovering it.

**Negative / costs**

- **More named types per design** — each cell is a struct/module with docs and (sometimes) drift-checked
  sets. Mitigated by the existing debug-assert-vs-full-scan discipline (`LiveSet`/`ResidentSet`), and by
  the rule that **a cell must earn its membrane**: YAGNI still binds; gratuitous hierarchy and ceremony
  are violations of §1A, not compliance with this ADR.
- **The ladder costs design time.** Scale it to the work: a milestone or new subsystem runs the full
  climb; a mechanical edit inside one cell does not. The gate is the *design test* (name cell, container,
  links), which is one paragraph.
- **A named precedent can be cargo-culted.** The law is *bounded cells + narrow links*, not "make
  everything nested." When bold and simple disagree, the tie-breaker remains the North Star.

## Alternatives considered

- **Leave it in `CLAUDE.md` only.** Rejected: `CLAUDE.md` is an agent-operational file that explicitly
  does not override the constitution, yet the precedent claims governing force — the mismatch would make
  the precedent unenforceable exactly when contested.
- **Rewrite `AGENTS.md` §1 instead.** Rejected: ADRs are the constitution's stated refinement mechanism;
  §1's hard law (authority + boundary) is untouched by this precedent, which governs *shape*, not
  *ownership*. `AGENTS.md` gains only an index entry.
- **Mechanical enforcement (lint/CI).** Rejected for now: the design test is a judgment about abstractions,
  not a syntactic property. Revisit only if violations recur in review.

## References

- The decree + operational mirror: `CLAUDE.md` § "Cells within cells, interlinked — *precedent*" (the
  three clauses + the ladder + the design test).
- First-enforcement code: `Rust/crates/geo/logi_3dtiles_scene/src/geo_world.rs` (the five cells),
  `Rust/crates/geo/logi_3dtiles_scene/src/stream_thread.rs` (`GeoTelemetry`),
  `Rust/crates/geo/logi_geo_ffi/src/ffi.rs` + `include/logi_geo_ffi.h` (`LogiGlobeTelemetry`, ABI v16),
  `Plugins/LogiBevy/Source/LogiBevyRuntime/Private/LogiTileRenderShared.cpp` (perspective-pinned stream
  view).
- Instances named in *Generalizes*; containers/links named in *Informs* (ADR-0014's arbiter, ADR-0007's
  promote-to-coarse link).
