//! `logi_ffi` — the narrow C ABI between Unreal (caller) and the Rust simulation.
//!
//! Contract (ADR-0005): plain data only, NO Unreal types cross, and Rust never
//! calls Unreal. Every entry point catches panics so a Rust panic can never
//! unwind across the C boundary (which would be undefined behavior).

use std::panic::{catch_unwind, AssertUnwindSafe};

use logi_render_extract::AgentInstance;
use logi_runtime::LogiRuntime;
use logi_schema::{AgentSpawnRecipe, LogiAgentId, LogiRouteId, LogiTileId, Vec3};

/// ABI version. Bump the major on any breaking change to this header.
pub const LOGI_ABI_MAJOR: u32 = 0;
pub const LOGI_ABI_MINOR: u32 = 1;

/// Versioned ABI descriptor returned by [`logi_abi_version`].
#[repr(C)]
pub struct LogiAbiVersion {
    pub major: u32,
    pub minor: u32,
}

/// Status codes returned across the boundary.
#[repr(i32)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum LogiStatus {
    Ok = 0,
    NullHandle = 1,
    Panic = 2,
}

/// Borrowed, read-only view over the runtime's agent snapshot buffer. Valid only
/// until the next call that mutates the same runtime.
#[repr(C)]
pub struct LogiInstanceView {
    pub ptr: *const AgentInstance,
    pub len: usize,
}

/// Plain-data spawn recipe as it crosses the ABI.
#[repr(C)]
pub struct LogiAgentSpawnDesc {
    pub agent_id: u64,
    pub route_id: u64,
    pub tile_id: u64,
    pub position: Vec3,
    pub payload_kg: f64,
}

/// Opaque handle to a runtime instance (a boxed `LogiRuntime`).
pub type LogiRuntimeHandle = *mut LogiRuntime;

/// Return the ABI version the library was built with.
#[no_mangle]
pub extern "C" fn logi_abi_version() -> LogiAbiVersion {
    LogiAbiVersion { major: LOGI_ABI_MAJOR, minor: LOGI_ABI_MINOR }
}

/// Create a runtime with a deterministic seed. Returns null on failure.
#[no_mangle]
pub extern "C" fn logi_runtime_create(seed: u64) -> LogiRuntimeHandle {
    catch_unwind(|| Box::into_raw(Box::new(LogiRuntime::new(seed)))).unwrap_or(std::ptr::null_mut())
}

/// Destroy a runtime created by [`logi_runtime_create`].
#[no_mangle]
pub extern "C" fn logi_runtime_destroy(handle: LogiRuntimeHandle) {
    if handle.is_null() {
        return;
    }
    let _ = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: `handle` came from `Box::into_raw` and is non-null.
        unsafe { drop(Box::from_raw(handle)) };
    }));
}

/// Spawn one agent. Returns its entity bits, or 0 on failure.
#[no_mangle]
pub extern "C" fn logi_runtime_spawn_agent(handle: LogiRuntimeHandle, desc: LogiAgentSpawnDesc) -> u64 {
    if handle.is_null() {
        return 0;
    }
    catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: non-null handle owned by the caller for the call's duration.
        let rt = unsafe { &mut *handle };
        rt.spawn_agent(AgentSpawnRecipe {
            agent: LogiAgentId(desc.agent_id),
            route: LogiRouteId(desc.route_id),
            tile: LogiTileId(desc.tile_id),
            position: desc.position,
            payload_kg: desc.payload_kg,
        })
    }))
    .unwrap_or(0)
}

/// Advance the deterministic sim by `steps` fixed ticks.
#[no_mangle]
pub extern "C" fn logi_runtime_tick(handle: LogiRuntimeHandle, steps: u32) -> LogiStatus {
    if handle.is_null() {
        return LogiStatus::NullHandle;
    }
    let r = catch_unwind(AssertUnwindSafe(|| {
        let rt = unsafe { &mut *handle };
        rt.tick(steps);
    }));
    if r.is_ok() {
        LogiStatus::Ok
    } else {
        LogiStatus::Panic
    }
}

/// Re-extract and borrow the agent render snapshot. The view is valid until the
/// next mutating call on this runtime. Fills `out_view`, or zeroes it on error.
#[no_mangle]
pub extern "C" fn logi_runtime_extract(handle: LogiRuntimeHandle, out_view: *mut LogiInstanceView) -> LogiStatus {
    if out_view.is_null() {
        return LogiStatus::NullHandle;
    }
    // Default the out-param to an empty view.
    // SAFETY: caller guarantees `out_view` points to writable storage.
    unsafe { *out_view = LogiInstanceView { ptr: std::ptr::null(), len: 0 } };
    if handle.is_null() {
        return LogiStatus::NullHandle;
    }
    let r = catch_unwind(AssertUnwindSafe(|| {
        let rt = unsafe { &mut *handle };
        let snap = rt.extract();
        LogiInstanceView { ptr: snap.agents.as_ptr(), len: snap.agents.len() }
    }));
    match r {
        Ok(view) => {
            unsafe { *out_view = view };
            LogiStatus::Ok
        }
        Err(_) => LogiStatus::Panic,
    }
}

/// Current fixed-step tick (0 on null handle).
#[no_mangle]
pub extern "C" fn logi_runtime_tick_count(handle: LogiRuntimeHandle) -> u64 {
    if handle.is_null() {
        return 0;
    }
    catch_unwind(AssertUnwindSafe(|| {
        let rt = unsafe { &*handle };
        rt.tick_count()
    }))
    .unwrap_or(0)
}
