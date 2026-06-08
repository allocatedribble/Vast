// C ABI shim over the vendored Google Draco decoder (Apache-2.0, vendor/draco/, v1.5.7) — the ONE
// C++ FFI boundary ADR-0009 reserves for Draco. Pure C interface (extern "C", no C++ types, no
// exceptions cross it): Rust hands raw tile bytes + the glTF KHR_draco_mesh_compression attribute
// unique-ids, the shim decodes the in-memory draco Mesh and writes plain float/uint32 attribute
// arrays that mirror what logi_gltf produces for uncompressed primitives.
//
// Ownership: every non-null pointer in LogiDracoMesh is heap-allocated by logi_draco_decode and MUST
// be released with exactly one logi_draco_free(&mesh). All fns are reentrant (no global state).
#ifndef LOGI_DRACO_SHIM_H_
#define LOGI_DRACO_SHIM_H_

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

// Status codes returned by logi_draco_decode. 0 == success; everything else leaves *out_mesh zeroed.
typedef enum LogiDracoStatus {
    LOGI_DRACO_OK = 0,
    LOGI_DRACO_ERR_EMPTY_INPUT = 1,       // null/zero-length buffer
    LOGI_DRACO_ERR_NOT_A_MESH = 2,        // decoded geometry is a point cloud, not a triangle mesh
    LOGI_DRACO_ERR_DECODE_FAILED = 3,     // draco returned a non-OK Status (corrupt/unsupported stream)
    LOGI_DRACO_ERR_MISSING_POSITION = 4,  // POSITION attribute id not present in the decoded mesh
    LOGI_DRACO_ERR_TOO_LARGE = 5,         // vertex/index count exceeds the caller-supplied cap (OOM guard)
    LOGI_DRACO_ERR_ALLOC = 6,             // an output allocation failed
    LOGI_DRACO_ERR_EXCEPTION = 7,         // a C++ exception escaped the decode (defensive; should not happen)
} LogiDracoStatus;

// Decoded attribute arrays. Counts are in ELEMENTS, not bytes. `normals`/`texcoords` are null when the
// caller passed a negative id or the attribute was absent; `positions` and `indices` are always present
// on success. Component layout matches logi_gltf: position f32x3, normal f32x3, texcoord f32x2.
typedef struct LogiDracoMesh {
    float *positions;    // num_vertices * 3 floats (x,y,z)
    float *normals;      // num_vertices * 3 floats, or null
    float *texcoords;    // num_vertices * 2 floats (u,v), or null
    uint32_t *indices;   // num_indices uint32 (3 per triangle)
    size_t num_vertices; // vertex count (point count of the decoded mesh)
    size_t num_indices;  // index count == num_faces * 3
} LogiDracoMesh;

// Decode a Draco-compressed buffer into flat attribute arrays.
//
// data/data_len           : the raw Draco bitstream (the glTF bufferView slice). Untrusted/network input.
// pos_id/normal_id/uv_id  : glTF KHR_draco_mesh_compression `attributes` unique-ids. Pass a NEGATIVE
//                           value for an attribute the primitive does not declare (normals/uv optional).
//                           pos_id is required (>= 0) — POSITION is mandatory in the extension.
// max_vertices/max_indices: hard caps; if the decoded mesh exceeds either, returns ERR_TOO_LARGE and
//                           allocates nothing (the caller bounds memory against hostile tiles).
// out_mesh                : zeroed on any error; on OK its pointers are owned by the caller.
//
// Returns a LogiDracoStatus. Never throws (all C++ exceptions are contained).
LogiDracoStatus logi_draco_decode(const uint8_t *data, size_t data_len, int32_t pos_id, int32_t normal_id,
                                  int32_t uv_id, size_t max_vertices, size_t max_indices, LogiDracoMesh *out_mesh);

// Release every buffer owned by *mesh and zero the struct. Safe to call on an already-zeroed struct and
// safe to call exactly once after a successful decode. Null `mesh` is a no-op.
void logi_draco_free(LogiDracoMesh *mesh);

#ifdef __cplusplus
}  // extern "C"
#endif

#endif  // LOGI_DRACO_SHIM_H_
