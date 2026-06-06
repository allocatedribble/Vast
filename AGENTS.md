# LogiUE — Agent Constitution

> This is the top-level instruction set for **any** automated or human agent writing code in this
> repository. Read it before writing a single line. If a change you are about to make violates a rule
> here, **stop** and either (a) change your approach, or (b) open an ADR under
> [`Docs/Architecture/`](Docs/Architecture/) proposing the exception. Exceptions are documents, not
> code comments.

LogiUE is **Bevy-first**: a Rust `bevy_ecs` simulation owns gameplay truth, and Unreal Engine 5.8 is its
renderer / animator / host. This supersedes the earlier Mass-first design — see
[ADR-0005](Docs/Architecture/ADR-0005-Bevy-ECS-Authority.md). Translate intent, never API shape.

### Architecture decision records — read before you design

The constitution is refined by ADRs in [`Docs/Architecture/`](Docs/Architecture/). Current law:

- **[ADR-0005](Docs/Architecture/ADR-0005-Bevy-ECS-Authority.md)** — Bevy ECS (Rust) owns gameplay; supersedes the Mass-authority parts of ADR-0001.
- **[ADR-0006](Docs/Architecture/ADR-0006-Editor-Inspectability.md)** — editor-only inspection shells over the Rust world; zero shipping cost.
- **[ADR-0007](Docs/Architecture/ADR-0007-Actorless-Rust-Terrain.md)** — actorless Rust terrain (**the Geo engine**); no `AActor` per tile. Supersedes the terrain-host clause of ADR-0005.
- **[ADR-0008](Docs/Architecture/ADR-0008-Terrain-Nanite-Lane.md)** — runtime Nanite encode is editor-only in UE 5.8; ship needs an offline bake or a static-relevance proxy.
- **[ADR-0009](Docs/Architecture/ADR-0009-Native-Rust-3DTiles-Stack.md)** — native Rust OGC 3D Tiles / real-world-earth stack (Milestone F).
- **[ADR-0011](Docs/Architecture/ADR-0011-Semantic-Tile-Enrichment.md)** — offline semantic-tile bake; ML stays offline, the runtime stays lean, render data is non-authoritative.
- **[ADR-0001](Docs/Architecture/ADR-0001-Mass-First-Unreal.md)** / **[ADR-0002](Docs/Architecture/ADR-0002-Rust-Terrain-Kernel.md)** / **[ADR-0003](Docs/Architecture/ADR-0003-Representation-LOD.md)** / **[ADR-0004](Docs/Architecture/ADR-0004-No-Compatibility-Code.md)** — superseded in part; their Determinism, "Unreal is a shell", terrain-truth, significance-budget, and no-compat clauses still hold.

> **The real-world terrain / earth stack is the "Geo" engine** — `Rust/crates/geo/` (crates `logi_geo_*`
> + the 3D-Tiles / geospatial / semantic crates), shipped as the cdylib `logi_geo.dll`. It was renamed
> from "terrain"; the noun *terrain* still names the ground/heightfield data it renders. ADR-0007/0009/0011
> govern it.

---

## 0. North Star

LogiUE is a **near-Earth-scale logistics simulation**:

- **Planetary / near-Earth scale**, continuous zoom from orbital flows down to **one courier carrying one package**.
- **Millions of simulated agents.**
- **"Real where you look, faked where you don't"** — full fidelity inside the player's attention; aggregate modeling elsewhere.
- **Deterministic, fixed-step simulation** wherever replay or snapshot equality is required.

When in doubt, choose the option that still works at **one million agents**.

---

## 1. The Hard Architectural Law

**Rust Bevy ECS owns runtime gameplay truth. Unreal owns presentation, animation, rendering, asset
hosting, editor tooling, input capture, and engine integration.**

Canonical runtime state — **delivery, agent, route, economy, shipment, and terrain state** — lives in the
**Bevy `World`** (Components and Resources), mutated by **Systems**. Nothing else owns it.

**No `UObject`, `AActor`, Blueprint, Mass fragment, or Unreal component owns authoritative gameplay
state.** They are shells: they mirror Rust-owned state for rendering and capture player intent; they
never originate truth.

**The boundary is one-directional.** Unreal calls Rust across a **narrow C ABI**. Plain-data structs
only — **no Unreal types cross into Rust, and Rust never calls back into Unreal.** (This is the
ADR-0002 terrain-kernel contract, now project-wide via ADR-0005.)

If you are about to store delivery/agent/route/economy/terrain state on an Actor, component, or Mass
fragment, stop — it belongs in a Bevy Component or Resource.

---

## 1A. The craft bar — ship production quality, copy nothing you needn't

Every change lands at the **highest production grade**. This is not aspirational; it is the acceptance
bar a diff must clear before it is committed:

- **No placeholders shipped as features.** No `todo!()`, stubbed return, or "wire it later" on a code
  path that *looks* like it works. A deferred capability is **absent and documented**, never faked. Dead
  code, unused fields, and commented-out blocks do not land (ADR-0004 — no compatibility/scaffolding cruft).
- **Errors are handled, never swallowed.** No `unwrap()`/`expect()`/`panic!`/`unreachable!`/slice-indexing
  that can fire on real or over-the-wire input in non-test code. **Decoders must not panic on bad bytes;
  every `extern "C"` fn catches unwinds** — a panic must never cross the C ABI (it is undefined behavior).
- **Determinism is preserved** (§8): seeded RNG only, no wall-clock in the step, no hash-iteration order
  leaking into output.
- **Single source of truth.** No copy-pasted logic or parallel structs kept in sync by hand. Shared
  contracts — wire/vertex formats, geodesy/transform math, hashes, error enums — live in exactly **one**
  crate and are imported, not re-typed.
- **Green before commit:** `cargo build`, `cargo test`, `cargo clippy -- -D warnings`, and `cargo fmt`
  all pass; C++ follows Unreal conventions and builds clean.

### Rust: copy nothing you needn't (zero-copy by default)

At a million agents and a streamed planet, **needless copies and allocations are the default failure
mode.** Treat every clone and allocation as a cost you must justify:

- **Borrow, don't clone.** Prefer `&T` / `&[T]` / `&str` over owned `T` / `Vec<T>` / `String`. Return
  slices, iterators, or `Cow<'_, _>` when the caller only reads; don't hand back a freshly-allocated
  `Vec`/`String` for read-only data.
- **Never `.clone()` / `.to_vec()` / `.to_owned()` to dodge the borrow checker** — restructure ownership.
- **Zero-copy across the FFI boundary.** Hand Unreal pointer+len **views** into Rust-owned buffers (valid
  until the next `step`/`destroy`); never copy a mesh/tile buffer to cross the ABI. Decode binary in place
  (`bytemuck` / slice casts), don't byte-bash element by element.
- **No `.collect()` then re-iterate** — chain iterators. **No reallocation in loops** — `with_capacity`
  up front. **No per-frame or per-entity allocation** on the snapshot/extract path (the documented main
  performance failure mode, §9).
- **`#[derive(Copy)]` POD components**; pass large aggregates by reference.

If a copy is genuinely required, it carries a one-line comment stating why. "It was easier" is not a why.

---

## 2. Vocabulary — speak Bevy, not Mass/Unity

| Term | What it is | Type basis |
|---|---|---|
| **Component** | Atomic data attached to an entity. The unit of state. | `#[derive(Component)]` |
| **Resource** | Global/singleton state in the `World`. | `#[derive(Resource)]` |
| **Entity** | An ID naming a set of Components. Not an object; not an Actor. | `Entity` |
| **Bundle** | A set of Components spawned together. | `#[derive(Bundle)]` |
| **Schedule** | An ordered set of Systems run per step (incl. the fixed-step sim schedule). | `Schedule` |
| **System** | A function that reads/writes the World through Queries. **All gameplay logic.** | `fn(...) ` system |
| **Query** | Declares which Components a System reads/writes. | `Query<...>` |
| **Command** | Deferred structural change (spawn/despawn, insert/remove). | `Commands` |
| **Event / Message** | Decoupled inter-system signal. | `Event` / `EventWriter`/`Reader` |
| **Archetype / Table storage** | Dense storage for stable compositions (default; cache-friendly iteration). | table storage |
| **SparseSet storage** | Storage for components that churn (frequent add/remove). | `#[component(storage = "SparseSet")]` |

Do **not** introduce Mass (`Fragment`, `Processor`, `Trait`, `FMassCommandBuffer`, …) or Unity
(`GameObject`, `MonoBehaviour`, …) vocabulary into gameplay code. Those terms appear only in migration notes.

---

## 3. The Rules

1. **Components are data** (Rust). Plain, `Copy`/POD-friendly where possible; no behavior.
2. **Systems are logic** (Rust). Pure over their Queries; deterministic; order-independent unless explicitly ordered.
3. **Bundles assemble entities.** New capability = new Component(s) + a Bundle + a System.
4. **Resources hold global/singleton state** (clock, config, RNG seed, terrain handle).
5. **Structural changes go through `Commands`** — never mutate archetype membership mid-iteration.
6. **`AActor`/`UObject`/Blueprint/widget are shells.** They mirror Rust state and capture intent only.
   Permitted shell zones are enumerated in [ADR-0005](Docs/Architecture/ADR-0005-Bevy-ECS-Authority.md).
7. **Terrain truth is Rust.** The coarse deterministic field gameplay reads lives in `logi_sim`; the
   render-side **Geo engine** (`Rust/crates/geo/`, cdylib `logi_geo.dll`) is **actorless and
   non-authoritative**. See [ADR-0007](Docs/Architecture/ADR-0007-Actorless-Rust-Terrain.md) (supersedes
   the terrain-host clause of ADR-0002/0005).
8. **Determinism by ordering, not serialization.** The sim runs on a **fixed-step Bevy schedule**,
   decoupled from frame rate, with **seeded RNG only** (no wall-clock, no `thread_rng`). **Systems run
   multi-threaded by default** (Bevy's parallel executor) and parallelism is *encouraged* — prefer
   disjoint/commutative system access. Replay/snapshot determinism is preserved by **explicit system
   ordering + commutative writes**, NOT by single-threading; a force-single-threaded mode exists for
   strict replay/tests. See [ADR-0001 § Determinism](Docs/Architecture/ADR-0001-Mass-First-Unreal.md).
9. **The FFI boundary is narrow and batched.** Unreal pulls a **per-frame snapshot** from
   `logi_render_extract`; it does **not** make per-entity calls across the ABI. No Unreal types in Rust;
   Rust never calls Unreal. Per-entity FFI chatter is the main performance failure mode — design against it.
10. **No compatibility code; keep Bevy headless.** UE 5.8 only; **`bevy_ecs` as a library**, never the
    full Bevy engine. **Forbidden:** any *direct* dependency that makes Bevy render, window, or play audio
    — `bevy_render`, `bevy_window`, `bevy_audio`, `winit`, a direct `wgpu`, or the umbrella `bevy`.
    **Allowed:** type-only transitive crates that `bevy_ecs`'s default features pull in — notably
    `bevy_reflect` → `wgpu-types` (pure `#[repr(C)]`/enum *type definitions*; no GPU, no renderer, no
    runtime). The line is *no second renderer and no GPU/windowing/audio runtime*, **not** *no crate whose
    name contains "wgpu"*. Unreal renders; Bevy only simulates. See
    [ADR-0004](Docs/Architecture/ADR-0004-No-Compatibility-Code.md).
11. **Significance is a budget, not a distance check** — one unified score drives sim + representation LOD.
    See [ADR-0003](Docs/Architecture/ADR-0003-Representation-LOD.md).
12. **Mass is non-authoritative and transitional only.** No new gameplay state or logic in Mass; the Mass
    plugins survive solely as an optional, clearly-marked debug/migration bridge and are slated for removal.
13. **Any hybrid exception needs an ADR.**
14. **Ship production quality; copy nothing you needn't** — the craft bar in **§1A** is part of the law:
    no faked/placeholder features, no swallowed errors, no panic across the C ABI, no needless
    `clone`/allocation, one home per shared contract. `cargo build`/`test`/`clippy -D warnings`/`fmt` are
    green before commit. Justify every copy.

---

## 4. Where things live

| Path | Role | Owns gameplay truth? |
|---|---|---|
| `Rust/crates/logi_runtime` | Owns the Bevy `World`, Schedules, Resources. | **Yes — the authority.** |
| `Rust/crates/logi_sim` | Gameplay Components + Systems (incl. the coarse deterministic terrain field). | **Yes.** |
| `Rust/crates/logi_render_extract` | Per-frame snapshots handed to Unreal. | Projection of truth. |
| `Rust/crates/logi_schema` | Spawn recipes, asset handles, archetype recipes. | Authoring data. |
| `Rust/crates/logi_ffi` | Gameplay C ABI: opaque handles, versioned ABI, snapshot buffers (`logi_ffi.dll`). | Boundary only. |
| **`Rust/crates/geo/`** | **The Geo engine** — real-world terrain / earth stack (ADR-0007/0009/0011). Render data is non-authoritative; the coarse field promoted to gameplay is owned by `logi_sim`. | Render data. |
| &nbsp;&nbsp;`geo/logi_geo_{pack,procgen,lod}` | Geo kernel: fractal pack tables, canonical `TerrainHeights` field, screen-space-error LOD (shared by sim + render). | Shared field. |
| &nbsp;&nbsp;`geo/logi_geo_ffi` | The Geo C ABI cdylib → **`logi_geo.dll`** (baked / procedural / globe / building lanes). | Boundary only. |
| &nbsp;&nbsp;`geo/logi_geospatial`, `geo/logi_qmesh`, `geo/logi_gltf`, `geo/logi_collada` | Geodesy + mesh/format decoders for the 3D-Tiles stack (ADR-0009). | No — decode. |
| &nbsp;&nbsp;`geo/logi_3dtiles_*` | OGC 3D Tiles: tileset + implicit parse, SSE selection, IO/streaming, Cesium ion. | No — render. |
| &nbsp;&nbsp;`geo/logi_semantic` | Baked semantic-tile store reader + render meshing (ADR-0011). | No — render. |
| `Plugins/LogiBevy/Source/LogiBevyRuntime` | C++ bridge: owns the runtime handle, pulls snapshots, drives ISM/actors, hosts the Geo render shells. | **No** — shell. |
| `Plugins/LogiBevy/Source/LogiBevyEditor` | Authoring/import/editor-only tooling. | No. |
| `Source/LogiUE` | Thin primary game module. | No. |
| Mass plugins (`MassGameplay`, `MassAI`) | **Non-authoritative, transitional** only. | **No — never.** |

---

## 5. Build & verify

- **Engine:** Unreal Engine **5.8** (`EngineAssociation` in `LogiUE.uproject` — do not change).
- **Rust:** `cargo build` / `cargo test` in `Rust/` builds and **headlessly tests** the simulation — no
  editor required. This is where determinism/replay tests live. Toolchain: `x86_64-pc-windows-msvc`.
  CI gates the workspace on `cargo fmt --check`, `cargo clippy -- -D warnings`, and `cargo test`.
- **C header:** `logi_ffi`'s C ABI header (`logi_bevy_ffi.h`) is generated by **cbindgen** (checked in)
  and consumed by the C++ bridge. The Geo ABI header (`logi_geo_ffi.h`) is hand-maintained alongside
  `geo/logi_geo_ffi/src`.
- **Unreal:** `Plugins/LogiBevy/Source/LogiBevyRuntime/LogiBevyRuntime.Build.cs` builds/locates the Rust
  artifacts (`logi_ffi.dll` + `logi_geo.dll`), links them, and stages the runtime binaries into the
  plugin's output. Start with a `cdylib` (delay-loaded DLL) for iteration; profile before switching to
  `staticlib`. `rebuild-editor.ps1` does the full close-editor → build Rust + C++ → relaunch loop.
- **Live editor + MCP:** a running editor exposes MCP tools at `http://localhost:3000/mcp`. Prefer
  non-destructive operations; new modules/plugins require a from-scratch build (Live Coding can't add them).

### Repository conventions
- **Git LFS** for binary assets; build output (`Binaries/`, `Intermediate/`, `Saved/`, `DerivedDataCache/`,
  Rust `target/`, generated `.sln/.slnx`) is git-ignored.

---

## 6. When you're unsure

Default to **Rust / Bevy**. Default to **data** (Components/Resources). Default to a **batched snapshot**
over per-entity FFI. Default to a **borrow** over a copy (§1A). Default to opening an **ADR** rather than
smuggling an exception into a commit. If a choice only works at a hundred agents, it is the wrong choice.
