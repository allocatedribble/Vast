//! `logi_sim` — gameplay Components, Resources, Systems, and the fixed-step
//! Schedule. Depends on `bevy_ecs` (library use only) — never the full Bevy engine.
//! This is where authoritative logistics gameplay lives (ADR-0005).

pub mod components;
pub mod resources;
pub mod schedules;
pub mod systems;

pub use components::*;
pub use resources::*;
pub use schedules::{build_fixed_step_schedule, FixedStep};
