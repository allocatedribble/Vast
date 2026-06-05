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
    DVec3::new(nx, ny, nz).normalize()
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
        match self {
            Self::U16(v) => v.iter().map(|&x| x as u32).collect(),
            Self::U32(v) => v.clone(),
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
/// `highest` is tracked as `u32` so the `u16` case can reach 65536 distinct vertices without overflow.
fn decode_indices_u16(raw: &[u16]) -> Vec<u16> {
    let mut highest: u32 = 0;
    raw.iter()
        .map(|&code| {
            let index = highest.wrapping_sub(code as u32);
            if code == 0 {
                highest += 1;
            }
            index as u16
        })
        .collect()
}

fn decode_indices_u32(raw: &[u32]) -> Vec<u32> {
    let mut highest: u32 = 0;
    raw.iter()
        .map(|&code| {
            let index = highest.wrapping_sub(code);
            if code == 0 {
                highest += 1;
            }
            index
        })
        .collect()
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
        match id {
            1 => ext.oct_normals = Some(r.read_bytes(len)?),
            2 => {
                ext.water_mask = Some(if len == 1 {
                    WaterMask::Uniform(r.read_u8()?)
                } else {
                    WaterMask::Grid(r.read_bytes(len)?)
                });
            }
            4 => {
                // payload = u32 jsonLength + json[jsonLength]; extensionLength == 4 + jsonLength.
                let json_len = r.read_u32()? as usize;
                let json = r.read_bytes(json_len)?;
                ext.metadata = Some(String::from_utf8_lossy(&json).into_owned());
            }
            other => ext.unknown.push((other, r.read_bytes(len)?)),
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
    // fits `usize`, but a 32-bit build would wrap (or panic→abort under overflow-checks); treat an absurd
    // triangle count as truncated input rather than corrupting the index count.
    let index_count = (r.read_u32()? as usize).checked_mul(3).ok_or(QmError::UnexpectedEof)?;
    let indices = if use_u32 {
        IndexBuffer::U32(decode_indices_u32(&r.read_u32_vec(index_count)?))
    } else {
        IndexBuffer::U16(decode_indices_u16(&r.read_u16_vec(index_count)?))
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
    // `2^z` as a float: `1u64 << z` panics (debug) / wraps (release) for the hostile `z >= 64` this
    // `pub` fn would otherwise accept; the float power saturates harmlessly instead. Real zooms are ≤ ~22.
    let pow = 2f64.powi(z as i32);
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

        let raw_indices = self.indices.to_u32();

        let normals = match &self.extensions.oct_normals {
            Some(oct) => (0..vc)
                .map(|i| {
                    let x = oct.get(i * 2).copied().unwrap_or(128);
                    let y = oct.get(i * 2 + 1).copied().unwrap_or(128);
                    oct_decode(x, y)
                })
                .collect(),
            None => compute_vertex_normals(&positions, &raw_indices),
        };

        // Drop any triangle that references a vertex past the end (defends against malformed indices).
        let mut indices = Vec::with_capacity(raw_indices.len());
        for tri in raw_indices.chunks_exact(3) {
            if (tri[0] as usize) < vc && (tri[1] as usize) < vc && (tri[2] as usize) < vc {
                indices.extend_from_slice(tri);
            }
        }

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
        // [0,0,0,3,1,0] -> the two triangles of a quad: 0,1,2, 0,2,3
        assert_eq!(decode_indices_u16(&[0, 0, 0, 3, 1, 0]), vec![0u16, 1, 2, 0, 2, 3]);
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
