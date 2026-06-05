//! Editor-only entity introspection ABI (ADR-0006). Compiled ONLY under the
//! `editor_inspector` feature and called only under `WITH_EDITOR` on the C++ side;
//! emitted into the SEPARATE header `logi_bevy_editor_ffi.h`, so the runtime hot
//! ABI and shipping builds stay clean (no `logi_editor_*` symbols ship). Queries
//! are paged + sampled (5-10 Hz), never per-row, and every mutation funnels
//! through the same owner-thread guard as the runtime ABI (§11).

use logi_runtime::LogiWorld;
use logi_schema::{LogiEntityId, LogiEntitySummary};

use crate::{clear_err, write_err, FfiError, LogiError, LOGI_ERR_NULL, LOGI_ERR_WRONG_THREAD, LOGI_OK};

/// Paged entity query (in). `limit == 0` means "no cap" (all from `offset`).
#[repr(C)]
pub struct LogiEntityQueryDesc {
    pub offset: u32,
    pub limit: u32,
}

/// Borrowed page of entity summaries (out). `entities` is valid until the next
/// `logi_editor_query_entities` on the same world (single-buffered, sampled).
#[repr(C)]
pub struct LogiEntityInspectorPacket {
    pub entity_count: u32,
    pub total_count: u32,
    pub entities: *const LogiEntitySummary,
}

/// Fill `out_packet` with a page of entity summaries. Owner-thread only.
///
/// # Safety
/// `world` must be a valid handle from [`crate::logi_world_create`] called on its owner thread; `query`
/// must be null or point to a readable `LogiEntityQueryDesc`; `out_packet` must be null or point to
/// writable `LogiEntityInspectorPacket` storage; `err` must be null or writable. The returned `entities`
/// pointer is valid only until the next query on the same world.
#[no_mangle]
pub unsafe extern "C" fn logi_editor_query_entities(
    world: *mut LogiWorld,
    query: *const LogiEntityQueryDesc,
    out_packet: *mut LogiEntityInspectorPacket,
    err: *mut LogiError,
) -> u32 {
    clear_err(err);
    if world.is_null() || query.is_null() || out_packet.is_null() {
        write_err(err, &FfiError::NullArg("editor_query"));
        return LOGI_ERR_NULL;
    }
    // SAFETY: non-null, checked above.
    let w = unsafe { &mut *world };
    if !w.is_owner_thread() {
        write_err(err, &FfiError::WrongThread("logi_editor_query_entities"));
        return LOGI_ERR_WRONG_THREAD;
    }
    let q = unsafe { &*query };
    let limit = if q.limit == 0 { u32::MAX } else { q.limit };
    let total = w.editor_query_entities(q.offset, limit);
    let entities = w.editor_entities();
    // SAFETY: out_packet non-null (checked); `entities` points into the world's
    // single editor buffer, valid until the next query.
    unsafe {
        *out_packet = LogiEntityInspectorPacket {
            entity_count: entities.len() as u32,
            total_count: total,
            entities: entities.as_ptr(),
        };
    }
    LOGI_OK
}

/// Pin/unpin an entity for editor debug focus. Owner-thread only.
///
/// # Safety
/// `world` must be a valid handle from [`crate::logi_world_create`] called on its owner thread; `err` must
/// be null or point to writable `LogiError` storage.
#[no_mangle]
pub unsafe extern "C" fn logi_editor_set_debug_pin(
    world: *mut LogiWorld,
    id: LogiEntityId,
    pinned: u8,
    err: *mut LogiError,
) -> u32 {
    clear_err(err);
    if world.is_null() {
        write_err(err, &FfiError::NullArg("world"));
        return LOGI_ERR_NULL;
    }
    // SAFETY: non-null, checked above.
    let w = unsafe { &mut *world };
    if !w.is_owner_thread() {
        write_err(err, &FfiError::WrongThread("logi_editor_set_debug_pin"));
        return LOGI_ERR_WRONG_THREAD;
    }
    w.editor_set_pin(id, pinned != 0);
    LOGI_OK
}
