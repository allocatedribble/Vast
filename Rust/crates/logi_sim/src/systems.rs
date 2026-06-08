//! Gameplay Systems — all logic lives here, pure over its Queries, deterministic.
//! (The fixed-step tick counter is advanced explicitly by the runtime between
//! phases, not by a system, so the phase boundaries stay clean.)

use crate::components::{Representation, StableId, Transform3d, Velocity3d};
use crate::resources::{RepresentationBudget, SimClock, UnrealFrameInput};
use bevy_ecs::entity::EntityHashMap;
use bevy_ecs::prelude::*;
use logi_schema::{LogiEntityId, RepresentationTier};
use std::cmp::Ordering;

/// Significance distance falloff (1/cm). At distance `d` cm the proximity term is
/// `1 / (1 + d * SIGNIFICANCE_DISTANCE_FALLOFF)`, so it halves roughly every
/// `1/FALLOFF = 10 km`. Tuned for planetary cm-scale: large enough that a courier
/// hundreds of metres from the camera still earns meaningful significance, small
/// enough that orbital-distance entities fall toward zero. Gameplay-tuning value.
const SIGNIFICANCE_DISTANCE_FALLOFF: f64 = 1.0e-4;

/// Integrate motion deterministically: translation += linear_velocity * dt.
///
/// `par_iter_mut` fans the per-entity integration across the compute pool (scales to 1M agents). Each entity
/// touches only its OWN `Transform3d` — no cross-entity reads, no reduction — so the result is bit-identical
/// regardless of thread count / batch order, preserving replay determinism (ADR-0001). Under the
/// single-threaded mode (no pool) it runs serially, the golden-replay reference. Work is uniform + cheap, so
/// the default batching (≈one batch per worker) is the right fit (no work-stealing needed).
pub fn integrate_motion(clock: Res<SimClock>, mut query: Query<(&mut Transform3d, &Velocity3d)>) {
    let dt = f64::from(clock.seconds_per_tick());
    query.par_iter_mut().for_each(|(mut transform, vel)| {
        // Skip stationary entities so we don't mark Transform3d changed (keeps the
        // render-extract delta minimal -- §8 change detection).
        if vel.linear_cm_s == [0.0, 0.0, 0.0] {
            return;
        }
        transform.translation_cm[0] += f64::from(vel.linear_cm_s[0]) * dt;
        transform.translation_cm[1] += f64::from(vel.linear_cm_s[1]) * dt;
        transform.translation_cm[2] += f64::from(vel.linear_cm_s[2]) * dt;
    });
}

/// Compute each entity's significance score from nearest-camera proximity
/// (ADR-0003). This is the *score*; budgeting ranks against it.
///
/// `CullState` is deliberately NOT queried here yet: no culling/frustum system
/// populates it, so its `projected_screen_area` / `visible_to_any_camera` would be
/// constant defaults and contribute nothing (audit Low). When a culling system
/// lands, re-add `&CullState` and fold the screen-area term back in.
pub fn compute_significance(input: Res<UnrealFrameInput>, mut query: Query<(&Transform3d, &mut Representation)>) {
    // `par_iter_mut` over the agents (scales to 1M). Each entity reads the shared, read-only camera list and
    // writes only its OWN `significance` — independent per entity, no reduction — so it's bit-identical at any
    // thread count (replay-deterministic, ADR-0001). `cameras` is borrowed once and shared across workers.
    let cameras = input.cameras.as_slice();
    query.par_iter_mut().for_each(|(transform, mut rep)| {
        let mut nearest_sq = f64::MAX;
        for cam in cameras {
            let dx = transform.translation_cm[0] - cam.position[0];
            let dy = transform.translation_cm[1] - cam.position[1];
            let dz = transform.translation_cm[2] - cam.position[2];
            let d2 = dx * dx + dy * dy + dz * dz;
            if d2 < nearest_sq {
                nearest_sq = d2;
            }
        }
        let significance = if nearest_sq == f64::MAX {
            0.0f32
        } else {
            // Closer => larger (see SIGNIFICANCE_DISTANCE_FALLOFF).
            (1.0 / (1.0 + nearest_sq.sqrt() * SIGNIFICANCE_DISTANCE_FALLOFF)) as f32
        };
        // Sanitize to a finite value: a NaN significance would make the budget sort order
        // input-dependent and break replay determinism (ADR-0001).
        let significance = if significance.is_finite() { significance } else { 0.0 };
        // Conditional write: only mark Changed when it actually changes (§8).
        if rep.significance != significance {
            rep.significance = significance;
        }
    });
}

/// One entry in the per-frame significance ranking.
#[derive(Clone, Copy)]
struct RankEntry {
    significance: f32,
    id: LogiEntityId,
    entity: Entity,
}

/// Per-system scratch, reused across frames via `Local` so budgeting allocates nothing on the hot path.
/// Opaque (all fields private); only `allocate_representation_budget` touches it.
#[derive(Default)]
pub struct BudgetScratch {
    /// `(significance, stable id, entity)` for every live entity this frame.
    ranked: Vec<RankEntry>,
    /// Tier assigned to each selected (top-K) entity; an absent entity ⇒ `None` tier.
    assignment: EntityHashMap<RepresentationTier>,
}

/// Total, deterministic tie-break key over the full stable identity — not just `index`, so two entities
/// that ever share an `index` across shards/kinds can never collide (the prior `index`-only key could).
#[inline]
fn stable_key(id: &LogiEntityId) -> (u64, u32, u16, u16) {
    (id.index, id.generation, id.shard, id.kind)
}

/// Allocate representation tiers TOP-DOWN by significance against the budget (ADR-0003) — never by raw
/// distance. Ties break by stable id for determinism.
///
/// Scales to 1M agents: a bounded top-K selection (`select_nth_unstable`, O(n) average) replaces a full
/// O(n·log n) sort of the whole population; the scratch buffers are reused across frames (zero per-frame
/// allocation); and the entity→tier map is keyed by `Entity` (correct across shards) with a fast hash.
pub fn allocate_representation_budget(
    budget: Res<RepresentationBudget>,
    mut scratch: Local<BudgetScratch>,
    mut query: Query<(Entity, &StableId, &mut Representation)>,
) {
    let BudgetScratch { ranked, assignment } = &mut *scratch;

    // Pass 1 (read): snapshot significance. NaN is sanitized to 0.0 so the ordering is total and
    // replay-deterministic (a NaN would otherwise make the sort order input-dependent).
    ranked.clear();
    for (entity, id, rep) in query.iter() {
        let significance = if rep.significance.is_finite() {
            rep.significance
        } else {
            0.0
        };
        ranked.push(RankEntry {
            significance,
            id: id.0,
            entity,
        });
    }

    // Rank by (significance desc, stable-id asc). Both operands are finite after sanitisation, so
    // `partial_cmp` only yields `Equal` for truly equal scores — the stable-id tie-break then totalises it.
    let cmp = |a: &RankEntry, b: &RankEntry| {
        b.significance
            .partial_cmp(&a.significance)
            .unwrap_or(Ordering::Equal)
            .then_with(|| stable_key(&a.id).cmp(&stable_key(&b.id)))
    };

    // Only the top `k = high + low + ism` entities can earn a non-None tier; rank just those.
    let k = (budget.high_res_actors as usize)
        .saturating_add(budget.low_res_actors as usize)
        .saturating_add(budget.instanced_meshes as usize)
        .min(ranked.len());
    if k >= ranked.len() {
        ranked.sort_unstable_by(cmp);
    } else if k > 0 {
        ranked.select_nth_unstable_by(k - 1, cmp);
        ranked[..k].sort_unstable_by(cmp);
    }

    // Assign tiers positionally to the ranked top-K.
    assignment.clear();
    let mut high = budget.high_res_actors;
    let mut low = budget.low_res_actors;
    let mut ism = budget.instanced_meshes;
    for entry in &ranked[..k] {
        let tier = if entry.significance <= 0.0 {
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
        if tier != RepresentationTier::None {
            assignment.insert(entry.entity, tier);
        }
    }

    // Pass 2 (write): apply the desired tier (None for anything not selected). Conditional writes preserve
    // change detection (§8) so unchanged representation stays out of the render-extract delta.
    for (entity, _id, mut rep) in &mut query {
        let tier = assignment.get(&entity).copied().unwrap_or(RepresentationTier::None);
        if rep.desired_tier != tier {
            rep.desired_tier = tier;
        }
        let next = step_tier(rep.current_tier, tier);
        if rep.current_tier != next {
            rep.current_tier = next;
        }
    }
}

/// Hysteresis: move `current` one tier toward `desired` per plan to damp churn.
fn step_tier(current: RepresentationTier, desired: RepresentationTier) -> RepresentationTier {
    let c = current as i32;
    let d = desired as i32;
    let next = if d > c {
        c + 1
    } else if d < c {
        c - 1
    } else {
        c
    };
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
    use hashbrown::HashMap;
    use logi_schema::{LogiEntityId, RepresentationTier};

    #[test]
    fn budget_allocates_tiers_top_down_by_significance() {
        let mut world = World::new();
        world.insert_resource(RepresentationBudget {
            high_res_actors: 1,
            low_res_actors: 1,
            instanced_meshes: 10,
        });

        let mut spawn = |index: u64, significance: f32| {
            world.spawn((
                StableId(LogiEntityId {
                    index,
                    generation: 1,
                    shard: 0,
                    kind: 1,
                }),
                Representation {
                    significance,
                    ..Default::default()
                },
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
