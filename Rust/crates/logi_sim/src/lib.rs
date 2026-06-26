//! `logi_sim` — gameplay Components, Resources, Systems, the phased fixed-step
//! schedules, and the terrain authority (folded in; ADR-0002, re-extractable).
//! Depends on `bevy_ecs` + `bevy_tasks` as libraries — never the full Bevy engine.

pub mod components;
pub mod morsel;
pub mod resources;
pub mod schedules;
pub mod systems;
pub mod terrain;
// The Exp-D significance index spike (`sig_index`) moved UP into the ECS fork as
// `bevy_ecs::incremental::BucketIndex` (logi_ecs divergence D-incremental, ADR-0020) — wiring it
// into `systems::allocate_representation_budget` is the queued ADR-0015 §3 step.

pub use components::*;
pub use resources::*;
pub use schedules::{resolve_worker_thread_count, LogiSchedule, LogiSchedulerMode, LogiSchedules};
