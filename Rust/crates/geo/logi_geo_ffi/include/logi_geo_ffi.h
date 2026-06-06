/*
 * logi_geo_ffi.h — C ABI for the Logi "Geo" engine (real-world terrain / earth stack).
 *
 * Mirrors the #[repr(C)] definitions in Rust/crates/geo/logi_geo_ffi/src/ffi.rs and src/ffi_baked.rs
 * byte-for-byte. Keep field order identical across this header and the Rust structs.
 * See Docs/Architecture/ADR-0007-Actorless-Rust-Terrain.md.
 * ABI version 5 (glam-typed: f32 render-local + f64 world; globe set_view takes a LogiGlobeView struct —
 * camera basis + aspect — for the view frustum; the deprecated globe step shim is removed).
 */
#ifndef LOGI_GEO_FFI_H
#define LOGI_GEO_FFI_H

#include <stdint.h>
#include <stddef.h>

/* ABI version. Must match logi_geo_abi_version() in the loaded DLL (mirrors LOGI_ABI_VERSION
 * in Rust/crates/geo/logi_geo_ffi/src/lib.rs). Callers verify before any other call. */
#define LOGI_GEO_ABI_VERSION 5u

#ifdef __cplusplus
extern "C" {
#endif

/* Status codes (int32). */
typedef int32_t LogiStatus;
#define LOGI_GEO_OK              0
#define LOGI_GEO_NULL_ARG       (-1)
#define LOGI_GEO_INVALID_HANDLE (-2)
#define LOGI_GEO_PANIC          (-3)
#define LOGI_GEO_INTERNAL       (-4)
#define LOGI_GEO_ABI_MISMATCH   (-5)

/* --- Shared Geo-DLL entry points ---
 * The ABI-version + last-error accessors below are SHARED by the globe / baked / building lanes (all one
 * logi_geo.dll). The pre-merge streaming synthetic-terrain world was RETIRED in the one-engine merge —
 * the baked + procedural lanes (logi_baked_*, see below) are the single Geo engine now. */

uint32_t   logi_geo_abi_version(void);

size_t     logi_geo_last_error_message(uint8_t* buf, size_t buf_len);

/* --- M1 building (AGI_HQ, ADR-0009) — additive; see src/ffi.rs. Decode a .dae + .jpg into a textured
 * mesh in Unreal render space (cm, Z-up LH). Separate from the streaming terrain world. --- */

typedef struct LogiBuilding LogiBuilding; /* opaque */

typedef struct LogiPlacement {
    double longitude_deg;
    double latitude_deg;
    double height_m;
    double heading_deg;
} LogiPlacement;

typedef struct LogiBuildingData {
    const uint8_t* vertices;          /* vertex_count * 32-byte vertices (pos f3 | normal f3 | uv f2) */
    uint64_t       vertices_len;
    uint32_t       vertex_count;
    uint32_t       vertex_stride;     /* = 32 */
    const uint8_t* indices;           /* index_count little-endian uint32 indices */
    uint32_t       index_count;
    uint32_t       _pad;
    const uint8_t* texture_jpeg;      /* raw JPEG bytes (decode C++-side) */
    uint64_t       texture_jpeg_len;
    float          bounds_center[3];
    float          bounds_radius;
    double         ecef_origin[3];    /* WGS84 ECEF (metres) — for globe placement (M2+) */
} LogiBuildingData;

LogiBuilding* logi_building_load(const char* dae_path, const char* jpg_path, const LogiPlacement* placement);
LogiStatus    logi_building_get_data(const LogiBuilding* building, LogiBuildingData* out);
void          logi_building_destroy(LogiBuilding* building);

/* --- Streaming globe (Google Photorealistic 3D Tiles, ADR-0009 Milestone F) — additive; see src/ffi.rs.
 * Resolves a Cesium ion asset, streams + decodes glTF tiles on a worker pool, and rebases each into
 * Unreal render space (cm, Z-up LH) at a fixed ENU origin. The world advances on its OWN Rust stream
 * thread (ADR-0012): the game thread pushes the camera with set_view() and pulls the latest frame with
 * poll() — it never blocks on streaming. A polled frame's tile buffers are zero-copy views into Rust-owned
 * memory, valid until the next poll() on that world. --- */

typedef struct LogiGlobeWorld LogiGlobeWorld; /* opaque */

typedef struct LogiGlobeCreateInfo {
    uint64_t ion_asset_id;
    double   origin_longitude_deg;
    double   origin_latitude_deg;
    double   origin_height_m;
    double   max_tile_distance_m;    /* spatial cull radius (metres) around the camera */
    uint32_t workers;                /* fetch threads (clamped 1..64) */
    uint32_t _reserved;
    uint64_t cache_budget_bytes;     /* persistent disk-cache budget (bytes); 0 disables (dir = cache_dir arg) */
} LogiGlobeCreateInfo;

typedef struct LogiGlobeTile {
    uint64_t       tile_key;          /* stable hash of the tile path — key for C++ section caching */
    const uint8_t* vertices;          /* vertex_count * 32-byte vertices (pos f3 | normal f3 | uv f2), UE cm */
    uint64_t       vertices_len;
    uint32_t       vertex_count;
    uint32_t       vertex_stride;     /* = 32 */
    const uint8_t* indices;           /* index_count little-endian uint32 indices */
    uint32_t       index_count;
    uint32_t       _pad;
    const uint8_t* texture_jpeg;      /* raw encoded image bytes (JPEG/PNG), decode C++-side; may be empty */
    uint64_t       texture_jpeg_len;
    float          bounds_center[3];  /* UE render space (cm) */
    float          bounds_radius;
} LogiGlobeTile;

typedef struct LogiGlobeFrame {
    const LogiGlobeTile* tiles;       /* -> world scratch array, valid until the next step() */
    uint32_t             tile_count;
    uint32_t             resident_tiles;
    uint32_t             in_flight;
    uint32_t             _pad;
    double               origin_ecef[3]; /* ENU origin's WGS84 ECEF (metres) — globe/atmosphere placement */
} LogiGlobeFrame;

/* Blocking (ion resolve + root fetch); call off the render thread. Returns NULL on failure. `cache_dir`
 * (UTF-8 path, or NULL/empty) + info.cache_budget_bytes enable the persistent on-disk tile cache. The
 * returned world advances on its own Rust stream thread — drive it with set_view + poll (below). */
LogiGlobeWorld* logi_globe_world_create(const LogiGlobeCreateInfo* info, const char* ion_token, const char* cache_dir);

/* The camera view pushed each frame (UE space). forward(dir) + up + aspect + fov build the view frustum for
 * frustum-aware streaming LOD (in-view full detail, off-view a coarse shell — never hard-culled by direction,
 * so the engine still casts off-screen shadows). dir/up = 0,0,0 => unknown (accept-all frustum, no view
 * weighting). 13 doubles = 104 bytes; mirror this layout byte-for-byte with the Rust LogiGlobeView. */
typedef struct LogiGlobeView {
    double camera_x_cm, camera_y_cm, camera_z_cm; /* camera position (UE cm) */
    double dir_x, dir_y, dir_z;                   /* camera forward (UE) */
    double up_x, up_y, up_z;                       /* camera up (UE) */
    double aspect;                                 /* viewport width / height */
    double fov_y_radians;
    double viewport_height;
    double max_sse_pixels;
} LogiGlobeView;

/* ADR-0012 poll-based FFI. set_view pushes the newest camera (newest-wins; cheap, non-blocking; NULL view =>
 * NullArg); the stream thread advances against it off the game thread. poll writes the latest published frame
 * to out_frame (non-blocking — an empty frame before the first one); the frame's tile buffers are zero-copy
 * views into Rust-owned geometry, valid until the next poll() on this world. */
LogiStatus      logi_globe_world_set_view(LogiGlobeWorld* world, const LogiGlobeView* view);
LogiStatus      logi_globe_world_poll(LogiGlobeWorld* world, LogiGlobeFrame* out_frame);
uint64_t        logi_globe_world_cached_bytes(const LogiGlobeWorld* world); /* disk-cache resident bytes (telemetry) */
void            logi_globe_world_destroy(LogiGlobeWorld* world);

/* --- Baked semantic-tile world (ADR-0011, Milestone HE-0) — additive; see src/ffi_baked.rs. Opens an
 * offline-baked tile store (the Tools/logi_bake output) and returns every tile's mesh in Unreal render
 * space (cm, Z-up LH) as zero-copy views valid until destroy. HE-0 = terrain only, whole AOI served
 * statically (no camera streaming yet). New symbols only — the terrain LogiFramePacket ABI is unchanged. --- */

typedef struct LogiBakedWorld LogiBakedWorld; /* opaque */

typedef struct LogiBakedTile {
    uint64_t       tile_key;          /* stable TileId key — the C++ section-cache key */
    const uint8_t* vertices;          /* vertex_count * 32-byte vertices (pos f3 | normal f3 | uv f2), UE cm */
    uint64_t       vertices_len;
    uint32_t       vertex_count;
    uint32_t       vertex_stride;     /* = 32 */
    const uint8_t* indices;           /* index_count little-endian uint32 indices */
    uint32_t       index_count;
    uint32_t       _pad;
    float          bounds_center[3];  /* UE render space (cm) */
    float          bounds_radius;
} LogiBakedTile;

typedef struct LogiBakedFrame {
    const LogiBakedTile* tiles;       /* -> world-owned array, valid until logi_baked_world_destroy */
    uint32_t             tile_count;
    uint32_t             _pad;
    double               origin_ecef[3]; /* ENU origin's WGS84 ECEF (metres) — globe/atmosphere placement */
} LogiBakedFrame;

typedef struct LogiBakedCreateInfo {
    uint32_t meshing_mode;     /* the meshing toggle: 0 = uniform grid, 1 = adaptive TIN */
    float    tin_max_error_m;  /* TIN max vertical error (metres); ignored when uniform */
    float    skirt_depth_m;    /* downward edge-skirt depth (metres); 0 = none (hides cross-tile cracks) */
} LogiBakedCreateInfo;

/* Procedural-world params (the one-engine procedural lane): build terrain from the canonical fractal field
 * gameplay shares — no store on disk. seed==0 => the canonical gameplay field; else a custom world. The AOI
 * is the 2*radius_m square at the origin (lon/lat degrees, height metres), a pyramid of levels 0..leaf_level
 * at grid_n^2 samples/tile. Meshing fields mirror LogiBakedCreateInfo. */
typedef struct LogiBakedProceduralInfo {
    uint64_t seed;
    double   origin_lon_deg;
    double   origin_lat_deg;
    double   origin_height_m;
    double   radius_m;
    uint32_t leaf_level;
    uint32_t grid_n;
    uint32_t meshing_mode;     /* the meshing toggle: 0 = uniform grid, 1 = adaptive TIN */
    float    tin_max_error_m;  /* TIN max vertical error (metres); ignored when uniform */
    float    skirt_depth_m;    /* downward edge-skirt depth (metres); 0 = none */
} LogiBakedProceduralInfo;

/* Open a baked store directory (UTF-8 path) with the meshing mode in `info` (NULL = uniform). Blocking
 * (reads + triangulates the AOI) — call off the render thread. Returns NULL on a null/non-UTF-8 path or an
 * open/decode failure. */
LogiBakedWorld* logi_baked_world_create(const char* store_dir_utf8, const LogiBakedCreateInfo* info);
/* Create a PROCEDURAL terrain world (no store on disk): generate the SSE-LOD pyramid from the canonical
 * fractal field. Identical handle to a store-backed world (step/select/get_frame/destroy). Blocking — call
 * off the render thread. Returns NULL on a null `info` or a build failure. */
LogiBakedWorld* logi_baked_world_create_procedural(const LogiBakedProceduralInfo* info);
LogiStatus      logi_baked_world_get_frame(const LogiBakedWorld* world, LogiBakedFrame* out_frame);
/* Camera-driven frame: distance-culls to tiles within `max_distance_cm` of the camera (Unreal cm; <= 0 =
 * no cull). Returned tiles are valid until destroy or the next step/get_frame on this world. */
LogiStatus      logi_baked_world_step(LogiBakedWorld* world, float camera_x_cm, float camera_y_cm,
                                      float camera_z_cm, float max_distance_cm, LogiBakedFrame* out_frame);
/* SSE-LOD frame: descends the baked pyramid, coarse-far / fine-near, refining by screen-space error
 * (sse_target_px). Use this for a pyramid store (a single-level store renders everything). */
LogiStatus      logi_baked_world_select(LogiBakedWorld* world, float camera_x_cm, float camera_y_cm,
                                        float camera_z_cm, float fov_y_radians, float viewport_height,
                                        float sse_target_px, LogiBakedFrame* out_frame);
void            logi_baked_world_destroy(LogiBakedWorld* world);

#ifdef __cplusplus
} /* extern "C" */
#endif

#endif /* LOGI_GEO_FFI_H */
