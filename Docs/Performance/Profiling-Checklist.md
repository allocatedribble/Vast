# Profiling Checklist

How to measure each budget in [Budgets.md](Budgets.md), and what must hold before a change merges.
**Profile before you optimize, and profile per world view** — orbital and street-level have entirely
different cost shapes ([ADR-0003](../Architecture/ADR-0003-Representation-LOD.md)).

> **Parked Geo optimizations:** three Geo perf items are deliberately *not* implemented because they're
> constant-factor wins on paths no profile has flagged — see
> [Deferred Optimizations](../Audit/2026-06-06-Geo-Audit-Deferred-Optimizations.md). Each names the exact
> flamegraph signal that would justify doing it. If your capture here hits one of those signals, that doc is
> where to start (and remove the entry once landed with a backing benchmark).

> **Updated by [ADR-0005](../Architecture/ADR-0005-Bevy-ECS-Authority.md):** the **simulation** is profiled
> in Rust — `cargo` benches, Tracy / `puffin`, and deterministic `cargo test` for replay equality (headless,
> no editor). **MassInsights is retired for the sim.** Unreal Insights + `stat` commands now cover the
> **C++ bridge, the FFI snapshot crossing, and rendering** only. Where "Mass" appears below, read "the Bevy
> sim (Rust)" for simulation rows and "the LogiBevy bridge" for the Unreal-side cost.

## Tools

- **Unreal Insights** — primary timeline profiler (game/render/GPU threads, frame breakdown).
- **MassInsights** — Mass-specific tracing (entities, processors, archetypes). *Confirmed mounted in
  this project* — use it for all Mass profiling.
- **`stat` commands** in the editor/PIE console (the MCP `console_command` action can drive these):
  - `stat unit` — frame, game, render, GPU at a glance.
  - `stat game` — game-thread breakdown.
  - `stat gpu` — GPU pass costs (Lumen, VSM, Nanite, RVT).
  - `stat scenerendering` — draw calls, primitives.
  - `stat animation` / Animation Budget Allocator stats — skeletal-mesh tick cost.
  - `stat streaming` — texture/asset streaming.
- **`mass.*` / `ai.debug.*` console commands** — Mass LOD and representation debugging.
- **`stat startfile` / `stat stopfile`** or Insights `.utrace` captures — keep captures that back any
  number written into [Budgets.md](Budgets.md).

## Per-budget measurement map

| Budget row | How to measure |
|---|---|
| Frame / thread split (§1) | `stat unit` + Insights timeline at the target entity count and view. |
| Mass sim-step time (§2) | MassInsights processor timings; isolate the fixed-step tick. |
| High/Medium sim-LOD counts (§2) | Mass LOD debug views; confirm caps are enforced, not exceeded. |
| Fragment memory / entity (§2) | Memory report + archetype/fragment sizes. |
| Representation tier counts (§3) | Mass representation debug; count actors vs. ISM vs. none. |
| Animation CPU (§4) | Animation Budget Allocator stats; verify it holds the cap under load. |
| MetaHuman LOD costs (§4) | Official spec for the table; Insights to confirm in-engine cost. |
| Lumen/VSM/Nanite/RVT (§5) | `stat gpu` per view; capture orbital **and** street separately. |
| World Partition streaming (§5) | `stat streaming`, World Partition cell visualization. |
| Dynamic vs. static draws (§6) | `stat scenerendering`; flag per-frame dynamic mesh rebuilds. |

## Determinism checks (when replay/snapshot equality is in scope)

- Run the same seeded scenario twice; assert **snapshot equality** of the simulation state.
- Vary frame rate (cap to 30/60/120) and confirm the deterministic core produces identical results —
  any divergence means frame-rate-dependent math leaked into the sim
  ([ADR-0001 § Determinism](../Architecture/ADR-0001-Mass-First-Unreal.md)).
- Confirm no `FMath::Rand`/`rand()`/wall-clock seeds in the deterministic core; only seeded
  `FRandomStream`.

## Pre-merge checklist

Before merging a change that touches runtime simulation or representation:

- [ ] **Budgets hold** at the target entity count and view (frame, Mass sim, representation, animation).
- [ ] **No new `Error:`/`Fatal`** lines in `Saved/Logs/LogiUE.log`; no new warnings you can't justify.
- [ ] **Structural changes go through `FMassCommandBuffer`** — no direct add/remove of Fragments or
      create/destroy of entities during processing.
- [ ] **No new gameplay authority on Actors/components/Blueprints** — truth stayed in Fragments
      ([ADR-0001](../Architecture/ADR-0001-Mass-First-Unreal.md)).
- [ ] **No per-frame dynamic mesh rebuilds** introduced; static/retained draw paths preferred
      ([Budgets §6](Budgets.md)).
- [ ] **Significance, not distance** — any new LOD decision feeds from the unified score, allocated
      against a budget ([ADR-0003](../Architecture/ADR-0003-Representation-LOD.md)).
- [ ] **Determinism preserved** if the change is in the deterministic core (re-run the equality check).
- [ ] **No Unity vocabulary / no version-fallback branches** introduced
      ([ADR-0004](../Architecture/ADR-0004-No-Compatibility-Code.md)).
- [ ] A **profile capture** backs any new number written into [Budgets.md](Budgets.md).
