//! **LogiSky S1** (ADR-0022): the atmosphere as a Rust-owned model. This crate is the sky cell's
//! interior — physical parameters, the CPU LUT bakers, and the event-gated per-frame derivation — with
//! NO renderer knowledge: everything here is deterministic, headless-testable plain data. The FFI lane
//! (`LogiSkyFrame` + LUT byte buffers) and the UE GPU shell are S2
//! ([Docs/Plans/LogiSky-Atmosphere-Remake.md]).
//!
//! Model: Hillaire 2020 ("A Scalable and Production Ready Sky and Atmosphere Rendering Technique") —
//! the same family the stock UE actor implements, so visual parity is a tuning exercise, not a model
//! gap. Three constituents: Rayleigh (exponential), Mie (exponential, scattering + absorption,
//! Cornette-Shanks g), ozone (tent profile, absorption only).
//!
//! **Sphere vs ellipsoid (ADR-0022 law 3):** the LUTs are spherically symmetric by construction — they
//! are parameterized by altitude, baked against a NOMINAL planet radius. Ellipsoid truth enters at
//! EVALUATION time: the consumer derives altitude above the WGS84 ellipsoid (`SkyState::derive`, and
//! the S2 AP pass intersects view rays with the ellipsoid). The radius affects the baked tables only
//! through curvature, where the ±11 km latitude variation on 6 371 km is ~0.2% — far below the model's
//! own step-count error. The bake is therefore origin-independent and re-runs only on PARAMETER change.
//!
//! Bi-tree shape (ADR-0019 §4): the LUT buffers are flat `f32` RGBA rows sized for one flat GPU upload
//! (bottom shaped for the top); the bake and the derived frame are event-gated so a parked camera under
//! an unchanged sun costs zero sky work (top shaped for the bottom).
//!
//! **Fidelity tiers (ADR-0023 — one truth, many formulas).** Every quantity here is a FAMILY of
//! evaluators off the one [`AtmosphereParams`] truth. Transmittance ships two tiers today:
//! the **reference** ([`transmittance_to_top`] — direct numerical integration, step count explicit;
//! its own oracle is the closed-form zenith golden test) and the **baked** tier (the LUT +
//! [`sample_transmittance_lut`], bound declared by [`TRANSMITTANCE_LUT_MAX_REL_ERROR`] and ENFORCED by
//! the cross-tier agreement test). Cheaper analytic tiers (e.g. Chapman-function transmittance for
//! distant/secondary evaluations) join the family with their own declared, tested bounds when a
//! measured need adds them — never as private formulas beside the truth.

#![forbid(unsafe_code)]

use glam::DVec3;
use logi_geospatial::Ellipsoid;

// ---- Parameters -------------------------------------------------------------------------------------

/// The atmosphere's physical parameters — the sky cell's authoritative state. Units are per-metre and
/// metres (converted from the literature's per-km values in [`AtmosphereParams::earth`]); colours are
/// linear sRGB triplets. Changing any field invalidates the baked LUTs ([`SkyState::set_params`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AtmosphereParams {
    /// Rayleigh scattering coefficient at sea level (per metre, RGB). Earth: (5.802, 13.558, 33.1) e-6/m.
    pub rayleigh_scattering_per_m: [f64; 3],
    /// Rayleigh density scale height (metres). Earth: 8 km.
    pub rayleigh_scale_height_m: f64,
    /// Mie scattering coefficient at sea level (per metre, grey). Earth: 3.996e-6/m.
    pub mie_scattering_per_m: f64,
    /// Mie absorption coefficient at sea level (per metre, grey). Earth: 4.40e-6/m.
    pub mie_absorption_per_m: f64,
    /// Cornette-Shanks anisotropy for the Mie phase (used by the S2 shader; carried with the model).
    pub mie_g: f64,
    /// Mie density scale height (metres). Earth: 1.2 km.
    pub mie_scale_height_m: f64,
    /// Ozone absorption coefficient at the tent peak (per metre, RGB). Earth: (0.650, 1.881, 0.085) e-6/m.
    pub ozone_absorption_per_m: [f64; 3],
    /// Ozone tent-profile centre altitude (metres). Earth: 25 km.
    pub ozone_center_m: f64,
    /// Ozone tent half-width (metres) — density falls linearly to zero this far from the centre. Earth: 15 km.
    pub ozone_half_width_m: f64,
    /// Ground albedo for the multi-scattering bounce (linear RGB). Earth-like default: 0.40 grey.
    pub ground_albedo: [f64; 3],
    /// The NOMINAL planet radius the spherical LUT geometry bakes against (metres) — see the crate doc's
    /// sphere-vs-ellipsoid note. Default: the WGS84 mean radius.
    pub bottom_radius_m: f64,
    /// Atmosphere shell thickness above `bottom_radius_m` (metres). Earth: 100 km.
    pub atmosphere_height_m: f64,
}

impl AtmosphereParams {
    /// Earth, per Hillaire 2020 §4 (the values the stock UE actor also defaults to), with the nominal
    /// radius set to the WGS84 mean radius so LUT altitude ≈ ellipsoid altitude everywhere on the globe.
    pub fn earth() -> Self {
        let e = Ellipsoid::wgs84();
        // Mean radius (2a + b) / 3 — the IUGG R1 definition.
        let mean_radius = (2.0 * logi_geospatial::WGS84_SEMI_MAJOR_AXIS + logi_geospatial::WGS84_SEMI_MINOR_AXIS) / 3.0;
        let _ = e; // the ellipsoid itself is consumed by SkyState::derive; the bake is spherical
        Self {
            rayleigh_scattering_per_m: [5.802e-6, 13.558e-6, 33.1e-6],
            rayleigh_scale_height_m: 8_000.0,
            mie_scattering_per_m: 3.996e-6,
            mie_absorption_per_m: 4.40e-6,
            mie_g: 0.8,
            mie_scale_height_m: 1_200.0,
            ozone_absorption_per_m: [0.650e-6, 1.881e-6, 0.085e-6],
            ozone_center_m: 25_000.0,
            ozone_half_width_m: 15_000.0,
            ground_albedo: [0.40, 0.40, 0.40],
            bottom_radius_m: mean_radius,
            atmosphere_height_m: 100_000.0,
        }
    }

    /// Top-of-atmosphere radius (metres).
    #[inline]
    pub fn top_radius_m(&self) -> f64 {
        self.bottom_radius_m + self.atmosphere_height_m
    }

    /// The three constituents' unitless density multipliers at altitude `h` metres above the ground:
    /// `(rayleigh, mie, ozone)`. Negative altitudes clamp to the sea-level density (terrain below the
    /// nominal sphere must not read super-dense air).
    #[inline]
    fn densities(&self, h_m: f64) -> (f64, f64, f64) {
        let h = h_m.max(0.0);
        let rayleigh = (-h / self.rayleigh_scale_height_m).exp();
        let mie = (-h / self.mie_scale_height_m).exp();
        let ozone = (1.0 - (h - self.ozone_center_m).abs() / self.ozone_half_width_m).max(0.0);
        (rayleigh, mie, ozone)
    }

    /// Total extinction σ_t at altitude `h` (per metre, RGB): Rayleigh scattering + Mie scattering +
    /// Mie absorption + ozone absorption, each at its local density.
    #[inline]
    fn extinction(&self, h_m: f64) -> [f64; 3] {
        let (dr, dm, doz) = self.densities(h_m);
        let mie_ext = (self.mie_scattering_per_m + self.mie_absorption_per_m) * dm;
        [
            self.rayleigh_scattering_per_m[0] * dr + mie_ext + self.ozone_absorption_per_m[0] * doz,
            self.rayleigh_scattering_per_m[1] * dr + mie_ext + self.ozone_absorption_per_m[1] * doz,
            self.rayleigh_scattering_per_m[2] * dr + mie_ext + self.ozone_absorption_per_m[2] * doz,
        ]
    }

    /// Total scattering σ_s at altitude `h` (per metre, RGB): Rayleigh + Mie scattering (no absorbers) —
    /// the in-scatter source term the multi-scattering bake integrates.
    #[inline]
    fn scattering(&self, h_m: f64) -> [f64; 3] {
        let (dr, dm, _) = self.densities(h_m);
        let mie = self.mie_scattering_per_m * dm;
        [
            self.rayleigh_scattering_per_m[0] * dr + mie,
            self.rayleigh_scattering_per_m[1] * dr + mie,
            self.rayleigh_scattering_per_m[2] * dr + mie,
        ]
    }
}

// ---- Ray/sphere helpers (the bake's only geometry) ---------------------------------------------------

/// Distance along the ray from radius `r`, direction cosine `mu` (vs the local zenith), to the sphere of
/// radius `sphere_r` — the NEAREST positive root, or `None` if the ray misses it. Standard quadratic with
/// the origin on the zenith axis (the bake's symmetry): `t² + 2 r μ t + r² − R² = 0`.
#[inline]
fn distance_to_sphere(r: f64, mu: f64, sphere_r: f64) -> Option<f64> {
    let disc = r * r * (mu * mu - 1.0) + sphere_r * sphere_r;
    if disc < 0.0 {
        return None;
    }
    let sq = disc.sqrt();
    let t0 = -r * mu - sq;
    let t1 = -r * mu + sq;
    if t0 > 0.0 {
        Some(t0)
    } else if t1 > 0.0 {
        Some(t1)
    } else {
        None
    }
}

/// Altitude above the nominal ground of the point `t` metres along the ray `(r, mu)`.
#[inline]
fn altitude_along(r: f64, mu: f64, t: f64, bottom: f64) -> f64 {
    (r * r + t * t + 2.0 * r * mu * t).sqrt() - bottom
}

/// Optical-depth integral of `σ` (RGB per metre at altitude) along `(r, mu)` for `dist` metres, midpoint
/// rule with `steps` samples. The bake's one integrator — transmittance is `exp(-result)`.
fn optical_depth(params: &AtmosphereParams, r: f64, mu: f64, dist: f64, steps: usize) -> [f64; 3] {
    let dt = dist / steps as f64;
    let mut acc = [0.0f64; 3];
    for i in 0..steps {
        let t = (i as f64 + 0.5) * dt;
        let h = altitude_along(r, mu, t, params.bottom_radius_m);
        let e = params.extinction(h);
        acc[0] += e[0] * dt;
        acc[1] += e[1] * dt;
        acc[2] += e[2] * dt;
    }
    acc
}

/// Transmittance (RGB, each in `(0, 1]`) from radius `r`, direction cosine `mu`, to the TOP of the
/// atmosphere — `exp(-optical depth)`. Returns black if the ray hits the ground first (the planet
/// occludes). This direct integral is the CPU-side reference; the GPU samples the baked LUT of the same
/// function.
pub fn transmittance_to_top(params: &AtmosphereParams, r: f64, mu: f64, steps: usize) -> [f64; 3] {
    if distance_to_sphere(r, mu, params.bottom_radius_m).is_some() && mu < 0.0 {
        return [0.0; 3]; // downward ray into the ground — fully occluded before the top
    }
    let Some(d_top) = distance_to_sphere(r, mu, params.top_radius_m()) else {
        return [1.0; 3]; // outside the shell looking away — vacuum
    };
    let od = optical_depth(params, r, mu, d_top, steps);
    [(-od[0]).exp(), (-od[1]).exp(), (-od[2]).exp()]
}

// ---- The transmittance LUT (256×64 RGBA f32) ---------------------------------------------------------

pub const TRANSMITTANCE_W: usize = 256;
pub const TRANSMITTANCE_H: usize = 64;
/// Integration steps per texel. 40 midpoint steps keeps every channel within ~0.5% of a 500-step
/// reference on the Earth set (verified by the golden test's tolerance) at a bake cost of ~2 M samples.
const TRANSMITTANCE_STEPS: usize = 40;

/// Bake the transmittance LUT: `TRANSMITTANCE_W × TRANSMITTANCE_H` RGBA `f32` texels (A = 1, padding for
/// the upload), row-major from the texture's v=0 row. Bruneton parameterization (the same mapping the
/// stock engine and Hillaire use, so the S2 shader's lookup is the published one):
/// v ← x_r = ρ/H with ρ = √(r²−bottom²), H = √(top²−bottom²); u ← x_μ = (d−d_min)/(d_max−d_min) with
/// d = distance to the top boundary, d_min = top−r, d_max = ρ+H. Only non-ground-hitting rays are
/// represented — exactly the domain the parameterization covers.
pub fn bake_transmittance(params: &AtmosphereParams) -> Vec<f32> {
    let bottom = params.bottom_radius_m;
    let top = params.top_radius_m();
    let h_max = (top * top - bottom * bottom).sqrt();
    let mut out = vec![0.0f32; TRANSMITTANCE_W * TRANSMITTANCE_H * 4];
    for row in 0..TRANSMITTANCE_H {
        // Texel centres, per the published lookup's half-texel offsets.
        let x_r = (row as f64 + 0.5) / TRANSMITTANCE_H as f64;
        let rho = h_max * x_r;
        let r = (rho * rho + bottom * bottom).sqrt();
        let d_min = top - r;
        let d_max = rho + h_max;
        for col in 0..TRANSMITTANCE_W {
            let x_mu = (col as f64 + 0.5) / TRANSMITTANCE_W as f64;
            let d = d_min + x_mu * (d_max - d_min);
            // Invert d → μ; d→0 degenerates to the straight-up ray.
            let mu = if d <= 0.0 {
                1.0
            } else {
                ((h_max * h_max - rho * rho - d * d) / (2.0 * r * d)).clamp(-1.0, 1.0)
            };
            let od = optical_depth(params, r, mu, d, TRANSMITTANCE_STEPS);
            let i = (row * TRANSMITTANCE_W + col) * 4;
            out[i] = (-od[0]).exp() as f32;
            out[i + 1] = (-od[1]).exp() as f32;
            out[i + 2] = (-od[2]).exp() as f32;
            out[i + 3] = 1.0;
        }
    }
    out
}

/// The baked transmittance tier's DECLARED error bound vs the reference tier (ADR-0023 clause 2):
/// maximum relative error where the reference transmittance is ≥ [`TRANSMITTANCE_LUT_ABS_FLOOR`], and
/// maximum ABSOLUTE error below it (deep-extinction texels carry no meaningful relative precision —
/// both tiers agree the path is opaque). Enforced by the cross-tier agreement test.
pub const TRANSMITTANCE_LUT_MAX_REL_ERROR: f64 = 0.02;
pub const TRANSMITTANCE_LUT_ABS_FLOOR: f64 = 0.01;

/// Sample the baked transmittance LUT at radius `r`, direction cosine `mu` — the BAKED tier's CPU
/// evaluator (the S2 GPU shader implements this same mapping in HLSL; keeping the CPU twin here is what
/// makes the tier testable against the reference, ADR-0023). Forward Bruneton mapping (the bake's
/// inverse) + clamped bilinear over the RGBA rows. Rays that hit the ground are outside the
/// parameterized domain and return black (fully occluded), matching [`transmittance_to_top`].
pub fn sample_transmittance_lut(lut: &[f32], params: &AtmosphereParams, r: f64, mu: f64) -> [f64; 3] {
    let bottom = params.bottom_radius_m;
    let top = params.top_radius_m();
    if mu < 0.0 && distance_to_sphere(r, mu, bottom).is_some() {
        return [0.0; 3];
    }
    let Some(d) = distance_to_sphere(r, mu, top) else {
        return [1.0; 3]; // outside the shell looking away — vacuum
    };
    let h_max = (top * top - bottom * bottom).sqrt();
    let rho = (r * r - bottom * bottom).max(0.0).sqrt();
    let d_min = top - r;
    let d_max = rho + h_max;
    let x_mu = if d_max > d_min {
        ((d - d_min) / (d_max - d_min)).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let x_r = (rho / h_max).clamp(0.0, 1.0);
    // Texel-space bilinear with the half-texel offset the bake used (texel centres at i + 0.5).
    let fx = (x_mu * TRANSMITTANCE_W as f64 - 0.5).clamp(0.0, (TRANSMITTANCE_W - 1) as f64);
    let fy = (x_r * TRANSMITTANCE_H as f64 - 0.5).clamp(0.0, (TRANSMITTANCE_H - 1) as f64);
    let (x0, y0) = (fx.floor() as usize, fy.floor() as usize);
    let (x1, y1) = ((x0 + 1).min(TRANSMITTANCE_W - 1), (y0 + 1).min(TRANSMITTANCE_H - 1));
    let (tx, ty) = (fx - x0 as f64, fy - y0 as f64);
    let texel = |x: usize, y: usize, c: usize| lut[(y * TRANSMITTANCE_W + x) * 4 + c] as f64;
    let mut out = [0.0; 3];
    for (c, o) in out.iter_mut().enumerate() {
        let a = texel(x0, y0, c) * (1.0 - tx) + texel(x1, y0, c) * tx;
        let b = texel(x0, y1, c) * (1.0 - tx) + texel(x1, y1, c) * tx;
        *o = a * (1.0 - ty) + b * ty;
    }
    out
}

// ---- The multi-scattering LUT (32×32 RGBA f32) -------------------------------------------------------

pub const MULTI_SCATTERING_W: usize = 32;
pub const MULTI_SCATTERING_H: usize = 32;
/// Sphere directions / march steps / sun-transmittance steps for the Ψ bake. 64 Fibonacci directions ×
/// 16 march steps × 16 sun steps ≈ 17 M σ-evaluations — sub-second in release, a few seconds in a debug
/// test run; Ψ is a smooth low-frequency field, far below these rates' noise floor (Hillaire §5.1 ships
/// 64 directions at 20 steps).
const MS_DIRECTIONS: usize = 64;
const MS_MARCH_STEPS: usize = 16;
const MS_SUN_T_STEPS: usize = 16;

/// Bake the multi-scattering LUT Ψ (Hillaire §5.1): `MULTI_SCATTERING_W × MULTI_SCATTERING_H` RGBA `f32`
/// texels, u ← cos(sun zenith) mapped from [−1, 1], v ← altitude mapped linearly bottom→top. Each texel
/// integrates second-order in-scatter `L₂` and the scattering transfer `f_ms` over the unit sphere
/// (isotropic phase), then closes the infinite bounce series as `Ψ = L₂ / (1 − f_ms)`. Includes the
/// ground-albedo bounce term and planet shadowing of the sun, per the paper.
pub fn bake_multi_scattering(params: &AtmosphereParams) -> Vec<f32> {
    let bottom = params.bottom_radius_m;
    let top = params.top_radius_m();
    let dirs = fibonacci_sphere(MS_DIRECTIONS);
    let inv_4pi = 1.0 / (4.0 * std::f64::consts::PI);
    let mut out = vec![0.0f32; MULTI_SCATTERING_W * MULTI_SCATTERING_H * 4];
    for row in 0..MULTI_SCATTERING_H {
        let x_alt = (row as f64 + 0.5) / MULTI_SCATTERING_H as f64;
        let r = bottom + x_alt * (top - bottom);
        for col in 0..MULTI_SCATTERING_W {
            let x_sun = (col as f64 + 0.5) / MULTI_SCATTERING_W as f64;
            let cos_sun = (x_sun * 2.0 - 1.0).clamp(-1.0, 1.0);
            let sun_dir = DVec3::new((1.0 - cos_sun * cos_sun).max(0.0).sqrt(), 0.0, cos_sun);
            let origin = DVec3::new(0.0, 0.0, r);

            let mut l2 = [0.0f64; 3];
            let mut f_ms = [0.0f64; 3];
            for dir in &dirs {
                let mu = dir.z; // zenith-axis symmetry: the ray's direction cosine at the origin
                let ground_t = distance_to_sphere(r, mu, bottom);
                let end_t = match ground_t {
                    Some(g) => g,
                    None => distance_to_sphere(r, mu, top).unwrap_or(0.0),
                };
                if end_t <= 0.0 {
                    continue;
                }
                let dt = end_t / MS_MARCH_STEPS as f64;
                let mut od_acc = [0.0f64; 3]; // running optical depth to the sample (midpoint)
                for s in 0..MS_MARCH_STEPS {
                    let t = (s as f64 + 0.5) * dt;
                    let p = origin + *dir * t;
                    let pr = p.length();
                    let h = pr - bottom;
                    let e = params.extinction(h);
                    // Transmittance camera→sample: accumulate to the midpoint (half a step behind, then
                    // advance the full step after sampling — a standard midpoint splitting).
                    let t_cam = [
                        (-(od_acc[0] + e[0] * dt * 0.5)).exp(),
                        (-(od_acc[1] + e[1] * dt * 0.5)).exp(),
                        (-(od_acc[2] + e[2] * dt * 0.5)).exp(),
                    ];
                    od_acc[0] += e[0] * dt;
                    od_acc[1] += e[1] * dt;
                    od_acc[2] += e[2] * dt;

                    let s_coeff = params.scattering(h);
                    // Sun transmittance at the sample (planet-shadowed): μ_sun from the LOCAL zenith.
                    let mu_sun = (p.dot(sun_dir) / pr).clamp(-1.0, 1.0);
                    let t_sun = if distance_to_sphere(pr, mu_sun, bottom).is_some() && mu_sun < 0.0 {
                        [0.0; 3]
                    } else {
                        transmittance_to_top(params, pr, mu_sun, MS_SUN_T_STEPS)
                    };
                    for c in 0..3 {
                        l2[c] += t_cam[c] * s_coeff[c] * t_sun[c] * inv_4pi * dt;
                        f_ms[c] += t_cam[c] * s_coeff[c] * dt;
                    }
                }
                // Ground bounce (Hillaire): the ray that hits the ground adds the sun light the ground
                // re-emits diffusely, attenuated camera→ground.
                if ground_t.is_some() {
                    let pg = origin + *dir * end_t;
                    let pgr = pg.length();
                    let mu_sun_g = (pg.dot(sun_dir) / pgr).clamp(-1.0, 1.0);
                    if mu_sun_g > 0.0 {
                        let t_cam_g = [(-od_acc[0]).exp(), (-od_acc[1]).exp(), (-od_acc[2]).exp()];
                        let t_sun_g = transmittance_to_top(params, pgr, mu_sun_g, MS_SUN_T_STEPS);
                        for c in 0..3 {
                            l2[c] += t_cam_g[c]
                                * t_sun_g[c]
                                * params.ground_albedo[c]
                                * std::f64::consts::FRAC_1_PI
                                * mu_sun_g;
                        }
                    }
                }
            }
            // Average the sphere integral (uniform directions ⇒ 4π/N weights folded with the 1/4π phase
            // ⇒ a plain mean), then close the geometric series.
            let n = dirs.len() as f64;
            let i = (row * MULTI_SCATTERING_W + col) * 4;
            for c in 0..3 {
                let l = l2[c] * (4.0 * std::f64::consts::PI) / n;
                let f = (f_ms[c] / n).min(0.999_9); // physical f_ms < 1; clamp guards the series
                out[i + c] = (l / (1.0 - f)) as f32;
            }
            out[i + 3] = 1.0;
        }
    }
    out
}

/// `n` near-uniform unit directions (Fibonacci sphere) — deterministic, no RNG.
fn fibonacci_sphere(n: usize) -> Vec<DVec3> {
    let golden = std::f64::consts::PI * (3.0 - 5.0f64.sqrt());
    (0..n)
        .map(|i| {
            let z = 1.0 - 2.0 * (i as f64 + 0.5) / n as f64;
            let rho = (1.0 - z * z).max(0.0).sqrt();
            let phi = golden * i as f64;
            DVec3::new(rho * phi.cos(), rho * phi.sin(), z)
        })
        .collect()
}

// ---- SkyState: the cell + the event-gated derived frame ----------------------------------------------

/// The baked tables, stamped with the parameter generation that produced them. The buffers are flat RGBA
/// `f32` rows sized for one flat GPU upload (S2 copies them into two small textures verbatim).
pub struct SkyLuts {
    pub transmittance: Vec<f32>,
    pub multi_scattering: Vec<f32>,
    pub generation: u64,
}

/// The per-frame derived sky block — everything the renderer needs that depends on camera/sun, in ECEF
/// (the S2 FFI converts to render space exactly where the tile rebase does, keeping the convention
/// single-homed). Plain data; renderer-agnostic (ADR-0022 goal 4).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SkyFrame {
    pub camera_ecef: DVec3,
    /// Geodetic (WGS84) altitude of the camera — the LUT altitude coordinate (law 3: ellipsoid truth at
    /// evaluation time).
    pub camera_altitude_m: f64,
    /// Geodetic surface normal under the camera — the local zenith axis.
    pub up_ecef: DVec3,
    pub sun_dir_ecef: DVec3,
    /// cos(sun zenith angle) at the camera — the multi-scattering LUT's u coordinate.
    pub cos_sun_zenith: f64,
    /// cos of the angle from zenith to the planet horizon at this altitude (negative below the horizontal):
    /// the sky-view/AP shader's horizon split.
    pub horizon_cos: f64,
    pub bottom_radius_m: f64,
    pub top_radius_m: f64,
    /// The LUT generation this frame was derived against — the renderer re-uploads textures iff it changes.
    pub lut_generation: u64,
}

/// The sky cell: parameters + lazily-baked LUTs + the event-gated derived frame. Single-owner (lives
/// beside the Geo world on its stream thread in S2); no interior mutability needed.
pub struct SkyState {
    params: AtmosphereParams,
    ellipsoid: Ellipsoid,
    luts: Option<SkyLuts>,
    generation: u64,
    cached: Option<SkyFrame>,
    /// Camera-move threshold (metres) below which `derive` returns the cached frame.
    pub camera_epsilon_m: f64,
    /// Sun-move threshold: cached is reused while `dot(new_sun, cached_sun) > 1 − this`.
    pub sun_epsilon: f64,
    /// Number of full (non-cached) derivations — the event-gate's observability (telemetry + tests).
    pub derives: u64,
}

impl SkyState {
    pub fn new(params: AtmosphereParams) -> Self {
        Self {
            params,
            ellipsoid: Ellipsoid::wgs84(),
            luts: None,
            generation: 1,
            cached: None,
            // Defaults sized for the render lane: a metre of camera motion or ~0.08° of sun motion is
            // imperceptible in the sky terms; both are far below one orbital-frame's typical deltas.
            camera_epsilon_m: 1.0,
            sun_epsilon: 1.0e-6,
            derives: 0,
        }
    }

    pub fn params(&self) -> &AtmosphereParams {
        &self.params
    }

    /// Replace the parameters: bumps the generation and drops the baked LUTs + cached frame (they
    /// re-derive lazily). The ONLY path that invalidates the tables (event-gated cadence, law 4).
    pub fn set_params(&mut self, params: AtmosphereParams) {
        if params != self.params {
            self.params = params;
            self.generation += 1;
            self.luts = None;
            self.cached = None;
        }
    }

    /// The baked LUTs for the current parameters, baking on first use after a parameter change.
    pub fn luts(&mut self) -> &SkyLuts {
        if self.luts.as_ref().is_none_or(|l| l.generation != self.generation) {
            self.luts = Some(SkyLuts {
                transmittance: bake_transmittance(&self.params),
                multi_scattering: bake_multi_scattering(&self.params),
                generation: self.generation,
            });
        }
        self.luts.as_ref().unwrap()
    }

    /// Derive the per-frame sky block for `camera_ecef` + `sun_dir_ecef` (unit not required — normalized
    /// here; a zero sun falls back to zenith). EVENT-GATED: returns the cached frame unchanged while the
    /// camera moved less than [`camera_epsilon_m`](Self::camera_epsilon_m) and the sun less than
    /// [`sun_epsilon`](Self::sun_epsilon) — the parked-camera case costs two compares.
    pub fn derive(&mut self, camera_ecef: DVec3, sun_dir_ecef: DVec3) -> SkyFrame {
        let sun = sun_dir_ecef.normalize_or_zero();
        if let Some(c) = &self.cached {
            if c.lut_generation == self.generation
                && (camera_ecef - c.camera_ecef).length() < self.camera_epsilon_m
                && sun.dot(c.sun_dir_ecef) > 1.0 - self.sun_epsilon
            {
                return *c;
            }
        }
        self.derives += 1;
        let carto = self.ellipsoid.cartesian_to_cartographic(camera_ecef);
        let up = self.ellipsoid.geodetic_surface_normal(camera_ecef);
        let sun = if sun == DVec3::ZERO { up } else { sun };
        let altitude = carto.height.max(0.0);
        // Horizon from the spherical LUT geometry at the local radius (consistent with the bake).
        let r = (self.params.bottom_radius_m + altitude).max(self.params.bottom_radius_m);
        let horizon_cos = -(1.0 - (self.params.bottom_radius_m / r).powi(2)).max(0.0).sqrt();
        let frame = SkyFrame {
            camera_ecef,
            camera_altitude_m: altitude,
            up_ecef: up,
            sun_dir_ecef: sun,
            cos_sun_zenith: up.dot(sun).clamp(-1.0, 1.0),
            horizon_cos,
            bottom_radius_m: self.params.bottom_radius_m,
            top_radius_m: self.params.top_radius_m(),
            lut_generation: self.generation,
        };
        self.cached = Some(frame);
        frame
    }
}

// ---- Tests -------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use logi_geospatial::Cartographic;

    /// The zenith transmittance has a CLOSED FORM for exponential constituents (∫₀^∞ β e^(−h/H) dh = βH;
    /// curvature is irrelevant on the vertical ray; truncation at 100 km is ~e^(−12.5) for Rayleigh) and
    /// the tent ozone (area = half-width × peak). The 40-step bake must land within 1% per channel — the
    /// model's real golden test, no snapshot pinning.
    #[test]
    fn zenith_transmittance_matches_the_closed_form() {
        let p = AtmosphereParams::earth();
        let got = transmittance_to_top(&p, p.bottom_radius_m, 1.0, TRANSMITTANCE_STEPS);
        for (c, &got_c) in got.iter().enumerate() {
            let od = p.rayleigh_scattering_per_m[c] * p.rayleigh_scale_height_m
                + (p.mie_scattering_per_m + p.mie_absorption_per_m) * p.mie_scale_height_m
                + p.ozone_absorption_per_m[c] * p.ozone_half_width_m;
            let want = (-od).exp();
            assert!(
                (got_c - want).abs() / want < 0.01,
                "channel {c}: got {got_c} want {want}"
            );
        }
    }

    #[test]
    fn transmittance_lut_is_physical_and_ordered() {
        let p = AtmosphereParams::earth();
        let lut = bake_transmittance(&p);
        assert_eq!(lut.len(), TRANSMITTANCE_W * TRANSMITTANCE_H * 4);
        for texel in lut.chunks_exact(4) {
            for &v in &texel[..3] {
                // Mathematically T ∈ (0, 1]; the longest grazing paths legitimately UNDERFLOW f32 to
                // exactly 0.0 (optical depth ≫ 88), which the GPU consumes fine — assert [0, 1].
                assert!((0.0..=1.0).contains(&v), "T out of [0,1]: {v}");
            }
            assert_eq!(texel[3], 1.0);
        }
        // Within the ground row (v=0): u=0 is the SHORTEST ray (d = d_min, near-zenith), u=1 the longest
        // grazing path (d = d_max) — transmittance must fall as u rises.
        let row0 = &lut[..TRANSMITTANCE_W * 4];
        assert!(
            row0[1] > row0[(TRANSMITTANCE_W - 1) * 4 + 1],
            "the grazing path must extinct more than the near-zenith path"
        );
        // The top row (near-vacuum residual path) must transmit more than the ground row at the same u.
        let top_row = &lut[(TRANSMITTANCE_H - 1) * TRANSMITTANCE_W * 4..];
        let u_mid = (TRANSMITTANCE_W / 2) * 4;
        assert!(top_row[u_mid] > row0[u_mid]);
    }

    #[test]
    fn multi_scattering_lut_is_finite_and_sun_monotone() {
        let p = AtmosphereParams::earth();
        let lut = bake_multi_scattering(&p);
        assert_eq!(lut.len(), MULTI_SCATTERING_W * MULTI_SCATTERING_H * 4);
        for texel in lut.chunks_exact(4) {
            for &v in &texel[..3] {
                assert!(v.is_finite() && v >= 0.0, "Ψ must be finite ≥ 0");
                // Ψ is NOT bounded by 1 (the ground-albedo bounce can push it past unity near a high
                // sun); ~10 is far above any physical Earth value — a finite-sanity ceiling only.
                assert!(v < 10.0, "Ψ implausibly large (got {v})");
            }
        }
        // At the ground row, Ψ with the sun overhead must exceed Ψ with the sun well below the horizon
        // (only multiply-bounced light survives there).
        let row0 = &lut[..MULTI_SCATTERING_W * 4];
        let lo = row0[4 + 1]; // u near 0 → cos_sun ≈ −1
        let hi = row0[(MULTI_SCATTERING_W - 2) * 4 + 1]; // u near 1 → cos_sun ≈ +1
        assert!(hi > lo, "Ψ(sun up) {hi} must exceed Ψ(sun down) {lo}");
    }

    #[test]
    fn sky_state_event_gate_and_geodesy() {
        let mut s = SkyState::new(AtmosphereParams::earth());
        let e = Ellipsoid::wgs84();
        let cam = e.cartographic_to_cartesian(Cartographic::from_degrees(139.74, 35.60, 3_000.0));
        let sun = DVec3::new(0.3, -0.5, 0.8).normalize();

        let f1 = s.derive(cam, sun);
        assert!((f1.camera_altitude_m - 3_000.0).abs() < 1.0, "ellipsoid altitude");
        assert!(f1.horizon_cos < 0.0, "the horizon dips below horizontal above ground");
        assert!((f1.up_ecef.length() - 1.0).abs() < 1e-12);
        assert_eq!(s.derives, 1);

        // A 10 cm camera shuffle + identical sun: the gate returns the cached frame (no recompute).
        let f2 = s.derive(cam + DVec3::new(0.1, 0.0, 0.0), sun);
        assert_eq!(s.derives, 1);
        assert_eq!(f1, f2);

        // A 10 km move recomputes; a parameter change invalidates LUTs + cache and bumps the generation.
        let _ = s.derive(cam + DVec3::new(10_000.0, 0.0, 0.0), sun);
        assert_eq!(s.derives, 2);
        let g0 = s.luts().generation;
        let mut p = *s.params();
        p.mie_g = 0.76;
        s.set_params(p);
        let f4 = s.derive(cam, sun);
        assert_eq!(s.derives, 3);
        assert_eq!(f4.lut_generation, g0 + 1);
        assert_eq!(s.luts().generation, g0 + 1);
    }

    /// THE CROSS-TIER AGREEMENT GATE (ADR-0023 clause 2): the baked tier (LUT + bilinear sampler) must
    /// stay within its DECLARED bound of the reference tier (200-step direct integration) across its
    /// domain — relative where the reference is meaningfully transparent, absolute in the
    /// deep-extinction floor. A grid over altitude × direction, ground rays excluded (outside the
    /// parameterized domain; both tiers return black there by contract).
    #[test]
    fn baked_tier_agrees_with_the_reference_within_its_declared_bound() {
        let p = AtmosphereParams::earth();
        let lut = bake_transmittance(&p);
        let bottom = p.bottom_radius_m;
        let top = p.top_radius_m();
        for ai in 0..8 {
            let r = bottom + (ai as f64 + 0.5) / 8.0 * (top - bottom);
            // The horizon direction cosine at this radius: sample from just above it up to the zenith.
            let mu_h = -(1.0 - (bottom / r).powi(2)).max(0.0).sqrt();
            for mi in 0..24 {
                let mu = mu_h + 1e-4 + (mi as f64 + 0.5) / 24.0 * (1.0 - mu_h - 1e-4);
                let reference = transmittance_to_top(&p, r, mu, 200);
                let baked = sample_transmittance_lut(&lut, &p, r, mu);
                for c in 0..3 {
                    if reference[c] >= TRANSMITTANCE_LUT_ABS_FLOOR {
                        let rel = (baked[c] - reference[c]).abs() / reference[c];
                        assert!(
                            rel <= TRANSMITTANCE_LUT_MAX_REL_ERROR,
                            "rel error {rel:.4} at r={r:.0} mu={mu:.4} c={c} (ref {} baked {})",
                            reference[c],
                            baked[c]
                        );
                    } else {
                        assert!(
                            (baked[c] - reference[c]).abs() <= TRANSMITTANCE_LUT_ABS_FLOOR,
                            "abs error past the floor at r={r:.0} mu={mu:.4} c={c}"
                        );
                    }
                }
            }
        }
    }
}
