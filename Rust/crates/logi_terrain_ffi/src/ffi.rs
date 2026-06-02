//! The C-ABI entry points (runtime-model.md §3.1). Every call is wrapped so a Rust panic becomes a
//! status code / null instead of unwinding across FFI (UB). The world handle is an opaque
//! `*mut LogiWorld` (a boxed pointer); null is invalid.

use std::cell::RefCell;
use std::panic::{self, AssertUnwindSafe};

use logi_terrain_scene::{
    LogiBufferHandle, LogiBufferView, LogiCameraState, LogiFrameId, LogiFramePacket,
    LogiStreamingBudget, LogiWorld, LogiWorldCreateInfo,
};

/// Status codes returned across the boundary. Matches `LogiStatus` (int32) in `logi_terrain.h`.
#[repr(i32)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LogiStatus {
    Ok = 0,
    NullArg = -1,
    InvalidHandle = -2,
    Panic = -3,
    Internal = -4,
    AbiMismatch = -5,
}

thread_local! {
    static LAST_ERROR: RefCell<String> = const { RefCell::new(String::new()) };
}

fn set_last_error(msg: impl Into<String>) {
    LAST_ERROR.with(|e| *e.borrow_mut() = msg.into());
}

fn guard(f: impl FnOnce() -> LogiStatus) -> LogiStatus {
    match panic::catch_unwind(AssertUnwindSafe(f)) {
        Ok(status) => status,
        Err(_) => {
            set_last_error("panic caught at logi-terrain FFI boundary");
            LogiStatus::Panic
        }
    }
}

/// Returns the ABI version. Unity must verify this before any other call.
#[no_mangle]
pub extern "C" fn logi_abi_version() -> u32 {
    crate::LOGI_ABI_VERSION
}

/// Creates a world. Pass `info == null` for defaults. Returns null on failure.
#[no_mangle]
pub extern "C" fn logi_world_create(info: *const LogiWorldCreateInfo) -> *mut LogiWorld {
    let result = panic::catch_unwind(AssertUnwindSafe(|| {
        let info = if info.is_null() {
            LogiWorldCreateInfo::defaults()
        } else {
            unsafe { *info }
        };
        Box::into_raw(Box::new(LogiWorld::new(info)))
    }));
    result.unwrap_or(std::ptr::null_mut())
}

/// Destroys a world created by [`logi_world_create`].
#[no_mangle]
pub extern "C" fn logi_world_destroy(world: *mut LogiWorld) {
    if world.is_null() {
        return;
    }
    let _ = panic::catch_unwind(AssertUnwindSafe(|| unsafe {
        drop(Box::from_raw(world));
    }));
}

/// Sets the camera state used by the next `step`.
#[no_mangle]
pub extern "C" fn logi_world_set_camera(
    world: *mut LogiWorld,
    camera: *const LogiCameraState,
) -> LogiStatus {
    guard(|| {
        let world = match unsafe { world.as_mut() } {
            Some(w) => w,
            None => return LogiStatus::InvalidHandle,
        };
        if camera.is_null() {
            return LogiStatus::NullArg;
        }
        world.set_camera(unsafe { *camera });
        LogiStatus::Ok
    })
}

/// Re-budgets streaming (memory + in-flight cap).
#[no_mangle]
pub extern "C" fn logi_world_set_budget(
    world: *mut LogiWorld,
    budget: *const LogiStreamingBudget,
) -> LogiStatus {
    guard(|| {
        let world = match unsafe { world.as_mut() } {
            Some(w) => w,
            None => return LogiStatus::InvalidHandle,
        };
        if budget.is_null() {
            return LogiStatus::NullArg;
        }
        world.set_budget(unsafe { *budget });
        LogiStatus::Ok
    })
}

/// Advances one frame and writes the new frame id to `out_frame_id`.
#[no_mangle]
pub extern "C" fn logi_world_step(
    world: *mut LogiWorld,
    delta_seconds: f64,
    out_frame_id: *mut LogiFrameId,
) -> LogiStatus {
    guard(|| {
        let world = match unsafe { world.as_mut() } {
            Some(w) => w,
            None => return LogiStatus::InvalidHandle,
        };
        if out_frame_id.is_null() {
            return LogiStatus::NullArg;
        }
        let frame_id = world.step(delta_seconds);
        unsafe { *out_frame_id = frame_id };
        LogiStatus::Ok
    })
}

/// Writes the frame packet for `frame_id` (must be a live frame) to `out_packet`.
#[no_mangle]
pub extern "C" fn logi_world_get_frame_packet(
    world: *mut LogiWorld,
    frame_id: LogiFrameId,
    out_packet: *mut LogiFramePacket,
) -> LogiStatus {
    guard(|| {
        let world = match unsafe { world.as_ref() } {
            Some(w) => w,
            None => return LogiStatus::InvalidHandle,
        };
        if out_packet.is_null() {
            return LogiStatus::NullArg;
        }
        match world.frame_packet(frame_id) {
            Some(packet) => {
                unsafe { *out_packet = packet };
                LogiStatus::Ok
            }
            None => {
                set_last_error("logi_world_get_frame_packet: stale or unknown frame_id");
                LogiStatus::InvalidHandle
            }
        }
    })
}

/// Resolves a buffer handle to a zero-copy view of Rust-owned bytes.
#[no_mangle]
pub extern "C" fn logi_world_acquire_buffer_view(
    world: *mut LogiWorld,
    handle: LogiBufferHandle,
    out_view: *mut LogiBufferView,
) -> LogiStatus {
    guard(|| {
        let world = match unsafe { world.as_ref() } {
            Some(w) => w,
            None => return LogiStatus::InvalidHandle,
        };
        if out_view.is_null() {
            return LogiStatus::NullArg;
        }
        match world.buffer_view(handle) {
            Some(view) => {
                unsafe { *out_view = view };
                LogiStatus::Ok
            }
            None => {
                set_last_error("logi_world_acquire_buffer_view: unknown buffer handle");
                LogiStatus::InvalidHandle
            }
        }
    })
}

/// Releases a frame, freeing its ring slot.
#[no_mangle]
pub extern "C" fn logi_world_release_frame(world: *mut LogiWorld, frame_id: LogiFrameId) {
    if world.is_null() {
        return;
    }
    let _ = panic::catch_unwind(AssertUnwindSafe(|| {
        if let Some(world) = unsafe { world.as_mut() } {
            world.release_frame(frame_id);
        }
    }));
}

/// Copies the last error message (UTF-8, not NUL-terminated) into `buf`; returns bytes written.
#[no_mangle]
pub extern "C" fn logi_last_error_message(buf: *mut u8, buf_len: usize) -> usize {
    if buf.is_null() || buf_len == 0 {
        return 0;
    }
    LAST_ERROR.with(|e| {
        let msg = e.borrow();
        let bytes = msg.as_bytes();
        let n = bytes.len().min(buf_len);
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), buf, n) };
        n
    })
}
