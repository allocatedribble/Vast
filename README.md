# Vast

**A near-Earth-scale planet simulation where the *game* lives in Rust and Unreal Engine is a rendering shell.**

NOT NEAR EARTH-SCALE YET.

Vast is a first- and third-person game set in a living, streamed planet — continuous from orbital
flows down to a single character on foot. It is built on one unusual bet, carried all the way through:

> **A Rust simulation owns every piece of gameplay truth. Unreal Engine 5.8 owns none of it.**
> Agents, routes, economy, shipments, terrain — all of it lives in a Rust [`bevy_ecs`] `World`,
> mutated by Systems, in Rust. Unreal renders, animates, hosts, and captures input. It mirrors the
> Rust-owned world; it never originates it.

Most engines put gameplay *inside* the engine. Vast inverts that: the simulation is a headless,
deterministic, data-oriented Rust core that can build and test without an editor open, and the
AAA renderer is a replaceable shell bolted on across a narrow C ABI. The boundary is one-directional
— Unreal calls Rust with plain-data structs; **no Unreal type ever crosses into Rust, and Rust never
calls back.**

---

## What it is

The logistics layer the project was first named for is now **one mode** inside a larger world. The
North Star (see [`AGENTS.md`](AGENTS.md) § 0):

- **Planetary scale, continuous zoom** — real-world Earth streamed from orbit to street level, no
  loading screens, down to one courier carrying one package.
- **Millions of simulated agents as the *floor*, not the ceiling.** The entity count is unbounded by
  design. What's bounded is the *active cut* — by compute budget, never by population — in **space**
  (simulate at full fidelity only what's near attention) and in **time** (a simulation cell's timestep
  is itself a resolution: every frame → once a day → *never*, replayed from a seed when finally observed).
- **"Real where you look, faked where you don't."** Full fidelity inside the player's attention;
  cheap statistical aggregates everywhere else, promoted to individuals on demand.
- **Deterministic, fixed-step simulation** wherever replay or snapshot equality is required — and
  determinism-from-seed doubles as the compression scheme that makes an unbounded world runnable:
  *state you can regenerate is state you don't have to store.*

When a design decision is in doubt, the tie-breaker is fixed: **choose the option that still works at
one million agents — and never hard-caps at any population.**

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

Both the gameplay simulation and the planet-streaming loop run on **Rust-owned threads**. Unreal, on
its game thread, only pushes the current view/input and pulls the latest retained snapshot — it never
blocks on the sim ([ADR-0012]).

### The ideas that make it possible

These are not aspirations bolted on later; they are the load-bearing structure, each with a written
decision record under [`Docs/Architecture/`](Docs/Architecture/):

| Idea | What it buys | ADR |
|---|---|---|
| **Bevy-ECS authority** | one source of gameplay truth, headless & testable | [ADR-0005] |
| **Actorless Rust terrain** | a streamed planet with no `AActor` per tile | [ADR-0007] |
| **Native Rust 3D Tiles stack** | real-world Earth decoded & selected in Rust, not a plugin | [ADR-0009] |
| **Poll-based FFI / decoupled sim** | the renderer never waits on the simulation | [ADR-0012] |
| **Simulation-LOD (space)** | active set bounded by budget, not population | [ADR-0015] |
| **Sim-Tiles (time)** | timestep as a resolution; unbounded entity count | [ADR-0021] |
| **Per-lane determinism** | a deterministic near-set + a free far field | [ADR-0016] |
| **Cells within cells, interlinked** | the structural precedent every abstraction follows | [ADR-0019] |
| **One truth, many formulas** | one quantity, a family of tested (cost, error) tiers | [ADR-0023] |

The repository's full constitution is [`AGENTS.md`](AGENTS.md) (read first) and
[`CLAUDE.md`](CLAUDE.md); the ADRs refine it. New work either follows the precedent or argues, in a
new ADR, why it can't.

---

## Status — honest

Vast is a **frontier project in active development**, not a finished engine. Where things stand:

- **Working today — the Geo engine.** Real-world Earth streams and renders in-editor: Google
  Photorealistic 3D Tiles fetched over **HTTP/3 (QUIC)**, glTF / Draco / quantized-mesh decoded in
  Rust, rendered through a **static-relevance proc-mesh proxy** (GPU-Scene cached draws, [ADR-0013]),
  with BC7 texture compression, a two-tier (bytes + decode) cache, frustum-aware LOD selection, and
  LOD-crack skirts. You can fly from orbit to rooftops. A dedicated `geo-bench` harness drives and
  measures it.
- **In place — the boundary & the runtime.** The one-directional C ABI, the poll-based snapshot
  hand-off, the `logi_ecs` ECS fork ([ADR-0020]) with its budget/significance system, and the
  two-lane `logi_runtime` executor+reactor exist and are exercised by the Geo loop.
- **Active frontier — gameplay simulation at scale.** The architecture for a million agents
  (Simulation-LOD in space and time) is specified and the ECS substrate is built; proving the
  deterministic, fully-decoupled agent simulation *at* that scale is the current work, not a finished
  claim.
- **Partial — LogiSky.** The Rust atmosphere model and its LUT bakers are built and tested; the UE
  GPU shell and its FFI lane are pending ([ADR-0022]).

If you're evaluating this: the planet-streaming core is real and runnable; the at-scale gameplay
simulation is the mountain currently being climbed.

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

- **Unreal Engine 5.8**
- **Rust** nightly (pinned in `rust-toolchain`; currently ~1.97 nightly), host `x86_64-pc-windows-msvc`
- Windows + PowerShell, Visual Studio C++ toolchain, **Git LFS** (binary assets)

**Simulation only** (headless — the fast inner loop):

```powershell
cd Rust
cargo build
cargo test --workspace
cargo clippy --workspace -- -D warnings
```

**Full editor** (Rust artifacts + Unreal):

```powershell
./rebuild-editor.ps1     # builds the Rust cdylibs, then the UE editor target
```

Then open `LogiUE.uproject` in Unreal Editor 5.8 and Play-In-Editor to fly the streamed planet.

---

## Where help is wanted

This is an ambitious build for a small effort, and several lanes are genuinely independent. If any of
these are your home turf, you'd move the needle:

- **Rust simulation / ECS at scale** — the Simulation-LOD active set, the statistical far field, and
  the deterministic seed-promotion that carry the agent count toward a million ([ADR-0015]/[ADR-0021]).
- **Geospatial / streaming** — 3D Tiles selection and decode, cache durability, QUIC transport, the
  working-set streaming step ([ADR-0009]/[ADR-0018]).
- **Real-time rendering (UE 5.8)** — Nanite/Lumen/VSM integration for streamed proc-mesh terrain,
  static-relevance draw paths, the atmosphere GPU shell ([ADR-0013]/[ADR-0022]).
- **Determinism & tooling** — fixed-step replay, snapshot equality, headless test harnesses, profiling.
- **Graphics/sim math** — geodesy, LOD error metrics, the tested (cost, error) formula tiers ([ADR-0023]).

Start by reading [`AGENTS.md`](AGENTS.md), then the ADR most relevant to your lane. Open an issue
describing what you'd like to take on. Architectural changes are proposed as ADRs, not smuggled into
commits — the constitution is how a small project stays coherent.

---

## License

Free

---

*Vast is built in the open as a frontier experiment: a planet, a million agents, and the bet that the
game belongs in Rust. If that bet sounds interesting, you're the person this README is for.*

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
