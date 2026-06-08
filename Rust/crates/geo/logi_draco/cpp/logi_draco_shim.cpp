// C ABI shim over the vendored Google Draco decoder. See logi_draco_shim.h for the contract.
//
// Design notes:
//  * The whole body runs under a try/catch that maps any escaping C++ exception to a status code —
//    a C++ exception unwinding across the `extern "C"` boundary into Rust is UB, and the Rust caller
//    is compiled `panic = "abort"`, so containment here is mandatory (hostile-input discipline).
//  * Vertex/index counts are validated against the caller's caps BEFORE any output allocation, so a
//    crafted "huge mesh" header cannot drive an unbounded malloc.
//  * Attribute values are pulled with PointAttribute::ConvertValue<float>, which dequantizes and
//    type-converts whatever Draco stored (int16 quantized positions, octahedral normals, ...) into
//    the plain f32 components logi_gltf expects. Fewer stored components than requested are zero-filled
//    by Draco; we always request the glTF-canonical 3/3/2.

#include "logi_draco_shim.h"

#include <cstdlib>
#include <cstring>
#include <memory>
#include <new>

#include "draco/attributes/point_attribute.h"
#include "draco/compression/decode.h"
#include "draco/core/decoder_buffer.h"
#include "draco/mesh/mesh.h"

namespace {

// Pull `components` float values for every point of `att` into `out` (point i -> out[i*components..]).
// `out` must hold mesh->num_points() * components floats. Returns false if any per-point conversion
// fails (treated as a decode error by the caller). `att` is assumed non-null.
bool ExtractFloatAttribute(const draco::PointAttribute *att, size_t num_points, int components, float *out) {
    for (size_t i = 0; i < num_points; ++i) {
        const draco::PointIndex pi(static_cast<uint32_t>(i));
        const draco::AttributeValueIndex avi = att->mapped_index(pi);
        if (!att->ConvertValue<float>(avi, components, out + i * static_cast<size_t>(components))) {
            return false;
        }
    }
    return true;
}

// Typed malloc that returns null on overflow or OOM. count==0 yields null (we only call it for >0).
template <typename T>
T *AllocArray(size_t count) {
    if (count == 0 || count > SIZE_MAX / sizeof(T)) {
        return nullptr;
    }
    return static_cast<T *>(std::malloc(count * sizeof(T)));
}

}  // namespace

extern "C" LogiDracoStatus logi_draco_decode(const uint8_t *data, size_t data_len, int32_t pos_id,
                                             int32_t normal_id, int32_t uv_id, size_t max_vertices,
                                             size_t max_indices, LogiDracoMesh *out_mesh) {
    if (out_mesh == nullptr) {
        return LOGI_DRACO_ERR_EMPTY_INPUT;
    }
    std::memset(out_mesh, 0, sizeof(*out_mesh));
    if (data == nullptr || data_len == 0) {
        return LOGI_DRACO_ERR_EMPTY_INPUT;
    }

    try {
        draco::DecoderBuffer buffer;
        // Draco does not take ownership; `data` only needs to outlive this call (it does — Rust holds it).
        buffer.Init(reinterpret_cast<const char *>(data), data_len);

        // Reject anything that isn't a triangle mesh up front (point clouds decode via a different path
        // and have no faces -> no indices for the renderer).
        const draco::StatusOr<draco::EncodedGeometryType> type_or =
            draco::Decoder::GetEncodedGeometryType(&buffer);
        if (!type_or.ok() || type_or.value() != draco::TRIANGULAR_MESH) {
            return type_or.ok() ? LOGI_DRACO_ERR_NOT_A_MESH : LOGI_DRACO_ERR_DECODE_FAILED;
        }

        draco::Decoder decoder;
        draco::StatusOr<std::unique_ptr<draco::Mesh>> mesh_or = decoder.DecodeMeshFromBuffer(&buffer);
        if (!mesh_or.ok() || mesh_or.value() == nullptr) {
            return LOGI_DRACO_ERR_DECODE_FAILED;
        }
        const std::unique_ptr<draco::Mesh> mesh = std::move(mesh_or).value();

        const size_t num_points = static_cast<size_t>(mesh->num_points());
        const size_t num_faces = static_cast<size_t>(mesh->num_faces());
        // num_indices = num_faces * 3; guard the multiply against overflow, then both caps.
        if (num_faces > SIZE_MAX / 3) {
            return LOGI_DRACO_ERR_TOO_LARGE;
        }
        const size_t num_indices = num_faces * 3;
        if (num_points == 0 || num_points > max_vertices || num_indices > max_indices) {
            return LOGI_DRACO_ERR_TOO_LARGE;
        }

        // POSITION is mandatory. Resolve it (and the optional normal/uv) by the glTF unique-ids.
        if (pos_id < 0) {
            return LOGI_DRACO_ERR_MISSING_POSITION;
        }
        const draco::PointAttribute *pos_att =
            mesh->GetAttributeByUniqueId(static_cast<uint32_t>(pos_id));
        if (pos_att == nullptr) {
            return LOGI_DRACO_ERR_MISSING_POSITION;
        }
        const draco::PointAttribute *normal_att =
            normal_id < 0 ? nullptr : mesh->GetAttributeByUniqueId(static_cast<uint32_t>(normal_id));
        const draco::PointAttribute *uv_att =
            uv_id < 0 ? nullptr : mesh->GetAttributeByUniqueId(static_cast<uint32_t>(uv_id));

        // Allocate outputs (positions + indices always; normals/uv only when present). On any failure,
        // free what we already took and report ERR_ALLOC so we never leak across the boundary.
        LogiDracoMesh result;
        std::memset(&result, 0, sizeof(result));
        result.num_vertices = num_points;
        result.num_indices = num_indices;
        result.positions = AllocArray<float>(num_points * 3);
        result.indices = AllocArray<uint32_t>(num_indices);
        if (result.positions == nullptr || result.indices == nullptr) {
            logi_draco_free(&result);
            return LOGI_DRACO_ERR_ALLOC;
        }
        if (normal_att != nullptr) {
            result.normals = AllocArray<float>(num_points * 3);
            if (result.normals == nullptr) {
                logi_draco_free(&result);
                return LOGI_DRACO_ERR_ALLOC;
            }
        }
        if (uv_att != nullptr) {
            result.texcoords = AllocArray<float>(num_points * 2);
            if (result.texcoords == nullptr) {
                logi_draco_free(&result);
                return LOGI_DRACO_ERR_ALLOC;
            }
        }

        // Fill attributes. A conversion failure means the stream/attribute is malformed -> decode error.
        if (!ExtractFloatAttribute(pos_att, num_points, 3, result.positions) ||
            (normal_att != nullptr && !ExtractFloatAttribute(normal_att, num_points, 3, result.normals)) ||
            (uv_att != nullptr && !ExtractFloatAttribute(uv_att, num_points, 2, result.texcoords))) {
            logi_draco_free(&result);
            return LOGI_DRACO_ERR_DECODE_FAILED;
        }

        // Flatten faces to a u32 index buffer. Every PointIndex is < num_points by construction of the
        // decoded mesh, so these are in-range for the position array (the Rust side re-checks anyway).
        for (size_t f = 0; f < num_faces; ++f) {
            const draco::Mesh::Face &face = mesh->face(draco::FaceIndex(static_cast<uint32_t>(f)));
            result.indices[f * 3 + 0] = face[0].value();
            result.indices[f * 3 + 1] = face[1].value();
            result.indices[f * 3 + 2] = face[2].value();
        }

        *out_mesh = result;
        return LOGI_DRACO_OK;
    } catch (const std::bad_alloc &) {
        std::memset(out_mesh, 0, sizeof(*out_mesh));
        return LOGI_DRACO_ERR_ALLOC;
    } catch (...) {
        std::memset(out_mesh, 0, sizeof(*out_mesh));
        return LOGI_DRACO_ERR_EXCEPTION;
    }
}

extern "C" void logi_draco_free(LogiDracoMesh *mesh) {
    if (mesh == nullptr) {
        return;
    }
    std::free(mesh->positions);
    std::free(mesh->normals);
    std::free(mesh->texcoords);
    std::free(mesh->indices);
    std::memset(mesh, 0, sizeof(*mesh));
}
