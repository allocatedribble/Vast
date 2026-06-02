# LogiUE — Agent Constitution

> This is the top-level instruction set for **any** automated or human agent writing code in
> this repository. Read it before writing a single line. If a change you are about to make
> violates a rule here, **stop** and either (a) change your approach, or (b) open an ADR under
> [`Docs/Architecture/`](Docs/Architecture/) proposing the exception. Exceptions are documents,
> not code comments.

LogiUE is the Unreal Engine 5.8 incarnation of the Logi logistics simulation. It is **not** a
port of the prior Unity/DOTS project — it is a re-implementation of the *intent* on a Mass-first
Unreal architecture. Translate intent, never API shape.

---

## 0. North Star

LogiUE is a **near-Earth-scale logistics simulation**. The design targets:

- **Planetary / near-Earth scale**, with a continuous zoom range from orbital-scale flows all the
  way down to **one courier carrying one package**.
- **Millions of simulated agents.**
- **"Real where you look, faked where you don't"** — full fidelity inside the player's attention;
  aggregate / statistical modeling everywhere else.
- **Deterministic, fixed-step simulation** wherever the design requires replay or snapshot equality.

Every technical decision serves that scale. When in doubt, choose the option that still works at
**one million agents**, not the one that is convenient at one hundred.

---

## 1. The Hard Architectural Law

**Mass owns runtime gameplay state.**

Canonical runtime state — **delivery state, agent state, route state, economy state, and the
runtime view of terrain truth** — lives in **Mass Fragments**. Nothing else owns it.

`AActor`, `UObject`, `UActorComponent`, Blueprint graphs, and UMG/Slate widgets are
**presentation, authoring, editor tooling, asset hosting, or engine integration only**. They
render, author, host assets, or bridge to engine systems. They do **not** own simulation truth.

If you find yourself storing delivery / agent / route / economy / terrain state on an Actor or a
component "just for now," you are writing code that will be deleted. Put it in a Fragment.

---

## 2. Vocabulary — speak Mass, not Unity

| Concept | What it is | Type basis |
|---|---|---|
| **Fragment** | An atomic, POD-style struct of data. The unit of state. | `USTRUCT`, `FMassFragment` |
| **Tag** | A zero-size Fragment used purely as a boolean archetype discriminator. | `FMassTag` |
| **Entity** | An ID naming a composition of Fragments. Not an object; not an Actor. | `FMassEntityHandle` |
| **Archetype** | All entities sharing an identical Fragment composition, stored together in memory chunks for cache-friendly iteration. | (engine-internal) |
| **Processor** | Stateless logic that runs over entities selected by an EntityQuery. **All gameplay logic lives here.** | `UMassProcessor` |
| **EntityQuery** | The declared set of Fragments/Tags a Processor reads/writes. | `FMassEntityQuery` |
| **Trait** | An authoring-time assembler declaring which Fragments/Tags/initial values an entity receives. Traits **compose capability**. | `UMassEntityTraitBase` |
| **Command Buffer** | The **only** sanctioned way to make *structural* changes (add/remove Fragments, create/destroy entities) — deferred and batched. | `FMassCommandBuffer` |

Do **not** introduce Unity/DOTS vocabulary into runtime code: no `GameObject`, `MonoBehaviour`,
`SubScene`, `Burst`, `SystemBase`, `IComponentData`, `EntityManager`-as-a-userland-type, etc.
The only place Unity terms may appear is in **migration notes** that explicitly translate an old
concept to its Mass equivalent.

---

## 3. The Rules

1. **Fragments are data.** No logic, no UObject pointers held as authority, no virtuals. Keep them
   POD-friendly and serialization/replication-friendly.
2. **Processors are logic.** Stateless. All reads/writes go through EntityQueries. No hidden
   per-processor mutable state that changes results between runs.
3. **Traits assemble capability.** New capability = new Fragment(s) + a Trait that adds them + a
   Processor that acts on them. Compose; do not build deep Actor inheritance trees.
4. **`AActor` is a shell, not authority.** Actors exist only where Unreal requires a scene presence
   (skeletal-mesh host, collision, audio emitter, camera). They **mirror** Fragment state for
   rendering; they never originate it. The exact permitted Actor zones are enumerated in
   [ADR-0001](Docs/Architecture/ADR-0001-Mass-First-Unreal.md).
5. **Blueprints are for authoring, animation graphs, UI, and editor convenience only.** No gameplay
   authority; no simulation branching that the C++/Mass layer depends on for correctness.
6. **Terrain truth is Rust.** Planet terrain selection, deterministic tile identity, terrain source
   data, and tile streaming *decisions* are owned by the native Rust kernel. Unreal renders it.
   See [ADR-0002](Docs/Architecture/ADR-0002-Rust-Terrain-Kernel.md).
7. **Determinism where the design requires it.** Simulation that participates in replay / snapshot
   equality runs on a deterministic fixed step. No frame-rate-dependent math, no nondeterministic
   iteration order, no `FMath::Rand` — use seeded random streams. See
   [ADR-0001](Docs/Architecture/ADR-0001-Mass-First-Unreal.md) § Determinism.
8. **Structural changes go through Mass command buffers.** Never add/remove Fragments or
   create/destroy entities directly during processing — it moves entities between archetypes
   mid-iteration and corrupts the pass.
9. **No compatibility code.** UE 5.8 only. No Unity compatibility layer, no DOTS shims, no
   backward-compatibility branches for older engine versions.
   See [ADR-0004](Docs/Architecture/ADR-0004-No-Compatibility-Code.md).
10. **No Unity vocabulary** except in clearly-marked migration notes.
11. **Significance is a budget, not a distance check.** Simulation and representation fidelity are
    driven by a single unified **significance score** (camera distance, screen size, frustum state,
    player interaction, hub importance, route profitability, debug pinning) — not by a raw distance
    threshold. See [ADR-0003](Docs/Architecture/ADR-0003-Representation-LOD.md).
12. **Any hybrid exception needs an ADR.** If you must violate a rule above, it is a new entry under
    [`Docs/Architecture/`](Docs/Architecture/), reviewed and accepted — not a comment buried in a diff.

---

## 4. Where things live

| Path | Role | Owns gameplay state? |
|---|---|---|
| `Source/LogiUE/` | Runtime C++ module. Currently template-thin (Core, CoreUObject, Engine, InputCore, EnhancedInput). | Will host Mass setup/bootstrapping; **not** a dumping ground for systems. |
| `Plugins/McpAutomationBridge/` | Editor automation / MCP bridge tooling. **Tooling, not gameplay.** | No. Never. |
| *(Turn 2+)* dedicated Mass plugin(s)/module(s) | Fragments, Processors, Traits, Mass subsystems. | **Yes — this is where runtime truth lives.** |
| *(Turn 2+)* Rust terrain kernel + UE integration module | Native terrain truth behind a narrow FFI. | Terrain truth: yes (in Rust). |

> **Do not pile systems into `Source/LogiUE`.** Turn 2 establishes the Mass module/plugin layout.
> Until then, prefer to stage new Mass work behind that decision rather than bloating the starter
> module.

---

## 5. Build & verify

- **Engine:** Unreal Engine **5.8** (`EngineAssociation` in `LogiUE.uproject` — do not change).
- **Live editor + MCP:** an Unreal Editor instance exposes an MCP bridge at `http://localhost:3000/mcp`
  (see `.mcp.json`, provided by `Plugins/McpAutomationBridge`). The editor must be **running** for
  MCP tools to work. Prefer Live Coding / incremental builds while the editor is open; do not force a
  full UBT relink of a loaded DLL.
- **A clean compile is proven by:** the editor loading the `LogiUE`/`LogiUEEditor` target with a valid
  target receipt and no `Error:`/`Fatal` lines in `Saved/Logs/LogiUE.log`.
- **Profiling:** MassInsights, Unreal Insights, and `stat` commands. See
  [Docs/Performance/Profiling-Checklist.md](Docs/Performance/Profiling-Checklist.md).

### Repository conventions

- **Git LFS** is configured (`.gitattributes`) for binary assets (`.uasset`, `.umap`, textures,
  audio, models, fonts). Do not commit large binaries outside LFS.
- Build output and caches (`Binaries/`, `Intermediate/`, `Saved/`, `DerivedDataCache/`, generated
  `.sln/.slnx`) are git-ignored. Never commit them.

---

## 6. When you're unsure

Default to **Mass**. Default to **data**. Default to **deleting Unity-isms**. Default to opening an
**ADR** rather than smuggling an exception into a commit. If a choice only works at a hundred agents,
it is the wrong choice.
