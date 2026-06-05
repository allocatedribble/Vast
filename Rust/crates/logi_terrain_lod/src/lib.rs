//! LOD selection: screen-space error driven refinement (the same model 3D Tiles / CDLOD use).

/// Screen-space error in pixels for a feature of `geometric_error` world-units viewed at `distance`,
/// with vertical FOV `fov_y_radians` and `viewport_height` pixels.
#[inline]
pub fn screen_space_error(geometric_error: f64, distance: f64, fov_y_radians: f64, viewport_height: f64) -> f64 {
    if distance <= 1e-6 {
        return f64::INFINITY;
    }
    // FIX 4: clamp fov away from 0/PI so tan() can't go to 0 or blow up (inf/NaN SSE).
    let t = (fov_y_radians.clamp(1e-3, std::f64::consts::PI - 1e-3) * 0.5)
        .tan()
        .max(1e-4);
    let pixels_per_unit = viewport_height / (2.0 * distance * t);
    geometric_error * pixels_per_unit
}

/// The coarsest level (0..=`max_level`) whose screen-space error is already within
/// `sse_target_pixels` at `distance` — i.e. the lowest detail that still meets the target (refining
/// further would be wasted). Model: geometric error halves each level.
pub fn desired_level(
    base_geometric_error: f64,
    distance: f64,
    fov_y_radians: f64,
    viewport_height: f64,
    sse_target_pixels: f64,
    max_level: u8,
) -> u8 {
    let mut level = 0u8;
    while level < max_level {
        // FIX 5: multiply by 0.5^level instead of dividing by `1 << level` — no shift overflow past
        // level 63 and exact enough for the SSE comparison.
        let ge = base_geometric_error * 0.5f64.powi(level as i32);
        if screen_space_error(ge, distance, fov_y_radians, viewport_height) <= sse_target_pixels {
            break;
        }
        level += 1;
    }
    level
}

/// Render bands (rendering-strategy.md §5.3): the detail/behavior tier a patch belongs to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TerrainBand {
    Near,
    Mid,
    Far,
    Orbital,
}

/// Classify a patch into a render band by camera `distance` (world units), given the band radii.
pub fn classify_band(distance: f64, near: f64, mid: f64, far: f64) -> TerrainBand {
    if distance <= near {
        TerrainBand::Near
    } else if distance <= mid {
        TerrainBand::Mid
    } else if distance <= far {
        TerrainBand::Far
    } else {
        TerrainBand::Orbital
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bands_classify_by_distance() {
        assert_eq!(classify_band(50.0, 100.0, 1000.0, 10000.0), TerrainBand::Near);
        assert_eq!(classify_band(500.0, 100.0, 1000.0, 10000.0), TerrainBand::Mid);
        assert_eq!(classify_band(5000.0, 100.0, 1000.0, 10000.0), TerrainBand::Far);
        assert_eq!(classify_band(50000.0, 100.0, 1000.0, 10000.0), TerrainBand::Orbital);
    }

    #[test]
    fn sse_decreases_with_distance() {
        let near = screen_space_error(10.0, 100.0, 1.0, 1080.0);
        let far = screen_space_error(10.0, 10000.0, 1.0, 1080.0);
        assert!(near > far);
    }

    #[test]
    fn coarser_sse_target_picks_lower_level() {
        let fine = desired_level(4096.0, 5000.0, 1.0, 1080.0, 1.0, 12);
        let coarse = desired_level(4096.0, 5000.0, 1.0, 1080.0, 64.0, 12);
        assert!(
            fine > coarse,
            "smaller SSE target -> finer level: fine {fine} vs coarse {coarse}"
        );
    }
}
