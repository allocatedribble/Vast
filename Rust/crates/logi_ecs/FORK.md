# logi_ecs — fork ledger

The Vast ECS. Adopted fork of **`bevy_ecs` 0.19.0-rc.2** (crates.io, MIT OR Apache-2.0 — license
files retained), decreed 2026-06-11 and constitutionally homed in
[ADR-0020](../../../Docs/Architecture/ADR-0020-LogiEcs-Incremental-World-Cell.md). The sibling crate
`logi_ecs_macros` is the matching fork of `bevy_ecs_macros` 0.19.0-rc.2.

**Direction:** the World is refactored into an **incremental, interlinked cell** (ADR-0019 applied to
the ECS itself): bounded by budget, interior updated O(Δ) — never by flat sweeps — and coupled to
sibling Worlds and to the renderer only through narrow, explicit links.

## Naming

- **Package** (Cargo identity): `logi_ecs` / `logi_ecs_macros`.
- **Crate symbol** (`[lib] name`): **still `bevy_ecs`** — rustc ships a compatibility hack for
  bevy's `ParamSet` implied-bounds pattern *keyed on the crate name*; under any other name every
  `ParamSet<…>` system signature fails E0477 (verified empirically 2026-06-11: same sources, same
  toolchain, only the crate name flipped). Divergence **D-paramset** below removes the reliance;
  the symbol rename to `logi_ecs` lands with it.
- **Dependency key**: consumers depend on it as `bevy_ecs = { package = "logi_ecs", path = … }`
  (workspace `Cargo.toml`), so consumer source (`use bevy_ecs::…`) and the derive macros' manifest
  path-resolution are untouched.

## Upstream tracking

Pinned at the fork point. There is no upstream merge cadence — divergences are deliberate and this
file is their ledger (the `cyper` / `logi_draco` vendoring precedent, taken further: this crate is
*ours* now). If an upstream fix is wanted, it is cherry-picked and recorded here.

## Divergences

| id | date | change | why |
|---|---|---|---|
| D-identity | 2026-06-11 | Package renamed `bevy_ecs` → `logi_ecs` (+ macros crate); examples/ dropped; registry metadata removed. | Fork adoption (ADR-0020). |
| D-place-clamp | 2026-06-11 | `relationship_source_collection.rs`: `SmallVec::place_most_recent` clamps to the post-pop `len()` (was `len()-1` — underflow-panic on a 1-element collection, off-by-one at the end otherwise); `Vec::place` clamps to `len()-1` (was `len()` — insert past the post-remove end panics). Both impls now share the trait-documented "out of bounds ⇒ end" semantics; regression test `ordered_place_clamps_to_end_without_panicking`. | Real upstream bug found by the 2026-06-06 audit; the sibling-sweep found the `Vec::place` boundary panic. |
| D-incremental | 2026-06-11 | New `incremental` module: `BucketIndex`, the O(K+Δ) bucketed top-K selector (first primitive of the incremental-World program), with the boundary-bucket partial-select (the spike's clone+full-sort of the boundary bucket was O(P log P) when the descent reached the population-holding bottom bucket). Moved up from `logi_sim::sig_index` (Exp D). 2026-06-12: + `ensure_slots` / `remove` / `occupied` (lazy slot growth, O(1) event-driven eviction, O(1) occupancy for the consumer's drift check) — the API the R1 consumer wiring needed. | ADR-0015 §3 / ADR-0020 §R1. |

## Queued (next divergences)

- **D-paramset** — make `ParamSet`/`SystemParam` impls well-formed under the tightened solver rules
  (no rustc grandfather clause), then flip the crate symbol name to `logi_ecs`.
- **R1 (cont.)** — feed `BucketIndex` from change-detection at write time (the in-band O(Δ) link),
  replacing the post-hoc scan that re-reads every significance every frame. (The consumer-side
  wiring LANDED 2026-06-12: `logi_sim::SignificanceIndex` +
  `allocate_representation_budget` rank against the index — quantized acceptance + determinism
  gates green; only the write-time feed remains fork-interior work.)
- **R2** — the morsel lane in-band: safe `morsel_for_each` over matched table columns
  (`logi_sim::morsel`'s proven Table-seam spike, promoted to a first-class API).
- **Shedding** — subsystems the project never uses (candidates: reflect, hierarchy/relationships,
  observers/events beyond what the towers consume) get cut once the towers' surface is pinned by
  tests; ADR-0004's no-compat law applies to the fork's interior too.
