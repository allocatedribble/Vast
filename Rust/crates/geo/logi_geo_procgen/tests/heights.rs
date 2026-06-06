use logi_geo_procgen::TerrainHeights;

#[test]
fn heights_deterministic_and_bounded() {
    let g = TerrainHeights::new(1234);
    assert_eq!(g.height_at(123.0, -456.0), g.height_at(123.0, -456.0), "deterministic");

    for &(x, z) in &[(0.0, 0.0), (10_000.0, -20_000.0), (-99_999.0, 88_888.0)] {
        let h = g.height_at(x, z);
        assert!(h.abs() <= g.amplitude as f32 + 1.0, "bounded by amplitude: {h}");
    }
}

#[test]
fn adjacent_tiles_share_border_heights() {
    let g = TerrainHeights::new(42);
    let size = 1000.0;
    let res = 33u16;
    let n = res as usize;

    // Tile A spans x in [0, size]; tile B spans x in [size, 2*size] — they share the x = size edge.
    let a = g.height_page(0.0, 0.0, size, res);
    let b = g.height_page(size, 0.0, size, res);

    let tol = a.error_metric + b.error_metric + 1e-2;
    for j in 0..n {
        let right_edge_a = a.elevation(j * n + (n - 1)); // i = n-1 -> x = size
        let left_edge_b = b.elevation(j * n); // i = 0 -> x = size
        assert!(
            (right_edge_a - left_edge_b).abs() <= tol,
            "row {j}: {right_edge_a} vs {left_edge_b} (tol {tol})"
        );
    }
}
