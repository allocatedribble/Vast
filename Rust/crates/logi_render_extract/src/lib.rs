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
use std::collections::HashMap;

use logi_schema::{LogiAnimationParams, LogiAssetHandle, LogiEntityId, RepresentationTier};
use logi_sim::components::{AnimationDrive, Renderable, Representation, StableId, Transform3d};
use logi_sim::resources::{SimClock, UnrealFrameInput};

/// One instanced-static-mesh instance delta. `stable_instance_slot` is assigned
/// and retained by Unreal (Rust sends `u32::MAX` to mean "assign/keep").
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct LogiInstanceUpdate {
	pub entity: LogiEntityId,
	pub stable_instance_slot: u32,
	/// Row-major 3x4 affine (cols 0-2 = rotation*scale, col 3 = translation).
	pub transform_3x4: [f32; 12],
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
	pub world_origin_cm: [f64; 3],
	pub instances: Vec<LogiInstanceUpdate>,
	pub batch_metas: Vec<BatchMeta>,
	pub actor_updates: Vec<LogiActorUpdate>,
	pub despawns: Vec<LogiEntityId>,
}

/// Build a row-major 3x4 affine (f32) from a transform, REBASED into the local
/// frame at `origin_cm` (planetary precision, P0 #6). Columns 0-2 are the rotation
/// matrix with per-axis scale; column 3 is `(world - origin)` narrowed to f32 --
/// so f32 keeps centimetre precision near the origin even at f64 planetary world
/// scale. Unreal adds `origin_cm` back when placing content.
pub fn transform_to_3x4(t: &Transform3d, origin_cm: [f64; 3]) -> [f32; 12] {
	let [x, y, z, w] = t.rotation_xyzw;
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
		r00 * sx, r01 * sy, r02 * sz, tx,
		r10 * sx, r11 * sy, r12 * sz, ty,
		r20 * sx, r21 * sy, r22 * sz, tz,
	]
}

/// Change-detected extraction (§8). Only entities whose render-relevant
/// components changed are emitted; the result is a delta the retained Unreal
/// side applies.
pub fn extract_render_snapshot(
	clock: Res<SimClock>,
	frame: Res<UnrealFrameInput>,
	mut snap: ResMut<RenderSnapshotResource>,
	query: Query<
		(&StableId, &Transform3d, &Renderable, &Representation, Option<&AnimationDrive>),
		Or<(Changed<Transform3d>, Changed<Renderable>, Changed<Representation>, Changed<AnimationDrive>)>,
	>,
) {
	let snap = &mut *snap;
	snap.snapshot_id = snap.snapshot_id.wrapping_add(1);
	snap.sim_tick = clock.tick;
	// Local-frame origin for f64->f32 rebasing (P0 #6): the primary camera position,
	// so content near the view keeps centimetre precision. Falls back to planet origin.
	let origin = frame.cameras.first().map(|c| c.position).unwrap_or([0.0; 3]);
	snap.world_origin_cm = origin;
	snap.instances.clear();
	snap.batch_metas.clear();
	snap.actor_updates.clear();
	snap.despawns.clear();

	// Group ISM instances by (mesh, material, lod) handle.
	let mut groups: HashMap<(LogiAssetHandle, LogiAssetHandle, u32), Vec<LogiInstanceUpdate>> = HashMap::new();

	for (id, transform, renderable, rep, anim) in query.iter() {
		match rep.current_tier {
			RepresentationTier::InstancedStaticMesh => {
				let lod = 0u32; // TODO: LOD selection from significance/screen size.
				groups.entry((renderable.mesh, renderable.material, lod)).or_default().push(LogiInstanceUpdate {
					entity: id.0,
					stable_instance_slot: u32::MAX, // Unreal assigns/keeps the slot.
					transform_3x4: transform_to_3x4(transform, origin),
					custom_data: [0.0; 8],
					flags: renderable.flags.0,
				});
			}
			RepresentationTier::LowResActor | RepresentationTier::HighResActor => {
				let animation = anim
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
					anim_blueprint: anim.map(|a| a.anim_graph).unwrap_or(LogiAssetHandle::NONE),
					transform_3x4: transform_to_3x4(transform, origin),
					significance: rep.significance,
					animation,
				});
			}
			RepresentationTier::None => {} // Not rendered.
		}
	}

	// Flatten groups into the contiguous instance buffer + batch metadata.
	// Deterministic batch order: sort by handle key.
	let mut keys: Vec<(LogiAssetHandle, LogiAssetHandle, u32)> = groups.keys().copied().collect();
	keys.sort_unstable();
	for key in keys {
		let group = &groups[&key];
		let offset = snap.instances.len() as u32;
		snap.instances.extend_from_slice(group);
		snap.batch_metas.push(BatchMeta {
			mesh: key.0,
			material: key.1,
			lod: key.2,
			offset,
			count: group.len() as u32,
		});
	}
}

/// Marshal the pointer-bearing C batch array from the resource (called at the
/// FFI boundary). `out` is owned by the runtime (not a Resource), so raw
/// pointers into `snapshot.instances` are allowed. Valid until the next tick.
pub fn build_ffi_batches(snapshot: &RenderSnapshotResource, out: &mut Vec<LogiStaticBatch>) {
	out.clear();
	out.reserve(snapshot.batch_metas.len());
	let base = snapshot.instances.as_ptr();
	for meta in &snapshot.batch_metas {
		out.push(LogiStaticBatch {
			mesh: meta.mesh,
			material: meta.material,
			lod: meta.lod,
			instance_count: meta.count,
			// SAFETY: offset is within `instances`, which is not mutated until the next tick.
			instances: unsafe { base.add(meta.offset as usize) },
		});
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use logi_schema::{asset_kind, LogiEntityId, RenderFlags};

	fn ism(world: &mut World, index: u64, mesh: u64, material: u64) {
		world.spawn((
			StableId(LogiEntityId { index, generation: 1, shard: 0, kind: 1 }),
			Transform3d::default(),
			Renderable {
				mesh: LogiAssetHandle::new(mesh, asset_kind::STATIC_MESH),
				material: LogiAssetHandle::new(material, asset_kind::MATERIAL),
				skeleton: LogiAssetHandle::NONE,
				flags: RenderFlags::NONE,
			},
			Representation { current_tier: RepresentationTier::InstancedStaticMesh, ..Default::default() },
		));
	}

	fn make_schedule() -> Schedule {
		let mut s = Schedule::new(logi_sim::schedules::LogiSchedule::RenderExtract);
		s.add_systems(extract_render_snapshot);
		s
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
			StableId(LogiEntityId { index: 4, generation: 1, shard: 0, kind: 1 }),
			Transform3d::default(),
			Renderable {
				mesh: LogiAssetHandle::NONE,
				material: LogiAssetHandle::NONE,
				skeleton: LogiAssetHandle::new(300, asset_kind::SKELETAL_MESH),
				flags: RenderFlags::NONE,
			},
			Representation { current_tier: RepresentationTier::HighResActor, ..Default::default() },
		));

		make_schedule().run(&mut world);
		let snap = world.resource::<RenderSnapshotResource>();
		assert_eq!(snap.batch_metas.len(), 2);
		assert_eq!(snap.instances.len(), 3);
		assert_eq!(snap.actor_updates.len(), 1);
		let bucket = snap.batch_metas.iter().find(|m| m.mesh.stable_hash == 100).expect("bucket");
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
		schedule.run(&mut world); // first run: newly-added -> extracted
		assert_eq!(world.resource::<RenderSnapshotResource>().instances.len(), 1);

		schedule.run(&mut world); // nothing changed -> empty delta
		assert_eq!(world.resource::<RenderSnapshotResource>().instances.len(), 0);
	}
}
