# ADR-0005 — Bevy ECS as the Gameplay Authority

> **2026-06-05 — renamed to the Geo engine.** The terrain / real-world-earth stack is now `Rust/crates/geo/` (`logi_geo_*` plus the 3D-Tiles / geospatial / semantic crates), cdylib `logi_geo.dll`, C++ `ULogiBakedGeoSubsystem`. The `logi_terrain*` / `LogiTerrain*` / `logi_terrain.dll` names below are historical (pre-rename); see [AGENTS.md](../../AGENTS.md) §4.

- **Status:** Accepted
- **Date:** 2026-06-01
- **Deciders:** LogiUE architecture
- **Supersedes:** the **Mass-authority decision of [ADR-0001](ADR-0001-Mass-First-Unreal.md)**. The
  data-oriented *intent* is preserved; the Mass *implementation* of runtime authority is withdrawn.
- **Generalizes:** [ADR-0002](ADR-0002-Rust-Terrain-Kernel.md) — the Rust-owns-truth / narrow-C-ABI
  principle, previously scoped to terrain, becomes the project-wide rule.

## Context

ADR-0001 made **Mass** the single runtime authority and explicitly rejected a "hand-rolled ECS." Two
things overturn the Mass half of that decision:

1. **The authority must be portable, deterministic, and engine-independent.** Mass is C++ welded to
   Unreal's build, types, and lifecycle. It cannot run, test, snapshot, or replay **outside the editor**,
   and it cannot be the reproducible truth the North Star (millions of agents, replay equality) needs.
2. **[ADR-0002](ADR-0002-Rust-Terrain-Kernel.md) already accepted a Rust kernel as terrain truth behind
   a narrow C ABI.** That contract — *Rust owns truth, Unreal renders it, Unreal calls Rust, Rust never
   calls Unreal* — is correct and should be **general**, not a terrain special case.

**`bevy_ecs` 0.18.1** is a mature, widely-used, **standalone** ECS that runs as a plain Rust library,
independent of the full Bevy engine. It provides archetypal **and** sparse-set storage, schedules,
systems, queries, change detection, events, and command buffers — everything Mass offered as authority —
but in deterministic, portable, headless-testable Rust. Adopting it is therefore **not** the
"hand-rolled ECS" ADR-0001 rejected; it is adopting a **proven external ECS**, fully consistent with
[ADR-0004](ADR-0004-No-Compatibility-Code.md)'s "use current mature tech, write no compatibility scaffolding."

## Decision — the new law

> **Rust Bevy ECS owns runtime gameplay truth. Unreal owns presentation, animation, rendering, asset
> hosting, editor tooling, input capture, and engine integration.**
>
> **No `UObject`, `AActor`, Blueprint, Mass fragment, or Unreal component owns authoritative gameplay
> state.** They mirror Rust-owned state for rendering and capture player intent; they never originate truth.

The engine boundary:

```
Unreal Engine 5.8
  ├─ Rendering: Nanite, ISM/HISM, skeletal meshes, materials, animation, VFX
  ├─ World/Editor: assets, maps, World Partition, authoring tools
  ├─ Input capture: player/controller/UI intent only
  ├─ C++ bridge plugin (Plugins/LogiBevy): narrow, boring, heavily profiled
  │
  ▼  C ABI boundary — plain-data only, NO Unreal types cross, Rust never calls Unreal
Rust native library (Rust/)
  ├─ logi_runtime         owns the Bevy World, Schedules, Resources
  ├─ logi_sim             gameplay Components + Systems
  ├─ logi_render_extract  per-frame snapshots for Unreal (batched, not per-entity)
  ├─ logi_schema          spawn recipes, asset handles, archetype recipes
  ├─ logi_terrain         terrain authority (ADR-0002), merged/called as a module
  └─ logi_ffi             opaque handles, versioned ABI, buffers, error reporting
```

**`bevy_ecs` is used as a library.** We depend on `bevy_ecs` (plus minimal companions such as
`bevy_app`/`bevy_tasks` only where genuinely needed) and **never** on `bevy_render`, `bevy_window`,
`wgpu`, `bevy_audio`, or the umbrella `bevy` crate. Unreal renders; Bevy only simulates.

### Vocabulary (Bevy, not Mass)

| Bevy term | Role | (Withdrawn Mass analogue) |
|---|---|---|
| **Component** | Atomic data on an entity | Fragment |
| **Resource** | Global/singleton state in the World | Subsystem/shared fragment |
| **Entity** | An ID naming a set of Components | Entity |
| **Bundle** | A set of Components spawned together | Trait/Archetype recipe |
| **Schedule** | An ordered set of Systems run per step | Processing phase |
| **System** | A function that reads/writes via Queries | Processor |
| **Query** | Declares the Components a System touches | EntityQuery |
| **Command** | Deferred structural change (spawn/insert/remove) | Command buffer op |
| **Event / Message** | Decoupled inter-system signal | Signal |
| **Archetype / Table storage** | Dense storage for stable compositions | Archetype/chunk |
| **SparseSet storage** | Storage for churny add/remove components | (n/a) |

No Mass or Unity vocabulary in gameplay code. The only place those terms may appear is migration notes.

### Preserved intent (unchanged)

- **Millions of agents.**
- **Deterministic, fixed-step simulation** for replay/snapshot equality — now the Bevy schedule.
- **Representation LOD** and **"real where you look, faked where you don't."**
- **Aggregate modeling** where unobserved.

[ADR-0001 § Determinism](ADR-0001-Mass-First-Unreal.md) and the
[ADR-0003](ADR-0003-Representation-LOD.md) significance-budget model **remain in force** — only the
*owner* changes: the deterministic core is now Bevy/Rust; the representation layer is Unreal.

### Allowed Unreal zones (replaces ADR-0001's "Allowed Actor zones")

Unreal Actors/objects are permitted only as **shells** that mirror Rust-owned state and/or capture
intent: (1) high-res representation actors + ISM/HISM, (2) player pawn & camera rig (issues intent),
(3) ~~terrain host actors~~ **— SUPERSEDED by [ADR-0007](ADR-0007-Actorless-Rust-Terrain.md): terrain
uses NO `AActor`, only renderer-facing component/proxy shells**, (4) UI/HUD, (5) audio,
(6) editor/authoring actors, (7) debug/visualization. None of them own gameplay truth.

### Disposition of Mass

- **Mass is removed from the gameplay path.** No gameplay state or logic in Mass.
- The Turn-2 `Source/LogiMass` module (Mass fragments for agents/shipments) is **withdrawn** — it was
  the literal embodiment of the superseded pattern.
- The Mass plugins (`MassGameplay`, `MassAI`) may remain enabled **only** as a transitional, explicitly
  **non-authoritative** convenience (e.g. a debug/migration bridge), and must be removed once nothing
  non-authoritative needs them. They must **never** own gameplay truth.
- `Source/LogiCore` and `Source/LogiTerrain` (Turn-2 C++) are superseded by the Rust workspace; their
  concepts move to Rust crates and their bridge role moves to `Plugins/LogiBevy`. Pending removal/absorption.

## Consequences

**Positive**
- Authority is **portable, deterministic, headless-testable** — the sim runs in CI and replay without the editor.
- **One truth language (Rust)** across simulation and terrain; the ADR-0002 seam generalizes cleanly.
- Sim logic is **decoupled from the Unreal release cadence**.
- A narrow, profile-able C ABI is a single, auditable boundary.

**Negative / costs**
- A **C ABI to design, version, and profile**; the FFI crossing must be **batched** (snapshot extract,
  not per-entity calls). That batching cost is the concrete performance reason to occasionally keep a
  piece of logic Unreal-side.
- **Two toolchains** (cargo + UnrealBuildTool) and their packaging/link friction.
- The team must learn `bevy_ecs`.

## Alternatives considered

- **Keep Mass as authority (ADR-0001).** Superseded: not portable/deterministic outside Unreal; cannot
  run headless; welds truth to the engine.
- **Full Bevy engine.** Rejected: we want only the ECS. `bevy_render`/`bevy_window`/`wgpu`/`bevy_audio`
  are unwanted — Unreal renders. Use `bevy_ecs` standalone.
- **Hand-rolled ECS.** Rejected (still): `bevy_ecs` is mature and current; adopting it satisfies
  [ADR-0004](ADR-0004-No-Compatibility-Code.md); hand-rolling would violate it.

## References

- `bevy_ecs` 0.18.1 — standalone ECS usable outside the full Bevy engine.
- [ADR-0001](ADR-0001-Mass-First-Unreal.md) (superseded in part), [ADR-0002](ADR-0002-Rust-Terrain-Kernel.md)
  (generalized), [ADR-0003](ADR-0003-Representation-LOD.md) (significance model preserved),
  [ADR-0004](ADR-0004-No-Compatibility-Code.md) (mature-tech / no-compat).
- LogiUE [`AGENTS.md`](../../AGENTS.md) §§ 1–3.
