//! Procedural terrain generation (rendering-strategy.md §5, data-model.md ProcSeedPage/HeightPage).
//!
//! Deterministic, dependency-light: heights are a fractal of hashed value noise (no RNG/clock), so the
//! same world position always yields the same height — which is exactly what gives **crack-free borders**
//! (adjacent tiles that sample a shared edge position get identical heights).

use logi_terrain_pack::HeightPage;

#[inline]
fn mix64(mut h: u64) -> u64 {
    h ^= h >> 33;
    h = h.wrapping_mul(0xff51_afd7_ed55_8ccd);
    h ^= h >> 33;
    h = h.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    h ^ (h >> 33)
}

#[inline]
fn lattice(xi: i64, zi: i64, seed: u64) -> f64 {
    let h = mix64(seed ^ (xi as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ (zi as u64).wrapping_mul(0xC2B2_AE3D_27D4_EB4F));
    // map to [-1, 1] (FIX 11: stay in f64)
    (h as f64 / u64::MAX as f64) * 2.0 - 1.0
}

#[inline]
fn smoothstep(t: f64) -> f64 {
    t * t * (3.0 - 2.0 * t)
}

// FIX 11: keep the lattice/floor/fractional math in f64 so the same world position yields the same
// height at large coordinates (no f32 narrowing of scaled coords ⇒ crack-free borders).
fn value_noise(x: f64, z: f64, seed: u64) -> f64 {
    let xi = x.floor() as i64;
    let zi = z.floor() as i64;
    let xf = x - xi as f64;
    let zf = z - zi as f64;
    let v00 = lattice(xi, zi, seed);
    let v10 = lattice(xi + 1, zi, seed);
    let v01 = lattice(xi, zi + 1, seed);
    let v11 = lattice(xi + 1, zi + 1, seed);
    let sx = smoothstep(xf);
    let sz = smoothstep(zf);
    let a = v00 + (v10 - v00) * sx;
    let b = v01 + (v11 - v01) * sx;
    a + (b - a) * sz
}

/// **Canonical terrain seed** — the SINGLE SOURCE OF TRUTH for which world the terrain *is*, shared
/// by BOTH the gameplay sampler (`logi_sim`) and the render scene (`logi_terrain_scene`). Build the
/// shared field via [`TerrainHeights::canonical`]; never hardcode a different seed in one consumer,
/// or the gameplay ground and the rendered ground describe different surfaces (agents float).
pub const DEFAULT_SEED: u64 = 0xA11CE;

/// **Canonical peak relief in metres** — the field is bounded to ±this. Both the height generation
/// and the renderer's screen-space-error estimate must use this one value (do not override per crate).
pub const DEFAULT_AMPLITUDE_M: f64 = 250.0;

/// Deterministic fractal terrain height field over world coordinates.
#[derive(Clone, Copy, Debug)]
pub struct TerrainHeights {
    pub seed: u64,
    /// Cycles per world unit for the base octave.
    pub base_frequency: f64,
    /// Peak amplitude in world units (meters).
    pub amplitude: f64,
    pub octaves: u32,
    pub lacunarity: f64,
    pub gain: f64,
}

impl TerrainHeights {
    pub fn new(seed: u64) -> Self {
        Self {
            seed,
            base_frequency: 1.0 / 2048.0,
            amplitude: DEFAULT_AMPLITUDE_M,
            octaves: 5,
            lacunarity: 2.0,
            gain: 0.5,
        }
    }

    /// The **canonical shared terrain field** ([`DEFAULT_SEED`] + the default params). BOTH the sim
    /// sampler and the renderer must construct via this so they describe the exact same surface.
    pub fn canonical() -> Self {
        Self::new(DEFAULT_SEED)
    }

    /// Height at a world position in **full f64 precision** (deterministic; bounded by ±amplitude).
    /// Prefer this over [`height_at`](Self::height_at) when *differencing* nearby samples (e.g. a
    /// finite-difference surface normal): subtracting two near-equal heights that have each been
    /// narrowed to f32 loses the small gradient to catastrophic cancellation (FIX L4).
    pub fn height_at_f64(&self, x: f64, z: f64) -> f64 {
        let mut freq = self.base_frequency;
        let mut amp = 1.0_f64;
        let mut sum = 0.0_f64;
        let mut norm = 0.0_f64;
        for o in 0..self.octaves {
            let s = self.seed.wrapping_add((o as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15));
            // FIX 11: pass full-precision f64 scaled coords (no f32 narrowing) into the noise.
            let n = value_noise(x * freq, z * freq, s);
            sum += n * amp;
            norm += amp;
            freq *= self.lacunarity;
            amp *= self.gain;
        }
        (sum / norm.max(1e-9)) * self.amplitude
    }

    /// Height at a world position (deterministic; bounded by ±amplitude).
    #[inline]
    pub fn height_at(&self, x: f64, z: f64) -> f32 {
        self.height_at_f64(x, z) as f32
    }

    /// Sample a [`HeightPage`] of `res` x `res` over `[origin, origin + size]`, **edge-inclusive**, so a
    /// neighbor tile sharing an edge samples identical world positions ⇒ matching border heights.
    pub fn height_page(&self, origin_x: f64, origin_z: f64, size: f64, res: u16) -> HeightPage {
        // FIX 12: downstream height_mesh requires >=2 samples per edge (it builds quads).
        assert!(res >= 2, "height page needs >=2 samples per edge");
        let n = res as usize;
        let step = size / (res as f64 - 1.0).max(1.0);
        let mut heights = Vec::with_capacity(n * n);
        for j in 0..n {
            for i in 0..n {
                let x = origin_x + i as f64 * step;
                let z = origin_z + j as f64 * step;
                heights.push(self.height_at(x, z));
            }
        }
        let holes = vec![false; n * n];
        HeightPage::from_heights(res, res, &heights, &holes)
    }
}
