//! Field-based terrain generation — the shared abstraction that feeds **both** the heightfield mesher and
//! the voxel lane. The procedural source is defined once; each lane consumes it differently:
//!
//! - [`HeightField`] — 2.5D: height (ENU up, metres) at an `(east, north)` position. The heightfield lane
//!   samples it into a [`SemanticTile`] grid ([`sample_tile`]).
//! - [`SignedDistanceField`] — 3D: density at `(x, y, z)` (negative solid, positive air). The voxel lane
//!   (`crate::voxel`) dual-contours it.
//! - [`HeightFieldSdf`] bridges a height field to an SDF (`density = z − height`), so one generator drives
//!   both lanes; caves/overhangs would instead supply a *native* SDF.
//!
//! The procedural fractal is the **canonical** [`TerrainHeights`] (`logi_geo_procgen`) — the *same*
//! deterministic surface gameplay agents stand on (`logi_sim`) and the kernel renderer draws. It is
//! **reused, not duplicated**: this module just adds the [`HeightField`] view + the SDF bridge over it.

use crate::format::SemanticTile;
use crate::{SemanticError, TileId};

/// The canonical deterministic fractal terrain field (re-exported from the kernel). One world identity,
/// shared by gameplay, the kernel renderer, and the new baked/voxel stack.
pub use logi_geo_procgen::TerrainHeights;

/// A 2.5D terrain height field: ENU **up** height (metres) at an `(east, north)` position (metres).
pub trait HeightField {
    fn height(&self, east_m: f64, north_m: f64) -> f64;
}

/// A 3D signed-distance field (the voxel lane's input): negative inside the solid, positive in air, with a
/// roughly unit gradient near the surface. Caves/overhangs/tunnels need a *native* SDF; gentle terrain can
/// be derived from a [`HeightField`] via [`HeightFieldSdf`].
pub trait SignedDistanceField {
    fn distance(&self, x: f64, y: f64, z: f64) -> f64;
}

/// View a [`HeightField`] as a [`SignedDistanceField`]: `density(x, y, z) = z − height(x, y)`. Exact along
/// the vertical and near-unit-gradient on gentle slopes (fine for dual contouring), so the voxel lane can
/// consume a height-field generator for free.
pub struct HeightFieldSdf<H>(pub H);

impl<H: HeightField> SignedDistanceField for HeightFieldSdf<H> {
    #[inline]
    fn distance(&self, x: f64, y: f64, z: f64) -> f64 {
        z - self.0.height(x, y)
    }
}

/// The canonical procedural fractal **is** a [`HeightField`] — so the surface gameplay uses also drives the
/// render mesher and (via [`HeightFieldSdf`]) the voxel lane. `height_at_f64` is the full-precision sampler.
impl HeightField for TerrainHeights {
    #[inline]
    fn height(&self, east_m: f64, north_m: f64) -> f64 {
        self.height_at_f64(east_m, north_m)
    }
}

/// Sample a [`HeightField`] into a [`SemanticTile`] heightfield grid (`grid_n × grid_n`, edge-inclusive so
/// neighbouring tiles share identical edge samples ⇒ crack-free borders). The procgen → baked-format
/// bridge for the heightfield lane.
pub fn sample_tile(
    field: &impl HeightField,
    id: TileId,
    grid_n: u32,
    origin_e: f64,
    origin_n: f64,
    size_m: f64,
) -> Result<SemanticTile, SemanticError> {
    let n = grid_n as usize;
    let span = (n as f64 - 1.0).max(1.0);
    let step = size_m / span;
    let mut heights = Vec::with_capacity(n * n);
    for j in 0..n {
        for i in 0..n {
            let e = origin_e + i as f64 * step;
            let north = origin_n + j as f64 * step;
            heights.push(field.height(e, north) as f32);
        }
    }
    SemanticTile::new(id, grid_n, origin_e, origin_n, size_m, heights)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_field_is_deterministic_and_bounded() {
        let f = TerrainHeights::canonical();
        // Pure function of position → crack-free (a shared edge position gives one height).
        assert_eq!(f.height(123.5, -456.25), f.height(123.5, -456.25));
        for (e, n) in [(0.0, 0.0), (5000.0, -3000.0), (-1e5, 2e5), (12.3, 45.6)] {
            assert!(f.height(e, n).abs() <= f.amplitude + 1e-6, "bounded at ({e},{n})");
        }
    }

    #[test]
    fn field_has_relief() {
        let f = TerrainHeights::new(7);
        let samples: Vec<f64> = (0..64).map(|k| f.height(k as f64 * 137.0, k as f64 * 91.0)).collect();
        let mn = samples.iter().cloned().fold(f64::MAX, f64::min);
        let mx = samples.iter().cloned().fold(f64::MIN, f64::max);
        assert!(mx - mn > 1.0, "field should vary, got range {}", mx - mn);
    }

    #[test]
    fn heightfield_bridges_to_sdf() {
        let f = TerrainHeights::canonical();
        let sdf = HeightFieldSdf(f);
        let (e, n) = (100.0, 200.0);
        let h = f.height(e, n);
        assert!((sdf.distance(e, n, h)).abs() < 1e-9); // on the surface
        assert!(sdf.distance(e, n, h + 10.0) > 0.0); // air above
        assert!(sdf.distance(e, n, h - 10.0) < 0.0); // solid below
    }

    #[test]
    fn sample_tile_from_field_is_crack_free() {
        let f = TerrainHeights::canonical();
        let a = sample_tile(&f, TileId::new(0, 0, 0), 9, 0.0, 0.0, 256.0).unwrap();
        let b = sample_tile(&f, TileId::new(0, 1, 0), 9, 256.0, 0.0, 256.0).unwrap();
        // a's east edge (i = 8) == b's west edge (i = 0), per row j.
        for j in 0..9 {
            assert_eq!(a.heights[j * 9 + 8], b.heights[j * 9], "shared edge row {j}");
        }
        assert!(a.h_max > a.h_min, "tile has relief");
    }
}
