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

## Cells within cells, interlinked — *precedent*

When shaping an algorithm or abstraction, hold the baseline: **"cells within cells, interlinked."**
This is established precedent for how structure is chosen here — the recurring winning shape in this
codebase is a nested hierarchy of bounded, self-similar cells, coupled only through narrow, explicit
links. Constitutional home: [ADR-0019](Docs/Architecture/ADR-0019-Cells-Within-Cells-Interlinked.md)
(this section is its operational mirror; the ADR governs on any divergence).

- **Cells within cells — nest, don't flatten.** Prefer the self-similar hierarchy: tiles within
  subtrees within the tile tree (ADR-0009/0018); individuals within flows within the statistical far
  field (ADR-0015); components within entities within archetypes within the `World`; tasks within lanes
  under one arbiter (ADR-0014). Each cell is bounded — it owns its invariants, its budget, and its
  working set — and the same shape repeats at every scale. Nesting is what turns `O(n)` into
  `O(log n)` / `O(active)`: a flat scan over a planet or a million agents does not ship.
- **Interlinked — couple through narrow, explicit links.** Cells touch only at deliberate seams: the
  C ABI boundary (ADR-0005/0012), `Promoted<T>` between determinism lanes (ADR-0016), parent↔child
  graft and clip links in the tile tree, the retained snapshot between sim and renderer. The link *is*
  the contract; everything behind it stays private. A design whose cells reach into each other's
  interiors is entangled, not interlinked — redraw it.
- **The ladder — run the lens at both ends, then climb.** Hold every abstraction at its *lowest* rung
  (bytes in a 32-byte vertex, bits folded into a key, one chain of dependent instructions) and — in
  parallel — at the *highest* rung in view (threads, processes, the Rust sim ↔ Unreal renderer
  organism). The two ends discipline each other: the bottom shows which shape actually repeats; the top
  shows what the containment must serve. Then use both views together to draw the cell **one level
  above** the current highest — the container not yet named, whose absence shows up as sibling cells
  describing the same truth with no link between them — and repeat from there. The climb closes only at
  the North-Star cell: the shipped game at a million agents. An abstraction is settled when the same
  cells-interlinked shape reads true at *every* rung; a rung where it doesn't is the next piece of work.
  (The method is itself self-similar: each rung of analysis is a cell, interlinked with the rungs above
  and below it.)
- **The bi-tree optimization rule — optimize both ends for each other, simultaneously, independently.**
  The hierarchy is a tree with two ends; an optimization pass runs at both at once, each end shaped FOR
  the opposite end: the bottom for the top (the 32-byte vertex makes the upload one flat memcpy; the
  incremental path key makes every map above probe O(1)), and the top for the bottom (the poll-FFI keeps
  per-tile work off the game thread; the working-set law keeps per-step instructions O(active)). The two
  passes are independent lanes (§1E) that meet only at the link between them — a change at one end that
  forces re-opening the other end's interior means the link is mis-drawn, not that the passes must fuse.
  An optimization is finished only when both ends have been asked "what does the opposite end now make
  cheap that wasn't before?" — one-ended optimization leaves half the win on the table. (ADR-0019 § 4.)
- **Superposed interiors; deliberate alphabets.** A cell's interior need not hold a definite value —
  the cheapest faithful state is often a *distribution over an alphabet* (a statistical aggregate, a
  seed, an unbaked derivation) collapsed only when an observation crosses a link, by a deterministic
  keyed rule so the measurement replays (the kick and Sim-LOD promotion are both collapse events;
  store-the-amplitude is the third option beside store-the-value and compute-on-demand). And the
  alphabet itself is a design axis — the bit is its degenerate two-letter case; identities are words
  (the path key), validity sets are grammars (availability), and every branching factor or tier count
  is a radix choice to argue, not inherit. Classical distributions only: compression + deferred
  choice are the harvest, never a claimed speedup. (ADR-0019 §§ 8–9; ADR-0023 § 7 — fidelity tiers
  are an observer-indexed alphabet.)
- **The design test.** Before building, name the cell (the bounded self-similar unit), the cell that
  contains it, and the links it exposes — then run the ladder through it. If the design can't be drawn
  as cells within cells, interlinked — if it's one flat sweep, or a web of cross-references with no
  hierarchy — that is the signal to find the structure first. New work follows this precedent or
  argues, in an ADR, why it can't.

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
- [ADR-0013 — Terrain Static Relevance + GPU-Scene](Docs/Architecture/ADR-0013-Terrain-Static-Relevance-GPU-Scene.md) —
  **implements ADR-0008's static-relevance option.** The proc-mesh tile proxy renders via static relevance
  (cached draw commands, GPU-Scene + GPU culling) instead of per-frame dynamic `FMeshBatch` rebuilds; the
  `Mesh.LODIndex = INDEX_NONE` renders-nothing gotcha is documented inside.
- [ADR-0014 — LogiRuntime: Unified Executor + Reactor](Docs/Architecture/ADR-0014-LogiRuntime-Unified-Executor-Reactor.md) —
  **generalizes ADR-0012 into one runtime.** Two cooperating lanes (work-stealing CPU + `compio` completion
  I/O) under one core arbiter, joined at the frame poll; compose the proven cores, build only the coordinator
  (arbiter + frame-harvest + the morsel/precomputed-stage CPU engines). Completes ADR-0012's gameplay
  decoupling; arbitrates ADR-0005's shared pool (physical-core budget); narrows ADR-0004 (a vendored
  `bevy_ecs` patch). Plan in [Docs/Plans/LogiRuntime-Design.md](Docs/Plans/LogiRuntime-Design.md).
- [ADR-0015 — Simulation-LOD](Docs/Architecture/ADR-0015-Simulation-LOD.md) — **extends ADR-0003 to govern
  whether an entity is simulated at all.** Active set bounded by **budget, not population**; statistical
  far field (one ODE per flow, O(#flows)); deterministic keyed-seed promotion. The only path past the
  ~10–30 M memory-bandwidth wall toward billions; necessitates ADR-0014's precomputed-stage executor.
- [ADR-0016 — Per-Lane Determinism](Docs/Architecture/ADR-0016-Per-Lane-Determinism.md) — **refines ADR-0001's
  determinism to per-lane scope.** A bounded deterministic near-set + a free non-deterministic far field,
  bridged by a typed `Promoted<T>`; counter-based RNG. Non-determinism is almost never the win — "no shared
  state" is, and determinism gets it too.
- [ADR-0017 — GPU-Resident Statistical Offload](Docs/Architecture/ADR-0017-GPU-Resident-Statistical-Offload.md) —
  **deferred / dark by default.** Warm/Cold bands resident in a Rust-owned `wgpu` device, shared read-only to
  UE via an OS handle; amends ADR-0005 by splitting **authority** (Rust keeps) from per-individual
  **reproducibility** (forfeited on GPU). Gated on a measured bandwidth-bound profile.
- [ADR-0018 — Geo Streaming Parallelism](Docs/Architecture/ADR-0018-Geo-Streaming-Parallelism.md) — **the Geo
  step is bounded by the active working-set, not the entity pool.** Drive every per-tile pass off a maintained
  live-set / explicit transition set (not a `par_iter` scan of the pre-spawned pool), event-drive
  `reconcile_index` to O(Δ), run the (fully-chained) Geo schedule SingleThreaded, and decouple decode off the
  stream-thread critical path (measure-gated). Per-step Geo work becomes O(active + Δ), never O(pool); refines
  ADR-0014 (the bevy *system* executor is idle in Geo by design — the lever is the working-set, not the DAG).
  From the round-4 research ([Docs/Research/2026-06-09-ECS-Round4-Pipeline-Fork-GPU-Research.md](Docs/Research/2026-06-09-ECS-Round4-Pipeline-Fork-GPU-Research.md)).
- [ADR-0019 — Cells Within Cells, Interlinked](Docs/Architecture/ADR-0019-Cells-Within-Cells-Interlinked.md) —
  **the structural precedent** (constitutional home of the section above). Every abstraction = bounded,
  self-similar cells nested hierarchically, coupled only through narrow explicit links; the both-ends
  ladder is the design procedure; cross-hierarchy links carry their own refresh discipline; the kick ≈
  Sim-LOD promotion (one pattern, two rungs). First enforcement 2026-06-11: the `GeoStreaming` sub-cell
  decomposition + the unified telemetry block (ABI v16) + the perspective-pinned stream view.
- [ADR-0020 — logi_ecs: the Adopted ECS Fork](Docs/Architecture/ADR-0020-LogiEcs-Incremental-World-Cell.md) —
  **the ECS is ours.** bevy_ecs 0.19.0-rc.2 adopted as `Rust/crates/logi_ecs` (+ `logi_ecs_macros`);
  divergences ledgered in `crates/logi_ecs/FORK.md`. Refactor law = ADR-0019 applied to the World itself:
  a cell bounded by budget, interior maintained O(Δ) (never flat sweeps), interlinked by narrow typed
  links. Consumers still write `use bevy_ecs::…` (dependency key; the crate SYMBOL stays `bevy_ecs`
  until D-paramset — a rustc ParamSet hack is keyed on the name).
- [ADR-0021 — Sim-Tiles: Temporal Simulation-LOD](Docs/Architecture/ADR-0021-Sim-Tiles-Temporal-Simulation-LOD.md) —
  **extends ADR-0015 with a second refinement axis: time.** A sim-cell's timestep is a resolution
  (frame → day → **never**: seed + lazy fast-forward — determinism-from-seed is the compression scheme;
  O(observed), never O(elapsed)). The sim-cell contract: bounds (space, population, dt band, budget) +
  an **exported error metric** (the program's SSE — designed first) + conserving flux links (settle on
  the coarse cadence) + a bounded commitments ledger + replay-stable hazard triggers (keyed RNG —
  promotion is observer-driven AND event-driven). Cross-shard links carry the coarser cadence (tick
  rate = latency tolerance). Entity count unbounded — a million agents is the floor, never a baked-in
  cap. Re-scopes R3/R4/R5 of ADR-0020's program; build slots live there.
- [ADR-0022 — LogiSky: the Rust-Modeled Atmosphere](Docs/Architecture/ADR-0022-LogiSky-Rust-Modeled-Atmosphere.md) —
  **remakes the stock SkyAtmosphere.** Rust owns the sky model (parameters, sun, WGS84 — single-homed
  with the terrain geodesy) and BAKES the LUTs; UE is a thin GPU shell. Aerial perspective is analytic
  from ELLIPSOID depth, never scene depth — the tile quilt + orbital froxel checkerboard become
  unrepresentable. Event-gated cadence; renderer-agnostic `LogiSkyFrame` + LUT buffers generalize the
  ECS render abstraction. Absorbs Fix B; retires the v15 anchor on completion. **Scope: ALL atmospheric
  rendering** (sky/AP/clouds/haze/twilight — one cell, no second atmospheric system). Plan in
  [Docs/Plans/LogiSky-Atmosphere-Remake.md](Docs/Plans/LogiSky-Atmosphere-Remake.md).
- [ADR-0023 — One Truth, Many Formulas](Docs/Architecture/ADR-0023-One-Truth-Many-Formulas.md) —
  **the resolution-ladder precedent, brother to ADR-0019** (0019 = the structure axis, 0023 = the
  evaluation axis). Every continuously-evaluated quantity = ONE parameter truth + a FAMILY of formulas
  at declared (cost, error) tiers — the exact/reference form always in-repo as the test oracle, baked
  and analytic tiers with TESTED bounds (agreement gates), seamless handoffs inside overlapping
  validity domains, joint coverage of every scale (orbit → on-foot). Generalizes ADR-0003/0015/0021 to
  rendering; first enforcement = logi_sky's fidelity tiers + cross-tier agreement gate.
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
  is built/tested with `cargo` (headless, no editor). The ECS is **our fork** `logi_ecs`
  (`Rust/crates/logi_ecs`, ADR-0020) consumed under the `bevy_ecs` dependency key — never the full
  `bevy` engine.
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
