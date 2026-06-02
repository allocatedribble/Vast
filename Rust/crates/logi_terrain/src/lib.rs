//! `logi_terrain` — terrain authority (ADR-0002, generalized by ADR-0005).
//! Pure Rust: no `bevy`, no Unreal. Deterministic — identical inputs produce
//! identical outputs on every machine. This is the migration target for the
//! former C++ `LogiTerrain` FFI stub.

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

/// The terrain kernel: owns residency and sampling. Currently a flat-ground
/// stub; the real heightfield + streaming model lands here.
#[derive(Debug, Default)]
pub struct Terrain {
    // Resident-tile set + source cache will live here.
}

impl Terrain {
    pub fn new() -> Self {
        Self::default()
    }

    /// True if the tile's source data is resident and queryable.
    pub fn is_tile_resident(&self, _tile: LogiTileId) -> bool {
        false
    }

    /// Deterministic surface sample. Flat ground (z up) until the kernel exists.
    pub fn sample_surface(&self, _tile: LogiTileId, _local_x: f64, _local_y: f64) -> TerrainSample {
        TerrainSample {
            height_cm: 0.0,
            normal: Vec3::new(0.0, 0.0, 1.0),
            resident: false,
        }
    }

    /// Ask the kernel to make a tile resident. Streaming decision is the kernel's.
    pub fn request_tile(&mut self, _tile: LogiTileId) {
        // Streaming queue: stub.
    }
}
