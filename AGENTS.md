# LogiUE — Agent Constitution

> This is the top-level instruction set for **any** automated or human agent writing code in this
> repository. Read it before writing a single line. If a change you are about to make violates a rule
> here, **stop** and either (a) change your approach, or (b) open an ADR under
> [`Docs/Architecture/`](Docs/Architecture/) proposing the exception. Exceptions are documents, not
> code comments.

LogiUE is **Bevy-first**: a Rust `bevy_ecs` simulation owns gameplay truth, and Unreal Engine 5.8 is its
renderer / animator / host. This supersedes the earlier Mass-first design — see
[ADR-0005](Docs/Architecture/ADR-0005-Bevy-ECS-Authority.md). Translate intent, never API shape.

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
7. **Terrain truth is Rust** — now part of the same Bevy-owned authority (`logi_terrain`), not a separate
   special case. See [ADR-0002](Docs/Architecture/ADR-0002-Rust-Terrain-Kernel.md).
8. **Determinism by ordering, not serialization.** The sim runs on a **fixed-step Bevy schedule**,
   decoupled from frame rate, with **seeded RNG only** (no wall-clock, no `thread_rng`). **Systems run
   multi-threaded by default** (Bevy's parallel executor) and parallelism is *encouraged* — prefer
   disjoint/commutative system access. Replay/snapshot determinism is preserved by **explicit system
   ordering + commutative writes**, NOT by single-threading; a force-single-threaded mode exists for
   strict replay/tests. See [ADR-0001 § Determinism](Docs/Architecture/ADR-0001-Mass-First-Unreal.md).
9. **The FFI boundary is narrow and batched.** Unreal pulls a **per-frame snapshot** from
   `logi_render_extract`; it does **not** make per-entity calls across the ABI. No Unreal types in Rust;
   Rust never calls Unreal. Per-entity FFI chatter is the main performance failure mode — design against it.
10. **No compatibility code.** UE 5.8 only; **`bevy_ecs` as a library**, never the full Bevy engine
    (no `bevy_render`/`bevy_window`/`wgpu`/`bevy_audio`/umbrella `bevy`). See
    [ADR-0004](Docs/Architecture/ADR-0004-No-Compatibility-Code.md).
11. **Significance is a budget, not a distance check** — one unified score drives sim + representation LOD.
    See [ADR-0003](Docs/Architecture/ADR-0003-Representation-LOD.md).
12. **Mass is non-authoritative and transitional only.** No new gameplay state or logic in Mass; the Mass
    plugins survive solely as an optional, clearly-marked debug/migration bridge and are slated for removal.
13. **Any hybrid exception needs an ADR.**

---

## 4. Where things live

| Path | Role | Owns gameplay truth? |
|---|---|---|
| `Rust/crates/logi_runtime` | Owns the Bevy `World`, Schedules, Resources. | **Yes — the authority.** |
| `Rust/crates/logi_sim` | Gameplay Components + Systems. | **Yes.** |
| `Rust/crates/logi_render_extract` | Per-frame snapshots handed to Unreal. | Projection of truth. |
| `Rust/crates/logi_schema` | Spawn recipes, asset handles, archetype recipes. | Authoring data. |
| `Rust/crates/logi_terrain` | Terrain authority (ADR-0002). | **Yes (terrain).** |
| `Rust/crates/logi_ffi` | Opaque handles, versioned C ABI, buffers, error reporting. | Boundary only. |
| `Plugins/LogiBevy/Source/LogiBevyRuntime` | C++ bridge: owns the runtime handle, pulls snapshots, drives ISM/actors. | **No** — shell. |
| `Plugins/LogiBevy/Source/LogiBevyEditor` | Authoring/import/editor-only tooling. | No. |
| `Source/LogiUE` | Thin primary game module. | No. |
| `Source/LogiCore`, `Source/LogiTerrain` (Turn-2 C++) | **Superseded** by the Rust workspace; pending removal/absorption. | No. |
| Mass plugins (`MassGameplay`, `MassAI`) | **Non-authoritative, transitional** only. | **No — never.** |

---

## 5. Build & verify

- **Engine:** Unreal Engine **5.8** (`EngineAssociation` in `LogiUE.uproject` — do not change).
- **Rust:** `cargo build` / `cargo test` in `Rust/` builds and **headlessly tests** the simulation — no
  editor required. This is where determinism/replay tests live. Toolchain: `x86_64-pc-windows-msvc`.
- **C header:** `logi_ffi`'s C ABI header is generated by **cbindgen** (checked in) and consumed by the
  C++ bridge.
- **Unreal:** `Plugins/LogiBevy/Source/LogiBevyRuntime/LogiBevyRuntime.Build.cs` builds/locates the Rust
  artifact, links it, and stages the runtime binary into the plugin's output. Start with a `cdylib`
  (delay-loaded DLL) for iteration; profile before switching to `staticlib`.
- **Live editor + MCP:** a running editor exposes MCP tools at `http://localhost:3000/mcp`. Prefer
  non-destructive operations; new modules/plugins require a from-scratch build (Live Coding can't add them).

### Repository conventions
- **Git LFS** for binary assets; build output (`Binaries/`, `Intermediate/`, `Saved/`, `DerivedDataCache/`,
  Rust `target/`, generated `.sln/.slnx`) is git-ignored.

---

## 6. When you're unsure

Default to **Rust / Bevy**. Default to **data** (Components/Resources). Default to a **batched snapshot**
over per-entity FFI. Default to opening an **ADR** rather than smuggling an exception into a commit. If a
choice only works at a hundred agents, it is the wrong choice.
