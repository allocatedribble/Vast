# Geo Audit — Deferred Optimizations (investigate later)

**Date:** 2026-06-06
**Update (2026-06-07):** TODO-1 (memoize the region bounding-sphere) **shipped** — `logi_3dtiles_tileset`'s `bounds_memo: OnceCell` + `Tile::region_sphere_ecef`, consumed by `logi_3dtiles_select::tile_bounding_sphere_ecef` — so per this doc's own delete-on-implement rule its entry is removed. The two items below remain deferred.
**Context:** Residue of the microscopic Geo audit (Quality / Logic / Performance, every algorithm ranked by O). The audit's Critical + High hostile-input items and the clear low-risk perf/quality wins were **implemented and verified this session** (`cargo build`/`test --workspace`/`clippy -D warnings`/`fmt` all green — see the session memory / git history). The two items below were **intentionally NOT implemented** and parked here.
**Why a TODO and not a fix:** each is a **constant-factor** improvement (not an order reduction) on a path that **no profile has flagged**, and each carries a cost — a design-purity break, a correctness risk, or structural churn the design deliberately avoids. [`AGENTS.md`](../../AGENTS.md) §1B (*measure first*) and §1A (*YAGNI*) say: get the order right everywhere (done), but spend instruction-level effort **only where the profiler points**. So these wait for a flamegraph. Each entry below names the exact signal that would justify doing it, and sketches the fix so the next contributor starts warm.

> **Do not implement these speculatively.** Open them only when the named profiling signal appears (see [`../Performance/Profiling-Checklist.md`](../Performance/Profiling-Checklist.md)). If you implement one, add a benchmark that demonstrates the win against [`../Performance/Budgets.md`](../Performance/Budgets.md), then delete its entry here.

---

## TODO-1 (was audit H2) — Resident-list / delta reconcile instead of whole-pool scans

**Performance · `Rust/crates/geo/logi_3dtiles_scene/src/geo_world.rs` — `publish`, `evict` (`Query::iter` over the entire pre-spawned entity pool), and `reconcile_index` (scan of `s.index`).**

**What.** The pool is `pool_size` pre-spawned tile entities sharing one archetype (a deliberate no-spawn/no-despawn design). Every step, the `evict` and `publish` systems iterate **all** of them to find the `Resident` / in-view subset, and `reconcile_index` scans the whole working-set `index`. The work that actually changes per frame (decode/evict/restore transitions) is a small fraction of the pool.

**Why deferred.**
- **Constant-factor (~2×), not order.** `publish`'s output is already `O(resident-in-view)` — it *must* emit every visible tile. The waste is only the scan over `Free` slots, i.e. a constant factor set by how oversized the pool is relative to the resident cap. It is **not** an order increase.
- **Geo does not scale with the million agents.** The pool is bounded by what's *visible/streamable* (tens of thousands), independent of the million **gameplay** agents — those live in `logi_sim`, a different `World`. So "O(pool) per frame" is "O(tens of thousands)", fixed-ish, at the decoupled stream-thread cadence — not the scaling cliff the North Star worries about.
- **The idiomatic fix fights the design + risks correctness.** Making queries iterate only residents needs either a marker component (an **archetype split** → the structural churn the pre-spawned-pool design exists to avoid) or a maintained `HashSet<Entity>` resident set fed from the **parallel** `decode` system (requires deferred `Commands` / a parallel collector, and interacts subtly with `Changed<>` change-detection). A bug here renders/evicts/leaks the wrong tiles — exactly the class of defect that's expensive to catch.

**Investigate when.** A flamegraph shows `evict` or `publish` (the `Query::iter` / state-filter loop) as a hot frame at a realistically large `pool_size`, **or** the pool is intentionally sized ≫ resident-cap (lots of `Free` headroom) so the scan waste dominates.

**Fix sketch.** Add a marker ZST component `Resident` and `Query<…, With<Resident>>` in `evict`/`publish` so bevy iterates only the resident archetype. Transition it alongside `TileState`: `decode` inserts it (via a parallel command buffer), `evict` / `reconcile_index` / `cancel_stale` remove it. Verify against the existing scene tests (`evict_*`, `publish_collects_visible_resident_tiles`, the streaming-pipeline tests) and add one asserting the resident archetype membership tracks `TileState::Resident` exactly across a decode→evict→restore cycle. Measure the archetype-move cost (bounded by transitions/step) against the scan saved.

---

## TODO-2 (was audit M1/H6) — Intern tile paths to an integer key

**Performance · `Rust/crates/geo/logi_3dtiles_scene/src/geo_world.rs` — the `TilePath = Vec<u32>`-keyed maps `index`, `grafted`, `in_flight_paths`, `decode_failed`, `bases`; and the `Vec<u32>` paths in `logi_3dtiles_select`'s `Selection`/`SelectionState`.**

**What.** Tile identity is the child-index path `Vec<u32>`. Per pass it is hashed for several map lookups and cloned into selection results. Interning each path to a single integer id would make lookups `O(1)` (a word) instead of `O(path-len)` and drop the clones.

**Why deferred — and why the trade is backwards.**
- **Constant-factor, and the constant is already small.** Paths are short (depth ~tens; hard-capped at `MAX_TILE_DEPTH = 512`). Hashing/cloning a ~25-element `Vec<u32>` is already cheap; this is not a measured bottleneck.
- **It would *add* a correctness risk.** Today's `Vec<u32>` keys are **collision-free**. A 64-bit interned/hashed key **can** collide → silently serve or route the *wrong* tile (the in-memory `MeshCache` and the C++ section cache key off `tile_key`, a 64-bit FNV of the path, precisely because their populations are tiny; the persistent disk cache already uses **128-bit** for the same reason — see `logi_3dtiles_io::cache_key`). Spending collision-freedom for an unmeasured speed gain inverts §1E's "correctness at scale."

**Investigate when.** A profile shows `Vec<u32>` path hashing/cloning hot in `select_request` / `select_tree` — **and** only with a key width (e.g. `u128`, or a true interner with a dense `u32` id allocated as tiles enter the tree) that keeps collisions negligible across the live working set.

**Fix sketch.** Prefer a real **interner** (path → dense `u32` id assigned on first insert, freed on evict) over a hash — it's collision-free *and* `O(1)`. The select crate would emit ids instead of (or alongside) `Vec<u32>`; the scene's five aux maps re-key to the id. This is a non-trivial cross-crate change; gate it on a benchmark.

---

## Not in this list (assessed → no action)

For the record, these audit "misc" items were checked and need **no** change (verified by reading, not assumed):

- **Octree 2²¹ coordinate guard** — present + correctly placed (`logi_3dtiles_implicit/src/lib.rs:144`; the `bit_index` tile path is already saturated by `BitSet` bounds).
- **Height-load non-finite reject** — already enforced (`logi_semantic/src/format.rs:71`).
- **`VERTEX_STRIDE` single-home** — already single-homed in `logi_geo_mesh` and re-exported; the bulk `interleave` vs per-vertex `push_vertex` split is justified (per-vertex avoids per-call allocation).
- **MeshCache keyspace (M2)** — the 64-bit in-memory key is collision-safe at the in-memory cache's size; the persistent disk cache already uses 128-bit. No change.
- **Distance-clamp single-home** — the clamps are contextually different (`.max(0.0)` for culling vs `.max(1.0)` for SSE division); merging them would obscure intent.
- **Bounded `HttpSource` request channel (M6)** — the owner's `max_in_flight` cap already bounds queue depth; an explicit bound only adds a drop/block failure mode.
- **View-jitter epsilon** — a speculative *feature* (skip re-selection on tiny view deltas), not an existing gap; YAGNI until a need is shown, and it could harm camera responsiveness.
