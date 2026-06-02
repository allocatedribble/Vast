use logi_terrain_decode::height_mesh;
use logi_terrain_pack::HeightPage;

#[test]
fn height_mesh_has_expected_shape_and_relief() {
    let res = 17u16;
    let n = res as usize;

    let mut heights = Vec::with_capacity(n * n);
    for j in 0..n {
        for i in 0..n {
            heights.push(50.0 * ((i as f32 * 0.5).sin() + (j as f32 * 0.4).cos()));
        }
    }
    let page = HeightPage::from_heights(res, res, &heights, &vec![false; n * n]);
    let mesh = height_mesh(&page, 1000.0);

    assert_eq!(mesh.vertex_count, (n * n) as u32);
    assert_eq!(mesh.index_count, ((n - 1) * (n - 1) * 6) as u32);
    assert_eq!(mesh.vertices.len(), n * n * 32, "32-byte interleaved vertices");
    assert_eq!(mesh.indices.len(), (n - 1) * (n - 1) * 6 * 4, "u32 indices");

    // The mesh must actually be displaced: the y channel (bytes 4..8 of each 32-byte vertex) varies.
    let mut min_y = f32::INFINITY;
    let mut max_y = f32::NEG_INFINITY;
    for v in 0..(n * n) {
        let at = v * 32 + 4;
        let y = f32::from_le_bytes([
            mesh.vertices[at],
            mesh.vertices[at + 1],
            mesh.vertices[at + 2],
            mesh.vertices[at + 3],
        ]);
        min_y = min_y.min(y);
        max_y = max_y.max(y);
    }
    assert!(max_y - min_y > 1.0, "terrain should have relief: {min_y}..{max_y}");
}
