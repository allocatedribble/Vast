# Runtime / Scheduler / Reactor Re-Evaluation — 2026-06-09

**Scope.** A microscopic re-evaluation of the entire scheduler/reactor/runtime effort: ADR-0012 (poll FFI /
decoupled sim thread), ADR-0014 (LogiRuntime two-lane executor+reactor), ADR-0015/0016 (the scaling +
determinism companions), ADR-0018 (Geo working-set law), the LogiRuntime-Design plan, and every landed
runtime artifact (the Geo stream thread, the poll FFI, the HTTP/h3 fetch lanes, the pool budgets, the
counter-RNG, the spike modules). Question asked: **are we on the right path, and what mistakes need
rectifying?**

**Method.** 13 agents mapped the ground truth (34 normative design claims; a fully line-anchored code map;
10 externally-checkable claims verified against primary sources — UE 5.8 source, bevy/compio/cyper/num_cpus
crate sources, the Leis 2014 paper, the PostHog/Iggy post-mortems), then 10 reviewer dimensions (FFI
contract, stream thread, ADR-0018 conformance, HTTP scheduler, h3 reactor, sim determinism, machine-wide
thread budget, docs↔code congruence, and two opposed strategy judges) produced 26 findings + 10 trajectory
verdicts. The adversarial-verification phase was lost to API capacity limits, so **every finding acted on
below was re-verified by hand against the working tree before any fix** (several were found already fixed
by a concurrent session and dropped). All fixes are Rust-only — no ABI bump, no C++ rebuild required.

**Verification.** `cargo test --workspace`: **57 suites, 245 passed, 0 failed** (including the new
cross-process determinism gate). `cargo clippy --workspace --all-targets`: clean. `--no-default-features`
(lean ureq-only) clippy: clean.

---

## 1. Verdict — right path, with corrections applied

**The trajectory is sound and is kept.** All ten dimensions returned *on-track* or
*on-track-with-corrections*; none returned *off-track*. Specifically:

- The **two-lane target** (work-stealing CPU + completion-I/O reactor under one core budget, joined at the
  frame poll) survives adversarial review. Its two load-bearing rejections were re-verified against primary
  sources: the PostHog rayon+tokio one-pool freeze is real and supports the lane split; Apache Iggy's
  retreat is real and supports rejecting ECS-under-thread-per-core (precision: Iggy kept TPC itself).
- The **realized pieces are the right ones, well built**: the Geo poll lane (stream thread + latest-wins
  mailbox + `Arc` retain + v12 frame-arena clip contract) passed the deepest concurrency scrutiny — the
  reviewers' torn-read / use-after-free / lost-wakeup / ring-protocol attacks all came back negative. The
  spike discipline (Exp A–D) was called "unusually honest": each licenses only its narrow thesis, and the
  project's own measurements demoted its own designs twice (executor → low priority; h2/h3 → structural
  adoption at parity).
- The **mistakes were real but peripheral**: convergence holes at the edges of the working-set law, one
  unbudgeted thread source, lifecycle bugs in the newest features (M4 prefetch, h3 fallback), a vacuous
  safety gate, and a cluster of normative-doc over-claims that misrepresented what is built. All are now
  fixed (§2) or explicitly gated (§3).
- The **load-bearing strategic fact**: the deterministic gameplay sim — the one workload the North Star
  scales — still runs synchronously on the UE game thread (`logi_world_tick`, ABI v6). The deferral was
  honest (plan §7.2) but the Accepted ADRs read as if it shipped; that is now corrected, and the decouple is
  pinned as the next milestone with a trigger (§4).

## 2. Mistakes rectified this session (all re-verified, then fixed; tests green)

**Correctness / robustness (Rust):**
1. **Stream-thread panic = silent permanent freeze** → `run_stream`'s step body is now
   `catch_unwind`-guarded; a poisoned step increments a `step_panics` counter (rate-limited stderr log) and
   the loop continues (legal: Geo is non-authoritative). Explicit note added: the future gameplay runner
   must hard-stop instead. (`stream_thread.rs`)
2. **compio blocking pool unbudgeted (default 256 threads)** → capped at `CYPER_BLOCKING_POOL_LIMIT = 4`
   via `Runtime::builder().with_proactor(...)`. This was the single largest unbudgeted thread source under
   a streaming burst. (`cyper_source.rs`)
3. **No h3→h2 fallback despite five docstrings claiming one** → an h3 transport failure on a cache-routed
   GET/HEAD now evicts + **quarantines** the host's Alt-Svc entry (5 min negative cache, so Google's
   every-response `Alt-Svc: h3` can't re-arm a broken path per-request) and retries once over h1/h2. A
   mid-session UDP block now degrades instead of stranding streaming for the 24 h Alt-Svc TTL.
   (vendored `cyper/src/{client,altsvc}.rs`)
4. **M4 predictive prefetch was functionally inert** (protection wholesale-cleared ~2 advances after issue,
   far under any RTT → permanent issue/cancel churn) → `prefetch_paths` now ages by
   `retain(in_flight_paths.contains)`; a prefetch is protected for its whole flight; H6 preemption remains
   the stale-speculation relief valve. (`geo_world.rs`)
5. **Geo convergence holes**: (a) a Glb-requested fetch that sniffs as a tileset leaked its reserved slot +
   index entry forever → reclaimed (mirrors `fail_fetch`); (b) a fetched-but-ungraftable manifest (and the
   reverse Glb-on-manifest mismatch) re-fetched in a 1 ms-cadence loop forever → marked terminal in
   `decode_failed` (session refresh clears it); (c) expired `next_retry` deadlines were never purged, so one
   abandoned transient failure kept the "settled" stream thread re-traversing at ~125 Hz for the session →
   purged each step in `bound_failure_sets`; (d) the 401 path hammered a stale session at 24 ms cadence →
   backoff floor raised to `SESSION_REFRESH_COOLDOWN`. (`geo_world.rs`)
6. **ureq cancel tombstones** (cancelled-but-buried heap entries — e.g. score −1e9 prefetches — never
   reclaimed; heap + cancel registry grew unbounded under sustained motion, falsifying the registry's
   documented bound) → amortized heap rebuild in `cancel()` purging cancelled entries + their registry
   slots (safe: an un-popped request has no worker reader, and the scene settles its own bookkeeping at
   cancel time). Registry-bound docs corrected. (`logi_3dtiles_io/lib.rs`)
7. **A panicking cyper fetch task leaked a stream-budget slot + an owner in-flight entry** (both sends sit
   after the `.await`; async-task swallows the unwind into the never-awaited handle) → an unwind-only
   drop-guard (`PanicCompletion`, fires on `thread::panicking()`) restores the exactly-one-result contract;
   cancel-drops stay silent. (`cyper_source.rs`)
8. **The parallel==serial determinism gate could pass vacuously** (process-global pool `OnceLock`: first
   test wins, both arms share one pool — and after this session's Exp-C executor change the two in-process
   arms became literally identical) → new **cross-process** gate
   `logi_runtime/tests/parallel_determinism.rs`: child process pinned to an asserted 1-thread pool produces
   the serial reference, parent asserts a >1-thread pool, full per-tick snapshot-stream digests compared
   bit-for-bit. In-crate test re-scoped as a smoke test. Passes.
9. **Exp C's "SingleThreaded for few/cheap-system schedules" had landed only for Geo** → now landed for all
   gameplay phases in both modes (MT executor was pure overhead at 1–2 systems/phase; `par_iter`
   parallelism is pool-level and unaffected; the modes now differ exactly by pool width — which is what the
   gate exercises). (`schedules.rs`)
10. **Telemetry/guards**: `decoded_this_step` re-wired (orphaned by the ADR-0018 sequential-decode rewrite;
    it is the decision-5 measure-gate signal); `LogiGlobeCreateInfo._reserved` is now `debug_assert`ed to 0
    at create; the per-step `base_url` heap clone hoisted into the rare refresh-enqueue branch.
11. **Stray artifacts removed**: `Rust/t1_build.log`, a path-mangled scratch file at repo root.

**Normative-doc corrections (the governance layer now matches the tree):**
- **ADR-0012**: Status scoped — Geo half implemented; gameplay half (sim thread, `push_input`/`poll`, ABI
  6→7) explicitly PENDING. The "lock-free seqlock" wording corrected to the shipped brief-Mutex slots, with
  the hard rule the gameplay mirror must honor (never hold a slot lock across a fixed sub-step).
- **ADR-0014**: implementation-status banner added; the present-tense arbiter claims ("collapses the two
  formulas… fixing the over-subscription today") de-tensed; **arbiter-as-type demoted** to shared formula +
  per-lane caps + priorities (type only on measured contention, post-gameplay-decouple); the **UE-sizes-off-
  physical premise corrected** (UE 5.8 uses logical−2 — source-verified; the real pre-fix picture was ~2×
  two-near-logical-pools contention, not "3–4× vs UE-physical"); the **vendored-`bevy_ecs` requirement
  withdrawn** (round-4 confirmed `ScheduleGraph::conflicting_systems()` is `pub` — no fork; the ADR-0004
  narrowing is withdrawn with it); Decision 3 carries an inline pointer to the realized compio-net-on-own-
  thread shape; Decision 6 marked PENDING with the two "mechanical mirror" corrections (SnapshotRing needs a
  real cross-thread sync layer — it is owner-thread-only today, no atomics; panic policy must hard-stop);
  #11378 figures re-sourced; Iggy precision; "no tokio" → "no tokio RUNTIME" (the crate rides as transitive
  hyper-util/h2 io-compat shims); stale in-repo line anchors replaced with symbols.
- **ADR-0016**: status marks the `Promoted<T>`/`FarFieldNonDet` boundary designed-not-built, landing with
  the first far-field consumer.
- **Plan (LogiRuntime-Design.md)**: §3.1 rewritten (premise + demotion + landed/not-landed + small-box
  note); §3.2 morsel single-socket provenance; §3.3 no-fork correction; §3.7 rewritten (not a mechanical
  mirror; the load-bearing-deferral framing + trigger); §7.2 **SigIndex governance resolved** (see §4);
  §7.3 added as the ledger of this re-eval; "no tokio" phrasings tightened.
- **Code comments**: both pool-formula docstrings rewritten on the corrected UE premise; the dead
  `get_physical()==0` guard comment scoped honestly (never fires on Windows); `request_extra`'s false
  "re-tried next pass" contract corrected (cap-skipped extras are dropped — zero P3DT impact, the fix
  recipe recorded for general-1.1); ureq `Drop` "frees promptly" scoped to the body phase (connect/stall
  windows are timeout-bounded); the io `Cargo.toml` tokio note tightened; a comparator-drift guard added to
  `sig_index.rs`'s `rank_topk_full`.
- Already fixed by a concurrent session before this audit's fixes landed (verified current): the io crate
  header (h3/parity story), the `(32)`→64 worker-cap comment, the geo-FFI v11 changelog, the windows-gro
  comment.

## 3. Real issues, correctly deferred — with their gates

| Issue | Gate that triggers the build |
|---|---|
| Thread priorities (compio `ABOVE_NORMAL`, blocking `BELOW_NORMAL`+EcoQoS) — all threads run NORMAL today | A contention profile showing UE frame hitches or IOCP-harvest delay under streaming load (~15 lines at the 3 spawn sites + a windows-sys feature) |
| Clip-child AABB recomputed per step while a REPLACE stand-in persists (O(vertices) × ≤4 children) | A transition-heavy flamegraph showing `publish` stage 2; memoize per tile_key or compute at decode |
| ureq `Drop` join can stall up to connect/stall timeouts on a black-holed network (fallback lane only — cyper cancels via task drop) | Observed PIE-exit hitch; then deadline-join + detached stragglers |
| `physical−3` floors to 1 worker on 4-core boxes | Minimum-player-spec decision; consider a proportional reserve below 8 cores |
| Auth-failure session-generation gating (the cooldown floor landed) | A recurrence of 401 storms in live telemetry |
| `compute_threads()` formula collapse (deliberate duplication, documented in lockstep) | Lands with the LogiSimRunner / S0 work |
| True-O(Δ) `reconcile_index` (settled-step skip landed per ADR-0018) | A churn-heavy flamegraph showing the O(index) scan |
| Decode decouple + writer-thread hoist (ADR-0018 dec. 5/6) | Unchanged: moving-camera flamegraph vs idle park; `decoded_this_step` (now wired) is the burst signal |

## 4. Decisions taken / recommended next moves

1. **SigIndex governance — RESOLVED (wire it next).** Round-4 and the plan disagreed; the plan's deferral
   leaned on a precondition (the spatial grid) the wiring does not require. Plan §7.2 now records the
   decision: wiring the validated O(K+Δ) index into `RepresentationPlan` is the next `logi_sim` work item,
   with the equivalence test driven through the real `allocate_representation_budget` (not the spike's
   copy). It kills the last O(P)-per-frame term in gameplay (AGENTS.md §1E / ADR-0015 law).
2. **LogiSimRunner is the load-bearing milestone.** One coordinated Rust+C++ rebuild: the sim thread, the
   `push_input`/`poll` split, ABI 6→7, a SnapshotRing cross-thread synchronization layer (property-tested),
   and a hard-stop panic protocol. Trigger pinned in the plan: before the first non-trivial gameplay system
   lands. Everything else in the runtime roadmap compounds on it.
3. **Cyper default stays (user-directed), with its validation gates recorded.** The strategy judges split
   here; resolution: parity is not a regression, the structural grounds stand (connection economy at the
   capped pool, 1-RTT connect, keep-alive survival), and the one-field `fetch_backend` revert knob plus the
   lean `--no-default-features` build keep it reversible. The open gates remain the full-pipeline and
   lossy-network A/Bs (plan §7.2). **Recommended**: upstream the two vendored seams (TransportConfig,
   h3-fallback+quarantine) as small PRs to compio-rs/cyper — on merge the 6 kLOC fork shrinks to nothing.
   Publishing a PR is an outward-facing act left to the user.
4. **Do-not-do list** (proposals rejected with reasons): reverting the cyper default (above); building the
   CoreArbiter type now (no second lane to arbitrate until gameplay decouples); a `get_or_init` size-equality
   assert (spurious-fires when C++ legitimately requests fewer workers than the cap); re-spiking windows-gro
   (null A/B, quinn #2041 risk); parallelizing the select traversal (deferred per ADR-0018 — not the
   bottleneck, large correctness surface).

## 5. External claims adjudicated (primary sources)

| Claim | Verdict |
|---|---|
| async-executor 1.14: global-queue spawn, no priority, no LIFO slot; bevy_tasks = one task/batch | **Confirmed** (source) |
| bevy MT executor single `Mutex<ExecutorState>` | **Confirmed** (code); the "10–70 µs/#11378 knee at 500–1000" figures were mis-sourced — corrected in ADR-0014 |
| bevy_ecs 0.19.0-rc.2 seams (`SystemExecutor` 3 methods, `set_executor` pub, `get_data_slice_for` pub, no `ExecutorKind`) | **Confirmed** |
| UE 5.8 sizes workers off physical cores | **REFUTED** — logical−2 on SMT (`NumberOfWorkerThreadsToSpawn`); docs corrected, physical−3 re-justified on port-sharing grounds |
| compio Proactor drivable solo; compio-quic = quinn-proto sans-IO, no high-level quinn; raw socket ops `pub(crate)` | **Confirmed** |
| cyper: no tokio | **Partially true** — no tokio *runtime*; the tokio crate (net/io-util) rides transitively via hyper-util/h2/h3. Docs tightened. ureq-no-h2 confirmed. |
| `num_cpus::get_physical()` can return 0 on Windows | **Refuted** (falls back to logical ≥1 internally); comments scoped |
| PostHog one-pool freeze; Iggy TPC+ECS retreat | **Confirmed** (precision: Iggy kept TPC, dropped ECS-under-TPC) |
| Morsel single-cursor = Leis 2014 | **Partially true** — exact as the single-socket specialization; the paper's canonical dispatcher is NUMA-partitioned. Noted in plan §3.2 |
| ComputeTaskPool get_or_init = process-global, first-caller-wins, loser's size silently ignored | **Confirmed** — motivated the cross-process determinism gate |

## 6. Right-path verdicts by dimension (one line each)

- **ffi-poll-contract**: on-track — strongest piece; v12 clip contract eviction-race-safe; ring protocol correct under slow consumers.
- **stream-thread-loop**: on-track-with-corrections — panic guard was the must-fix (landed); loop is a sound template for the gameplay runner *with* the recorded policy differences.
- **geo-workingset (ADR-0018)**: on-track-with-corrections — all four landed decisions verified line-by-line (LiveSet sites, settled-skip producers, debug-only drift asserts); the holes were at the edges (landed fixes #4–5).
- **http-scheduler-io**: on-track-with-corrections — monitor protocol lost-wakeup-free; H5/H6 enforced as documented; keeping the ureq lane is justified (bootstrap/fallback/knob), not drift.
- **cyper-h3-reactor**: on-track-with-corrections — matches ADR-0014 §3.5 as refined; vendored patch minimal; fallback + panic guard were the two real defects (landed).
- **sim-runtime-determinism**: on-track-with-corrections — counter-RNG correct; sort-before-output discipline verified at every sim output; the vacuous gate was the highest-leverage fix (landed).
- **thread-budget**: on-track-with-corrections — compute pool genuinely de-oversubscribed; the blocking-pool cap (landed) + priorities (gated) close the real machine-wide gap without the arbiter type.
- **docs-congruence**: on-track-with-corrections — plans were honest ledgers; the two normative ADRs lagged the tree (corrected).
- **strategy (North-Star)**: on-track-with-corrections — architecture right, spikes honest; sequencing risk = the gameplay decouple (now pinned with a trigger).
- **strategy (YAGNI)**: on-track-with-corrections — deferrals mostly disciplined; arbiter demoted, fork withdrawn, cyper fork to be upstreamed; inverse-YAGNI on SigIndex resolved.
