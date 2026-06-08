# ADR-0001 — Mass-First Unreal

- **Status:** **Superseded in part by [ADR-0005](ADR-0005-Bevy-ECS-Authority.md)**
- **Date:** 2026-06-01
- **Deciders:** Vast architecture
- **Supersedes:** the prior Unity/DOTS data-oriented architecture (intent preserved, implementation discarded)

> ⚠️ **Superseded in part by [ADR-0005 — Bevy ECS Authority](ADR-0005-Bevy-ECS-Authority.md).**
> The decision that *Mass* is the runtime authority is **withdrawn** — gameplay truth now lives in a Rust
> `bevy_ecs` `World`, and Unreal is the renderer/host. What **carries forward unchanged**: the
> data-oriented intent, the **§ Determinism** contract below, and the principle that Unreal
> Actors/UObjects/Blueprints are *shells* that never own truth. Read the Mass-specific text below as
> historical record: "Fragment → Component, Processor → System, Trait → Bundle, command buffer → Commands."

## Context

Vast must simulate **millions of logistics agents** at near-Earth scale while rendering a small,
moving window of them at full fidelity ("real where you look, faked where you don't"). The prior
project established the necessary shape: **data-oriented first, deterministic fixed-step simulation,
simulation LOD, aggregate modeling**.

Unreal Engine 5.8 ships **Mass** — an ECS-style, data-oriented gameplay framework — as the native
answer to exactly this problem. Mass stores entities as compositions of **Fragments** grouped into
**Archetypes** (identical compositions stored contiguously in memory chunks), and runs stateless
**Processors** over **EntityQueries**. Structural changes are deferred through a **command buffer**
so entities are never moved between archetypes mid-iteration.

The classic Unreal `AActor`/`UObject`/component/Blueprint model does not scale to millions of
instances: per-object overhead, virtual dispatch, scattered memory, and tick cost make it unviable as
the *authority* for simulation state. It remains excellent for *presentation* and *engine integration*.

## Decision

> ⚠️ **Withdrawn by [ADR-0005](ADR-0005-Bevy-ECS-Authority.md).** The authority is now Rust `bevy_ecs`,
> not Mass. The text below is retained for history; the data-oriented intent it expresses is preserved.

**Mass is the single authority for runtime gameplay state.** Specifically:

- **Delivery state, agent state, route state, economy state, and the runtime view of terrain truth
  live in Mass Fragments** and are mutated only by Mass Processors.
- **Logic lives in Processors**, selected via EntityQueries, kept stateless and order-independent.
- **Capability is assembled by Traits**, not by Actor/Component inheritance.
- **All structural changes go through `FMassCommandBuffer`.**
- `AActor`/`UObject`/components/Blueprints/widgets are **shells** — presentation, authoring, asset
  hosting, or engine integration. They **mirror** Fragment state; they never originate it.

### Allowed Actor zones (exhaustive)

Actors are permitted **only** in the following roles. In every case the Actor is a pooled shell that
reads Fragment state for rendering/integration and writes nothing back that constitutes authority.

1. **High-resolution representation actors** — skeletal-mesh hosts for couriers/vehicles/characters
   when their significance is **High**. Pooled and recycled via the Mass representation stack; despawned
   to ISM / no-representation as significance drops.
2. **Player pawn & camera rig** — the local player's controlled pawn, camera, spring arms, and view
   logic. Reads simulation state; issues *intent*, not authority.
3. **Terrain host actors** — Landscape / Nanite / ISM proxy actors and World Partition collision hosts
   that **render** terrain whose truth is owned by Rust (see
   [ADR-0002](ADR-0002-Rust-Terrain-Kernel.md)).
4. **UI / HUD** — UMG widgets and their owning actors. Display and input capture only.
5. **Audio** — audio emitter/listener actors and components driven by Fragment state.
6. **Editor / authoring actors** — PCG actors, spawn-configuration actors, volumes, and other
   build-time tooling that produces data consumed by Mass at runtime.
7. **Debug / visualization actors** — gizmos, overlays, and pin markers used for inspection. Never
   shipped as gameplay authority.

Anything not on this list that wants to be an Actor is a design smell — re-express it as Fragments +
Processors, or open a new ADR.

### Determinism

Simulation that participates in **replay or snapshot equality** must be deterministic:

- Runs on a **fixed simulation step**, decoupled from frame rate. No `DeltaTime`-dependent results in
  the deterministic core.
- **Deterministic iteration order** — do not rely on archetype/chunk ordering that can vary between
  runs; sort by stable keys (e.g. entity-stable IDs, deterministic tile identity) where order matters.
- **Seeded randomness only** — `FRandomStream` seeded from deterministic inputs; never `FMath::Rand`,
  `rand()`, or wall-clock seeds.
- **No nondeterministic parallelism** in the deterministic core — parallel Processors must produce
  identical results regardless of scheduling (commutative writes, or partitioned/owned writes).
- Cross-boundary calls into the Rust terrain kernel must be deterministic in their tile identity and
  selection outputs (see [ADR-0002](ADR-0002-Rust-Terrain-Kernel.md)).

## Consequences

**Positive**
- Scales to millions of agents with cache-friendly, data-oriented iteration.
- Clean separation: simulation truth (Mass) vs. presentation (Actors) vs. terrain truth (Rust).
- Directly leverages Mass LOD / representation (see [ADR-0003](ADR-0003-Representation-LOD.md)).
- Deterministic core enables replay, snapshotting, and reproducible debugging.

**Negative / costs**
- Higher up-front discipline; contributors must learn Mass and resist the Actor reflex.
- Some engine systems (animation, physics, audio) are Actor-centric and require explicit bridging
  through pooled representation actors.
- Debugging data-oriented code needs dedicated tooling (MassInsights, custom debug views).

## Alternatives considered

- **Actor-per-agent.** Rejected: does not scale past tens of thousands; violates the North Star.
- **Hand-rolled ECS.** Rejected: reinvents Mass, loses MassGameplay's representation/LOD stack and
  engine integration, and creates a maintenance burden at odds with [ADR-0004](ADR-0004-No-Compatibility-Code.md).
- **Preserve the Unity/DOTS layer.** Rejected outright — see
  [ADR-0004](ADR-0004-No-Compatibility-Code.md). We translate intent, not API shape.

## References

- Unreal Engine MassEntity documentation (Fragments, Archetypes, Processors, EntityQuery, command buffer).
- Vast [`AGENTS.md`](../../AGENTS.md) §§ 1–3.
