//! `logi_render_extract` — the batched, retained, mostly-SoA rendering extraction
//! path (§8). A change-detected system extracts only entities whose render-
//! relevant components changed, splits them by representation tier into
//! instanced-mesh batches (grouped by mesh/material/LOD) vs. per-actor updates,
//! and writes a `RenderSnapshotResource`. Unreal consumes the batched snapshot
//! and applies deltas to retained ISM/actors -- never per-entity Rust queries.
//!
//! The resource stays `Send + Sync` (no raw pointers); the pointer-bearing
//! C `LogiStaticBatch` array is marshalled at the FFI boundary (see
//! [`build_ffi_batches`]), owned by the non-resource runtime.

use bevy_ecs::prelude::*;
use std::collections::HashSet;

use logi_schema::{LogiAnimationParams, LogiAssetHandle, LogiEntityId, RepresentationTier};
use logi_sim::components::{AnimationDrive, Renderable, Representation, StableId, Transform3d};
use logi_sim::resources::{SimClock, UnrealFrameInput};

/// Agent-render rebase origin quantum, in world centimetres (256 m). The primary
/// camera position is snapped to this grid before it becomes `world_origin_cm`
/// (audit H3): retained instances only re-bake when the camera crosses a cell,
/// instead of drifting every frame as a continuous camera position would force.
/// Deliberately FINE (256 m) — small per-agent transforms need tight f32 precision
/// near the camera. (Contrast the much coarser TERRAIN rebase grid,
/// the retired render-scene crate REBASE_SNAP = 2048 m: big terrain tiles tolerate a far origin,
/// so a wide grid means rarer rebuilds. Different precision needs, different grids.)
pub const ORIGIN_GRID_CM: f64 = 25_600.0;

/// Snap a world-cm position to the [`ORIGIN_GRID_CM`] lattice, per axis.
#[inline]
fn snap_origin(pos: [f64; 3]) -> [f64; 3] {
    [
        (pos[0] / ORIGIN_GRID_CM).round() * ORIGIN_GRID_CM,
        (pos[1] / ORIGIN_GRID_CM).round() * ORIGIN_GRID_CM,
        (pos[2] / ORIGIN_GRID_CM).round() * ORIGIN_GRID_CM,
    ]
}

/// One instanced-static-mesh instance delta. `stable_instance_slot` is assigned
/// and retained by Unreal (Rust sends `u32::MAX` to mean "assign/keep").
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct LogiInstanceUpdate {
    pub entity: LogiEntityId,
    pub stable_instance_slot: u32,
    /// Row-major 3x4 affine (cols 0-2 = rotation*scale, col 3 = translation).
    pub transform_3x4: [f32; 12],
    /// Per-instance custom data uploaded to the ISM (e.g. material params). Part of
    /// the ABI struct, but the extract currently emits all-zeros: no system writes
    /// per-instance custom data yet. Reserved so adding one needs no ABI bump.
    pub custom_data: [f32; 8],
    pub flags: u32,
}

/// A batch of instances sharing mesh/material/LOD. `instances` points into the
/// snapshot's instance buffer (valid until the next tick). Built at the FFI
/// boundary -- NOT stored in the (Send+Sync) resource.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct LogiStaticBatch {
    pub mesh: LogiAssetHandle,
    pub material: LogiAssetHandle,
    pub lod: u32,
    pub instance_count: u32,
    pub instances: *const LogiInstanceUpdate,
}

/// A per-actor update (low/high-res actor tiers). Skeletal mesh + anim + transform.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct LogiActorUpdate {
    pub entity: LogiEntityId,
    pub actor_class: LogiAssetHandle,
    pub skeletal_mesh: LogiAssetHandle,
    pub anim_blueprint: LogiAssetHandle,
    pub transform_3x4: [f32; 12],
    pub significance: f32,
    pub animation: LogiAnimationParams,
}

/// Pointer-free batch metadata stored in the resource (keeps it `Send + Sync`).
/// `offset`/`count` index into `RenderSnapshotResource::instances`.
#[derive(Clone, Copy, Debug)]
pub struct BatchMeta {
    pub mesh: LogiAssetHandle,
    pub material: LogiAssetHandle,
    pub lod: u32,
    pub offset: u32,
    pub count: u32,
}

/// The owned, reused snapshot buffers. Lives in the World as a Resource and is
/// rewritten by [`extract_render_snapshot`] each frame. `Send + Sync` (no pointers).
#[derive(Resource, Default)]
pub struct RenderSnapshotResource {
    pub snapshot_id: u64,
    pub sim_tick: u64,
    /// Local-frame origin (world centimetres, f64). Every `transform_3x4`
    /// translation in this snapshot is relative to it; Unreal adds it back.
    /// Snapped to the [`ORIGIN_GRID_CM`] lattice (audit H3).
    pub world_origin_cm: [f64; 3],
    pub instances: Vec<LogiInstanceUpdate>,
    pub batch_metas: Vec<BatchMeta>,
    pub actor_updates: Vec<LogiActorUpdate>,
    pub despawns: Vec<LogiEntityId>,

    // --- Persistent cross-frame extract state (NOT part of the published delta) ---
    /// The set of entities currently mirrored on the Unreal side as a *rendered*
    /// tier (ISM / Low / High actor — never `None`), as of the last COMMITTED
    /// (successfully published) snapshot. Diffing this against the freshly computed
    /// set yields retirements for BOTH demote-to-None and despawn (audit H1).
    rendered: HashSet<LogiEntityId>,
    /// The rendered set computed by the most recent extract, awaiting commit. The
    /// runtime promotes this into `rendered` only when the snapshot actually
    /// publishes (audit H2: a dropped frame must re-detect the same retirements).
    pending_rendered: HashSet<LogiEntityId>,
    /// The snapped origin of the last COMMITTED snapshot, used to detect when the
    /// camera crosses an [`ORIGIN_GRID_CM`] cell and a full re-emit is required.
    committed_origin_cm: [f64; 3],
    /// Whether `committed_origin_cm` has been seeded yet (the very first extract
    /// has no prior origin and must do a full emit at whatever origin it picks).
    origin_seeded: bool,
    /// Set by the runtime when a ring publish DROPS (audit H2): the un-published
    /// delta would otherwise be lost, because Bevy's per-system change detection has
    /// already advanced past it (the extract saw and consumed the `Changed` flags on
    /// the dropped frame). The next extract therefore forces a FULL re-emit of every
    /// rendered entity — recovering whatever the dropped frame would have sent —
    /// rather than relying on `Changed` to fire again. Cleared on the next successful
    /// commit; persists across consecutive drops.
    force_full_reextract: bool,
}

impl RenderSnapshotResource {
    /// Promote the most recent extract's bookkeeping into the committed state.
    /// Called by the runtime ONLY after a successful ring publish (audit H1 + H2):
    /// on a dropped frame this is skipped so the same retirements / origin-rebase
    /// are recomputed and retried on the next published frame.
    pub fn commit(&mut self) {
        // Swap rather than clone: `rendered` takes the freshly computed set in O(1) and the old set lands
        // in `pending_rendered`, which the next extract clears before refilling. (audit: a `clone_from`
        // re-hashed/copied up to ~100k ids on every published frame.)
        std::mem::swap(&mut self.rendered, &mut self.pending_rendered);
        self.committed_origin_cm = self.world_origin_cm;
        self.origin_seeded = true;
        self.force_full_reextract = false;
    }

    /// Flag that the last publish dropped, so the next extract must do a full
    /// re-emit to recover the lost delta (audit H2). Called by the runtime when
    /// `SnapshotRing::publish` returns false.
    pub fn mark_publish_dropped(&mut self) {
        self.force_full_reextract = true;
    }
}

/// Build a row-major 3x4 affine (f32) from a transform, REBASED into the local
/// frame at `origin_cm` (planetary precision, P0 #6). Columns 0-2 are the rotation
/// matrix with per-axis scale; column 3 is `(world - origin)` narrowed to f32 --
/// so f32 keeps centimetre precision near the origin even at f64 planetary world
/// scale. Unreal adds `origin_cm` back when placing content.
pub fn transform_to_3x4(t: &Transform3d, origin_cm: [f64; 3]) -> [f32; 12] {
    // Defensively normalize the quaternion (L5): the rotation->matrix expansion
    // below assumes a unit quaternion, so a denormalized one would silently bake a
    // shear/scale into the basis. Authoritative transforms should already be unit
    // (debug_assert catches regressions); the renormalize keeps release output sane.
    let [mut x, mut y, mut z, mut w] = t.rotation_xyzw;
    let q_len_sq = x * x + y * y + z * z + w * w;
    debug_assert!(
        (q_len_sq - 1.0).abs() < 1.0e-3,
        "Transform3d.rotation_xyzw must be unit, got |q|^2 = {q_len_sq}"
    );
    if q_len_sq > 0.0 {
        let inv = 1.0 / q_len_sq.sqrt();
        x *= inv;
        y *= inv;
        z *= inv;
        w *= inv;
    } else {
        // Degenerate (all-zero) quaternion: fall back to identity rotation.
        (x, y, z, w) = (0.0, 0.0, 0.0, 1.0);
    }
    let (sx, sy, sz) = (t.scale[0], t.scale[1], t.scale[2]);

    let r00 = 1.0 - 2.0 * (y * y + z * z);
    let r01 = 2.0 * (x * y - w * z);
    let r02 = 2.0 * (x * z + w * y);
    let r10 = 2.0 * (x * y + w * z);
    let r11 = 1.0 - 2.0 * (x * x + z * z);
    let r12 = 2.0 * (y * z - w * x);
    let r20 = 2.0 * (x * z - w * y);
    let r21 = 2.0 * (y * z + w * x);
    let r22 = 1.0 - 2.0 * (x * x + y * y);

    let tx = (t.translation_cm[0] - origin_cm[0]) as f32;
    let ty = (t.translation_cm[1] - origin_cm[1]) as f32;
    let tz = (t.translation_cm[2] - origin_cm[2]) as f32;

    [
        r00 * sx,
        r01 * sy,
        r02 * sz,
        tx,
        r10 * sx,
        r11 * sy,
        r12 * sz,
        ty,
        r20 * sx,
        r21 * sy,
        r22 * sz,
        tz,
    ]
}

/// Change-detected extraction (§8). Normally only entities whose render-relevant
/// components changed are emitted; the result is a delta the retained Unreal side
/// applies.
///
/// Two cross-frame mechanisms ride on top of the delta:
///
/// * **Retirement (audit H1).** The full rendered set is recomputed every frame
///   (iterating ALL rendered entities, not just changed ones). Anything that was
///   in the last committed set but is no longer rendered — whether it despawned or
///   demoted to `None` — is pushed into `despawns` exactly once so Unreal tears
///   down its shell. The committed set only advances on a successful publish, so a
///   dropped frame re-detects the same retirements (audit H2).
/// * **Origin rebase (audit H3).** The primary camera position is snapped to the
///   [`ORIGIN_GRID_CM`] lattice. While the snapped origin is unchanged we emit the
///   normal `Changed<>` delta; when the camera crosses a cell the origin moves and
///   we FORCE a full re-emit of every rendered instance re-baked to the new origin
///   (otherwise retained instances would drift). Like the rendered set, the origin
///   only commits on publish.
///
/// We iterate an UNFILTERED query and consult per-entity `Ref::is_changed()`
/// ourselves (rather than an `Or<Changed<..>>` query filter) because the full
/// rendered set is needed every frame regardless of what changed.
// The render-component tuple is inherently wide; a `type` alias would hurt
// readability more than it helps (idiomatic for Bevy systems).
/// Scratch element used to group ISM instances before segmentation: (mesh, material, lod, instance).
type GroupEntry = (LogiAssetHandle, LogiAssetHandle, u32, LogiInstanceUpdate);

#[allow(clippy::type_complexity)]
pub fn extract_render_snapshot(
    clock: Res<SimClock>,
    frame: Res<UnrealFrameInput>,
    mut snap: ResMut<RenderSnapshotResource>,
    // Reused across frames so ISM grouping allocates nothing on the hot path (audit).
    mut group_scratch: Local<Vec<GroupEntry>>,
    query: Query<(
        &StableId,
        Ref<Transform3d>,
        Ref<Renderable>,
        Ref<Representation>,
        Option<Ref<AnimationDrive>>,
    )>,
) {
    let snap = &mut *snap;
    snap.snapshot_id = snap.snapshot_id.wrapping_add(1);
    snap.sim_tick = clock.tick;
    // Local-frame origin for f64->f32 rebasing (P0 #6): the primary camera position
    // SNAPPED to the origin grid (audit H3), so content near the view keeps
    // centimetre precision without drifting every frame. Falls back to planet origin.
    let origin = snap_origin(frame.cameras.first().map(|c| c.position).unwrap_or([0.0; 3]));
    snap.world_origin_cm = origin;
    // Force a FULL re-emit (every rendered entity, no Changed filter) when either:
    //   * the snapped origin moved / this is the first extract (audit H3: re-bake all
    //     instances to the new origin so retained content doesn't drift), or
    //   * the previous publish dropped (audit H2: recover the lost delta, since
    //     per-system change detection won't re-fire the Changed flags next frame).
    let force_full = !snap.origin_seeded || origin != snap.committed_origin_cm || snap.force_full_reextract;

    snap.instances.clear();
    snap.batch_metas.clear();
    snap.actor_updates.clear();
    snap.despawns.clear();
    snap.pending_rendered.clear();
    group_scratch.clear();

    for (id, transform, renderable, rep, anim) in query.iter() {
        // Is this entity rendered at all this frame? (Drives the retirement diff.)
        let rendered = !matches!(rep.current_tier, RepresentationTier::None);
        if rendered {
            snap.pending_rendered.insert(id.0);
        }

        // Emit on a forced full re-emit (origin moved / first frame / post-drop
        // recovery) OR when any render-relevant component changed this step (§8 delta).
        let changed = transform.is_changed()
            || renderable.is_changed()
            || rep.is_changed()
            || anim.as_ref().map(|a| a.is_changed()).unwrap_or(false);
        if !force_full && !changed {
            continue;
        }

        match rep.current_tier {
            RepresentationTier::InstancedStaticMesh => {
                // M2: only emit as an ISM instance when the mesh handle is valid.
                // During a transient hysteresis step an actor can momentarily sit at
                // the ISM tier with a NONE mesh; emitting it would create a phantom
                // instance, so skip it (it stays in the rendered set and is retired
                // normally once it settles).
                if !renderable.mesh.is_valid() {
                    continue;
                }
                let lod = 0u32; // TODO: LOD selection from significance/screen size.
                group_scratch.push((
                    renderable.mesh,
                    renderable.material,
                    lod,
                    LogiInstanceUpdate {
                        entity: id.0,
                        stable_instance_slot: u32::MAX, // Unreal assigns/keeps the slot.
                        transform_3x4: transform_to_3x4(&transform, origin),
                        custom_data: [0.0; 8], // reserved (see field doc): no per-instance data source yet.
                        flags: renderable.flags.0,
                    },
                ));
            }
            RepresentationTier::LowResActor | RepresentationTier::HighResActor => {
                let animation = anim
                    .as_ref()
                    .map(|a| LogiAnimationParams {
                        locomotion_state: a.locomotion_state,
                        speed_cm_s: a.speed_cm_s,
                        turn_rate: a.turn_rate,
                        aim_yaw_pitch: a.aim_yaw_pitch,
                    })
                    .unwrap_or_default();
                snap.actor_updates.push(LogiActorUpdate {
                    entity: id.0,
                    actor_class: LogiAssetHandle::NONE, // resolved Unreal-side by tier + renderable.
                    skeletal_mesh: renderable.skeleton,
                    anim_blueprint: anim.as_ref().map(|a| a.anim_graph).unwrap_or(LogiAssetHandle::NONE),
                    transform_3x4: transform_to_3x4(&transform, origin),
                    significance: rep.significance,
                    animation,
                });
            }
            RepresentationTier::None => {} // Not rendered.
        }
    }

    // Retire entities (audit H1): everything in the last committed rendered set that
    // is no longer rendered (demoted to None OR despawned) is pushed exactly once.
    // Deterministic order: sort by id. The committed set advances only on publish
    // (see `RenderSnapshotResource::commit`), so this is recomputed on a dropped
    // frame and the same retirements are retried next publish.
    // Extend the already-cleared `despawns` buffer in place rather than `.collect()`-ing a fresh Vec and
    // reassigning — the reassign drops the retained capacity and reallocates every extract, the one
    // per-frame heap allocation on this otherwise allocation-free path (§9). Destructure to borrow the
    // three sets as disjoint fields.
    {
        let RenderSnapshotResource {
            rendered,
            pending_rendered,
            despawns,
            ..
        } = &mut *snap;
        despawns.extend(rendered.difference(&*pending_rendered).copied());
        despawns.sort_unstable_by_key(|id| (id.index, id.generation, id.shard, id.kind));
    }

    // Flatten the grouped instances into the contiguous instance buffer + pointer-free batch metadata.
    // Sort by batch key (then entity id) for a deterministic, replay-stable order, then emit one BatchMeta
    // per contiguous run of equal keys. (audit: replaces a per-frame HashMap + one heap Vec per bucket with
    // a single reused scratch buffer + a sort — no per-bucket allocation, no HashMap iteration.)
    group_scratch.sort_unstable_by(|a, b| {
        (a.0, a.1, a.2).cmp(&(b.0, b.1, b.2)).then_with(|| {
            let (x, y) = (a.3.entity, b.3.entity);
            (x.index, x.generation, x.shard, x.kind).cmp(&(y.index, y.generation, y.shard, y.kind))
        })
    });
    let mut i = 0;
    while i < group_scratch.len() {
        let (mesh, material, lod, _) = group_scratch[i];
        let offset = snap.instances.len() as u32;
        let mut count = 0u32;
        while i < group_scratch.len()
            && (group_scratch[i].0, group_scratch[i].1, group_scratch[i].2) == (mesh, material, lod)
        {
            snap.instances.push(group_scratch[i].3);
            i += 1;
            count += 1;
        }
        snap.batch_metas.push(BatchMeta {
            mesh,
            material,
            lod,
            offset,
            count,
        });
    }
}

/// Marshal the pointer-bearing C batch array (the SINGLE home for this `unsafe`
/// pointer-baking; the runtime's `SnapshotRing::publish` calls this instead of
/// re-implementing it — audit M24). Each emitted `LogiStaticBatch.instances` points
/// at `instances_base + meta.offset`; `out` must be owned non-resource storage (raw
/// pointers may not live in a `Send + Sync` resource).
///
/// # Safety
/// `instances_base` must point to the start of a contiguous instance buffer that
/// outlives `out`, and every `meta.offset + meta.count` in `metas` must be in
/// bounds of that buffer. The produced pointers are valid only while that buffer
/// is neither freed nor reallocated.
pub unsafe fn build_ffi_batches(
    instances_base: *const LogiInstanceUpdate,
    metas: &[BatchMeta],
    out: &mut Vec<LogiStaticBatch>,
) {
    out.clear();
    out.reserve(metas.len());
    for meta in metas {
        out.push(LogiStaticBatch {
            mesh: meta.mesh,
            material: meta.material,
            lod: meta.lod,
            instance_count: meta.count,
            // SAFETY: caller guarantees `offset` is within the buffer at `instances_base`,
            // which outlives `out` and is not reallocated while these pointers are used.
            instances: unsafe { instances_base.add(meta.offset as usize) },
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use logi_schema::{asset_kind, LogiCameraView, LogiEntityId, RenderFlags};

    fn ism(world: &mut World, index: u64, mesh: u64, material: u64) {
        world.spawn((
            StableId(LogiEntityId {
                index,
                generation: 1,
                shard: 0,
                kind: 1,
            }),
            Transform3d::default(),
            Renderable {
                mesh: LogiAssetHandle::new(mesh, asset_kind::STATIC_MESH),
                material: LogiAssetHandle::new(material, asset_kind::MATERIAL),
                skeleton: LogiAssetHandle::NONE,
                flags: RenderFlags::NONE,
            },
            Representation {
                current_tier: RepresentationTier::InstancedStaticMesh,
                ..Default::default()
            },
        ));
    }

    fn make_schedule() -> Schedule {
        let mut s = Schedule::new(logi_sim::schedules::LogiSchedule::RenderExtract);
        s.add_systems(extract_render_snapshot);
        s
    }

    /// Run the extract once, then commit its bookkeeping — modelling the runtime's
    /// "extract then successfully publish" path (the runtime calls
    /// `RenderSnapshotResource::commit` after a successful ring publish). Standalone
    /// extract tests must do this so the rendered-set / origin baselines advance.
    fn run_and_commit(schedule: &mut Schedule, world: &mut World) {
        schedule.run(world);
        world.resource_mut::<RenderSnapshotResource>().commit();
    }

    #[test]
    fn ism_entities_group_into_batches_by_mesh_material() {
        let mut world = World::new();
        world.insert_resource(SimClock::default());
        world.insert_resource(UnrealFrameInput::default());
        world.insert_resource(RenderSnapshotResource::default());
        ism(&mut world, 1, 100, 200);
        ism(&mut world, 2, 100, 200); // same bucket
        ism(&mut world, 3, 101, 200); // different mesh -> different bucket
        world.spawn((
            StableId(LogiEntityId {
                index: 4,
                generation: 1,
                shard: 0,
                kind: 1,
            }),
            Transform3d::default(),
            Renderable {
                mesh: LogiAssetHandle::NONE,
                material: LogiAssetHandle::NONE,
                skeleton: LogiAssetHandle::new(300, asset_kind::SKELETAL_MESH),
                flags: RenderFlags::NONE,
            },
            Representation {
                current_tier: RepresentationTier::HighResActor,
                ..Default::default()
            },
        ));

        make_schedule().run(&mut world);
        let snap = world.resource::<RenderSnapshotResource>();
        assert_eq!(snap.batch_metas.len(), 2);
        assert_eq!(snap.instances.len(), 3);
        assert_eq!(snap.actor_updates.len(), 1);
        let bucket = snap
            .batch_metas
            .iter()
            .find(|m| m.mesh.stable_hash == 100)
            .expect("bucket");
        assert_eq!(bucket.count, 2);
    }

    #[test]
    fn change_detection_emits_delta_only() {
        let mut world = World::new();
        world.insert_resource(SimClock::default());
        world.insert_resource(UnrealFrameInput::default());
        world.insert_resource(RenderSnapshotResource::default());
        ism(&mut world, 1, 100, 200);

        let mut schedule = make_schedule();
        run_and_commit(&mut schedule, &mut world); // first run: newly-added -> extracted
        assert_eq!(world.resource::<RenderSnapshotResource>().instances.len(), 1);

        run_and_commit(&mut schedule, &mut world); // nothing changed -> empty delta
        assert_eq!(world.resource::<RenderSnapshotResource>().instances.len(), 0);
    }

    /// H1: an entity demoted from a rendered tier to `None` is retired exactly once
    /// (in `despawns`) and never again on subsequent stable frames. The same code
    /// path catches an outright despawn (an id that vanishes from the query).
    #[test]
    fn demotion_to_none_retires_exactly_once() {
        let mut world = World::new();
        world.insert_resource(SimClock::default());
        world.insert_resource(UnrealFrameInput::default());
        world.insert_resource(RenderSnapshotResource::default());
        let e = world
            .spawn((
                StableId(LogiEntityId {
                    index: 7,
                    generation: 1,
                    shard: 0,
                    kind: 1,
                }),
                Transform3d::default(),
                Renderable {
                    mesh: LogiAssetHandle::new(100, asset_kind::STATIC_MESH),
                    material: LogiAssetHandle::new(200, asset_kind::MATERIAL),
                    skeleton: LogiAssetHandle::NONE,
                    flags: RenderFlags::NONE,
                },
                Representation {
                    current_tier: RepresentationTier::InstancedStaticMesh,
                    ..Default::default()
                },
            ))
            .id();

        let mut schedule = make_schedule();
        // Frame 1: rendered, so it joins the committed set; nothing retired yet.
        run_and_commit(&mut schedule, &mut world);
        assert!(world.resource::<RenderSnapshotResource>().despawns.is_empty());

        // Demote to None -> no longer rendered.
        world.get_mut::<Representation>(e).unwrap().current_tier = RepresentationTier::None;

        // Frame 2: retirement emitted exactly once.
        run_and_commit(&mut schedule, &mut world);
        {
            let snap = world.resource::<RenderSnapshotResource>();
            assert_eq!(snap.despawns.len(), 1);
            assert_eq!(snap.despawns[0].index, 7);
            assert_eq!(snap.instances.len(), 0); // demoted entity is not emitted as an instance
        }

        // Frame 3: stable -> NOT retired again (the commit dropped it from the set).
        run_and_commit(&mut schedule, &mut world);
        assert!(world.resource::<RenderSnapshotResource>().despawns.is_empty());
    }

    /// H3: when the snapped origin changes (camera crosses an ORIGIN_GRID_CM cell)
    /// every rendered entity is re-emitted (full re-bake), even if nothing about the
    /// entity changed. While the snapped origin holds, an unchanged entity is not
    /// re-emitted.
    #[test]
    fn origin_cell_crossing_forces_full_reemit() {
        let mut world = World::new();
        world.insert_resource(SimClock::default());
        world.insert_resource(UnrealFrameInput::default());
        world.insert_resource(RenderSnapshotResource::default());
        ism(&mut world, 1, 100, 200);

        let mut schedule = make_schedule();
        // Frame 1: first extract always re-emits; camera at origin -> snapped [0,0,0].
        run_and_commit(&mut schedule, &mut world);
        assert_eq!(world.resource::<RenderSnapshotResource>().instances.len(), 1);
        assert_eq!(world.resource::<RenderSnapshotResource>().world_origin_cm, [0.0; 3]);

        // Frame 2: nudge the camera but stay inside the same cell (< half a grid step).
        world.resource_mut::<UnrealFrameInput>().cameras = vec![LogiCameraView {
            position: [ORIGIN_GRID_CM * 0.25, 0.0, 0.0],
            ..Default::default()
        }];
        run_and_commit(&mut schedule, &mut world);
        assert_eq!(world.resource::<RenderSnapshotResource>().world_origin_cm, [0.0; 3]); // same cell
        assert_eq!(world.resource::<RenderSnapshotResource>().instances.len(), 0); // unchanged -> no re-emit

        // Frame 3: cross into the next cell -> origin moves, full re-emit of all rendered.
        world.resource_mut::<UnrealFrameInput>().cameras = vec![LogiCameraView {
            position: [ORIGIN_GRID_CM * 0.75, 0.0, 0.0],
            ..Default::default()
        }];
        run_and_commit(&mut schedule, &mut world);
        assert_eq!(
            world.resource::<RenderSnapshotResource>().world_origin_cm,
            [ORIGIN_GRID_CM, 0.0, 0.0]
        );
        assert_eq!(world.resource::<RenderSnapshotResource>().instances.len(), 1);
        // re-baked despite no entity change
    }

    /// M2: an entity sitting at the ISM tier with an invalid (NONE) mesh — e.g. a
    /// transient hysteresis step — must NOT be emitted as a phantom instance.
    #[test]
    fn ism_tier_with_invalid_mesh_is_not_emitted() {
        let mut world = World::new();
        world.insert_resource(SimClock::default());
        world.insert_resource(UnrealFrameInput::default());
        world.insert_resource(RenderSnapshotResource::default());
        world.spawn((
            StableId(LogiEntityId {
                index: 9,
                generation: 1,
                shard: 0,
                kind: 1,
            }),
            Transform3d::default(),
            Renderable {
                mesh: LogiAssetHandle::NONE, // invalid mesh at ISM tier
                material: LogiAssetHandle::NONE,
                skeleton: LogiAssetHandle::NONE,
                flags: RenderFlags::NONE,
            },
            Representation {
                current_tier: RepresentationTier::InstancedStaticMesh,
                ..Default::default()
            },
        ));

        make_schedule().run(&mut world);
        let snap = world.resource::<RenderSnapshotResource>();
        assert_eq!(snap.instances.len(), 0);
        assert_eq!(snap.batch_metas.len(), 0);
    }

    /// L5: a slightly denormalized rotation quaternion is renormalized so the basis
    /// stays orthonormal (no scale/shear baked into the 3x4). The drift is kept
    /// inside the `debug_assert` tolerance so the defensive assert (which guards
    /// against gross corruption) does not fire here.
    #[test]
    fn transform_renormalizes_quaternion() {
        // 90 deg about +Z, nudged 0.02% off unit length. The basis must come back to
        // the exact 90 deg-about-Z rotation, NOT a 1.0002-scaled version of it.
        let s = std::f32::consts::FRAC_1_SQRT_2; // sin(45) = cos(45)
        let k = 1.000_2_f32; // |q|^2 ~= 1.0004, within the 1e-3 debug_assert tolerance
        let t = Transform3d {
            rotation_xyzw: [0.0, 0.0, s * k, s * k],
            ..Default::default()
        };
        let m = transform_to_3x4(&t, [0.0; 3]);
        // Expected rotation columns for 90 deg about +Z: [[0,-1,0],[1,0,0],[0,0,1]].
        assert!(m[0].abs() < 1e-3, "r00 ~= 0, got {}", m[0]);
        assert!((m[1] + 1.0).abs() < 1e-3, "r01 ~= -1, got {}", m[1]);
        assert!((m[4] - 1.0).abs() < 1e-3, "r10 ~= 1, got {}", m[4]);
        assert!(m[5].abs() < 1e-3, "r11 ~= 0, got {}", m[5]);
        assert!((m[10] - 1.0).abs() < 1e-3, "r22 ~= 1, got {}", m[10]);
    }
}
