//! The phased schedule model (ADR-0005 scheduling treaty). Unreal owns the
//! frame; Bevy owns the step. Hard phase boundaries; explicit executor mode.

use crate::systems::{allocate_representation_budget, compute_significance, integrate_motion};
use bevy_ecs::prelude::*;
use bevy_ecs::schedule::{ExecutorKind, ScheduleLabel};
use bevy_tasks::{ComputeTaskPool, TaskPoolBuilder};

/// The hard phase boundaries of a single tick. Each runs to completion before
/// the next, in the order the runtime drives them.
#[derive(ScheduleLabel, Clone, Debug, PartialEq, Eq, Hash)]
pub enum LogiSchedule {
    /// Apply ingested Unreal input (cameras, player intent) into the World.
    InputIngest,
    /// Per fixed step: setup before the authoritative sim.
    FixedPreSim,
    /// Per fixed step: the authoritative gameplay systems.
    FixedSim,
    /// Per fixed step: cleanup/derive after the sim.
    FixedPostSim,
    /// Once per frame: compute representation/significance plan.
    RepresentationPlan,
    /// Once per frame: prepare the render snapshot.
    RenderExtract,
    /// Once per frame: housekeeping (recycle ids, compact, telemetry).
    Maintenance,
}

/// Executor staging. Default is `BoundedParallel` (multi-threaded) -- systems run
/// in parallel by default and parallelism is encouraged. `SingleThreadedDeterministic`
/// is opt-in for strict replay equality / tests; determinism otherwise comes from
/// explicit system ordering, not from serializing execution.
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

/// Worker-thread policy: `clamp(requested, 1, max(1, cores - reserved))`.
/// Never "all cores" — leave headroom for Unreal's game/render threads.
/// (Uses logical cores as a proxy; refine with physical-core detection later.)
pub fn resolve_worker_thread_count(requested: u32) -> usize {
    const UNREAL_RESERVED_THREADS: usize = 2;
    let logical = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
    let cap = logical.saturating_sub(UNREAL_RESERVED_THREADS).max(1);
    (requested.max(1) as usize).min(cap)
}

impl LogiSchedules {
    pub fn new(mode: LogiSchedulerMode, worker_thread_count: u32) -> Self {
        let kind = match mode {
            LogiSchedulerMode::SingleThreadedDeterministic => ExecutorKind::SingleThreaded,
            LogiSchedulerMode::BoundedParallel => {
                let threads = resolve_worker_thread_count(worker_thread_count);
                // Bound the Bevy compute pool explicitly (never logical-core default).
                ComputeTaskPool::get_or_init(|| {
                    TaskPoolBuilder::new()
                        .num_threads(threads)
                        .thread_name("LogiBevyCompute".to_string())
                        .build()
                });
                ExecutorKind::MultiThreaded
            }
        };

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
            schedule.set_executor_kind(kind);
        }

        // Gameplay systems land in their phase (more as the sim grows).
        fixed_sim.add_systems(integrate_motion);
        // Bevy computes the representation decision; Unreal executes it (ADR-0003).
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
