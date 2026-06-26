<!-- Generated 2026-06-08 by a 45-opus-agent audit workflow (wf_f734c20b-301): 7 lenses (transport, worker-pool/FIFO-queue, scheduling+preemption, retry/backoff, knobs+O-notation census, memory-flow, client/HTTP2 research) + adversarial verify + synthesis. -->

# Geo HTTP Tile-Streaming Stack — Microscopic Audit Synthesis

**Scope:** the ADR-0009 native Rust 3D Tiles fetch path — `logi_3dtiles_io::HttpSource` (worker pool, transport, disk cache) and `logi_3dtiles_scene::geo_world` (selection → priority → dispatch → drain), plus the `stream_thread` owner loop and the FFI/C++ knobs that feed them. All line citations re-verified against the current working tree (heavy uncommitted changes present; cited code confirmed live, not mid-rewrite).

---

## 1. Executive summary

**State of the stack.** The pipeline is **correct and does not leak**. Caches are O(1)/O(log n) on the hot path (no O(n²) anywhere), cancellation is genuinely mid-download (per-chunk `AtomicBool` probe, not the old download-then-discard), failure classification (transient vs terminal vs auth-expired) is sound and well-tested, and the keep-alive connection pool was already tuned to kill the re-handshake throttle that previously capped 8 workers at ~8 MB/s. There are **no correctness/crash/corruption bugs**. So the stack is *sound* — but it does **not deliver the user's stated goals**, and there is **one real liveness bug** that can silently freeze streaming.

The gap between "works" and "what you asked for" is concentrated in **three structural choices, all in the request→worker hand-off**, plus **one transport ceiling**:

**Top throughput wins (in leverage order):**
1. **HTTP/2 multiplexing** (ureq 2.12.1 → an h2-capable client). P3DT is *one host* (`tile.googleapis.com`) that **negotiates ALPN h2 today** (probe-confirmed) and advertises h3. Blocking HTTP/1.1 caps wire concurrency at `W` (workers, ≤32 in the shipped UI) — one in-flight request per TCP/TLS connection. A single h2 connection multiplexes ~100 concurrent streams, turning the ceiling from *connection count* into *streams × connections*. Highest ceiling-raiser for the small-tile P3DT workload.
2. **Couple `max_in_flight` to the worker count** (today a hardcoded `128`, fully decoupled). It is a *sizing policy*, not concurrency — at 16 workers ~112 of the 128 slots are pure queue backlog.
3. **Offload the synchronous disk writes** (cache `put` on the fetch worker; geom serialize+write inside the decode `par_iter`) to a dedicated low-priority writer thread (Arc hand-off, no copy).

**Top scheduling wins:**
1. **Replace the FIFO request channel with a priority-ordered hand-off** so the per-step `fetch_priority` sort actually governs *service* order, not just *enqueue* order. This is the single change that makes "highest-priority-to-render fetched first" true.
2. **Re-rank in-flight/queued work each pass** against the live camera (the priority is view-dependent and currently frozen at submit, so a camera *turn* cannot re-prioritize submitted work).
3. **Priority preemption** — cancel the lowest-priority in-flight to admit a higher-priority newcomer when the budget is full (cancellation is cheap and the user waived its cost).
4. **Fold subtree fetches into the same priority order** (they gate refinement but enqueue in traversal order, unprioritized, ahead of the ranked tile loop).

**Top bugs (must-fix):**
- **Frozen-clock backoff (the one genuine liveness bug, Medium).** The retry timer is keyed on `frame`, which only advances while there is work in flight or the camera is moving. A 429/5xx wave that drains in-flight to 0 over a still camera *freezes `frame`*, making the retry deadline unreachable — the region stays stuck at coarse LOD until an unrelated camera move, with no telemetry. Same root cause re-bites the **session-refresh cooldown** (also `frame`-keyed) and the session refresh additionally does a **blocking `http_get` on the stream thread** (up to 60 s stall).

**Is it sound, or are there must-fix bugs?** It is **architecturally sound and safe to run** — but the frozen-clock backoff is a real correctness-of-liveness bug that should be fixed, and the scheduling/transport structure must change to meet the stated throughput + priority goals. Nothing here is a crash; everything is "streams correctly, just sub-optimally, and can stall its own retries."

**Dedup note.** Seven of the deferred findings are facets of two defects (the FIFO-vs-priority gap and the `max_in_flight` decoupling), seen through different lenses (transport, workerpool-queue, scheduling-priority, memory-flow, redesign). They are consolidated below into **C1** and **H2** respectively, with the per-lens nuances preserved.

---

## 2. Confirmed findings (deduped, grouped by corrected severity)

Severities are the **adversarially-corrected** ones (several were downgraded from the proposers' initial ratings). Confidence is the verifier's.

### CRITICAL
*None.* (The "FIFO" finding was proposed critical but corrected to High: it degrades scheduling fidelity gracefully — `cancel_stale` drains the off-screen tombstones at memory speed — rather than crashing or corrupting.)

### HIGH

**H1 — FIFO request channel defeats the priority sort; submitted work cannot be re-ranked or preempted**
*(consolidates the 6 "FIFO/priority" findings across the transport, workerpool-queue, scheduling-priority, and memory-flow lenses)*
- **File:** `logi_3dtiles_io/src/lib.rs:800` (`flume::unbounded`), `:811` (`req_rx.recv()` FIFO); producer at `geo_world.rs:1786-1790` (sort) → `:1795-1868`/`:2174` (enqueue).
- **Problem:** `select_request` decorate-sorts `sel.load` by `fetch_priority` and enqueues highest-first, **but the `f64` priority is never carried into the channel** — only `(RequestId, url, CancelFlag)` is sent, into a strict-FIFO MPMC channel. The sort therefore orders only *this step's* enqueue burst. With `max_in_flight=128` and ~16 workers, ~112 already-queued requests sit ahead of a tile that becomes top-priority on a *later* pass (camera turned). Three coupled consequences: **(a)** cross-step priority inversion — fresh high-priority tiles land at the FIFO tail; **(b)** no re-ranking — `fetch_priority` is *view-dependent* (`align = dot(dir_to_tile, cam_dir)`) yet frozen at submit, so a camera *rotation* (the common interaction) cannot re-prioritize submitted work; **(c)** no preemption — when the budget is full the dispatch loop just `break`s (`:1836`); `cancel_stale` only frees tiles that **left** the selection (`:2110` filter), never ones merely lower-priority than a newcomer.
- **Fix:** Replace the FIFO with an **owner-side max-heap keyed on `fetch_priority`**, drained by workers. Two viable shapes — (A) `Mutex<BinaryHeap<PrioReq>>` + `Condvar` inside `HttpSource`, workers pop-max; or (B, simpler, fits the poll model) the **owner holds the pending heap**, re-sorts the still-wanted set against the live camera each pass, and tops the channel up to only `~workers` deep so the channel never accumulates a reorderable backlog. Add priority **preemption** (cancel the min-priority in-flight to admit a higher newcomer, gated by a margin to avoid thrashing). Carry `priority: f64` through `TileContentSource::request` (ADR-0004: no compat). Heap removal then makes cancel-while-queued O(log n) instead of leaving a dequeue tombstone.
- **Big-O:** enqueue O(1)/dequeue FIFO (priority lost across passes) → **push/pop/reprioritize O(log n)**, `n` = queued ≈ `max_in_flight` (hundreds; ~9 comparisons). The owner-side re-sort is O(n log n)/pass — *already paid today* at `:1790`.
- **Confidence:** High. **Caveats:** option (B) preferred (re-ranking keeps priority camera-fresh; static heap keys would go stale). Preserve `MANIFEST_FETCH_BOOST` so manifests/subtrees still outrank same-distance geometry. Mind all four in-flight removal sites (drain, cancel_stale, prune, fail) when maintaining the heap (lazy-delete-on-pop is the clean way). Swapping `flume` for `Mutex+Condvar` requires re-implementing the Drop teardown (a shutdown flag + `notify_all`, see H3).

**H2 — `max_in_flight` is a hardcoded 128, fully decoupled from the worker count**
*(consolidates the 5 "max_in_flight" findings; corrected to Medium by most verifiers, but elevated here to High-adjacent because it is the prerequisite that makes H1's backlog exist and the one explicit user ask — kept at the top of §3)*
- **File:** `geo_world.rs:1266` (`max_in_flight: 128`, the sole definition); gate sites `:1716, :1835, :1991, :2056, :2212`; worker count arrives independently at `ffi.rs:539` (`info.workers.clamp(1,64)`) → `HttpSource::new(workers,…)` (`geo_world.rs:1313`); C++ default 16, clamp 1..32 (`LogiBakedGeoTerrainActor.h:258-259`).
- **Problem:** `HttpSource::in_flight()` counts **queued + downloading + done-but-undrained** (incremented on send `lib.rs:868`, removed on drain `:888`). So at 16 workers the `128` cap permits ~112 *queued* requests — it sizes the FIFO backlog (the priority-inversion engine of H1), not concurrency. `from_tileset_with_decoder` doesn't even receive `workers`, so there's nowhere to couple it today. At the 64-worker FFI ceiling it inverts to a too-shallow 2× runway. The user explicitly asked for this coupling.
- **Fix:** `max_in_flight = (workers * IN_FLIGHT_PER_WORKER)` with a named const (K≈3–8; memory is a non-issue per user intent, so bias K toward keeping every worker's pipeline full). Thread `workers` into `from_tileset`/`from_tileset_with_decoder` (only callers are `from_ion`, which already has it, plus tests; updates `abi_layout.rs` offset asserts if a new FFI field is added). Once H1's heap lands, depth no longer harms scheduling, so K can rise purely for pipelining/keep-alive throughput and be measured.
- **Big-O:** n/a (constant → `workers·K`, O(1) to compute).
- **Confidence:** High.

**H3 — Worker teardown can stall up to 60 s: an in-flight fetch ignores the Drop disconnect**
- **File:** `logi_3dtiles_io/src/lib.rs:841-854` (Drop), `:811` (loop re-checks disconnect only at `recv()`), `:388-393` (read loop probes only the *per-request* flag), `:278` (`HTTP_OVERALL_TIMEOUT=60s`).
- **Problem:** Drop correctly drops the sender then joins (no detached threads), but a worker *inside* `http_get_cancellable` observes the channel disconnect only after the fetch returns; its per-request `AtomicBool` is **not flipped by Drop**. So a worker mid-download of a large/slow tile blocks `join()` up to 60 s, pinning N `Arc<DiskCache>` clones — a multi-second hang on the frequent editor hot-reload / world re-creation dev loop.
- **Fix:** Add a shared `shutdown: Arc<AtomicBool>` to `HttpSource`, OR it with the per-request flag in the read loop (`cancel || shutdown`), and set it **before** dropping the sender in Drop. Teardown then returns in ~one chunk's read time. *(Lighter alternative the proposer missed: in Drop, lock the existing `cancels` registry and flip every per-request flag — reuses the exact abort path, no new field; the dedicated flag is cleaner and gives a reusable global-abort primitive.)*
- **Big-O:** `join` O(workers), each blocking up to 60 s → bounded by one chunk-read latency. Cost: one extra Relaxed load per 64 KiB chunk.
- **Confidence:** High.

**H4 — Backoff retry timer is `frame`-keyed; a 429 wave over a parked camera can permanently stall retries** *(the one genuine liveness bug)*
- **File:** `stream_thread.rs:314` (work gate), `:317`/`:378` (`frame++` only inside the work branch; idle park does not advance `frame`); deadlines at `geo_world.rs:1596/1613`; re-select gate `:1805, :2064`.
- **Problem:** `work = view.is_some() && (view_changed || textured_dirty || world.in_flight() > 0)`. A transient-failure wave that drains in-flight to 0 with a still, bit-identical camera (and no new texture confirmation) makes `work=false` → the loop parks and **`frame` freezes**. The backoff deadline `frame + delay` is now unreachable; the rate-limited tiles are never retried until an unrelated camera move re-arms `work`. Silent (transient ≠ give-up, so nothing is counted). The convergence test only passes because it drives a strictly-increasing `f` in a tight loop — production does not.
- **Fix:** Decouple the retry clock from the work gate. Minimal: track the *minimum* pending `next_retry` in a single `Option<u64>` (O(1) on insert/clear) and OR `has_pending_retry()` into `work`, parking only until the soonest deadline. Better (pairs with H5): move backoff to wall-clock `Instant` deadlines and compute park duration as `min(idle_park, soonest_deadline − now)`.
- **Big-O:** retry wake O(camera-moving), unbounded wall-time → **O(1)/step, wakes at the exact deadline**.
- **Confidence:** High. (Trigger is a narrow conjunction — any sub-cm camera move fires all due retries — so "indefinite" usually means "until next input"; but in genuinely-parked states (menu/AFK/cutscene/held top-down logistics view) the stall is real and invisible.)

**H5 — Backoff + Retry-After are tuned in `frame` ticks under a non-uniform clock → unpredictable wall-clock delays**
- **File:** `geo_world.rs:1609-1614` (capped-exp + jitter), `:1576-1578` (`Retry-After secs × APPROX_TICKS_PER_SEC`), `:235-256` (consts).
- **Problem:** The backoff *math* is correct (`shift=(n−1).min(6)`, `exp=24<<shift`, `delay=exp.max(retry_after).+jitter`, all saturating). The *unit* is the bug: a "tick" = one stream-loop work iteration = `SHORT_PARK(1ms) + advance cost`, and on Windows with default ~15.6 ms timer granularity the 1 ms park can sleep ~15 ms — so the rate is well below the assumed 1000/s. "24 ticks ≈ 24 ms" and the Retry-After conversion both drift; the politeness floor holds only by *over-waiting*, which hurts throughput recovery and makes the politeness/throughput trade-off (the explicit ask) guesswork.
- **Fix:** Move backoff + Retry-After to wall-clock `std::time::Instant` deadlines (the Geo lane is non-authoritative render data, ADR-0007 — determinism is not required here; tests inject a mockable clock). `Retry-After` → `now + Duration` with no rate guess. Keep the capped-exponential + jitter shape. Removes `APPROX_TICKS_PER_SEC`.
- **Big-O:** O(1)/failure (delay magnitude rate-dependent → exact wall-clock).
- **Confidence:** High. (Requires a clock seam + rewriting the frame-integer test assertions — a real but contained test-scaffold refactor.)

**H6 — No priority preemption when the in-flight cap is full** *(the actionable core of H1's preemption facet; kept separate because the verifier rated it independently High)*
- **File:** `geo_world.rs:1835-1837` (cap-break), `:1716-1718` (subtree cap-break), `:2099-2142`/`:2110` (cancel_stale filter).
- **Problem:** Admission is first-come-within-a-pass + a hard `break`. The only slot-freeing mechanism, `cancel_stale`, cancels a fetch only if its path **left** `sel.load` — never if it is merely lower-priority than a newcomer. So once the cap saturates with whatever was requested first (skewed toward earlier-selected = coarser/farther tiles), a higher-priority tile that becomes selected *this* frame finds `in_flight >= max_in_flight` and is not fetched until a slot frees naturally. Under a static/slow-panning camera with a saturated link, reticle/look-direction tiles can sit behind a full slate of stale lower-priority fetches.
- **Fix:** Maintain a **min-heap of in-flight scores** (`BinaryHeap<Reverse<(OrderedF64, RequestId)>>`) alongside `in_flight`; refresh scores each pass for still-selected paths. When the cap is full and the next ranked tile out-scores the min in-flight (by a margin, e.g. >1.5×), cancel the min and admit the newcomer instead of `break`. Exempt subtree fetches from preemption (they gate refinement — give them effectively-infinite heap score or a small reserved budget). Factor the existing cancel + slot/bookkeeping reclaim into a shared `cancel_inflight(id)` helper; use lazy-delete on pop to stay consistent across all four removal sites.
- **Big-O:** admission O(m log m) sort + O(m) issue unchanged; preemption O(log n) per displaced slot (vs newcomer latency = wait-for-natural-completion today).
- **Confidence:** High.

**H7 — ureq is blocking HTTP/1.1, no multiplexing: wire concurrency capped at W against a single h2-capable host** *(the transport ceiling; consolidates the two HTTP/2 findings)*
- **File:** `logi_3dtiles_io/Cargo.toml:14` (`ureq = "2"`); `lib.rs:358` (`.call()` blocking), `:386` (blocking read), `:283-302` (agent). Endpoint probe: `tile.googleapis.com` → `ALPN protocol: h2` + `Alt-Svc: h3=":443"`.
- **Problem:** ureq 2.x is HTTP/1.1-only; one request per worker thread, one connection per concurrent request. Peak concurrency to the one host = `W` (16, up to 32 in the shipped UI). The keep-alive pool already fixed re-handshaking (the prior ~8 MB/s throttle), so the *residual* ceiling is structural: HTTP/2 would multiplex ~100 streams over 1–2 connections, hiding per-request RTT for the many small GLB/`.subtree` fetches and collapsing N handshakes to ~2.
- **Fix:** Evaluate an h2-capable client confined to `HttpSource`, kernel stays sync/poll-driven. Candidates: **(a)** `ureq` 3.x (rustls path supports h2) — lowest-risk agent swap, measure first; **(b)** `reqwest::blocking` (wraps hyper+h2; owns its *own* internal current-thread runtime, so the gameplay owner thread stays runtime-free — but it *does* pull tokio+hyper into the cdylib graph, an ADR-0012 decision point); **(c)** `curl`/curl-multi (h2, poll-driven `curl_multi_poll`, no tokio — but libcurl + native TLS, against the pure-rustls grain); **(d)** async `reqwest` on a private non-tokio executor (needs an ADR amendment). **Reject** raw `hyper+h2` (bespoke, more code than reqwest) and **RFC 9218 / h2 PRIORITY frames** (deprecated/unimplemented — schedule in *our* heap, not at the wire). Land behind the existing `TileContentSource` trait so the synthetic/test lane is untouched; benchmark MB/s + p50 tile latency vs the current pool (§1B). Cancel maps to drop-stream/RST_STREAM (O(1) any phase — fixes H8 too).
- **Big-O:** concurrent requests = O(W), 1 conn/req → **O(streams × conns)** over ~1–2 connections; time-to-fill ~O(tiles/W × RTT) → ~O(tiles/streams × RTT).
- **Confidence:** High (the ceiling is real and caps the stated max-bandwidth goal). The MB/s win over a *warm* H1.1 pool is RTT-bound-specific — measure-gated, not guaranteed extra steady-state bandwidth; the connection-count + handshake + protocol-priority benefits are unconditional.

### MEDIUM

**M1 — Session-refresh cooldown is `frame`-keyed and the refresh is a blocking `http_get` on the stream thread**
- **File:** `geo_world.rs:1346-1383` (gate `:1356`, blocking `http_get` `:1361`), `:256` (`SESSION_REFRESH_COOLDOWN=2000` ticks).
- **Problem:** The 401-storm collapse logic is correct and not livelocking (one refresh fixes the whole tree, throttled even on failure). But **(a)** the cooldown is `frame`-keyed (H4's root cause): on a *failed* root re-fetch with a parked camera, `frame` freezes and the cooldown-gated retry is unreachable → planet stuck at coarse LOD; **(b)** the refresh does a **synchronous blocking `http_get(&base_url)` on the stream thread** (up to 60 s), during which no draining/selection/mailbox-service happens. Documented as an accepted trade-off, but the 60 s tail is under-sold.
- **Fix:** **(a)** make `session_refresh_pending` contribute to `work` + use a wall-clock cooldown (mirrors the `textured_dirty` term already in the gate); **(b)** move the root re-fetch onto the worker pool (enqueue like a subtree/tileset fetch, consume in `drain_raw`) so streaming continues and the 401'd tiles re-drive the moment the fresh session lands.
- **Big-O:** O(1) gate; blocking refresh O(1 RTT)≤60s → async, zero stall.
- **Confidence:** High.

**M2 — Cache `put` and geom serialize+write are synchronous on the fetch/decode hot paths**
- **File:** `lib.rs:437` (`c.put` → `:714` `fs::write`+rename, in-band on the fetch worker); `geo_world.rs:666` (`gc.put_raw(key, &serialize_geometry(&arc))` inside the decode `par_iter_mut` at `:641`).
- **Problem:** (1) Every successful download blocks its worker on a full-payload `fs::write`+rename before it can `recv()` the next request. (2) The M3 geom tier serializes the decoded geometry into a fresh full-size Vec **and** writes it synchronously on the rayon decode worker — which gates frame publication (the schedule `.chain()`s decode before publish). On cold first-visit terrain (exactly when throughput matters) every tile pays both, in-band. The code comment at `:656-662` already flags this as the right fix.
- **Fix:** Route both the byte-cache `put` and the geom serialize+write to a dedicated low-priority writer thread fed by an unbounded channel of `(key, Arc<[u8]>)` — the bytes are already owned/Arc'd, so the hand-off is a pointer move, not a copy. The serialize allocation also leaves the decode critical section.
- **Big-O:** fetch worker +1 synchronous `fs::write`/tile; decode worker +serialize+write/cold-tile → **0 in-band disk I/O** on fetch/decode.
- **Confidence:** High *design*; magnitude unmeasured (un-fsynced writes land in the OS page cache — a µs-scale memcpy usually dominated by the Draco decode, but can block ms under writeback throttling). Measure first per §1B; the design is the one the existing comment defers.

**M3 — Subtree fetches enqueue in traversal order, unprioritized, sharing the tile in-flight budget**
- **File:** `geo_world.rs:1712-1728` (subtree loop, runs *before* the `:1786` tile sort), `:1716` (same `max_in_flight` gate).
- **Problem:** `.subtree` files gate refinement (fetching them first is correct — the `MANIFEST_FETCH_BOOST=8.0` at `:2700` encodes exactly this for *grafted tilesets*), but the subtree loop iterates `needed_subtrees` in tree-walk order with **no distance/SSE/look-direction key** and bypasses `fetch_priority` entirely, consuming the *same* budget *before* the ranked tile loop. So far/off-view subtrees can exhaust the cap ahead of the near subtree that unblocks the reticle region's refinement.
- **Fix:** Record the requesting tile's distance/bounds-center alongside `(url, child_count)` (already available where `needed` is built) and sort `needed_subtrees` by the same priority blend (subtree priority ≈ the tiles it unlocks + a graft boost) before the enqueue loop — or, better, fold subtrees and tiles into **one** global priority heap (H1) so a freed worker always pulls the single most-valuable pending fetch, subtree or tile.
- **Big-O:** subtree enqueue O(s) traversal-order → O(s log s) ranked (or O(log q)/item in the unified heap), `s` small.
- **Confidence:** Medium. (Bites only under genuine subtree pressure — deep implicit regions + wide FOV where `needed_subtrees` + in-flight exceeds the cap; usually the count is under the cap and order is moot.)

### LOW

**L1 — Per-request `Arc<AtomicBool>` cancel flag + the `cancels` `Mutex<HashMap>` are owner-only overhead.** `lib.rs:769/863` mints a heap `Arc` per request; the `Mutex` registry (`:775, :864, :884, :902`) is locked on every request/cancel/poll but is **provably owner-thread-only** (workers receive the flag by value via the channel tuple; `GeoWorld` is single-owner per ADR-0012). **Fix:** fold the flag into the existing `in_flight` set (`HashMap<RequestId, CancelFlag>`), deleting the `Mutex`, the duplicate id map, and two misleading "concurrency" comments — `cancel` becomes one unsynchronized `get` + atomic store. *Title overstates ("N workers serialize"); zero contention exists. A generation-stamped flag slab (option a) is the only sound allocation-free variant — a single shared epoch (option b) is incompatible with the existing selective-cancel + prefetch-survival semantics, do not adopt.* Confidence High; measure-gated.

**L2 — `cancel_stale` rescans the entire `in_flight` map every step + rebuilds the `wanted` set.** `geo_world.rs:2099-2113` is O(in_flight) per step (the `is_empty` guard is *after* the full scan) plus O(|sel.load|) to build `wanted` (`:1770`, borrowed slices, no copies) + one `TilePath` (`Vec<u32>`) clone per stale entry. Fine at 128; grows with a coupled-deeper cap. **Fix:** fold cancellation into H1's single sorted pass (cancel = "not re-dispatched"), or drive it off the set-difference of prev/current `wanted` and store `Arc<[u32]>` paths for cheap clones. *Note: a pure wanted-diff is incomplete — staleness is also gated by `prefetch_paths` membership; the fold-into-selection path is the clean one.* Confidence High; "only worth doing as part of the larger rework."

**L3 — `cancel` aborts the read loop but not a worker blocked in connect/TLS/headers.** `lib.rs:355` (pre-flight) and `:391` (per-chunk) bracket the body read, but `.call()` (`:358`, connect+TLS+request+headers) is opaque — a `cancel()` during the handshake is unobserved until `.call()` returns or `HTTP_CONNECT_TIMEOUT`(10s)/`HTTP_OVERALL_TIMEOUT`(60s) fires. **Fix (in-bounds, short-term):** shorten `HTTP_CONNECT_TIMEOUT` to ~3–4 s + add a first-byte timeout so a stuck pre-body request frees its worker sooner. The proper fix is H7's h2 (drop-stream cancel, O(1) any phase). *Impact is degraded throughput (W−k draining), not dispatch starvation: `cancel()` removes the id from `in_flight()` immediately, so a cancelled-but-running worker does not hold a dispatch slot, and the unbounded channel never blocks new high-priority enqueues. The "oversize workers vs cap" sub-suggestion is moot — the gate is on the in-flight count, not free threads.* Confidence High.

**L4 — GLB byte buffer pays one redundant payload copy at the Arc hop.** `geo_world.rs:1490` `Arc::from(bytes.into_boxed_slice())` always copies the payload into the Arc allocation (plus a shrink-realloc when `cap≠len`). **Fix:** build `Arc<[u8]>` once in the worker / return it from the fetch (the bytes are transient, single-owner, dropped right after decode — the long-lived shared handle is the separate `ResidentMesh(Arc<TileGeometry>)`). *The proposer's "Content-Length is the compressed size" mechanism is wrong — ureq strips Content-Length when it decompresses, so the hint is 0 on the gzip path; and P3DT GLBs are Draco+JPEG (already-compressed), not gzipped, so the "2–5× churn" headline is unsubstantiated. Use `Arc<[u8]>`, not `Arc<Vec<u8>>` (the latter is a fatter double-indirection and mismatches the `FetchedBytes(Option<Arc<[u8]>>)` store).* Confidence: the redundant copy is real; magnitude overstated. Low.

**L5 — Per-request fresh 64 KiB stack read-chunk + `extend_from_slice` copy.** `lib.rs:387/400`. The body Vec is **already zero-copy** socket→Arc (the finding correctly says *not* to pool it). Only the per-call scratch chunk + per-chunk memcpy are avoidable — read straight into `Vec::spare_capacity_mut` + `set_len`. *Stack array, not a heap alloc; the eliminated work (a 64 KiB memset often elided + a copy of cache-hot bytes) is dwarfed by socket/TLS/decode.* Optional polish; flamegraph-gated. Confidence Medium. Low.

**L6 — `cache_key` (FNV-1a over the URL path) is hashed twice per cache-miss fetch.** `lib.rs:432` (`get`→`cache_key`) then `:437` (`put`→`cache_key`) hash the same ~150-byte URL; LLVM can't CSE across the intervening download. **Fix:** compute the key once in `fetch_one`, call the already-`pub` `get_raw(key)`/`put_raw(key)`. O(path_len) ×2 → ×1. Trivial, byte-identical, no new API. Confidence High; saving (~tens of ns) is unmeasurable but the user asked. Low.

**L7 — Failure-set bookkeeping is correctly bounded and mostly O(1)** *(informational — do NOT "fix")*. `decode_failed`/`fetch_attempts`/`next_retry` are hashbrown (O(1)), wholesale-cleared past `MAX_DEAD_PATHS=16384` (`:1391`); `subtree_cache` FIFO-evicts via VecDeque (O(1) amortized). The only cost is L2's `cancel_stale` scan. Correct as-is. Confidence High.

---

## 3. Throughput knobs table

Every knob in the census. **Lead row is the user's explicit ask.** "Coupling" = what it must move in lockstep with.

| Knob | File | Current | Governs | Assessment | Recommended (coupling) |
|---|---|---|---|---|---|
| **`max_in_flight`** | `geo_world.rs:1266` | **128 (hardcoded literal)** | Total outstanding requests across all 3 lanes (GLB/subtree/extra-contents); the pipeline-depth throttle | **Decoupled from workers (H2).** At 16 workers ~112 slots are FIFO backlog, not concurrency | **`= workers·K` (K≈3–8)**; thread `workers` into `from_tileset*`. Bias K high (memory waived). **Couple → workers**; once H1's heap lands, depth is harmless so K rises for pipelining only |
| **`workers`** | `ffi.rs:539` (clamp 1..64) · `actor.h:259` (default 16, clamp 1..32) · `actor.cpp:293` | C++ 16/clamp 32; Rust clamp 64; 1:1 OS threads, 1 blocking conn each | Concurrent HTTP requests = simultaneously-downloading tiles | **Bandwidth lever.** C++32 vs Rust64 clamp mismatch (UI can't reach the Rust ceiling) | **H1.1:** raise C++→64, default 32–48 (one host tolerates it; idle pool 64/256 already sized). **H2 (h2):** becomes a *decode/CPU drainer* count (4–8 suffice). **Unify the two clamps.** `max_in_flight` derives FROM this |
| `max_idle_connections` / `_per_host` | `lib.rs:298-299` | 256 / 64 | ureq keep-alive pool size | **Correct & the coupling is right** (per-host ≥ workers ⇒ warm reuse, the prior #1 fix). | Keep ≥ workers; if you raise the worker clamp, raise per-host to match (derive both from `workers`). Moot under h2 (1 conn) |
| `HTTP_CONNECT_TIMEOUT` | `lib.rs:276` | 10 s | TCP+TLS connect wall before a worker frees | Slightly generous for one well-connected host | Drop to ~3–5 s so a flaky connect frees the worker sooner (L3); transient retry re-tries anyway |
| `HTTP_IO_TIMEOUT` | `lib.rs:277` | 30 s (per read/write) | Per-chunk stall ceiling | **Keep.** 30 s/chunk is safe; a slow-but-healthy stream survives | Keep |
| `HTTP_OVERALL_TIMEOUT` | `lib.rs:278` | 60 s | Whole-fetch ceiling; also bounds Drop teardown (H3) | **Keep for the fetch**; it's the H3 stall bound | Keep (and add the H3 shutdown flag so teardown ignores it) |
| `READ_CHUNK_BYTES` | `lib.rs:338` | 64 KiB | Cancel-probe granularity (1 atomic load/chunk) | **Well-chosen** (2–16 probes/tile; load is noise vs socket read) | Keep |
| `MAX_TILE_BYTES` | `lib.rs:271` | 64 MiB | OOM/DoS body cap | **Safety wall, not a throughput knob** | Keep |
| `MAX_FETCH_ATTEMPTS` | `geo_world.rs:212` | 4 | Transient retries before terminal | **Reasonable** with Retry-After + backoff | Keep |
| `RETRY_BACKOFF_BASE_TICKS` | `geo_world.rs:241` | 24 ticks, ×2/attempt, +jitter | Anti-retry-storm pacing | **Correct shape; wrong unit** (H5 — `frame` ticks ≠ ms) | Keep shape; **move to wall-clock `Instant`** (H5) |
| `SESSION_REFRESH_COOLDOWN` | `geo_world.rs:256` | 2000 ticks | Min steps between P3DT session re-fetches | **Collapse logic correct; `frame`-keyed (M1) + blocking refresh** | Wall-clock cooldown + async refresh (M1) |
| `max_tile_distance` | `actor.h:255` → `geo_world.rs:1267` | 6000 m | Far-plane cull = upstream fetch-demand bound | Design/visual choice, not a perf bug | **Couple → pool (24576) + resident cap (18432)**: raising it without scaling those drops tiles into coarse holes |
| `max_sse` (Detail Target) | `ffi.rs:763` → `SelectParams` | tuned 8→6 px | SSE refine threshold = **dominant** fetch-demand multiplier | Not a bug; the appetite the throughput fixes exist to satisfy | **Move in lockstep with workers/`max_in_flight`/pool/resident-cap** |
| `CULLED_SSE_FACTOR` | `geo_world.rs:567` | 2.0 (was 4.0) | Off-frustum coarse-shell refine = off-view fetch load | Keep 2.0 (seam quality); a lever to relax toward 3–4 if bandwidth tight under fast turns | Coupled to `FRUSTUM_MARGIN_RAD` |
| `DECODE_BATCH` | `geo_world.rs:556` | 1 | par_iter granularity for the heavy decode | **Correct for high-variance work** | Keep. Watch: if fetch throughput rises (H1/H7), decode (`cores−2`) is the next bottleneck — M3 geom tier is the relief valve |
| `SSE_BATCH` | `geo_world.rs:553` | 1024 | Batch for the cheap fused SSE/LRU pass | **Correctly large for uniform cheap work** | Keep |
| `PREFETCH_*` (lookahead / min-motion / max-per-step) | `geo_world.rs:431-437` | 8.0× / 0.5 m / **8/step** | M4 speculative prefetch from spare budget | Reasonable; 8/step is **conservative** given "aggressive prefetch is fine" | Raise to 16–32; **couple the cap to spare budget** (`max_in_flight − reactive demand`), not a flat 8 — a priority heap naturally de-prioritizes speculative tiles |
| `MAX_GEOM_RESTORES_PER_STEP` | `geo_world.rs:426` | 8 | Synchronous M3 disk-read back-pressure | Keep ~8 as a stall guard | 16 safe on NVMe; minor |
| entity pool / `MaxResident` | `ffi.rs:542` (24576) · `:546` (18432) | pool 24576 / resident 18432 | Hard ceiling on tracked tiles; exhaustion silently drops a fetch → coarse hole | Deliberately tuned to the 6 px working set | **Verify pool > resident + max_in_flight + churn** (margin shrinks with a deeper coupled cap). Add a periodic sweep returning long-dead decode-failed slots (`ffi.rs:531` TODO leak) |

---

## 4. Intelligent-scheduling redesign

The concrete design that makes **"highest-priority-to-render fetched + rendered first, with preemption"** true. It closes the FIFO gap (H1), the no-re-ranking gap (H1b), the no-preemption gap (H6), and the unprioritized-subtree gap (M3) in one coherent owner-side dispatcher. Everything is inside `HttpSource` + `GeoStreaming`; the kernel stays sync/poll-based; no Unreal types cross.

### The render-priority signal (keep — it is the right one)
`fetch_priority` (`geo_world.rs:2709-2718`) already blends the three correct terms:
```
importance = geometric_error / distance.max(1)          // closer + coarse-fallback first
align      = dot((bounds_center − cam_pos).norm, cam_dir) // ahead 1.1× … behind 0.1×, never 0
graft_boost= MANIFEST_FETCH_BOOST(8.0) if tileset-graft  // manifests/subtrees lead same-distance geometry
score      = importance · (0.6 + 0.5·align) · graft_boost
```
One `sqrt` + one `div` + one `dot` per tile, computed once via decorate-sort. **Do not change the formula** — the defect is that the *queue does not obey it across passes* and that it is *not recomputed for already-submitted work*.

### Data structures
- **Owner-side pending heap** (the surplus the cap can't yet dispatch):
  `BinaryHeap<PrioReq>` *or*, to support re-keying cheaply, a `keyed_priority_queue::KeyedPriorityQueue<TilePath, OrderedF64>` so a re-selected tile *updates* its key instead of duplicating. `PrioReq = { score: OrderedF64, seq: u64, id: RequestId, url: Arc<str>, cancel: CancelFlag }`. **Subtrees and tiles share this one heap** (M3) — a subtree's score = the tiles it unlocks + graft_boost.
- **In-flight min-heap** for preemption (H6): `BinaryHeap<Reverse<(OrderedF64, RequestId)>>` maintained alongside `in_flight`; peek-min O(1), pop O(log n). Subtree fetches inserted with `+∞` score (never preempted). **Lazy-delete on pop** to stay consistent with the four removal sites (drain / cancel_stale / prune / fail).
- **Dispatch cap** `dispatch_cap = (workers·K)` (H2), the number actually handed to workers; the heap holds the rest.

### The per-pass algorithm (replaces the FIFO + the `:1835` break)
Each `advance` step, on the owner thread:
1. **Drain** completions (unchanged), removing from both `in_flight` and the in-flight min-heap.
2. **Cancel-implicit:** anything no longer in `sel.load ∪ prefetch_paths` is dropped from the pending heap (O(log n) keyed removal) and its in-flight fetch cancelled (existing mid-download abort). *(Replaces today's full `cancel_stale` scan, L2.)*
3. **Re-rank (H1b):** recompute `fetch_priority` for all still-wanted pending + in-flight paths against the **current** `cam_pos`/`cam_dir`; update heap keys. This is the O(n log n) sort already paid at `:1790`, now feeding a *live* structure instead of a discarded order.
4. **Dispatch:** while `in_flight() < dispatch_cap`, pop-max from the pending heap and hand to a worker.
5. **Preempt (H6):** if the pending heap's max out-scores the in-flight min by a margin (>1.5×) and the cap is full, cancel the min in-flight and dispatch the newcomer.

Workers stay dumb: pop the next dispatched item (or, in shape A, pop-max directly from a shared `Mutex<BinaryHeap>` under a `Condvar`). The worker that next frees always pulls the globally-highest-priority pending tile **including ones submitted after lower-priority ones** — the goal.

### Preemption + cancellation cost
Cancellation is already cheap (per-chunk `AtomicBool` abort + immediate `in_flight` removal). With the heap, cancel-while-queued is an O(log n) heap removal, not a dequeue tombstone. Per the user, cancelling hundreds/pass is fine — so step 2's drain-and-rebuild is acceptable even at a deep cap. Under h2 (H7) preemption becomes drop-stream/RST_STREAM, freeing the slot instantly at any phase (also fixes L3).

### O-notation and why each lane wins
| Operation | Today | Redesigned | n |
|---|---|---|---|
| Service order | FIFO (priority lost across passes) | **pop-max O(log n)** | queued ≈ dispatch_cap |
| Re-rank submitted work | none (frozen at submit) | **O(n log n)/pass** (already paid) | wanted pending+in-flight |
| Admit higher-priority newcomer when full | wait-for-completion | **preempt O(log n)** | in-flight |
| Cancel queued | tombstone dequeue (worker wakeup wasted) | **heap remove O(log n)** | queued |
| Subtree ordering | O(s) traversal-order | **unified heap O(log q)/item** | pending |

The single O(n log n) sort the system already runs every step now **governs wire order end-to-end** instead of being discarded the moment the queue has depth.

---

## 5. Client / HTTP-version recommendation

**Endpoint fact (probe-confirmed):** `tile.googleapis.com` negotiates **ALPN h2** and advertises **h3** (`Alt-Svc`). Current pin: **ureq 2.12.1 over rustls 0.23.40 + ring — HTTP/1.1 only** (`Cargo.lock:1791`).

**The HTTP/2 verdict for single-host P3DT:** **adopt.** One host = one h2 authority = maximal multiplex benefit. HTTP/2 turns the ceiling from *connection count* (≤W) into *streams × connections* (~100 streams over 2–4 connections), eliminating the per-connection head-of-line blocking and collapsing N TLS handshakes to ~2. This is the **highest-ceiling** throughput change and the ideal shape for "many small tiles, one host." **Do not rely on h2 PRIORITY frames** (deprecated by RFC 9113, server-unimplemented) or RFC 9218 `Priority:` headers (unverified Google support, not surfaced by the Rust clients) — **schedule in our own heap (§4)**; the wire just needs to carry many concurrent prioritizable streams. Open **2–4 connections** (not 1) so TCP-level HoL (a lost packet stalls all streams on a connection) doesn't freeze the whole pool. **h3/QUIC** removes even TCP HoL but is experimental in the Rust ecosystem (unstable `reqwest` flag) and may be UDP-blocked — **defer behind a feature flag.**

**Mapped to the no-tokio / blocking-pool constraint** (ADR-0012: the gameplay owner thread runs no runtime; the Geo lane is non-authoritative render data on Rust-owned threads, ADR-0007):

| Option | h2? | no-tokio fit | Effort | Expected gain | Verdict |
|---|---|---|---|---|---|
| **ureq 3.x** (rustls path → h2) | ✅ | ✅ trivial (blocking, no runtime) | **Low** — agent swap; some 2.x→3.x API churn | Multiplexing over the existing blocking-pool shape, fewer connections | **Spike first** (lowest-risk quick win — measure before committing) |
| **`reqwest::blocking`** (hyper+h2) | ✅ | ✅ on *our* threads (owns a hidden internal current-thread runtime; owner/gameplay stay runtime-free) — but pulls tokio+hyper into the 2 cdylibs (ADR-0012 decision point) | **Med** | Full h2, mature, connection reuse, gzip; one `Client` Send+Sync shares the pool across workers | **Adopt (primary)** if ureq 3.x proves insufficient — confine to `HttpSource`, never cross the C ABI |
| **`isahc`** (libcurl) | ✅ | ✅ sync path, no runtime | **Med** | h2 without tokio in the graph | **Prototype fallback** — but native TLS (Schannel/OpenSSL) + a C dep, against the pure-rustls/ring grain |
| **`curl`/curl-multi** | ✅ | ✅ poll-driven `curl_multi_poll` | **Med** | h2, one multi-handle, poll model fits | **Consider** — same C/native-TLS caveat as isahc |
| **async `reqwest` + private executor** | ✅ | ❌ as-written (needs an ADR amendment confining one runtime to the Geo lane) | **High** | Cleanest model: thousands of cheap futures, drop=cancel, Semaphore=limit | **Bold option** — prototype behind a flag; best long-term shape |
| **raw `hyper`+`h2`** | ✅ | ❌ async | **High** | Max control, lowest overhead | **Reject** — bespoke connection/ALPN/retry code reqwest already provides+tests |
| **`attohttpc` / sideways H1.1** | ❌ | ✅ | Low | none for the goal | **Reject** — same ceiling as ureq, less mature |
| **Keep ureq 2.12.1** | ❌ | ✅ | none | zero | **Reject** — leaves the multiplexed throughput on the table |

**Ordered recommendation:** **(1)** spike **ureq 3.x** behind the `TileContentSource` trait, benchmark MB/s + p50 tile-to-screen latency vs the current pool (§1B) — if it delivers h2 with the blocking-pool intact, land it (lowest risk). **(2)** else adopt **`reqwest::blocking`** (accept the tokio-in-cdylib ADR decision, confined to `HttpSource`). **(3)** keep the §4 priority heap regardless — it orders *which* streams to open when streams are capped, and it is what enforces "highest-priority first" at the application layer since the wire won't. **Preserve the single shared client** (one `OnceLock` `Client` for tile + ion + subtree/root fetches) so manifests multiplex onto the same warm connections; `pool_max_idle_per_host` ≥ the connection count.

---

## 6. Memory-flow — per-tile byte-buffer lifecycle

Tracing one GLB tile's bytes socket → renderable, with move/copy at each hop:

| Hop | File | Operation | Move or copy |
|---|---|---|---|
| 1. Pre-size body | `lib.rs:375-380` | `Vec::with_capacity(hint)`, `hint = Content-Length.min(MAX_TILE_BYTES)` | 1 alloc. **Note:** ureq *strips* Content-Length when it decompresses, so on a gzip response `hint=0` → geometric regrowth. P3DT GLBs are Draco+JPEG (not gzipped), so the common case has an exact hint and no regrowth. |
| 2. Read loop | `lib.rs:387-400` | fresh `[0u8; 64 KiB]` stack chunk → `extend_from_slice` into the Vec | **1 copy/chunk** (stack→Vec) + stack scratch per call (L5) |
| 3. Worker → owner | `lib.rs:817` → `poll_completed` → `drain_raw` | `Ok(bytes)` through flume, `Loaded` struct | **Move** (good — zero-copy) |
| 4. Owner → entity | `geo_world.rs:1490` | `Arc::from(bytes.into_boxed_slice())` → `FetchedBytes(Some(Arc<[u8]>))` | **1 copy** of the whole payload into the Arc alloc (+shrink-realloc if `cap≠len`) — **L4, the one guaranteed redundant copy** |
| 5. Decode | `geo_world.rs:641-668` | `decode(bytes.0.as_deref())` → `ResidentMesh(Arc<TileGeometry>)`; bytes dropped right after; M3 `serialize_geometry(&arc)` → fresh full-size Vec → `fs::write` | Decode reads borrowed; **M3 = +1 full-size serialize alloc + synchronous write on the decode worker (M2)** |

**The body Vec is already zero-copy from socket to Arc at hops 1–3** — do **not** pool it. Concrete allocator-traffic reductions on the hot path:

1. **Kill hop 4's redundant copy (L4):** build `Arc<[u8]>` once in the worker (or return it from `http_get_cancellable`) so the entity store receives a move, not a copy. The bytes are transient/single-owner; the long-lived shared handle is the separate `ResidentMesh(Arc<TileGeometry>)`. Use `Arc<[u8]>` (matches `FetchedBytes`), **not** `Arc<Vec<u8>>`.
2. **Eliminate hop 2's scratch + per-chunk copy (L5):** read straight into `Vec::spare_capacity_mut` + `set_len`, growing geometrically when the hint is absent. Removes the per-call 64 KiB stack array and the per-chunk memcpy. *Adds an `unsafe set_len` + manual growth — flamegraph-gate it; on the no-gzip GLB path the win is noise.*
3. **Move M3's serialize+write off the decode worker (M2):** the `Arc<TileGeometry>` crosses to a writer thread as a pointer; the serialize allocation leaves the decode critical section.
4. **Optional per-worker scratch-Vec pool:** `mem::take` a recycled Vec into the Arc and replace with a fresh one — reuses capacity across tiles, cutting even the one remaining body alloc. The user explicitly invited buffer reuse here; measure-gate it.

The cancel flag and `cancels` map are the other per-request allocations (L1) — fold the flag into `in_flight`, removing the `Mutex<HashMap>` and a heap `Arc`-new per request (owner-only, no contention to begin with).

---

## 7. O-notation table (census algorithms)

| Algorithm | File | Op | Time | Space | n | Hot? | Reduction |
|---|---|---|---|---|---|---|---|
| **Worker service order** | `lib.rs:811` + `geo_world.rs:1786` | dequeue | pop O(1) but **effective priority = O(queue_depth) staleness** | O(queue_depth) unbounded channel | queued requests (can exceed cap across passes) | ✅ | **§4 shared max-heap: pop-max O(log n)**; carry `priority` through `request` |
| select_request decorate-sort | `geo_world.rs:1786-1790` | sort | O(L log L) | O(L) | L = selected-unloaded/step (hundreds–low thousands) | ✅ | Output discarded by FIFO; with §4 heap, **delete the sort** — push L into the heap O(L log Q), global order maintained |
| fetch_priority score | `geo_world.rs:2709-2718` | per-tile | O(1) (1 sqrt+div+dot) | O(1) | L/step | ✅ | Correct signal; computed once via decorate-sort. **Make the queue obey it** (don't optimize the score) |
| cancel_stale scan | `geo_world.rs:2099-2113` | filter | O(in_flight + L) (guard *after* scan) | O(stale + L) | in_flight ≤ cap; L = load set | ✅ | Fold into §4's single pass; index in-flight by path (`HashMap<TilePath,RequestId>`) → **O(Δ)** so a deep cap stays cheap |
| reactive enqueue gauntlet | `geo_world.rs:1795-1868` | per-tile probes | O(L) hashbrown probes (O(1)/tile); only non-O(1) is `resolve_url` | O(1)/tile | L/step | ✅ | `resolve_url` re-parses the base URL per tile/step — **memoize the parsed base per step** / cache resolved URLs on the path (§1E) |
| drain_raw completion | `geo_world.rs:1406-1498` | drain | O(D) + parse for grafts | O(D) | D = completed/step | ✅ | Already O(D), O(1)/result. Future: `cancels.lock()` per result is a serialization point at high rate — shard or fold into `in_flight` (L1) |
| HTTP transport model | `lib.rs:283-302, 358` | fetch | throughput = N_workers × (1/RTT-bound); per-conn HoL | O(N_workers) sockets | N = workers/conns to one host | ✅ | **H7: h2 → O(streams×conns) over ~2 connections**; eliminates per-conn HoL |
| shared http_agent | `lib.rs:283` + ion + session-refresh | obtain | O(1); pool internally synced | shared pool | all HTTP callers | ❌ | Sharing is correct (warm reuse). Ensure the session-refresh multiplexes onto the same h2 connection (M1) |
| Backoff timer eval | `geo_world.rs:1609-1614` / gate `:1805` | per-failure | O(1) | O(failing paths), bounded `MAX_DEAD_PATHS` | failing paths | ✅ | Math correct; **wall-clock `Instant`** (H5) + track min-deadline O(1) for the wake (H4) |
| Failure-set bounding | `geo_world.rs:1391` / `:944` | len-check + clear | O(1) amortized (wholesale clear; VecDeque FIFO) | bounded (16384/8192) | dead paths | ✅ | Correct & bounded (L7) — do not change |

---

**Relevant files (all absolute):**
- `C:\Users\premi\work\LogiUE\Rust\crates\geo\logi_3dtiles_io\src\lib.rs` — `HttpSource` (flume FIFO `:800`/`:811`, Drop `:841`, request/cancel/poll `:857-911`), transport (`:283-302`, `:353-405`), cache_key/get/put (`:526-536`, `:644-717`), timeouts (`:276-278`), `READ_CHUNK_BYTES` `:338`, `MAX_TILE_BYTES` `:271`.
- `C:\Users\premi\work\LogiUE\Rust\crates\geo\logi_3dtiles_scene\src\geo_world.rs` — `max_in_flight: 128` `:1266`, select_request sort+dispatch `:1786-1871`, subtree loop `:1712-1728`, cap gates `:1716/1835`, `fetch_priority`+`MANIFEST_FETCH_BOOST` `:2700-2718`, cancel_stale `:2099-2142`, fail_fetch/backoff `:1580-1624`, session refresh `:1346-1383`, GLB Arc hop `:1490`, failure-set bounding `:1391`/`:944`, M3 geom write `:666`.
- `C:\Users\premi\work\LogiUE\Rust\crates\geo\logi_3dtiles_scene\src\stream_thread.rs` — work gate (frozen clock) `:314`, `frame++` `:317`, idle park `:378`.
- `C:\Users\premi\work\LogiUE\Rust\crates\geo\logi_geo_ffi\src\ffi.rs` — `workers.clamp(1,64)` `:539`, pool/resident `:542/:546`, decode-failed leak TODO `:531`.
- `C:\Users\premi\work\LogiUE\Plugins\LogiBevy\Source\LogiBevyRuntime\Private\LogiBakedGeoTerrainActor.h:258-259` / `.cpp:293` — `GlobeFetchWorkers` default 16, clamp 1..32.
- `C:\Users\premi\work\LogiUE\Rust\Cargo.lock:1791` — ureq 2.12.1 (HTTP/1.1).