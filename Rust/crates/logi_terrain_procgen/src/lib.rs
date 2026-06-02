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
fn lattice(xi: i64, zi: i64, seed: u64) -> f32 {
    let h = mix64(seed ^ (xi as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ (zi as u64).wrapping_mul(0xC2B2_AE3D_27D4_EB4F));
    // map to [-1, 1]
    (h as f64 / u64::MAX as f64) as f32 * 2.0 - 1.0
}

#[inline]
fn smoothstep(t: f32) -> f32 {
    t * t * (3.0 - 2.0 * t)
}

fn value_noise(x: f32, z: f32, seed: u64) -> f32 {
    let xi = x.floor() as i64;
    let zi = z.floor() as i64;
    let xf = x - xi as f32;
    let zf = z - zi as f32;
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
            amplitude: 400.0,
            octaves: 5,
            lacunarity: 2.0,
            gain: 0.5,
        }
    }

    /// Height at a world position (deterministic; bounded by ±amplitude).
    pub fn height_at(&self, x: f64, z: f64) -> f32 {
        let mut freq = self.base_frequency;
        let mut amp = 1.0_f64;
        let mut sum = 0.0_f64;
        let mut norm = 0.0_f64;
        for o in 0..self.octaves {
            let s = self.seed.wrapping_add((o as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15));
            let n = value_noise((x * freq) as f32, (z * freq) as f32, s) as f64;
            sum += n * amp;
            norm += amp;
            freq *= self.lacunarity;
            amp *= self.gain;
        }
        ((sum / norm.max(1e-9)) * self.amplitude) as f32
    }

    /// Sample a [`HeightPage`] of `res` x `res` over `[origin, origin + size]`, **edge-inclusive**, so a
    /// neighbor tile sharing an edge samples identical world positions ⇒ matching border heights.
    pub fn height_page(&self, origin_x: f64, origin_z: f64, size: f64, res: u16) -> HeightPage {
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
