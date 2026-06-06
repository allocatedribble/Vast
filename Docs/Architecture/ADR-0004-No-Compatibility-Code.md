# ADR-0004 — No Compatibility Code

- **Status:** Accepted
- **Date:** 2026-06-01
- **Deciders:** LogiUE architecture
- **Related:** [ADR-0001](ADR-0001-Mass-First-Unreal.md), [ADR-0005](ADR-0005-Bevy-ECS-Authority.md)

> **Reinforced by [ADR-0005](ADR-0005-Bevy-ECS-Authority.md).** The same rule now applies to Rust: depend
> on **`bevy_ecs` as a library** at its current version — never the full `bevy` engine, never a *direct*
> `bevy_render`/`bevy_window`/`bevy_audio`/`winit`/`wgpu` dependency (no second renderer; Unreal renders).
> **Clarification (2026-06-05):** type-only transitive crates that `bevy_ecs`'s default features pull in —
> e.g. `bevy_reflect` → `wgpu-types` (pure type definitions, no GPU/renderer/runtime) — are **allowed**.
> The bar is "no renderer/GPU/windowing/audio *runtime*", not the crate name. No compatibility shims across
> engine or crate versions; migrate forward and delete.

## Context

LogiUE descends from a Unity/DOTS project and runs on a single, current engine: **Unreal Engine 5.8**.
There is a strong temptation, when migrating, to (a) preserve old DOTS type names "for continuity," and
(b) keep fallback paths for older engine versions or alternate renderers "just in case." Both are traps.
A compatibility layer ossifies the worst of both worlds: it imports Unity's mental model into an engine
that does not share it, and it taxes **every** future change with branches no one exercises. At planetary
scale, that tax compounds.

## Decision

**Target Unreal 5.8 only. Write no compatibility code.**

### Banned
- **No Unity compatibility layer.** Do not preserve DOTS type names for nostalgia. Do not create
  `Unity*`, `GameObject*`, `MonoBehaviour*`, `SubScene*`, or `Burst*` conceptual wrappers. Translate
  **intent**, not API shape.
- **No backward-compatibility branches.** No UE 5.3 / 5.4 / 5.5 / 5.6 / 5.7 fallbacks.
- **No "legacy renderer" paths.** Lumen + Virtual Shadow Maps + Nanite are the default high-end posture
  (profiled per view); do not keep a non-Lumen/baked-lighting fallback path in the codebase.
- **No old input fallback.** Enhanced Input only.
- **No dual Unity/Unreal data path.** One data model — Mass Fragments (see
  [ADR-0001](ADR-0001-Mass-First-Unreal.md)). There is no parallel Unity-shaped data path.
- **No Unity vocabulary** in runtime code. Permitted only inside clearly-marked *migration notes* that
  translate an old concept to its Mass equivalent.

### Do instead
- **Translate intent.** Ask "what did the old system *accomplish*?" and build the UE-5.8-native, Mass-first
  way to accomplish it.
- **Target 5.8 APIs directly.** Use the current API; when it changes in a future engine bump, migrate
  forward and delete the old call — do not branch.
- **Delete dead paths.** If a code path exists only to support a version, platform, or engine we do not
  ship, remove it.
- **PCG stays in its lane.** PCG is for **editor/world-building** generation and may consume Rust/Mass
  outputs as content seeds; it is **not** authoritative runtime logistics simulation.

## Consequences

**Positive**
- Smaller, sharper codebase; every line targets the engine we actually ship.
- No untested fallback branches silently rotting.
- Contributors learn the Mass-first model instead of a Unity-flavored veneer over it.

**Negative / costs**
- A future engine upgrade is a **migration**, not a flag flip — we pay that cost when it comes, on
  purpose, instead of carrying compatibility scaffolding continuously.
- Developers with Unity muscle memory must re-learn idioms rather than reach for a familiar wrapper.

## Enforcement

- Any genuine, unavoidable hybrid exception is a **new ADR**, reviewed and accepted — never a quiet
  branch or a `#if` smuggled into a diff.
- Reviewers should reject PRs that introduce Unity vocabulary in runtime code, version-fallback branches,
  or a parallel data path.

## References

- LogiUE [`AGENTS.md`](../../AGENTS.md) § 3 rules 9–10, 12.
