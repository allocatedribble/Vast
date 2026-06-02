//! `logi_sim` — gameplay Components, Resources, Systems, the phased fixed-step
//! schedules, and the terrain authority (folded in; ADR-0002, re-extractable).
//! Depends on `bevy_ecs` + `bevy_tasks` as libraries — never the full Bevy engine.

pub mod components;
pub mod resources;
pub mod schedules;
pub mod systems;
pub mod terrain;

pub use components::*;
pub use resources::*;
pub use schedules::{resolve_worker_thread_count, LogiSchedule, LogiSchedulerMode, LogiSchedules};
