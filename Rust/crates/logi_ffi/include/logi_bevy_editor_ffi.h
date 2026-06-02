#pragma once
#ifndef LOGI_BEVY_EDITOR_FFI_H
#define LOGI_BEVY_EDITOR_FFI_H

/*
 * Editor-only entity introspection ABI (ADR-0006). Compiled ONLY under WITH_EDITOR
 * on the C++ side and the Rust `editor_inspector` feature on the DLL side; NEVER
 * include or link this in shipping/test builds. Hand-authored to mirror
 * Rust/crates/logi_ffi/src/editor.rs + logi_schema::LogiEntitySummary (kept in a
 * SEPARATE header so the runtime hot ABI stays clean).
 *
 * Contract: queries are paged + sampled (5-10 Hz), never per-row from Slate paint;
 * edits route through explicit commands; the owner-thread guard applies (§11).
 */

#include "logi_bevy_ffi.h" /* LogiWorld, LogiEntityId, LogiError, LOGI_OK, ... */

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* One entity row in the Bevy Entity Outliner. Mirrors logi_schema::LogiEntitySummary. */
typedef struct LogiEntitySummary {
  LogiEntityId id;
  double position_cm[3];
  float significance;
  uint8_t representation_tier;
  uint8_t pinned;
  uint8_t _pad[2];
} LogiEntitySummary;

/* Paged entity query (in). limit == 0 means "no cap" (all from offset). */
typedef struct LogiEntityQueryDesc {
  uint32_t offset;
  uint32_t limit;
} LogiEntityQueryDesc;

/* Borrowed page of entity summaries (out). `entities` is valid until the next
 * logi_editor_query_entities on the same world (single-buffered, sampled). */
typedef struct LogiEntityInspectorPacket {
  uint32_t entity_count;
  uint32_t total_count;
  const LogiEntitySummary *entities;
} LogiEntityInspectorPacket;

/* Fill out_packet with a page of entity summaries. Owner-thread only. */
uint32_t logi_editor_query_entities(LogiWorld *world,
                                    const LogiEntityQueryDesc *query,
                                    LogiEntityInspectorPacket *out_packet,
                                    LogiError *err);

/* Pin/unpin an entity for editor debug focus. Owner-thread only. */
uint32_t logi_editor_set_debug_pin(LogiWorld *world,
                                   LogiEntityId id,
                                   uint8_t pinned,
                                   LogiError *err);

#ifdef __cplusplus
} /* extern "C" */
#endif

#endif /* LOGI_BEVY_EDITOR_FFI_H */
