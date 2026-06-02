//! Gameplay Systems — all logic lives here, pure over its Queries, deterministic.
//! (The fixed-step tick counter is advanced explicitly by the runtime between
//! phases, not by a system, so the phase boundaries stay clean.)

use crate::components::{CullState, Representation, StableId, Transform3d, Velocity3d};
use crate::resources::{RepresentationBudget, SimClock, UnrealFrameInput};
use bevy_ecs::prelude::*;
use logi_schema::RepresentationTier;
use std::cmp::Ordering;
use std::collections::HashMap;

/// Integrate motion deterministically: translation += linear_velocity * dt.
pub fn integrate_motion(clock: Res<SimClock>, mut query: Query<(&mut Transform3d, &Velocity3d)>) {
	let dt = f64::from(clock.seconds_per_tick());
	for (mut transform, vel) in &mut query {
		// Skip stationary entities so we don't mark Transform3d changed (keeps the
		// render-extract delta minimal -- §8 change detection).
		if vel.linear_cm_s == [0.0, 0.0, 0.0] {
			continue;
		}
		transform.translation_cm[0] += f64::from(vel.linear_cm_s[0]) * dt;
		transform.translation_cm[1] += f64::from(vel.linear_cm_s[1]) * dt;
		transform.translation_cm[2] += f64::from(vel.linear_cm_s[2]) * dt;
	}
}

/// Compute each entity's significance score from camera proximity + projected
/// screen area (ADR-0003). This is the *score*; budgeting ranks against it.
pub fn compute_significance(
	input: Res<UnrealFrameInput>,
	mut query: Query<(&Transform3d, &CullState, &mut Representation)>,
) {
	for (transform, cull, mut rep) in &mut query {
		let mut nearest_sq = f64::MAX;
		for cam in &input.cameras {
			let dx = transform.translation_cm[0] - cam.position[0];
			let dy = transform.translation_cm[1] - cam.position[1];
			let dz = transform.translation_cm[2] - cam.position[2];
			let d2 = dx * dx + dy * dy + dz * dz;
			if d2 < nearest_sq {
				nearest_sq = d2;
			}
		}
		let distance_term = if nearest_sq == f64::MAX {
			0.0f32
		} else {
			// Closer => larger.
			(1.0 / (1.0 + nearest_sq.sqrt() * 1.0e-4)) as f32
		};
		let area_term = cull.projected_screen_area.max(0.0);
		// Significance from nearest-camera proximity (+ projected screen area once
		// a culling system feeds CullState). Camera position drives promotion to
		// actor tiers; a dedicated culling/frustum system will refine visibility.
		let _ = cull.visible_to_any_camera;
		let significance = distance_term + area_term;
		// Conditional write: only mark Changed when it actually changes (§8).
		if rep.significance != significance {
			rep.significance = significance;
		}
	}
}

/// Allocate representation tiers TOP-DOWN by significance against the budget
/// (ADR-0003) — never by raw distance. Ties break by StableId for determinism.
pub fn allocate_representation_budget(
	budget: Res<RepresentationBudget>,
	mut query: Query<(&StableId, &mut Representation)>,
) {
	// Rank by (significance desc, stable-id asc) — deterministic ordering.
	let mut ranked: Vec<(u64, f32)> = query.iter().map(|(id, rep)| (id.0.index, rep.significance)).collect();
	ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(Ordering::Equal).then_with(|| a.0.cmp(&b.0)));

	let mut high = budget.high_res_actors;
	let mut low = budget.low_res_actors;
	let mut ism = budget.instanced_meshes;

	let mut tier_for: HashMap<u64, RepresentationTier> = HashMap::with_capacity(ranked.len());
	for (id, significance) in &ranked {
		let tier = if *significance <= 0.0 {
			RepresentationTier::None
		} else if high > 0 {
			high -= 1;
			RepresentationTier::HighResActor
		} else if low > 0 {
			low -= 1;
			RepresentationTier::LowResActor
		} else if ism > 0 {
			ism -= 1;
			RepresentationTier::InstancedStaticMesh
		} else {
			RepresentationTier::None
		};
		tier_for.insert(*id, tier);
	}

	for (id, mut rep) in &mut query {
		if let Some(&tier) = tier_for.get(&id.0.index) {
			// Conditional writes preserve change detection (§8): unchanged
			// representation does not re-enter the render-extract delta.
			if rep.desired_tier != tier {
				rep.desired_tier = tier;
			}
			let next = step_tier(rep.current_tier, tier);
			if rep.current_tier != next {
				rep.current_tier = next;
			}
		}
	}
}

/// Hysteresis: move `current` one tier toward `desired` per plan to damp churn.
fn step_tier(current: RepresentationTier, desired: RepresentationTier) -> RepresentationTier {
	let c = current as i32;
	let d = desired as i32;
	let next = if d > c { c + 1 } else if d < c { c - 1 } else { c };
	match next {
		0 => RepresentationTier::None,
		1 => RepresentationTier::InstancedStaticMesh,
		2 => RepresentationTier::LowResActor,
		_ => RepresentationTier::HighResActor,
	}
}

#[cfg(test)]
mod tests {
	use super::allocate_representation_budget;
	use crate::components::{Representation, StableId};
	use crate::resources::RepresentationBudget;
	use crate::schedules::LogiSchedule;
	use bevy_ecs::prelude::*;
	use logi_schema::{LogiEntityId, RepresentationTier};
	use std::collections::HashMap;

	#[test]
	fn budget_allocates_tiers_top_down_by_significance() {
		let mut world = World::new();
		world.insert_resource(RepresentationBudget { high_res_actors: 1, low_res_actors: 1, instanced_meshes: 10 });

		let mut spawn = |index: u64, significance: f32| {
			world.spawn((
				StableId(LogiEntityId { index, generation: 1, shard: 0, kind: 1 }),
				Representation { significance, ..Default::default() },
			));
		};
		spawn(1, 0.1);
		spawn(2, 0.9);
		spawn(3, 0.5);

		let mut schedule = Schedule::new(LogiSchedule::RepresentationPlan);
		schedule.add_systems(allocate_representation_budget);
		schedule.run(&mut world);

		let mut query = world.query::<(&StableId, &Representation)>();
		let mut tier: HashMap<u64, RepresentationTier> = HashMap::new();
		for (id, rep) in query.iter(&world) {
			tier.insert(id.0.index, rep.desired_tier);
		}

		// Top-down: highest significance wins the scarce HighRes slot, etc.
		assert_eq!(tier[&2], RepresentationTier::HighResActor); // 0.9
		assert_eq!(tier[&3], RepresentationTier::LowResActor); //  0.5
		assert_eq!(tier[&1], RepresentationTier::InstancedStaticMesh); // 0.1
	}
}
