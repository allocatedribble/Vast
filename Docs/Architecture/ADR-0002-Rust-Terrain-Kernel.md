# ADR-0002 — Rust Terrain Kernel

> **2026-06-05 — renamed to the Geo engine.** The terrain / real-world-earth stack is now `Rust/crates/geo/` (`logi_geo_*` plus the 3D-Tiles / geospatial / semantic crates), cdylib `logi_geo.dll`, C++ `ULogiBakedGeoSubsystem`. The `logi_terrain*` / `LogiTerrain*` / `logi_terrain.dll` names below are historical (pre-rename); see [AGENTS.md](../../AGENTS.md) §4.

- **Status:** Accepted
- **Date:** 2026-06-01
- **Deciders:** Vast architecture
- **Related:** [ADR-0005](ADR-0005-Bevy-ECS-Authority.md) (generalizes this ADR), [ADR-0001](ADR-0001-Mass-First-Unreal.md), [ADR-0003](ADR-0003-Representation-LOD.md)

> **Generalized by [ADR-0005](ADR-0005-Bevy-ECS-Authority.md).** This ADR's principle — Rust owns truth
> behind a narrow C ABI, Unreal renders, Rust never calls Unreal — is now the project-wide law, not a
> terrain special case. Terrain authority lives in the `logi_terrain` crate inside the Rust workspace.

## Context

Vast renders a planet. Terrain at near-Earth scale must be **deterministic** (the same tile must
resolve to the same identity and source data on every machine and every run, to support replay and
distributed/streamed play), **portable** (terrain truth must not be hostage to Unreal's asset or
streaming lifecycle), and **fast to query** at simulation rates independent of rendering.

The prior engineering standard already named a **Rust terrain kernel boundary**. We keep it. Rust is a
deterministic, memory-safe, no-GC native language well suited to owning the authoritative terrain model
as a library that Unreal links against — not as gameplay logic, but as the *source of truth* for the
planet's surface.

## Decision

**Rust owns terrain truth. Unreal owns terrain presentation.** The boundary is a narrow, well-typed
FFI; data crosses it, types do not.

### Rust owns
- **Planet terrain selection** — which terrain exists where, at planetary scale.
- **Deterministic tile identity** — stable, reproducible IDs for tiles; identical inputs → identical IDs
  on every platform.
- **Terrain source data** — the authoritative heightfield / material / climate / classification source
  from which Unreal derives renderable assets.
- **Tile streaming *decisions*** — what to load, unload, and prioritize, as a function of streaming
  sources and the significance model. Rust decides *what*; Unreal executes *how*.

### Unreal owns
- **Rendering** — Nanite Landscape, Runtime Virtual Texturing material compositing, Virtual Shadow
  Maps. (Note: Nanite landscapes still stream non-Nanite landscape data too, required by RVT, water,
  and related systems.)
- **World Partition integration** — mapping Rust tile decisions onto World Partition streaming cells
  and HLOD proxies.
- **Collision** — runtime collision components derived from Rust source data.
- **Material presentation** — landscape/material graphs, decals, splines conforming to terrain.

### The boundary
- A **narrow C-ABI FFI**: plain-data structs in, plain-data structs/handles out. No Unreal types cross
  into Rust; **Rust never calls Unreal**. Unreal calls Rust for tile identity, selection, source data,
  and streaming decisions, then renders the result.
- **Deterministic outputs.** Tile identity and selection are pure functions of their inputs (seed,
  coordinates, parameters). This is part of the determinism contract in
  [ADR-0001 § Determinism](ADR-0001-Mass-First-Unreal.md).
- **Ownership & lifetime** are explicit across the boundary (who allocates, who frees); no shared
  mutable aliasing. Buffers handed to Unreal are either copied or lifetime-scoped by handle.
- The **Mass runtime view of terrain** (the fragment-side data simulation reads) is *derived from* Rust
  source data; Rust remains the truth, Mass holds a cache/projection for the simulation step.

## Consequences

**Positive**
- Terrain truth is deterministic and engine-independent; replay and reproducibility hold across machines.
- Clear separation lets the rendering stack evolve (Nanite/RVT/VSM) without touching terrain truth.
- Rust's safety/perf characteristics suit a hot, deterministic, GC-free kernel.

**Negative / costs**
- An FFI boundary to design, version, and test; marshalling has a cost that must be measured.
- Build complexity: a Rust toolchain integrated into the Unreal build, with a UE integration module.
- Two languages to reason about at the seam; the contract must be documented and kept narrow.

## Alternatives considered

- **Pure-Unreal Landscape as truth.** Rejected: ties terrain truth to Unreal's asset/streaming
  lifecycle, weakens determinism, and couples truth to renderer changes.
- **Terrain truth in Mass/C++.** Rejected: loses the deterministic, portable, reusable native kernel and
  conflates simulation with the planet model.
- **PCG as authoritative runtime terrain.** Rejected: PCG is an **editor/world-building** tool and may
  consume Rust/Mass outputs as seeds, but it is **not** authoritative runtime logistics or terrain truth
  (see [ADR-0004](ADR-0004-No-Compatibility-Code.md) and `AGENTS.md`).

## References

- Vast [`AGENTS.md`](../../AGENTS.md) § 3 rule 6.
- Unreal: World Partition & HLOD, Nanite Landscape, Runtime Virtual Texturing, Virtual Shadow Maps.
