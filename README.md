# Vast

**A near-Earth-scale planet simulation in which the game logic lives in Rust and Unreal Engine 5.8
serves as a rendering shell.**

> **Scope note, honest:** "near-Earth-scale" is the design target, not today's state. The
> planet-streaming core runs now; the at-scale simulation is in active development (see [Status](#status)).

Vast is a first- and third-person game set in a continuously streamed planet — from orbital flows down
to a single character on foot, with no loading screens between. It is built on one architectural
commitment, carried consistently throughout:

> **A Rust simulation owns every piece of gameplay truth. Unreal Engine 5.8 owns none of it.**
> Agents, routes, economy, shipments, and terrain all live in a Rust [`bevy_ecs`] `World`, mutated by
> Systems in Rust. Unreal renders, animates, hosts, and captures input. It mirrors the Rust-owned
> world; it never originates it.

Most engines place gameplay inside the engine. Vast inverts that: the simulation is a headless,
deterministic, data-oriented Rust core that builds and tests without an editor open, and the renderer
is a replaceable shell connected across a narrow C ABI. The boundary is one-directional — Unreal calls
Rust with plain-data structs; no Unreal type crosses into Rust, and Rust never calls back.

The full architectural rationale lives in [`AGENTS.md`](AGENTS.md) and the decision records under
[`Docs/Architecture/`](Docs/Architecture/). This README is a summary; neither is required to follow
the overview below.

---

## Overview

The project was originally named for its logistics layer, which is now one mode within a larger world.
The guiding objectives (detailed in [`AGENTS.md`](AGENTS.md) § 0):

- **Planetary scale, continuous zoom.** Real-world Earth streamed from orbit to street level, without
  loading screens, down to a single courier carrying a single package.
- **A million simulated agents as the floor, not the ceiling.** The entity count is unbounded by
  design. What is bounded is the *active cut* — by compute budget, never by population — in **space**
  (full fidelity only near the player's attention) and in **time** (a simulation cell's timestep is
  itself a resolution: every frame, once a day, or never — replayed from a seed when finally observed).
- **Full fidelity where observed; statistical aggregates elsewhere.** Cheap aggregates everywhere
  outside the player's attention, promoted to individuals on demand.
- **Deterministic, fixed-step simulation** wherever replay or snapshot equality is required.
  Determinism-from-seed also serves as the compression scheme that makes an unbounded world runnable:
  state that can be regenerated need not be stored.

When a design decision is genuinely in doubt, the tie-breaker is fixed: choose the option that still
works at one million agents, and never hard-caps at any population.

---

## Architecture at a glance

```
        ┌──────────────────────────────────────────────────────────────┐
        │  Unreal Engine 5.8  —  the shell                              │
        │  rendering · animation · Nanite/Lumen/VSM · input · editor    │
        │  AActors / UObjects / widgets = mirrors of Rust-owned state   │
        └───────────────▲───────────────────────────┬──────────────────┘
                        │  pull snapshot             │  push view + input
                        │  (plain-data views)        │  (plain-data structs)
        ════════════════╪═══════════ narrow C ABI ═══╪══════════════════
                        │      one-directional, zero-copy, panic-guarded
        ┌───────────────┴───────────────────────────▼──────────────────┐
        │  Rust  —  the truth                                           │
        │                                                               │
        │   logi_sim        gameplay World (logi_ecs): agents, routes,  │
        │                   economy, shipments — Systems mutate it      │
        │   geo/            the "Geo" engine: real-Earth OGC 3D Tiles    │
        │                   streaming, decode, LOD — owns terrain truth  │
        │   logi_sky        Rust-modeled atmosphere, bakes GPU LUTs     │
        │   logi_runtime    two lanes (work-stealing CPU + compio I/O)  │
        │                   under one arbiter; sim + streaming run here  │
        └───────────────────────────────────────────────────────────────┘
```

Both the gameplay simulation and the planet-streaming loop run on Rust-owned threads. Unreal, on its
game thread, only pushes the current view/input and pulls the latest retained snapshot; it never blocks
on the simulation ([ADR-0012]).

### Load-bearing decisions

These are the structural foundations rather than later additions. Each has a written decision record
under [`Docs/Architecture/`](Docs/Architecture/):

| Decision | Rationale | ADR |
|---|---|---|
| **Bevy-ECS authority** | one source of gameplay truth, headless and testable | [ADR-0005] |
| **Actorless Rust terrain** | a streamed planet with no `AActor` per tile | [ADR-0007] |
| **Native Rust 3D Tiles stack** | real-world Earth decoded and selected in Rust, not via a plugin | [ADR-0009] |
| **Poll-based FFI / decoupled sim** | the renderer never waits on the simulation | [ADR-0012] |
| **Simulation-LOD (space)** | active set bounded by budget, not population | [ADR-0015] |
| **Sim-Tiles (time)** | timestep as a resolution; unbounded entity count | [ADR-0021] |
| **Per-lane determinism** | a deterministic near-set and a free far field | [ADR-0016] |
| **Cells within cells, interlinked** | the structural precedent every abstraction follows | [ADR-0019] |
| **One truth, many formulas** | one quantity, a family of tested (cost, error) tiers | [ADR-0023] |

The full constitution is [`AGENTS.md`](AGENTS.md) (read first) and [`CLAUDE.md`](CLAUDE.md); the ADRs
refine it. New work either follows the established precedent or argues, in a new ADR, why it should not.

---

## Status

Vast is a frontier project in active development, not a finished engine. Current state:

- **Working — the Geo engine.** Real-world Earth streams and renders in-editor: Google Photorealistic
  3D Tiles fetched over HTTP/3 (QUIC), glTF / Draco / quantized-mesh decoded in Rust, rendered through
  a static-relevance proc-mesh proxy (GPU-Scene cached draws, [ADR-0013]), with BC7 texture
  compression, a two-tier (bytes + decode) cache, frustum-aware LOD selection, and LOD-crack skirts.
  Flight from orbit to rooftops is functional. A dedicated `geo-bench` harness drives and measures it.
- **In place — the boundary and the runtime.** The one-directional C ABI, the poll-based snapshot
  hand-off, the `logi_ecs` ECS fork ([ADR-0020]) with its budget/significance system, and the two-lane
  `logi_runtime` executor + reactor exist and are exercised by the Geo loop.
- **Active frontier — gameplay simulation at scale.** The architecture for a million agents
  (Simulation-LOD in space and time) is specified and the ECS substrate is built. Proving the
  deterministic, fully-decoupled agent simulation at that scale is the current work, not a finished
  claim.
- **Partial — LogiSky.** The Rust atmosphere model and its LUT bakers are built and tested; the UE GPU
  shell and its FFI lane are pending ([ADR-0022]).

In summary: the planet-streaming core is real and runnable; the at-scale gameplay simulation is the
current objective.

---

## Repository layout

```
LogiUE.uproject          Unreal Engine 5.8 project (the name "Logi" predates the rename to Vast)
AGENTS.md  CLAUDE.md      the architectural constitution — read before designing
rebuild-editor.ps1        one-shot Rust build + UE editor rebuild

Rust/                     the simulation (cargo workspace; builds & tests headless, no editor)
  crates/
    logi_ecs/             the adopted bevy_ecs fork — gameplay World (ADR-0020)
    logi_sim/             gameplay simulation systems
    logi_runtime/         unified executor + reactor (ADR-0014)
    logi_ffi/             gameplay C ABI (cbindgen-generated header, checked in)
    logi_render_extract/  snapshot/extract path for the renderer
    geo/                  the "Geo" engine — real-Earth terrain stack (ships as logi_geo.dll)
      logi_3dtiles_*/     OGC 3D Tiles: tileset, implicit tiling, selection, scene, io, Cesium ion
      logi_gltf/ logi_qmesh/ logi_draco/ logi_collada/   decoders
      logi_geo_*/         geo ffi, lod, mesh, pack, procgen
      logi_geospatial/    WGS84 geodesy (single-homed with the sky)
      logi_semantic/      offline semantic-tile enrichment (ADR-0011)
      logi_sky/           LogiSky atmosphere model + LUT bakers (ADR-0022)

Plugins/LogiBevy/         the UE plugin that links & calls the Rust artifact
  Source/LogiBevyRuntime/   runtime shell: proc-mesh tiles, snapshot pull, view push
  Source/LogiBevyEditor/    editor-only inspection shells (ADR-0006)

Tools/logi_bake/          offline asset/semantic bake pipeline
Docs/                     Architecture (ADRs) · Plans · Audit · Research · Performance · Testing
```

---

## Building

**Prerequisites**

- Unreal Engine 5.8
- Rust nightly (pinned in `rust-toolchain`; currently ~1.97 nightly), host `x86_64-pc-windows-msvc`
- Windows with PowerShell, the Visual Studio C++ toolchain, and Git LFS (for binary assets)

**Simulation only** (headless — the fast inner loop):

```powershell
cd Rust
cargo build
cargo test --workspace
cargo clippy --workspace -- -D warnings
```

**Full editor** (Rust artifacts plus Unreal):

```powershell
./rebuild-editor.ps1     # builds the Rust cdylibs, then the UE editor target
```

Open `LogiUE.uproject` in Unreal Editor 5.8 and use Play-In-Editor to fly the streamed planet.

> **Map data.** The Geo engine streams real-world terrain from Google Photorealistic 3D Tiles and
> Cesium ion, which require a user-supplied API key and are subject to their own terms of use (see
> [License](#license)). The code is open source; the imagery is licensed separately by the user.

---

## Contributing

This is an ambitious build maintained by a small effort, and several lanes are largely independent.
Contributions are welcome, particularly in these areas:

- **Rust simulation / ECS at scale** — the Simulation-LOD active set, the statistical far field, and
  the deterministic seed-promotion that carry the agent count toward a million ([ADR-0015]/[ADR-0021]).
- **Geospatial / streaming** — 3D Tiles selection and decode, cache durability, QUIC transport, the
  working-set streaming step ([ADR-0009]/[ADR-0018]).
- **Real-time rendering (UE 5.8)** — Nanite/Lumen/VSM integration for streamed proc-mesh terrain,
  static-relevance draw paths, the atmosphere GPU shell ([ADR-0013]/[ADR-0022]).
- **Determinism and tooling** — fixed-step replay, snapshot equality, headless test harnesses,
  profiling.
- **Graphics / simulation math** — geodesy, LOD error metrics, the tested (cost, error) formula tiers
  ([ADR-0023]).

Before starting, read [`AGENTS.md`](AGENTS.md) and the ADR most relevant to your lane, then open an
issue describing the work you intend to take on. One ground rule keeps a small project coherent:
architectural changes are proposed as ADRs, not introduced silently in commits.

---

## Acknowledgements

Vast is built on the work of others, with gratitude:

- **Bevy / `bevy_ecs`.** The gameplay World is a fork of [`bevy_ecs`](https://github.com/bevyengine/bevy)
  (0.19.0-rc.2) by the Bevy project and its contributors, used under MIT/Apache-2.0. The upstream
  license files are retained in-tree and every divergence is ledgered in
  [`Rust/crates/logi_ecs/FORK.md`](Rust/crates/logi_ecs/FORK.md). Vast is an independent project and is
  not affiliated with or endorsed by the Bevy project.
- **Google Photorealistic 3D Tiles.** Real-world terrain imagery is served by Google's
  [Photorealistic 3D Tiles](https://developers.google.com/maps/documentation/tile/3d-tiles) API. The
  imagery and the Google name are the property of Google; its use requires a Google Maps Platform API
  key and is subject to the Google Maps Platform Terms of Service, including the required on-screen
  attribution. Vast displays that attribution and does not redistribute the imagery.
- **Cesium ion.** The 3D Tiles pipeline interoperates with [Cesium ion](https://cesium.com/platform/cesium-ion/)
  by Cesium GS, Inc. "Cesium" and "Cesium ion" are trademarks of Cesium GS, Inc.; access requires a
  Cesium ion account and key and is governed by Cesium's Terms of Service. Vast is not affiliated with
  or endorsed by Cesium GS, Inc.
- **Draco.** Mesh decompression uses Google's [Draco](https://github.com/google/draco) library
  (Apache-2.0), vendored under
  [`Rust/crates/geo/logi_draco/vendor/draco`](Rust/crates/geo/logi_draco/vendor/draco) with its license
  retained.
- **cyper.** HTTP/3 tile streaming uses a fork of [`cyper`](https://github.com/compio-rs/cyper) (MIT);
  the patches are documented in [`Rust/third_party/cyper/PATCHES.md`](Rust/third_party/cyper/PATCHES.md).

Trademarks and product names are the property of their respective owners; references here are for
identification only and do not imply endorsement.

---

## License

Vast is open source, dual-licensed under either of:

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option. This is the standard of the Rust ecosystem and is compatible with the licenses of the
project's principal dependencies (`bevy_ecs`, `gpui`, `cyper`, Draco).

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in the
work, as defined in the Apache-2.0 license, shall be dual-licensed as above, without additional terms
or conditions.

**Third-party code.** Vendored and forked components retain their own permissive licenses in-tree: the
`bevy_ecs` fork under [`Rust/crates/logi_ecs`](Rust/crates/logi_ecs) (MIT/Apache-2.0), the `cyper` fork
under [`Rust/third_party/cyper`](Rust/third_party/cyper) (MIT), and vendored Draco (Apache-2.0).
Distributed binaries must include the corresponding notices.

**Map data is not covered by this license.** Google Photorealistic 3D Tiles and Cesium ion imagery are
streamed at runtime with a user-supplied API key and are governed by Google's and Cesium's terms of
service, including attribution and redistribution requirements. Vast does not redistribute that data;
the license above covers the engine, not the map.

[`bevy_ecs`]: https://docs.rs/bevy_ecs
[ADR-0005]: Docs/Architecture/ADR-0005-Bevy-ECS-Authority.md
[ADR-0006]: Docs/Architecture/ADR-0006-Editor-Inspectability.md
[ADR-0007]: Docs/Architecture/ADR-0007-Actorless-Rust-Terrain.md
[ADR-0009]: Docs/Architecture/ADR-0009-Native-Rust-3DTiles-Stack.md
[ADR-0011]: Docs/Architecture/ADR-0011-Semantic-Tile-Enrichment.md
[ADR-0012]: Docs/Architecture/ADR-0012-Poll-Based-FFI-Decoupled-Sim-Thread.md
[ADR-0013]: Docs/Architecture/ADR-0013-Terrain-Static-Relevance-GPU-Scene.md
[ADR-0014]: Docs/Architecture/ADR-0014-LogiRuntime-Unified-Executor-Reactor.md
[ADR-0015]: Docs/Architecture/ADR-0015-Simulation-LOD.md
[ADR-0016]: Docs/Architecture/ADR-0016-Per-Lane-Determinism.md
[ADR-0018]: Docs/Architecture/ADR-0018-Geo-Streaming-Parallelism.md
[ADR-0019]: Docs/Architecture/ADR-0019-Cells-Within-Cells-Interlinked.md
[ADR-0020]: Docs/Architecture/ADR-0020-LogiEcs-Incremental-World-Cell.md
[ADR-0021]: Docs/Architecture/ADR-0021-Sim-Tiles-Temporal-Simulation-LOD.md
[ADR-0022]: Docs/Architecture/ADR-0022-LogiSky-Rust-Modeled-Atmosphere.md
[ADR-0023]: Docs/Architecture/ADR-0023-One-Truth-Many-Formulas.md
