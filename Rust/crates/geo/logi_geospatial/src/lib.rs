//! WGS84 ellipsoid geodesy for the Logi globe: geodetic↔ECEF, surface normals, and local ENU
//! (East-North-Up) frames. Pure `f64` math on `glam` — the foundation for the actorless 3D-Tiles /
//! real-world-terrain stack (ADR-0009). Every tile / agent / camera position is geocentric ECEF f64,
//! rebased to a camera-tracking floating origin for f32 rendering (the globe precision model, plan §9.4).
//!
//! The math mirrors cesium-native's `CesiumGeospatial::Ellipsoid` (cartographicToCartesian /
//! scaleToGeodeticSurface / geodeticSurfaceNormal) so results agree with the tilesets we stream.

#![forbid(unsafe_code)]

use glam::{DMat3, DVec3};

/// WGS84 semi-major (equatorial) axis, metres.
pub const WGS84_SEMI_MAJOR_AXIS: f64 = 6_378_137.0;
/// WGS84 semi-minor (polar) axis, metres.
pub const WGS84_SEMI_MINOR_AXIS: f64 = 6_356_752.314_245_179;

/// A geodetic position: `longitude` & `latitude` in **radians**, `height` above the ellipsoid in **metres**.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Cartographic {
    pub longitude: f64,
    pub latitude: f64,
    pub height: f64,
}

impl Cartographic {
    #[inline]
    pub const fn new(longitude: f64, latitude: f64, height: f64) -> Self {
        Self {
            longitude,
            latitude,
            height,
        }
    }

    /// From degrees — a convenience for human-authored placements (e.g. KML `<longitude>/<latitude>`).
    #[inline]
    pub fn from_degrees(longitude_deg: f64, latitude_deg: f64, height: f64) -> Self {
        Self::new(longitude_deg.to_radians(), latitude_deg.to_radians(), height)
    }
}

/// The unit geodetic surface normal (the local "up") at a geodetic longitude/latitude (radians), in ECEF.
/// Exactly unit by construction.
#[inline]
pub fn geodetic_surface_normal_cartographic(longitude: f64, latitude: f64) -> DVec3 {
    let cos_lat = latitude.cos();
    DVec3::new(cos_lat * longitude.cos(), cos_lat * longitude.sin(), latitude.sin())
}

/// A triaxial ellipsoid (WGS84 by default). All positions are geocentric ECEF metres.
#[derive(Clone, Copy, Debug)]
pub struct Ellipsoid {
    pub radii: DVec3,
    pub radii_squared: DVec3,
    pub one_over_radii_squared: DVec3,
}

impl Default for Ellipsoid {
    fn default() -> Self {
        Self::wgs84()
    }
}

impl Ellipsoid {
    /// The WGS84 reference ellipsoid.
    pub fn wgs84() -> Self {
        Self::from_radii(WGS84_SEMI_MAJOR_AXIS, WGS84_SEMI_MAJOR_AXIS, WGS84_SEMI_MINOR_AXIS)
    }

    /// A custom triaxial ellipsoid from its three radii (metres).
    pub fn from_radii(x: f64, y: f64, z: f64) -> Self {
        let radii = DVec3::new(x, y, z);
        let radii_squared = radii * radii;
        Self {
            radii,
            radii_squared,
            one_over_radii_squared: DVec3::ONE / radii_squared,
        }
    }

    /// Unit normal of the ellipsoid implicit surface at an ECEF point — the direction of `∇(xᵀ M x)`
    /// where `M = diag(1/radii²)`. This equals the true geodetic surface normal (the local "up") **only
    /// for points on the ellipsoid surface**; for an off-surface point it is the gradient direction at
    /// that point, which is *not* in general the normal at the nearest surface point. For the geodetic
    /// normal at a given lon/lat use [`geodetic_surface_normal_cartographic`]; to first project onto the
    /// surface use [`Self::scale_to_geodetic_surface`].
    ///
    /// Returns [`DVec3::ZERO`] at the geocenter (the zero gradient has no defined direction).
    #[inline]
    pub fn geodetic_surface_normal(&self, ecef: DVec3) -> DVec3 {
        (ecef * self.one_over_radii_squared).normalize_or_zero()
    }

    /// Geodetic (lon/lat/height) → geocentric ECEF metres.
    pub fn cartographic_to_cartesian(&self, c: Cartographic) -> DVec3 {
        let n = geodetic_surface_normal_cartographic(c.longitude, c.latitude);
        let k = self.radii_squared * n;
        let gamma = n.dot(k).sqrt();
        k / gamma + n * c.height
    }

    /// Geocentric ECEF metres → geodetic (lon/lat/height). Inverse of [`Self::cartographic_to_cartesian`].
    ///
    /// At/near the geocenter the lon/lat/height are undefined (no surface point, no "up" direction); rather
    /// than emit `NaN` lat/lon — which would silently poison every derived ENU frame — this returns the
    /// sentinel `Cartographic { longitude: 0, latitude: 0, height: -semi_minor_axis }` (the geocenter is
    /// one semi-minor axis "below" the prime-meridian/equator surface point). Callers feeding untrusted
    /// ECEF should treat a near-`-semi_minor_axis` height as "degenerate input".
    pub fn cartesian_to_cartographic(&self, ecef: DVec3) -> Cartographic {
        let surface = self.scale_to_geodetic_surface(ecef);
        let n = self.geodetic_surface_normal(surface);
        // `geodetic_surface_normal` returns ZERO only at the geocenter (degenerate gradient); guard it so
        // the lat/lon below can't be `atan2(0,0)`/`asin` of a meaningless direction.
        if n == DVec3::ZERO {
            return Cartographic::new(0.0, 0.0, -self.radii.z);
        }
        let h_vec = ecef - surface;
        Cartographic {
            longitude: n.y.atan2(n.x),
            latitude: n.z.asin(),
            height: h_vec.dot(ecef).signum() * h_vec.length(),
        }
    }

    /// Project an ECEF point onto the ellipsoid surface (its nearest surface point), via the Newton
    /// iteration from cesium-native's `scaleToGeodeticSurface`. Returns the input unchanged for points
    /// at the geocenter (no meaningful surface point). Bounded iteration; converges in a few steps for
    /// any realistic near-surface point.
    pub fn scale_to_geodetic_surface(&self, position: DVec3) -> DVec3 {
        let oors = self.one_over_radii_squared;
        let x2 = position.x * position.x * oors.x;
        let y2 = position.y * position.y * oors.y;
        let z2 = position.z * position.z * oors.z;
        let squared_norm = x2 + y2 + z2;
        if squared_norm <= f64::EPSILON {
            return position; // at/near the geocenter — surface point undefined
        }
        let ratio = (1.0 / squared_norm).sqrt();
        let intersection = position * ratio;
        let gradient = intersection * oors * 2.0;
        let mut lambda = (1.0 - ratio) * position.length() / (0.5 * gradient.length());
        let mut correction = 0.0_f64;
        let (mut xm, mut ym, mut zm) = (1.0, 1.0, 1.0);
        for _ in 0..100 {
            lambda -= correction;
            xm = 1.0 / (1.0 + lambda * oors.x);
            ym = 1.0 / (1.0 + lambda * oors.y);
            zm = 1.0 / (1.0 + lambda * oors.z);
            let (xm2, ym2, zm2) = (xm * xm, ym * ym, zm * zm);
            let func = x2 * xm2 + y2 * ym2 + z2 * zm2 - 1.0;
            if func.abs() <= 1e-12 {
                break;
            }
            let denominator = x2 * (xm2 * xm) * oors.x + y2 * (ym2 * ym) * oors.y + z2 * (zm2 * zm) * oors.z;
            correction = func / (-2.0 * denominator);
        }
        DVec3::new(position.x * xm, position.y * ym, position.z * zm)
    }
}

/// A local **East-North-Up** frame at a geodetic origin — the bridge between geocentric ECEF and a
/// tangent-plane local frame. Orients streamed tiles and derives the camera "up" on the globe (plan §9.4).
#[derive(Clone, Copy, Debug)]
pub struct EnuFrame {
    pub origin_ecef: DVec3,
    /// Columns are the ECEF east/north/up basis vectors; maps an ENU-local vector → ECEF (rotation only).
    pub enu_to_ecef: DMat3,
    /// The inverse (transpose) rotation; maps an ECEF delta → ENU-local.
    pub ecef_to_enu: DMat3,
}

impl EnuFrame {
    /// Build the ENU frame at a geodetic origin.
    pub fn from_cartographic(ellipsoid: &Ellipsoid, c: Cartographic) -> Self {
        let origin_ecef = ellipsoid.cartographic_to_cartesian(c);
        let up = geodetic_surface_normal_cartographic(c.longitude, c.latitude);
        let east = DVec3::new(-c.longitude.sin(), c.longitude.cos(), 0.0);
        let north = up.cross(east);
        let enu_to_ecef = DMat3::from_cols(east, north, up);
        Self {
            origin_ecef,
            enu_to_ecef,
            ecef_to_enu: enu_to_ecef.transpose(),
        }
    }

    /// Build the ENU frame at the geodetic location of an ECEF point.
    pub fn from_ecef(ellipsoid: &Ellipsoid, ecef: DVec3) -> Self {
        Self::from_cartographic(ellipsoid, ellipsoid.cartesian_to_cartographic(ecef))
    }

    /// An ECEF point → its East-North-Up coordinates relative to this frame's origin.
    #[inline]
    pub fn ecef_to_local(&self, ecef: DVec3) -> DVec3 {
        self.ecef_to_enu * (ecef - self.origin_ecef)
    }

    /// East-North-Up coordinates (relative to the origin) → ECEF.
    #[inline]
    pub fn local_to_ecef(&self, enu: DVec3) -> DVec3 {
        self.origin_ecef + self.enu_to_ecef * enu
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EPS_M: f64 = 1e-6; // sub-micrometre on ~6.4e6 m coords

    #[test]
    fn equator_prime_meridian_is_semi_major_on_x() {
        let e = Ellipsoid::wgs84();
        let p = e.cartographic_to_cartesian(Cartographic::new(0.0, 0.0, 0.0));
        assert!((p.x - WGS84_SEMI_MAJOR_AXIS).abs() <= EPS_M, "{p:?}");
        assert!(p.y.abs() <= EPS_M && p.z.abs() <= EPS_M, "{p:?}");
    }

    #[test]
    fn north_pole_is_semi_minor_on_z() {
        let e = Ellipsoid::wgs84();
        let p = e.cartographic_to_cartesian(Cartographic::new(0.0, std::f64::consts::FRAC_PI_2, 0.0));
        assert!((p.z - WGS84_SEMI_MINOR_AXIS).abs() <= EPS_M, "{p:?}");
        assert!(p.x.abs() <= EPS_M && p.y.abs() <= EPS_M, "{p:?}");
    }

    #[test]
    fn geodetic_ecef_roundtrips() {
        let e = Ellipsoid::wgs84();
        let cases = [
            Cartographic::from_degrees(-75.596_701_345_9, 40.038_697_749_4, 50.0), // AGI HQ sample
            Cartographic::from_degrees(0.0, 0.0, 0.0),
            Cartographic::from_degrees(180.0, 0.0, 1000.0),
            Cartographic::from_degrees(-122.0, 37.0, -50.0),
            Cartographic::from_degrees(2.3, 48.8, 12_345.0),
            Cartographic::from_degrees(139.7, 35.7, 0.0),
        ];
        for c in cases {
            let ecef = e.cartographic_to_cartesian(c);
            let back = e.cartesian_to_cartographic(ecef);
            assert!((back.longitude - c.longitude).abs() <= 1e-11, "lon {c:?} -> {back:?}");
            assert!((back.latitude - c.latitude).abs() <= 1e-11, "lat {c:?} -> {back:?}");
            assert!((back.height - c.height).abs() <= 1e-4, "h {c:?} -> {back:?}");
            // 0.1 mm
        }
    }

    #[test]
    fn enu_basis_is_orthonormal_right_handed() {
        let e = Ellipsoid::wgs84();
        let c = Cartographic::from_degrees(-75.6, 40.0, 50.0);
        let f = EnuFrame::from_cartographic(&e, c);
        let (east, north, up) = (f.enu_to_ecef.x_axis, f.enu_to_ecef.y_axis, f.enu_to_ecef.z_axis);
        for v in [east, north, up] {
            assert!((v.length() - 1.0).abs() <= 1e-12, "not unit: {v:?}");
        }
        assert!(east.dot(north).abs() <= 1e-12);
        assert!(north.dot(up).abs() <= 1e-12);
        assert!(up.dot(east).abs() <= 1e-12);
        assert!((east.cross(north) - up).length() <= 1e-12, "not right-handed"); // E×N = U
                                                                                 // "up" is exactly the geodetic surface normal at the origin's lon/lat.
                                                                                 // 1e-12, not 1e-15: a tighter bound is below the agreement `sin`/`cos` guarantee across libm
                                                                                 // implementations (it fails under miri though the math is exact) — keep it portable.
        assert!((up - geodetic_surface_normal_cartographic(c.longitude, c.latitude)).length() <= 1e-12);
    }

    #[test]
    fn enu_local_roundtrips_and_origin_is_zero() {
        let e = Ellipsoid::wgs84();
        let f = EnuFrame::from_cartographic(&e, Cartographic::from_degrees(10.0, 50.0, 0.0));
        assert!(f.ecef_to_local(f.origin_ecef).length() <= 1e-9); // origin → ENU (0,0,0)
                                                                  // 100 m along ENU +Z is 100 m up the surface normal.
        let up_ecef = f.local_to_ecef(DVec3::new(0.0, 0.0, 100.0));
        let up_normal = e.geodetic_surface_normal(f.origin_ecef);
        assert!((up_ecef - (f.origin_ecef + up_normal * 100.0)).length() <= 1e-6);
        // arbitrary ECEF delta round-trips.
        let p = f.origin_ecef + DVec3::new(1234.5, -678.9, 42.0);
        assert!((f.local_to_ecef(f.ecef_to_local(p)) - p).length() <= 1e-6);
    }

    #[test]
    fn geocenter_returns_finite_sentinel_not_nan() {
        let e = Ellipsoid::wgs84();
        // The exact geocenter has no surface point / "up": the normal degenerates to the zero vector and
        // `cartesian_to_cartographic` returns the documented sentinel (lon=lat=0, height=-semi_minor),
        // never NaN.
        assert_eq!(e.geodetic_surface_normal(DVec3::ZERO), DVec3::ZERO);
        let c = e.cartesian_to_cartographic(DVec3::ZERO);
        assert_eq!((c.longitude, c.latitude), (0.0, 0.0));
        assert!((c.height - (-WGS84_SEMI_MINOR_AXIS)).abs() <= 1e-3, "{c:?}");

        // Crucially, *every* near-geocenter point must stay finite (no NaN to poison a derived ENU frame),
        // even where lon/lat are not physically meaningful.
        for p in [
            DVec3::splat(1e-9),
            DVec3::new(1.0, 0.0, 0.0),
            DVec3::new(-1.0, 2.0, -3.0),
        ] {
            let c = e.cartesian_to_cartographic(p);
            assert!(
                c.longitude.is_finite() && c.latitude.is_finite() && c.height.is_finite(),
                "non-finite for {p:?}: {c:?}"
            );
        }
    }

    #[test]
    fn height_above_surface_recovered() {
        let e = Ellipsoid::wgs84();
        let surface = e.cartographic_to_cartesian(Cartographic::from_degrees(-75.6, 40.0, 0.0));
        let aloft = surface + e.geodetic_surface_normal(surface) * 400_000.0; // ~LEO altitude
        let back = e.cartesian_to_cartographic(aloft);
        assert!((back.height - 400_000.0).abs() <= 1e-3, "{back:?}");
    }
}
