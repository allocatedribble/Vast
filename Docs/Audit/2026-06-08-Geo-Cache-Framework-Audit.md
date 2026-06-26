<!-- Generated 2026-06-08 by a 33-opus-agent audit workflow (wf_12dc4574-9e1): 7 parallel audit lenses + adversarial verify + synthesis. 25 findings / 15 confirmed / 10 rejected; 27 algorithms catalogued; 11 techniques researched. -->

# Geo Cache Framework — Microscopic Audit Synthesis

**Scope:** `DiskCache` (persistent on-disk LRU of fetched tile bytes — `Rust/crates/geo/logi_3dtiles_io/src/lib.rs`) and `MeshCache` / "Flash Cache" (in-RAM LRU of decoded geometry — `Rust/crates/geo/logi_3dtiles_scene/src/geo_world.rs`), with the FFI delta/telemetry path (`logi_geo_ffi/src/ffi.rs`) and the C++ host reconcile (`LogiBakedGeoTerrainActor.cpp`) as supporting surfaces.

---

## 1. Executive summary

**State of both caches.** The two caches are architecturally sound and, on the *steady-state hot path*, already well-optimized. The O-notation census found **no `O(n²)`-over-the-tile-set landmine on the per-step path**: nearly every hot operation is `O(1)` (hashbrown maps for the disk index, in-flight set, cancel registry, `MeshCache`, FFI `key_to_index`; `TMap`/`TSet` on the C++ side) or `O(log n)` (the disk LRU `BTreeMap`). The team has already applied the right reductions in the hottest places: `ResidentSet` (`O(resident)` not `O(pool)`), `select_nth_unstable_by` in the resident evict system (`O(n)` not full sort, `geo_world.rs:648-650`), decorate-sort in `select_request` (`O(m)` score evals, `geo_world.rs:1672`), the FFI v8 `O(Δ)` delta diff feeding C++ `ApplyGlobeDelta`, cap-bounded in-flight scans, a FIFO subtree cache with `O(1)` eviction, and self-healing wholesale-clear dead-path caps.

**Are they bullet-proof? No — close on the steady-state hot path, not yet on durability/integrity.** The gaps are concentrated in **crash-safety, on-disk integrity, and cross-session reuse correctness**, not in algorithmic complexity. Specifically: the disk cache serves bytes with **zero content-integrity verification** (`lib.rs:589`), **leaks orphaned `.tmp{seq}` files** that escape the byte budget across crashes (`lib.rs:611-619`), has a **TOCTOU phantom-entry** path on a failed read (`lib.rs:581-594`), and **never persists decoded geometry** so every cross-session revisit re-pays the dominant Draco decode (`geo_world.rs:1696-1700`). None of these corrupts *gameplay* truth (terrain render data is non-authoritative per ADR-0007), so the failure modes are render-surface defects + a slow disk leak, not authority violations — but they are the difference between "works" and "bullet-proof."

**Top risks (in priority order):**

1. **No on-disk integrity check** — a single bit-rotted/truncated cache file is *permanently* poisonous: a cache hit short-circuits re-fetch (`lib.rs:424`), so the bad tile never self-heals. (Finding D1 — High→corrected Medium.)
2. **Orphaned `.tmp{seq}` leak** — crashed in-flight writes deposit permanent orphans that are invisible to the byte budget, so the on-disk footprint grows past `GlobeCacheBudgetGB` without bound. (Finding D2 — High→Medium.)
3. **No persisted decoded-geometry tier** — every `MeshCache` miss (eviction beyond the 384 MiB RAM window, *and every cold start*) re-runs Draco decode + ENU rebase on bytes already on local disk; the single largest avoidable CPU sink. (Finding F1 — High→Medium.)
4. **First-approach-at-speed always misses** — only camera *look-direction* is retained; position *velocity* is discarded (`geo_world.rs:2170-2173`), so no predictive prefetch exists and the caches only help on *revisit*. (Finding F4 — Medium.)
5. **Both budgets are tuned blind** — neither warm-restore hit-rate nor disk hit-rate is observable through the lock-free FFI; the budget comments themselves invite tuning that no telemetry supports. (Findings F2/F3 — Medium→Low.)

**Top improvements (highest ROI first):**

1. **Persist the `DiskCache` index** (sidecar or `redb`) — kills the `O(F log F)` + ~F-`stat()` full-dir-scan on `open()` and preserves true LRU recency across restarts.
2. **Add an XXH3-128 / CRC32C content-integrity check** to disk entries — turns silent corruption into a clean re-fetch; *free* if the store moves to `redb`. `crc32fast` is already in the lockfile.
3. **Swap `MeshCache::trim`'s full sort for `select_nth_unstable_by`** — `O(n log n)`→`O(n)`, mirroring the sibling evict system already in the same file.
4. **Replace `reconcile_index`'s unconditional `O(index)` full scan with a drained "dirty" transition queue** — the last per-step full map walk, reducible to `O(Δ)` (mirrors `NewResidents`). *(From census worst-offenders; no confirmed-findings entry — included as connective analysis, not a prioritized finding.)*
5. **Prototype a scan-resistant FIFO-class eviction (SIEVE / S3-FIFO)** for the Flash Cache to raise hit-rate on the orbit/re-entry case a planet fly-through depends on.

---

## 2. Algorithm O-notation comparison table

Every algorithm from the census. `n` meanings are per-row. "Hot?" = on the per-step / per-tile-fetch hot path.

| Algorithm | Cache | Op | Time | Space | n | Hot? | Reduction idea |
|---|---|---|---|---|---|---|---|
| `DiskCache::open` dir-scan + index build (`lib.rs:536-578`) | disk | open | **O(F log F)** | O(F) | F = cached `.bin` files | No | Persist index sidecar → ~O(F) one-read; rebuild-by-scan only after crash |
| `cache_key` FNV-1a-128 (`lib.rs:505-515`) | disk | get/put | O(L) | O(1) | L = URL path bytes | Yes | Hoist: compute once per `fetch_one`, pass `u128` to get+put |
| `cache_path` shard format (`lib.rs:518-521`) | disk | get/put | O(1) | O(1) | const | Yes | Reuse stack buffer / cache shard `PathBuf` (negligible vs I/O) |
| `CacheIndex::touch` BTreeMap bump (`lib.rs:465-473`) | disk | hit | O(log n) | O(1) | n = resident entries | Yes | CLOCK/S3-FIFO ref-bit → O(1), drop the BTreeMap |
| `CacheIndex::insert` (`lib.rs:474-483`) | disk | put | O(log n) | O(1) | n = resident entries | Yes | Same CLOCK/S3-FIFO → O(1) |
| `CacheIndex::evict_to` (`lib.rs:489-499`) | disk | put | O(k log n) | O(k) | n entries, k evicted (~1) | Yes | CLOCK ring-advance → O(1) amortized |
| `DiskCache::get` membership+read+touch (`lib.rs:581-594`) | disk | hit | O(L + size + log n) | O(size) | path/bytes/entries | Yes | Shard the index Mutex (16-64 stripes) + CLOCK |
| `DiskCache::put` write+insert+evict (`lib.rs:598-636`) | disk | put | O(L + size + k log n) | O(size + k) | path/bytes/entries/evicted | Yes | Skip redundant `create_dir_all`; shard Mutex; pass pre-hashed key |
| `DiskCache::bytes` (`lib.rs:639-641`) | disk | telemetry | O(1) | O(1) | const | No | Make `total` an `AtomicU64` → lock-free load |
| HttpSource cancel registry (`lib.rs:656,738-789`) | disk | req/cancel/poll | O(1) | O(in_flight) | live fetches | Yes | Already O(1); in_flight capped |
| HttpSource in_flight HashSet (`lib.rs:667,749-775`) | disk | ins/rem/len | O(1) | O(in_flight) | live fetches | Yes | None — O(1) |
| `fetch_one` worker cache-first (`lib.rs:415-435`) | disk | fetch | O(L+size+log n) hit; +network miss | O(size) | per tile fetch | Yes | Compute `cache_key` once, thread into get/put |
| `MeshCache` insert/take/contains/len/byte_len (`geo_world.rs:329-394`) | flash | all | O(1) | O(1) | n = decoded meshes | Yes | None — O(1) |
| **`MeshCache::trim`** score+sort (`geo_world.rs:361-384`) | flash | trim | **O(n log n)** over budget | O(n) | n = decoded meshes | Yes | `select_nth_unstable_by` → O(n) avg (copy the sibling evict system) |
| evict SYSTEM resident scan + select_nth (`geo_world.rs:586-671`) | flash | per step | O(resident) | O(candidates) | resident working set | Yes | Reuse a scratch `Vec` (drop per-step alloc) |
| `select_request` priority sort (`geo_world.rs:1670-1737`) | flash | per step | O(m log m) | O(m) | m = selected-unloaded | Yes | Partial sort (select_nth to in-flight budget) if m grows |
| `cancel_stale` in-flight scan (`geo_world.rs:1655-1657,1793-1833`) | flash | per step | O(in_flight) | O(m + stale) | in_flight (capped) | Yes | Reuse scratch set (negligible) |
| **`reconcile_index`** full index scan (`geo_world.rs:1219,1957-1998`) | flash | per step | **O(index)** | O(evicted+failed) | index = working-set entries | Yes | Drain a "dirty" transition queue → **O(Δ)/step** (mirror `NewResidents`) |
| `subtree_cache` FIFO ins+evict (`geo_world.rs:787-791,824-839`) | flash | insert | O(1) amortized | O(8192 cap) | cached subtrees | Yes | Leave (YAGNI); CLOCK bit if it ever thrashes |
| dead-path set caps wholesale clear (`geo_world.rs:841-848,1279-1284`) | flash | insert | O(1) amort (rare O(n)) | O(caps) | failed paths | No | Intentional self-healing — no change |
| `prune_cold_grafts` cold scan + root dedup (`geo_world.rs:1220,2010-2124`) | flash | every 120 steps | **O(grafts + index + c²)** | O(cold+to_free) | c = cold grafts this pass | No | Sort-then-linear-sweep → O(c log c) only if c grows |
| `tile_key` FNV-1a-64 (`geo_world.rs:2397-2406`) | flash | per tile | O(path_len) | O(1) | path depth (~tens) | Yes | None; 64-bit collision documented-accepted |
| FFI `build_globe_frame` rebuild + O(Δ) diff (`ffi.rs:649-713`) | shared | per New poll | O(n + prev) | O(n) | published + prev frame tiles | Yes | Already linear, reused allocs — no change |
| `logi_globe_world_tile_by_key` (`ffi.rs:889-910`) | shared | lookup | O(1) | O(1) | single key | Yes | None — O(1) |
| C++ `ApplyGlobeDelta` O(Δ) reconcile (`LogiBakedGeoTerrainActor.cpp:1763-1777`) | cpp | steady state | O(Δ) | O(Δ) | added+removed tiles | Yes | None — O(Δ) |
| C++ `ReconcileGlobeFull` + `EvictDeparted` (`...cpp:1783-1840,1948-1993`) | cpp | first/burst | O(n + resident) | O(resident) | visible + built tiles | Yes | Use Rust `removed_keys` in full path too (minor) |
| C++ Tiles TMap / TexturedKeys / FadingOut (`...cpp:896,915-916,…`) | cpp | all | O(1) | O(resident+fading+pending) | host bookkeeping | Yes | None — O(1) |

### Worst offenders & how to reduce them

1. **`reconcile_index` (`geo_world.rs:1957-1998`) — the only unconditional `O(index)` full map scan on the per-step hot path.** Every advance walks every in-flight+resident index entry just to find the handful that turned `Free`/`Fetched`. **Reduce:** drain a "dirty" transition queue (push from `evict` and `decode`, exactly as `NewResidents` already does for `Resident`) → **O(Δ)/step**. Highest-value reduction; note this came from the census worst-offenders list and is *not* itself one of the adversarially-confirmed findings.
2. **`MeshCache::trim` (`geo_world.rs:361-384`) — `O(n log n)` full sort to drop only the overflow tail.** The sibling evict system at `geo_world.rs:648-650` already solves the identical farthest+stalest top-k problem with `select_nth_unstable_by` (`O(n)` avg). **Reduce:** copy that pattern → **O(n)**. (Confirmed finding H/Med below.)
3. **`DiskCache::open` (`lib.rs:536-578`) — `O(F log F)` + ~F `stat()` syscalls at every world create.** F can be 100k+ on a multi-GB planet cache. **Reduce:** persist an index sidecar; scan only after a crash → ~**O(F)** one streamed read.
4. **Single `Mutex<CacheIndex>` (`lib.rs:528`) — one global lock serializes the entire fetch worker pool**, with a `BTreeMap` `O(log n)` op held under it. **Reduce:** stripe by hash bits + CLOCK/S3-FIFO → sharded `O(1)`. (Lock holds are already I/O-free, so measure first.)
5. **`prune_cold_grafts` root-dedup (`geo_world.rs:2030-2035`) — `O(c²)` worst-case** `cold.iter().any(prefix)`. Mitigated by the 120-step gate + early-return + small typical c. **Reduce only if it ever shows on a flamegraph:** sort-then-linear-sweep → **O(c log c)**.

---

## 3. Prioritized findings

Only adversarially-confirmed findings. Severities are the **corrected** verdicts (where a verdict downgraded the author's severity, the original is noted). Two dedups are flagged inline.

### Critical
*None.* No confirmed finding rises to Critical — all integrity/safety gaps are bounded and land on render-surface (non-authoritative per ADR-0007) data, not gameplay truth.

### High
*None after correction.* The three author-rated "High" findings (D1 integrity, D2 tmp-leak, F1 decoded-cache) were each **corrected to Medium** by their verdicts. They lead the Medium tier below.

### Medium

#### M1 — DiskCache serves bit-rotted / truncated tile bytes verbatim — no content integrity check
- **File:** `lib.rs:581-594` (get), `596-636` (put), `536-578` (open); absent workspace-wide.
- **Problem:** `get` does `std::fs::read(cache_path(...)).ok()?` (`lib.rs:589`) and returns the bytes with no hash/length verification. The atomic temp+rename guards *in-process torn writes* (Issue-D) but nothing *after* the file is in place: silent bit-rot, external truncation (AV/backup/user), or a filesystem short read all yield a valid-named file with wrong/partial bytes. Downstream `sniff_content_kind` only checks a 4-byte `glTF` magic (`streaming.rs:224-235`), so a magic-intact-but-truncated GLB is handed straight to the decoder. Confirmed no checksum exists in either crate.
- **Impact:** A single corrupt file is **permanently poisonous** — a cache hit short-circuits the network (`lib.rs:424`), so the tile never self-heals via re-fetch. Probability is non-negligible over a ~200 GB persistent cache's lifetime (`GlobeCacheBudgetGB=200`, dir survives restarts). Render-surface only (cannot corrupt sim).
- **Fix:** Embed a fast checksum in the filename: `<32-hex-key>.<8-hex-crc>.bin`. Use `crc32fast` (CRC32C) — **already in `Cargo.lock` via flate2** (add as a *direct* dep of `logi_3dtiles_io`), SIMD-accelerated, and the sibling `logi_semantic` offline-bake format already uses a trailing CRC-32 for the same purpose (strong in-tree precedent). Compute in `put`, verify in `get` (on mismatch: delete file + drop index entry + return `None` so `fetch_one` re-fetches), parse expected CRC from the stem in `open`, defer byte-verify to first read. **Drop the incorrect rationale that `open` would promote a stray `.tmpN`** — `open` already filters to `.bin` (`lib.rs:546`).
- **Big-O:** get `O(size)` read with no verify → `O(size)` read + `O(size)` CRC (memory-bandwidth-bound, ~30 GB/s, overlapped with the inevitable decode). open unchanged.
- **Confidence:** High. **(Author High → corrected Medium.)**

#### M2 — `put()` leaks orphaned `.tmp{seq}` files forever; disk grows past budget across sessions
- **File:** `lib.rs:611-619` (write/rename), `536-557` (open scan).
- **Problem:** `put` writes a process-unique `path.with_extension("tmp{seq}")` (`lib.rs:613`) then renames to `.bin` (`617`). A process kill between write-success (`614`) and rename-completion (`617`) strands the `.tmp{seq}` permanently. `open`'s scan skips every file whose extension `!= "bin"` via `continue` (`546-548`), so orphans are never indexed, never counted toward `total`, never deleted. There is no `Drop for DiskCache` and the only two `remove_file` sites are the rename-fail path (`618`) and eviction unlink (`634`).
- **Impact:** Across many sessions on a streamed planet, every crashed in-flight write deposits a permanent orphan invisible to the byte budget, so the `P3DTCache` footprint grows without bound beyond `GlobeCacheBudgetGB` — contradicting the byte-budgeted contract and the `lib.rs:596` doc ("a crash never leaves a partial file"). A slow, gradual leak (the leak window is sub-ms per write; only abnormal termination inside it strands a file), not a runaway.
- **Fix:** In `open`'s scan, delete any file whose extension is not `bin` (or whose stem is `tmp`-prefixed) instead of `continue`-ing. Safe because `open` is the constructor and runs **before any worker/`put` exists** (the `Arc<DiskCache>` is only handed to `HttpSource` after construction), so there are provably zero concurrent writers and no live reader can hold a `.tmpN`. `O(files)` — already paid by the scan.
- **Big-O:** unbounded disk growth `O(crashes)` across sessions → `O(files)` at open (already paid).
- **Confidence:** High. **(Author High → corrected Medium.)**

#### M3 — No persisted decoded-geometry cache: every MeshCache miss re-runs Draco decode + ENU rebase, even on a DiskCache hit
- **File:** `geo_world.rs:536-577` (decode), `306-395` (MeshCache), `1748-1786` (restore_cached), `1696-1700` (the only reuse probe); `logi_gltf/src/lib.rs:90-103` (Draco).
- **Problem:** Exactly two tiers exist — `DiskCache` (avoids re-fetch) and `MeshCache` (avoids re-decode, but RAM-only, 384 MiB, dies with the session). `select_request` falls through on `!MeshCache.contains_key(key)` (`geo_world.rs:1697`) to a fetch that is *usually a DiskCache hit serving bytes with no network* (`lib.rs:424`) — but those bytes then re-run the FULL `decode`: `logi_gltf::decode_glb` runs the vendored Draco decoder (`gltf/lib.rs:90-103`, used by every P3DT fine tile) plus `rebase_gltf` (per-vertex ECEF→ENU→Unreal). Decode is "the costly step the audit flagged for re-entry hitching" (verbatim, `geo_world.rs:309`).
- **Impact:** Every re-entry beyond the 384 MiB RAM window — and **every cold start over a previously-explored hero location** — re-pays the most expensive per-tile CPU op purely to reconstruct geometry already on local disk. Largest avoidable CPU sink in the framework.
- **Fix:** Add a **third tier**: a byte-budgeted, integrity-hashed persisted decoded-geometry cache. `TileGeometry` is three flat buffers + 4 scalars (`streaming.rs:37-47`: `vertices: Vec<u8>`, `indices: Vec<u32>`, `texture: Vec<u8>` (raw encoded JPEG/PNG — still compact), bounds + `unlit` bool) — serialize as a fixed header + three raw blocks, no serde. Probe before fetch; a hit deserializes into `Arc<TileGeometry>` (memcpy-bound) and takes the `restore_cached` path, skipping fetch AND Draco. Write from `evict` (`geo_world.rs:657-659`) on a **background writer / the fetch worker pool**, never the stream critical path. **Key-soundness caveat (must not be implemented as the finding's text says):** the cached vertices are baked into Unreal space *relative to `enu.origin_ecef`* (the hero origin folded into every vertex). Keying on the "session-independent path-hash" alone is **unsound** — a relaunch at a different origin would replay offset geometry. Key on **`(origin, path-hash)`** or store+validate the origin in the header. The within-session and same-hero-relaunch cases (constant origin) are sound.
- **Big-O:** per re-entry `O(Draco-decode + V)` CPU → `O(V)` memcpy deserialize, no Draco; probe `O(1)` + one file read; disk `O(decoded working set)`, budgeted.
- **Confidence:** High. **(Author High → corrected Medium.)**

#### M4 — No predictive prefetch along camera motion: velocity discarded, so high-speed flight always misses
- **File:** `geo_world.rs:2141-2182` (advance_from_ue), `2376-2385` (fetch_priority), `1672-1737` (select_request load loop).
- **Problem:** A fresh `camera_ecef` is fed each frame (`geo_world.rs:2154`) but only `cam_dir_ecef` (look direction) is retained (`2170-2173`) — velocity is unknown. `fetch_priority` (`2376-2385`) uses direction-alignment purely as a *ranking tie-breaker among tiles already in `sel.load`*; it adds no tile the current-frame SSE traversal didn't select. So tiles are only fetched once the camera is already close enough to cross the SSE threshold this frame; at speed the camera arrives before fine tiles fetch+decode (multi-hundred-ms round trip) → the classic coarse-ancestor blur, and the caches can't help because the tiles were never requested ahead of need.
- **Impact:** Cache hit-rate is bounded by *reactive* selection — the caches help on revisit (reverse/orbit), **never on first-approach-at-speed**, the dominant fly-through case. Known artifact; the adjacent frustum-margin / off-frustum coarse shell mitigations target *rotation* and shadow casting, not forward translation.
- **Fix:** Retain the previous frame's `camera_ecef` in `StreamState` (one `DVec3`, alongside `cam_dir_ecef`). **Decisive caveat — do NOT derive `velocity = (cam − prev)/dt` with `dt ≈ SHORT_PARK`:** `frame` is a pure iteration counter incremented only on work iterations (`stream_thread.rs:272`), not a fixed-cadence clock, and introducing wall-clock `dt` would violate the deterministic-systems law (`geo_world.rs:240`). Instead use the **dt-free displacement vector** `(cam − prev)` (already scales with speed) and express lookahead as a fraction of tile radius / cull distance. Then run the selector a **second time** against the predicted view (coarser `max_sse`) and feed its `load` into a low-priority prefetch queue drained only when `in_flight() < max_in_flight` (the existing cap gate at `geo_world.rs:1704`). **The "bias fetch_priority" alternative does NOT prefetch** — it only reorders tiles already selected; only the second-traversal variant adds tiles. Gate the extra traversal on nonzero motion (a parked camera pays nothing) and budget it — it is a real per-work-frame `O(selected)` cost, not `O(1)`.
- **Big-O:** selection `O(selected)`/frame, first-approach hit-rate ~0 → one extra `O(selected)` coarse traversal per moving frame, gated behind the in-flight cap; raises first-approach hit-rate toward the cache ceiling.
- **Confidence:** High. **(Medium, unchanged.)**

### Low

#### L1 — DEDUP: warm-cache + disk hit-rate / occupancy not observable through the lock-free FFI
> **Merge note:** the two separate observability findings — "MeshCache RAM occupancy has no lock-free telemetry" and "warm-cache effectiveness & disk hit-rate not observable / budgets untunable" — describe the same gap (the Rust→C++ lock-free snapshot omits cache effectiveness signals). Merged here; the broader one subsumes the occupancy-only one.

- **File:** `stream_thread.rs:59-96,257-313`; `ffi.rs:923-967`; `geo_world.rs:408` (`mesh_cache_hits_total`), `2209-2212` (`mesh_cache()`); `lib.rs:639` (disk `bytes()` only).
- **Problem:** `run_stream` mirrors into `StreamShared` atomics only `cached_bytes` (which is the **disk** total, not RAM) + the four `*_failed` counters (`stream_thread.rs:294-309`). It does **not** mirror `mesh_cache_hits_total`, `fetches_cancelled_total`, or RAM occupancy, and no FFI accessor surfaces them. `mesh_cache_hits_total` ("the cache's effectiveness", `geo_world.rs:408`) and `mesh_cache()` occupancy are owner-thread-only. The `DiskCache` has **no hit/miss counter at all** — `bytes()` is its only telemetry.
- **Impact:** Both budgets (`DEFAULT_MESH_CACHE_BYTES = 384 MiB`; the disk GB) are set blind — you cannot tell whether 384 MiB starves the warm cache (low hit-rate) or is oversized, nor whether the disk cache saves fetches. Purely diagnostic; no runtime/algorithmic cost.
- **Fix:** Mirror `mesh_cache_hits` / `fetches_cancelled` (from `GeoStats`) and `mesh_cache()` occupancy into `StreamShared` atomics in the same block as `cached_bytes` (one relaxed store/advance each). Add hit/miss `AtomicU64`s to `DiskCache` (bump in `get` on the `contains_key` branch, `lib.rs:585`) surfaced via `HttpSource`. Add one packed FFI accessor (mask lanes like `logi_globe_world_failed_counts`, `ffi.rs:941`), documented in `logi_geo_ffi.h` near line 186. **Additive ABI bump required** (new field/accessor → `LOGI_ABI_VERSION` + matching `offset_of!`/`abi_layout.rs` asserts).
- **Big-O:** N/A → `O(1)` stores/advance + `O(1)` increments/get.
- **Confidence:** High. **(Author Medium/Low → corrected Low.)**

#### L2 — `MeshCache::trim` sorts ALL entries (O(n log n)); sibling evict() does the same selection in O(n)
- **File:** `geo_world.rs:361-384`.
- **Problem:** `trim()` scores every entry into a `Vec` (`O(n)`) then does a FULL `sort_unstable_by` (`geo_world.rs:375`) over all n entries, but only needs the coldest k to reach the 80% low-water mark — it fully orders the ~80% it keeps for nothing. The sibling resident evictor solves the identical "farthest+stalest top-k" problem with `select_nth_unstable_by` (`O(n)` avg, `geo_world.rs:648-650`); the sibling `DiskCache` uses an `O(log n)` `BTreeMap`. `MeshCache` is the one cache paying `O(n log n)` for a partial selection.
- **Impact:** Amortized (no-op under budget; fires ~once per 20% turnover), so negligible on a slow camera — but it runs on the stream advance thread inside `evict` (`geo_world.rs:668`), and under a fast/teleporting camera that thrashes the working-set boundary it fires every step, the log-factor over a few-thousand entries competing with decode/publish. Also a gratuitous two-evictors-one-slow inconsistency.
- **Fix:** Mirror `evict()`: keep the `O(n)` `scored` collect, compute bytes-to-evict, then `select_nth_unstable_by` to partition only the overflow entries. **Caveat the author undersells:** `evict`'s `select_nth` is trivial because `n=overflow` is a known *count*; `trim` terminates on a *byte* budget over variable-size entries, so there is no known split index — the fix needs a count estimate + top-up loop, or a `select_nth`-partitioned prefix. More than a one-line swap.
- **Big-O:** `O(n log n)` → `O(n)` avg; space `O(n)` unchanged.
- **Confidence:** High. **(Author Medium → corrected Low — absolute cost is tiny, not shown on a flamegraph; fix opportunistically for consistency.)**

#### L3 — `tile_key` is 64-bit FNV-1a while sibling `cache_key` is 128-bit by an explicit anti-collision decision (M16)
- **File:** `geo_world.rs:2387-2406`; sibling `lib.rs:505-515`.
- **Problem:** `tile_key()` is 64-bit FNV-1a with a documented-accepted ~1e-13 birthday risk; `content_key()` extends the same 64-bit space per multi-content index. The sibling `cache_key()` made the *opposite* call (FNV-1a-128) for the identical aliasing hazard, explicitly citing audit M16 as rejecting 64-bit. A `MeshCache`/C++ section-cache collision aliases two tiles' geometry.
- **Impact:** Genuinely low probability; consequence is a single visibly-wrong tile (not a crash/corruption) that self-heals on next eviction (the key leaves the bounded LRU map). The actionable issue is a **reviewer-visible inconsistency**, not a bug at current scale.
- **Fix:** **Recommended option (b) only — a one-line cross-reference** in the `tile_key` note explaining 64-bit is acceptable here (transient render-section key, self-heals; a `DiskCache` collision serves wrong bytes *persistently* from disk — the distinction that justifies the divergence) while 128-bit was required for the durable disk identity. **Do NOT take option (a) (widen to `u128`)** — `LogiGlobeTile.tile_key` is `uint64_t` in the ABI (`logi_geo_ffi.h:120`, `offset_of==0`, in `abi_layout.rs:70`), so it would force an ABI bump for no current correctness gain. **Caveat:** the author's sub-claim that the collision `n` is the "session distinct-path total" is statistically overstated — the correct `n` is the *co-resident-in-table* population (bounded LRU); do not let that argument drive an escalation. **This becomes load-bearing the moment the cross-session decoded tier (M3) lands** (key population climbs), at which point revisit the `(origin, path-hash)` + width question.
- **Big-O:** `O(path_len)` unchanged (documentation-only).
- **Confidence:** High. **(Low, unchanged.)**

#### L4 — DEDUP: MeshCache byte budget under-estimates true resident RAM (Arc shared with published frames) + fixed-constant budget not derived from the working set
> **Merge note:** two related accounting/clarity findings are grouped here because both concern *what the 384 MiB budget actually bounds*: (a) the budget counts `geom.byte_size()` once even when a published `GeoRenderFrame` still holds an `Arc::clone`, and the resident-set meshes aren't in the cache budget at all; (b) the budget is a hard constant unrelated to `MaxResident`/avg tile size. They share the same fix surface (doc + the 0=default derivation) and the same conclusion (correct LRU behavior; clarity/tuning, not a bug). Kept as one Low item with both halves.

- **File:** `geo_world.rs:199-203` (constant + doc), `306-310,336-348,705-707` (Arc sharing), `2229-2235` (`set_mesh_cache_budget`); `LogiBakedGeoTerrainActor.cpp:294` (override).
- **Problem (a):** `insert` counts `geom.byte_size()` once per entry regardless of other `Arc` owners (`geo_world.rs:337`); a just-evicted tile deposited into the cache (`658`) can be simultaneously pinned by a still-live published frame's `Arc::clone` (`707/722`), counted once. Resident meshes aren't in the cache budget at all (the cache only holds *evicted* meshes; `take` moves them out on restore). So the 384 MiB bounds *cache-owned* decoded bytes, not total decoded RAM (= resident + last published frame + cache, with overlap). **Problem (b):** `DEFAULT_MESH_CACHE_BYTES` is a hard 384 MiB with no relationship to `MaxResident` (18432) × avg `byte_size`; a fixed constant cannot stay near the hit-rate sweet spot across providers/LOD/hardware.
- **Impact:** Correct LRU behavior (total RAM *is* bounded — the resident set is separately capped by `MaxResident`), but the "bounds RAM" doc phrasing invites mis-tuning the budget as a whole-process RAM ceiling. Compounds L1 (no hit-rate signal to hand-tune).
- **Fix:** **(a)** Doc-only: tighten `geo_world.rs:201` / the `MeshCache` rustdoc (`306-310`) to say it bounds *cache-owned (evicted-but-retained) decoded bytes, in front of the separately-capped resident set* — not total RAM. **Do NOT discount shared bytes via `Arc::strong_count`** — the bytes are still resident, so discounting would understate RAM. **(b)** Keep the authoritative FFI override (`mesh_cache_budget_mb > 0`) as-is. **Do NOT implement the author's `DEFAULT = k·MaxResident·EWMA` with k≈1.5-2** — that sizes the buffer at ~1.5-2× the *full* working set (~2.3 GB at 85 KB × 18432, ~6× today's 384 MiB, hostile to the 8 GB target). A re-entry buffer should be a *fraction* of the working set (k≪1). Also note the create-time chicken-and-egg (no `byte_size` sample until tiles decode). Defer any hit-rate-driven adaptation until L1 lands.
- **Big-O:** `O(1)` unchanged (doc + cold-path derivation).
- **Confidence:** Medium. **(Both Low, unchanged.)**

#### L5 — MeshCache warm-restore does two hash lookups (`contains_key` then `take`) for the same key per warm hit
- **File:** `geo_world.rs:1697` (contains_key), `1756` (take), `329-356` (MeshCache).
- **Problem:** `select_request` calls `contains_key(key)` (`1697`) then `restore_cached` calls `take(key)` (`1756`) — two hashbrown lookups + two `resource`/`resource_mut` fetches of the same 8-byte key per warm hit. `take` already returns `Option`, so the probe is redundant; it exists only because the FreeList slot is secured between the two (pop-then-take, no-leak ordering).
- **Impact:** Minor, on the per-tile-per-frame warm-hit path. Pure removable work (CLAUDE.md §1E "the cheapest car is the one you never drive"). Not an `O()` change.
- **Fix:** Make `restore_cached` return `bool` (false at the two early-returns `1753-1755`/`1756-1759`, true at fn end); caller becomes `if self.restore_cached(lt, key, view.frame) { continue; }` and drops `contains_key` at `1697`. **Preserve the pop-FreeList-before-take / push-back-on-miss ordering** so the no-leak guarantee is unchanged. Verified behaviorally identical for cold tiles and empty-FreeList passes.
- **Big-O:** 2 lookups + 2 resource accesses/warm hit → 1 each.
- **Confidence:** High. **(Low, unchanged.)**

---

## 4. Bullet-proofing checklist

Legend: ✅ holds today · ⚠️ partial · ❌ missing.

### DiskCache (`logi_3dtiles_io/src/lib.rs`)

| Property | Status | Evidence / gap |
|---|---|---|
| **Torn-write safety (in-process)** | ✅ | Process-unique `tmp{seq}` + atomic `rename` prevents two workers interleaving one URL into a torn blob (Issue-D, `lib.rs:607-620`). |
| **Crash leaves no partial `.bin`** | ✅ | A crash mid-write strands only a `.tmp`, never a half `.bin` (rename is atomic). |
| **Orphaned `.tmp` reclamation** | ❌ | **M2:** `.tmp{seq}` from a crashed write is never deleted, never budgeted; disk grows unbounded. Fix = delete non-`.bin` in `open`'s scan. |
| **Content integrity on read** | ❌ | **M1:** no hash/length check (`lib.rs:589`); a corrupt file is permanently served. Fix = filename CRC32C + verify-on-read. |
| **Read-failure self-reconcile (TOCTOU)** | ⚠️ | **L-dup'd (see note):** a failed `fs::read` after a re-put-vs-deferred-unlink race leaves a phantom index entry inflating `total` until the key is re-requested or evicted (`lib.rs:581-594`). Bounded; self-heals on re-request. The code's own comment (`lib.rs:621-625`) documents the race. Fix = remove entry + lru tick + `saturating_sub` on read-miss. |
| **Concurrency: I/O outside the lock** | ✅ | `fs::read` (get) and `remove_file` (evict) both happen outside the index `Mutex` (`lib.rs:589`, `633-635`) — disk I/O never serializes the worker pool (H5). |
| **Mutex poisoning** | ✅ (degrades gracefully) | Every `idx.lock()` uses `.ok()?` / `unwrap_or(0)` / `Err(_) => return` (`lib.rs:584,590,626,640`) — a poisoned lock yields a benign cache miss / skipped put, never a panic. |
| **Lock contention** | ⚠️ | Single global `Mutex<CacheIndex>` (`lib.rs:528`) serializes all worker get/put bookkeeping (+ `O(log n)` BTreeMap under it). Holds are I/O-free so it's a "measure-first" — stripe + CLOCK if a flamegraph shows it hot. |
| **Disk-full / write failure** | ⚠️ | `put` silently returns on `fs::write` error (`lib.rs:614-615`) — correct (best-effort cache), but there is no telemetry/log of write failures, so a full disk is invisible. (Ties to L1.) |
| **Restart / reopen correctness** | ⚠️ | `open` rebuilds the index from a dir-scan, but recency = scan order, not real LRU (documented, `lib.rs:533-535`) → first cold-start eviction is arbitrary. Persisting the index (Research) fixes both this and the `O(F log F)` cost. |
| **Empty/failed-body poisoning** | ✅ | `put` early-returns on empty bytes (`lib.rs:599`); `fetch_one` caches only successful non-empty bodies — failures/cancels/empty-200 never poison the cache. |
| **Duplicate-shard double-count** | ⚠️ (unreachable) | **L-confirmed:** a same-key `.bin` in two shard dirs would double-count `total` + orphan an lru tick (`lib.rs:540-567`), but this code's writes always map a key to one shard via `cache_path` — only external tampering triggers it. Defensive-hardening only. |

### MeshCache / Flash Cache (`logi_3dtiles_scene/src/geo_world.rs`)

| Property | Status | Evidence / gap |
|---|---|---|
| **Thread-safety** | ✅ | A Bevy `Resource` accessed single-threaded on the stream-owned advance thread (`evict`/`select_request`); no cross-thread mutation, no lock needed. |
| **Crash-safety** | ✅ (trivially) | RAM-only — nothing to corrupt on disk; a crash just loses the warm window. |
| **Byte accounting** | ⚠️ | Running `self.bytes` is exact for *cache-owned* bytes (`take` discounts immediately, `geo_world.rs:354`), but **L4:** it under-represents total decoded RAM (Arc shared with published frames; resident set excluded). Correct behavior, misleading doc. |
| **Eviction integrity** | ✅ | NaN-safe comparator (`unwrap_or(Ordering::Equal)`, `geo_world.rs:375`); `select_nth`/sort drop the correct coldest entries; debug-assert drift guard (compiled out in release). |
| **Restart / reopen** | ❌ | **M3:** no persistence — every cold start re-decodes (Draco + rebase) tiles whose *bytes* are on disk. Fix = persisted decoded tier keyed on `(origin, path-hash)`. |
| **Collision safety** | ⚠️ | **L3:** 64-bit `tile_key` (documented-accepted), vs 128-bit on the disk side. Self-heals on eviction; becomes load-bearing once M3 lands. |
| **Observability** | ❌ | **L1:** hit-rate + occupancy not surfaced through the lock-free FFI; budget tuned blind. |
| **Eviction cost** | ⚠️ | **L2:** `trim` full-sorts (`O(n log n)`) where the sibling uses `select_nth` (`O(n)`). Negligible slow-camera, real under teleport thrash. |

---

## 5. Cross-session reuse verdict

**Does the disk cache truly reuse tiles across sessions? — Yes for bytes, with three caveats; and there is a real, larger gap one tier up (decode is never reused).** The user's suspicion is justified, but the issue is *not* that the byte cache fails to reuse across sessions — it does.

**What works:** the `DiskCache` is genuinely persistent and session-independent. The key (`cache_key`, `lib.rs:505-515`) is an FNV-1a-128 over the **query-stripped URL path**, which is stable across sessions and across ephemeral query parameters (e.g. P3DT session tokens) — so a tile fetched in session 1 is found and served from disk in session 2 with no network (`fetch_one` checks `cache.get` before `http_get`, `lib.rs:421-426`). The cache dir (`Saved/P3DTCache`) survives restarts. So byte-level cross-session reuse is real and working.

**The three caveats / gaps:**

1. **Decode is never reused across sessions (the real gap — M3).** Byte reuse only saves the *network*. On every cold start, even a 100%-disk-hit revisit to a previously-explored region re-runs the **full Draco decode + ENU rebase** (`geo_world.rs:536-577`, `gltf/lib.rs:90-103`) — the dominant per-tile CPU cost. The `MeshCache` that *would* skip decode is RAM-only and dies with the session (`geo_world.rs:306-318`). So "reuse across sessions" is half-delivered: cheap network, full CPU. This is the gap most likely behind the user's perception that revisiting a place after relaunch still hitches.
2. **First cold-start eviction is arbitrary (recency not persisted).** `open` rebuilds the LRU order from *scan order, not real recency* (documented, `lib.rs:533-535`), so the first eviction pass after a restart may drop genuinely-hot tiles. Self-corrects once `touch`/`insert` re-establish recency, but the persisted-index research item fixes it outright.
3. **A corrupt persisted file is permanently reused as garbage (M1).** Because the cross-session hit short-circuits re-fetch (`lib.rs:424`), any byte-rot/truncation introduced *between* sessions is served verbatim forever — cross-session persistence is exactly what makes the missing integrity check (§3 M1) bite. (And M2's orphan `.tmp` leak is a cross-session disk-growth bug.)

**Verdict:** byte-level cross-session reuse is correct and working; the genuine cross-session deficiencies are (1) no decoded-geometry persistence → repeated decode cost, (2) arbitrary first-eviction from a non-persisted index, (3) no integrity guard on persisted bytes. Fixing M3 + the persisted index (with the `(origin, path-hash)` key from §3 M3) + M1 together makes cross-session reuse genuinely bullet-proof.

---

## 6. Integrity-hashing recommendation

**Recommended scheme: a 32-bit CRC32C embedded in the cache filename, verified on read.** This is the concrete form of §3 M1 and the Research "XXH3-128 integrity" item, tuned to what's already in-tree.

- **Hash choice — CRC32C via `crc32fast`.** `crc32fast` is **already in `Cargo.lock`** (transitive via `flate2`), SIMD/hardware-accelerated (~7+ GB/s), pure-Rust, no new external footprint. Torn-write/truncation/bit-rot detection — the actual threat — needs *change detection*, not cryptographic collision resistance, so CRC32C is the right default. (If a stronger self-verifying / content-addressed scheme is ever wanted, XXH3-128 via `xxhash-rust` at ~30 GB/s is the upgrade; the sibling `logi_semantic` bake already uses a trailing CRC-32, so CRC is the consistent in-tree precedent.) **Add `crc32fast` as a *direct* dep of `logi_3dtiles_io`** (it's only transitive today).
- **Where stored — in the filename: `<32-hex-key>.<8-hex-crc>.bin`.** This survives `open`'s scan-rebuild with **no sidecar, no header parse, no extra `stat`/read** — the expected CRC is parsed from the file stem alongside the key. (If the index is later persisted to a sidecar/`redb` per Research, store the digest there instead and drop it from the name.)
- **Verify-on-read.** In `get`, after `std::fs::read` (`lib.rs:589`), recompute `crc32fast::hash(&bytes)` and compare to the stem's CRC. On mismatch: `remove_file` + drop the index entry (+ lru tick, + `saturating_sub` on `total`) + return `None` → `fetch_one` cleanly re-fetches (the existing graceful-miss path). In `open`, parse both key and CRC from the stem; **defer the byte-verify to first read** (don't hash all F files at startup). In `put`, compute the CRC before write and bake it into the path.
- **Cost.** `get` = `O(size)` read + `O(size)` CRC, single streaming pass, memory-bandwidth-bound and fully overlapped with the inevitable decode — a rounding error next to the bandwidth + decode a poisoned hit would otherwise waste. `put` = same + one CRC. `open` unchanged (deferred). Storage cost: 8 hex chars in the filename per entry (negligible).
- **Net effect.** Converts an invisible, *permanent* corruption (currently a confusing decode failure or a wrong-looking tile that never self-heals) into a clean, self-healing cache miss + re-fetch. On the **key** itself, no change is needed: FNV-1a-128 already makes accidental cross-URL key collision negligible (M16); the integrity gap is about file *contents* drifting from a correct name, which the content CRC closes.
- **Free alternative.** Adopting `redb` for the store (Research §7) subsumes this entirely — it already checksums every page with XXH3-128 in a Merkle tree and rolls back partial commits. If `redb` lands, the integrity item costs zero added code.

---

## 7. Research-backed improvement roadmap

Mapped to this code, ordered by ROI. Effort is rough (S ≈ hours, M ≈ a day or two, L ≈ multi-day + prototype gating).

| # | Improvement (Research item) | Maps to | Expected win | Big-O change | Effort | ROI verdict |
|---|---|---|---|---|---|---|
| 1 | **Persist the DiskCache index** (sidecar, or `redb` table `{u128 → (size,tick)}`) | `lib.rs:536-578` open scan | Startup: ~F `stat()`s → one sequential read; **true LRU recency survives restart** (fixes the arbitrary first-eviction, §5 caveat 2); attacks the documented NTFS many-small-files cold-start cost | open `O(F log F)` + F syscalls → ~`O(E)` one read; build `O(E)` (pre-sorted) | M | **ADOPT** — clearest scale win; fall back to scan only after a crash (self-healing) |
| 2 | **Content-integrity hash on disk entries** (CRC32C in filename / XXH3-128) | `lib.rs:589` get; §3 M1; §6 | Silent corruption → clean re-fetch; bullet-proofs the cross-session persistence that makes corruption bite | get `O(size)` → `O(size)` read + `O(size)` hash (overlapped) | S | **ADOPT** — `crc32fast` already in-tree; **FREE if (1) adopts `redb`** (built-in XXH3-128 Merkle + rollback) |
| 3 | **Scan-resistant FIFO-class eviction for the Flash Cache** — SIEVE (~20 LOC, zero deps) or S3-FIFO via `quick_cache` (sync, byte-`Weighter`) | `geo_world.rs:361-384` trim; §3 L2 | Removes the per-trim `O(n log n)` sort; raises hit-rate on the orbit/reverse re-entry the cache exists for (S3-FIFO measured >6× faster/op than optimized LRU; SIEVE beats LRU on >45% of traces) | trim `O(n log n)` → `O(1)` amortized/evict | M | **PROTOTYPE** — start with hand-rolled SIEVE (no new dep, fully auditable); benchmark hit-rate on a recorded fly-through vs the current distance·staleness scorer. Caveat: a fast teleport could briefly retain far tiles (keep `distance` as a tie-break weight or admission signal) |
| 4 | **Persist decoded geometry across sessions** (rkyv zero-copy mmap, or the same `redb`/pack tier in a separate namespace) | `geo_world.rs:306-395,536-577`; §3 M3 | Cross-session (and beyond-RAM-window) revisit skips the dominant Draco decode + rebase → near-memcpy warm restore | revisit `O(decode + V)` → `O(V)` page-in / memcpy | L | **PROTOTYPE, gated on a decode-cost flamegraph** — biggest CPU win in the framework, but a whole new byte-budgeted + integrity-hashed + **version-stamped** tier. **Key on `(origin, path-hash)`, not path-hash alone** (vertices are baked relative to `enu.origin_ecef`). rkyv archives are layout-version-fragile → stamp + discard-on-mismatch (no-compat) |
| 5 | **Frequency admission (W-TinyLFU)** — Count-Min sketch gate so one-hit-wonder tiles don't evict the hot home region | `geo_world.rs:336` insert, `lib.rs:598` put | Highest single hit-rate lever for a scan+locality workload (W-TinyLFU ~99% of Belady-optimal on skewed traces) | admission `O(1)` (fixed sketch) → `O(1)` with frequency signal; +~8 B/entry | M | **CONSIDER — as the alternative to evaluate against #3, not in addition** (S3-FIFO's small-queue + ghost already approximates frequency admission). Measure both on the same recorded fly-through |
| 6 | **Velocity-predictive prefetch** (second coarse traversal along the dt-free displacement vector, gated behind the in-flight cap) | `geo_world.rs:2154-2173,2376-2385`; §3 M4 | Hides fetch+decode latency behind motion — converts first-approach-at-speed misses into warm hits; complements the existing cancel-on-stale | selection `O(selected)` → +one `O(selected)` coarse pass per moving frame | M | **CONSIDER** — lower ROI than the cache internals (cancel-on-stale already captures the revisit benefit). **Must use displacement, not wall-clock `dt`** (determinism), and only the second-traversal variant prefetches |
| 7 | **Stripe / shard the DiskCache index lock** (`Vec<Mutex<CacheIndex>>` by key bits, or a concurrent-cache crate) | `lib.rs:528,581-636` | Removes the worker-pool metadata serialization point as `workers` scales (up to 32) | per-op global-serial → sharded ~`O(log(n/S))` | M | **CONSIDER / measure-first** — holds are already I/O-free, so this is only worth it if a flamegraph shows the Mutex hot. Per-shard budget is the one design wrinkle |
| 8 | **Single pack-file store vs millions of `.bin`** | `lib.rs:518-520` cache_path | Fewer inodes/MFT records, faster enumeration + eviction (the NTFS small-file fix) | reads `O(1)` mmap+offset; writes append | M (delegated to `redb`) / L (hand-rolled) | **CONSIDER — subsumed by `redb`** (which also delivers #1, #2). Hand-rolling free-space management is real work; a hybrid (pack small, keep large GLBs as files) captures most of the win |
| 9 | **Widen `tile_key` to 128-bit** | `geo_world.rs:2397-2406`; §3 L3 | Removes the silent-aliasing failure mode | `O(path_len)` unchanged | M (ripples through FFI ABI) | **CONSIDER — defer; doc-only today (option b).** Becomes load-bearing once #4 lands (key population climbs). Cheaper mitigation: store path + verify on `take` (stays 64-bit on the wire) |
| 10 | **Content-addressed disk keys (dedup)** | `lib.rs:505-515` | Dedup byte-identical tiles | lookup `O(1)`; +URL→hash indirection | M | **REJECT for P3DT** — per-tile-unique photographic atlases ⇒ near-zero dedup; the self-verification benefit is fully captured by the cheaper #2 |

**The combined move:** items 1 + 2 + 8 are *all delivered by adopting `redb`* (pure-Rust ACID B-tree, the same class of vendored exception already accepted for Draco): the index becomes the directory, integrity comes from built-in XXH3-128 Merkle checksums + crash rollback, and the pack-file consolidation is internal. That single decision is the highest-leverage durability investment.

---

## 8. MeshCache → "Flash Cache" rename plan

The rename was adversarially confirmed against a fresh workspace grep; below is the exact ordered edit list and the ABI verdict.

### ABI-bump verdict

**The FFI field `mesh_cache_budget_mb` is binary-compatible under a pure source rename — NO `LOGI_ABI_VERSION` bump is required.** The field is `pub mesh_cache_budget_mb: u32` at **offset 44** in `#[repr(C)] LogiGlobeCreateInfo` (`ffi.rs:338`), mirrored as `uint32_t mesh_cache_budget_mb;` (`logi_geo_ffi.h:115`). `#[repr(C)]` layout is fixed by declaration order + types; a rename changes only the source identifier, not the type (`u32`), offset (44), struct size (56), or alignment (8). The ABI tests pin offset/size **by name** (`abi_layout.rs:65 => 44`, `ffi.rs:455 offset_of! == 44`, `ffi.rs:448 size == 56`), so they must be edited to the new name in lockstep but the asserted **values stay 44/56**. The project's own v6 changelog precedent confirms the rule: it bumped *only because C++ changed how it read the bytes (semantics)*, not for a layout-identical change — and a rename changes neither layout nor semantics.

**Recommended scope (consistent with the rename-scope findings):** rename the *internal Rust symbols* fully (so the codebase reads coherently), but **keep the FFI field `mesh_cache_budget_mb` and the C++ `GlobeMeshCacheBudgetMB` UPROPERTY as-is** (update only the user-facing `DisplayName` / doc text). The FFI field is a stable cross-language contract; renaming it churns the header + C++ write site + two test files for zero functional gain and risks a header/C++ desync build break. If product *insists* on `flash_*` on the wire, the rename is still binary-compatible (asserts stay 44/56, no version bump) but requires 8 coordinated edits (`ffi.rs:338` decl, `ffi.rs:550-551` read, `ffi.rs:446/455` assert+comment, header L115, `abi_layout.rs:65`, `globe_contract.rs:53,98`, and `LogiBakedGeoTerrainActor.cpp:294`) — and the header + C++ name **must** match or the C++ TU fails to compile.

### Ordered internal-Rust edit list (items 1-22)

Apply together (per-file standalone compilation is not required). Every omission is a hard compile error or a `cargo doc`/clippy `-D warnings` intra-doc-link failure.

**`geo_world.rs`:**
1. L203 `pub const DEFAULT_MESH_CACHE_BYTES` → `DEFAULT_FLASH_CACHE_BYTES`
2. L202 doc `[GeoWorld::set_mesh_cache_budget]` → `set_flash_cache_budget`
3. L291 `struct MeshCacheEntry` → `FlashCacheEntry`
4. L313-318 `pub struct MeshCache` → `FlashCache` + field type `HashMap<u64, MeshCacheEntry>` → `FlashCacheEntry`
5. L320 `impl MeshCache` → `impl FlashCache`
6. L338 `let entry = MeshCacheEntry` → `FlashCacheEntry`
7. L406 doc `(MeshCache)` → `(FlashCache)` *(type-link reference; distinct from the deferred `mesh_cache_hits_total` field at L408 — this one renames with the type)*
8. L591 `mut cache: ResMut<MeshCache>` → `ResMut<FlashCache>`
9. L580 doc `[MeshCache]` → `[FlashCache]`
10. L1033 `MeshCache::new(DEFAULT_MESH_CACHE_BYTES)` → `FlashCache::new(DEFAULT_FLASH_CACHE_BYTES)`
11. L1697 `resource::<MeshCache>()` → `<FlashCache>`
12. L1743 doc `[MeshCache]` → `[FlashCache]`
13. L1756 `resource_mut::<MeshCache>()` → `<FlashCache>`
14. L1946 doc `[MeshCache]` → `[FlashCache]`
15. L2208-2212 accessor `pub fn mesh_cache(&self)` → `flash_cache` + `resource::<MeshCache>()` → `<FlashCache>`
16. L2229-2234 `pub fn set_mesh_cache_budget` → `set_flash_cache_budget`, body `resource_mut::<MeshCache>()` → `<FlashCache>`, doc `[DEFAULT_MESH_CACHE_BYTES]` → `[DEFAULT_FLASH_CACHE_BYTES]`
17. L2388,2391,2394,2410 four doc-comment `MeshCache` → `FlashCache`
18. tests L2831 fn `mesh_cache_evicts_…` (rename optional, cosmetic), L2843/L2857 `MeshCache::new` → `FlashCache::new`, L2923/2934/3001/3012 `w.mesh_cache()` → `w.flash_cache()`

**`ffi.rs`:**
19. L336 doc `[DEFAULT_MESH_CACHE_BYTES]` → `[DEFAULT_FLASH_CACHE_BYTES]`
20. L551 `world.set_mesh_cache_budget(...)` → `world.set_flash_cache_budget(...)`

**`streaming.rs`:**
21. L51 doc `mesh LRU (MeshCache)` → `flash cache (FlashCache)`

**`Cargo.toml`:**
22. L13 comment `mesh cache` → `flash cache`

**Deliberately NOT renamed** (own findings): the FFI field `mesh_cache_budget_mb` (`ffi.rs:338` + ABI asserts `446/455` + header L115 + `globe_contract.rs:53/98` + `abi_layout.rs:65`), and `mesh_cache_hits_total` (`geo_world.rs:408` def + `1780` write + tests `2993/3006`).

**Verification:** `cargo build -p logi_3dtiles_scene && cargo build -p logi_geo_ffi && cargo test --workspace && cargo clippy --workspace -- -D warnings`. A final `rg -i 'MeshCache|mesh_cache'` over `Rust/` should then return **only** `mesh_cache_budget_mb` and `mesh_cache_hits_total`.

---

*Relevant files (all absolute):* `C:\Users\premi\work\LogiUE\Rust\crates\geo\logi_3dtiles_io\src\lib.rs` · `C:\Users\premi\work\LogiUE\Rust\crates\geo\logi_3dtiles_scene\src\geo_world.rs` · `C:\Users\premi\work\LogiUE\Rust\crates\geo\logi_3dtiles_scene\src\stream_thread.rs` · `C:\Users\premi\work\LogiUE\Rust\crates\geo\logi_3dtiles_scene\src\streaming.rs` · `C:\Users\premi\work\LogiUE\Rust\crates\geo\logi_geo_ffi\src\ffi.rs` · `C:\Users\premi\work\LogiUE\Rust\crates\geo\logi_geo_ffi\include\logi_geo_ffi.h` · `C:\Users\premi\work\LogiUE\Rust\crates\geo\logi_gltf\src\lib.rs` · `C:\Users\premi\work\LogiUE\Plugins\LogiBevy\Source\LogiBevyRuntime\Private\LogiBakedGeoTerrainActor.cpp`