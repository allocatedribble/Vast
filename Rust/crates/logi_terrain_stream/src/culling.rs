//! View-aware culling geometry (system-boundary §2.3): hierarchical **frustum** culling and
//! **horizon** culling, in world space (`glam::DVec3`, f64 — planet-scale).
//!
//! - **Frustum** is built from the *live* camera basis (`forward`/`up`) + FOV + aspect — the same
//!   per-frame view the LOD picker uses. Inward-oriented planes; a whole quadtree subtree is pruned
//!   when its bounding sphere is fully outside any plane.
//! - **Horizon** culling is parameterized by planet radius so it is ready for the globe endpoint
//!   (3D-Tiles/Cesium). The current flat local world opts out via [`HorizonCuller::flat`]; the
//!   sphere path implements Cesium's scaled-space occlusion test and is unit-tested.

use glam::DVec3;

/// A plane `dot(normal, p) + d = 0`, with `normal` pointing toward the **inside** of the frustum.
#[derive(Clone, Copy, Debug)]
pub struct Plane {
    pub normal: DVec3,
    pub d: f64,
}

impl Plane {
    /// Signed distance from the plane to `p` (positive = inside).
    #[inline]
    pub fn signed_distance(&self, p: DVec3) -> f64 {
        self.normal.dot(p) + self.d
    }
}

/// A 6-plane view frustum (left, right, top, bottom, near, far), all normals pointing inward.
#[derive(Clone, Copy, Debug)]
pub struct Frustum {
    pub planes: [Plane; 6],
}

impl Frustum {
    /// Build from the live camera. `forward`/`up` need not be normalized or exactly orthogonal — they
    /// are re-orthonormalized. `aspect` = `viewport_width / viewport_height`.
    pub fn from_camera(
        pos: DVec3,
        forward: DVec3,
        up: DVec3,
        fov_y_radians: f64,
        aspect: f64,
        near: f64,
        far: f64,
    ) -> Self {
        let f = forward.try_normalize().unwrap_or(DVec3::Z);
        // FIX 7b: if forward is parallel to `up`, f.cross(up) is ~zero; pick an alternate up-axis that is
        // guaranteed non-parallel to `f` (avoid a fallback right-vector collinear with `f`), then re-derive.
        let r = f.cross(up).try_normalize().unwrap_or_else(|| {
            // f is nearly ±Y → Y is parallel to f, use X; otherwise Y is a safe non-parallel axis.
            let alt_up = if f.y.abs() > 0.9 { DVec3::X } else { DVec3::Y };
            f.cross(alt_up).normalize()
        });
        let u = r.cross(f); // re-orthonormalized true up

        // FIX 7a: clamp the FOV into (0, PI) before tan() so an out-of-range/degenerate angle can't blow up.
        let half_v =
            ((fov_y_radians.clamp(1e-3, std::f64::consts::PI - 1e-3)) * 0.5).tan().max(1e-4);
        let half_h = half_v * aspect.max(1e-4);

        // Four frustum corner rays in world space.
        let tl = f + u * half_v - r * half_h;
        let tr = f + u * half_v + r * half_h;
        let bl = f - u * half_v - r * half_h;
        let br = f - u * half_v + r * half_h;

        // Side planes pass through the camera; near/far are offset along forward. Normal orientation is
        // fixed afterward against a point known to be inside, so cross-product winding need not be exact.
        let mut planes = [
            plane_through(pos, bl.cross(tl)),               // left
            plane_through(pos, tr.cross(br)),               // right
            plane_through(pos, tl.cross(tr)),               // top
            plane_through(pos, br.cross(bl)),               // bottom
            plane_with_normal(f, pos + f * near),           // near
            plane_with_normal(-f, pos + f * far),           // far
        ];

        let interior = pos + f * ((near + far) * 0.5);
        for p in planes.iter_mut() {
            if p.signed_distance(interior) < 0.0 {
                p.normal = -p.normal;
                p.d = -p.d;
            }
        }
        Self { planes }
    }

    /// `false` iff the sphere is fully outside at least one plane (then the subtree is culled).
    #[inline]
    pub fn intersects_sphere(&self, center: DVec3, radius: f64) -> bool {
        for p in &self.planes {
            if p.signed_distance(center) < -radius {
                return false;
            }
        }
        true
    }
}

#[inline]
fn plane_through(point: DVec3, normal: DVec3) -> Plane {
    let n = normal.try_normalize().unwrap_or(DVec3::Y);
    Plane { normal: n, d: -n.dot(point) }
}

#[inline]
fn plane_with_normal(normal: DVec3, point: DVec3) -> Plane {
    let n = normal.try_normalize().unwrap_or(DVec3::Z);
    Plane { normal: n, d: -n.dot(point) }
}

/// Horizon (curvature-occlusion) culling. Parameterized by planet sphere so it is ready for the globe
/// endpoint; the flat local world opts out ([`HorizonCuller::flat`]), where nothing is ever occluded.
#[derive(Clone, Copy, Debug)]
pub struct HorizonCuller {
    /// Planet center + radius (world space), or `None` for a flat world (no curvature occlusion).
    sphere: Option<(DVec3, f64)>,
}

impl HorizonCuller {
    #[inline]
    pub fn flat() -> Self {
        Self { sphere: None }
    }
    #[inline]
    pub fn sphere(center: DVec3, radius: f64) -> Self {
        Self { sphere: Some((center, radius)) }
    }

    /// Is `point` visible from `camera` (not occluded by planetary curvature)? Flat world: always.
    /// Sphere world: Cesium's scaled-space horizon-occlusion test (sphere scaled to unit radius).
    pub fn is_visible(&self, camera: DVec3, point: DVec3) -> bool {
        let (center, radius) = match self.sphere {
            None => return true,
            Some(s) => s,
        };
        if radius <= 0.0 {
            return true;
        }
        let cv = (camera - center) / radius;
        let vh2 = cv.dot(cv) - 1.0; // squared horizon distance in scaled space
        if vh2 <= 0.0 {
            return true; // camera at/below the surface: nothing is occluded
        }
        let pt = (point - center) / radius;
        let vt = pt - cv;
        if vt.dot(vt) <= 0.0 {
            return true; // FIX 7c: point coincides with camera → nothing to occlude; avoid div-by-zero.
        }
        let vt_dot_vc = -vt.dot(cv);
        let occluded = vt_dot_vc > vh2 && (vt_dot_vc * vt_dot_vc) / vt.dot(vt) > vh2;
        !occluded
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn forward_z() -> Frustum {
        Frustum::from_camera(
            DVec3::ZERO,
            DVec3::Z,
            DVec3::Y,
            std::f64::consts::FRAC_PI_2, // 90° vertical FOV
            1.0,
            0.1,
            100.0,
        )
    }

    #[test]
    fn sphere_in_front_is_visible() {
        assert!(forward_z().intersects_sphere(DVec3::new(0.0, 0.0, 10.0), 1.0));
    }

    #[test]
    fn sphere_behind_is_culled() {
        assert!(!forward_z().intersects_sphere(DVec3::new(0.0, 0.0, -10.0), 1.0));
    }

    #[test]
    fn sphere_far_to_the_side_is_culled() {
        assert!(!forward_z().intersects_sphere(DVec3::new(100.0, 0.0, 1.0), 1.0));
    }

    #[test]
    fn big_sphere_straddling_near_plane_is_kept() {
        // Center just behind the camera, but the radius reaches into the frustum.
        assert!(forward_z().intersects_sphere(DVec3::new(0.0, 0.0, -0.5), 5.0));
    }

    #[test]
    fn off_axis_basis_is_reorthonormalized() {
        // Non-orthogonal, unnormalized basis must not break plane construction.
        let f = Frustum::from_camera(
            DVec3::ZERO,
            DVec3::new(0.0, 0.0, 2.0),
            DVec3::new(0.0, 0.9, 0.4),
            std::f64::consts::FRAC_PI_3,
            1.6,
            0.5,
            500.0,
        );
        assert!(f.intersects_sphere(DVec3::new(0.0, 0.0, 50.0), 1.0));
        assert!(!f.intersects_sphere(DVec3::new(0.0, 0.0, -50.0), 1.0));
    }

    #[test]
    fn flat_horizon_never_culls() {
        let h = HorizonCuller::flat();
        assert!(h.is_visible(DVec3::new(0.0, 100.0, 0.0), DVec3::new(1.0e6, 0.0, 0.0)));
    }

    #[test]
    fn sphere_horizon_culls_far_side_keeps_near_side() {
        let r = 1000.0;
        let h = HorizonCuller::sphere(DVec3::ZERO, r);
        let camera = DVec3::new(0.0, r + 10.0, 0.0);
        assert!(h.is_visible(camera, DVec3::new(0.0, r, 0.0)), "near surface visible");
        assert!(!h.is_visible(camera, DVec3::new(0.0, -r, 0.0)), "far side occluded");
    }
}
