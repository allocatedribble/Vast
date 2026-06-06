# CLAUDE.md

**Read [`AGENTS.md`](AGENTS.md) first. It is the architectural constitution for this project and it
governs every change you make.** This file adds Claude Code–specific operational notes; it does not
override `AGENTS.md`.

## The one rule that matters most

**Rust Bevy ECS owns runtime gameplay truth. Unreal is the renderer / animator / host.** Delivery /
agent / route / economy / shipment / terrain state lives in the Bevy `World` (Components + Resources),
mutated by Systems — **in Rust**. `AActor`, `UObject`, components, Blueprints, widgets, and Mass
fragments are shells: they mirror Rust-owned state and capture player intent, and never own truth.
Unreal calls Rust across a narrow C ABI; **Rust never calls Unreal; no Unreal types cross the boundary.**
See [`AGENTS.md`](AGENTS.md) § 1 and [ADR-0005](Docs/Architecture/ADR-0005-Bevy-ECS-Authority.md).

## Bring full effort and invention

Two dispositions are out of bounds on this project: **laziness** and **timidity** — see
[`AGENTS.md`](AGENTS.md) § 1D for the law. In practice:

- **Don't be lazy — finish the whole job.** Solve the actual problem, not its nearest symptom: fix the
  cause, then sweep for its siblings; handle the edge cases and error paths, not just the happy path. Read
  the code that proves a claim before you make it, and verify a task (build / run / test) before calling it
  done. If answering honestly needs ten files or a throwaway experiment, do that.
- **Be creative — experiment in the name of progress.** This is a frontier build; propose the bold
  approach, prototype rival designs, and let measurement decide. Spikes are cheap and welcome — keep them
  isolated, benchmark against the budgets, then land the winner at production grade or delete it.
- **Ambition respects the law, it doesn't dodge it.** Creativity is a better solution *inside* the
  constraints (§1 architecture, the §1A craft bar, YAGNI) — or a proposed **ADR** when the constraint
  itself is what's blocking progress, never an exception smuggled into a commit. When bold and simple
  disagree, the tie-breaker is the North Star: pick what still works — and still ships — at a million agents.

## Reduce the order, then the instruction count

Efficiency is a design criterion here, not an afterthought — see [`AGENTS.md`](AGENTS.md) § 1E.

- **Know the big-O of every loop and call you write** (time *and* space) and the `n` it runs over. At a
  million agents and a streamed planet, `O(n²)` over the agent set does not ship — reach for the hash set,
  the spatial index, the sort-then-sweep, the precomputed lookup, and pick the data structure that makes
  the hot operation `O(1)` / `O(log n)`. This is correctness at scale, not premature optimization.
- **On profiled hot paths, minimize both the count and the cost of instructions.** Real latency is
  `instructions × cycles + stalls`, and at scale the stalls (cache misses, branch mispredicts) dominate —
  so hoist invariants out of loops, lay data out contiguously for the cache, keep inner loops
  branch-predictable and allocation-free, and avoid div / sqrt / alloc / virtual dispatch there. Let the
  compiler vectorize (iterator chains over manual indexing) and *verify the win* on the flamegraph or in
  the asm — never assume.
- **Picture serial code as lanes of traffic — less traffic flows faster.** Each chain of *dependent*
  lines (each waiting on the one before) is one lane and its operations are cars; latency is when the
  last car on the longest lane — the *critical path* — clears. So take work off the road entirely (the
  cheapest car is the one you never drive — §1A / YAGNI), shorten the critical path (fewer dependent
  steps), then widen to more lanes by breaking false dependencies — across systems (the parallel
  executor; never serialize for "simplicity") and within a core (a superscalar issues independent
  instructions side by side, SIMD several per lane). The caveat the profiler enforces: it's traffic *per
  lane*, not raw line count, that sets latency — two independent accumulators can *add* instructions yet
  finish sooner than one jammed chain.
- **Right time for each.** Get the *order* right everywhere (cheap, universal); spend *instruction-level*
  effort only where the profiler points (§1B — measure first). On cold paths, readability wins. The best
  solution is the lowest-order one, with the fewest and cheapest instructions, that a teammate can still
  read.

## Architecture decision records (read before designing)

- [ADR-0005 — Bevy ECS Authority](Docs/Architecture/ADR-0005-Bevy-ECS-Authority.md) — **the current law.**
  Bevy ECS (Rust) owns gameplay; supersedes the Mass-authority parts of ADR-0001.
- [ADR-0006 — Editor Inspectability](Docs/Architecture/ADR-0006-Editor-Inspectability.md) — editor-only
  inspection shells (virtual Outliner rows + one selection proxy) over the Rust world; zero shipping cost.
- [ADR-0007 — Actorless Rust Terrain](Docs/Architecture/ADR-0007-Actorless-Rust-Terrain.md) — **supersedes
  the terrain-host-actor clause of ADR-0005.** No `AActor` per tile; Rust owns terrain truth, Unreal owns
  renderer shells (Nanite/Lumen/VSM participants) only.
- [ADR-0008 — Terrain Nanite Lane](Docs/Architecture/ADR-0008-Terrain-Nanite-Lane.md) — **refines ADR-0007's
  Milestone-E clause.** Runtime Nanite encode is infeasible in stock UE 5.8 (editor-only encoder); the
  production lane is an **editor-only** Nanite `UStaticMesh` built via the engine path. Ship needs an
  offline bake or a fast static-relevance proxy.
- [ADR-0009 — Native Rust 3D Tiles Stack](Docs/Architecture/ADR-0009-Native-Rust-3DTiles-Stack.md) —
  **Milestone F.** A native Rust OGC 3D Tiles / real-world-terrain stack (tile selection +
  glTF/quantized-mesh decode in Rust; reuses the kernel core; one vendored-Draco FFI exception; streams
  from Cesium ion). Implements ADR-0007's Cesium provider; plan in
  [Docs/Plans/Milestone-F-3DTiles-Rust-Stack.md](Docs/Plans/Milestone-F-3DTiles-Rust-Stack.md).
- [ADR-0011 — Semantic Tile Enrichment](Docs/Architecture/ADR-0011-Semantic-Tile-Enrichment.md) — **offline
  bake** that turns real-world data into our own enriched tiles (terrain / trees / foliage / water /
  param-procedural buildings) over the ADR-0009 path. ML stays offline; the runtime stays lean; render data
  is non-authoritative (promote to coarse for gameplay, ADR-0007). Subsumes the reserved ADR-0010 (foliage).
  Plan in [Docs/Plans/Hero-Location-Semantic-Enrichment.md](Docs/Plans/Hero-Location-Semantic-Enrichment.md).
- [ADR-0012 — Poll-Based FFI / Decoupled Sim Thread](Docs/Architecture/ADR-0012-Poll-Based-FFI-Decoupled-Sim-Thread.md) —
  **amends ADR-0005's owner-thread.** The FFI `step` becomes a `poll`: the gameplay sim and the Geo stream
  loop advance on **Rust-owned threads**, and Unreal (on its game thread) only pushes view/input and pulls
  the latest retained snapshot. Geo runs fully async (ADR-0007); gameplay stays fixed-step deterministic.
  Plan in [Docs/Plans/Async-Sim-Decoupling-and-Geo-Performance.md](Docs/Plans/Async-Sim-Decoupling-and-Geo-Performance.md).
- [ADR-0001 — Mass-First Unreal](Docs/Architecture/ADR-0001-Mass-First-Unreal.md) — **superseded in part**
  by ADR-0005. Its Determinism section and the "Unreal is a shell" principle still hold.
- [ADR-0002 — Rust Terrain Kernel](Docs/Architecture/ADR-0002-Rust-Terrain-Kernel.md) — terrain truth in
  Rust; now generalized by ADR-0005.
- [ADR-0003 — Representation & Simulation LOD](Docs/Architecture/ADR-0003-Representation-LOD.md) — unified
  significance score (still in force; the sim side is now Bevy).
- [ADR-0004 — No Compatibility Code](Docs/Architecture/ADR-0004-No-Compatibility-Code.md) — UE 5.8 only;
  `bevy_ecs` as a library, not full Bevy.

## Performance docs
- [Docs/Performance/Budgets.md](Docs/Performance/Budgets.md) · [Docs/Performance/Profiling-Checklist.md](Docs/Performance/Profiling-Checklist.md)

## Environment & tooling

- **Engine:** Unreal Engine 5.8 (`LogiUE.uproject`). Mass plugins (`MassGameplay`, `MassAI`) remain
  enabled **only** as a transitional, non-authoritative bridge — do not build gameplay on them.
- **Rust:** cargo/rustc 1.97 nightly, host `x86_64-pc-windows-msvc`. The simulation lives in `Rust/` and
  is built/tested with `cargo` (headless, no editor). `bevy_ecs` only — never the full `bevy` engine.
- **C ABI header:** generated by `cbindgen` from `logi_ffi` and checked in.
- **Unreal↔Rust:** `Plugins/LogiBevy` builds/links the Rust artifact via `LogiBevyRuntime.Build.cs`.
- **MCP bridge:** a **running** Unreal Editor exposes MCP tools at `http://localhost:3000/mcp`. Treat it
  as a shared live session; prefer non-destructive operations. New modules/plugins need a from-scratch build.
- **Platform:** Windows; use PowerShell syntax in the shell.

## Git conventions

- Git LFS for binary assets; ignores all build output (`Binaries/`, `Intermediate/`, `Saved/`,
  `DerivedDataCache/`, Rust `target/`, generated `.sln/.slnx`).
- Commits use the GitHub `noreply` email (set repo-locally) — do not "fix" it back to a private address.
- Commit or push **only when asked**.

## House style

- **Rust** gameplay code: idiomatic `bevy_ecs`; deterministic systems; seeded RNG.
- **C++** bridge code: Unreal conventions (`F`/`U`/`A`/`E` prefixes, `UPROPERTY`/`USTRUCT`/`UCLASS`),
  narrow and heavily profiled; no gameplay authority.
- No Mass/Unity vocabulary in gameplay code (migration notes only).
- When a task would violate `AGENTS.md`, surface it and propose an ADR instead of quietly diverging.
