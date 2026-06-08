# Research + Design — UE 5.8 Frame Profiling & the Vast-rs Benchmarking Utility

> **Status:** Research complete; design proposed. **P0 (`logi_prof` crate) built + verified; P1 (divan
> harness + the instrumentation-overhead bench) landed.** P2 (instrument the real phases), P3 (FFI
> channel), P4 (C++ bridge) await sign-off.
> **Date:** 2026-06-06.
> **Scope:** (1) a deep map of Unreal Engine 5.8 per-function frame profiling, (2) the current profiling
> state of Vast on both sides of the FFI, (3) the design of a Rust-side benchmarking/profiling utility
> and how it bridges into Unreal's existing tools — **without Rust ever calling Unreal** (ADR-0005 §1).
> **Feeds:** [Docs/Performance/Budgets.md](../Performance/Budgets.md) (replaces its `TODO: profile`
> placeholders) and [Docs/Performance/Profiling-Checklist.md](../Performance/Profiling-Checklist.md).
> **Governed by:** [ADR-0012](../Architecture/ADR-0012-Poll-Based-FFI-Decoupled-Sim-Thread.md) (the
> decoupled sim/stream threads this utility must see) and AGENTS.md §1 / §1B ("measure first").

---

## 0. The one-paragraph answer

Unreal 5.8 profiles per-function CPU time with **Unreal Insights** consuming a **Trace** stream
(`-trace=cpu,frame,counters,...`), where `TRACE_CPUPROFILER_EVENT_SCOPE` scopes appear as nested timer
events **on the track of the OS thread that emitted them, timestamped at call time**. That last clause is
the whole problem for us: Vast's heavy work runs on **Rust-owned threads** (`LogiGeoStream`,
`LogiBevyCompute` pool) that Unreal did not create, so it is **structurally invisible** to Insights — today
the editor sees `logi_world_tick` as one opaque block and the entire Geo stream thread not at all. The fix
is a three-layer utility: **(A)** offline `cargo` microbenchmarks for true per-function isolation timing;
**(B)** an always-on, lock-free, in-process Rust instrumentation layer (`logi_prof`) that aggregates
per-phase/per-system timings on the Rust threads; **(C)** a poll-only FFI channel that the C++ bridge drains
each game-thread frame and re-emits as **UE trace counters + `stat` + CSV**, so Rust timings land on the
*same Insights timeline* as the game/render/GPU threads. **(D)** For full per-function flamegraphs of the
Rust threads, link Tracy **into Rust only** (UE keeps Insights) and correlate by frame index — co-running
UE's Tracy plugin and Rust's `tracy-client` in one process is a version-locked, two-singletons trap.

---

## Part I — Unreal Engine 5.8 frame profiling (the research)

### I.1 The stack: Trace → Unreal Insights

UE's modern profiler is **Trace** (the instrumentation/transport layer, module `TraceLog`) feeding
**Unreal Insights** (`Engine/Binaries/Win64/UnrealInsights.exe`, the analyzer/UI). Data is a `.utrace`
file (LZ4-framed) or a live TCP session. Insights' **Timing Insights** window is the per-function view:
one **track per thread**, a **GPU** track, a **Frames** track (per-frame bar graph), and a **Counters**
track. Each timer event's width is its wall time; the **Callers/Callees** panels give inclusive vs
exclusive time per timer. ([Timing Insights][ti], [Trace in UE5][trace], [Unreal Insights][ui])

**Enabling it.** Channels are selected at launch via `-trace=` or toggled at runtime:

| Mechanism | Form |
|---|---|
| Command line | `-trace=cpu,frame,bookmark,counters,gpu,stats,task,log` (plus `-statnamedevents` to give every `stat` a named CPU event) |
| Route to file | `-tracefile=Foo.utrace` |
| Route to a running store | `-tracehost=127.0.0.1` (Insights Session Browser running) |
| Runtime console | `Trace.Start` / `Trace.Stop` / `Trace.Enable cpu,counters` / `Trace.SnapshotFile` |
| Legacy stat capture | `stat startfile` / `stat stopfile` (already named in our Profiling-Checklist) |

Default-on channels: **Bookmark, Cpu, Frame, Gpu, Log, Region, Screenshot**. `Counters` and `Stats` are
**opt-in** — we must add them to `-trace=` for our counters to record. ([Trace in UE5][trace])

### I.2 The four instrumentation surfaces (and which we use)

UE exposes per-function timing four different ways; they target different windows and have different costs.

1. **CPU profiler scopes** — `TRACE_CPUPROFILER_EVENT_SCOPE(Name)` / `..._STR("literal")` /
   `..._STR_CONDITIONAL`. Header `Runtime/Core/Public/ProfilingDebugging/CpuProfilerTrace.h`. RAII; emits a
   begin/end pair on the **current thread**, timestamped from `FPlatformTime::Cycles64()`. Renders as nested
   timers in Timing Insights. Internally `FCpuProfilerTrace::OutputBeginEvent / OutputBeginDynamicEvent /
   OutputEndEvent` (the `*Dynamic*` form interns a runtime string; the literal form is cheaper). **This is
   the primary per-function timer.** ([rick.me.uk][rick], [dev guide][devtrace])

2. **`stat` cycle counters** — `DECLARE_STATS_GROUP(TEXT("LogiSim"), STATGROUP_LogiSim, STATCAT_Advanced)`
   + `DECLARE_CYCLE_STAT(...)` + `SCOPE_CYCLE_COUNTER(...)`. Shows in the **`stat LogiSim`** console overlay
   *and* (with `-statnamedevents`) in Insights. Value counters: `DECLARE_DWORD_ACCUMULATOR_STAT` +
   `INC_DWORD_STAT`, `DECLARE_FLOAT_COUNTER_STAT` + `SET_FLOAT_STAT`. ([Tom Looman][looman])

3. **Trace counters** — `TRACE_DECLARE_INT_COUNTER(Id, TEXT("Logi/Geo/ResidentTiles"))` /
   `TRACE_DECLARE_FLOAT_COUNTER` + `TRACE_COUNTER_SET / _ADD / _INCREMENT / _SUBTRACT`. Header
   `CountersTrace.h`. Plots on the Insights **Counters** track, time-aligned to frames. Cheapest way to get
   a *time-series scalar* onto the unified timeline. ([Tom Looman][looman], [dev guide][devtrace])

4. **CSV profiler** — `CSV_DEFINE_CATEGORY(LogiSim, true)` + `CSV_CUSTOM_STAT(LogiSim, DecodeMs, V,
   ECsvCustomStatOp::Set)` and `CSV_SCOPED_TIMING_STAT(...)`. Emits a per-frame `.csv` (started with
   `-csv=` / `CsvProfiler.Start`). This is the **headless / CI / batch** lane — no UI, machine-diffable,
   perfect for regression dashboards. ([UE CSV profiler docs][csv])

Plus two cross-cutting helpers: **bookmarks** `TRACE_BOOKMARK(TEXT("LOD swap"))` (`MiscTrace.h`) drop
vertical markers on every track for correlation; **named events** `SCOPED_NAMED_EVENT` show up in external
GPU/CPU profilers (PIX/RenderDoc/Razor) too.

### I.3 Defining *custom* trace events (the lever for advanced integration)

UE lets you author your own trace event types — relevant to the stretch goal of putting Rust spans on a
dedicated Insights track. The `TraceLog` API: `UE_TRACE_CHANNEL(MyChannel)`, then

```cpp
UE_TRACE_EVENT_BEGIN(LogiProf, RustSpan)
    UE_TRACE_EVENT_FIELD(uint32, NameId)
    UE_TRACE_EVENT_FIELD(uint64, BeginCycle)
    UE_TRACE_EVENT_FIELD(uint64, EndCycle)
    UE_TRACE_EVENT_FIELD(uint32, ThreadClass)
UE_TRACE_EVENT_END()
```

emitted with `UE_TRACE_LOG(...)` or — crucially — **`UE_TRACE_LOG_SCOPE_T`, which takes an explicit
timestamp** for event correlation. Field types include the scalars we need plus `Widestring`/`Ansistring`
and arrays. Event flags `NoSync` (cheaper) and `Important` (kept in the persistent buffer). Consuming a
custom event in Insights requires a **`TraceServices` `IAnalyzer` + `IProvider`** pair and (for a visual
track) an editor-side contributor — i.e. a small Insights plugin. ([dev guide][devtrace])

### I.4 The thread-attribution constraint (decisive)

A CPU timer event is attributed to **the OS thread that calls the macro, at the moment it calls it**. UE
auto-creates a Timing track for any thread that emits, naming it from the thread registry. **Threads UE did
not create do not get a track unless they themselves emit UE trace events** — and to do so they would have
to call into the engine (forbidden for us, ADR-0005). There is no public macro to emit a *backdated* CPU
event *attributed to another thread*. Consequence:

- Cesium-for-Unreal's async tile work *does* appear on worker tracks — but only because those workers are
  **engine task-graph threads**. ([Cesium profiling][cesium])
- Vast's `LogiGeoStream` and `LogiBevyCompute` threads are **foreign** to UE → they will **never**
  auto-appear, no matter what `-trace=` flags we set.

So the unified timeline cannot be achieved by "just instrument the Rust threads with UE macros." It is
achieved by **(C) counters** (sampled on the game thread → inherently frame-aligned) for the always-on
view, and optionally **(D1) a custom analyzer track** replaying timestamped Rust spans for the deep view.

### I.5 The clock

UE on Windows: `FPlatformTime::Cycles64()` is backed by **`QueryPerformanceCounter`** (QPC); the Trace
clock is derived from it. Rust `std::time::Instant` on Windows is **also** QPC. They read the **same
hardware counter** with different epochs/scaling — so a shared epoch is recoverable: at init the C++ bridge
pushes `(qpc_origin, qpc_freq)` to Rust (a one-directional *push*, allowed), and Rust stamps spans in that
domain. For the **counters** path no alignment is needed at all (the value is read on the game thread inside
the frame). For the **D1 replay** path the shared epoch is required and **must be validated empirically** —
QPC virtualization/RDTSC scaling differences are exactly the kind of thing AGENTS.md §1B says to measure,
not assume. ([QPC][qpc])

### I.6 Tracy and UE 5.8

UE has a Trace→Tracy path (community `TracyUnrealPlugin`, built on UE's *external profiler API*, enabled
with `-tracy`, statically linking a Tracy client into the engine). ([TracyUnrealPlugin][tup]) The trap:
**Tracy's client is a per-binary singleton.** If UE statically links one copy and the Rust `tracy-client`
crate links another into `logi_geo.dll`, the process has **two clients fighting for port 8086 and two
disjoint zone stores**. The documented remedy is one shared Tracy DLL that both link with `TRACY_IMPORTS`,
in **exact protocol-version lockstep** — and `tracy-client` explicitly requires you to pin the Tracy
version. ([Tracy DLL guidance][tracydll], [rust_tracy_client][rtc]) **Verdict: do not co-run.** Use Tracy
as a **Rust-only** profiler (UE uses Insights); for the cdylib-in-host case configure `tracy-client` with
`manual-lifetime` + `delayed-init` + `ondemand`.

---

## Part II — Vast's current profiling state

### II.1 Rust side (mapped from the workspace)

- **Two cdylibs.** `logi.dll` (`logi_ffi → logi_runtime → logi_sim`, gameplay) and `logi_geo.dll`
  (`logi_geo_ffi → logi_3dtiles_scene …`, Geo). Both link `bevy_ecs`/`bevy_tasks` 0.18.1 and `mimalloc`.
- **Gameplay is still synchronous on the game thread.** `logi_world_tick`
  ([logi_ffi/src/lib.rs:267](../../Rust/crates/logi_ffi/src/lib.rs#L267)) runs the whole schedule —
  `FixedSim`(`integrate_motion`) → `RepresentationPlan`(`compute_significance` → `allocate_representation_budget`)
  → `RenderExtract` — then publishes the triple-buffered `SnapshotRing`. The ADR-0012 sim-thread decouple is
  **not done for gameplay** yet. Two executor modes: `SingleThreadedDeterministic`, `BoundedParallel`
  ([schedules.rs:47](../../Rust/crates/logi_sim/src/schedules.rs#L47)).
- **Geo is already decoupled.** `LogiGeoStream` thread owns a bevy_ecs World; `select` + `drain_fetches` run
  as pre-schedule `GeoWorld` methods, then the schedule chains
  `sse_cull_lru(par, batch 1024) → decode(par, batch 1) → evict → publish`; handoff via a `Mutex` latest-wins
  mailbox + per-poll `Arc<TileGeometry>` clone. Poll FFI `logi_globe_world_set_view` / `_poll`.
- **par_iter sites:** `integrate_motion`, `compute_significance` (gameplay); `decode`, `sse_cull_lru` (Geo).
- **Existing counters (incidental, not a harness):** `SnapshotRing.dropped_frames`; `GeoStats`
  (resident, decoded_this_step, evicted_this_step, decode_failed_total, mesh_cache_hits_total,
  fetches_cancelled_total); stream-thread atomics (produced, resident_tiles, in_flight, cached_bytes,
  polled).
- **No benchmarking infrastructure at all** — no `criterion`/`divan`/`puffin`/`tracy`; no `benches/`; no
  `Instant`-timing on hot paths. `tracing` is a workspace dep used **only for error logging**.
- **Workspace `[profile.release]`:** `panic="abort"`, `lto="thin"`, `codegen-units=1`.

### II.2 C++ side (mapped from `Plugins/LogiBevy`)

- **`ULogiBevySubsystem::TickSimulation`** (TG_PrePhysics) → `logi_world_tick`, wrapped in **one**
  `TRACE_CPUPROFILER_EVENT_SCOPE(LogiBevy_SimTick)`.
- **`ULogiBevyRepresentationSubsystem::TickRepresentation`** (TG_PostPhysics) → `get_render_snapshot` +
  `buffer_release`, wrapped in `LogiBevy_RepresentationApply`; ISM upload in `LogiBevy_IsmUpload`.
- **`ALogiBakedGeoTerrain::Tick`** (~30 Hz) → `logi_globe_world_set_view/_poll` or `logi_baked_world_select`
  — **no profiling scope at all.**
- One `RETURN_QUICK_DECLARE_CYCLE_STAT` on `ULogiBuildingSubsystem`. **No** CSV, **no** custom stat group,
  **no** trace counters, **no** Tracy.
- Targets are Game + Editor, `BuildSettingsVersion.V7`, **no** explicit trace/CSV/Tracy flags. Project has
  the **MCP server plugin enabled** (`ModelContextProtocol`) — its `console_command` can drive `stat`/`Trace.*`.

### II.3 The gap, stated precisely

Today Insights shows **three game-thread scopes and one black box** (`logi_world_tick`). The dominant CPU
work in the project — Geo decode/rebase/select on `LogiGeoStream`, and (after the ADR-0012 gameplay
decouple) the sim schedule on the sim thread — is **invisible**, on both the Insights timeline and the
`stat` overlay. Budgets.md is **almost entirely `TODO: profile`** because there has been no instrument to
fill it. This utility's job is to make the Rust threads measurable, offline and live, and to put their
numbers where the rest of the frame already is.

---

## Part III — The benchmarking / profiling utility (design)

Four layers, each independently useful, building on each other. Layers A–C are the always-on backbone;
D is the deep-dive.

```
  ┌─────────────────────────────────────────────────────────────────────────────┐
  │  A. cargo microbenchmarks (divan)         — headless, per-function isolation  │
  │     `cargo bench`, CI-gated, scaling sweeps 1k→1M agents / tile counts        │
  ├─────────────────────────────────────────────────────────────────────────────┤
  │  B. logi_prof  — in-process, lock-free, always-on (feature `prof`)            │
  │     profile_scope!("…") on the Rust threads → per-frame per-phase aggregate   │
  │        │                                          │                            │
  │        ▼ (feature `tracy`)                        ▼ drain_frame()              │
  │  D. Rust-only Tracy flamegraph              C. FFI poll channel               │
  │     (per-function, all Rust threads)          logi_prof_poll(&out)            │
  └───────────────────────────────────────────────────────│─────────────────────┘
                                                            ▼  (game thread, per frame)
                          C++ ULogiProfilingSubsystem  →  TRACE_COUNTER_SET  (Insights Counters track)
                                                       →  SET_FLOAT_STAT     (`stat LogiSim`/`LogiGeo`)
                                                       →  CSV_CUSTOM_STAT     (headless CSV / CI)
                                                       →  TRACE_BOOKMARK      (LOD swaps, frame drops)
```

### A. Offline microbenchmarks — `divan`

**Why divan over criterion.** The benches that matter here are **scaling sweeps** — `integrate_motion` and
`compute_significance` at 1k/10k/100k/1M agents, `decode` at varying vertex counts, `select` at varying tree
depth. Divan's `#[divan::bench(args = […])]` expresses these natively, it has lower per-sample overhead and
CI-stable sample scaling, and it reports throughput. ([Divan][divan]) Criterion's edge is bootstrapped
confidence intervals / built-in regression deltas; if we want that for the CI gate we can run criterion on
the handful of *stable* hot fns and divan for the sweeps, or use CodSpeed (supports both). **Recommendation:
divan as the single tool**, with a committed-baseline JSON + a compare script in CI.

Benches to author (one `benches/*.rs` per crate, `harness = false`):

| Crate | Bench | Measures | Parameterization |
|---|---|---|---|
| `logi_sim` | `sim_scaling` | `integrate_motion`, `compute_significance`, `allocate_representation_budget` | agents = [1k,10k,100k,1M] × {single, parallel} |
| `logi_runtime` | `snapshot` | `extract_render_snapshot`, ring `publish`/`find`/`release` | entities = [1k…1M] |
| `logi_3dtiles_scene` | `decode` | `decode_glb` + folded rebase; `sse_cull_lru` math | verts/tile = [1k…200k]; tiles = [64…4096] |
| `logi_3dtiles_select` | `select` | tileset tree traversal + SSE refine | depth/breadth sweep |
| `logi_qmesh` / `logi_gltf` | `codec` | quantized-mesh parse, glTF decode | payload size |

These give the **literal "per-function execution time"** the request centers on, in isolation, deterministic,
and gate-able: wire into the existing CI workspace gate (which already runs fmt+clippy+test) so a regression
past a threshold fails the build. They also validate the **big-O claims** AGENTS.md §1E demands (the
single-vs-parallel sweep is the 1M-agent scaling proof).

### B. In-process instrumentation — the `logi_prof` crate

A new tiny crate `Rust/crates/logi_prof`, depended on by `logi_sim`, `logi_runtime`, `logi_render_extract`
and the Geo crates. **Zero cost when the `prof` feature is off** (the macro expands to nothing) — honoring
the "zero shipping cost" posture (ADR-0006 spirit).

**Hot-path primitive** — a literal-name scoped timer, no allocation, no contention:

```rust
// usage at a phase/system boundary (NOT per-entity):
pub fn decode(/* … */) {
    logi_prof::profile_scope!("geo.decode");
    // … par_iter_mut decode …
}
```

**Mechanics (lock-free, bounded, allocation-free after warmup):**
- Names are `&'static str`, interned **once** to a dense `u32` id at first touch (a one-time `OnceLock`
  insert); the hot record stores the id, never the string.
- Each thread owns a **double-buffered sample ring** (`[Vec<Sample>; 2]`, preallocated). `profile_scope!`'s
  guard reads a monotonic counter at enter/drop (`minstant`/`quanta` TSC, falling back to `Instant`) and
  pushes `{id, begin, end}` into the *active* buffer — a thread-local push, **no atomics on the hot path**.
- A thread registers its rings in a global registry (`Mutex<Vec<Arc<ThreadSink>>>`) on first emit.
- Once per frame the **owner thread** (sim/stream loop) calls `logi_prof::end_frame()`: it atomically flips
  each sink's active-buffer index (seqlock-style — the producer re-reads the index once per frame), folds
  the now-inactive buffers into a **per-frame aggregate** keyed by id (count, inclusive ns, exclusive ns via
  a tiny scope stack, max ns), and publishes it into a double-buffered `ProfileFrame` the FFI reads. This
  mirrors the existing `SnapshotRing` discipline (publish/retire, one consumer) the codebase already trusts.

**Volume is bounded by design:** `profile_scope!` wraps *phases/systems/pool-batches* (~dozens of records
per frame), never per-agent bodies, so the MPSC/aggregate cost is O(spans), not O(entities). The `decode`
par-batches can emit one record per batch (work-steal granularity) — still bounded by batch count.

**Counters** ride the same frame: `logi_prof::counter("geo.resident_tiles", n)` folds scalars
(`dropped_frames`, `resident`, `in_flight`, `decode_failed`, pool depth) into the same `ProfileFrame`.

**Why custom, not `tracing` spans:** `tracing`'s span machinery (dynamic fields, dispatch) is heavier than a
literal-id push and we have hot scopes; the custom macro keeps the enter/exit to a counter read + a push.
(`tracing-tracy` remains available for the D-layer; see below.) This is a deliberate "measure first" choice
and is itself benchmarked in layer A.

### C. The FFI poll channel → Unreal's tools

**The boundary stays one-directional (ADR-0005 / ADR-0012): Unreal pulls; Rust never pushes.** Profiling is
just another retained, poll-only POD channel — diagnostic data, no Unreal types cross.

**ABI additions** (bump `logi_ffi` v6→v7, `logi_geo_ffi` v5→v6; cbindgen regenerates the headers):

```rust
#[repr(C)] pub struct LogiProfSpan {
    pub name:          *const c_char, // interned 'static, null-terminated
    pub thread_class:  u32,           // 0=GameFFI 1=Sim 2=GeoStream 3=Worker
    pub call_count:    u32,
    pub inclusive_ns:  u64,
    pub exclusive_ns:  u64,
    pub max_ns:        u64,
}
#[repr(C)] pub struct LogiProfCounter { pub name: *const c_char, pub value: f64 }
#[repr(C)] pub struct LogiProfFrame {
    pub frame_id:      u64,
    pub owner_wall_ns: u64,           // total owner-thread frame time
    pub span_count:    u32,
    pub spans:         *const LogiProfSpan,
    pub counter_count: u32,
    pub counters:      *const LogiProfCounter,
}
// Copy-out (caller-owned buffer + capacity) — profiling volume is tiny, so we avoid the
// retained-pointer lifetime dance entirely. Returns LOGI_OK / LOGI_ERR_*.
#[no_mangle] pub unsafe extern "C" fn logi_prof_poll(
    world: *mut LogiWorld, out: *mut LogiProfFrame,
    span_cap: u32, span_buf: *mut LogiProfSpan,
    counter_cap: u32, counter_buf: *mut LogiProfCounter,
) -> u32;
// geo mirror: logi_globe_world_prof_poll(...). Gated by a LOGI_FLAG_PROFILING init flag
// AND the `prof` feature → shipping with the flag off pays nothing.
```

**C++ side — `ULogiProfilingSubsystem`** (new, or fold into the existing subsystems). Once per frame, on the
game thread, after the existing tick/poll:

```cpp
DECLARE_STATS_GROUP(TEXT("LogiSim"), STATGROUP_LogiSim, STATCAT_Advanced);
CSV_DEFINE_CATEGORY(LogiProf, true);

void ULogiProfilingSubsystem::Drain() {
    TRACE_CPUPROFILER_EVENT_SCOPE(LogiBevy_ProfDrain);
    LogiProfFrame F; /* + stack/heap span+counter buffers */
    if (logi_prof_poll(World, &F, Cap, Spans, Cap, Counters) != LOGI_OK) return;
    for (uint32 i = 0; i < F.span_count; ++i) {
        const FName N(F.spans[i].name);
        const double Ms = F.spans[i].inclusive_ns / 1.0e6;
        TRACE_COUNTER_SET(/* dynamic */ N, Ms);           // Insights Counters track
        SET_FLOAT_STAT_FName(N, Ms);                       // `stat LogiSim`
        FCsvProfiler::Get()->RecordCustomStat(N, CSV_CATEGORY_INDEX(LogiProf), Ms, ECsvCustomStatOp::Set);
    }
    for (uint32 i = 0; i < F.counter_count; ++i)
        TRACE_COUNTER_SET(FName(F.counters[i].name), F.counters[i].value);
}
```

Result: every Rust phase shows up **three ways** — live on the Insights **Counters** track (frame-aligned to
the game/render/GPU timeline), in the **`stat LogiSim` / `stat LogiGeo`** PIE overlay (drivable through the
MCP `console_command`), and in **CSV** for headless/CI runs. Plus we finally wrap `set_view`/`poll`/`release`
and the snapshot-apply loops in their own `TRACE_CPUPROFILER_EVENT_SCOPE` so the *game-thread* FFI cost
decomposes (today only `logi_world_tick` is scoped, and the Geo actor tick is not scoped at all).

> Note on dynamic counter/stat names: UE's counter/stat macros prefer compile-time `TEXT` ids. For a fixed
> known set of phases we declare them statically (best UI). For the fully-dynamic set we use the
> `FName`-keyed runtime variants; **verify the exact runtime-counter API in-engine before relying on it** —
> do not ship a guessed signature (Budgets.md anti-guessing rule).

### D. Per-function depth on the Rust threads

**D2 — Rust-only Tracy (recommended).** `logi_prof` with `--features tracy` routes every `profile_scope!`
to a Tracy zone (via `tracy-client`) and marks frames at the sim/stream loop boundary
(`Client::frame_mark()`). Run PIE; connect a **Rust-dedicated** Tracy GUI. You get the full per-function
flamegraph across `LogiGeoStream` and every `LogiBevyCompute` worker — the work Insights can't see —
including lock waits and the par_iter batch distribution. Pin the Tracy version (`tracy-client` requires
it); configure `manual-lifetime` + `delayed-init` + `ondemand` for the cdylib-in-host lifecycle (clean PIE
start/stop, Live-Coding reloads). **Do not** also enable UE's `-tracy` plugin (the two-singleton/port trap,
§I.6). Correlate Tracy↔Insights by frame index + matching `TRACE_BOOKMARK`s.

**D1 — custom Insights track (stretch / R&D).** A `TraceServices` analyzer plugin that ingests a
`UE_TRACE_LOG_SCOPE_T` custom event carrying Rust span begin/end in the **shared QPC epoch** (§I.5) and lays
them on synthetic `LogiSim` / `LogiGeoStream` tracks *inside the same Insights capture* as UE. This is the
only way to get true per-function Rust scopes on the **one** unified timeline. It is real engineering (a
custom analyzer + provider + track contributor) and version-sensitive; **document it, prototype only after
A–C land and only if the counters view proves insufficient.**

---

## Part IV — Phased build plan

| Phase | Deliverable | Touches | Risk |
|---|---|---|---|
| **P0 ✅** | `logi_prof` crate: `profile_scope!` / `counter!`, per-frame aggregate (inclusive+exclusive ns, by name×thread-class), `prof` feature off-by-default (zero cost when off); unit tests. Built + verified (fmt / clippy `-D warnings` both feature states / tests). | Rust only (new crate) | low — no ABI/editor |
| **P1 ◑** | divan harness wired + `overhead` bench (measure-first, below). **Remaining:** sim-scaling / snapshot / decode / select benches (need small `pub`/test-helper exposure in those crates) + CI baseline gate. | Rust only | low |
| **P2** | Instrument the real phases: `profile_scope!` in the sim schedule phases + the Geo pipeline systems; counters from existing telemetry | Rust only | low |
| **P3** | FFI channel: ABI bump, `logi_prof_poll` (+ geo mirror), cbindgen headers, `LOGI_FLAG_PROFILING` | **ABI** (versioned) | med — coordinate ABI |
| **P4** | C++ `ULogiProfilingSubsystem`: drain → trace counters + `stat LogiSim/LogiGeo` + CSV; scope the Geo actor tick & FFI calls | **C++ bridge** (editor rebuild) | med — live editor / Live Coding |
| **P5** | `tracy` feature (layer D2): Rust-only Tracy flamegraph | Rust only (opt feature) | low |
| **P6** *(stretch)* | D1 custom Insights analyzer track | UE plugin | high — defer/justify |
| **P7** | Replace `Budgets.md` `TODO: profile` with measured numbers, each citing a capture | Docs | low |

Build order respects risk: P0–P2 are pure Rust (no editor coordination — important given Live Coding
rewrites C++ mid-session). P3–P4 touch the versioned ABI and the running editor and should land together
behind the `prof` feature + `LOGI_FLAG_PROFILING` so shipping pays nothing.

### Measured so far — instrumentation overhead (P0/P1)

The §1B "measure first" check, so we know the always-on cost *before* instrumenting hot paths. Capture:
`cargo bench -p logi_prof --features prof` (`crates/logi_prof/benches/overhead.rs`), release build, this
host (medians):

| Operation | Median cost |
|---|---|
| `profile_scope!` enter+exit (`scope_enter_exit`) | **≈ 54 ns** |
| `counter!` set (`counter_set`) | **≈ 8 ns** |
| `frame_with_scopes`: N=1 → 256 | 300 ns → 14.9 µs ⇒ **≈ 57 ns / scope** marginal + **≈ 250 ns** fixed `end_frame` drain |

**Verdict:** ~100 instrumented phase scopes/frame ≈ **< 6 µs**, i.e. ~0.03 % of a 16.6 ms frame — always-on
instrumentation is affordable, so layer B can stay on in dev/profiling builds without distorting the budget
it measures. (The `end_frame` per-frame map allocation is the first thing to pool if drain cost ever
matters at very high span counts.) With `prof` off the same bench measures the no-op path ≈ 0.

## Part V — Invariants this utility must preserve

- **One-directional boundary** (ADR-0005 §1): the profiling channel is a *pull*; Rust never calls Unreal.
  Clock-epoch push (§I.5) is the only Unreal→Rust data and it is plain scalars at init, not a callback.
- **Determinism** (ADR-0001/0012): instrumentation must not perturb the fixed-step sim's logic or step
  count; `profile_scope!` is side-effect-free w.r.t. World state and is compiled out in golden/replay runs.
- **Owner-thread invariant** (§11): `logi_prof_poll` is a read from the game thread like the snapshot read;
  the aggregate is published by the owner thread (sim/stream), one producer.
- **Zero shipping cost:** `prof` feature off → macro is a no-op; `LOGI_FLAG_PROFILING` off → FFI returns
  empty. No always-on overhead in a shipping build.
- **Measure first / no guessed facts** (§1B, Budgets.md): the divan sweeps validate the big-O; the
  in-engine counter/stat API signatures and the QPC epoch are **verified in-engine** before being trusted.

## Part VI — Open decisions (for sign-off)

1. **Build scope this session** — P0–P2 (pure-Rust foundation) only, vs through P4 (full always-on
   Insights/stat/CSV integration, needs an editor rebuild).
2. **Microbench tool** — divan (recommended) vs criterion vs both.
3. **Unified per-function timeline** — accept counters + Rust-only Tracy (recommended), or commit to the
   D1 custom Insights analyzer track (high effort).

---

## Sources

- Timing Insights — Epic: <https://dev.epicgames.com/documentation/unreal-engine/timing-insights-in-unreal-engine-5> [ti]
- Trace in Unreal Engine 5 — Epic: <https://dev.epicgames.com/documentation/unreal-engine/trace-in-unreal-engine-5> [trace]
- Unreal Insights reference — Epic: <https://dev.epicgames.com/documentation/unreal-engine/unreal-insights-reference-in-unreal-engine-5> [ui]
- Developer Guide to Tracing — Epic: <https://dev.epicgames.com/documentation/en-us/unreal-engine/developer-guide-to-tracing-in-unreal-engine> [devtrace]
- C++ Profiling in UE5 — rick.me.uk: <https://www.rick.me.uk/posts/2024/12/cpp-profiling-in-unreal-engine-5/> [rick]
- Counters/Traces/Stats — Tom Looman: <https://tomlooman.com/unreal-engine-profiling-stat-commands/> [looman]
- Cesium for Unreal — Insights profiling: <https://cesium.com/learn/cesium-unreal/ref-doc/performance-profiling-unreal-insights.html> [cesium]
- CSV profiler (Test & Optimize) — Epic docs [csv]
- QueryPerformanceCounter — Microsoft Learn: <https://learn.microsoft.com/en-us/windows/win32/sysinfo/acquiring-high-resolution-time-stamps> [qpc]
- TracyUnrealPlugin: <https://github.com/Nesquick0/TracyUnrealPlugin> [tup]
- Tracy-in-DLL guidance — wolfpld/tracy issue #26 & manual: <https://github.com/wolfpld/tracy/issues/26> [tracydll]
- rust_tracy_client: <https://github.com/nagisa/rust_tracy_client> [rtc]
- Divan: <https://nikolaivazquez.com/blog/divan/> and <https://docs.rs/divan> [divan]
