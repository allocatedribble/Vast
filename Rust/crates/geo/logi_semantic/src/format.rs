//! The baked **Logi Semantic Tile** (`.lst`) binary format (HE-0: a terrain heightfield).
//!
//! Explicit little-endian, byte-for-byte shared with the offline `Tools/logi_bake` writer — this is the
//! cross-language contract. Layout (all LE; no implicit padding beyond the explicit `_pad` fields):
//!
//! | off | type      | field      |                                                            |
//! |----:|-----------|------------|------------------------------------------------------------|
//! |   0 | `[u8; 4]` | magic      | `b"LSTL"`                                                  |
//! |   4 | `u16`     | version    | [`LST_VERSION`]                                            |
//! |   6 | `u16`     | flags      | 0 (reserved)                                              |
//! |   8 | `u8`      | level      | tile quadtree level                                        |
//! |   9 | `u8`      | _pad0      | 0                                                          |
//! |  10 | `u32`     | x          | tile x (east)                                              |
//! |  14 | `u32`     | y          | tile y (north)                                             |
//! |  18 | `u32`     | grid_n     | heightfield is `grid_n × grid_n` samples (>= 2)            |
//! |  22 | `u32`     | _pad1      | 0                                                          |
//! |  26 | `f64`     | origin_e   | ENU east of the tile SW corner (m, from the AOI origin)    |
//! |  34 | `f64`     | origin_n   | ENU north of the tile SW corner (m)                        |
//! |  42 | `f64`     | size_m     | square tile extent (m)                                     |
//! |  50 | `f32`     | h_min      | min height (informational)                                 |
//! |  54 | `f32`     | h_max      | max height (informational)                                 |
//! |  58 | `f32`×N²  | heights    | ENU **up** metres, row-major south→north (j), west→east (i)|
//! | end | `u32`     | crc32      | zlib CRC-32 over all preceding bytes                       |
//!
//! `height(i, j) = heights[j * grid_n + i]`.

use crate::{crc32, SemanticError, TileId};

/// `.lst` magic bytes.
pub const LST_MAGIC: [u8; 4] = *b"LSTL";
/// Current tile format version. The runtime refuses any other version (no silent misread).
pub const LST_VERSION: u16 = 1;

/// Header byte length preceding the height grid.
const HEADER_LEN: usize = 58;

/// A decoded terrain tile: a square `grid_n × grid_n` heightfield (ENU **up** metres) over the tile's
/// metric extent, positioned by its SW-corner ENU offset from the AOI origin. Row-major south→north
/// (outer, `j`), west→east (inner, `i`): `height(i, j) = heights[j * grid_n + i]`.
#[derive(Clone, Debug, PartialEq)]
pub struct SemanticTile {
    pub id: TileId,
    pub grid_n: u32,
    pub origin_e: f64,
    pub origin_n: f64,
    pub size_m: f64,
    pub h_min: f32,
    pub h_max: f32,
    pub heights: Vec<f32>,
}

impl SemanticTile {
    /// Build a tile, computing `h_min`/`h_max` and validating the grid (`grid_n >= 2`, finite heights,
    /// `heights.len() == grid_n²`).
    pub fn new(
        id: TileId,
        grid_n: u32,
        origin_e: f64,
        origin_n: f64,
        size_m: f64,
        heights: Vec<f32>,
    ) -> Result<Self, SemanticError> {
        let count = (grid_n as usize)
            .checked_mul(grid_n as usize)
            .ok_or(SemanticError::BadGrid)?;
        if grid_n < 2 || heights.len() != count {
            return Err(SemanticError::BadGrid);
        }
        let (mut h_min, mut h_max) = (f32::INFINITY, f32::NEG_INFINITY);
        for &h in &heights {
            if !h.is_finite() {
                return Err(SemanticError::BadGrid);
            }
            h_min = h_min.min(h);
            h_max = h_max.max(h);
        }
        Ok(Self {
            id,
            grid_n,
            origin_e,
            origin_n,
            size_m,
            h_min,
            h_max,
            heights,
        })
    }

    /// Encode to the `.lst` byte layout (header + heights + trailing CRC-32).
    pub fn encode(&self) -> Vec<u8> {
        let n = self.grid_n as usize;
        let mut b = Vec::with_capacity(HEADER_LEN + n * n * 4 + 4);
        b.extend_from_slice(&LST_MAGIC);
        b.extend_from_slice(&LST_VERSION.to_le_bytes());
        b.extend_from_slice(&0u16.to_le_bytes()); // flags
        b.push(self.id.level);
        b.push(0u8); // _pad0
        b.extend_from_slice(&self.id.x.to_le_bytes());
        b.extend_from_slice(&self.id.y.to_le_bytes());
        b.extend_from_slice(&self.grid_n.to_le_bytes());
        b.extend_from_slice(&0u32.to_le_bytes()); // _pad1
        b.extend_from_slice(&self.origin_e.to_le_bytes());
        b.extend_from_slice(&self.origin_n.to_le_bytes());
        b.extend_from_slice(&self.size_m.to_le_bytes());
        b.extend_from_slice(&self.h_min.to_le_bytes());
        b.extend_from_slice(&self.h_max.to_le_bytes());
        debug_assert_eq!(b.len(), HEADER_LEN);
        for &h in &self.heights {
            b.extend_from_slice(&h.to_le_bytes());
        }
        let crc = crc32(&b);
        b.extend_from_slice(&crc.to_le_bytes());
        b
    }

    /// Decode the `.lst` byte layout, validating magic, version, grid, total length, and CRC-32.
    pub fn decode(bytes: &[u8]) -> Result<Self, SemanticError> {
        if bytes.len() < HEADER_LEN + 4 {
            return Err(SemanticError::Truncated {
                got: bytes.len(),
                need: HEADER_LEN + 4,
            });
        }
        if bytes[0..4] != LST_MAGIC {
            return Err(SemanticError::BadMagic);
        }
        let version = u16::from_le_bytes([bytes[4], bytes[5]]);
        if version != LST_VERSION {
            return Err(SemanticError::Version(version as u32));
        }
        // flags @ 6..8 reserved/ignored.
        let level = bytes[8];
        // _pad0 @ 9.
        let x = u32::from_le_bytes(bytes[10..14].try_into().unwrap());
        let y = u32::from_le_bytes(bytes[14..18].try_into().unwrap());
        let grid_n = u32::from_le_bytes(bytes[18..22].try_into().unwrap());
        if grid_n < 2 {
            return Err(SemanticError::BadGrid);
        }
        // _pad1 @ 22..26.
        let origin_e = f64::from_le_bytes(bytes[26..34].try_into().unwrap());
        let origin_n = f64::from_le_bytes(bytes[34..42].try_into().unwrap());
        let size_m = f64::from_le_bytes(bytes[42..50].try_into().unwrap());
        let h_min = f32::from_le_bytes(bytes[50..54].try_into().unwrap());
        let h_max = f32::from_le_bytes(bytes[54..58].try_into().unwrap());

        let count = (grid_n as usize)
            .checked_mul(grid_n as usize)
            .ok_or(SemanticError::BadGrid)?;
        let body_len = HEADER_LEN + count * 4;
        let need = body_len + 4;
        if bytes.len() != need {
            return Err(SemanticError::Truncated { got: bytes.len(), need });
        }
        let stored_crc = u32::from_le_bytes(bytes[body_len..need].try_into().unwrap());
        if crc32(&bytes[..body_len]) != stored_crc {
            return Err(SemanticError::Crc);
        }
        let mut heights = Vec::with_capacity(count);
        for k in 0..count {
            let o = HEADER_LEN + k * 4;
            heights.push(f32::from_le_bytes(bytes[o..o + 4].try_into().unwrap()));
        }
        Ok(Self {
            id: TileId { level, x, y },
            grid_n,
            origin_e,
            origin_n,
            size_m,
            h_min,
            h_max,
            heights,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> SemanticTile {
        let n = 4u32;
        let heights: Vec<f32> = (0..n * n).map(|k| k as f32 * 0.5).collect();
        SemanticTile::new(TileId::new(2, 1, 3), n, -100.0, 200.0, 64.0, heights).unwrap()
    }

    #[test]
    fn roundtrip_is_byte_identical_and_lossless() {
        let t = sample();
        let bytes = t.encode();
        assert_eq!(bytes.len(), HEADER_LEN + 16 * 4 + 4);
        let back = SemanticTile::decode(&bytes).unwrap();
        assert_eq!(t, back);
    }

    #[test]
    fn encode_is_deterministic() {
        assert_eq!(sample().encode(), sample().encode());
    }

    #[test]
    fn rejects_bad_magic() {
        let mut b = sample().encode();
        b[0] = b'X';
        assert_eq!(SemanticTile::decode(&b), Err(SemanticError::BadMagic));
    }

    #[test]
    fn detects_crc_tamper() {
        let mut b = sample().encode();
        b[60] ^= 0xFF; // flip a byte inside the height grid
        assert_eq!(SemanticTile::decode(&b), Err(SemanticError::Crc));
    }

    #[test]
    fn rejects_unknown_version_before_crc() {
        let mut b = sample().encode();
        b[4] = 9; // version lo byte → 9; checked before CRC
        assert_eq!(SemanticTile::decode(&b), Err(SemanticError::Version(9)));
    }

    #[test]
    fn rejects_truncated() {
        let b = sample().encode();
        assert!(matches!(
            SemanticTile::decode(&b[..40]),
            Err(SemanticError::Truncated { .. })
        ));
    }

    #[test]
    fn new_rejects_degenerate_grid() {
        assert_eq!(
            SemanticTile::new(TileId::new(0, 0, 0), 1, 0.0, 0.0, 1.0, vec![0.0]),
            Err(SemanticError::BadGrid)
        );
        assert_eq!(
            SemanticTile::new(TileId::new(0, 0, 0), 2, 0.0, 0.0, 1.0, vec![0.0; 3]),
            Err(SemanticError::BadGrid)
        );
    }
}
