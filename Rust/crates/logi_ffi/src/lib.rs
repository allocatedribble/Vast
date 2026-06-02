//! `logi_ffi` — the narrow C ABI between Unreal (caller) and the Rust simulation.
//!
//! Contract (ADR-0005 / AGENTS.md): plain data only, NO Unreal types cross, and
//! Rust never calls Unreal. Opaque handles; explicit ownership; every exported
//! function validates the ABI version + null pointers; the hot path is batched
//! (one snapshot, never per-entity calls). Panics never cross the boundary:
//! release builds use `panic = "abort"`; non-abort builds are caught here.

use core::slice;
use std::panic::{catch_unwind, AssertUnwindSafe};

use logi_render_extract::{LogiActorUpdate, LogiStaticBatch};
use logi_runtime::{LogiWorld, WorldConfig};
use logi_schema::{LogiCameraView, LogiEntityId, LogiPlayerIntent, LogiSpawnCommand};

/// ABI version. Bump on any breaking change to this header. Callers pass their
/// compiled-against version in `LogiInitDesc::abi_version`; mismatches are rejected.
pub const LOGI_ABI_VERSION: u32 = 3;

// Status / error codes (returned in `status` fields and `LogiError::code`).
pub const LOGI_OK: u32 = 0;
pub const LOGI_ERR_NULL: u32 = 1;
pub const LOGI_ERR_ABI: u32 = 2;
pub const LOGI_ERR_PANIC: u32 = 3;
pub const LOGI_ERR_SNAPSHOT: u32 = 4;

/// `LogiInitDesc::flags` bit: run the bounded-parallel scheduler. Mirror of
/// `logi_schema::LOGI_FLAG_BOUNDED_PARALLEL` (kept in sync by the check below;
/// duplicated here so it appears in the generated C header).
pub const LOGI_FLAG_BOUNDED_PARALLEL: u32 = 1;
const _: () = assert!(LOGI_FLAG_BOUNDED_PARALLEL == logi_schema::LOGI_FLAG_BOUNDED_PARALLEL);

const LOGI_ERROR_MSG_LEN: usize = 256;

/// Structured error out-parameter. The caller owns the storage; `message` is a
/// fixed, null-terminated buffer (no allocation crosses the boundary).
#[repr(C)]
pub struct LogiError {
	pub code: u32,
	pub message: [core::ffi::c_char; 256],
}

/// Creation parameters.
#[repr(C)]
pub struct LogiInitDesc {
	pub abi_version: u32,
	pub worker_thread_count: u32,
	pub fixed_step_hz: u32,
	pub deterministic_seed: u64,
	pub flags: u32,
}

/// Per-frame input from Unreal. `cameras`/`player_intents` are borrowed for the
/// duration of the call only.
#[repr(C)]
pub struct LogiFrameInput {
	pub unreal_time_seconds: f64,
	pub frame_delta_seconds: f32,
	pub max_fixed_steps: u32,
	pub camera_count: u32,
	pub cameras: *const LogiCameraView,
	pub player_intent_count: u32,
	pub player_intents: *const LogiPlayerIntent,
}

/// Result of a tick.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct LogiTickResult {
	pub fixed_steps_run: u32,
	pub dropped_steps: u32,
	pub sim_tick: u64,
	pub render_snapshot_id: u64,
	pub status: u32,
}

impl LogiTickResult {
	fn empty(status: u32) -> Self {
		Self { fixed_steps_run: 0, dropped_steps: 0, sim_tick: 0, render_snapshot_id: 0, status }
	}
}

/// Borrowed view over a render snapshot (§8): instanced-mesh batches, per-actor
/// updates, and despawns. All pointers are valid only until the next
/// `logi_world_tick` on the same world.
#[repr(C)]
pub struct LogiRenderSnapshot {
	pub snapshot_id: u64,
	pub sim_tick: u64,
	pub static_batch_count: u32,
	pub static_batches: *const LogiStaticBatch,
	pub actor_update_count: u32,
	pub actor_updates: *const LogiActorUpdate,
	pub despawn_count: u32,
	pub despawns: *const LogiEntityId,
}

/// Opaque owned buffer. Reserved for owned out-buffers; release with
/// [`logi_buffer_release`]. (The snapshot hot path uses a borrowed view instead.)
#[allow(dead_code)]
pub struct LogiBuffer {
	data: Vec<u8>,
}

// --- error helpers -----------------------------------------------------------

#[derive(thiserror::Error, Debug)]
enum FfiError {
	#[error("null pointer argument: {0}")]
	NullArg(&'static str),
	#[error("ABI version mismatch: caller={caller}, library={library}")]
	AbiMismatch { caller: u32, library: u32 },
	#[error("operation panicked: {0}")]
	Panic(&'static str),
	#[error("render snapshot {0} is unavailable")]
	SnapshotUnavailable(u64),
}

impl FfiError {
	fn code(&self) -> u32 {
		match self {
			FfiError::NullArg(_) => LOGI_ERR_NULL,
			FfiError::AbiMismatch { .. } => LOGI_ERR_ABI,
			FfiError::Panic(_) => LOGI_ERR_PANIC,
			FfiError::SnapshotUnavailable(_) => LOGI_ERR_SNAPSHOT,
		}
	}
}

/// Write a structured error into the caller's out-param (no-op if null).
fn write_err(err: *mut LogiError, e: &FfiError) {
	if err.is_null() {
		return;
	}
	let msg = e.to_string();
	let bytes = msg.as_bytes();
	let n = bytes.len().min(LOGI_ERROR_MSG_LEN - 1);
	// SAFETY: caller guarantees `err` points to writable LogiError storage.
	unsafe {
		(*err).code = e.code();
		let dst = (*err).message.as_mut_ptr() as *mut u8;
		core::ptr::copy_nonoverlapping(bytes.as_ptr(), dst, n);
		*dst.add(n) = 0;
	}
}

/// Reset the caller's error out-param to "ok".
fn clear_err(err: *mut LogiError) {
	if err.is_null() {
		return;
	}
	// SAFETY: caller guarantees `err` points to writable LogiError storage.
	unsafe {
		(*err).code = LOGI_OK;
		(*err).message[0] = 0;
	}
}

// --- exported C ABI ----------------------------------------------------------

/// Return the ABI version this library was built with.
#[no_mangle]
pub extern "C" fn logi_abi_version() -> u32 {
	LOGI_ABI_VERSION
}

/// Create a world. Validates the ABI version. Returns null on failure (and fills `err`).
#[no_mangle]
pub extern "C" fn logi_world_create(desc: *const LogiInitDesc, err: *mut LogiError) -> *mut LogiWorld {
	clear_err(err);
	if desc.is_null() {
		write_err(err, &FfiError::NullArg("desc"));
		return std::ptr::null_mut();
	}
	// SAFETY: non-null, checked above.
	let desc = unsafe { &*desc };
	if desc.abi_version != LOGI_ABI_VERSION {
		write_err(err, &FfiError::AbiMismatch { caller: desc.abi_version, library: LOGI_ABI_VERSION });
		return std::ptr::null_mut();
	}

	let config = WorldConfig {
		fixed_step_hz: desc.fixed_step_hz,
		deterministic_seed: desc.deterministic_seed,
		worker_thread_count: desc.worker_thread_count,
		flags: desc.flags,
	};

	match catch_unwind(AssertUnwindSafe(|| Box::into_raw(Box::new(LogiWorld::new(config))))) {
		Ok(ptr) => ptr,
		Err(_) => {
			write_err(err, &FfiError::Panic("logi_world_create"));
			std::ptr::null_mut()
		}
	}
}

/// Destroy a world created by [`logi_world_create`].
#[no_mangle]
pub extern "C" fn logi_world_destroy(world: *mut LogiWorld) {
	if world.is_null() {
		return;
	}
	// SAFETY: `world` came from Box::into_raw and is non-null.
	let _ = catch_unwind(AssertUnwindSafe(|| unsafe { drop(Box::from_raw(world)) }));
}

/// Advance the simulation. The fixed-step accumulator is internal to Rust.
#[no_mangle]
pub extern "C" fn logi_world_tick(world: *mut LogiWorld, input: *const LogiFrameInput, err: *mut LogiError) -> LogiTickResult {
	clear_err(err);
	if world.is_null() || input.is_null() {
		write_err(err, &FfiError::NullArg(if world.is_null() { "world" } else { "input" }));
		return LogiTickResult::empty(LOGI_ERR_NULL);
	}

	// SAFETY: both non-null (checked). `world` is uniquely owned by the caller
	// for this call (rule 7: not reentrant).
	let result = catch_unwind(AssertUnwindSafe(|| {
		let world = unsafe { &mut *world };
		let input = unsafe { &*input };

		let cameras: &[LogiCameraView] = if input.cameras.is_null() || input.camera_count == 0 {
			&[]
		} else {
			unsafe { slice::from_raw_parts(input.cameras, input.camera_count as usize) }
		};
		let intents: &[LogiPlayerIntent] = if input.player_intents.is_null() || input.player_intent_count == 0 {
			&[]
		} else {
			unsafe { slice::from_raw_parts(input.player_intents, input.player_intent_count as usize) }
		};

		world.tick(input.frame_delta_seconds, input.max_fixed_steps, cameras, intents)
	}));

	match result {
		Ok(o) => LogiTickResult {
			fixed_steps_run: o.fixed_steps_run,
			dropped_steps: o.dropped_steps,
			sim_tick: o.sim_tick,
			render_snapshot_id: o.render_snapshot_id,
			status: LOGI_OK,
		},
		Err(_) => {
			write_err(err, &FfiError::Panic("logi_world_tick"));
			LogiTickResult::empty(LOGI_ERR_PANIC)
		}
	}
}

/// Fill `out_snapshot` with a borrowed view of a render snapshot. Pass
/// `snapshot_id == 0` for the latest. View is valid until the next tick.
#[no_mangle]
pub extern "C" fn logi_world_get_render_snapshot(
	world: *mut LogiWorld,
	snapshot_id: u64,
	out_snapshot: *mut LogiRenderSnapshot,
	err: *mut LogiError,
) -> u32 {
	clear_err(err);
	if world.is_null() || out_snapshot.is_null() {
		write_err(err, &FfiError::NullArg(if world.is_null() { "world" } else { "out_snapshot" }));
		return LOGI_ERR_NULL;
	}
	// Default the out-param to empty.
	// SAFETY: non-null, checked above.
	unsafe {
		*out_snapshot = LogiRenderSnapshot {
			snapshot_id: 0,
			sim_tick: 0,
			static_batch_count: 0,
			static_batches: std::ptr::null(),
			actor_update_count: 0,
			actor_updates: std::ptr::null(),
			despawn_count: 0,
			despawns: std::ptr::null(),
		};
	}

	let result = catch_unwind(AssertUnwindSafe(|| {
		let world = unsafe { &*world };
		// `snapshot_id == 0` means "latest"; otherwise it must match.
		if snapshot_id != 0 && snapshot_id != world.snapshot_id() {
			return None;
		}
		let batches = world.static_batches();
		let actors = world.actor_updates();
		let despawns = world.despawns();
		Some(LogiRenderSnapshot {
			snapshot_id: world.snapshot_id(),
			sim_tick: world.snapshot_sim_tick(),
			static_batch_count: batches.len() as u32,
			static_batches: batches.as_ptr(),
			actor_update_count: actors.len() as u32,
			actor_updates: actors.as_ptr(),
			despawn_count: despawns.len() as u32,
			despawns: despawns.as_ptr(),
		})
	}));

	match result {
		Ok(Some(view)) => {
			unsafe { *out_snapshot = view };
			LOGI_OK
		}
		Ok(None) => {
			write_err(err, &FfiError::SnapshotUnavailable(snapshot_id));
			LOGI_ERR_SNAPSHOT
		}
		Err(_) => {
			write_err(err, &FfiError::Panic("logi_world_get_render_snapshot"));
			LOGI_ERR_PANIC
		}
	}
}

/// Submit a batch of spawn commands (a controlled mutation phase). Returns the
/// number applied; 0 with an error set on failure.
#[no_mangle]
pub extern "C" fn logi_submit_spawn_commands(
	world: *mut LogiWorld,
	commands: *const LogiSpawnCommand,
	count: u32,
	err: *mut LogiError,
) -> u32 {
	clear_err(err);
	if world.is_null() || (commands.is_null() && count != 0) {
		write_err(err, &FfiError::NullArg(if world.is_null() { "world" } else { "commands" }));
		return 0;
	}
	if count == 0 {
		return 0;
	}

	let result = catch_unwind(AssertUnwindSafe(|| {
		let world = unsafe { &mut *world };
		let cmds = unsafe { slice::from_raw_parts(commands, count as usize) };
		world.submit_spawn_commands(cmds)
	}));

	match result {
		Ok(applied) => applied,
		Err(_) => {
			write_err(err, &FfiError::Panic("logi_submit_spawn_commands"));
			0
		}
	}
}

/// Release an owned buffer previously handed out by the API. (Reserved; the
/// snapshot path uses borrowed views.)
#[no_mangle]
pub extern "C" fn logi_buffer_release(_world: *mut LogiWorld, buffer: *mut LogiBuffer) {
	if buffer.is_null() {
		return;
	}
	// SAFETY: `buffer`, if non-null, was produced by this library via Box::into_raw.
	let _ = catch_unwind(AssertUnwindSafe(|| unsafe { drop(Box::from_raw(buffer)) }));
}
