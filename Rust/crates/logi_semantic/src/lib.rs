//! `logi_semantic` — the **baked semantic tile** layer (ADR-0011, Milestone HE-0).
//!
//! Reads the offline [`Tools/logi_bake`] output (a versioned, content-addressed tile store) and turns it
//! into the engine's render contract: heightfield **terrain** tiles → interleaved 32-byte vertices +
//! `u32` indices, ready for the actorless frame packet. No `unsafe`, no Unreal types, deterministic.
//!
//! The bake (Python/ML, off-engine) is the *producer*; this crate is the *consumer* — the two agree on
//! the explicit little-endian [`format`] byte layout (the cross-language contract: a pure-Rust round-trip
//! plus a Python-generated golden fixture both gate it).
//!
//! HE-0 scope = **terrain only**. Trees/foliage (instance batches), water, and buildings extend the
//! format + this crate in later HE milestones; the format is **versioned** so the runtime refuses an
//! unknown major rather than misreading it.

#![forbid(unsafe_code)]

pub mod format;
pub mod mesh;
pub mod store;
pub mod world;

// WIP / TODO (off by default): the future voxel terrain lane. Compiles a skeleton only under the `voxel`
// cargo feature; see the module docs. Not wired into the runtime — a Rust-owned lane for if/when voxel
// terrain is ever needed (dig/destructible/caves), NOT the actor-based Voxel Plugin.
#[cfg(feature = "voxel")]
pub mod voxel;

pub use format::{SemanticTile, LST_VERSION};
pub use mesh::{triangulate, triangulate_enu, MeshingMode, TerrainMesh, VERTEX_STRIDE};
pub use store::{BakedStore, BakedTileSource, Manifest, MANIFEST_FORMAT_VERSION};
pub use world::{BakedRenderTile, BakedWorld};

/// A tile in the baked local quadtree: `level` (0 = the single root over the AOI bbox), `x`/`y` in
/// `0..2^level` (`x` = east, `y` = north). The `.lst` filename is `L{level}_X{x}_Y{y}.lst`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TileId {
    pub level: u8,
    pub x: u32,
    pub y: u32,
}

impl TileId {
    pub fn new(level: u8, x: u32, y: u32) -> Self {
        Self { level, x, y }
    }

    /// Stable `u64` packing for runtime maps: `level<<56 | (x & 2^28-1)<<28 | (y & 2^28-1)`.
    /// 28 bits each covers `level <= 28` (a sub-millimetre leaf over any planetary AOI) with room to spare.
    pub fn key(self) -> u64 {
        ((self.level as u64) << 56) | (((self.x as u64) & 0x0FFF_FFFF) << 28) | ((self.y as u64) & 0x0FFF_FFFF)
    }

    /// The tile's filename within the store (`tiles/` subdir).
    pub fn file_name(self) -> String {
        format!("L{}_X{}_Y{}.lst", self.level, self.x, self.y)
    }
}

/// Errors decoding a baked tile or opening a store. No panics cross out of this crate.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SemanticError {
    #[error("truncated tile (got {got} bytes, need {need})")]
    Truncated { got: usize, need: usize },
    #[error("bad magic (not a .lst tile)")]
    BadMagic,
    #[error("unsupported tile/manifest version {0}")]
    Version(u32),
    #[error("crc32 mismatch — corrupt tile")]
    Crc,
    #[error("bad grid (grid_n < 2 or height count mismatch)")]
    BadGrid,
    #[error("tile {0:?} not in store")]
    TileNotFound(TileId),
    #[error("io: {0}")]
    Io(String),
    #[error("manifest: {0}")]
    Manifest(String),
}

/// CRC-32 (IEEE / zlib `crc32`: reflected, polynomial `0xEDB88320`, init+xorout `0xFFFFFFFF`) —
/// byte-compatible with Python `zlib.crc32(data) & 0xffffffff`, so this decoder validates the integrity
/// of tiles the Python writer emits. Table-less (the tile count is small; clarity over throughput).
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32_matches_known_vectors() {
        // Canonical CRC-32/ISO-HDLC check value: "123456789" → 0xCBF43926 (matches Python zlib.crc32).
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0);
        assert_eq!(crc32(b"The quick brown fox jumps over the lazy dog"), 0x414F_A339);
    }

    #[test]
    fn tile_key_is_stable_and_distinct() {
        assert_eq!(TileId::new(0, 0, 0).key(), 0);
        assert_ne!(TileId::new(2, 1, 3).key(), TileId::new(2, 3, 1).key());
        assert_eq!(TileId::new(3, 5, 7).file_name(), "L3_X5_Y7.lst");
    }
}
