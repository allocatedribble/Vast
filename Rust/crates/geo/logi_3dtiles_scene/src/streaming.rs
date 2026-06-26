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
    /// flash cache (`FlashCache`). Counts the three owned buffers; the small fixed-size bounds fields are noise.
    #[inline]
    pub fn byte_size(&self) -> usize {
        self.vertices.len() + self.indices.len() * 4 + self.texture.len()
    }
}

/// On-disk format tag for a serialized [`TileGeometry`] blob (the persisted geometry tier, audit M3). Bumped on
/// ANY layout OR content-semantics change so a stale blob from an older build is rejected by
/// [`deserialize_geometry`] and re-decoded (no migration — discard-and-rebuild, ADR-0004).
/// "LGM2": skirts (2026-06-11) — pre-skirt blobs must re-decode or the cracks they were cut to fix persist.
/// "LGM3": skirt drop re-scaled (same day) — LGM2's 500 m cap left coarse-LOD cracks open (the orbital
/// flat-purple triangles); blobs with the under-sized skirts must re-decode.
const GEOM_BLOB_MAGIC: u32 = 0x4C47_4D33; // "LGM3"
/// Fixed header: magic(4) + 3 buffer lengths(u32) + center[f32;3](12) + radius(f32)(4) + unlit(u8)(1) + pad(3).
const GEOM_BLOB_HEADER: usize = 36;
/// Per-buffer sanity cap so a corrupt/hostile header can't request a giant allocation (a decoded tile's buffers
/// are far below the 64 MiB tile ceiling).
const GEOM_BLOB_MAX_BUF: usize = 64 * 1024 * 1024;

/// Serialize a decoded [`TileGeometry`] to a flat blob for the persisted geometry tier (audit M3): a fixed
/// header (magic + buffer lengths + bounds + unlit) then the three raw POD buffers (no serde). Whole-blob
/// integrity is the geometry `DiskCache`'s CRC-in-filename; the magic + exact-length check in
/// [`deserialize_geometry`] is belt-and-suspenders against a format mismatch / truncation.
pub(crate) fn serialize_geometry(g: &TileGeometry) -> Vec<u8> {
    let mut out = Vec::with_capacity(GEOM_BLOB_HEADER + g.vertices.len() + g.indices.len() * 4 + g.texture.len());
    out.extend_from_slice(&GEOM_BLOB_MAGIC.to_le_bytes());
    out.extend_from_slice(&(g.vertices.len() as u32).to_le_bytes());
    out.extend_from_slice(&(g.indices.len() as u32).to_le_bytes());
    out.extend_from_slice(&(g.texture.len() as u32).to_le_bytes());
    for c in g.bounds_center {
        out.extend_from_slice(&c.to_le_bytes());
    }
    out.extend_from_slice(&g.bounds_radius.to_le_bytes());
    out.push(g.unlit as u8);
    out.extend_from_slice(&[0u8; 3]); // pad to GEOM_BLOB_HEADER
    out.extend_from_slice(&g.vertices);
    for i in &g.indices {
        out.extend_from_slice(&i.to_le_bytes());
    }
    out.extend_from_slice(&g.texture);
    out
}

/// Inverse of [`serialize_geometry`]. Returns `None` on a magic mismatch (stale format → re-decode), an
/// over-cap length (corrupt header), or a size that doesn't match the declared buffers (truncation) — it never
/// panics on bad bytes.
pub(crate) fn deserialize_geometry(b: &[u8]) -> Option<TileGeometry> {
    if b.len() < GEOM_BLOB_HEADER {
        return None;
    }
    let u32_at = |o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
    if u32_at(0) != GEOM_BLOB_MAGIC {
        return None;
    }
    let (vlen, ilen, tlen) = (u32_at(4) as usize, u32_at(8) as usize, u32_at(12) as usize);
    if vlen > GEOM_BLOB_MAX_BUF || ilen > GEOM_BLOB_MAX_BUF || tlen > GEOM_BLOB_MAX_BUF {
        return None;
    }
    let f32_at = |o: usize| f32::from_le_bytes(b[o..o + 4].try_into().unwrap());
    let bounds_center = [f32_at(16), f32_at(20), f32_at(24)];
    let bounds_radius = f32_at(28);
    let unlit = b[32] != 0;
    // Exact-size check (truncation guard); `checked` math so a hostile length can't wrap.
    let want = GEOM_BLOB_HEADER
        .checked_add(vlen)?
        .checked_add(ilen.checked_mul(4)?)?
        .checked_add(tlen)?;
    if b.len() != want {
        return None;
    }
    let (vstart, istart) = (GEOM_BLOB_HEADER, GEOM_BLOB_HEADER + vlen);
    let tstart = istart + ilen * 4;
    let indices = b[istart..tstart]
        .chunks_exact(4)
        .map(|c| u32::from_le_bytes(c.try_into().unwrap()))
        .collect();
    Some(TileGeometry {
        vertices: b[vstart..istart].to_vec(),
        indices,
        texture: b[tstart..].to_vec(),
        bounds_center,
        bounds_radius,
        unlit,
    })
}

// (The axis-aligned `vertex_aabb` hole-clip helper was deleted 2026-06-09: the producer is now
// `geo_world::oriented_clip_box`, which bounds the mesh in the tile's OWN ground frame — an axis-aligned box
// over-cut tilted far-from-origin tiles, the PIE jagged-reveal + gap-ring artifact.)

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
        // `node_transform` is already f64 (re-read from the GLB JSON in logi_gltf — the `gltf` crate's f32
        // would quantise P3DT's Earth-radius placement to a ~0.76 m PER-TILE grid → tile-aligned depth seams).
        let node = glam::DMat4::from_cols_array_2d(&prim.node_transform);
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
        // Rebase indices into the combined buffer. Both decode lanes already validated every index
        // `< positions.len()` (logi_gltf / logi_draco), so `i + base` can only overflow if the AGGREGATE
        // vertex count approaches u32::MAX — unreachable under the 64 MiB fetch cap, but if it ever fires,
        // drop this primitive's WHOLE triangle list (C5, audit 2026-06-10): the old per-index skip silently
        // shifted every subsequent index by one, garbling the rest of the list into random triangles.
        // The primitive's already-emitted vertices stay as harmless unreferenced orphans.
        if (base as u64) + (vcount as u64) <= u32::MAX as u64 {
            for &i in &prim.indices {
                indices.push(i + base);
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
    // SKIRTS (2026-06-11 — the symmetric triangular see-through holes): adjacent tiles at DIFFERENT LODs
    // don't share boundary vertices, so their edges crack open and a grazing view sees clean through the
    // shell to the planet's far side (rows of regular dark triangles along cut boundaries — geometry gaps,
    // not clip artifacts: fragment discard cuts box-edge shapes, these were triangle-shaped). The standard
    // invisible fix (cesium quantized-mesh ships it natively): extrude every boundary edge DOWN along the
    // geocentric axis by a fraction of the tile's extent, stretching the edge texel over the apron.
    {
        let earth_center = {
            let ec = enu_pos_to_ue(enu.ecef_to_local(DVec3::ZERO));
            glam::Vec3::new(ec[0], ec[1], ec[2])
        };
        let centroid = glam::Vec3::new((mn[0] + mx[0]) * 0.5, (mn[1] + mx[1]) * 0.5, (mn[2] + mx[2]) * 0.5);
        add_skirts(&mut vertices, &mut indices, centroid, earth_center, &mut mn, &mut mx);
    }
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

/// Skirt sizing: drop = this fraction of the tile's AABB diagonal, clamped to [min, max] cm. The drop must
/// cover the vertical mismatch between adjacent LODs ≈ the coarser neighbour's geometric error — and P3DT
/// halves BOTH tile size and geometric error per level, so a fixed fraction of the tile's own extent tracks
/// that mismatch at every depth (5% of diag ≈ 2× the tile's own GE). The original 500 m max silenced exactly
/// the coarse-LOD cracks skirts exist to close (the 2026-06-11 orbital flat-purple triangles: rendered cut
/// depth 2–10, GE up to ~5.3 km ⇒ mismatch ~10 km); 25 km covers the deepest rendered mismatch with margin
/// and still bounds a degenerate planet-root tile. Cost: 2 triangles per boundary edge (boundary ≈ O(√T) of
/// a tile's T triangles) + side-facing fill only along cut edges.
const SKIRT_DROP_FRACTION: f32 = 0.05;
const SKIRT_DROP_MIN_CM: f32 = 100.0; // 1 m
const SKIRT_DROP_MAX_CM: f32 = 2_500_000.0; // 25 km

/// Append SKIRTS to a rebased tile mesh: every BOUNDARY edge (used by exactly one triangle) is extruded
/// `drop` centimetres toward the earth center, closing the see-through cracks where adjacent tiles at
/// different LODs don't share edge vertices (the 2026-06-11 symmetric-triangle holes). Skirt vertices copy
/// the parent vertex's normal + UV — the edge texel stretches over the apron, invisible from above — and
/// the skirt quad inherits the boundary edge's winding AS WOUND in its owning triangle, so it faces the
/// same way the surface does. `mn`/`mx` are extended so the tile's bounds (culling + clip boxes) cover the
/// apron. Interior mesh holes (rare) get harmless skirts too — hidden beneath the surface.
fn add_skirts(
    vertices: &mut Vec<u8>,
    indices: &mut Vec<u32>,
    centroid: glam::Vec3,
    earth_center: glam::Vec3,
    mn: &mut [f32; 3],
    mx: &mut [f32; 3],
) {
    const STRIDE: usize = 32; // pos f3 | normal f3 | uv f2 (logi_geo_mesh::VERTEX_STRIDE)
    if indices.len() < 3 || vertices.len() < STRIDE {
        return;
    }
    let down = (earth_center - centroid).normalize_or_zero();
    if down == glam::Vec3::ZERO {
        return; // centroid at the earth center — hostile/degenerate content; no skirt is always safe
    }
    let diag = (glam::Vec3::from(*mx) - glam::Vec3::from(*mn)).length();
    let drop = (diag * SKIRT_DROP_FRACTION).clamp(SKIRT_DROP_MIN_CM, SKIRT_DROP_MAX_CM);
    // Boundary edges: undirected use-count over all triangles, keeping the DIRECTED first occurrence so the
    // skirt inherits the surface winding. O(T) hashing.
    let mut edges: hashbrown::HashMap<(u32, u32), (u32, u32, u32)> = hashbrown::HashMap::with_capacity(indices.len());
    for t in indices.chunks_exact(3) {
        for (a, b) in [(t[0], t[1]), (t[1], t[2]), (t[2], t[0])] {
            edges
                .entry((a.min(b), a.max(b)))
                .and_modify(|e| e.0 += 1)
                .or_insert((1, a, b));
        }
    }
    let vcount = (vertices.len() / STRIDE) as u32;
    let boundary: Vec<(u32, u32)> = edges
        .into_values()
        .filter(|&(count, a, b)| count == 1 && a < vcount && b < vcount)
        .map(|(_, a, b)| (a, b))
        .collect();
    if boundary.is_empty() {
        return; // a closed mesh — nothing to skirt
    }
    // Duplicate each boundary vertex once (displaced down, attributes copied), then stitch the quads.
    let mut skirt_of: hashbrown::HashMap<u32, u32> = hashbrown::HashMap::with_capacity(boundary.len());
    for &(a, b) in &boundary {
        for i in [a, b] {
            skirt_of.entry(i).or_insert_with(|| {
                let off = i as usize * STRIDE;
                let mut v: [u8; STRIDE] = vertices[off..off + STRIDE].try_into().expect("stride slice");
                let f32_at =
                    |v: &[u8; STRIDE], k: usize| f32::from_le_bytes(v[k * 4..k * 4 + 4].try_into().expect("f32"));
                // PER-VERTEX drop direction: toward the earth center from THIS vertex, not from the tile
                // centroid — a constant centroid direction goes tangent at the corners of large curved tiles
                // (a hemisphere-scale tile's corner "down" differs by up to ~90°), and at km-scale drops a
                // tangent skirt is a visible flap poking through the surface instead of a hidden apron.
                let pos = glam::Vec3::new(f32_at(&v, 0), f32_at(&v, 1), f32_at(&v, 2));
                let dir = (earth_center - pos).normalize_or_zero();
                let disp = if dir == glam::Vec3::ZERO { down } else { dir } * drop;
                for (k, d) in [disp.x, disp.y, disp.z].into_iter().enumerate() {
                    let p = f32_at(&v, k) + d;
                    v[k * 4..k * 4 + 4].copy_from_slice(&p.to_le_bytes());
                    mn[k] = mn[k].min(p);
                    mx[k] = mx[k].max(p);
                }
                let new_index = (vertices.len() / STRIDE) as u32;
                vertices.extend_from_slice(&v);
                new_index
            });
        }
    }
    indices.reserve(boundary.len() * 6);
    for (a, b) in boundary {
        let (a2, b2) = (skirt_of[&a], skirt_of[&b]);
        // Edge wound a→b in its owning triangle ⇒ the apron (a, b, b2) + (a, b2, a2) faces with the surface.
        indices.extend_from_slice(&[a, b, b2, a, b2, a2]);
    }
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
    fn skirts_extrude_every_boundary_edge_downward() {
        // An open quad (4 verts, 2 triangles) above the origin; "down" toward an earth center at -Z.
        // All 4 outer edges are boundary (the diagonal is shared) ⇒ +4 skirt verts, +8 skirt triangles.
        const STRIDE: usize = 32;
        let pos = [
            [0.0f32, 0.0, 1000.0],
            [100.0, 0.0, 1000.0],
            [100.0, 100.0, 1000.0],
            [0.0, 100.0, 1000.0],
        ];
        let mut vertices = Vec::new();
        for p in pos {
            let v8 = [p[0], p[1], p[2], 0.0, 0.0, 1.0, 0.25, 0.75]; // pos | normal | uv
            vertices.extend_from_slice(bytemuck::cast_slice(&v8));
        }
        let mut indices: Vec<u32> = vec![0, 1, 2, 0, 2, 3];
        let (mut mn, mut mx) = ([0.0f32, 0.0, 1000.0], [100.0f32, 100.0, 1000.0]);
        add_skirts(
            &mut vertices,
            &mut indices,
            glam::Vec3::new(50.0, 50.0, 1000.0),
            glam::Vec3::new(50.0, 50.0, -1.0e9), // earth center far below ⇒ down = -Z
            &mut mn,
            &mut mx,
        );
        assert_eq!(vertices.len() / STRIDE, 8, "4 boundary verts duplicated once each");
        assert_eq!(indices.len(), 6 + 4 * 6, "2 surface + 8 skirt triangles");
        // diag ≈ 141 cm ⇒ 5% ≈ 7 cm, clamped UP to the 100 cm floor: every skirt vertex exactly 100 below
        // its parent, bounds extended to match, normal + uv copied verbatim.
        assert_eq!(
            (mn[2], mx[2]),
            (900.0, 1000.0),
            "bounds extended downward by the clamped drop"
        );
        for i in 4..8 {
            let off = i * STRIDE;
            let z = f32::from_le_bytes(vertices[off + 8..off + 12].try_into().unwrap());
            assert_eq!(z, 900.0, "skirt vertex {i} dropped by the 100 cm floor (z={z})");
            let nz = f32::from_le_bytes(vertices[off + 20..off + 24].try_into().unwrap());
            let u = f32::from_le_bytes(vertices[off + 24..off + 28].try_into().unwrap());
            assert_eq!((nz, u), (1.0, 0.25), "normal + uv copied from the parent vertex");
        }
        // Winding: each skirt quad's first triangle reuses its boundary edge AS WOUND (a, b, b').
        for q in indices[6..].chunks_exact(6) {
            assert!(
                q[0] < 4 && q[1] < 4 && q[2] >= 4,
                "apron tri = (surface a, surface b, skirt b')"
            );
        }
    }

    #[test]
    fn skirt_drop_scales_with_coarse_tile_extent() {
        // The 2026-06-11 orbital flat-purple triangles: coarse-LOD cracks are km-scale (GE up to ~5.3 km in
        // the rendered cut), and the original 500 m drop cap couldn't close them. A 100 km tile must get a
        // ~7 km skirt (5% of its diagonal), NOT a 500 m one.
        const STRIDE: usize = 32;
        const KM100: f32 = 1.0e7; // cm
        let pos = [
            [0.0f32, 0.0, 0.0],
            [KM100, 0.0, 0.0],
            [KM100, KM100, 0.0],
            [0.0, KM100, 0.0],
        ];
        let mut vertices = Vec::new();
        for p in pos {
            let v8 = [p[0], p[1], p[2], 0.0, 0.0, 1.0, 0.5, 0.5];
            vertices.extend_from_slice(bytemuck::cast_slice(&v8));
        }
        let mut indices: Vec<u32> = vec![0, 1, 2, 0, 2, 3];
        let (mut mn, mut mx) = ([0.0f32, 0.0, 0.0], [KM100, KM100, 0.0]);
        add_skirts(
            &mut vertices,
            &mut indices,
            glam::Vec3::new(KM100 * 0.5, KM100 * 0.5, 0.0),
            glam::Vec3::new(KM100 * 0.5, KM100 * 0.5, -6.4e8), // earth center ~6,400 km below
            &mut mn,
            &mut mx,
        );
        // drop = 5% × diag (≈ 1.414e7) ≈ 7.07e5 cm ≈ 7.07 km; per-vertex direction tilts ≤ ~0.8% off -Z.
        assert_eq!(vertices.len() / STRIDE, 8, "4 boundary verts duplicated");
        assert!(
            (-7.2e5..=-6.9e5).contains(&mn[2]),
            "a 100 km tile's skirt drops ~7 km (got {} cm) — the old 500 m cap must not bind",
            mn[2]
        );
    }

    #[test]
    fn geometry_blob_roundtrips_and_rejects_corruption() {
        // M3: serialize → deserialize must reconstruct the geometry EXACTLY, and a magic/length corruption must
        // be rejected (`None`) rather than producing a bad mesh — the persisted geometry tier relies on both.
        let g = TileGeometry {
            vertices: (0..96u8).collect(), // 3 vertices × 32-byte layout
            indices: vec![0u32, 1, 2, 2, 1, 0],
            texture: vec![0xFF, 0xD8, 0xFF, 0xAA], // a few "JPEG" bytes
            bounds_center: [1.5, -2.0, 3.25],
            bounds_radius: 12.5,
            unlit: true,
        };
        let blob = serialize_geometry(&g);
        let back = deserialize_geometry(&blob).expect("roundtrip");
        assert_eq!(back.vertices, g.vertices);
        assert_eq!(back.indices, g.indices);
        assert_eq!(back.texture, g.texture);
        assert_eq!(back.bounds_center, g.bounds_center);
        assert_eq!(back.bounds_radius, g.bounds_radius);
        assert_eq!(back.unlit, g.unlit);

        // Truncation → None (the exact-size guard); wrong magic → None (a stale-format blob is re-decoded, not
        // mis-read); too short for even the header → None.
        assert!(
            deserialize_geometry(&blob[..blob.len() - 1]).is_none(),
            "truncated blob rejected"
        );
        let mut bad = blob.clone();
        bad[0] ^= 0xFF;
        assert!(deserialize_geometry(&bad).is_none(), "bad magic rejected");
        assert!(deserialize_geometry(&[0u8; 4]).is_none(), "too short rejected");
    }

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
                node_transform: glam::DMat4::IDENTITY.to_cols_array_2d(),
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
