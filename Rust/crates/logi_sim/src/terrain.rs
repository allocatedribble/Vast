//! Terrain authority (ADR-0002), folded into `logi_sim`. Pure, deterministic —
//! identical inputs produce identical outputs on every machine. May be
//! re-extracted to its own `logi_terrain` crate when it grows.

use logi_schema::{LogiTileId, Vec3};

/// A deterministic terrain surface sample.
#[derive(Clone, Copy, Debug, Default)]
pub struct TerrainSample {
	/// Surface height in Unreal world units (cm).
	pub height_cm: f64,
	/// Up-facing surface normal.
	pub normal: Vec3,
	/// Whether the sample came from resident source data.
	pub resident: bool,
}

/// The terrain kernel: owns residency and sampling. Flat-ground stub for now.
#[derive(Debug, Default)]
pub struct Terrain {}

impl Terrain {
	pub fn new() -> Self {
		Self::default()
	}

	pub fn is_tile_resident(&self, _tile: LogiTileId) -> bool {
		false
	}

	pub fn sample_surface(&self, _tile: LogiTileId, _local_x: f64, _local_y: f64) -> TerrainSample {
		TerrainSample {
			height_cm: 0.0,
			normal: Vec3::new(0.0, 0.0, 1.0),
			resident: false,
		}
	}

	pub fn request_tile(&mut self, _tile: LogiTileId) {}
}
