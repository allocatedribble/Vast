//! A camera view frustum's **side** planes in ECEF, for the streaming visibility classification
//! (frustum-aware LOD, plan §9.2 culled-SSE). Near/far are left to the distance cull, so only the 4 side
//! planes are kept. This is a *streaming* cull (decides residency / LOD) that complements Unreal's
//! render-time frustum + occlusion culling — see `Docs/.../Milestone-F` §9.2 and the culling research:
//! out-of-view tiles are kept resident at a coarse LOD (so the engine can still cast their shadows from its
//! larger-than-view shadow frustum), never hard-culled by direction; only distance hard-culls.

use glam::DVec3;

/// The 4 side planes (left / right / bottom / top) of a view frustum in ECEF — each through the camera
/// `apex` with an **inward** unit normal. A sphere is "in view" iff it isn't fully on the outer side of any
/// plane. Built with an angular margin so screen-edge tiles still classify in-view (no edge LOD seam). The
/// [`Default`] — and any degenerate camera basis — is **accept-all**: every sphere is reported in view.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ViewFrustum {
    apex: DVec3,
    /// Inward unit normals; the all-`ZERO` set never rejects (accept-all).
    planes: [DVec3; 4],
}

impl Default for ViewFrustum {
    fn default() -> Self {
        Self {
            apex: DVec3::ZERO,
            planes: [DVec3::ZERO; 4],
        }
    }
}

impl ViewFrustum {
    /// Build from the camera basis in ECEF: `forward` + `up` (need not be exactly orthonormal — the up is
    /// re-orthogonalized), the vertical FOV, the viewport `aspect` (width / height), and an angular `margin`
    /// (radians) that widens the frustum so edge tiles stay full-detail. A degenerate basis (zero forward, or
    /// forward ∥ up) yields the accept-all frustum.
    pub(crate) fn new(apex: DVec3, forward: DVec3, up: DVec3, fov_y_radians: f64, aspect: f64, margin: f64) -> Self {
        let f = forward.normalize_or_zero();
        let r = f.cross(up).normalize_or_zero();
        if f == DVec3::ZERO || r == DVec3::ZERO {
            return Self::default(); // no usable orientation → accept everything
        }
        let u = r.cross(f); // re-orthonormalized up (unit: r ⟂ f, both unit)
        let lim = std::f64::consts::FRAC_PI_2 - 1e-3; // keep half-angles < 90° so the planes stay sane
        let half_v = (fov_y_radians * 0.5 + margin).clamp(0.0, lim);
        let half_h = (((fov_y_radians * 0.5).tan() * aspect.max(1e-3)).atan() + margin).clamp(0.0, lim);
        let (sv, cv) = half_v.sin_cos();
        let (sh, ch) = half_h.sin_cos();
        // Each inward normal is `forward·sin(half) ± axis·cos(half)`: it's ⟂ to that frustum edge and points
        // into the frustum (its dot with `forward` is `sin(half) > 0`).
        Self {
            apex,
            planes: [
                f * sh + r * ch, // left   (inward → +right)
                f * sh - r * ch, // right  (inward → −right)
                f * sv + u * cv, // bottom (inward → +up)
                f * sv - u * cv, // top    (inward → −up)
            ],
        }
    }

    /// Is the sphere at least partially inside the frustum — i.e. not fully on the outer side of any side
    /// plane? Conservative (a sphere straddling a plane counts as inside).
    pub(crate) fn contains_sphere(&self, center: DVec3, radius: f64) -> bool {
        let rel = center - self.apex;
        self.planes.iter().all(|n| rel.dot(*n) >= -radius)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_ahead_rejects_behind_and_side() {
        // Camera at the origin looking +X, up +Z, 60° vertical FOV, 16:9, ~3° margin.
        let f = ViewFrustum::new(DVec3::ZERO, DVec3::X, DVec3::Z, 60f64.to_radians(), 16.0 / 9.0, 0.05);
        assert!(f.contains_sphere(DVec3::new(100.0, 0.0, 0.0), 1.0), "straight ahead");
        assert!(!f.contains_sphere(DVec3::new(-100.0, 0.0, 0.0), 1.0), "directly behind");
        assert!(!f.contains_sphere(DVec3::new(0.0, 100.0, 0.0), 1.0), "90° to the side");
        assert!(!f.contains_sphere(DVec3::new(0.0, 0.0, 100.0), 1.0), "90° above");
        // A big sphere straddling the apex is partially in front → accepted.
        assert!(
            f.contains_sphere(DVec3::new(-1.0, 0.0, 0.0), 100.0),
            "huge sphere over the apex"
        );
    }

    #[test]
    fn margin_widens_the_accepted_cone() {
        let cam = DVec3::ZERO;
        // A point just outside the 60° vfov (~31° off-axis vertically) is rejected with no margin…
        let tight = ViewFrustum::new(cam, DVec3::X, DVec3::Z, 60f64.to_radians(), 1.0, 0.0);
        let just_outside = DVec3::new(100.0, 0.0, 62.0); // atan(62/100) ≈ 31.8° > 30°
        assert!(!tight.contains_sphere(just_outside, 1.0));
        // …but accepted once a generous margin widens the frustum.
        let loose = ViewFrustum::new(cam, DVec3::X, DVec3::Z, 60f64.to_radians(), 1.0, 0.2);
        assert!(loose.contains_sphere(just_outside, 1.0));
    }

    #[test]
    fn default_and_degenerate_basis_accept_everything() {
        assert!(ViewFrustum::default().contains_sphere(DVec3::new(-1e6, 2e6, -3e6), 1.0));
        // forward ∥ up → no right axis → accept-all.
        let degen = ViewFrustum::new(DVec3::ZERO, DVec3::Z, DVec3::Z, 1.0, 1.0, 0.0);
        assert!(degen.contains_sphere(DVec3::new(0.0, 0.0, -100.0), 1.0));
    }
}
