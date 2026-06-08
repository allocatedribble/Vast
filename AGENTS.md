# Vast — Agent Constitution

> This is the top-level instruction set for **any** automated or human agent writing code in this
> repository. Read it before writing a single line. If a change you are about to make violates a rule
> here, **stop** and either (a) change your approach, or (b) open an ADR under
> [`Docs/Architecture/`](Docs/Architecture/) proposing the exception. Exceptions are documents, not
> code comments.

Vast is **Bevy-first**: a Rust `bevy_ecs` simulation owns gameplay truth, and Unreal Engine 5.8 is its
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
- **[ADR-0012](Docs/Architecture/ADR-0012-Poll-Based-FFI-Decoupled-Sim-Thread.md)** — poll-based FFI: the sim/stream loop runs on Rust-owned threads, Unreal only pushes view/input and pulls the latest snapshot. Amends ADR-0005's owner-thread (relocated off the game thread, not removed); plan in [Docs/Plans/Async-Sim-Decoupling-and-Geo-Performance.md](Docs/Plans/Async-Sim-Decoupling-and-Geo-Performance.md).
- **[ADR-0001](Docs/Architecture/ADR-0001-Mass-First-Unreal.md)** / **[ADR-0002](Docs/Architecture/ADR-0002-Rust-Terrain-Kernel.md)** / **[ADR-0003](Docs/Architecture/ADR-0003-Representation-LOD.md)** / **[ADR-0004](Docs/Architecture/ADR-0004-No-Compatibility-Code.md)** — superseded in part; their Determinism, "Unreal is a shell", terrain-truth, significance-budget, and no-compat clauses still hold.

> **The real-world terrain / earth stack is the "Geo" engine** — `Rust/crates/geo/` (crates `logi_geo_*`
> + the 3D-Tiles / geospatial / semantic crates), shipped as the cdylib `logi_geo.dll`. It was renamed
> from "terrain"; the noun *terrain* still names the ground/heightfield data it renders. ADR-0007/0009/0011
> govern it.

---

## 0. North Star

Vast is a **near-Earth-scale, dynamic planet simulation** — a first- and third-person game in a living
world. The logistics layer the project was named for is now **one mode** inside it:

- **Planetary / near-Earth scale**, continuous zoom from orbital flows down to **a single character on foot** (or one courier carrying one package).
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
ADR-0002 terrain-kernel contract, now project-wide via ADR-0005.) The implementation-level safety
contract for this boundary — what may cross, panic handling, string marshaling — is **§1C**.

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

## 1B. Rust house rules — idiomatic, simple, safe

§1A is the *acceptance bar*; this is *how to write the code that clears it.* These are house rules, not
architecture — where one collides with a rule in §3, **§3 wins.**

**Idiomatic Rust.**
- **Recoverable errors are `Result<T, E>`, propagated with `?`** — never `unwrap()`/`expect()`/`panic!`
  off the test path (§1A states this for the boundary; it holds everywhere). Model failure as data.
- **Libraries define typed errors with `thiserror`** (already the workspace dep): matchable variants,
  `#[from]` for conversions, `#[source]` for chains. Reserve **`anyhow` + `.context(...)`** for a
  top-level binary or dev tool — we ship none today, so in practice everything is `thiserror`.
- **Push invariants into the type system** (Effective Rust, Items 1–6): newtypes over raw primitives (a
  `TileId`, not a bare `u32`), enums over `bool` flags, `Option`/`Result` over sentinels like `-1`.
- **Follow the Rust API Guidelines:** implement the standard traits (`Debug`, `Clone`, `PartialEq`, …)
  where they fit, take `impl AsRef<_>`/generics for flexible args, and document every public item.
- **Prefer iterator transforms over manual index loops** where it reads clearer (Effective Rust, Item 9).

**Simplicity (YAGNI).**
- **Build the simplest thing that satisfies the requirement.** No abstraction, trait, generic, or config
  knob for a hypothetical future need — *the best code is the code you never wrote.*
- **Don't import GoF patterns Rust makes unnecessary** — reach for traits, enums, and closures directly
  instead of Strategy/Command/Visitor scaffolding.
- **Minimize dependencies; justify each new crate** in the PR. This compounds rule 10 (`bevy_ecs`-only,
  no second renderer): a dep that drags in a runtime/renderer/windowing/audio stack is *forbidden*, not
  merely discouraged. Readable beats clever.
- **If a solution feels complicated, stop and propose a simpler one before coding.**

**Performance — measure first.**
- **Profile before you optimize.** Benchmark hot game-loop paths with **criterion**; find the hot path
  with **samply** / **cargo-flamegraph** (§5). Don't micro-optimize cold paths.
- The borrow-don't-clone / no-per-frame-allocation rules live in **§1A**. In addition: **return
  `impl Iterator` instead of `.collect()`-ing a temp you only re-iterate**, use `extend` over
  collect-then-append, give `size_hint` where you can, **reuse a cleared "workhorse" `Vec`** across
  frames rather than reallocating, and reach for `Cow<'_, _>` when data is usually read-only but
  occasionally mutated.

**Bug-resistance — lean on the compiler.**
- **Match enums exhaustively.** Avoid catch-all `_` arms that silently swallow a newly-added variant —
  name the variants so the compiler flags the next one.
- **Handle every error and edge case.** No silent `let _ =` on a fallible result; no ignored `Result`.
- **No `unsafe` without a `// SAFETY:` comment** justifying every invariant. Keep `unsafe` blocks minimal
  and **covered by `miri`** (§1C, §5).
- **Test new logic, including the failure paths.** Use **proptest** for pure functions (decoders,
  geodesy/transform math, packing).

---

## 1C. The C ABI contract — implementing the §1 boundary safely

§1 fixes *who* owns truth and that the boundary is one-directional; this is the *how* — the safety
contract every `extern "C"` entrypoint in `logi_ffi` and `geo/logi_geo_ffi` obeys. It complements §1A's
zero-copy-view rule; it does not replace it.

- **Only plain data crosses.** `#[repr(C)]` structs, opaque `*mut`/`*const` handles, scalars, and
  `Option<extern "C" fn(...)>` — nothing else. **Never** hand a `&str`, `String`, `Vec`, or an
  un-`#[repr]`'d enum across the ABI. Marshal strings as a `*const c_char` + length (own them with
  `CString`); marshal buffers as the pointer+len **views** into Rust-owned memory described in §1A.
- **No shared mutable state across the boundary** — concurrent mutation from both sides is a data race,
  hence UB. Unreal pulls **one batched per-frame snapshot** (§3.9); it does not reach in and mutate.
- **Every entrypoint wraps its body in `catch_unwind`.** A panic must never unwind across the C ABI (UB).
  Our release profile is **`panic = "abort"`** (`Rust/Cargo.toml`), so in shipping builds a panic aborts
  the process *before* it can unwind; `catch_unwind` is therefore the contract for **unwind (dev/test)
  builds** and a defense-in-depth backstop, while `abort` is the hard release guarantee. Either way, no
  unwind reaches Unreal. **Decoders over untrusted bytes return `Result` — they never panic** — so bad
  input is a handled error, not an abort.
- **Link `--release` into Unreal.** The cdylibs use the dynamic MSVC CRT that matches Unreal's; a
  debug-CRT build mismatches and will not link/load. (Build & link details: §5.)
- **Standing gates for the two FFI crates:** `cargo +nightly miri test` over their unsafe Rust paths, and
  a `cargo geiger` review of the unsafe surface (§5).
- **Stay on raw `extern "C"` + cbindgen.** If you ever evaluate `cxx`, you own compiling its generated
  `.cc` shim inside UBT — adopt it only if you are prepared for that. The header (`logi_bevy_ffi.h`)
  stays cbindgen-generated and checked in.

---

## 1D. Effort & invention — finish the job, and dare to do it better

§1A sets *how high* the bar is; this sets *how you carry yourself toward it.* The two dispositions this
section forbids are **laziness** and **timidity.**

**Be thorough — finish the whole job, not the easy 80%.**
- **Solve the actual problem, root and branch.** Trace a defect to its cause and fix the cause; don't
  patch the symptom and move on. When a change touches one call site, find the others. When you fix a class
  of bug, sweep the codebase for its siblings.
- **No half-measures left as "done."** Don't stop at the first thing that compiles or the first green
  test. Handle the edge cases, the error paths, the empty / oversized / malformed input, the second lane.
  "It works on the happy path" is a starting point, not a deliverable — a stub dressed as a feature is the
  craft bar (§1A) violated as a work ethic: laziness with a bow on it.
- **Read enough to be sure.** Before you claim something is unused, unreachable, or safe to delete, read
  the code that proves it — don't guess. Before you report a task complete, verify it: build it, run it,
  test it, and say plainly what you checked and what you didn't.
- **Chase the question down.** If understanding the system takes ten files, read ten files; if it takes a
  throwaway experiment to learn the truth, run one. Don't answer from a half-formed picture.

**Be inventive — experiment in the name of progress.**
- **Ambition is welcome.** This is a frontier project — a Rust-authoritative simulation hosted by Unreal
  at planetary scale — and the interesting solutions are often the ones nobody has written down yet.
  Propose the bold approach, not only the safe one; when two paths exist, it is fair to prototype both and
  let the measurement decide.
- **Experiments are encouraged — make them cheap and honest.** Spike a throwaway prototype to de-risk a
  design, benchmark a wild idea against the obvious one, try the unconventional data layout. Keep
  experiments isolated (a branch, a bench, a throwaway test), measure them against the budgets and perf
  gates (§1B, §5), and either land the winner at production grade or delete it — a spike never ships as-is.
- **Creativity serves the North Star and respects the law — it is not licence to break it.** Invention
  shows up as a *better solution inside the constraints*, or as a **proposed ADR** when the constraint
  itself is what's holding progress back (§ intro, Rule 13). The architectural law is not the enemy of
  ambition; smuggling an exception past it in a commit is.
- **Invention is not gold-plating.** Daring to do it better is not building for a future nobody asked for —
  YAGNI and "the simplest thing that satisfies the requirement" (§1B) still hold. The creativity that
  counts solves a *real* problem more elegantly, cheaply, or scalably; it does not bolt on speculative
  machinery. When ambition and simplicity pull apart, the tie-breaker is the North Star: which option still
  works — and still ships — at a million agents?

---

## 1E. Complexity & cost — pick the lowest order, then the cheapest instruction

At planetary scale the algorithm's *shape* decides whether the project is possible at all; the instruction
stream decides whether the hot paths hold frame budget. Reason about both — at the right time.

**Asymptotic complexity is a design-time discipline — always on, never "premature."**
- **Know the big-O of every loop, query, and std call you write** — time *and* space — and the `n` it runs
  over. At a million agents, a streamed planet, and a per-frame extract, the gap between `O(n)` and
  `O(n²)` is the gap between *ships* and *does not run*. Choosing the lower-order algorithm up front is not
  optimization; it is correctness at scale (§0, North Star).
- **Reduce the order.** Prefer `O(1)` / `O(log n)` / `O(n)` over `O(n log n)` over `O(n²)`. A nested loop
  over the same collection, a linear scan inside a per-entity system, an `O(n)` `contains` in a hot loop,
  an accidental quadratic from `remove`-in-a-loop — these are design defects. Reach for the hash set, the
  spatial index (grid / BVH), the sort-then-sweep, the precomputed lookup. Significance / LOD (§0,
  ADR-0003) exists precisely so the *expensive* work runs over a **bounded** `n`, not all of it.
- **Pick the data structure that makes the hot operation cheap**, then live with its trade-offs: a
  `HashMap` for membership, a sorted `Vec` + binary search for cache-friendly lookup, `SparseSet` vs table
  storage by churn (§2). The right container turns an `O(n)` scan into an `O(1)` hit.
- **This is not licence to single-thread or serialize for "simplicity."** Lower the order first, *then*
  exploit the parallel executor (Rule 8) — they compound, they don't compete.

**Instruction-level cost is a hot-path discipline — guided by the profiler (§1B), not by guesswork.**
- On the paths the profiler shows are hot (per-frame, per-agent, per-tile, the decode / extract loop),
  minimize **both the number of instructions and their cost.** A function's real latency is
  `instructions × cycles-each + stalls`, and at scale the **stalls usually dominate** — a cache miss costs
  hundreds of arithmetic ops, a branch mispredict tens. So **hoist invariants out of loops** (don't
  recompute what doesn't change), fold constants, strength-reduce (shift / multiply over divide), and keep
  inner loops **branch-predictable and allocation-free**.
- **Lay data out for the cache.** Contiguous, hot-fields-together (SoA where it pays), sequential access
  over pointer-chasing — table-storage iteration is cache-friendly *because* of this (§2). Access pattern
  beats raw instruction count at a million agents; the §1A no-copy rules serve this directly.
- **Let the compiler do the work, then verify the win.** Prefer iterator chains (they elide bounds checks
  and vectorize) over manual indexing; give monomorphization and inlining a chance; avoid div / sqrt / heap
  alloc / virtual dispatch / syscalls in the innermost loop. Confirm the result on the flamegraph or in the
  asm (Compiler Explorer / `cargo-show-asm`) — **never assume.**

**Serial work is traffic in a lane — fewer cars, then more lanes.** A mental model that unifies both
disciplines: picture every run of *dependent* statements — each waiting on the result of the one before —
as a single lane of road, and each operation on it as a car. *Independent* lines get their own lanes and
flow side by side; *dependent* ones queue nose-to-tail in one, and latency is when the last car on the
longest lane — the **critical path** — clears. So the moves, strongest first, are: **take cars off the
road** (work you never do is free — why §1A's no-copy and §1B's YAGNI pay double on a hot path); **shorten
the lane** (fewer dependent steps on the critical path); then **widen to more lanes** by breaking false
dependencies so independent work runs concurrently — at the macro scale on Bevy's parallel executor
(Rule 8), and at the micro scale where a superscalar core issues independent instructions to separate
ports in one cycle and SIMD carries several elements per lane. "The less traffic, the better" is the right
instinct, with one caveat the profiler enforces: it is traffic *per lane on the critical path*, not raw
line count, that sets latency — splitting one dependency chain into two independent accumulators *adds*
instructions yet finishes sooner, because two lanes now flow where one was jammed. So never serialize for
"simplicity" (Rule 8), and don't tune by counting lines — count the cars that must cross the line one
behind another, and clear that lane.

**The two scales together — and the guard-rail.** Get the **order** right *everywhere* (cheap, universal,
correctness at scale); spend **instruction-level** effort *only* where measurement says it lands. On cold
paths, readability wins (§1B) — a clever constant-factor trick that obscures cold code is a regression, not
a win. The best solution is the **lowest-order one, with the fewest and cheapest instructions, that a
teammate can still read.**

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
15. **Bring full effort and invention** — the disposition in **§1D** is law too: finish the whole problem
    (root cause, every call site, the error paths), experiment boldly but isolate and measure your spikes,
    and route every exception through an **ADR** rather than a quiet divergence. Laziness and timidity are
    both out of bounds.
16. **Reduce the order, then the instruction count** — **§1E** is law: know the big-O (time and space) of
    every loop and call and pick the lowest order for the `n` it runs over (**quadratic-over-agents does
    not ship**); then, on profiled hot paths only, minimize instruction count and cost — hoist invariants,
    cache-friendly layout, branch-predictable, no div / alloc in inner loops. Cold paths stay readable.

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
- **Rust:** `cargo build` and the **`cargo nextest run`** runner in `Rust/` build and **headlessly test**
  the simulation — no editor required; this is where determinism/replay tests live. nextest does **not**
  run doctests, so also run **`cargo test --doc`**. Toolchain: nightly, `x86_64-pc-windows-msvc`; the
  linker is **rust-lld** (the bundled LLVM linker, wired in `Rust/.cargo/config.toml` for the msvc
  target). Tighten the inner loop with **`bacon`** or **`cargo watch`**, and prefer a single targeted
  test (`cargo nextest run -p <crate> <name>`) over the whole suite for speed.
- **Unsafe-boundary gates (FFI crates).** For `logi_ffi` and `geo/logi_geo_ffi`, run
  **`cargo +nightly miri test`** over their unsafe Rust paths and review the unsafe surface with
  **`cargo geiger`** (§1C). Optimize only after measuring: **criterion** benchmarks for hot game-loop
  paths, **samply** / **cargo-flamegraph** to find them (§1B).
- **FFI acceptance test.** An FFI change is not done until a **UE → Rust → UE** round trip (e.g. an editor
  call that reaches Rust and logs back) **links and runs in the editor with no LNK errors.**
- **CI** (`.github/workflows/rust.yml`, headless, Linux host target) currently gates the workspace on
  `cargo fmt --check`, `cargo clippy -- -D warnings`, and `cargo test`. *Planned hardening:* move to
  **nextest**, add **cargo-deny** (licenses/bans/advisories), **cargo-audit** (RUSTSEC), a scheduled
  **cargo-udeps** (unused deps), and a **miri** job for the FFI crates. The rust-lld flag is msvc-only, so
  it does not affect the Linux CI build.
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
smuggling an exception into a commit. Default to **finishing the job over leaving it half-done**, and to
the **bold-but-lawful** option over the timid one (§1D). Default to the **lower-order algorithm** and the
data structure that makes the hot operation cheap (§1E). If a choice only works at a hundred agents, it is
the wrong choice.
