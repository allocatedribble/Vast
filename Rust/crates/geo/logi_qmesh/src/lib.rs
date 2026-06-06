//! Decoder for Cesium **quantized-mesh-1.0** terrain tiles (the format Cesium World Terrain streams),
//! for the actorless 3D-Tiles / real-world-terrain stack (ADR-0009, Milestone F).
//!
//! [`parse`] is a pure byte codec (no geometry math): header → delta/zig-zag vertices → high-watermark
//! indices → edge rings → extensions. [`QuantizedMeshTile::to_ecef_mesh`] then geocodes the quantized
//! u/v/height into geocentric **ECEF f64** positions (+ normals) via [`logi_geospatial`]; the kernel
//! rebases ECEF → local f32 against the camera-tracking floating origin downstream (plan §9.4).
//!
//! Spec: `CesiumGS/quantized-mesh` (`README.md` binary layout, `SPECIFICATION.md` `layer.json`). All
//! multi-byte values are little-endian.

#![forbid(unsafe_code)]

use glam::DVec3;
use logi_geospatial::{Cartographic, Ellipsoid};

/// What went wrong decoding a tile.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QmError {
    /// A read ran past the end of the buffer (truncated or malformed tile).
    UnexpectedEof,
    /// A decoded triangle index referenced a vertex `>= vertex_count` (corrupt high-watermark stream).
    BadIndex,
}

// ---- bit/byte building blocks (pub for reuse + direct testing) -------------------------------------

/// Decode a zig-zag-encoded `u16` back to a signed delta: `(v >> 1) ^ -(v & 1)`.
#[inline]
pub fn zigzag_decode(value: u16) -> i32 {
    ((value >> 1) as i32) ^ (-((value & 1) as i32))
}

/// Zig-zag encode a signed delta to `u16` (inverse of [`zigzag_decode`]; the encoder side, for tooling
/// and tests).
#[inline]
pub fn zigzag_encode(n: i32) -> u16 {
    ((n << 1) ^ (n >> 31)) as u16
}

/// Decode an oct-encoded unit normal (extension 1): two `u8`s → a unit `DVec3`. Per cesium-native
/// `AttributeCompression::octDecodeInRange` (rangeMax 255), Cigolle et al. 2014.
pub fn oct_decode(x: u8, y: u8) -> DVec3 {
    let from_snorm = |b: u8| (b as f64 / 255.0) * 2.0 - 1.0;
    let mut nx = from_snorm(x);
    let mut ny = from_snorm(y);
    let nz = 1.0 - (nx.abs() + ny.abs());
    if nz < 0.0 {
        let old_nx = nx;
        nx = (1.0 - ny.abs()) * sign_not_zero(old_nx);
        ny = (1.0 - old_nx.abs()) * sign_not_zero(ny);
    }
    // try_normalize (not normalize): a degenerate zero vector → +Z instead of a NaN normal. The fold
    // above makes a true zero unreachable for valid u8 inputs, but this hardens against any future
    // snorm-range change silently feeding NaN into ECEF lighting.
    DVec3::new(nx, ny, nz).try_normalize().unwrap_or(DVec3::Z)
}

#[inline]
fn sign_not_zero(f: f64) -> f64 {
    if f < 0.0 {
        -1.0
    } else {
        1.0
    }
}

// ---- parsed tile ----------------------------------------------------------------------------------

/// The 88-byte tile header (geocentric ECEF, metres).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct QuantizedMeshHeader {
    /// Tile center, ECEF (a relocation/culling reference — NOT added to vertex positions).
    pub center: DVec3,
    pub min_height: f32,
    pub max_height: f32,
    pub bounding_sphere_center: DVec3,
    pub bounding_sphere_radius: f64,
    pub horizon_occlusion_point: DVec3,
}

/// Triangle indices, `u16` for ≤ 65536 vertices else `u32` (CCW winding).
#[derive(Clone, Debug, PartialEq)]
pub enum IndexBuffer {
    U16(Vec<u16>),
    U32(Vec<u32>),
}

impl IndexBuffer {
    #[inline]
    pub fn len(&self) -> usize {
        match self {
            Self::U16(v) => v.len(),
            Self::U32(v) => v.len(),
        }
    }
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    /// Widen to `u32` for uniform downstream use.
    pub fn to_u32(&self) -> Vec<u32> {
        self.iter_u32().collect()
    }

    /// Borrowing iterator over the indices widened to `u32` — no allocation, no clone (audit M10). Lets
    /// callers walk the index buffer (normals, triangle filtering) without materialising a `Vec<u32>` copy.
    pub fn iter_u32(&self) -> impl Iterator<Item = u32> + '_ {
        // Two concrete iterator types unified behind one `enum` so the return type stays a single `impl`.
        enum Iter<'a> {
            U16(std::slice::Iter<'a, u16>),
            U32(std::slice::Iter<'a, u32>),
        }
        impl Iterator for Iter<'_> {
            type Item = u32;
            #[inline]
            fn next(&mut self) -> Option<u32> {
                match self {
                    Iter::U16(it) => it.next().map(|&x| x as u32),
                    Iter::U32(it) => it.next().copied(),
                }
            }
        }
        match self {
            Self::U16(v) => Iter::U16(v.iter()),
            Self::U32(v) => Iter::U32(v.iter()),
        }
    }
}

/// The perimeter vertex indices on each tile edge — used to build skirts that hide LOD-boundary cracks.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct EdgeRings {
    pub west: Vec<u32>,
    pub south: Vec<u32>,
    pub east: Vec<u32>,
    pub north: Vec<u32>,
}

/// Water mask (extension 2): one byte (uniform) or a 256×256 grid (`0`=land, `255`=water).
#[derive(Clone, Debug, PartialEq)]
pub enum WaterMask {
    Uniform(u8),
    Grid(Vec<u8>),
}

/// Optional tile extensions.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Extensions {
    /// Oct-encoded per-vertex normals (extension 1): `2 * vertex_count` raw bytes (decode with
    /// [`oct_decode`]).
    pub oct_normals: Option<Vec<u8>>,
    /// Water mask (extension 2).
    pub water_mask: Option<WaterMask>,
    /// Metadata JSON (extension 4), kept as a raw string for downstream parsing.
    pub metadata: Option<String>,
    /// Any unrecognised extensions, preserved as `(id, payload)`.
    pub unknown: Vec<(u8, Vec<u8>)>,
}

/// A parsed quantized-mesh tile: still-quantized `u`/`v`/`height` (deltas resolved, each `0..=32767`)
/// plus indices, edges, and extensions. Geocode with [`Self::to_ecef_mesh`].
#[derive(Clone, Debug, PartialEq)]
pub struct QuantizedMeshTile {
    pub header: QuantizedMeshHeader,
    pub u: Vec<u16>,
    pub v: Vec<u16>,
    pub height: Vec<u16>,
    pub indices: IndexBuffer,
    pub edges: EdgeRings,
    pub extensions: Extensions,
}

// ---- the byte reader ------------------------------------------------------------------------------

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }
    #[inline]
    fn remaining(&self) -> usize {
        self.buf.len() - self.pos.min(self.buf.len())
    }
    fn read_array<const N: usize>(&mut self) -> Result<[u8; N], QmError> {
        let end = self.pos.checked_add(N).ok_or(QmError::UnexpectedEof)?;
        let slice = self.buf.get(self.pos..end).ok_or(QmError::UnexpectedEof)?;
        let mut arr = [0u8; N];
        arr.copy_from_slice(slice);
        self.pos = end;
        Ok(arr)
    }
    fn read_u8(&mut self) -> Result<u8, QmError> {
        Ok(self.read_array::<1>()?[0])
    }
    fn read_u16(&mut self) -> Result<u16, QmError> {
        Ok(u16::from_le_bytes(self.read_array()?))
    }
    fn read_u32(&mut self) -> Result<u32, QmError> {
        Ok(u32::from_le_bytes(self.read_array()?))
    }
    fn read_f32(&mut self) -> Result<f32, QmError> {
        Ok(f32::from_le_bytes(self.read_array()?))
    }
    fn read_f64(&mut self) -> Result<f64, QmError> {
        Ok(f64::from_le_bytes(self.read_array()?))
    }
    fn read_dvec3(&mut self) -> Result<DVec3, QmError> {
        Ok(DVec3::new(self.read_f64()?, self.read_f64()?, self.read_f64()?))
    }
    fn read_bytes(&mut self, n: usize) -> Result<Vec<u8>, QmError> {
        if self.remaining() < n {
            return Err(QmError::UnexpectedEof);
        }
        let end = self.pos + n;
        let out = self.buf[self.pos..end].to_vec();
        self.pos = end;
        Ok(out)
    }
    fn read_u16_vec(&mut self, n: usize) -> Result<Vec<u16>, QmError> {
        if self.remaining() < n.saturating_mul(2) {
            return Err(QmError::UnexpectedEof);
        }
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            out.push(self.read_u16()?);
        }
        Ok(out)
    }
    fn read_u32_vec(&mut self, n: usize) -> Result<Vec<u32>, QmError> {
        if self.remaining() < n.saturating_mul(4) {
            return Err(QmError::UnexpectedEof);
        }
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            out.push(self.read_u32()?);
        }
        Ok(out)
    }
    /// Advance to the next `n`-byte boundary (the IndexData alignment pad).
    fn align_to(&mut self, n: usize) {
        let rem = self.pos % n;
        if rem != 0 {
            self.pos += n - rem;
        }
    }
}

// ---- delta/zig-zag + high-watermark decode --------------------------------------------------------

/// Resolve the zig-zag + prefix-sum encoding of a `u`/`v`/`height` array (result each `0..=32767`).
fn decode_delta_zigzag(raw: &[u16]) -> Vec<u16> {
    let mut acc: i32 = 0;
    raw.iter()
        .map(|&z| {
            // `wrapping_add`: the running sum can overflow i32 on hostile/corrupt input (many max
            // deltas); the result is truncated `as u16` anyway, so wrapping is the intended semantics
            // and avoids a debug-build overflow panic on untrusted bytes.
            acc = acc.wrapping_add(zigzag_decode(z));
            acc as u16
        })
        .collect()
}

/// High-watermark / forward-difference index decode: `index = highest - code; if code==0 { highest++ }`.
/// The running `index`/`highest` are kept as **u32** throughout (never truncated to `u16`): a corrupt code
/// can drive `index` past `vertex_count`, and folding that down to `u16` would silently alias e.g. 65536 →
/// vertex 0 — a wrong-but-plausible mesh (audit M30). Any index `>= vertex_count` is rejected as malformed
/// input rather than truncated. The `u16` transport width caps a valid index at 65535 < vertex_count
/// (≤ 65536), so widening the *result* to u32 loses nothing and the bounds check is exact for both arms.
fn decode_indices_hw(raw: impl Iterator<Item = u32>, vertex_count: usize) -> Result<Vec<u32>, QmError> {
    let mut highest: u32 = 0;
    let vc = vertex_count as u32; // vertex_count is bounded by the buffer length, well within u32
    let mut out = Vec::new();
    for code in raw {
        let index = highest.wrapping_sub(code);
        if code == 0 {
            highest += 1;
        }
        // `>=` (not `>`) — a valid index is `0..vertex_count`. `wrapping_sub` underflow surfaces here as a
        // huge value that fails the bound, so a malformed forward-difference stream errors cleanly.
        if index >= vc {
            return Err(QmError::BadIndex);
        }
        out.push(index);
    }
    Ok(out)
}

fn read_edge_ring(r: &mut Reader, use_u32: bool) -> Result<Vec<u32>, QmError> {
    let n = r.read_u32()? as usize;
    if use_u32 {
        r.read_u32_vec(n)
    } else {
        Ok(r.read_u16_vec(n)?.into_iter().map(u32::from).collect())
    }
}

fn read_extensions(r: &mut Reader) -> Result<Extensions, QmError> {
    let mut ext = Extensions::default();
    // Each extension is a 1-byte id + u32 length + payload; loop until fewer than a header remains.
    while r.remaining() >= 5 {
        let id = r.read_u8()?;
        let len = r.read_u32()? as usize;
        // Validate the declared extension length against what's left BEFORE dispatching (robustness): a
        // lying `len` (larger than the remaining buffer) errors cleanly here, and reading the whole payload
        // as one bounded slice keeps a malformed inner structure (e.g. extension 4's jsonLength) from
        // running past this extension's boundary into the next one.
        let payload = r.read_bytes(len)?;
        match id {
            1 => ext.oct_normals = Some(payload),
            2 => {
                // Spec: a uniform water mask is exactly 1 byte; a non-uniform mask is exactly
                // 256*256 = 65536 bytes. Any other length is malformed — leave the mask absent rather than
                // emit a wrongly-sized `Grid` a consumer would misindex (land rendered as water, or panic).
                ext.water_mask = match len {
                    1 => payload.first().copied().map(WaterMask::Uniform),
                    65_536 => Some(WaterMask::Grid(payload)),
                    _ => None,
                };
            }
            4 => {
                // payload = u32 jsonLength + json[jsonLength]; extensionLength == 4 + jsonLength. Read the
                // json bounded to THIS extension's payload — a `jsonLength` exceeding it is clamped, never
                // bleeding into the following extension.
                let json_len = payload
                    .get(0..4)
                    .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize)
                    .unwrap_or(0);
                let json = payload.get(4..4 + json_len.min(payload.len().saturating_sub(4)));
                ext.metadata = Some(String::from_utf8_lossy(json.unwrap_or(&[])).into_owned());
            }
            other => ext.unknown.push((other, payload)),
        }
    }
    Ok(ext)
}

/// Parse the binary tile (decompress gzip in the transport layer first). Pure byte codec — no geometry.
pub fn parse(bytes: &[u8]) -> Result<QuantizedMeshTile, QmError> {
    let mut r = Reader::new(bytes);

    let header = QuantizedMeshHeader {
        center: r.read_dvec3()?,
        min_height: r.read_f32()?,
        max_height: r.read_f32()?,
        bounding_sphere_center: r.read_dvec3()?,
        bounding_sphere_radius: r.read_f64()?,
        horizon_occlusion_point: r.read_dvec3()?,
    };

    let vertex_count = r.read_u32()? as usize;
    let u = decode_delta_zigzag(&r.read_u16_vec(vertex_count)?);
    let v = decode_delta_zigzag(&r.read_u16_vec(vertex_count)?);
    let height = decode_delta_zigzag(&r.read_u16_vec(vertex_count)?);

    // IndexData — index width depends on vertex count; pad to its alignment first.
    let use_u32 = vertex_count > 65536;
    r.align_to(if use_u32 { 4 } else { 2 });
    // checked_mul: the only unchecked size multiply in the parser. On the 64-bit target `u32::MAX * 3`
    // fits `usize`; on a 32-bit target it would silently wrap in the default release profile (overflow
    // checks off) — and would panic→abort only if `overflow-checks` were enabled. Using `checked_mul`
    // regardless makes the behaviour identical on every target/profile: an absurd triangle count is treated
    // as truncated input rather than corrupting the index count.
    let index_count = (r.read_u32()? as usize).checked_mul(3).ok_or(QmError::UnexpectedEof)?;
    // Decode with a u32 accumulator and reject any index past `vertex_count` (audit M30). For the u16 arm
    // every accepted index is `< vertex_count ≤ 65536`, i.e. `≤ 65535`, so the narrowing back to `u16` for
    // the compact transport representation is now lossless (no silent 65536 → 0 aliasing).
    let indices = if use_u32 {
        let raw = r.read_u32_vec(index_count)?;
        IndexBuffer::U32(decode_indices_hw(raw.into_iter(), vertex_count)?)
    } else {
        let raw = r.read_u16_vec(index_count)?;
        let decoded = decode_indices_hw(raw.into_iter().map(u32::from), vertex_count)?;
        IndexBuffer::U16(decoded.into_iter().map(|i| i as u16).collect())
    };

    let edges = EdgeRings {
        west: read_edge_ring(&mut r, use_u32)?,
        south: read_edge_ring(&mut r, use_u32)?,
        east: read_edge_ring(&mut r, use_u32)?,
        north: read_edge_ring(&mut r, use_u32)?,
    };

    let extensions = read_extensions(&mut r)?;

    Ok(QuantizedMeshTile {
        header,
        u,
        v,
        height,
        indices,
        edges,
        extensions,
    })
}

// ---- tiling -> geodetic bounds --------------------------------------------------------------------

/// A tile's geographic extent, in **radians**.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GeodeticBounds {
    pub west: f64,
    pub south: f64,
    pub east: f64,
    pub north: f64,
}

/// Geographic bounds of a `{z,x,y}` tile in the default **EPSG:4326 / TMS** scheme (2 root tiles at z=0;
/// `2·2^z` columns × `2^z` rows; `y` increases northward). (Web-Mercator / slippyMap are future work.)
pub fn tile_bounds_geodetic(z: u32, x: u32, y: u32) -> GeodeticBounds {
    use std::f64::consts::{FRAC_PI_2, PI};
    // Clamp `z` to a sane max before the power: real quantized-mesh zooms top out around 22, and an
    // unclamped huge `z` drives `2^z → ∞` so `d_lon`/`d_lat → 0` — degenerate zero-area bounds that would
    // collapse every vertex of the tile to a point (audit qmesh-L3). `MAX_TILE_ZOOM` keeps `d_lon`/`d_lat`
    // representable and non-zero for any input. (`2^z` as a float also avoids the `1u64 << z` overflow the
    // old `z >= 64` path had.)
    const MAX_TILE_ZOOM: u32 = 32;
    let pow = 2f64.powi(z.min(MAX_TILE_ZOOM) as i32);
    let d_lon = PI / pow; // 360° / (2·2^z)
    let d_lat = PI / pow; // 180° / 2^z
    let west = -PI + (x as f64) * d_lon;
    let south = -FRAC_PI_2 + (y as f64) * d_lat;
    GeodeticBounds {
        west,
        south,
        east: west + d_lon,
        north: south + d_lat,
    }
}

// ---- geodetic -> ECEF mesh ------------------------------------------------------------------------

/// A decoded tile mesh in geocentric **ECEF f64**, ready for the kernel's floating-origin rebase.
#[derive(Clone, Debug, PartialEq)]
pub struct EcefMesh {
    pub positions: Vec<DVec3>,
    pub normals: Vec<DVec3>,
    /// CCW triangle indices; any triangle referencing an out-of-range vertex is dropped.
    pub indices: Vec<u32>,
    pub min_height: f32,
    pub max_height: f32,
    pub center: DVec3,
    pub bounding_sphere_radius: f64,
}

impl QuantizedMeshTile {
    /// Number of vertices in the tile.
    #[inline]
    pub fn vertex_count(&self) -> usize {
        self.u.len()
    }

    /// Geocode the quantized `u`/`v`/`height` into ECEF positions + unit normals (oct-decoded when the
    /// normals extension is present, else computed from faces) for the given geographic `bounds`.
    pub fn to_ecef_mesh(&self, bounds: GeodeticBounds, ellipsoid: &Ellipsoid) -> EcefMesh {
        let vc = self.vertex_count();
        let (min_h, max_h) = (self.header.min_height as f64, self.header.max_height as f64);
        let (d_lon, d_lat) = (bounds.east - bounds.west, bounds.north - bounds.south);

        let positions: Vec<DVec3> = (0..vc)
            .map(|i| {
                let lon = bounds.west + (self.u[i] as f64 / 32767.0) * d_lon;
                let lat = bounds.south + (self.v[i] as f64 / 32767.0) * d_lat;
                let height = min_h + (self.height[i] as f64 / 32767.0) * (max_h - min_h);
                ellipsoid.cartographic_to_cartesian(Cartographic::new(lon, lat, height))
            })
            .collect();

        // Build the valid-triangle index list straight from a borrowing iterator (no `to_u32()` clone —
        // audit M10), dropping any triangle that references a vertex past the end (defence-in-depth: the
        // parser already rejects OOB indices, but `to_ecef_mesh` may run on tiles from other sources).
        let mut idx_iter = self.indices.iter_u32();
        let mut indices: Vec<u32> = Vec::with_capacity(self.indices.len());
        while let (Some(a), Some(b), Some(c)) = (idx_iter.next(), idx_iter.next(), idx_iter.next()) {
            if (a as usize) < vc && (b as usize) < vc && (c as usize) < vc {
                indices.extend_from_slice(&[a, b, c]);
            }
        }

        // Normals from the oct-encoded extension when present AND correctly sized (2 bytes/vertex), else
        // area-weighted face normals over the already-filtered (in-range) triangles — so the normal pass
        // never re-checks bounds. A short oct buffer (audit qmesh-L2) is treated as absent rather than
        // silently padding the tail with "up", which would have produced flat-shaded garbage on those
        // vertices with no signal.
        let normals = match &self.extensions.oct_normals {
            Some(oct) if oct.len() >= vc * 2 => (0..vc).map(|i| oct_decode(oct[i * 2], oct[i * 2 + 1])).collect(),
            _ => compute_vertex_normals(&positions, &indices),
        };

        EcefMesh {
            positions,
            normals,
            indices,
            min_height: self.header.min_height,
            max_height: self.header.max_height,
            center: self.header.center,
            bounding_sphere_radius: self.header.bounding_sphere_radius,
        }
    }
}

/// Area-weighted face normals averaged onto vertices (the fallback when no oct-normal extension).
fn compute_vertex_normals(positions: &[DVec3], raw_indices: &[u32]) -> Vec<DVec3> {
    let mut normals = vec![DVec3::ZERO; positions.len()];
    for tri in raw_indices.chunks_exact(3) {
        let (a, b, c) = (tri[0] as usize, tri[1] as usize, tri[2] as usize);
        if a >= positions.len() || b >= positions.len() || c >= positions.len() {
            continue;
        }
        // (b-a) × (c-a) is the CCW face normal, magnitude = 2·area (the area weight).
        let face = (positions[b] - positions[a]).cross(positions[c] - positions[a]);
        normals[a] += face;
        normals[b] += face;
        normals[c] += face;
    }
    for n in &mut normals {
        *n = if n.length_squared() > 0.0 {
            n.normalize()
        } else {
            DVec3::Z
        };
    }
    normals
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::{FRAC_PI_2, PI};

    #[test]
    fn zigzag_known_values_and_roundtrip() {
        assert_eq!(zigzag_decode(0), 0);
        assert_eq!(zigzag_decode(1), -1);
        assert_eq!(zigzag_decode(2), 1);
        assert_eq!(zigzag_decode(3), -2);
        for n in [-32767, -2, -1, 0, 1, 2, 32767] {
            assert_eq!(zigzag_decode(zigzag_encode(n)), n, "roundtrip {n}");
        }
    }

    #[test]
    fn high_watermark_decodes_known_sequence() {
        // [0,0,0,3,1,0] -> the two triangles of a quad: 0,1,2, 0,2,3 (4 distinct vertices).
        let raw = [0u16, 0, 0, 3, 1, 0];
        let decoded = decode_indices_hw(raw.iter().map(|&c| c as u32), 4).expect("valid");
        assert_eq!(decoded, vec![0u32, 1, 2, 0, 2, 3]);
    }

    #[test]
    fn high_watermark_rejects_out_of_range_index() {
        // A code stream that resolves to a vertex index past `vertex_count` is malformed, not aliased to 0.
        // First code 1 with highest=0 → wrapping_sub → u32::MAX, far past any vertex_count → BadIndex.
        assert_eq!(decode_indices_hw([1u32].into_iter(), 4), Err(QmError::BadIndex));
        // A valid forward-difference run that walks one vertex past the declared count also errors.
        // [0,0,0,0,0] declares vertices 0..4 then references nothing OOB; add a back-reference to 4:
        let raw = [0u16, 0, 0, 0, 4]; // last code 4: index = highest(4) - 4 = 0 (in range) — sanity stays valid
        assert!(decode_indices_hw(raw.iter().map(|&c| c as u32), 4).is_ok());
    }

    #[test]
    fn delta_zigzag_resolves_running_sum() {
        // values 0, 32767, 0  ->  deltas 0, +32767, -32767  ->  zigzag 0, 65534, 65533
        assert_eq!(decode_delta_zigzag(&[0, 65534, 65533]), vec![0u16, 32767, 0]);
    }

    #[test]
    fn oct_decode_is_unit_and_up_at_center() {
        let up = oct_decode(128, 128);
        assert!((up.length() - 1.0).abs() <= 1e-9);
        assert!(up.z > 0.99, "{up:?}"); // (128,128) ≈ +Z
        for (x, y) in [(0u8, 0u8), (255, 255), (0, 255), (200, 40), (64, 192)] {
            assert!((oct_decode(x, y).length() - 1.0).abs() <= 1e-9, "({x},{y}) not unit");
        }
    }

    #[test]
    fn tile_bounds_match_spec_examples() {
        // README worked examples (EPSG:4326 / TMS).
        let approx = |a: f64, b: f64| (a - b).abs() <= 1e-12;
        let b = tile_bounds_geodetic(1, 0, 0); // (-180,-90)-(-90,0)
        assert!(approx(b.west, -PI) && approx(b.east, -FRAC_PI_2));
        assert!(approx(b.south, -FRAC_PI_2) && approx(b.north, 0.0));
        let b = tile_bounds_geodetic(1, 3, 1); // (90,0)-(180,90)
        assert!(approx(b.west, FRAC_PI_2) && approx(b.east, PI));
        assert!(approx(b.south, 0.0) && approx(b.north, FRAC_PI_2));
        // the two z=0 roots split the world in half along longitude.
        assert!(approx(tile_bounds_geodetic(0, 0, 0).west, -PI));
        assert!(approx(tile_bounds_geodetic(0, 1, 0).east, PI));
    }

    /// Assemble a minimal valid quantized-mesh-1.0 byte stream from structured input (the encoder side).
    fn encode_tile(
        header: &QuantizedMeshHeader,
        u: &[u16],
        v: &[u16],
        height: &[u16],
        triangles: &[u16],
        edges: &EdgeRings,
        oct_normals: Option<&[u8]>,
    ) -> Vec<u8> {
        fn enc_delta_zigzag(values: &[u16]) -> Vec<u16> {
            let mut prev = 0i32;
            values
                .iter()
                .map(|&val| {
                    let d = val as i32 - prev;
                    prev = val as i32;
                    zigzag_encode(d)
                })
                .collect()
        }
        fn enc_high_watermark(indices: &[u16]) -> Vec<u16> {
            let mut highest = 0u32;
            indices
                .iter()
                .map(|&i| {
                    let code = highest.wrapping_sub(i as u32);
                    if i as u32 == highest {
                        highest += 1;
                    }
                    code as u16
                })
                .collect()
        }
        let mut out = Vec::new();
        for c in [header.center.x, header.center.y, header.center.z] {
            out.extend_from_slice(&c.to_le_bytes());
        }
        out.extend_from_slice(&header.min_height.to_le_bytes());
        out.extend_from_slice(&header.max_height.to_le_bytes());
        for c in [
            header.bounding_sphere_center.x,
            header.bounding_sphere_center.y,
            header.bounding_sphere_center.z,
        ] {
            out.extend_from_slice(&c.to_le_bytes());
        }
        out.extend_from_slice(&header.bounding_sphere_radius.to_le_bytes());
        for c in [
            header.horizon_occlusion_point.x,
            header.horizon_occlusion_point.y,
            header.horizon_occlusion_point.z,
        ] {
            out.extend_from_slice(&c.to_le_bytes());
        }
        out.extend_from_slice(&(u.len() as u32).to_le_bytes());
        for arr in [u, v, height] {
            for z in enc_delta_zigzag(arr) {
                out.extend_from_slice(&z.to_le_bytes());
            }
        }
        // u16 indices (small tile): align to 2 (already even after the 88-byte header + 4 + 6N).
        while out.len() % 2 != 0 {
            out.push(0);
        }
        out.extend_from_slice(&((triangles.len() / 3) as u32).to_le_bytes());
        for code in enc_high_watermark(triangles) {
            out.extend_from_slice(&code.to_le_bytes());
        }
        for ring in [&edges.west, &edges.south, &edges.east, &edges.north] {
            out.extend_from_slice(&(ring.len() as u32).to_le_bytes());
            for &i in ring {
                out.extend_from_slice(&(i as u16).to_le_bytes());
            }
        }
        if let Some(oct) = oct_normals {
            out.push(1u8); // extension id
            out.extend_from_slice(&(oct.len() as u32).to_le_bytes());
            out.extend_from_slice(oct);
        }
        out
    }

    fn sample_header() -> QuantizedMeshHeader {
        QuantizedMeshHeader {
            center: DVec3::new(1.0, 2.0, 3.0),
            min_height: -10.0,
            max_height: 250.0,
            bounding_sphere_center: DVec3::new(4.0, 5.0, 6.0),
            bounding_sphere_radius: 7.5,
            horizon_occlusion_point: DVec3::new(8.0, 9.0, 10.0),
        }
    }

    #[test]
    fn parse_roundtrips_a_built_tile() {
        let header = sample_header();
        let u = [0u16, 32767, 0, 32767];
        let v = [0u16, 0, 32767, 32767];
        let height = [0u16, 16000, 8000, 32767];
        let triangles = [0u16, 1, 2, 1, 3, 2]; // two CCW tris of the quad
        let edges = EdgeRings {
            west: vec![0, 2],
            south: vec![0, 1],
            east: vec![1, 3],
            north: vec![2, 3],
        };
        let oct = [128u8, 128, 200, 40, 64, 192, 10, 250]; // 2 bytes per vertex
        let bytes = encode_tile(&header, &u, &v, &height, &triangles, &edges, Some(&oct));

        let tile = parse(&bytes).expect("parse");
        assert_eq!(tile.header, header);
        assert_eq!(tile.u, u);
        assert_eq!(tile.v, v);
        assert_eq!(tile.height, height);
        assert_eq!(tile.indices, IndexBuffer::U16(vec![0, 1, 2, 1, 3, 2]));
        assert_eq!(tile.edges, edges);
        assert_eq!(tile.extensions.oct_normals.as_deref(), Some(&oct[..]));
    }

    #[test]
    fn to_ecef_mesh_places_corners_on_the_globe() {
        let header = sample_header();
        // a unit quad covering the tile's full u/v range, flat at min height.
        let tile = parse(&encode_tile(
            &header,
            &[0, 32767, 0, 32767],
            &[0, 0, 32767, 32767],
            &[0, 0, 0, 0], // all at min height (-10 m)
            &[0, 1, 2, 1, 3, 2],
            &EdgeRings::default(),
            None,
        ))
        .unwrap();

        let e = Ellipsoid::wgs84();
        let bounds = tile_bounds_geodetic(8, 73, 95); // an arbitrary mid-latitude tile
        let mesh = tile.to_ecef_mesh(bounds, &e);

        assert_eq!(mesh.positions.len(), 4);
        assert_eq!(mesh.indices, vec![0, 1, 2, 1, 3, 2]);
        // vertex 0 is u=v=0 (the SW corner) at the min height.
        let expected_sw =
            e.cartographic_to_cartesian(Cartographic::new(bounds.west, bounds.south, header.min_height as f64));
        assert!(
            (mesh.positions[0] - expected_sw).length() <= 1e-6,
            "{:?}",
            mesh.positions[0]
        );
        // vertex 3 is u=v=32767 (the NE corner).
        let expected_ne =
            e.cartographic_to_cartesian(Cartographic::new(bounds.east, bounds.north, header.min_height as f64));
        assert!((mesh.positions[3] - expected_ne).length() <= 1e-6);
        // computed normals are unit and point away from the geocenter (outward up).
        for (p, n) in mesh.positions.iter().zip(&mesh.normals) {
            assert!((n.length() - 1.0).abs() <= 1e-9);
            assert!(n.dot(*p) > 0.0, "normal should point outward");
        }
    }

    #[test]
    fn truncated_buffer_errors_cleanly() {
        let bytes = encode_tile(
            &sample_header(),
            &[0, 1],
            &[0, 1],
            &[0, 1],
            &[0, 1, 0],
            &EdgeRings::default(),
            None,
        );
        assert_eq!(parse(&bytes[..40]), Err(QmError::UnexpectedEof)); // cut mid-header
    }
}
