//! `HeightPage` — quantized elevation (data-model.md §4.2). The first concrete page codec.

use crate::page::PackError;

const HEIGHT_PAGE_MAGIC: &[u8; 4] = b"HGTP";
const HEADER_LEN: usize = 20; // magic4 + width2 + height2 + min4 + max4 + error4

/// A quantized elevation grid. Heights are stored as `u16` mapped into `[min_elevation, max_elevation]`;
/// `hole_mask` marks no-data samples (1 bit each, row-major).
#[derive(Clone, Debug)]
pub struct HeightPage {
    pub width: u16,
    pub height: u16,
    pub min_elevation: f32,
    pub max_elevation: f32,
    pub error_metric: f32,
    pub hole_mask: Vec<u8>,
    pub samples: Vec<u16>,
}

impl HeightPage {
    /// Quantize a row-major heightfield. `holes[i] == true` marks a no-data sample (excluded from range).
    pub fn from_heights(width: u16, height: u16, heights: &[f32], holes: &[bool]) -> Self {
        let n = width as usize * height as usize;
        assert_eq!(heights.len(), n, "heights length must be width*height");

        let mut min_e = f32::INFINITY;
        let mut max_e = f32::NEG_INFINITY;
        for (i, &h) in heights.iter().enumerate() {
            if !holes.get(i).copied().unwrap_or(false) {
                min_e = min_e.min(h);
                max_e = max_e.max(h);
            }
        }
        if !min_e.is_finite() {
            min_e = 0.0;
            max_e = 0.0;
        }
        let range = (max_e - min_e).max(f32::EPSILON);

        let samples: Vec<u16> = heights
            .iter()
            .map(|&h| {
                let t = ((h - min_e) / range).clamp(0.0, 1.0);
                (t * 65535.0 + 0.5) as u16
            })
            .collect();

        let mut hole_mask = vec![0u8; n.div_ceil(8)];
        for (i, &hole) in holes.iter().take(n).enumerate() {
            if hole {
                hole_mask[i / 8] |= 1 << (i % 8);
            }
        }

        Self {
            width,
            height,
            min_elevation: min_e,
            max_elevation: max_e,
            error_metric: range / 65535.0,
            hole_mask,
            samples,
        }
    }

    pub fn is_hole(&self, index: usize) -> bool {
        self.hole_mask
            .get(index / 8)
            .map_or(false, |b| b & (1 << (index % 8)) != 0)
    }

    /// Reconstruct elevation at a sample index (lossy by up to `error_metric`).
    pub fn elevation(&self, index: usize) -> f32 {
        let range = self.max_elevation - self.min_elevation;
        self.min_elevation + (self.samples[index] as f32 / 65535.0) * range
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(HEADER_LEN + self.hole_mask.len() + self.samples.len() * 2);
        out.extend_from_slice(HEIGHT_PAGE_MAGIC);
        out.extend_from_slice(&self.width.to_le_bytes());
        out.extend_from_slice(&self.height.to_le_bytes());
        out.extend_from_slice(&self.min_elevation.to_le_bytes());
        out.extend_from_slice(&self.max_elevation.to_le_bytes());
        out.extend_from_slice(&self.error_metric.to_le_bytes());
        out.extend_from_slice(&self.hole_mask);
        for &s in &self.samples {
            out.extend_from_slice(&s.to_le_bytes());
        }
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, PackError> {
        if bytes.len() < HEADER_LEN || &bytes[0..4] != HEIGHT_PAGE_MAGIC {
            return Err(PackError::BadPage);
        }
        let width = u16::from_le_bytes([bytes[4], bytes[5]]);
        let height = u16::from_le_bytes([bytes[6], bytes[7]]);
        let min_elevation = f32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);
        let max_elevation = f32::from_le_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]);
        let error_metric = f32::from_le_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);

        let n = width as usize * height as usize;
        let mask_len = n.div_ceil(8);
        let samples_off = HEADER_LEN + mask_len;
        if bytes.len() != samples_off + n * 2 {
            return Err(PackError::BadPage);
        }

        let hole_mask = bytes[HEADER_LEN..samples_off].to_vec();
        let mut samples = Vec::with_capacity(n);
        for i in 0..n {
            let at = samples_off + i * 2;
            samples.push(u16::from_le_bytes([bytes[at], bytes[at + 1]]));
        }

        Ok(Self { width, height, min_elevation, max_elevation, error_metric, hole_mask, samples })
    }
}
