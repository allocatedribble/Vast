//! Draco mesh decompression for the geo 3D-Tiles stack.
//!
//! Google Photorealistic 3D Tiles and Cesium World Terrain ship their fine-LOD glTF tiles with geometry
//! compressed via the `KHR_draco_mesh_compression` extension; the uncompressed-glTF decoder (`logi_gltf`)
//! cannot read those primitives, so the planet never reaches full detail. This crate fills exactly that
//! gap: it decodes a Draco bitstream + the extension's attribute→unique-id mapping into the SAME plain
//! attribute arrays `logi_gltf` already produces for uncompressed primitives (positions `f32`×3, normals
//! `f32`×3, texcoords `f32`×2, indices `u32`), so the integration is a drop-in for `logi_gltf`'s Draco
//! branch.
//!
//! ## Why a C++ FFI (ADR-0009's reserved exception)
//! There is no usable pure-Rust Draco *decoder* (the only pure-Rust crate, `draco-oxide`, has its decoder
//! unimplemented/WIP; `draco_decoder`/`draco-rs` are themselves C++ FFI wrappers). So we take ADR-0009's
//! one reserved vendored-Draco exception: the Google Draco **decoder subset** (Apache-2.0, v1.5.7) is
//! vendored under `vendor/draco/` and built by `build.rs` with `cc` behind a minimal C ABI shim
//! (`cpp/logi_draco_shim.{h,cpp}`). Rust never exposes any Draco/C++ type; the boundary is a flat C struct.
//!
//! ## Hostile-input discipline
//! Tile bytes arrive from untrusted network sources and the shipping build is `panic = "abort"`, so this
//! crate must never panic or over-allocate on malformed input. The shim contains every C++ exception
//! (mapping it to a status code — unwinding across the C ABI would be UB), validates vertex/index counts
//! against caller-supplied caps *before* allocating, and the safe wrapper re-checks index ranges. Bad
//! bytes become a typed [`DracoError`], never a crash.
//!
//! ## Integrating into `logi_gltf` (the `KHR_draco_mesh_compression` primitive branch)
//! `logi_gltf` is NOT edited by this crate; this is the spec for wiring it in. A glTF primitive that uses
//! the extension has its real geometry in `primitive.extensions.KHR_draco_mesh_compression`, NOT in the
//! usual accessor `bufferView`s — the extension JSON is `{ "bufferView": N, "attributes": { "POSITION":
//! id, "NORMAL": id, "TEXCOORD_0": id, ... } }`, where each id is a *Draco* attribute unique-id (distinct
//! from the glTF accessor ids). `decode_mesh_primitives` (in `logi_gltf/src/lib.rs`) should, per primitive,
//! BEFORE its current `reader.read_positions()` path:
//!
//! 1. Add the dep: `logi_draco = { path = "../logi_draco" }`, and enable the `gltf` crate's `extensions`
//!    feature in `logi_gltf/Cargo.toml` (it is currently off) so `Primitive::extension_value` is populated.
//! 2. `let Some(ext) = prim.extension_value("KHR_draco_mesh_compression") else { /* fall through to the
//!    existing uncompressed path */ };` — `ext` is a `&serde_json::Value`.
//! 3. Read `let bv = ext["bufferView"].as_u64()?;` and the attribute ids from `ext["attributes"]`:
//!    `position = attrs["POSITION"].as_u64()? as u32`, `normal = attrs.get("NORMAL")...`, `texcoord =
//!    attrs.get("TEXCOORD_0")...` into a [`DracoAttributeIds`] (POSITION required; NORMAL/TEXCOORD optional).
//! 4. Slice the Draco bitstream from the blob via that bufferView:
//!    `let view = gltf.document.views().nth(bv as usize)?;` then (mirroring the existing image-blob slice)
//!    `let start = view.offset(); let end = start.checked_add(view.length())?;
//!    let drc = blob?.get(start..end)?;` — only when `view.buffer().source()` is `Bin` (else skip, like the
//!    uncompressed path treats non-BIN buffers).
//! 5. `let m = logi_draco::decode_mesh(drc, &ids, DecodeLimits::default())` — on `Err`, `continue` (drop the
//!    primitive, bump `decode_failed`), exactly as the uncompressed path drops an unreadable primitive.
//! 6. Build the vertices the SAME way: `interleave(&m.positions, m.normals.as_deref(), m.texcoords.as_deref())`,
//!    and use `m.indices` directly (already range-checked here). Then push the `DecodedPrimitive` with the
//!    material/`node_transform` resolved exactly as the uncompressed branch does. Indices are non-indexed-safe:
//!    Draco mesh decode always yields faces, so `m.indices` is always populated.
//!
//! Net: the Draco branch produces byte-identical `DecodedPrimitive` shapes to the uncompressed branch — the
//! only difference is WHERE positions/normals/uv/indices come from. The default [`DecodeLimits`] equals
//! `logi_gltf`'s own `MAX_VERTICES_PER_PRIMITIVE` / `MAX_INDICES_PER_PRIMITIVE`, so both paths bound memory
//! identically.

// FFI is required (no pure-Rust decoder exists); we cannot use `#![forbid(unsafe_code)]`. Unsafe is
// confined to the `ffi` module and the one extraction function, and every `unsafe` block is justified.
#![deny(unsafe_op_in_unsafe_fn)]

use std::os::raw::c_int;

/// What went wrong decoding a Draco buffer. Every variant is a *handled* condition — a malformed/hostile
/// tile yields one of these, never a panic. The integrator treats any `Err` as a dropped primitive
/// (counted as a `decode_failed`), exactly like `logi_gltf` drops an unreadable uncompressed primitive.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum DracoError {
    /// The input buffer was empty (zero-length bitstream).
    #[error("empty Draco buffer")]
    EmptyInput,
    /// The bitstream decoded to a point cloud, not a triangle mesh (3D-Tiles geometry must be a mesh).
    #[error("Draco stream is not a triangle mesh")]
    NotAMesh,
    /// The Draco decoder rejected the bitstream (corrupt, truncated, or an unsupported encoding).
    #[error("Draco decode failed (corrupt or unsupported bitstream)")]
    DecodeFailed,
    /// The declared POSITION attribute id was not present in the decoded mesh.
    #[error("Draco mesh is missing its POSITION attribute (id {0})")]
    MissingPosition(u32),
    /// The decoded mesh exceeded the caller's vertex or index cap (OOM guard against hostile sizes).
    #[error("Draco mesh exceeds the allocation cap (vertices or indices too large)")]
    TooLarge,
    /// An output allocation failed (out of memory).
    #[error("allocation failed while decoding Draco mesh")]
    Alloc,
    /// A C++ exception escaped the decoder (defensive; the shim contains it). Treated as a decode failure.
    #[error("internal Draco decoder error")]
    Internal,
    /// An index in the decoded mesh referenced a vertex that does not exist (corrupt connectivity). The
    /// safe wrapper rejects this so the renderer never receives an out-of-range index (driver UB).
    #[error("Draco mesh has an out-of-range vertex index")]
    BadIndex,
}

/// The `KHR_draco_mesh_compression` attribute→unique-id mapping for ONE primitive, taken straight from
/// the glTF extension JSON (`primitive.extensions.KHR_draco_mesh_compression.attributes`). POSITION is
/// mandatory in the extension; NORMAL/TEXCOORD_0 are optional (absent ⇒ `None`, and the corresponding
/// output array is `None`). The integrator reads these ids from the glTF and passes them here verbatim —
/// they are *Draco* attribute ids, distinct from the glTF accessor ids.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DracoAttributeIds {
    /// Draco unique-id of the POSITION attribute (required).
    pub position: u32,
    /// Draco unique-id of the NORMAL attribute, if the primitive declares one.
    pub normal: Option<u32>,
    /// Draco unique-id of the TEXCOORD_0 attribute, if the primitive declares one.
    pub texcoord: Option<u32>,
}

/// Hard caps on the decoded mesh, applied *before* allocation, so a crafted "huge mesh" header cannot
/// drive an unbounded allocation (the bytes themselves are already capped upstream by the fetch layer).
/// [`DecodeLimits::default`] mirrors `logi_gltf`'s per-primitive ceilings so a Draco primitive and an
/// uncompressed one are bounded identically.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DecodeLimits {
    /// Maximum vertices (points) in the decoded mesh.
    pub max_vertices: usize,
    /// Maximum indices (== faces × 3) in the decoded mesh.
    pub max_indices: usize,
}

impl Default for DecodeLimits {
    fn default() -> Self {
        // Identical to logi_gltf's MAX_VERTICES_PER_PRIMITIVE / MAX_INDICES_PER_PRIMITIVE: a 64 MiB tile
        // (the fetch cap) cannot legitimately exceed these, so anything larger is corrupt/hostile.
        Self {
            max_vertices: 1 << 23, // ~8.4M
            max_indices: 1 << 24,  // ~16.8M
        }
    }
}

/// A decoded Draco mesh as plain attribute arrays — the SAME shapes `logi_gltf`'s `DecodedPrimitive`
/// is built from. Feed these straight into `logi_geo_mesh::interleave(&positions, normals.as_deref(),
/// texcoords.as_deref())` and use `indices` as-is. `normals`/`texcoords` are `Some` iff the primitive
/// declared that attribute (and it decoded); when `None`, `interleave` applies the same defaults it uses
/// for an uncompressed primitive that omits them.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DracoMesh {
    /// Vertex positions, one `[x, y, z]` per vertex.
    pub positions: Vec<[f32; 3]>,
    /// Vertex normals, present iff the primitive declared a NORMAL attribute.
    pub normals: Option<Vec<[f32; 3]>>,
    /// Texture coordinates (UV set 0), present iff the primitive declared a TEXCOORD_0 attribute.
    pub texcoords: Option<Vec<[f32; 2]>>,
    /// Triangle indices (3 per face), every value `< positions.len()`.
    pub indices: Vec<u32>,
}

/// The C ABI surface of the vendored-Draco shim. Mirrors `cpp/logi_draco_shim.h` exactly. All of this is
/// `unsafe` to touch; the safe wrapper [`decode_mesh`] is the only public entry point.
mod ffi {
    use std::os::raw::c_int;

    /// Mirror of `LogiDracoMesh`. Pointers are owned by the shim until `logi_draco_free`.
    #[repr(C)]
    pub struct LogiDracoMesh {
        pub positions: *mut f32,
        pub normals: *mut f32,
        pub texcoords: *mut f32,
        pub indices: *mut u32,
        pub num_vertices: usize,
        pub num_indices: usize,
    }

    extern "C" {
        /// Decode a Draco buffer into flat arrays. Returns a `LogiDracoStatus` (0 == OK). See the header.
        pub fn logi_draco_decode(
            data: *const u8,
            data_len: usize,
            pos_id: c_int,
            normal_id: c_int,
            uv_id: c_int,
            max_vertices: usize,
            max_indices: usize,
            out_mesh: *mut LogiDracoMesh,
        ) -> c_int;

        /// Release every buffer owned by `*mesh` and zero it. Safe to call once after a successful decode.
        pub fn logi_draco_free(mesh: *mut LogiDracoMesh);
    }
}

// Status codes — must match the `LogiDracoStatus` enum in cpp/logi_draco_shim.h.
const LOGI_DRACO_OK: c_int = 0;
const LOGI_DRACO_ERR_EMPTY_INPUT: c_int = 1;
const LOGI_DRACO_ERR_NOT_A_MESH: c_int = 2;
const LOGI_DRACO_ERR_DECODE_FAILED: c_int = 3;
const LOGI_DRACO_ERR_MISSING_POSITION: c_int = 4;
const LOGI_DRACO_ERR_TOO_LARGE: c_int = 5;
const LOGI_DRACO_ERR_ALLOC: c_int = 6;
const LOGI_DRACO_ERR_EXCEPTION: c_int = 7;

/// Map a glTF attribute id to the C ABI's signed convention: a present id must fit in a positive
/// `c_int` (Draco unique-ids are small), and "absent" is any negative value (we use -1). An id too
/// large for `c_int` cannot be a real Draco id, so we treat it as absent rather than wrapping it.
fn id_to_c(id: Option<u32>) -> c_int {
    match id {
        Some(v) if v <= c_int::MAX as u32 => v as c_int,
        _ => -1,
    }
}

/// Decode a `KHR_draco_mesh_compression` primitive into plain attribute arrays.
///
/// `data` is the Draco bitstream (the glTF bufferView slice for the extension). `ids` is the extension's
/// attribute→unique-id map for this primitive. `limits` bounds the decoded size (use
/// [`DecodeLimits::default`] to match `logi_gltf`).
///
/// On success the returned [`DracoMesh`] has `positions` and `indices` populated, `normals`/`texcoords`
/// populated iff `ids` declared them, every index `< positions.len()`, and all position components
/// finite-by-construction (Draco emits finite floats). On any malformed/oversized/corrupt input it
/// returns a typed [`DracoError`] — never panics, never over-allocates.
pub fn decode_mesh(data: &[u8], ids: &DracoAttributeIds, limits: DecodeLimits) -> Result<DracoMesh, DracoError> {
    if data.is_empty() {
        return Err(DracoError::EmptyInput);
    }
    // POSITION is mandatory and must fit the signed C convention; an id past c_int::MAX is not a real
    // Draco id, so the shim would not find it — report MissingPosition rather than silently wrapping.
    let pos_c = id_to_c(Some(ids.position));
    if pos_c < 0 {
        return Err(DracoError::MissingPosition(ids.position));
    }

    let mut raw = ffi::LogiDracoMesh {
        positions: std::ptr::null_mut(),
        normals: std::ptr::null_mut(),
        texcoords: std::ptr::null_mut(),
        indices: std::ptr::null_mut(),
        num_vertices: 0,
        num_indices: 0,
    };

    // SAFETY: `data` is a valid slice for `data.len()` bytes for the duration of the call (Draco only
    // borrows it). `&mut raw` is a valid, properly-aligned, zeroed `LogiDracoMesh`. The id args use the
    // negative-means-absent convention the shim documents. The shim never throws across this boundary.
    let status = unsafe {
        ffi::logi_draco_decode(
            data.as_ptr(),
            data.len(),
            pos_c,
            id_to_c(ids.normal),
            id_to_c(ids.texcoord),
            limits.max_vertices,
            limits.max_indices,
            &mut raw,
        )
    };

    if status != LOGI_DRACO_OK {
        // On a non-OK status the shim guarantees `raw` is zeroed (nothing to free), so just translate.
        return Err(match status {
            LOGI_DRACO_ERR_EMPTY_INPUT => DracoError::EmptyInput,
            LOGI_DRACO_ERR_NOT_A_MESH => DracoError::NotAMesh,
            LOGI_DRACO_ERR_MISSING_POSITION => DracoError::MissingPosition(ids.position),
            LOGI_DRACO_ERR_TOO_LARGE => DracoError::TooLarge,
            LOGI_DRACO_ERR_ALLOC => DracoError::Alloc,
            LOGI_DRACO_ERR_DECODE_FAILED | LOGI_DRACO_ERR_EXCEPTION => DracoError::DecodeFailed,
            _ => DracoError::Internal,
        });
    }

    // Success: copy the shim-owned arrays into owned Vecs, then free the shim allocation. We convert the
    // result into a `DracoMesh` and *always* free `raw` before returning (including on the BadIndex path).
    let result = copy_out(&raw, ids);

    // SAFETY: `raw` was filled by a successful `logi_draco_decode`, so every non-null pointer is a shim
    // allocation freed exactly once here. After this call `raw` is zeroed; we do not touch it again.
    unsafe { ffi::logi_draco_free(&mut raw) };

    result
}

/// Copy the shim's flat buffers into owned, typed Vecs and validate index ranges. Does NOT free `raw`
/// (the caller does, unconditionally). Separated so the `?` early-returns here still let the caller free.
fn copy_out(raw: &ffi::LogiDracoMesh, ids: &DracoAttributeIds) -> Result<DracoMesh, DracoError> {
    let nv = raw.num_vertices;
    let ni = raw.num_indices;

    // A successful decode always has positions + indices; treat a null/zero here as an internal fault
    // rather than trusting it (defensive — the shim does not return OK in that state).
    if raw.positions.is_null() || nv == 0 {
        return Err(DracoError::Internal);
    }

    // SAFETY: on OK, `positions` points to `nv*3` initialized f32 (shim contract). We read, not retain.
    let pos_flat = unsafe { std::slice::from_raw_parts(raw.positions, nv * 3) };
    let positions: Vec<[f32; 3]> = pos_flat.chunks_exact(3).map(|c| [c[0], c[1], c[2]]).collect();

    let normals = if ids.normal.is_some() && !raw.normals.is_null() {
        // SAFETY: when a normal id was requested and the pointer is non-null, the shim allocated nv*3 f32.
        let n = unsafe { std::slice::from_raw_parts(raw.normals, nv * 3) };
        Some(n.chunks_exact(3).map(|c| [c[0], c[1], c[2]]).collect())
    } else {
        None
    };

    let texcoords = if ids.texcoord.is_some() && !raw.texcoords.is_null() {
        // SAFETY: when a uv id was requested and the pointer is non-null, the shim allocated nv*2 f32.
        let t = unsafe { std::slice::from_raw_parts(raw.texcoords, nv * 2) };
        Some(t.chunks_exact(2).map(|c| [c[0], c[1]]).collect())
    } else {
        None
    };

    let indices: Vec<u32> = if ni == 0 || raw.indices.is_null() {
        Vec::new()
    } else {
        // SAFETY: on OK with ni>0, `indices` points to `ni` initialized u32 (shim contract).
        let idx = unsafe { std::slice::from_raw_parts(raw.indices, ni) };
        // Defense in depth: reject any out-of-range index so a corrupt mesh never hands the renderer an
        // OOB vertex fetch (matches logi_gltf's OOB-index guard). O(indices) — one pass we already make.
        if idx.iter().any(|&i| i as usize >= positions.len()) {
            return Err(DracoError::BadIndex);
        }
        idx.to_vec()
    };

    Ok(DracoMesh {
        positions,
        normals,
        texcoords,
        indices,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // A real Draco fixture vendored from Google Draco's testdata (Apache-2.0): `cube_att.drc` is a cube
    // (8 verts after dedup, 12 triangles) with POSITION (unique-id 0) and a generic attribute, no
    // normals/uv. `test_nm.drc` is a mesh with POSITION + NORMAL. Bytes embedded at compile time so the
    // test is hermetic (no testdata path at runtime).
    const CUBE_DRC: &[u8] = include_bytes!("../tests/fixtures/cube_att.drc");
    const NORMALS_DRC: &[u8] = include_bytes!("../tests/fixtures/test_nm.edgebreaker.drc");

    fn all_finite3(v: &[[f32; 3]]) -> bool {
        v.iter().all(|p| p.iter().all(|c| c.is_finite()))
    }

    #[test]
    fn decodes_cube_positions_and_indices() {
        let ids = DracoAttributeIds {
            position: 0,
            normal: None,
            texcoord: None,
        };
        let mesh = decode_mesh(CUBE_DRC, &ids, DecodeLimits::default()).expect("cube_att.drc should decode");

        // cube_att.drc carries a per-corner generic attribute, so its corners are NOT deduplicated to 8:
        // 6 faces × 4 corners = 24 distinct points, 12 triangles = 36 indices. (Verified by decoding the
        // vendored fixture — see the deleted examples/probe.rs.)
        assert_eq!(mesh.positions.len(), 24, "cube_att.drc decodes to 24 corner vertices");
        assert_eq!(mesh.indices.len(), 36, "cube has 12 triangles = 36 indices");
        assert_eq!(mesh.indices.len() % 3, 0, "indices form whole triangles");
        assert!(all_finite3(&mesh.positions), "positions must be finite");
        // No normal/uv id was requested => those arrays are absent (drop-in defaults apply downstream).
        assert!(mesh.normals.is_none());
        assert!(mesh.texcoords.is_none());
        // Every index is in range (also enforced by copy_out, asserted here for clarity).
        assert!(mesh.indices.iter().all(|&i| (i as usize) < mesh.positions.len()));

        // The decoded corners must span the real unit cube [0,0,0]→[1,1,1] (a non-degenerate box, not
        // all-zero) on every axis — proves positions were actually dequantized, not zero-filled.
        for axis in 0..3 {
            let lo = mesh.positions.iter().map(|p| p[axis]).fold(f32::INFINITY, f32::min);
            let hi = mesh.positions.iter().map(|p| p[axis]).fold(f32::NEG_INFINITY, f32::max);
            assert!(
                hi - lo > 0.5,
                "cube must have unit extent on axis {axis} (got {lo}..{hi})"
            );
        }
    }

    #[test]
    fn decodes_mesh_with_normals() {
        // POSITION is unique-id 0, NORMAL is unique-id 1 in Draco's test_nm fixtures.
        let ids = DracoAttributeIds {
            position: 0,
            normal: Some(1),
            texcoord: None,
        };
        let mesh = decode_mesh(NORMALS_DRC, &ids, DecodeLimits::default()).expect("test_nm should decode");

        // Verified counts for this fixture (edgebreaker cl10): 99 vertices, 170 triangles = 510 indices.
        assert_eq!(mesh.positions.len(), 99, "test_nm.edgebreaker.drc has 99 vertices");
        assert_eq!(
            mesh.indices.len(),
            510,
            "test_nm.edgebreaker.drc has 170 triangles = 510 indices"
        );
        assert_eq!(mesh.indices.len() % 3, 0, "indices form whole triangles");
        assert!(all_finite3(&mesh.positions), "positions finite");
        assert!(
            mesh.indices.iter().all(|&i| (i as usize) < mesh.positions.len()),
            "indices in range"
        );

        let normals = mesh
            .normals
            .expect("NORMAL id was requested and is present in the fixture");
        assert_eq!(normals.len(), mesh.positions.len(), "one normal per vertex");
        assert!(all_finite3(&normals), "normals finite");
        // Decoded normals are (approximately) unit length — sanity that octahedral decode produced a real
        // direction, not zeros. Allow a wide tolerance (quantization).
        assert!(
            normals.iter().any(|n| {
                let len = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt();
                (len - 1.0).abs() < 0.2
            }),
            "at least some decoded normals should be ~unit length"
        );
    }

    #[test]
    fn empty_input_is_rejected_not_panicking() {
        let ids = DracoAttributeIds {
            position: 0,
            normal: None,
            texcoord: None,
        };
        assert_eq!(
            decode_mesh(&[], &ids, DecodeLimits::default()),
            Err(DracoError::EmptyInput)
        );
    }

    #[test]
    fn garbage_bytes_fail_cleanly() {
        // Random non-Draco bytes must produce a typed error (not a panic, not UB). The leading bytes are
        // not Draco's "DRACO" magic, so GetEncodedGeometryType / decode rejects them.
        let ids = DracoAttributeIds {
            position: 0,
            normal: None,
            texcoord: None,
        };
        let junk = vec![0xABu8; 4096];
        let err = decode_mesh(&junk, &ids, DecodeLimits::default()).unwrap_err();
        assert!(
            matches!(err, DracoError::DecodeFailed | DracoError::NotAMesh),
            "garbage must be a clean decode error, got {err:?}"
        );
    }

    #[test]
    fn truncated_stream_fails_cleanly() {
        // A valid header followed by a cut-off body must not over-read or panic — just a decode error.
        let ids = DracoAttributeIds {
            position: 0,
            normal: None,
            texcoord: None,
        };
        let truncated = &CUBE_DRC[..CUBE_DRC.len() / 2];
        let err = decode_mesh(truncated, &ids, DecodeLimits::default()).unwrap_err();
        assert!(
            matches!(
                err,
                DracoError::DecodeFailed | DracoError::NotAMesh | DracoError::BadIndex
            ),
            "truncated stream must be a clean error, got {err:?}"
        );
    }

    #[test]
    fn vertex_cap_is_enforced() {
        // A cap below the cube's real vertex count must reject it (the OOM guard) rather than allocate.
        let ids = DracoAttributeIds {
            position: 0,
            normal: None,
            texcoord: None,
        };
        let tiny = DecodeLimits {
            max_vertices: 2,
            max_indices: 1 << 24,
        };
        assert_eq!(decode_mesh(CUBE_DRC, &ids, tiny), Err(DracoError::TooLarge));
    }

    #[test]
    fn wrong_position_id_reports_missing() {
        // An id the mesh doesn't contain must be reported as MissingPosition, not a crash.
        let ids = DracoAttributeIds {
            position: 999,
            normal: None,
            texcoord: None,
        };
        assert_eq!(
            decode_mesh(CUBE_DRC, &ids, DecodeLimits::default()),
            Err(DracoError::MissingPosition(999))
        );
    }
}
