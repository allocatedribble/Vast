//! Gameplay Systems — all logic lives here, pure over its Queries, deterministic.

use crate::components::{Position, Velocity};
use crate::resources::SimClock;
use bevy_ecs::prelude::*;
use logi_schema::Vec3;

/// Advance the fixed-step clock by one tick.
pub fn advance_clock(mut clock: ResMut<SimClock>) {
    clock.tick = clock.tick.wrapping_add(1);
}

/// Integrate motion deterministically: position += velocity * dt.
pub fn integrate_motion(clock: Res<SimClock>, mut query: Query<(&mut Position, &Velocity)>) {
    let dt = clock.seconds_per_tick();
    for (mut pos, vel) in &mut query {
        pos.0 = Vec3::new(
            pos.0.x + vel.0.x * dt,
            pos.0.y + vel.0.y * dt,
            pos.0.z + vel.0.z * dt,
        );
    }
}
