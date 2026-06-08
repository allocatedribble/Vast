//! The phased schedule model (ADR-0005 scheduling treaty). Unreal owns the
//! frame; Bevy owns the step. Hard phase boundaries; explicit executor mode.

use crate::systems::{allocate_representation_budget, compute_significance, integrate_motion};
use bevy_ecs::prelude::*;
use bevy_ecs::schedule::{MultiThreadedExecutor, ScheduleLabel, SingleThreadedExecutor};
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

/// Executor staging. Default is `BoundedParallel` -- it builds the bounded compute pool that the hot
/// per-agent systems fan out over with `par_iter_mut` (the real scaling lever at 1M agents; see
/// `systems.rs`), and runs independent systems concurrently. `SingleThreadedDeterministic` is opt-in for
/// strict replay equality / tests: it runs everything serially (on a 1-thread pool, so `par_iter` takes its
/// serial-fold path). Determinism holds in BOTH because the per-agent systems are conflict-free + write only each
/// entity's own components — order-independent — so parallel output is bit-identical to serial (asserted by
/// `logi_runtime`'s `parallel_execution_matches_single_threaded_bit_for_bit`).
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

/// Worker-thread policy: `clamp(requested, 1, max(1, cores - reserved))`.
/// Never "all cores" — leave headroom for Unreal's game/render threads.
///
/// TODO(perf, audit Low): uses LOGICAL cores as a proxy for the core budget.
/// Hyperthreaded logical cores are not full execution resources, so on SMT CPUs
/// this can over-subscribe the compute pool versus Unreal. Refine with
/// physical-core detection (e.g. a `num_cpus::get_physical()`-style probe) and
/// re-tune `UNREAL_RESERVED_THREADS` once measured on target hardware.
pub fn resolve_worker_thread_count(requested: u32) -> usize {
    const UNREAL_RESERVED_THREADS: usize = 2;
    let logical = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
    let cap = logical.saturating_sub(UNREAL_RESERVED_THREADS).max(1);
    (requested.max(1) as usize).min(cap)
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
            // `SingleThreaded` keeps strict-serial replay equality; `MultiThreaded` runs independent
            // systems concurrently over the shared pool built above.
            match mode {
                LogiSchedulerMode::SingleThreadedDeterministic => {
                    schedule.set_executor(SingleThreadedExecutor::new());
                }
                LogiSchedulerMode::BoundedParallel => {
                    schedule.set_executor(MultiThreadedExecutor::new());
                }
            }
        }

        // Gameplay systems land in their phase (more as the sim grows). `integrate_motion` +
        // `compute_significance` are per-agent and `par_iter_mut` internally — the parallelism that scales to
        // 1M agents comes from THAT (intra-system), not from the executor running these few systems
        // concurrently. The pool the executor builds is what those `par_iter`s fan out over.
        fixed_sim.add_systems(integrate_motion);
        // Bevy computes the representation decision; Unreal executes it (ADR-0003). `compute_significance`
        // par_iters; `allocate_representation_budget` is a sequential global ranking (chained after it).
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
