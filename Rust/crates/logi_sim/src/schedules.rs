//! The deterministic fixed-step Schedule.

use crate::systems::{advance_clock, integrate_motion};
use bevy_ecs::prelude::*;
use bevy_ecs::schedule::{ExecutorKind, ScheduleLabel};

/// Label for the deterministic fixed-step simulation schedule.
#[derive(ScheduleLabel, Clone, Debug, PartialEq, Eq, Hash)]
pub struct FixedStep;

/// Build the fixed-step schedule. Single-threaded executor so iteration order is
/// deterministic for replay/snapshot equality (ADR-0001 Determinism).
pub fn build_fixed_step_schedule() -> Schedule {
    let mut schedule = Schedule::new(FixedStep);
    schedule.set_executor_kind(ExecutorKind::SingleThreaded);
    schedule.add_systems((advance_clock, integrate_motion).chain());
    schedule
}
