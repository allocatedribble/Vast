//! Gameplay Systems — all logic lives here, pure over its Queries, deterministic.
//! (The fixed-step tick counter is advanced explicitly by the runtime between
//! phases, not by a system, so the phase boundaries stay clean.)

use crate::components::{Representation, StableId, Transform3d, Velocity3d};
use crate::resources::{RepresentationBudget, SignificanceIndex, SimClock, UnrealFrameInput};
use bevy_ecs::entity::{Entities, EntityHashMap, EntityIndex};
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

/// One entry in the exact re-ranking of the *selected* set (k-sized, never population-sized).
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
    /// The quantized top-K slots out of `SignificanceIndex` (k-sized).
    selected: Vec<u32>,
    /// `(significance, stable id, entity)` for the SELECTED entities only (k-sized).
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
/// distance — ranking against the persistent [`SignificanceIndex`] (ADR-0020 R1, ADR-0015 §3) instead of
/// re-ranking the whole population every frame.
///
/// Shape per frame:
/// 1. **Evict** (`O(Δ_removed)`): drain `RemovedComponents<Representation>` (includes despawns) out of the
///    index — event-driven, never a scan.
/// 2. **Feed** (`O(P)` iterate, `O(Δ)` structural): upsert every live entity's significance; an upsert is a
///    true no-op unless the score crossed a quantum boundary. (This pass replaces the old `O(P)` snapshot
///    *and* will shrink to `O(Δ)` when the write-time change-detection feed lands — ADR-0020 R1 cont.; the
///    iteration order does not affect the result, so the feed is order-independent.)
/// 3. **Select** (`O(k log k + boundary)`): quantized top-K out of the index, independent of population.
/// 4. **Re-rank exactly** (`O(k log k)`): the SELECTED set is ordered by `(significance desc, stable_key
///    asc)` — the original exact comparator — before tiers are assigned. So quantization decides only WHO
///    crosses the budget boundary (boundary ties within one score quantum resolve by ascending entity
///    index — deterministic and replay-stable); it never reorders how the selected are tiered.
///
/// The acceptance contract (vs the retired exact full ranking): a selected entity is never more than one
/// score quantum (1/255) below any rejected one — `bevy_ecs::incremental::bucket_of` is the quantizer —
/// and the result is a pure function of (significances, entity indices, stable ids), so replay and the
/// cross-process determinism gate hold. Locked by `budget_selection_is_quantized_top_k_on_a_live_world`.
pub fn allocate_representation_budget(
    budget: Res<RepresentationBudget>,
    entities: &Entities,
    mut sig_index: ResMut<SignificanceIndex>,
    mut removed: RemovedComponents<Representation>,
    mut scratch: Local<BudgetScratch>,
    mut query: Query<(Entity, &StableId, &mut Representation)>,
) {
    let BudgetScratch {
        selected,
        ranked,
        assignment,
    } = &mut *scratch;

    // 1) Evict: event-driven removals (despawn removes every component, so despawns arrive here too).
    for entity in removed.read() {
        sig_index.remove(entity);
    }

    // 2) Feed: upsert every live entity. NaN/∞ quantize to the bottom bucket (the same "never outranks a
    // genuine score" guarantee the old NaN sanitisation gave the comparator).
    for (entity, _id, rep) in query.iter() {
        sig_index.upsert(entity, rep.significance);
    }
    // Drift check (debug builds): the index occupancy must mirror the live population exactly — the
    // LiveSet discipline. A miss means a removal event was lost or a writer bypassed the feed.
    debug_assert_eq!(
        sig_index.occupied(),
        query.iter().count(),
        "SignificanceIndex drifted from the live Representation population"
    );

    // 3) Select: quantized top-K, population-independent. `select_top_k` self-truncates when fewer
    // entities are live than the budget sum.
    let k = (budget.high_res_actors as usize)
        .saturating_add(budget.low_res_actors as usize)
        .saturating_add(budget.instanced_meshes as usize);
    sig_index.select_top_k(k, selected);

    // 4) Exact re-rank of the selected set (k-sized): resolve slot → Entity → (significance, stable id),
    // then order by the original exact comparator. Both operands are finite after sanitisation, so
    // `partial_cmp` only yields `Equal` for truly equal scores — the stable-id tie-break then totalises it.
    ranked.clear();
    for &slot in selected.iter() {
        let Some(index) = EntityIndex::from_raw_u32(slot) else {
            continue; // u32::MAX is not a valid entity index; unreachable from a real upsert.
        };
        let entity = entities.resolve_from_index(index);
        match query.get(entity) {
            Ok((entity, id, rep)) => {
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
            Err(_) => {
                // A selected slot with no live entity: a removal event was missed. The eviction pass
                // makes this unreachable; heal lazily (one-frame budget underfill) and flag in debug.
                debug_assert!(false, "dead slot {slot} survived in SignificanceIndex");
                sig_index.remove(entity);
            }
        }
    }
    let cmp = |a: &RankEntry, b: &RankEntry| {
        b.significance
            .partial_cmp(&a.significance)
            .unwrap_or(Ordering::Equal)
            .then_with(|| stable_key(&a.id).cmp(&stable_key(&b.id)))
    };
    ranked.sort_unstable_by(cmp);

    // Assign tiers positionally to the ranked top-K (`ranked` holds at most k entries — fewer when
    // fewer entities are live).
    assignment.clear();
    let mut high = budget.high_res_actors;
    let mut low = budget.low_res_actors;
    let mut ism = budget.instanced_meshes;
    for entry in ranked.iter() {
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
    use crate::resources::{RepresentationBudget, SignificanceIndex};
    use crate::schedules::LogiSchedule;
    use bevy_ecs::incremental::bucket_of;
    use bevy_ecs::prelude::*;
    use hashbrown::HashMap;
    use logi_schema::{LogiEntityId, RepresentationTier};

    /// A budget-system test world: the resources the system reads plus the persistent index.
    fn budget_world(budget: RepresentationBudget) -> (World, Schedule) {
        let mut world = World::new();
        world.insert_resource(budget);
        world.insert_resource(SignificanceIndex::default());
        let mut schedule = Schedule::new(LogiSchedule::RepresentationPlan);
        schedule.add_systems(allocate_representation_budget);
        (world, schedule)
    }

    fn spawn_agent(world: &mut World, index: u64, significance: f32) -> Entity {
        world
            .spawn((
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
            ))
            .id()
    }

    /// SplitMix64-finalizer score in [0,1), biased low like a real far field (test scaffolding
    /// only — mirrors the sanctioned mixer's constants).
    fn score_of(i: u32) -> f32 {
        let mut z = (i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        let u = (z >> 40) as f32 / (1u64 << 24) as f32;
        u * u
    }

    fn tiers_by_index(world: &mut World) -> HashMap<u64, RepresentationTier> {
        let mut query = world.query::<(&StableId, &Representation)>();
        query
            .iter(world)
            .map(|(id, rep)| (id.0.index, rep.desired_tier))
            .collect()
    }

    #[test]
    fn budget_allocates_tiers_top_down_by_significance() {
        let (mut world, mut schedule) = budget_world(RepresentationBudget {
            high_res_actors: 1,
            low_res_actors: 1,
            instanced_meshes: 10,
        });
        spawn_agent(&mut world, 1, 0.1);
        spawn_agent(&mut world, 2, 0.9);
        spawn_agent(&mut world, 3, 0.5);

        schedule.run(&mut world);
        let tier = tiers_by_index(&mut world);

        // Top-down: highest significance wins the scarce HighRes slot, etc.
        assert_eq!(tier[&2], RepresentationTier::HighResActor); // 0.9
        assert_eq!(tier[&3], RepresentationTier::LowResActor); //  0.5
        assert_eq!(tier[&1], RepresentationTier::InstancedStaticMesh); // 0.1
    }

    /// **The R1 acceptance contract** (ADR-0020 R1; LogiRuntime-Design §7.2 — "drive the
    /// equivalence test through the real `allocate_representation_budget` on a live World"):
    /// the index-driven selection is a valid QUANTIZED top-K — (a) the budget fills exactly,
    /// (b) no rejected entity outranks a selected one by more than one score quantum,
    /// (c) tier order WITHIN the selected set follows the exact `(significance desc, id asc)`
    /// comparator (quantization decides only boundary membership), and (d) the whole result
    /// is deterministic across identical worlds.
    #[test]
    fn budget_selection_is_quantized_top_k_on_a_live_world() {
        const N: u32 = 4_096;
        let budget = RepresentationBudget {
            high_res_actors: 16,
            low_res_actors: 64,
            instanced_meshes: 176,
        };
        let k = 256usize;

        let build = || {
            let (mut world, schedule) = budget_world(budget);
            for i in 0..N {
                spawn_agent(&mut world, u64::from(i) + 1, score_of(i));
            }
            (world, schedule)
        };

        let (mut world, mut schedule) = build();
        schedule.run(&mut world);

        let mut query = world.query::<(&StableId, &Representation)>();
        let rows: Vec<(f32, u64, RepresentationTier)> = query
            .iter(&world)
            .map(|(id, rep)| (rep.significance, id.0.index, rep.desired_tier))
            .collect();

        // (a) The budget fills exactly (enough positive-significance entities exist).
        let selected: Vec<_> = rows.iter().filter(|r| r.2 != RepresentationTier::None).collect();
        assert_eq!(selected.len(), k, "budget fills exactly");

        // (b) Quantized boundary: no rejected entity sits in a HIGHER bucket than any selected one.
        let min_selected_bucket = selected.iter().map(|r| bucket_of(r.0)).min().unwrap();
        let max_rejected_bucket = rows
            .iter()
            .filter(|r| r.2 == RepresentationTier::None)
            .map(|r| bucket_of(r.0))
            .max()
            .unwrap();
        assert!(
            max_rejected_bucket <= min_selected_bucket,
            "a rejected entity outranked a selected one beyond the quantum \
             (rejected hi bucket {max_rejected_bucket}, selected lo bucket {min_selected_bucket})"
        );

        // (c) Exact tier order within the selected set: every HighRes >= every LowRes >= every
        // ISM by significance (>= because equal scores may legitimately straddle a tier
        // boundary — the stable-id tie-break decides those).
        let extreme =
            |tier: RepresentationTier, min: bool| {
                selected.iter().filter(|r| r.2 == tier).map(|r| r.0).fold(
                    if min { f32::MAX } else { f32::MIN },
                    |a, s| if min { a.min(s) } else { a.max(s) },
                )
            };
        assert!(
            extreme(RepresentationTier::HighResActor, true) >= extreme(RepresentationTier::LowResActor, false),
            "every HighRes must (weakly) outrank every LowRes"
        );
        assert!(
            extreme(RepresentationTier::LowResActor, true) >= extreme(RepresentationTier::InstancedStaticMesh, false),
            "every LowRes must (weakly) outrank every ISM"
        );

        // (d) Determinism: an identical world produces identical tiers.
        let (mut world2, mut schedule2) = build();
        schedule2.run(&mut world2);
        let tiers1 = tiers_by_index(&mut world);
        let tiers2 = tiers_by_index(&mut world2);
        assert_eq!(tiers1, tiers2, "identical worlds tier identically");
    }

    /// The persistent index survives churn: despawns leave via `RemovedComponents` (the
    /// in-system drift debug_assert verifies occupancy == live population every run),
    /// significance rewrites re-bucket, and recycled entity indices (bevy reuses freed rows)
    /// re-enter cleanly. Fresh high-significance entities win tiers immediately.
    #[test]
    fn budget_index_tracks_churn_despawn_and_reuse() {
        const N: u32 = 512;
        let budget = RepresentationBudget {
            high_res_actors: 4,
            low_res_actors: 8,
            instanced_meshes: 20,
        };
        let k = 32usize;

        let (mut world, mut schedule) = budget_world(budget);
        for i in 0..N {
            spawn_agent(&mut world, u64::from(i) + 1, score_of(i));
        }
        schedule.run(&mut world);

        // Despawn every currently-selected entity (forces the eviction path through the
        // exact rows the index ranked highest, and frees their entity indices for reuse).
        let mut query = world.query::<(Entity, &Representation)>();
        let winners: Vec<Entity> = query
            .iter(&world)
            .filter(|(_, rep)| rep.desired_tier != RepresentationTier::None)
            .map(|(e, _)| e)
            .collect();
        assert_eq!(winners.len(), k);
        for entity in winners {
            world.despawn(entity);
        }

        // Rewrite some survivors' significance (the re-bucket path)…
        let mut query = world.query::<&mut Representation>();
        for (n, mut rep) in query.iter_mut(&mut world).enumerate() {
            if n % 10 == 0 {
                rep.significance = 0.05;
            }
        }
        // …and spawn fresh entities that should sweep the HighRes tier (some will land on
        // recycled entity indices).
        let fresh: Vec<Entity> = (0..4).map(|j| spawn_agent(&mut world, 10_000 + j, 0.99)).collect();

        // Two more runs: the first processes the removal events + new scores; the second
        // verifies idempotence (no event is double-processed, occupancy stays exact — the
        // in-system debug_assert is the drift gate).
        schedule.run(&mut world);
        schedule.run(&mut world);

        let mut query = world.query::<(Entity, &Representation)>();
        let mut selected = 0usize;
        for (entity, rep) in query.iter(&world) {
            if rep.desired_tier != RepresentationTier::None {
                selected += 1;
            }
            if fresh.contains(&entity) {
                assert_eq!(
                    rep.desired_tier,
                    RepresentationTier::HighResActor,
                    "a fresh 0.99-significance entity must win HighRes"
                );
            }
        }
        assert_eq!(selected, k, "the budget re-fills exactly after churn");
    }
}
