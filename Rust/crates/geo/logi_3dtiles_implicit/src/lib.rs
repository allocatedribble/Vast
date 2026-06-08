//! 3D Tiles **implicit tiling**: the binary subtree format + Morton/availability indexing, for the
//! actorless 3D-Tiles stack (ADR-0009, Milestone F). A tile flagged `implicitTiling` is the root of a
//! procedurally-subdivided quadtree/octree; `.subtree` files carry which tiles/contents/child-subtrees
//! exist, packed as bitstreams indexed in Morton (Z-order). The traversal synthesises children on the
//! fly from these availabilities.
//!
//! Spec: 3D Tiles 1.1 `specification/ImplicitTiling`. Binary subtree = a 24-byte header
//! (`subt`, version 1, jsonByteLength u64, binaryByteLength u64) + a JSON chunk + a binary chunk.

#![forbid(unsafe_code)]

use serde::Deserialize;

/// What went wrong reading a subtree.
#[derive(thiserror::Error, Clone, Debug, PartialEq, Eq)]
pub enum ImplicitError {
    #[error("unexpected end of subtree data")]
    UnexpectedEof,
    #[error("bad subtree magic")]
    BadMagic,
    #[error("unsupported subtree version: {0}")]
    BadVersion(u32),
    #[error("subtree JSON error: {0}")]
    Json(String),
    /// An availability referenced a bitstream/bufferView/buffer that isn't the internal binary chunk.
    /// (External buffers — `buffer.uri` — are not yet fetched.)
    #[error("availability references an external/unsupported buffer")]
    ExternalBufferUnsupported,
    #[error("bad subtree bufferView")]
    BadBufferView,
}

// ---- Morton (Z-order) interleave ------------------------------------------------------------------

/// 2D Morton code: interleave the bits of `x` (even positions) and `y` (odd positions). Quadtree.
pub fn morton_2d(x: u32, y: u32) -> u64 {
    spread2(x) | (spread2(y) << 1)
}

/// 3D Morton code for an octree: interleave `x`/`y`/`z` into every third bit.
pub fn morton_3d(x: u32, y: u32, z: u32) -> u64 {
    spread3(x) | (spread3(y) << 1) | (spread3(z) << 2)
}

fn spread2(v: u32) -> u64 {
    let mut x = v as u64 & 0x0000_0000_FFFF_FFFF;
    x = (x | (x << 16)) & 0x0000_FFFF_0000_FFFF;
    x = (x | (x << 8)) & 0x00FF_00FF_00FF_00FF;
    x = (x | (x << 4)) & 0x0F0F_0F0F_0F0F_0F0F;
    x = (x | (x << 2)) & 0x3333_3333_3333_3333;
    (x | (x << 1)) & 0x5555_5555_5555_5555
}

fn spread3(v: u32) -> u64 {
    let mut x = v as u64 & 0x1F_FFFF; // 21 bits (3*21 = 63)
    x = (x | (x << 32)) & 0x001F_0000_0000_FFFF;
    x = (x | (x << 16)) & 0x001F_0000_FF00_00FF;
    x = (x | (x << 8)) & 0x100F_00F0_0F00_F00F;
    x = (x | (x << 4)) & 0x10C3_0C30_C30C_30C3;
    (x | (x << 2)) & 0x1249_2492_4924_9249
}

/// The availability **bit index** of a tile at `level` (relative to the subtree root) with the given
/// `relative_morton`: `levelOffset + relativeMorton`, where `levelOffset = (childCount^level - 1) /
/// (childCount - 1)` is the count of nodes in all coarser levels of the subtree.
pub fn bit_index(level: u32, relative_morton: u64, child_count: u32) -> u64 {
    let cc = child_count as u64;
    // Hostile-input guard: an invalid arity (< 2 → divide-by-zero / underflow) or a `level` large enough
    // to overflow `cc^level` (≥ 32 quadtree, ≥ 22 octree — `level` rides the uncapped `subtreeLevels`
    // from tileset.json) would otherwise panic (debug) or wrap to a garbage index (release). Saturate to
    // u64::MAX so `Availability::get` simply reports the tile "unavailable" instead.
    if cc < 2 {
        return u64::MAX;
    }
    match cc.checked_pow(level) {
        Some(p) => ((p - 1) / (cc - 1)).saturating_add(relative_morton),
        None => u64::MAX,
    }
}

// ---- availability ---------------------------------------------------------------------------------

/// A tile/content/child-subtree availability: a constant (all/none) or a packed bitstream (LSB-first
/// within each byte), indexed by [`bit_index`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Availability {
    Constant(bool),
    Bitstream(Vec<u8>),
}

impl Availability {
    /// Is the given bit set?
    #[inline]
    pub fn get(&self, index: u64) -> bool {
        match self {
            Self::Constant(b) => *b,
            Self::Bitstream(bytes) => bytes
                .get((index >> 3) as usize)
                .map(|byte| (byte >> (index & 7)) & 1 == 1)
                .unwrap_or(false),
        }
    }
}

/// A decoded subtree: the three availability bitstreams + the subdivision arity. Query with
/// local (subtree-relative) tile coordinates.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Subtree {
    pub child_count: u32, // 4 (quadtree) or 8 (octree) — from the tileset's implicitTiling
    pub tile_availability: Availability,
    pub content_availability: Vec<Availability>,
    pub child_subtree_availability: Availability,
}

impl Subtree {
    fn morton(&self, x: u32, y: u32, z: u32) -> u64 {
        if self.child_count == 8 {
            morton_3d(x, y, z)
        } else {
            morton_2d(x, y)
        }
    }

    /// Whether `(x, y, z)` is representable without aliasing. The octree Morton (`spread3`) packs only 21
    /// bits per axis, so a coordinate ≥ 2^21 would silently alias another tile's availability bit; the
    /// quadtree Morton (`spread2`) carries full 32-bit x/y losslessly, so only octree needs the guard. The
    /// single home of this rule (was inlined only in `is_child_subtree_available`). A real tileset nests far
    /// shallower; an out-of-range coordinate is treated as unavailable everywhere.
    #[inline]
    fn coord_in_range(&self, x: u32, y: u32, z: u32) -> bool {
        !(self.child_count == 8 && (x | y | z) >= (1 << 21))
    }

    /// Is the tile at local `(level, x, y[, z])` available in this subtree?
    pub fn is_tile_available(&self, level: u32, x: u32, y: u32, z: u32) -> bool {
        self.coord_in_range(x, y, z)
            && self
                .tile_availability
                .get(bit_index(level, self.morton(x, y, z), self.child_count))
    }

    /// Is content `content_index` available at local `(level, x, y[, z])`?
    pub fn is_content_available(&self, content_index: usize, level: u32, x: u32, y: u32, z: u32) -> bool {
        self.coord_in_range(x, y, z)
            && self
                .content_availability
                .get(content_index)
                .map(|a| a.get(bit_index(level, self.morton(x, y, z), self.child_count)))
                .unwrap_or(false)
    }

    /// Does a child subtree exist rooted at local `(x, y[, z])` (indexed at the deepest+1 level, i.e. the
    /// subtree-root coordinates — no level offset)?
    pub fn is_child_subtree_available(&self, x: u32, y: u32, z: u32) -> bool {
        self.coord_in_range(x, y, z) && self.child_subtree_availability.get(self.morton(x, y, z))
    }
}

// ---- subtree JSON ---------------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SubtreeJson {
    #[serde(default)]
    buffers: Vec<BufferJson>,
    #[serde(default)]
    buffer_views: Vec<BufferViewJson>,
    tile_availability: AvailabilityJson,
    #[serde(default)]
    content_availability: Option<OneOrMany<AvailabilityJson>>,
    child_subtree_availability: AvailabilityJson,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BufferJson {
    #[serde(default)]
    uri: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BufferViewJson {
    buffer: usize,
    byte_offset: u64,
    byte_length: u64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AvailabilityJson {
    #[serde(default)]
    constant: Option<u8>,
    #[serde(default)]
    bitstream: Option<usize>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum OneOrMany<T> {
    One(T),
    Many(Vec<T>),
}

/// Parse a `.subtree` binary, resolving availabilities against the internal binary chunk. `child_count`
/// comes from the tileset's `implicitTiling.subdivisionScheme` (4 quadtree / 8 octree).
pub fn parse_subtree(bytes: &[u8], child_count: u32) -> Result<Subtree, ImplicitError> {
    if bytes.len() < 24 {
        return Err(ImplicitError::UnexpectedEof);
    }
    let magic = &bytes[0..4];
    if magic != b"subt" {
        return Err(ImplicitError::BadMagic);
    }
    let version = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
    if version != 1 {
        return Err(ImplicitError::BadVersion(version));
    }
    let json_len = read_u64(bytes, 8)? as usize;
    let bin_len = read_u64(bytes, 16)? as usize;

    let json_start = 24usize;
    let json_end = json_start.checked_add(json_len).ok_or(ImplicitError::UnexpectedEof)?;
    let json = bytes.get(json_start..json_end).ok_or(ImplicitError::UnexpectedEof)?;
    // The binary chunk follows the JSON, 8-byte aligned.
    let bin_start = align8(json_end);
    let bin_end = bin_start.checked_add(bin_len).ok_or(ImplicitError::UnexpectedEof)?;
    let binary = bytes.get(bin_start..bin_end).ok_or(ImplicitError::UnexpectedEof)?;

    let meta: SubtreeJson = serde_json::from_slice(json).map_err(|e| ImplicitError::Json(e.to_string()))?;

    // Resolve a single availability: a constant, or a bitstream sliced from the internal buffer.
    let resolve = |a: &AvailabilityJson| -> Result<Availability, ImplicitError> {
        if let Some(c) = a.constant {
            return Ok(Availability::Constant(c != 0));
        }
        let view_idx = a.bitstream.ok_or(ImplicitError::BadBufferView)?;
        let view = meta.buffer_views.get(view_idx).ok_or(ImplicitError::BadBufferView)?;
        // Only the internal binary buffer is supported (a buffer with no `uri`); an external `uri` errors.
        let buffer = meta.buffers.get(view.buffer).ok_or(ImplicitError::BadBufferView)?;
        if buffer.uri.is_some() {
            return Err(ImplicitError::ExternalBufferUnsupported);
        }
        let start = view.byte_offset as usize;
        let end = start
            .checked_add(view.byte_length as usize)
            .ok_or(ImplicitError::BadBufferView)?;
        let slice = binary.get(start..end).ok_or(ImplicitError::BadBufferView)?;
        Ok(Availability::Bitstream(slice.to_vec()))
    };

    let tile_availability = resolve(&meta.tile_availability)?;
    let child_subtree_availability = resolve(&meta.child_subtree_availability)?;
    let content_availability = match &meta.content_availability {
        None => Vec::new(),
        Some(OneOrMany::One(a)) => vec![resolve(a)?],
        Some(OneOrMany::Many(v)) => v.iter().map(resolve).collect::<Result<_, _>>()?,
    };

    Ok(Subtree {
        child_count,
        tile_availability,
        content_availability,
        child_subtree_availability,
    })
}

fn read_u64(bytes: &[u8], at: usize) -> Result<u64, ImplicitError> {
    let s = bytes.get(at..at + 8).ok_or(ImplicitError::UnexpectedEof)?;
    let mut a = [0u8; 8];
    a.copy_from_slice(s);
    Ok(u64::from_le_bytes(a))
}

#[inline]
fn align8(n: usize) -> usize {
    (n + 7) & !7
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn morton_2d_interleaves() {
        assert_eq!(morton_2d(0, 0), 0);
        assert_eq!(morton_2d(1, 0), 0b01);
        assert_eq!(morton_2d(0, 1), 0b10);
        assert_eq!(morton_2d(1, 1), 0b11);
        assert_eq!(morton_2d(3, 0), 0b0101); // x bits in even positions
        assert_eq!(morton_2d(0, 3), 0b1010);
        assert_eq!(morton_2d(2, 1), 0b0110); // x=2(0b10)->0b0100, y=1->0b0010 => 0b0110
    }

    #[test]
    fn morton_3d_interleaves() {
        assert_eq!(morton_3d(0, 0, 0), 0);
        assert_eq!(morton_3d(1, 0, 0), 0b001);
        assert_eq!(morton_3d(0, 1, 0), 0b010);
        assert_eq!(morton_3d(0, 0, 1), 0b100);
        assert_eq!(morton_3d(1, 1, 1), 0b111);
    }

    #[test]
    fn bit_index_level_offsets() {
        // quadtree: level 0 offset 0, level 1 offset 1, level 2 offset 5 ((4^2-1)/3=5), level 3 offset 21.
        assert_eq!(bit_index(0, 0, 4), 0);
        assert_eq!(bit_index(1, 0, 4), 1);
        assert_eq!(bit_index(1, 3, 4), 4);
        assert_eq!(bit_index(2, 0, 4), 5);
        assert_eq!(bit_index(3, 0, 4), 21);
        // octree: level 1 offset 1, level 2 offset 9 ((8^2-1)/7=9).
        assert_eq!(bit_index(1, 0, 8), 1);
        assert_eq!(bit_index(2, 0, 8), 9);
    }

    #[test]
    fn availability_bit_query() {
        // bit 5 set in a bitstream (byte 0, bit 5).
        let a = Availability::Bitstream(vec![0b0010_0000]);
        assert!(a.get(5));
        assert!(!a.get(4));
        assert!(!a.get(8)); // past the end => unavailable
        assert!(Availability::Constant(true).get(99999));
        assert!(!Availability::Constant(false).get(0));
    }

    /// Build a minimal `.subtree`: a quadtree where the root + all 4 level-1 children are available, with
    /// a single internal buffer holding the tile-availability bitstream.
    fn build_subtree() -> Vec<u8> {
        // levels 0..1 => 1 + 4 = 5 bits; set bits 0..=4 (root + 4 children) => 0b0001_1111 = 0x1F.
        let bitstream = vec![0x1Fu8];
        let json = format!(
            r#"{{"buffers":[{{"byteLength":{}}}],"bufferViews":[{{"buffer":0,"byteOffset":0,"byteLength":{}}}],"tileAvailability":{{"bitstream":0,"availableCount":5}},"contentAvailability":{{"constant":1}},"childSubtreeAvailability":{{"constant":0}}}}"#,
            bitstream.len(),
            bitstream.len()
        );
        let json_bytes = json.into_bytes();
        let json_padded = (json_bytes.len() + 7) & !7;
        let bin_padded = (bitstream.len() + 7) & !7;

        let mut out = Vec::new();
        out.extend_from_slice(b"subt");
        out.extend_from_slice(&1u32.to_le_bytes());
        out.extend_from_slice(&(json_padded as u64).to_le_bytes());
        out.extend_from_slice(&(bin_padded as u64).to_le_bytes());
        out.extend_from_slice(&json_bytes);
        out.resize(24 + json_padded, b' '); // space-pad the JSON chunk
        out.extend_from_slice(&bitstream);
        out.resize(24 + json_padded + bin_padded, 0); // zero-pad the binary chunk
        out
    }

    #[test]
    fn parses_subtree_and_resolves_availability() {
        let st = parse_subtree(&build_subtree(), 4).expect("parse");
        assert_eq!(st.child_count, 4);
        // root (level 0) available.
        assert!(st.is_tile_available(0, 0, 0, 0));
        // all 4 level-1 children available.
        for (x, y) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
            assert!(st.is_tile_available(1, x, y, 0), "child {x},{y}");
        }
        // a level-2 tile is past the 5 set bits => unavailable.
        assert!(!st.is_tile_available(2, 0, 0, 0));
        // content is constant-available; no child subtrees.
        assert!(st.is_content_available(0, 0, 0, 0, 0));
        assert!(!st.is_child_subtree_available(0, 0, 0));
    }

    #[test]
    fn rejects_bad_header() {
        assert_eq!(parse_subtree(b"nope", 4), Err(ImplicitError::UnexpectedEof));
        let mut bad = build_subtree();
        bad[0] = b'x';
        assert_eq!(parse_subtree(&bad, 4), Err(ImplicitError::BadMagic));
    }
}
