//! Foundational types for the Logi terrain kernel: tile/page IDs, world math, config, telemetry.
//!
//! Owned by `logi_terrain_core` per the doctrine: tile/page IDs, world coordinates, config, telemetry,
//! and memory-budget primitives. Math vocabulary is re-exported from `glam`.

/// A quadtree tile address: a `level` plus `(x, y)` within that level's `2^level x 2^level` grid.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub struct TileId {
    pub level: u8,
    pub x: u32,
    pub y: u32,
}

impl TileId {
    #[inline]
    pub const fn new(level: u8, x: u32, y: u32) -> Self {
        Self { level, x, y }
    }

    #[inline]
    pub const fn root() -> Self {
        Self { level: 0, x: 0, y: 0 }
    }

    /// Tiles per side at `level` (2^level). `level` must be < 32.
    #[inline]
    pub const fn side(level: u8) -> u32 {
        // FIX L2: guard the `1u32 << level` shift — `level >= 32` overflows (UB in release, panic in
        // debug). Matches the sibling guards on children()/key()/ancestor_at(); a plain message (no
        // interpolation) keeps this usable in a `const fn`.
        debug_assert!(level < 32, "side() level overflows the 32-bit `1u32 << level` shift");
        1u32 << level
    }

    /// Parent (one level coarser). The root is its own parent.
    #[inline]
    pub fn parent(self) -> Self {
        if self.level == 0 {
            self
        } else {
            Self {
                level: self.level - 1,
                x: self.x >> 1,
                y: self.y >> 1,
            }
        }
    }

    /// The four children (one level finer).
    #[inline]
    pub fn children(self) -> [TileId; 4] {
        // FIX 2: guard level/coord overflow — key layout supports x,y < 2^28 (max level 27).
        debug_assert!(
            self.level < 27,
            "children() at level {} would overflow the 28-bit x/y key field",
            self.level
        );
        let l = self.level + 1;
        let (x, y) = (self.x << 1, self.y << 1);
        [
            TileId::new(l, x, y),
            TileId::new(l, x + 1, y),
            TileId::new(l, x, y + 1),
            TileId::new(l, x + 1, y + 1),
        ]
    }

    /// The ancestor at `level` (must be <= self.level).
    #[inline]
    pub fn ancestor_at(self, level: u8) -> TileId {
        // FIX 3: a coarser-than-self target is a caller bug — it would silently return self.
        debug_assert!(
            level <= self.level,
            "ancestor_at({}) above self.level {}",
            level,
            self.level
        );
        let mut t = self;
        while t.level > level {
            t = t.parent();
        }
        t
    }

    /// A stable packed 64-bit key. Non-overlapping fields: level in bits[56..62] (6 bits, ≤63),
    /// x in bits[28..56] (28 bits), y in bits[0..28] (28 bits). Top bit 63 is left free so callers
    /// can steal it (e.g. `key() << 1` in the scene handle path).
    #[inline]
    pub fn key(self) -> u64 {
        // FIX 1: mask each field to its width so a large x/y can't bleed into the level or x bits,
        // and keep bit 63 free for the handle's `<< 1`.
        debug_assert!(
            self.x < (1 << 28) && self.y < (1 << 28) && self.level < 64,
            "TileId out of key range: level {} x {} y {}",
            self.level,
            self.x,
            self.y
        );
        (((self.level as u64) & 0x3F) << 56) | (((self.x as u64) & 0x0FFF_FFFF) << 28) | ((self.y as u64) & 0x0FFF_FFFF)
    }

    /// Inverse of [`key`] — reconstruct a tile from its packed key.
    #[inline]
    pub fn from_key(key: u64) -> TileId {
        TileId::new(
            ((key >> 56) & 0x3F) as u8,
            ((key >> 28) & 0x0FFF_FFFF) as u32,
            (key & 0x0FFF_FFFF) as u32,
        )
    }
}

/// Opaque page identifier (a unit of streamed content within a tile).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub struct PageId(pub u64);

/// Opaque world-index node identifier.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct NodeId(pub u64);

/// glam is the kernel's math vocabulary. The FFI boundary uses these directly because they are
/// `#[repr(C)]` and byte-identical to Unity.Mathematics (`Vec3`/`DVec3`/`Mat4`/`Quat` ↔
/// `float3`/`double3`/`float4x4`/`quaternion`) — so Unity reads packet data with zero per-field
/// conversion. World-space (planet-scale) values use [`DVec3`] (f64); render/local values use the f32
/// types after the camera-relative rebase Rust performs on upload.
pub use glam::{DVec3, Mat4, Quat, Vec3};

/// Render-local bounding sphere (camera-relative, f32) for culling / screen-space error. `#[repr(C)]`,
/// byte-identical to a C# `{ float3 center; float radius; }`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct LogiBoundingSphere {
    pub center: Vec3,
    pub radius: f32,
}

/// Kernel construction parameters. `#[repr(C)]` — shared with the FFI boundary.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct LogiKernelConfig {
    /// Hard residency budget for streamed tile data, in bytes (0 = library default).
    pub memory_budget_bytes: u64,
    /// Worker threads for IO/decode (0 = library default).
    pub worker_thread_count: u32,
    pub _reserved: u32,
}

impl LogiKernelConfig {
    pub const DEFAULT_BUDGET: u64 = 512 * 1024 * 1024;

    pub fn defaults() -> Self {
        Self {
            memory_budget_bytes: Self::DEFAULT_BUDGET,
            worker_thread_count: 0,
            _reserved: 0,
        }
    }
}

impl Default for LogiKernelConfig {
    fn default() -> Self {
        Self::defaults()
    }
}

/// Performance/health counters, pulled by Unity for debug visualization.
#[derive(Clone, Copy, Debug, Default)]
pub struct Telemetry {
    pub frames: u64,
    pub resident_tiles: u32,
    pub bytes_resident: u64,
    pub peak_bytes_resident: u64,
    pub cache_hits: u64,
    pub cache_misses: u64,
    pub evictions: u64,
    pub requests_issued: u64,
    pub requests_completed: u64,
    pub in_flight: u32,
    pub fallbacks_used: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tile_key_roundtrips() {
        for t in [
            TileId::new(0, 0, 0),
            TileId::new(8, 130, 250),
            TileId::new(12, 4095, 1),
            TileId::new(20, 1 << 19, 7),
            // FIX 1: max 28-bit x/y at a high level — fields must not bleed into each other.
            TileId::new(27, (1 << 28) - 1, (1 << 28) - 1),
            TileId::new(63, 0, (1 << 28) - 1),
        ] {
            assert_eq!(TileId::from_key(t.key()), t, "key roundtrip for {t:?}");
        }
    }

    #[test]
    fn tile_key_leaves_top_bit_free() {
        // FIX 1: the scene handle path does `key() << 1`; the largest valid key must stay < 2^63.
        let max = TileId::new(63, (1 << 28) - 1, (1 << 28) - 1);
        assert!(max.key() < (1u64 << 63), "key {} must leave bit 63 free", max.key());
    }
}
