//! `logi_geo_mesh` — the shared render-geometry contract for the Geo engine.
//!
//! One home (AGENTS.md §1A "one home per shared contract") for the pieces every Geo decoder/mesher would
//! otherwise re-implement: the interleaved vertex layout, the ENU↔Unreal coordinate convention, and the
//! AABB→bounding-sphere reduction. Pure data + math — no GPU, no FFI, no `unsafe`.

#![forbid(unsafe_code)]

use glam::DVec3;

/// 32 bytes per vertex: `pos f3 | normal f3 | uv f2` (little-endian f32) — the engine-wide vertex contract.
pub const VERTEX_STRIDE: usize = 32;

/// Interleave attribute arrays into the [`VERTEX_STRIDE`]-byte vertex layout. A missing normal defaults to
/// `+Z` and a missing uv to `(0, 0)`; a short normal/uv array falls back per-vertex to the same defaults.
pub fn interleave(positions: &[[f32; 3]], normals: Option<&[[f32; 3]]>, uvs: Option<&[[f32; 2]]>) -> Vec<u8> {
    let mut out = Vec::with_capacity(positions.len() * VERTEX_STRIDE);
    for (i, p) in positions.iter().enumerate() {
        let n = normals.and_then(|a| a.get(i)).copied().unwrap_or([0.0, 0.0, 1.0]);
        let uv = uvs.and_then(|a| a.get(i)).copied().unwrap_or([0.0, 0.0]);
        for f in p.iter().chain(n.iter()).chain(uv.iter()) {
            out.extend_from_slice(&f.to_le_bytes());
        }
    }
    out
}

/// ENU metres (East, North, Up; right-handed Z-up) → Unreal render space (cm; **left-handed** Z-up).
/// Scale ×100 and negate North (the Y axis). The negation flips RH→LH — the same reflection the building
/// lane uses — and *also* flips triangle winding into Unreal's front-face convention, so mesh indices are
/// passed through unchanged (reversing them would double-flip and back-face-cull the whole mesh).
#[inline]
pub fn enu_pos_to_ue(p: DVec3) -> [f32; 3] {
    [(p.x * 100.0) as f32, (-p.y * 100.0) as f32, (p.z * 100.0) as f32]
}

/// Direction counterpart of [`enu_pos_to_ue`] (no scale): negate North only.
#[inline]
pub fn enu_dir_to_ue(d: DVec3) -> [f32; 3] {
    [d.x as f32, -d.y as f32, d.z as f32]
}

/// Unreal render space (cm) → ENU metres — the inverse of [`enu_pos_to_ue`], for the camera position.
#[inline]
pub fn ue_cm_to_enu(p: [f64; 3]) -> DVec3 {
    DVec3::new(p[0] / 100.0, -p[1] / 100.0, p[2] / 100.0)
}

/// Bounding sphere `(center, radius)` of the axis-aligned box `[mn, mx]` — the plain center-and-half-
/// diagonal formula. The caller is responsible for guarding a degenerate (empty / non-finite) box.
#[inline]
pub fn aabb_to_sphere(mn: [f32; 3], mx: [f32; 3]) -> ([f32; 3], f32) {
    let center = [(mn[0] + mx[0]) * 0.5, (mn[1] + mx[1]) * 0.5, (mn[2] + mx[2]) * 0.5];
    let radius = 0.5 * ((mx[0] - mn[0]).powi(2) + (mx[1] - mn[1]).powi(2) + (mx[2] - mn[2]).powi(2)).sqrt();
    (center, radius)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interleave_packs_stride_with_defaults() {
        let v = interleave(&[[1.0, 2.0, 3.0]], None, None);
        assert_eq!(v.len(), VERTEX_STRIDE);
        assert_eq!(&v[0..4], &1.0f32.to_le_bytes()); // pos x
        assert_eq!(&v[12..16], &0.0f32.to_le_bytes()); // normal x
        assert_eq!(&v[20..24], &1.0f32.to_le_bytes()); // normal z (+Z default)
        assert_eq!(&v[24..28], &0.0f32.to_le_bytes()); // uv u
    }

    #[test]
    fn enu_to_ue_negates_north_and_scales() {
        assert_eq!(enu_pos_to_ue(DVec3::new(1.0, 2.0, 3.0)), [100.0, -200.0, 300.0]);
        assert_eq!(enu_dir_to_ue(DVec3::new(1.0, 1.0, 1.0)), [1.0, -1.0, 1.0]);
    }

    #[test]
    fn ue_cm_to_enu_is_inverse_of_pos() {
        let enu = DVec3::new(1.5, -2.5, 3.5);
        let ue = enu_pos_to_ue(enu);
        let back = ue_cm_to_enu([ue[0] as f64, ue[1] as f64, ue[2] as f64]);
        assert!((back - enu).length() < 1e-4);
    }

    #[test]
    fn aabb_sphere_center_and_radius() {
        let (c, r) = aabb_to_sphere([-1.0, -2.0, -2.0], [1.0, 2.0, 2.0]);
        assert_eq!(c, [0.0, 0.0, 0.0]);
        assert!((r - 0.5 * (4.0f32 + 16.0 + 16.0).sqrt()).abs() < 1e-6);
    }
}
