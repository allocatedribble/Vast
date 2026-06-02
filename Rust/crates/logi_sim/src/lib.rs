//! `logi_sim` — gameplay Components, Resources, Systems, the fixed-step Schedule,
//! and the terrain authority (folded in; ADR-0002, can be re-extracted later).
//! Depends on `bevy_ecs` as a library — never the full Bevy engine.

pub mod components;
pub mod resources;
pub mod schedules;
pub mod systems;
pub mod terrain;

pub use components::*;
pub use resources::*;
pub use schedules::{build_fixed_step_schedule, FixedStep};
