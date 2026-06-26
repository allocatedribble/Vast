# Vast — ECS Round-4 Research Report: Pipeline-Parallelism, Fork Levers, and the CPU/GPU Saturation Frontier

> **Provenance.** Produced 2026-06-09 by a 17-agent Opus workflow (8 research facets → per-facet adversarial
> verification → synthesis; ~1.9 M agent tokens). The load-bearing claims were then **independently
> re-verified against the live source by the main session**: `SigIndex` is dead code (only `lib.rs:9` +
> its own tests reference it); `DECODE_BATCH = 1` (`geo_world.rs:602`); `advance_streaming` is a 7-stage
> serial spine (`geo_world.rs:1420-1428`); `allocate_representation_budget` runs two O(P) passes and does
> NOT use `SigIndex` (`systems.rs:110-191`). **Line numbers are approximate** — the running UE editor /
> Live Coding rewrites these files mid-session, so anchors drift by a few lines; trust the symbol names.
>
> **Scope.** This report synthesizes 8 research facets and their adversarial verdicts into one ranked, O-notation-grounded plan. It **extends** ADR-0014 (LogiRuntime executor+reactor), ADR-0015 (Simulation-LOD), ADR-0016 (per-lane determinism), and ADR-0017 (GPU-resident offload) — it does **not** restate them. Every load-bearing claim is tagged **[CONFIRMED]**, **[CAVEATED]**, or **[REFUTED]** per the verify pass. Code anchors were re-verified against the working tree (`geo_world.rs:1435-1443`, `systems.rs:110-191`, `schedules.rs:155-158`, `geo_world.rs:1267`, and the `SigIndex` dead-code grep).
>
> **The North Star tie-breaker used throughout:** works *and* ships at a billion agents. That ruthlessly demotes anything whose win is a control-plane constant (scheduler wakeups, dispatch objects) and promotes anything that removes **traffic** (the bandwidth wall) or removes **population from the per-frame critical path** (the O(P) → O(K+Δ) reductions).

---

## 1. Executive summary — the 8 highest-value novel conclusions

1. **The single cheapest large win is wiring code we already wrote.** ADR-0015's proven O(K+Δ) significance index (`sig_index.rs`, the 64× spike: 8511 µs → 133 µs at 1 M) is **dead code** — referenced by zero systems; production `allocate_representation_budget` still does a full O(P) scan + `select_nth` *plus a second O(P) write pass* every frame. **[CONFIRMED]** ~1 day to wire. This is a wiring task, not research.

2. **The Geo "schedule" is a red herring; the real hot path is a 7-stage serial *method chain* (`advance_streaming`)** in which the multi-threaded system executor is **genuinely unused** — all parallelism is intra-system `par_iter`. **[CONFIRMED]** But the headline "3-stage software-pipeline → 2-3× throughput" is **[REFUTED]** as a magnitude: the stages share one `&mut GeoStreaming` struct, the stream thread is *I/O-latency-bound* (a deliberate 1 ms `SHORT_PARK` per step during streaming), and decode dominates the cheap stages. Realistic win ≈ **1.1-1.4× latency-hiding**, not throughput. The *correct* takeaway survives: put executor effort **above** the schedule.

3. **The two hot gameplay kernels sit on opposite sides of the roofline, and the project was about to apply the wrong lever to each.** `integrate_motion` is bandwidth-bound at **AI ≈ 0.07 flop/byte (~218× left of the Zen4 ridge)** — SIMD and more cores are near-useless; only **traffic reduction** (fusion + non-temporal stores) helps. **[CONFIRMED]** `compute_significance` and `rebase_gltf` are genuinely **compute-bound** — and `rebase_gltf` runs **scalar f64** because glam's `DVec3`/`DMat4` have no SIMD backend, leaving ~3-4× of vector width on the floor. **[CONFIRMED, magnitude CAVEATED to 3-4× not 4-8×]**

4. **The decode `Changed<FetchedBytes>` filter is O(pool), and at `DECODE_BATCH=1` it spawns ~24,576 tasks every stream step** even when nothing changed — the dominant cost is task-spawn, not the tick scan. A non-fork watermark check (public `Table::get_changed_ticks_slice_for`) eliminates both. **[CONFIRMED, amplified beyond the original claim]** But it lives on the stream thread, not the bandwidth-bound gameplay loop — a real Geo-lane win, not a billions lever.

5. **The precomputed-stage executor does NOT require a bevy_ecs fork.** The conflict graph (`ScheduleGraph::conflicting_systems()`) and dependency DAG are fully `pub` at build time; the fork's *only* unique value is reading the same data *in-band* inside the executor. **[CONFIRMED]** This demotes ADR-0014's "vendored patch" premise to "optional ~6-line micro-opt with a non-fork fallback." For Vast's current S=3-8 systems the win is ~0% regardless.

6. **A fuller GPU-ECS is NOT-YET necessary and NEVER necessary as the World-of-record** — but the crossover number in one facet was wrong by ~2 orders of magnitude. The correct net-win crossover for a *branch-free, no-readback* kinematic band is **~6×10⁴ resident agents**, not 1-5 M. **[REFUTED→corrected]** This makes ADR-0017's *narrow* offload attractive ~50-100× sooner than believed, while the *broad* GPU-ECS verdict stands for the right reason: the gameplay active set is **readback-bound and branch/determinism-bound, independent of N**.

7. **ADR-0015's statistical far field and a GPU-resident ECS are rival answers to the same wall — and the O(#flows) aggregate wins by 3-6 orders of magnitude.** **[CONFIRMED, with the caveat that Little's law alone is too weak for spatial congestion — but a per-segment PDE is still O(#segments) ≪ O(agents)].** Conclusion: never hold 10⁹ agents resident for *gameplay*; the GPU's only residual role is a bounded *visible-swarm render hint*.

8. **The strongest near-term GPU win in the entire codebase is Geo decode/rebase — a fixed-function compute kernel with ZERO readback — not any ECS.** **[CONFIRMED, magnitude codec-dependent]** It is also the *cheapest, lowest-risk* way to de-risk the wgpu↔D3D12 shared-handle interop that ADR-0017 depends on, because the decoded mesh never crosses PCIe back to the CPU. **Caveat:** wgpu buffer *export* is currently unsupported public API (PR #6161 is texture-only, import-only) — worse than ADR-0017 states.

---

## 2. Geo pipeline-parallel redesign (the headline) — what to actually build

### 2.1 The verified ground truth

`advance_streaming` (geo_world.rs:1435-1443) runs **strictly serially on one stream thread**:

```
drain_raw → refresh_session_if_needed → select_request → advance(schedule)
          → reconcile_index → prune_cold_grafts → bound_failure_sets
```

and `advance()` runs `(sse_cull_lru, decode, evict, publish).chain()` (geo_world.rs:1267) — every stage a barrier. **The bevy multi-threaded system executor is therefore idle in Geo; 100% of Geo parallelism is intra-system `par_iter`.** This is the single most important structural fact in the facet set and it is **[CONFIRMED]** against source.

**O-notation of the current spine** (n = tiles selected/decoded per step, d = per-tile decode cost):
```
frame latency = O(traverse_visited  +  reconcile_index_O(index)  +  n·d  +  evict  +  publish)   [all serial]
```

### 2.2 Where the original "2-3× pipeline" claim breaks (intellectual honesty)

The verify pass **[REFUTED]** the 2-3× magnitude on three grounded points; the redesign must respect all three:

- **Shared mutable state, not independent operators.** Every stage borrows `&mut GeoStreaming` (the `index: HashMap<TilePath,Entity>`, `in_flight`, `subtree_cache`, `sel_state`, the tree root) *and* the bevy World, with a **tight cross-frame producer/consumer cycle**: `select_request` issues fetches into `in_flight` (frame N) that `drain_raw` consumes (frame N+1); `reconcile_index`/`prune_cold_grafts` mutate the very index `select_request` reads. A naïve double-buffer must snapshot `index + tree + sel_state` = **+O(resident + tree-nodes) of structural state per in-flight frame**, plausibly ≥ one tile's decode — so the "cheap orchestration layer" framing is false.
- **Decode dominates the cheap stages**, so software-pipeline steady-state throughput `1/max(wᵢ)` only hides the *cheap* traversal behind decode → `1/(1 − w_select/W)` ≈ **1.1-1.4×**, not 2-3×.
- **The thread is I/O-latency-bound exactly when decode is non-empty:** `stream_thread.rs` parks 1 ms (`SHORT_PARK`) after every step while `in_flight > 0`, and network fetch is already on a *separate* ureq pool. The CPU `max(stage)` the pipeline exposes sits *under* the park it overlaps.

### 2.3 The redesign worth building — ranked by verified payoff

Rather than the full 7-stage pipeline, the evidence points to a **smaller, decoupled-stage** plan. Build in this order:

**(P-1) Decouple decode as detached pool tasks + harvest-next-step [CONFIRMED feasible, no fork].**
Make the owner thread do only the cheap tree/index-mutating stages; hand the embarrassingly-parallel `decode` (Draco + `rebase_gltf`) to the shared `ComputeTaskPool` as detached tasks whose `Arc<TileGeometry>` results land in a flume queue that `drain_raw` harvests at the top of the next step — *reusing the existing `poll_completed` completion-queue pattern* (geo_world.rs). The owner thread still does the cheap *write* (`mesh.0 = Some(arc); state = Resident`) exactly as `deliver_bytes` already does. **Determinism-safe** (Geo is non-authoritative, ADR-0007). Needs an in-decode flag (mirror `in_flight_paths`) so a >1-frame fat tile isn't re-enqueued.
- **O:** removes `n·d` from the owner-thread critical path → owner thread = `O(traverse + reconcile + evict + publish)`; decode runs concurrently in the pool. Steady-state `O(max(cheap_stages, n·d))`, +O(n) queue, +2 frames per-tile latency.
- **Win:** real on a **burst** of large concurrent decodes that briefly exceeds the 1 ms park (hero-origin first descent / fast camera over cold terrain). Near-zero on a settled camera (decode batch empty) or a steady I/O-bound stream. **Measure-gate** on a moving-camera flamegraph: if per-step decode wall-time < SHORT_PARK + RTT, defer.

**(P-2) Fix the unconditional O(pool) costs that the pipeline does NOT remove [CONFIRMED, higher certainty than P-1].** These pay *every* step and are cheaper to land:
- **`reconcile_index` is O(index)/step** (`for (p,&e) in &s.index`, geo_world.rs:2516+) even when no transitions occurred (it early-outs *after* the full scan). **Event-drive it**: push evicted/decode-failed paths into a queue at the two transition sites (evict already frees the slot; decode-fail is the only other producer) → **O(Δ_transitions)**. ~0.5 day, low risk.
- **`decode`'s `Changed<FetchedBytes>` is O(pool) and at `DECODE_BATCH=1` spawns ~24,576 tasks/step** (verified: `BatchingStrategy::fixed(1)` → `step_by(1)` over the whole pool). Drive decode from the already-maintained `NewResidents`/just-fetched set via random-access `tiles.get(e)` (like `evict`/`publish` do) → **O(Δ_fetched)** with **zero task spawns on a no-change step**. The dominant saving is the ~24 K task-spawns (~1 ms of pure dispatch on an active step), not the ~10 µs tick scan. ~hours.
- **`sse_cull_lru` is O(pool) with a non-trivial per-row body** (distance + screen-space-error, `SSE_BATCH=1024`) and has **no `Changed` gate at all** — this is the *heavier* O(pool) scan. Drive it from `ResidentSet ∪ in-flight` (a contiguous `Vec<Entity>`, cache-friendly, chunked) → **O(active)**. ~1-2 days. **This is the more material half** of the "pool tax" finding — fixing decode without fixing this is a rounding-error win. **[CONFIRMED]**

**(P-3) Morselize the `rebase_gltf` per-vertex loop *inside* the decode body, and SIMD-vectorize the affine apply** (see §4) — only for atypically dense tiles; gate by `prim.positions.len()`. The across-tile `par_iter` already saturates the pool for typical P3DT leaves. **[CONFIRMED small/conditional]**

**(P-4) Hoist the synchronous `fs::write` off the decode worker [CONFIRMED feasible].** `decode` runs `gc.put_raw(key, serialize_geometry(&arc))` = a blocking `fs::write`+rename on the pool worker (the audit comment already names a writer thread as "the proper fix"). A flume-fed writer thread removes a syscall from the decode critical path on **cold** terrain only (warm tiles hit the `contains()` guard). ~hours, measure-gated.

**(P-5, research-only) Level-synchronous BFS-front traversal** to collapse the selection critical path from O(visited) to ~O(depth + n/P). **[CONFIRMED novel vs cesium-native, CONFIRMED high-risk]** This is genuine new ground (cesium-native's selection is explicitly serial-recursive; no public 3D-Tiles runtime parallelizes the *select* walk). But the finding itself concedes traversal is **almost certainly not the current bottleneck**, the correctness surface (anti-pop kick, hysteresis dead-band, ADD/REPLACE graft inheritance, contiguous-suffix truncate) is subtle and stateful, and the three `is_loaded`/`is_dead`/visibility closures must become `Send+Sync`. **Build only behind a feature flag with the existing DFS as a differential oracle, and only after a flamegraph shows `select_request` > ~1% of the stream frame.** Insurance for the billions-of-tiles deep-tree regime, not a current win.

### 2.4 The negative result that prunes wasted effort [CONFIRMED]

**There is NO Rust-side texture decode or BC7 compression on the stream thread.** Cargo.lock has no image/jpeg/png/ktx2/basis crate; `logi_gltf` ships raw encoded bytes; `rebase_gltf` only `mem::take`s them; the FFI ships raw JPEG (`logi_geo_ffi.h:95`); `CompressBlocksBC7` (Intel ISPC) lives entirely in C++ `LogiBakedGeoTerrainActor`, dispatched to a UE worker. **The Rust decode budget is Draco + the f64 ENU rebase, period.** Do not chase a Rust texture-parallelization lane — it does not exist.

---

## 3. bevy_ecs fork-augmentation roadmap (beyond the precomputed-stage executor)

Ranked by **(impact × hot-path-ness) / effort**, with the verify-pass verdict and build order. **The meta-finding: the precomputed-stage executor itself does not need a fork** (§1.5), and *nothing* on this list helps gameplay until it has thousands of systems (Exp C: scheduler overhead < 0.5% of a 33 ms frame until then). So the executor levers are deliberately ranked *below* the storage/change-detection levers.

| Rank | Lever | Mechanism | O-notation | Fork? | Verdict | Build order |
|---|---|---|---|---|---|---|
| **F1** | **Per-Column change-tick watermark** (max changed-tick/Column) | `Changed<T>` is `IS_ARCHETYPAL=false` → per-row `is_newer_than` over *every* matched row (filter.rs:1080, iter.rs:263-281). A `max_changed: Tick` per Column, O(1) on write, lets the iterator skip an unchanged table in one compare. bevy issue #5097, unmerged. | O(N) → O(t + Δ), t=matched tables. +4B/Column/table. | **Tier-1 NO fork** (public `get_changed_ticks_slice_for`); Tier-2 fork for in-loop maintenance | **[CONFIRMED]** | **Tier-1 now** for Geo decode (§2.3 P-2). Tier-2 only if a 2nd consumer appears. |
| **F2** | **Hot/cold split of `Transform3d`** (24B translation column + 28B orientation/scale column) | One `BlobArray`/Column → today's 56B AoS-in-column drags rotation+scale through cache on translation-only passes. Split → 24B stride-1 column, unblocks SIMD on the compute-bound kernel. | integrate bytes 56→24/row (~2×, bandwidth); significance gets SIMD 3-4×. +1 Column header. | **NO fork** (separate ComponentIds → already non-conflicting) | **[CONFIRMED]** | **High** — prerequisite for §4 fusion + NT stores + significance SIMD. Clean-break rebase of FFI/render extract (ADR-0004). |
| **F3** | **SIMD change-scan in the morsel engine** (non-fork) | Read `get_changed_ticks_slice_for` + `get_data_slice_for` (both `pub`), run a branchless `is_newer_than` sweep → dense changed-row index, then process only those. | O(N) branchy → O(N) SIMD scan + O(N_changed) work. | **NO fork** | **[CONFIRMED]** | Medium — generalizes F1 to any sparse-subset gameplay near-set system. |
| **F4** | **Tick-free band columns** (drop the 8B added+changed Tick arrays on kinematic band components) | Every `Column` unconditionally allocates `added_ticks + changed_ticks` (8B/component/entity, column.rs:27-28); no opt-out exists; `check_change_ticks` is an O(C_total) periodic world scan. | -8B/component/entity RSS; deletes those columns from the periodic stall. | **Fork-only on Table** — but the **morsel SoA path already sidesteps it with zero fork** | **[CONFIRMED-with-caveats]** | **Low** — bounded by the *active near-set* (~0.24-0.72 GB at 10-30M, not 24 GB at billions; the far field has no rows). The verify pass **refuted** the "cache-line pollution on integrate" sub-claim (ticks are a *separate* allocation, not interleaved with payload). The no-fork SoA escape (ADR-0014 Option B) dominates. |
| **F5** | **Precomputed-stage executor** (custom `impl SystemExecutor`) | Compile conflict graph → barrier-separated stages once. | O(stages) barriers vs stock MT O(S) wake loop. | **Optional ~6-LOC fork** (Tier-1 no-fork via `set_executor` + `Schedule::systems()` correlation) | **[CONFIRMED — fork demoted to micro-opt]** | **Lowest** — ~0% win at S=3-8; revisit only past hundreds of systems (#11378 knee ~470 µs at S=1000). |
| **F6** | **Hot-executor spin-park + dependency-batched signaling** | `async-executor` `notify()` does an OS futex/SetEvent wake (~1-3 µs) only when no worker is hot; the `notified` atomic short-circuits to a CAS when one is searching. Spin-then-park keeps it hot. | O(E) wakes → O(E) CAS, ~0 OS wakes steady-state. | Vendored async-executor / LogiExec | **[CONFIRMED-with-caveats]** | **Lowest** — bevy's finisher *self-ticks* and pops the dependent next, so queues stay full mid-system and OS-wakes are already rare. Dependency-batching is *already done* by bevy. Insurance for a wide future gameplay graph only. |
| **F7** | **Field-range (column-range) access conflicts** | Widen the conflict key from ComponentId to (ComponentId, byte-range) so disjoint-field writers run concurrently. | O(k)→O(1) critical-path collapse for a k-disjoint-writer stage. | **Heavy fork** (unsafe disjoint-`&mut`-into-one-Column aliasing proof) | **[CONFIRMED zero benefit today]** | **Do not build.** Every gameplay phase is single-writer or `.chain()`ed (schedules.rs:155-158); Geo is fully chained. The **hot/cold split (F2) already achieves field-disjointness with no fork.** |
| **F8** | **Incremental cached query views** (`IncrementalView<Filter>`) | Generalize Geo's hand-rolled `ResidentSet`/`NewResidents`/`TexturedSet` into one dirty-driven materialized view, driven by the F1 watermark. | O(Δ) maintenance vs O(n) rebuild; consumers O(r). | NO fork | **[novel-synthesis, not adversarially tested]** | Medium — YAGNI until a 2nd consumer (ADR-0015's active-set is that consumer). Makes the "ResidentSet drifted" bug class (the reason the O(pool) `debug_assert` exists) structurally impossible. |

**The one fork that is actually worth it soon: F1 Tier-1 (no fork) for Geo, then F2 (no fork) for gameplay.** Neither requires patching bevy_ecs. The first *genuine* fork candidate is F1 Tier-2, and only if a second `Changed`-driven consumer materializes.

---

## 4. CPU 100%-useful saturation playbook — the right lever per kernel

The decisive insight, **[CONFIRMED]** by the in-codebase morsel bench (1.57 ns/agent serial → 0.9 ns/agent at 8 threads = only **1.7×**): the two hot kernels are on opposite sides of the roofline ridge, and **"use SIMD + all cores" is the wrong instinct for the central gameplay loop.**

### 4.1 Per-kernel roofline classification

| Kernel | Arithmetic intensity | Regime | Right lever | Wrong lever (does NOT help) |
|---|---|---|---|---|
| **`integrate_motion`** (systems.rs:26 / morsel.rs:108) | **~0.07 flop/byte** (6 flops / ~84 B, RFO incl.) — ~218× left of the Zen4 ridge (~15.5 flop/byte) | **Memory-bandwidth-bound** | **Traffic reduction only:** vertical fusion + non-temporal stores | SIMD lanes (idle on a saturated bus), more cores (1.7× ceiling already hit) |
| **`compute_significance`** (systems.rs:47) | dependent chain `d²→sqrt→1/(1+√·k)` ≈ 13 ns/agent scalar, ~10× above the bandwidth floor | **Compute-bound** (sqrt/div latency chain) | **SIMD (`std::simd` f64x4 → `vsqrtpd`/`vdivpd`)**: ~3-4× | — |
| **`rebase_gltf`** (streaming.rs:190) | ~12 fma point + ~9 fma normal per vertex, high AI | **Compute-bound** | **SIMD/ISPC over SoA positions**: ~3-4× | — (but Amdahl-gated by the Draco share — profile the split first) |
| **Draco decode** (C++ FFI) | entropy/connectivity decode | **Serial-per-tile** | **Across-tile fan-out (already present at `DECODE_BATCH=1`)** | per-tile parallelism (irreducible serial core) |

### 4.2 The bandwidth-bound levers (integrate) — these COMPOUND

1. **Vertical fusion of `integrate_motion` + `compute_significance` [CONFIRMED].** Today they are two independent full streaming passes over the same `Transform3d` SoA. `compute_significance` re-reads the 24B f64 position column in a *separate* phase (`RepresentationPlan`). Fuse so position is produced and consumed in registers → **removes one full 24B/agent f64 read-stream/frame**. O(2n) → O(n) position traffic. *Caveat:* forces shared cadence — if significance later drops off-cadence, recompute it only on the last fixed substep.

2. **Non-temporal (streaming) stores on the write-only translation column [CONFIRMED, measure-gated].** integrate writes `translation_cm` and never reads it back in the same frame → the mandatory read-for-ownership of each destination line is wasted bandwidth. `_mm256_stream_pd` over the aligned stride-1 `tx/ty/tz` columns: **84B → ~60B (~28% fewer bytes)**. Needs the F2 split for alignment + a trailing `sfence` (the frame barrier provides ordering). **Apply only to the FINAL writer of a column** (not significance's read of position). Stacks multiplicatively with fusion (different bytes).

> **Honest scope on integrate:** its *materiality* depends on the active-set size K, not the roofline. At K≈10M it's ~9-16 ms/frame (dominant). At ADR-0015's budgeted K≈10⁴-10⁶ it's ~0.1-1 ms — below the noise floor. **So these levers matter exactly where the LOD work *fails* to shrink K; the significance-SIMD win (which feeds the index that *does* shrink K) is the more universally material of the two.**

### 4.3 The compute-bound levers — `rebase_gltf` is the one place 100% CPU is the goal

`rebase_gltf` runs **scalar f64** — verified against glam 0.32.1 source: `src/f64/` has only flat scalar files (no `coresimd/sse2/neon` subdirs that `src/f32/` has); `DVec3 = {x,y,z:f64}` plain, `DMat4::transform_point3` is a scalar axpy chain. Zero `Vec3A`/`std::simd`/`core::arch` anywhere in `crates/geo`. **[CONFIRMED]**

- **Fix:** structure positions as SoA per primitive, apply the prebuilt affine with **ISPC `foreach`** (already vendored for BC7 + the morsel spike) or `std::simd` (unstable, fine on 1.97 nightly). **Realistic ~3-4× on AVX2 f64** (verify pass corrected the "4-8×" ceiling: `vdivpd`/`vsqrtpd` aren't fully pipelined; 8× needs the f32-after-cancellation path).
- **Constraint:** the f64 ECEF cancellation in `ecef_to_local_affine` (streaming.rs:290) **must stay f64 and per-primitive scalar** — only the per-vertex *apply* vectorizes. Local ENU coords are small, so an f32 apply (after the affine pre-subtracts the local origin) is the path to 8× if profiling justifies it.
- **Amdahl gate:** decode-stage = Draco (C++ FFI) + rebase. If Draco dominates (plausible for dense photogrammetry), a 4× on rebase yields only a modest stage win. **Profile the Draco/rebase split first.**

### 4.4 Cross-lane arbitration — bandwidth-bound vs compute-bound contention

Gameplay and Geo share **one** `ComputeTaskPool` (`logical-2` = 22 on the dev box). A Geo decode burst can occupy every worker when the gameplay tick needs them. **[CONFIRMED architecturally, but the boldest sub-claims are CAVEATED to *medium* confidence]:**

- **Exp E was never run** — the "+20% p99" is the *gate*, not a measurement. Contention is hypothesized, unquantified.
- The proposed "whole burst → one tile" yield improves a worst case that **small-batch decode already largely prevents**: at `DECODE_BATCH=1` a freeing worker picks up a gameplay task after one *batch*, so the wait is already ~O(one batch), and per-*tile* yield is actually *coarser*.
- **The more likely p99 lever is the physical-core budget (Exp F):** `logical-2 = 22` over-subscribes a 12-physical-core box ~3-4×. Correcting the pool to `physical-3` reduces oversubscription — a steady-state fix that beats a yield flag.

**Playbook:** (1) run Exp E first; if p99 < +20%, do nothing. (2) Fix the physical-core budget. (3) *Only if contention is real*, add the genuinely novel piece — **fill provably-idle workers between frame barriers with bounded, abandonable speculative prefetch** (the `prefetch_along_motion` candidates already exist, capped by `MAX_PREFETCH_PER_STEP`). Speculative + bounded + lower-priority than both lanes = "fill idle cores" done right.

---

## 5. O-notation reduction table — ranked by (impact × hot-path-ness) / effort

| # | Reduction | From → To | n = | Effort | Verdict | Fork/FFI |
|---|---|---|---|---|---|---|
| **1** | **Wire `SigIndex` into `allocate_representation_budget`** | O(P) scan + 2nd O(P) write/frame → O(Δ + k log k + buckets) | live agents | **~1 day** | **[CONFIRMED]** 64× spike | **Pure Rust, no FFI** |
| **2** | **`sse_cull_lru` over the active set** (unconditional O(pool) float scan) | O(pool) → O(active) | pool=24,576 | ~1-2 days | **[CONFIRMED — the material half]** | Pure Rust |
| **3** | **`reconcile_index` event-driven** | O(index)/step → O(Δ_transitions) | resident index | **~0.5 day** | **[CONFIRMED]** | Pure Rust |
| **4** | **`decode` off explicit fetched-set** (+kills ~24K task-spawns/step) | O(pool) scan + O(pool) spawns → O(Δ_fetched) | pool=24,576 | ~hours | **[CONFIRMED, amplified]** | Pure Rust (Tier-1 watermark) |
| **5** | **Vertical fusion integrate+significance** | 2× position read → 1× | active agents | ~1-2 days | **[CONFIRMED]** | Pure Rust |
| **6** | **Hot/cold `Transform3d` split** | 56B → 24B integrate stride | active agents | medium | **[CONFIRMED]** | Pure Rust (ADR-0004 rebase) |
| **7** | **`compute_significance` SIMD** (after split) | scalar → f64x4 (~3-4×) | active agents | ~days | **[CONFIRMED]** | Pure Rust (`std::simd`) |
| **8** | **`rebase_gltf` SIMD/ISPC apply** | scalar f64 → ~3-4× | verts/tile | ~3-5 days | **[CONFIRMED, Amdahl-gated]** | ISPC (vendored) or `std::simd` |
| **9** | **NT stores on translation column** | 84B → 60B (~28%) | active agents | ~hours | **[CONFIRMED, measure-gated]** | Pure Rust (unsafe `core::arch`) |
| **10** | **`compute_significance` camera-grid** | O(P·C) → O(cells·C + P) | agents × cameras | ~1 day | **[novel, untested]** | Pure Rust |
| **11** | **F1 change-tick watermark (general)** | O(N matched) → O(t+Δ) | matched rows | days (Tier-1) | **[CONFIRMED]** | Pure Rust Tier-1 |
| **12** | **bucketed top-K `select_nth` boundary fix** | O(b log b) → O(b) | boundary bucket | **~1 hr** | **[novel, untested]** | Pure Rust (before index ships) |
| **13** | **Incremental BFS traversal** | O(visited)/frame → O(Δ_frontier) | visited nodes | **~2 weeks, ADR-worthy** | **[CONFIRMED novel, high-risk]** | Pure Rust, flag-gated |

**The top of this table is overwhelmingly pure-Rust, no-FFI, sub-week work.** #1-#4 alone remove every remaining O(P)/O(pool) term from the two hot lanes' critical paths. **Do these before any fork or GPU work.**

---

## 6. GPU-ECS verdict — NOT-YET, with a precise trigger; NEVER as the World-of-record

### 6.1 The bandwidth + readback math (corrected)

Two walls in series decide everything:

- **Readback wall [CONFIRMED]:** PCIe 5.0 x16 ≈ 64 GB/s usable one-way. `read_set × bytes ≤ ~2 GB/frame` at 30 Hz → **CPU-readable set hard-capped at ~3×10⁷, independent of VRAM.** 10⁸ agents × 64B = 6.4 GB = 3 frames to copy = impossible. Anything the CPU reads each tick **stays CPU.**
- **Throughput crossover [REFUTED→corrected]:** the right formula is net-win, `N* = D_dispatch / (1/BW_cpu − 1/BW_gpu)`. With the codebase's *measured* CPU rate (1.11×10⁹ agents/s) and GPU ~8×10⁹/s: **N\* ≈ 6×10⁴ at D=50 µs**, not the 1-5 M one facet claimed (it mistakenly solved the GPU-compute-exceeds-its-own-dispatch knee). This means a *branch-free, no-readback* band pays for the GPU from **~60 K resident agents** — making ADR-0017's narrow offload attractive ~50-100× sooner than believed.

### 6.2 The verdict

**A fuller GPU-ECS (entities/components resident + GPU-side structural ops + per-class compute "systems") is:**

- **NEVER the World-of-record** — the readback wall + FP non-reproducibility (FMA contraction, reassociation, transcendental ULPs across vendors) + the deterministic near-set requirement (ADR-0001/0016) permanently forbid it. **[CONFIRMED]** A GPU sim can at most be a **non-authoritative co-processor for the far field.**
- **NOT-YET necessary for gameplay** — but for the *right* reason: the active set is **readback- and branch/determinism-bound, independent of N** (not because the crossover sits above the cap — that arithmetic was wrong). **[the conclusion survives the refutation]**
- **NEVER necessary as a *framework*** — because no portable GPU structural-mutation primitive exists (spawn/despawn/add-component = O(N) prefix-sum+scatter+rebind, forbidden in the hot loop). Every shipping "GPU ECS" freezes the schema and mutates only field values. **[CONFIRMED]** Vast already satisfies this precondition (ADR-0015 bands-as-archetypes, Geo pool+free-list).

### 6.3 The one trigger for the fuller machine [CONFIRMED as the only distinguishing capability]

Build a GPU-side structural engine **iff** a measured profile shows the Warm/Cold far field needs **per-frame GPU-side compaction/sort/spawn at > ~3×10⁷ ops** that the CPU cannot orchestrate as O(Δ) index edits over PCIe (e.g. a mass-promotion event — a city evacuating). Until then, ADR-0017's resident-kinematics offload is the **maximal correct GPU surface**, and even *that* stays dark until the P0 crossover spike fires.

### 6.4 What competes the GPU away [CONFIRMED]

ADR-0015's **O(#flows) statistical far field** (Little's law per segment, stock conservation per depot) is the *rival* answer to the same wall, and it wins by 3-6 orders of magnitude (10³-10⁵ vs 10⁸-10⁹), with no GPU, no readback, no second device. *Caveat:* Little's law alone is too weak for spatial congestion (shockwaves, queues) — those need a per-segment PDE, but that's still **O(#segments) ≪ O(agents)**, so the direction holds. The GPU's only residual role is an **optional bounded visible-swarm render hint** over ADR-0017's resident buffer — which needs no components/systems/structural ops, i.e. *not a GPU-ECS*.

### 6.5 The portability constraint that shapes any GPU sim [CONFIRMED]

WGSL has **no forward-progress guarantee and no cross-workgroup sync** (wgpu #7445 closed as not-planned; broken on macOS/Qualcomm/some AMD). A **persistent-threads megakernel / device-side work queue is spec-illegal** — the portable shape is forced to **host-driven wavefront dispatch** (radix-bin by behavior class → per-class indirect dispatch). The cost: O(S_eff) re-reads of agent state from VRAM between stages (L2 absorbs some, so the divisor is in (1, S], not S), plus a ~36 µs dispatch floor capping class×stage count to a few hundred. **This further raises the crossover N and strengthens the deferred default.** It also deletes the entire CUDA-style device-enqueue scheduler class on portability grounds — the very reason wgpu was chosen over CUDA.

### 6.6 The interop risk is worse than ADR-0017 states [CONFIRMED-with-caveats]

ADR-0017 line 91 says "import is proven (PR #6161); export may need a wgpu-hal patch" — *singular*. Reality: PR #6161 is **import-only, texture-only, D3D11→Vulkan**. Buffer **import** does exist (`create_buffer_from_hal`, PR #3355), but buffer **export** of a shared NT handle — the operative capability (Rust-owned SoA → UE, mandated by ADR-0005 since UE can't own the device) — is **absent on every backend**, fence-sharing (#4872) is open, and even imported buffers have a state-tracking usability gap. **The P1 spike scope is understated ~2-3×:** prove buffer export AND a shareable fence AND a usable imported buffer, all via custom unsafe wgpu-hal, likely upstream. **Gate the entire ADR-0017 commitment on this spike, cheaply, first.**

### 6.7 The actual cheapest GPU step [CONFIRMED, codec-dependent]

**Geo decode/rebase**, not gameplay: a fixed-function compute kernel (~150 GB/s GPU meshopt vs ~7-10 GB/s CPU) whose output is a render vertex buffer consumed GPU-side — **zero PCIe readback, sidestepping the wall entirely.** It is the *best first interop target* precisely because it de-risks the shared-handle path without the readback complexity. **Caveats:** (a) win is bounded to the cold-first-visit decode burst (warm revisits hit the M3 geom DiskCache); (b) the 150 GB/s figure is meshopt's byte-codec — P3DT uses **Draco** (entropy/edgebreaker), materially less GPU-friendly, so the GPU lane likely needs a Draco→meshopt **transcode** (which changes the ADR-0011 offline bake format and the FFI). Treat the magnitude as "order-of-magnitude, codec-dependent."

---

## 7. Prioritized experiments — cheapest → most expensive

Each tagged **[pure-Rust no-FFI]** (cheapest), **[needs fork]**, or **[needs GPU]**.

### Tier 0 — hours, pure-Rust, do immediately
1. **Wire `SigIndex` + re-run bit-identical replay** **[pure-Rust]**. *Hypothesis:* reproduces 64× at 1M with deterministic top-K. *Measure:* budget-phase ns/agent at 100k/1M + tier-assignment set-equality over 100 replay frames. *Threshold:* >10× speedup, zero divergence.
2. **`select_nth` boundary fix for the bucketed top-K** **[pure-Rust]**, before the index ships. *Threshold:* removes the log; boundary-bucket cost flat in P under power-law skew.
3. **Event-drive `reconcile_index`** **[pure-Rust]**. *Measure:* stationary-camera reconcile cost. *Threshold:* >5× drop, zero missed transitions over a 1000-step soak.
4. **`decode` off explicit fetched-set** **[pure-Rust]**. *Measure:* task-spawn count + decode-stage time vs pool_size. *Threshold:* spawns → ~0 on no-change steps; decode-gating flat in pool_size.
5. **Per-stage stream-thread profile under a moving-camera P3DT capture** **[pure-Rust, measurement only]**. *The gate for §2:* if decode < ~30% of the serial sum OR the thread is parked >70%, the pipeline is deprioritized for I/O concurrency. **Run this before P-1.**
6. **Exp E: decode-burst tick-p99 census** **[pure-Rust]**. *The gate for §4.4:* if p99 < +20%, the yield-checkpoint is unjustified; fix the physical-core budget instead.

### Tier 1 — days, pure-Rust
7. **Roofline-classify both kernels with hardware counters** **[pure-Rust]**. *Threshold:* integrate within ~80% of STREAM BW AND <1.2× from 8→12 cores ⇒ confirmed bandwidth-bound (stop adding cores/SIMD); rebase scaling >0.7× linear ⇒ SIMD-worth-it.
8. **`sse_cull_lru` over active-set vs O(pool)** **[pure-Rust]**. *Threshold:* current grows >1.5× from 2k→64k pool; prototype stays within 1.1× (flat).
9. **Fuse integrate+significance, A/B position-column traffic** **[pure-Rust]**. *Threshold:* combined ns/agent −15%, bit-identical golden test passes.
10. **NT-store spike on the position column** **[pure-Rust, unsafe]**. *Threshold:* ≥20% integrate throughput, no correctness regression.
11. **CPU sorted-state near-field index** (neighborhood-grid trick, MOSS-derived) **[pure-Rust]**. *Hypothesis:* a maintained spatial sort (few fix-up passes/frame) beats per-frame grid rebuild and may raise the CPU ceiling before any GPU. *Threshold:* >1.5× faster than rebuild-hash at 1M, neighbor-set error acceptable.

### Tier 2 — days-weeks, fork or GPU
12. **Non-fork precomputed-stage executor spike** **[needs fork to confirm fork is *unnecessary*]**. *Threshold:* builds with zero source patches AND bit-identical to MT; record the exact minimal `pub(super)` set if correlation fails (that becomes the fork's precise diff).
13. **ISPC/`std::simd` vectorize the rebase apply** **[ISPC vendored]**. *Threshold:* apply ≥3× faster AND cold-terrain decode-stage drops; f64 matrix-build path unchanged.
14. **2-stage Geo decouple (decode detached + harvest)** **[pure-Rust]**, *only if Tier-0 #5 shows decode dominant + CPU-bound.* *Threshold:* ≥1.5× tiles/sec on a moving trace, zero tile-state races.
15. **P0 resident-kinematic GPU crossover** **[needs GPU]**. *Threshold:* GPU ns/agent < CPU by ≥5× at a realistic N AND dispatch tax < 1 ms; if wavefront re-reads collapse the win below ~3×, stays CPU. **Re-derive crossover with the corrected formula (~6×10⁴, not 1-5M).**
16. **P1 buffer-interop round-trip** **[needs GPU]**. *Threshold:* GO only if **buffer** (not texture) export+import+fence works on ≥2 of {NVIDIA,AMD,Intel} with <~200 LOC unsafe wgpu-hal; else ADR-0017 stays dark.
17. **Geo-decode-GPU spike** **[needs GPU]** — the cheapest interop de-risk (zero readback). *Threshold:* >5× on real tile sizes AND cold-terrain advance p99 drops; abandon if fetch dominates.

---

## 8. Rejected / overhyped — and why (so we don't chase them)

| Idea | Verdict | Why rejected/downgraded |
|---|---|---|
| **"3-stage Geo software-pipeline → 2-3× throughput"** | **[REFUTED magnitude]** | Stages share one `&mut GeoStreaming` (index+tree+sel_state), the thread is I/O-park-bound during streaming, decode dominates the cheap stages. Real win ≈ 1.1-1.4× latency-hiding. The *framing* (effort above the schedule) survives; the multiplier does not. Build the *decoupled-decode* form (P-1), not the full 7-stage pipeline. |
| **"GPU-vs-morsel crossover ≈ 1-5 M agents"** | **[REFUTED]** | Mis-derived (solved GPU-compute-exceeds-own-dispatch, not net-win). Correct ≈ **6×10⁴**. The error *inverted* the argument — at the true crossover most of the active set sits *above* it, so the gameplay-stays-CPU verdict must rest on **readback + branch/determinism bounds**, not on the crossover. |
| **"rebase SIMD → 4-8×"** | **[CAVEATED to 3-4×]** | AVX2 f64 is 4-wide and `vdivpd`/`vsqrtpd` aren't fully pipelined; 8× needs the f32-after-cancellation path. And it's Amdahl-gated by the Draco share. |
| **"Change-ticks pollute the integrate cache line (56B payload + 8B ticks read together)"** | **[REFUTED sub-claim]** | Ticks are a *separate* `ThinArrayPtr` allocation, not interleaved with the `BlobArray` payload. A read (or the morsel SoA path) never pulls them. The real tick tax is RSS + the periodic O(C_total) stall, **bounded by the active near-set (~0.24-0.72 GB), not 24 GB at billions** (the far field has no rows). |
| **"check_tick is a multi-GB stall at billions"** | **[CAVEATED]** | True per-element math, but fires ~every 4.8 hrs at 30 Hz and is bounded by the *active* set under ADR-0015. A rare frame-spike risk, not a steady-state throughput tax. |
| **Field-range (column-range) access fork (F7)** | **[CONFIRMED zero benefit]** | Every gameplay phase is single-writer or chained; Geo is fully chained; the multi-threaded executor is near-idle. The no-fork hot/cold split (F2) delivers all of it. **Don't build the fork.** |
| **Precomputed-stage executor as a *required fork*** | **[demoted]** | The conflict graph is fully `pub` at build time; a non-fork executor works via `set_executor` + `Schedule::systems()` correlation. The fork is a ~6-LOC micro-opt, and the win is ~0% at S=3-8 regardless. |
| **Hot-executor spin-park (F6) as a current win** | **[CAVEATED to ~0% today]** | bevy's finisher self-ticks and pops the dependent next; queues stay full mid-system so OS-wakes are already rare; dependency-batching is already done. Insurance for a wide future graph, competing with the precomputed-stage barrier model. |
| **Per-tile decode yield-checkpoint as the p99 fix** | **[CAVEATED, medium confidence]** | Exp E never ran (the +20% is a gate, not a measurement); small-batch decode already bounds the wait to ~one batch (finer than one tile); the **physical-core budget (Exp F)** is the more likely lever. Run Exp E first. |
| **A GPU "ECS" framework (resident components + systems + GPU structural ops)** | **[NOT-YET / NEVER-as-framework]** | No portable GPU structural-mutation primitive; readback + determinism forbid authority; ADR-0015's O(#flows) aggregate competes the population away. At most a non-authoritative far-field co-processor, triggered only by >3×10⁷ GPU-side structural ops/frame. |
| **A GPU-ECS for the Geo tile lifetime** | **[wrong problem]** | The tile-ECS sweeps ~1.5 MB (24,576 entities × ~60B) — bandwidth-trivial. One decoded tile moves more bytes than the whole tile-ECS. The GPU win is the *decode kernel*, not the scheduling ECS. |
| **Rust-side texture/BC7 parallelization on the stream thread** | **[doesn't exist]** | Image decode + BC7 are entirely C++/UE on a separate worker; Rust ships raw bytes. The Rust decode budget is Draco + rebase only. |

---

## 9. The bottom line for the build

**Spend the next sprint entirely in pure-Rust, no-FFI, sub-week reductions (§5 rows 1-4), because they remove every remaining O(P)/O(pool) term from the two hot lanes' critical paths and require no fork and no GPU.** In order: wire the dead `SigIndex` (#1, the 64× already in the tree), then the three Geo O(pool)/O(index) → O(active)/O(Δ) fixes (#2-#4, with `sse_cull_lru` as the material one).

**Then** the bandwidth + compute levers (§5 rows 5-9): the integrate fusion + the hot/cold split that unblocks significance/rebase SIMD — the only multi-percent wins on the gameplay loop, gated by the roofline classification (Exp #7).

**Defer every fork** — the precomputed-stage executor doesn't need one and wins ~0% at our system count; the change-tick watermark and component split work with public seams.

**Keep the GPU dark** until (a) the corrected ~6×10⁴ crossover spike (Exp #15) *and* (b) the buffer-interop spike (Exp #16) both pass — and make the **Geo decode kernel** (zero-readback) the first GPU target, not gameplay. A GPU-ECS framework is the wrong machine; the right machine is ADR-0015's O(#flows) aggregate on the CPU, with the GPU as at most a non-authoritative far-field swarm renderer.

---

### Verified code anchors (re-checked against the working tree this session)
- `Rust/crates/geo/logi_3dtiles_scene/src/geo_world.rs:1435-1443` — the 7-stage serial `advance_streaming` spine.
- `Rust/crates/geo/logi_3dtiles_scene/src/geo_world.rs:1267` — `(sse_cull_lru, decode, evict, publish).chain()`.
- `Rust/crates/geo/logi_3dtiles_scene/src/geo_world.rs:602` — `DECODE_BATCH = 1` (→ ~24,576 task-spawns/step).
- `Rust/crates/logi_sim/src/systems.rs:110-191` — live O(P) `allocate_representation_budget` (scan + `select_nth_unstable_by` + 2nd O(P) write pass).
- `Rust/crates/logi_sim/src/sig_index.rs` — the **only** file referencing `SigIndex` (dead code, the cheapest large win).
- `Rust/crates/logi_sim/src/schedules.rs:155-158` — `fixed_sim` single-writer; `representation_plan` `.chain()`ed (confirms F7 zero-benefit).
