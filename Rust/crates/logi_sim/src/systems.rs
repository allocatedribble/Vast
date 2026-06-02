//! Gameplay Systems — all logic lives here, pure over its Queries, deterministic.
//! (The fixed-step tick counter is advanced explicitly by the runtime between
//! phases, not by a system, so the phase boundaries stay clean.)

use crate::components::{Position, Velocity};
use crate::resources::SimClock;
use bevy_ecs::prelude::*;
use logi_schema::Vec3;

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
