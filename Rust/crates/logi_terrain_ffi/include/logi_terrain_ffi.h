/*
 * logi_terrain_ffi.h — C ABI for the Logi terrain runtime (the LogiTerrainWorld object).
 *
 * Mirrors Rust/crates/logi_terrain_scene/src/lib.rs and Rust/crates/logi_terrain_ffi/src/ffi.rs
 * byte-for-byte. Keep field order identical across this header and the Rust #[repr(C)] structs.
 * See Docs/Architecture/ADR-0007-Actorless-Rust-Terrain.md.
 * ABI version 3 (glam-typed: f32 render-local + f64 world).
 */
#ifndef LOGI_TERRAIN_FFI_H
#define LOGI_TERRAIN_FFI_H

#include <stdint.h>
#include <stddef.h>

/* ABI version. Must match logi_terrain_abi_version() in the loaded DLL (mirrors LOGI_ABI_VERSION
 * in Rust/crates/logi_terrain_ffi/src/lib.rs). Callers verify before any other call. */
#define LOGI_TERRAIN_ABI_VERSION 3u

#ifdef __cplusplus
extern "C" {
#endif

/* Status codes (int32). */
typedef int32_t LogiStatus;
#define LOGI_TERRAIN_OK              0
#define LOGI_TERRAIN_NULL_ARG       (-1)
#define LOGI_TERRAIN_INVALID_HANDLE (-2)
#define LOGI_TERRAIN_PANIC          (-3)
#define LOGI_TERRAIN_INTERNAL       (-4)
#define LOGI_TERRAIN_ABI_MISMATCH   (-5)

#define LOGI_INDEX_FORMAT_U16 0u
#define LOGI_INDEX_FORMAT_U32 1u
#define LOGI_BUFFER_KIND_VERTEX 0u
#define LOGI_BUFFER_KIND_INDEX  1u

typedef uint64_t LogiFrameId;
typedef uint64_t LogiBufferHandle;
typedef struct LogiTerrainWorld LogiTerrainWorld; /* opaque */

/* glam-typed vectors, byte-identical to Unity.Mathematics float3 / double3. */
typedef struct LogiVec3 {
    float x, y, z;          /* f32 render-local (glam Vec3 ↔ float3) */
} LogiVec3;

typedef struct LogiDVec3 {
    double x, y, z;         /* f64 world / planet-scale (glam DVec3 ↔ double3) */
} LogiDVec3;

typedef struct LogiBoundingSphere {
    LogiVec3 center;        /* render-local (f32) */
    float    radius;
} LogiBoundingSphere;

typedef struct LogiCameraState {
    LogiDVec3 position;     /* f64 world */
    LogiVec3  forward;      /* f32 unit direction */
    LogiVec3  up;
    float     fov_y_radians;
    float     viewport_width;
    float     viewport_height;
    float     sse_target_pixels;
} LogiCameraState;

typedef struct LogiStreamingBudget {
    uint64_t memory_budget_bytes;
    uint32_t max_in_flight;
    uint32_t _reserved;
} LogiStreamingBudget;

typedef struct LogiWorldCreateInfo {
    double              world_size;
    LogiStreamingBudget budget;
    uint32_t            max_level;
    uint32_t            _reserved;
} LogiWorldCreateInfo;

typedef struct LogiLocalFrame {
    LogiDVec3 origin;       /* f64 world point Unity treats as local zero */
} LogiLocalFrame;

typedef struct LogiBufferView {
    const uint8_t* data;
    uint64_t       byte_length;
    uint32_t       element_stride;
    uint32_t       kind;
} LogiBufferView;

typedef struct LogiTerrainPatchDraw {
    uint64_t           patch_handle;
    LogiBufferHandle   vertex_buffer;
    LogiBufferHandle   index_buffer;
    LogiBoundingSphere bounds;
    LogiVec3           local_position;  /* f32, relative to local_frame.origin */
    float              geometric_error;
    uint32_t           lod;
    uint32_t           vertex_count;
    uint32_t           vertex_stride;
    uint32_t           index_count;
    uint32_t           index_format;
    uint32_t           material_id;
    uint32_t           _reserved;
} LogiTerrainPatchDraw;

typedef struct LogiUploadCommand {
    uint64_t       page_handle;
    const uint8_t* data;
    uint64_t       byte_length;
    uint32_t       slot;
    uint32_t       kind;
} LogiUploadCommand;

typedef struct LogiInstanceBatchDraw {
    LogiBufferHandle mesh_handle;
    const uint8_t*   instance_data;
    uint32_t         instance_count;
    uint32_t         instance_stride;
    uint32_t         material_id;
    uint32_t         _reserved;
} LogiInstanceBatchDraw;

typedef struct LogiDecalBatchDraw {
    const uint8_t* data;
    uint32_t       count;
    uint32_t       material_id;
} LogiDecalBatchDraw;

typedef struct LogiColliderCommand {
    uint64_t       patch_handle;
    const uint8_t* data;
    uint64_t       byte_length;
    uint32_t       kind;
    uint32_t       _reserved;
} LogiColliderCommand;

typedef struct LogiAttributionEntry {
    const uint8_t* text;
    uint32_t       text_len;
    uint32_t       source_id;
} LogiAttributionEntry;

typedef struct LogiTerrainTelemetry {
    uint64_t frames;
    uint64_t bytes_resident;
    uint64_t peak_bytes_resident;
    uint64_t cache_hits;
    uint64_t cache_misses;
    uint64_t evictions;
    uint64_t requests_issued;
    uint64_t requests_completed;
    uint64_t fallbacks_used;
    uint32_t resident_tiles;
    uint32_t in_flight;
} LogiTerrainTelemetry;

typedef struct LogiFramePacket {
    uint64_t       frame_id;
    LogiLocalFrame local_frame;

    const LogiTerrainPatchDraw*  terrain_patches;
    const LogiUploadCommand*     material_page_uploads;
    const LogiUploadCommand*     height_page_uploads;
    const LogiInstanceBatchDraw* instance_batches;
    const LogiDecalBatchDraw*    decal_batches;
    const LogiColliderCommand*   collider_commands;
    const LogiAttributionEntry*  attributions;

    uint32_t terrain_patch_count;
    uint32_t material_page_upload_count;
    uint32_t height_page_upload_count;
    uint32_t instance_batch_count;
    uint32_t decal_batch_count;
    uint32_t collider_command_count;
    uint32_t attribution_count;
    uint32_t _pad;

    LogiTerrainTelemetry telemetry;
} LogiFramePacket;

/* --- Entry points (see src/ffi.rs) --- */

uint32_t   logi_terrain_abi_version(void);

LogiTerrainWorld* logi_terrain_world_create(const LogiWorldCreateInfo* info);
void       logi_terrain_world_destroy(LogiTerrainWorld* world);

LogiStatus logi_terrain_world_set_camera(LogiTerrainWorld* world, const LogiCameraState* camera);
LogiStatus logi_terrain_world_set_budget(LogiTerrainWorld* world, const LogiStreamingBudget* budget);

LogiStatus logi_terrain_world_step(LogiTerrainWorld* world, double delta_seconds, LogiFrameId* out_frame_id);
LogiStatus logi_terrain_world_get_frame_packet(LogiTerrainWorld* world, LogiFrameId frame_id, LogiFramePacket* out_packet);
LogiStatus logi_terrain_world_acquire_buffer_view(LogiTerrainWorld* world, LogiBufferHandle handle, LogiBufferView* out_view);
void       logi_terrain_world_release_frame(LogiTerrainWorld* world, LogiFrameId frame_id);

size_t     logi_terrain_last_error_message(uint8_t* buf, size_t buf_len);

#ifdef __cplusplus
} /* extern "C" */
#endif

#endif /* LOGI_TERRAIN_FFI_H */
