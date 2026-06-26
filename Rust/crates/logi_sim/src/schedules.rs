//! The phased schedule model (ADR-0005 scheduling treaty). Unreal owns the
//! frame; Bevy owns the step. Hard phase boundaries; explicit executor mode.

use crate::systems::{allocate_representation_budget, compute_significance, integrate_motion};
use bevy_ecs::prelude::*;
use bevy_ecs::schedule::{ScheduleLabel, SingleThreadedExecutor};
use bevy_tasks::{ComputeTaskPool, TaskPoolBuilder};

/// The hard phase boundaries of a single tick. Each runs to completion before
/// the next, in the order the runtime drives them.
///
/// Several phases are **reserved**: they have no systems yet and the runtime skips
/// running them while empty (audit M25 — an empty schedule still has executor
/// overhead per step). They exist so future work lands in the right phase without
/// reshuffling the treaty; add a system via `LogiSchedules::new` and the runtime
/// begins running that phase automatically.
#[derive(ScheduleLabel, Clone, Debug, PartialEq, Eq, Hash)]
pub enum LogiSchedule {
    /// Apply ingested Unreal input (cameras, player intent) into the World.
    /// **Reserved**: the runtime currently ingests input directly into
    /// `UnrealFrameInput` (the data arrives as FFI call args, not yet World state),
    /// so this schedule holds no systems. Reserved for input-derived systems that
    /// read the freshly-ingested resource.
    InputIngest,
    /// Per fixed step: setup before the authoritative sim. **Reserved** (empty).
    FixedPreSim,
    /// Per fixed step: the authoritative gameplay systems.
    FixedSim,
    /// Per fixed step: cleanup/derive after the sim. **Reserved** (empty).
    FixedPostSim,
    /// Once per frame: compute representation/significance plan.
    RepresentationPlan,
    /// Once per frame: prepare the render snapshot.
    RenderExtract,
    /// Once per frame: housekeeping (recycle ids, compact, telemetry).
    /// **Reserved** (empty).
    Maintenance,
}

/// Pool staging. Default is `BoundedParallel` -- it builds the bounded compute pool that the hot
/// per-agent systems fan out over with `par_iter_mut` (the real scaling lever at 1M agents; see
/// `systems.rs`). `SingleThreadedDeterministic` is opt-in for strict replay equality / tests: a 1-thread
/// pool, so `par_iter` takes its serial-fold path. The system EXECUTOR is SingleThreaded in both modes
/// (Exp C — see `LogiSchedules::new`); the modes differ only in pool size. Determinism holds in BOTH because
/// the per-agent systems are conflict-free + write only each entity's own components — order-independent —
/// so parallel output is bit-identical to serial (asserted by `logi_runtime`'s
/// `parallel_execution_matches_single_threaded_bit_for_bit`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogiSchedulerMode {
    SingleThreadedDeterministic,
    BoundedParallel,
}

/// The set of phase schedules driven each tick.
pub struct LogiSchedules {
    pub input_ingest: Schedule,
    pub fixed_pre_sim: Schedule,
    pub fixed_sim: Schedule,
    pub fixed_post_sim: Schedule,
    pub representation_plan: Schedule,
    pub render_extract: Schedule,
    pub maintenance: Schedule,
}

/// Run a phase schedule only if it has any systems (audit M25): the reserved
/// phases are empty, and running an empty schedule still pays the executor's
/// per-step setup. `systems_len()` reflects systems added at build time even
/// before the first run, so a phase that later gains a system runs automatically.
#[inline]
pub fn run_if_populated(schedule: &mut Schedule, world: &mut World) {
    if schedule.systems_len() > 0 {
        schedule.run(world);
    }
}

/// Worker-thread policy: `clamp(requested, 1, max(1, physical - reserved))`.
/// Never "all cores" — leave headroom for Unreal's always-hot threads.
///
/// Sizes off PHYSICAL cores, not logical (ADR-0014). Two grounds, corrected after a
/// source check of UE 5.8 (re-eval 2026-06-09 — the earlier "UE sizes off physical"
/// claim here was WRONG: `NumberOfWorkerThreadsToSpawn` returns **logical − 2** on
/// any SMT box, e.g. 22 workers on 12-physical/24-logical):
/// 1. **Don't add a second near-logical pool on top of UE's.** UE already runs
///    ~logical−2 TaskGraph workers + 3 always-hot named threads (Game/Render/RHI);
///    our old `logical − 2` pool stacked a second 22-thread pool on the same 12
///    cores — ~2× mutual over-subscription. A `physical − 3` pool leaves the named
///    threads' cores uncontested.
/// 2. **HT siblings share execution ports.** Two hyperthreads on one core share its
///    execution units and L1/L2; on the cache-/SIMD-bound per-agent and decode hot
///    paths a sibling-thread worker adds contention and scheduler thrash, not
///    throughput — so logical-count workers are not "free parallelism" even alone.
///
/// We therefore reserve 3 physical cores for UE's named threads and let the pool
/// take the rest: `physical − 3` (12 → 9 here). NB on small boxes this floors hard
/// (4-core ⇒ 1 worker) — a deliberate UE-first conservatism; revisit the reserve
/// against the real minimum player spec before shipping (re-eval 2026-06-09).
/// Keep this formula in lockstep with `geo_compute_threads`
/// (`logi_3dtiles_scene/src/geo_world.rs`) — deliberately duplicated, not shared,
/// to avoid a cross-tower dependency; the planned S0 `compute_threads()` collapse
/// lands with the LogiSimRunner work (ADR-0014 §3.1).
pub fn resolve_worker_thread_count(requested: u32) -> usize {
    // Keep UE's three named threads (Game / Render / RHI) on uncontested physical cores.
    const UNREAL_RESERVED_THREADS: usize = 3;
    let physical = num_cpus::get_physical();
    let physical = if physical > 0 {
        physical
    } else {
        // Cross-platform belt-and-braces: on Windows `get_physical()` never returns 0
        // (it falls back to the logical count internally — source-verified 2026-06-09).
        // Elsewhere, logical/4 errs toward leaving UE headroom on unknown topology.
        (std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4) / 4).max(1)
    };
    let cap = physical.saturating_sub(UNREAL_RESERVED_THREADS).max(1);
    (requested.max(1) as usize).clamp(1, cap)
}

/// Ensure the one global `ComputeTaskPool` exists, bounded to `threads` workers. Idempotent: `get_or_init`'s
/// closure runs only for the FIRST caller in the process, so the gameplay and geo towers share ONE bounded
/// pool and neither grabs all cores (the geo tower used to build an *unbounded* pool — gone). Under the
/// `multi_threaded` feature `par_iter` calls `ComputeTaskPool::get()` and PANICS if the pool is absent, so a
/// pool must exist before any schedule with a `par_iter` system runs — including single-threaded mode, where
/// a 1-thread pool makes `par_iter` take its serial-fold path (`thread_num() <= 1`).
fn ensure_compute_pool(threads: usize) {
    ComputeTaskPool::get_or_init(|| {
        TaskPoolBuilder::new()
            .num_threads(threads.max(1))
            .thread_name("LogiBevyCompute".to_string())
            .build()
    });
}

impl LogiSchedules {
    pub fn new(mode: LogiSchedulerMode, worker_thread_count: u32) -> Self {
        // Build the shared compute pool BEFORE any schedule runs (see `ensure_compute_pool`). The executor
        // is set per-schedule below: in 0.19 (`set_executor` replaced `ExecutorKind`) an executor is an
        // owned instance rather than a copyable enum, so each schedule gets its own fresh one.
        match mode {
            LogiSchedulerMode::SingleThreadedDeterministic => {
                // Strict-serial reference: a 1-thread pool keeps every `par_iter` on its serial-fold path
                // while the executor runs systems one at a time. A pool must still exist (see above).
                ensure_compute_pool(1);
            }
            LogiSchedulerMode::BoundedParallel => {
                ensure_compute_pool(resolve_worker_thread_count(worker_thread_count));
            }
        }

        let mut input_ingest = Schedule::new(LogiSchedule::InputIngest);
        let mut fixed_pre_sim = Schedule::new(LogiSchedule::FixedPreSim);
        let mut fixed_sim = Schedule::new(LogiSchedule::FixedSim);
        let mut fixed_post_sim = Schedule::new(LogiSchedule::FixedPostSim);
        let mut representation_plan = Schedule::new(LogiSchedule::RepresentationPlan);
        let mut render_extract = Schedule::new(LogiSchedule::RenderExtract);
        let mut maintenance = Schedule::new(LogiSchedule::Maintenance);

        for schedule in [
            &mut input_ingest,
            &mut fixed_pre_sim,
            &mut fixed_sim,
            &mut fixed_post_sim,
            &mut representation_plan,
            &mut render_extract,
            &mut maintenance,
        ] {
            // 0.19: `set_executor` takes an owned executor instance (replaces `set_executor_kind`).
            // EVERY phase runs the SingleThreaded system executor in BOTH modes (Exp C, landed for
            // gameplay 2026-06-09 — Geo got this with ADR-0018): at our 1–2 systems per phase the
            // MultiThreaded executor is pure per-step overhead (8.4 µs vs 1.0 µs at N=4) with nothing
            // to run concurrently — the 1M-agent parallelism is INTRA-system `par_iter` over the pool
            // built above, which the system executor choice does not affect. The two modes differ only
            // in POOL SIZE (1 vs bounded-N), which is exactly the serial-vs-parallel `par_iter` lever
            // the determinism gate exercises. Revisit per-phase (switch a phase to
            // `MultiThreadedExecutor`) only when it actually holds several independent systems.
            schedule.set_executor(SingleThreadedExecutor::new());
        }

        // Gameplay systems land in their phase (more as the sim grows). `integrate_motion` +
        // `compute_significance` are per-agent and `par_iter_mut` internally — the parallelism that scales to
        // 1M agents comes from THAT (intra-system), not from the executor running these few systems
        // concurrently. The pool the executor builds is what those `par_iter`s fan out over.
        fixed_sim.add_systems(integrate_motion);
        // Bevy computes the representation decision; Unreal executes it (ADR-0003). `compute_significance`
        // par_iters; `allocate_representation_budget` (chained after it) is a sequential quantized
        // selection against the persistent `SignificanceIndex` (ADR-0020 R1 — O(k log k + boundary)
        // selection, event-driven eviction; the O(P) re-rank is gone).
        representation_plan.add_systems((compute_significance, allocate_representation_budget).chain());

        Self {
            input_ingest,
            fixed_pre_sim,
            fixed_sim,
            fixed_post_sim,
            representation_plan,
            render_extract,
            maintenance,
        }
    }
}
