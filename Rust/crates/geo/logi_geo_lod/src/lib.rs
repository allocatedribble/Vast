//! LOD selection: screen-space error driven refinement (the same model 3D Tiles / CDLOD use).

/// Screen-space error in pixels for a feature of `geometric_error` world-units viewed at `distance`,
/// with vertical FOV `fov_y_radians` and `viewport_height` pixels.
#[inline]
pub fn screen_space_error(geometric_error: f64, distance: f64, fov_y_radians: f64, viewport_height: f64) -> f64 {
    // M1 (SSE edge): a feature with zero geometric error contributes zero screen-space error at ANY
    // distance — including the degenerate camera-on-feature case. Check this BEFORE the distance clamp so
    // `ge == 0` at `distance ~ 0` yields 0 (already at target), not INFINITY (which would refine forever).
    if geometric_error <= 0.0 {
        return 0.0;
    }
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

/// Parameters for SSE-driven hierarchical refinement — the single criterion both the baked quadtree
/// selector (`logi_semantic::world::BakedWorld::select`) and the 3D-Tiles pointer-tree selector
/// (`logi_3dtiles_select::select_tree_implicit`) descend by (audit M22/DUP-2). The tree-walks stay separate (they
/// traverse structurally different trees — an indexed quadtree vs a pointer tree threading a world
/// transform and a streaming fallback); only the refine *decision* below is shared.
#[derive(Clone, Copy, Debug)]
pub struct RefineParams {
    pub fov_y_radians: f64,
    pub viewport_height: f64,
    /// Target screen-space error in pixels: refine while a tile's SSE exceeds this.
    pub sse_target: f64,
    /// Hysteresis dead-band fraction (`0.0` = none). A tile refined on the previous pass coarsens only once
    /// its SSE drops below `sse_target / (1 + hysteresis)`; a fresh/coarse tile refines at the plain target.
    /// This holds a tile's state when SSE sits on the line instead of flip-flopping frame to frame.
    /// `hysteresis == 0.0` (or `was_refined == false`) reproduces the stateless plain-threshold behavior.
    pub hysteresis: f64,
}

/// The shared refine decision: should a tile whose screen-space error is computed from `geometric_error`
/// at `distance` (camera→tile, in the SAME world units as `geometric_error`) be **refined into its
/// children**? The caller still gates on the tile actually HAVING (resident) children. `was_refined` is
/// whether this tile was refined on the previous pass — pass `false` for a stateless selector. This is the
/// one home for the criterion + hysteresis dead-band (audit M22/DUP-2).
#[inline]
pub fn should_refine(geometric_error: f64, distance: f64, params: &RefineParams, was_refined: bool) -> bool {
    let sse = screen_space_error(
        geometric_error,
        distance.max(1.0),
        params.fov_y_radians,
        params.viewport_height,
    );
    let threshold = if was_refined && params.hysteresis > 0.0 {
        params.sse_target / (1.0 + params.hysteresis)
    } else {
        params.sse_target
    };
    sse > threshold
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sse_decreases_with_distance() {
        let near = screen_space_error(10.0, 100.0, 1.0, 1080.0);
        let far = screen_space_error(10.0, 10000.0, 1.0, 1080.0);
        assert!(near > far);
    }

    /// M1 (SSE edge): a feature with zero geometric error has zero screen-space error at every distance —
    /// including the degenerate camera-on-feature case — so it never forces refinement (no INFINITY).
    #[test]
    fn zero_geometric_error_has_zero_sse_even_at_the_camera() {
        assert_eq!(screen_space_error(0.0, 0.0, 1.0, 1080.0), 0.0);
        assert_eq!(screen_space_error(0.0, 1e-9, 1.0, 1080.0), 0.0);
        assert_eq!(screen_space_error(0.0, 10_000.0, 1.0, 1080.0), 0.0);
    }

    #[test]
    fn should_refine_refines_above_target_keeps_below() {
        let p = RefineParams {
            fov_y_radians: 1.0,
            viewport_height: 1080.0,
            sse_target: 4.0,
            hysteresis: 0.0,
        };
        assert!(
            should_refine(4096.0, 200.0, &p, false),
            "big error / close camera ⇒ high SSE ⇒ refine"
        );
        assert!(
            !should_refine(0.001, 50_000.0, &p, false),
            "tiny error / far ⇒ low SSE ⇒ keep"
        );
    }

    #[test]
    fn hysteresis_dead_band_holds_a_refined_tile() {
        let plain = RefineParams {
            fov_y_radians: 1.0,
            viewport_height: 1080.0,
            sse_target: 4.0,
            hysteresis: 0.0,
        };
        let hyst = RefineParams {
            hysteresis: 0.25,
            ..plain
        };
        // ge/dist chosen so SSE ≈ 3.6 px — between target/(1+H)=3.2 and target=4 (the dead-band).
        let (ge, dist) = (3.643, 1000.0);
        assert!(!should_refine(ge, dist, &plain, false), "below plain target ⇒ keep");
        assert!(
            !should_refine(ge, dist, &hyst, false),
            "fresh tile uses the plain target ⇒ keep"
        );
        assert!(
            should_refine(ge, dist, &hyst, true),
            "a tile refined last pass holds in the dead-band"
        );
        assert!(
            !should_refine(ge, dist, &plain, true),
            "no hysteresis ⇒ was_refined has no effect"
        );
    }
}
