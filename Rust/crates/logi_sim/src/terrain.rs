//! Terrain query surface for the sim (ADR-0002 / ADR-0007). Coarse, deterministic
//! ground sampled from the ported terrain kernel's procedural height field
//! (`logi_terrain_procgen`): identical inputs yield identical outputs on every
//! machine, which is exactly what gameplay (pathing, placement, route cost) needs.
//!
//! The full streaming terrain — residency, Cesium/native packs, LOD — is the
//! renderer's, owned by the terrain kernel and surfaced through the terrain FFI.
//! Gameplay reads only this coarse deterministic representation (plan §10), so the
//! authoritative sim never depends on what happens to be streamed in for display.

use logi_schema::{LogiTileId, Vec3};
use logi_terrain_procgen::TerrainHeights;

/// A deterministic terrain surface sample.
#[derive(Clone, Copy, Debug, Default)]
pub struct TerrainSample {
	/// Surface height in Unreal world units (cm).
	pub height_cm: f64,
	/// Up-facing unit surface normal (+Z up).
	pub normal: Vec3,
	/// Whether the sample came from resident source data.
	pub resident: bool,
}

/// Coarse deterministic terrain sampler backing gameplay queries. Wraps the
/// kernel's procedural height field; every position is deterministically available
/// (gameplay needs no streaming).
#[derive(Debug)]
pub struct Terrain {
	heights: TerrainHeights,
}

impl Default for Terrain {
	fn default() -> Self {
		Self::canonical()
	}
}

impl Terrain {
	pub fn new() -> Self {
		Self::default()
	}

	/// The **canonical shared terrain** — the same field (seed + amplitude + noise params) the renderer
	/// draws (`logi_terrain_procgen::TerrainHeights::canonical`). Gameplay MUST sample this so agents sit
	/// on the terrain you see; do not seed the sim terrain independently of the render scene.
	pub fn canonical() -> Self {
		Self { heights: TerrainHeights::canonical() }
	}

	/// Test/diagnostic helper: a terrain with an arbitrary seed. NOT for gameplay — production terrain
	/// must be [`Terrain::canonical`] so the sim and render fields agree.
	pub fn with_seed(seed: u64) -> Self {
		Self { heights: TerrainHeights::new(seed) }
	}

	/// Coarse terrain is procedural, so every tile is deterministically available.
	pub fn is_tile_resident(&self, _tile: LogiTileId) -> bool {
		true
	}

	/// Sample the coarse surface. `world_x` / `world_y` are world-plane metres (the
	/// full tile -> ECEF rebasing lives in the renderer-facing terrain FFI; gameplay
	/// uses this flat coarse field). Height is returned in Unreal cm.
	pub fn sample_surface(&self, _tile: LogiTileId, world_x: f64, world_y: f64) -> TerrainSample {
		let height_m = self.heights.height_at_f64(world_x, world_y);
		TerrainSample {
			height_cm: height_m * 100.0,
			normal: self.surface_normal(world_x, world_y),
			resident: true,
		}
	}

	/// Coarse route-cost multiplier from local slope (steeper => costlier). Flat
	/// ground is 1.0; gameplay pathing can scale edge costs by this.
	pub fn route_cost(&self, tile: LogiTileId, world_x: f64, world_y: f64) -> f32 {
		let n = self.sample_surface(tile, world_x, world_y).normal;
		// n.z is cos(slope); 1/n.z grows as the surface tilts away from flat.
		(1.0 / n.z.max(0.05)).min(20.0)
	}

	/// Surface normal via central finite differences of the height field.
	fn surface_normal(&self, x: f64, y: f64) -> Vec3 {
		const E: f64 = 1.0; // 1-metre epsilon
		// FIX L4: difference in f64 (height_at_f64), not the f32 height_at — subtracting two near-equal
		// large heights narrowed to f32 first loses the small gradient to catastrophic cancellation.
		let dhx = (self.heights.height_at_f64(x + E, y) - self.heights.height_at_f64(x - E, y)) / (2.0 * E);
		let dhy = (self.heights.height_at_f64(x, y + E) - self.heights.height_at_f64(x, y - E)) / (2.0 * E);
		let (nx, ny, nz) = (-dhx as f32, -dhy as f32, 1.0_f32);
		let len = (nx * nx + ny * ny + nz * nz).sqrt();
		Vec3::new(nx / len, ny / len, nz / len)
	}

	pub fn request_tile(&mut self, _tile: LogiTileId) {}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn surface_is_deterministic_resident_and_unit_normal() {
		let t = Terrain::new();
		let tile = LogiTileId::new(1);
		let a = t.sample_surface(tile, 1234.0, 5678.0);
		let b = t.sample_surface(tile, 1234.0, 5678.0);
		assert_eq!(a.height_cm, b.height_cm); // deterministic
		assert!(a.resident);
		let n = a.normal;
		let len = (n.x * n.x + n.y * n.y + n.z * n.z).sqrt();
		assert!((len - 1.0).abs() < 1e-4, "normal must be unit, got {len}");
		assert!(t.route_cost(tile, 1234.0, 5678.0) >= 1.0);
	}

	#[test]
	fn seed_changes_the_field() {
		let a = Terrain::with_seed(1).sample_surface(LogiTileId::new(1), 100.0, 200.0);
		let b = Terrain::with_seed(2).sample_surface(LogiTileId::new(1), 100.0, 200.0);
		assert_ne!(a.height_cm, b.height_cm);
	}
}
