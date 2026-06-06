//! Shared **decode / rebase + content-classification helpers** for the Geo stack. The streaming *world*
//! itself is now the `bevy_ecs` [`GeoWorld`](crate::geo_world::GeoWorld) — the old `TilesetWorld` streaming
//! engine was retired once `GlobeStreamer`/the FFI moved to the ECS orchestration (ADR-0012 Addendum A;
//! ADR-0004 "no dead code"). What remains here is the reusable, world-agnostic machinery both still need:
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

/// A streaming/tileset error (parse / fetch / graft).
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
/// via ONE folded matrix per vertex (see [`ecef_to_local_affine`]); takes the first non-empty texture.
/// Returns `None` for empty / degenerate (non-finite-bounds) geometry — **never panics** on bad bytes.
/// The Geo World's production decoder ([`crate::geo_world::production_decoder`]) is its only caller.
pub(crate) fn rebase_gltf(enu: &EnuFrame, g: logi_gltf::DecodedGltf, world: DMat4) -> Option<TileGeometry> {
    // Accumulate transformed vertices as f32 (8/vertex), cast to bytes once at the end (audit M12).
    let mut verts_f32: Vec<f32> = Vec::new();
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
        // Read each 32-byte vertex as a whole [f32; 8] via `pod_read_unaligned` (the glTF `Vec<u8>` has
        // alignment 1, so a `&[u8]`→`&[f32]` cast would panic — a decoder must never panic on bad bytes).
        // `chunks_exact(32)` drops a non-multiple-of-32 tail. Target is little-endian (x86_64). (audit M12)
        let mut vcount = 0u32;
        verts_f32.reserve(prim.vertices.len() / 4);
        for chunk in prim.vertices.chunks_exact(32) {
            let vtx: [f32; 8] = bytemuck::pod_read_unaligned(chunk);
            // One matrix: position via the affine, normal via its linear part (shared), then normalize.
            let p_enu = m_local.transform_point3(DVec3::new(vtx[0] as f64, vtx[1] as f64, vtx[2] as f64));
            let n_enu = m_local
                .transform_vector3(DVec3::new(vtx[3] as f64, vtx[4] as f64, vtx[5] as f64))
                .normalize_or_zero();
            // ENU metres (RH) → Unreal render space (cm, LH, Z-up): negate North. See `enu_pos_to_ue`.
            let pe = enu_pos_to_ue(p_enu);
            let ne = enu_dir_to_ue(n_enu);
            verts_f32.extend_from_slice(&[pe[0], pe[1], pe[2], ne[0], ne[1], ne[2], vtx[6], vtx[7]]);
            for k in 0..3 {
                mn[k] = mn[k].min(pe[k]);
                mx[k] = mx[k].max(pe[k]);
            }
            vcount += 1;
        }
        // Rebase indices into the combined buffer; skip on overflow rather than wrapping (hostile input).
        for &i in &prim.indices {
            if let Some(idx) = i.checked_add(base) {
                indices.push(idx);
            }
        }
        base = base.saturating_add(vcount);
    }
    if verts_f32.is_empty() {
        return None;
    }
    let texture = g
        .images
        .into_iter()
        .map(|im| im.data)
        .find(|d| !d.is_empty())
        .unwrap_or_default();
    let (bounds_center, bounds_radius) = aabb_to_sphere(mn, mx);
    // Reject non-finite bounds (all-NaN / degenerate geometry) — they would poison culling/placement (M14).
    if !bounds_center.iter().all(|c| c.is_finite()) || !bounds_radius.is_finite() {
        return None;
    }
    // One cast from the f32 accumulator to the byte buffer the renderer uploads (M12).
    let vertices: Vec<u8> = bytemuck::cast_slice(&verts_f32).to_vec();
    Some(TileGeometry {
        vertices,
        indices,
        texture,
        bounds_center,
        bounds_radius,
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

/// Best-effort *hint* that a content URI points at an external `.json` tileset (vs a `.glb`/`.b3dm`
/// geometry tile), judged by the PATH extension after stripping the query/fragment — so a signed tile URL
/// whose token happens to contain ".json" isn't misclassified. This is only a request-time hint: the
/// authoritative classification sniffs the fetched bytes (see [`sniff_content_kind`]), because
/// extensionless / query-only tileset endpoints carry no usable extension (audit M18).
pub(crate) fn is_tileset_uri(uri: &str) -> bool {
    let path = uri.split(['?', '#']).next().unwrap_or(uri);
    path.rsplit('.')
        .next()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("json"))
}

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

    #[test]
    fn is_tileset_uri_strips_query_before_extension() {
        assert!(is_tileset_uri("https://h/root.json"));
        assert!(is_tileset_uri("https://h/root.JSON?access_token=T"));
        // A .glb whose signed query merely contains ".json" must NOT be classed as a tileset.
        assert!(!is_tileset_uri("https://h/x.glb?session=a.json.b"));
        // Extensionless endpoints have no usable hint — byte sniffing is authoritative.
        assert!(!is_tileset_uri("https://h/v1/3dtiles?key=K"));
    }
}
