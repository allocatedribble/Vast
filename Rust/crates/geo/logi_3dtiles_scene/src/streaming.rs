//! Shared **decode / rebase + content-classification helpers** for the Geo stack. The streaming *world*
//! itself is the `bevy_ecs` [`GeoWorld`](crate::geo_world::GeoWorld) (ADR-0012 Addendum A); this module is
//! the reusable, world-agnostic machinery it builds on:
//!
//! - [`rebase_gltf`] — fold a decoded glTF into one interleaved [`TileGeometry`] in Unreal render space
//!   (tile-local → ECEF → ENU-local → UE cm), the body of the Geo World's production decoder.
//! - [`ecef_to_local_affine`] — the one-matrix ECEF→ENU-local fold `rebase_gltf` applies per vertex.
//! - [`sniff_content_kind`] / [`is_tileset_uri`] — classify a fetched tile (GLB vs external `.json`).
//! - [`tile_at_mut`] — walk a child-index path into the (grafting) tileset tree.

use glam::{DMat4, DVec3};
use logi_3dtiles_tileset::Tile;
use logi_geo_mesh::{aabb_to_sphere, enu_dir_to_ue, enu_pos_to_ue};
use logi_geospatial::EnuFrame;

/// A streaming/tileset error.
#[derive(Clone, Debug)]
pub enum StreamError {
    Tileset(String),
}

/// What a fetched tile-content payload is: a renderable glTF tile, or a nested external `.json` tileset to
/// graft into the tree. Decided by [`sniff_content_kind`] (bytes), falling back to [`is_tileset_uri`] (URI).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ContentKind {
    Tileset, // a nested `.json` external tileset → fetch + graft into the tree
    Glb,     // a renderable glTF tile → fetch + decode
}

/// The immutable, shareable geometry of one decoded tile, in **Unreal render space** (cm, Z-up LH). The
/// ECEF geometry is rebased to ENU-local at the world origin, then converted to Unreal space (negate
/// North), so the C++ side uploads it with no coordinate math. It is **never mutated after decode** and is
/// held behind an [`Arc`](std::sync::Arc) so a published render frame can pin exactly the tiles it
/// references and let them outlive eviction from the resident set — the lifetime contract the decoupled
/// stream thread needs (ADR-0012). The renderer reads the buffers zero-copy via raw pointers that stay
/// valid for as long as any `Arc<TileGeometry>` to them lives.
pub struct TileGeometry {
    pub vertices: Vec<u8>, // 32-byte: pos f3 (Unreal cm) | normal f3 (Unreal) | uv f2
    pub indices: Vec<u32>,
    pub texture: Vec<u8>, // embedded image bytes (JPEG/PNG), or empty
    pub bounds_center: [f32; 3],
    pub bounds_radius: f32,
    /// `KHR_materials_unlit`: the tile's material bakes its own lighting (Google P3DT bakes sun/shadow into
    /// the albedo). Carried to the FFI (`LogiGlobeTile.flags` bit 0) so the renderer can shade it unlit and
    /// not double-light it. `true` if ANY decoded primitive is unlit (P3DT marks every material unlit).
    pub unlit: bool,
}

impl TileGeometry {
    /// Approximate heap footprint of the renderer-bound buffers (bytes) — the budget unit for the decoded-
    /// mesh LRU (`MeshCache`). Counts the three owned buffers; the small fixed-size bounds fields are noise.
    #[inline]
    pub fn byte_size(&self) -> usize {
        self.vertices.len() + self.indices.len() * 4 + self.texture.len()
    }
}

/// Combine a decoded glTF's primitives into one mesh in **Unreal render space** (cm), rebasing each vertex
/// tile-local → ECEF (`world·yup2zup·node`, the P3DT placement) → ENU-local at `enu`'s origin → Unreal space
/// via ONE folded matrix per vertex (see [`ecef_to_local_affine`]); binds the material's base-color image.
/// Returns `None` for empty / degenerate (non-finite-bounds) geometry — **never panics** on bad bytes.
/// The Geo World's production decoder ([`crate::geo_world::production_decoder`]) is its only caller.
pub(crate) fn rebase_gltf(enu: &EnuFrame, g: logi_gltf::DecodedGltf, world: DMat4) -> Option<TileGeometry> {
    // Build the interleaved 32-byte vertex BYTES directly: each transformed vertex's 8 f32s are cast to 32
    // little-endian bytes on the stack and appended once, so there is no second full-buffer `Vec<f32>`→
    // `Vec<u8>` realloc+memcpy at the end (that was a copy of the largest buffer in the tile, every tile).
    let mut vertices: Vec<u8> = Vec::new();
    let mut indices: Vec<u32> = Vec::new();
    let mut base = 0u32;
    // ±INFINITY sentinels (not f32::MAX/MIN): degenerate/all-NaN geometry then yields non-finite bounds the
    // guard below catches, instead of a ~f32::MAX radius silently poisoning culling/placement (audit M14).
    let mut mn = [f32::INFINITY; 3];
    let mut mx = [f32::NEG_INFINITY; 3];
    // glTF is Y-up; 3D Tiles tile space (+ our WGS84 ECEF) is Z-up. The +90°-about-X rotation converts the
    // tile's glTF-Y-up-ECEF placement (baked into the node transform by P3DT) into Z-up ECEF (x,y,z)→(x,-z,y);
    // omitting it lands the tile ~Earth-diameter off. Chain: ecef = world · yup2zup · node · vertex.
    let yup2zup = DMat4::from_rotation_x(std::f64::consts::FRAC_PI_2);
    // Fold the ECEF→ENU-local rebase into one affine matrix, composed per-primitive with `world·yup2zup·node`,
    // so the per-vertex loop applies a SINGLE matrix (~half the matrix ops/vertex). Identical math, in f64.
    let to_local = ecef_to_local_affine(enu);
    for prim in &g.primitives {
        let node = glam::Mat4::from_cols_array_2d(&prim.node_transform).as_dmat4();
        let m_local = to_local * (world * yup2zup * node);
        // Normals transform by the INVERSE-TRANSPOSE of the linear part (3D Tiles 1.1 / glTF). For a rotation
        // (Google P3DT) this equals the linear part itself (orthogonal ⇒ R⁻ᵀ = R), so it's a no-op there; it
        // differs — and matters for correct lighting — only under NON-UNIFORM scale. Computed once per primitive.
        // Falls back to the plain linear part if singular (degenerate primitive → `normalize_or_zero` zeroes it).
        let linear = glam::DMat3::from_mat4(m_local);
        let normal_mat = {
            let it = linear.inverse().transpose();
            if it.to_cols_array().iter().all(|c| c.is_finite()) {
                it
            } else {
                linear
            }
        };
        // Whether `normal_mat` is ORTHONORMAL (a pure rotation/reflection — the Google P3DT case, where each tile
        // placement is rigid). For an orthonormal map ‖M·n‖ = ‖n‖, so transforming a (spec-unit) glTF normal yields
        // a unit normal: the per-vertex `normalize_or_zero` is then redundant work — a sqrt + 3 divides EVERY
        // vertex — and we skip it on this fast path, keeping it only on the NON-uniform-scale path where the
        // inverse-transpose genuinely rescales. A zero input still maps to zero under M, so the degenerate→zero
        // guarantee holds. Tested once per primitive; `abs_diff_eq` tolerates fp noise. (audit SIMD-B)
        let ortho = (normal_mat * normal_mat.transpose()).abs_diff_eq(glam::DMat3::IDENTITY, 1e-9);

        let mut vcount = 0u32;
        vertices.reserve(prim.positions.len() * 32);
        // Transform + interleave ONE vertex into the 32-byte layout (pos f3 | normal f3 | uv f2), appended once,
        // while tracking the local AABB. Hoisted out of the attribute-presence branch below so both iteration
        // paths run identical math (audit D2). Reads the raw attribute arrays directly — no `interleave()` in the
        // decoder + `pod_read` back here.
        let emit =
            |p: &[f32; 3], n: [f32; 3], uv: [f32; 2], vertices: &mut Vec<u8>, mn: &mut [f32; 3], mx: &mut [f32; 3]| {
                // Position via the folded affine (f64); normal via the inverse-transpose, normalized only when scaled.
                let p_enu = m_local.transform_point3(DVec3::new(p[0] as f64, p[1] as f64, p[2] as f64));
                let nv = normal_mat * DVec3::new(n[0] as f64, n[1] as f64, n[2] as f64);
                let n_enu = if ortho { nv } else { nv.normalize_or_zero() };
                // ENU metres (RH) → Unreal render space (cm, LH, Z-up): negate North. See `enu_pos_to_ue`.
                let pe = enu_pos_to_ue(p_enu);
                let ne = enu_dir_to_ue(n_enu);
                // Interleave THIS vertex's 8 f32s and cast to 32 LE bytes on the stack (no heap), append once.
                let v8 = [pe[0], pe[1], pe[2], ne[0], ne[1], ne[2], uv[0], uv[1]];
                vertices.extend_from_slice(bytemuck::cast_slice(&v8));
                for k in 0..3 {
                    mn[k] = mn[k].min(pe[k]);
                    mx[k] = mx[k].max(pe[k]);
                }
            };
        // Hoist the attribute-presence + length test OUT of the per-vertex loop (it is loop-invariant). The common
        // P3DT case — normals AND uvs present and at least as long as positions — runs a branch-free ZIPPED loop
        // (no per-vertex `Option`/bounds check, the autovectorizer's preferred shape). Anything irregular (missing
        // or short attribute arrays) falls back to the per-vertex defaulted lookup. Defaults match
        // `logi_geo_mesh::interleave`: missing normal → +Z, missing uv → (0, 0).
        let normals_full = prim.normals.as_deref().filter(|a| a.len() >= prim.positions.len());
        let uvs_full = prim.uvs.as_deref().filter(|a| a.len() >= prim.positions.len());
        match (normals_full, uvs_full) {
            (Some(ns), Some(uvs)) => {
                for ((p, &n), &uv) in prim.positions.iter().zip(ns).zip(uvs) {
                    emit(p, n, uv, &mut vertices, &mut mn, &mut mx);
                    vcount += 1;
                }
            }
            _ => {
                for (i, p) in prim.positions.iter().enumerate() {
                    let n = prim
                        .normals
                        .as_ref()
                        .and_then(|a| a.get(i))
                        .copied()
                        .unwrap_or([0.0, 0.0, 1.0]);
                    let uv = prim.uvs.as_ref().and_then(|a| a.get(i)).copied().unwrap_or([0.0, 0.0]);
                    emit(p, n, uv, &mut vertices, &mut mn, &mut mx);
                    vcount += 1;
                }
            }
        }
        // Rebase indices into the combined buffer; skip on overflow rather than wrapping (hostile input).
        for &i in &prim.indices {
            if let Some(idx) = i.checked_add(base) {
                indices.push(idx);
            }
        }
        base = base.saturating_add(vcount);
    }
    if vertices.is_empty() {
        return None;
    }
    // Texture (audit #3): bind the image the material's `baseColorTexture` actually names — NOT just the
    // first non-empty image, which silently picks the wrong map (a normal / metallic-roughness texture) on a
    // multi-image material. Take the first primitive that declares a base-color image and use exactly that one
    // (empty if it's an unfetched external URI — we must not substitute a different map). Only when NO
    // primitive declares a base-color texture (vertex-coloured / untextured materials) do we fall back to the
    // first non-empty image — preserving the prior behaviour for those tiles.
    let base_color_idx = g.primitives.iter().find_map(|p| p.base_color_texture);
    // KHR_materials_unlit: flag the tile if any primitive is unlit (P3DT marks every material unlit — baked
    // lighting), so the renderer can shade it emissive and not double-light it.
    let unlit = g.primitives.iter().any(|p| p.unlit);
    let mut images = g.images;
    let texture = match base_color_idx {
        Some(i) => images
            .get_mut(i)
            .map(|im| std::mem::take(&mut im.data))
            .unwrap_or_default(),
        None => images
            .into_iter()
            .map(|im| im.data)
            .find(|d| !d.is_empty())
            .unwrap_or_default(),
    };
    let (bounds_center, bounds_radius) = aabb_to_sphere(mn, mx);
    // Reject non-finite bounds (all-NaN / degenerate geometry) — they would poison culling/placement (M14).
    if !bounds_center.iter().all(|c| c.is_finite()) || !bounds_radius.is_finite() {
        return None;
    }
    Some(TileGeometry {
        vertices,
        indices,
        texture,
        bounds_center,
        bounds_radius,
        unlit,
    })
}

/// The ECEF→ENU-local rebase as one affine `DMat4`: rotate by `enu.ecef_to_enu`, translate by −R·origin
/// (the identical map to [`EnuFrame::ecef_to_local`]). Composed once per primitive with `world·yup2zup·node`,
/// it lets the per-vertex hot loop apply a SINGLE matrix — position via `transform_point3`, normal via
/// `transform_vector3` (they share the linear part) — instead of a transform followed by a separate
/// `ecef_to_local`/`ecef_to_enu` step: ~half the per-vertex matrix work. Kept in f64: the only difference
/// from the two-step is a ~1e-9 m cancellation (rotate-then-subtract two ~6.4e6 m operands), negligible at
/// cm render scale — folding to f32 here would NOT be safe (that is why this stays f64).
fn ecef_to_local_affine(enu: &EnuFrame) -> DMat4 {
    let r = enu.ecef_to_enu;
    DMat4::from_translation(-(r * enu.origin_ecef)) * DMat4::from_mat3(r)
}

/// The external-tileset URI hint, single-homed in `logi_3dtiles_tileset` (re-exported here — it was a
/// byte-for-byte duplicate). A request-time hint only; the authoritative classification sniffs the fetched
/// bytes (see [`sniff_content_kind`]), since extensionless / query-only tileset endpoints carry no extension.
pub(crate) use logi_3dtiles_tileset::is_tileset_uri;

/// Classify fetched tile content by sniffing its leading bytes (audit M18): a `glTF` magic ⇒ a GLB
/// geometry tile; an opening `{` (after any UTF-8 BOM / leading ASCII whitespace) ⇒ a JSON tileset.
/// Returns `None` when neither signature matches, so the caller can fall back to the URI hint.
pub(crate) fn sniff_content_kind(bytes: &[u8]) -> Option<ContentKind> {
    if bytes.starts_with(b"glTF") {
        return Some(ContentKind::Glb);
    }
    // Skip a UTF-8 BOM, then any leading ASCII whitespace, and look for the JSON object opener.
    let rest = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
    let first = rest.iter().find(|b| !b.is_ascii_whitespace());
    match first {
        Some(b'{') => Some(ContentKind::Tileset),
        _ => None,
    }
}

/// Walk a child-index `path` from `root` to the tile it names, for grafting an external tileset in place.
pub(crate) fn tile_at_mut<'a>(root: &'a mut Tile, path: &[u32]) -> Option<&'a mut Tile> {
    let mut t = root;
    for &i in path {
        t = t.children.get_mut(i as usize)?;
    }
    Some(t)
}

#[cfg(test)]
mod streaming_tests {
    use super::*;

    #[test]
    fn ecef_to_local_affine_matches_two_step() {
        // The folded rebase matrix must equal `EnuFrame::ecef_to_local` (position) + `ecef_to_enu` (normal),
        // so collapsing the per-vertex transform pair into one matrix is a pure speedup, not a behavior change.
        use logi_geospatial::{Cartographic, Ellipsoid};

        let e = Ellipsoid::wgs84();
        let enu = EnuFrame::from_cartographic(&e, Cartographic::from_degrees(-73.9857, 40.7484, 0.0));
        let affine = ecef_to_local_affine(&enu);

        // Position: the affine reproduces the two-step rebase (within f64 cancellation noise) at the origin,
        // a near point, and far ECEF points.
        for p in [
            enu.origin_ecef,
            enu.origin_ecef + DVec3::new(120.0, -340.0, 75.0),
            DVec3::new(6_378_137.0, 0.0, 0.0),
            DVec3::new(1.0e6, 2.0e6, 3.0e6),
        ] {
            let folded = affine.transform_point3(p);
            let two_step = enu.ecef_to_local(p);
            assert!(
                (folded - two_step).length() < 1e-6,
                "position fold mismatch: {folded:?} vs {two_step:?}"
            );
        }
        // Direction: the affine's linear part reproduces the `ecef_to_enu` rotation used for normals.
        for v in [DVec3::X, DVec3::Y, DVec3::new(1.0, -2.0, 0.5)] {
            let folded = affine.transform_vector3(v);
            let direct = enu.ecef_to_enu * v;
            assert!((folded - direct).length() < 1e-9, "direction fold mismatch");
        }
    }

    #[test]
    fn sniff_classifies_by_magic_not_extension() {
        // glTF magic ⇒ GLB even from an extensionless / query-only URL.
        assert_eq!(sniff_content_kind(b"glTF\x02\0\0\0"), Some(ContentKind::Glb));
        // A JSON object ⇒ tileset, tolerating a UTF-8 BOM and leading whitespace.
        assert_eq!(sniff_content_kind(b"{\"root\":{}}"), Some(ContentKind::Tileset));
        assert_eq!(
            sniff_content_kind(b"\xEF\xBB\xBF  \n{\"asset\":{}}"),
            Some(ContentKind::Tileset)
        );
        // Unrecognisable content ⇒ None (caller falls back to the URI hint). Empty too.
        assert_eq!(sniff_content_kind(b"<html>"), None);
        assert_eq!(sniff_content_kind(b""), None);
    }

    // (`is_tileset_uri` now lives + is tested in `logi_3dtiles_tileset`; this layer only re-exports it.)

    /// Build a one-vertex decoded glTF carrying `images`, the first primitive pointing its baseColorTexture
    /// at `base_color`. Enough for `rebase_gltf` to emit a (degenerate but valid) `TileGeometry`.
    fn decoded_with_images(base_color: Option<usize>, images: Vec<logi_gltf::EmbeddedImage>) -> logi_gltf::DecodedGltf {
        logi_gltf::DecodedGltf {
            primitives: vec![logi_gltf::DecodedPrimitive {
                positions: vec![[0.0, 0.0, 0.0]],
                normals: None,
                uvs: None,
                indices: vec![0],
                base_color_factor: [1.0; 4],
                base_color_texture: base_color,
                unlit: false,
                node_transform: glam::Mat4::IDENTITY.to_cols_array_2d(),
            }],
            images,
        }
    }

    fn jpeg_image(tag: &[u8]) -> logi_gltf::EmbeddedImage {
        logi_gltf::EmbeddedImage {
            mime_type: "image/jpeg".to_string(),
            data: tag.to_vec(),
        }
    }

    #[test]
    fn rebase_binds_the_base_color_image_not_the_first() {
        use logi_geospatial::{Cartographic, Ellipsoid};
        let enu = EnuFrame::from_cartographic(&Ellipsoid::wgs84(), Cartographic::from_degrees(-73.9857, 40.7484, 0.0));
        // Image 0 is a normal map, image 1 the albedo the material names. The old "first non-empty" code would
        // have bound image 0 (the normal map) → wrong/low-quality surface; #3 must bind image 1.
        let g = decoded_with_images(Some(1), vec![jpeg_image(b"NORMAL"), jpeg_image(b"ALBEDO")]);
        let geom = rebase_gltf(&enu, g, DMat4::IDENTITY).expect("one-vertex geometry is valid");
        assert_eq!(
            geom.texture, b"ALBEDO",
            "must bind the baseColorTexture image (1), not the first image (0)"
        );
    }

    #[test]
    fn rebase_falls_back_to_first_image_when_no_base_color_declared() {
        use logi_geospatial::{Cartographic, Ellipsoid};
        let enu = EnuFrame::from_cartographic(&Ellipsoid::wgs84(), Cartographic::from_degrees(0.0, 0.0, 0.0));
        // No baseColorTexture (vertex-coloured / untextured material) → first non-empty image is the best guess.
        let g = decoded_with_images(None, vec![jpeg_image(b"ONLYIMG")]);
        let geom = rebase_gltf(&enu, g, DMat4::IDENTITY).expect("one-vertex geometry is valid");
        assert_eq!(
            geom.texture, b"ONLYIMG",
            "no base-color declared ⇒ first non-empty image"
        );
    }
}
