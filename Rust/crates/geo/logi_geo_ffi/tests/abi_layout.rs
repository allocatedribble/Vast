//! ABI layout contract for the hand-maintained `include/logi_geo_ffi.h`.
//!
//! The header is NOT cbindgen-generated, so the Rust↔C layouts can only diverge silently. The
//! authoritative guards are the per-field `core::mem::offset_of!` + `size_of` `const _` blocks in
//! `src/ffi.rs` and `src/ffi_baked.rs` (they fail the *build* on drift). This test re-states the same
//! contract as data — one table per struct — so the expected offsets are reviewable in one place against
//! the header, and a CI run prints the offending field name (not just "build failed") if anything drifts.
//!
//! When you intentionally change a boundary struct: update the header, the `const _` asserts in the two
//! `ffi*` files, this table, and `LOGI_ABI_VERSION` together.

use core::mem::{align_of, offset_of, size_of};
use logi_geo::ffi::{
    LogiBuildingData, LogiGlobeCreateInfo, LogiGlobeFrame, LogiGlobeTile, LogiGlobeView, LogiPlacement,
};
use logi_geo::ffi_baked::{LogiBakedCreateInfo, LogiBakedFrame, LogiBakedProceduralInfo, LogiBakedTile};

/// Assert `$ty` has total size `$size`, alignment `$align`, and each `(field, offset)` exactly as the
/// checked-in header lays it out. Mirrors the compile-time `const _` guards in the `ffi*` modules.
macro_rules! assert_layout {
    ($ty:ty, size = $size:expr, align = $align:expr, { $($field:ident => $off:expr),+ $(,)? }) => {{
        assert_eq!(size_of::<$ty>(), $size, concat!(stringify!($ty), " total size"));
        assert_eq!(align_of::<$ty>(), $align, concat!(stringify!($ty), " alignment"));
        $(
            assert_eq!(
                offset_of!($ty, $field),
                $off,
                concat!(stringify!($ty), ".", stringify!($field), " offset"),
            );
        )+
    }};
}

#[test]
fn globe_and_building_structs_match_the_header() {
    assert_layout!(LogiPlacement, size = 32, align = 8, {
        longitude_deg => 0,
        latitude_deg => 8,
        height_m => 16,
        heading_deg => 24,
    });

    assert_layout!(LogiBuildingData, size = 96, align = 8, {
        vertices => 0,
        vertices_len => 8,
        vertex_count => 16,
        vertex_stride => 20,
        indices => 24,
        index_count => 32,
        _pad => 36,
        texture_jpeg => 40,
        texture_jpeg_len => 48,
        bounds_center => 56,
        bounds_radius => 68,
        ecef_origin => 72,
    });

    assert_layout!(LogiGlobeCreateInfo, size = 56, align = 8, {
        ion_asset_id => 0,
        origin_longitude_deg => 8,
        origin_latitude_deg => 16,
        origin_height_m => 24,
        max_tile_distance_m => 32,
        workers => 40,
        mesh_cache_budget_mb => 44,
        cache_budget_bytes => 48,
    });

    assert_layout!(LogiGlobeTile, size = 88, align = 8, {
        tile_key => 0,
        vertices => 8,
        vertices_len => 16,
        vertex_count => 24,
        vertex_stride => 28,
        indices => 32,
        index_count => 40,
        flags => 44,
        texture_jpeg => 48,
        texture_jpeg_len => 56,
        bounds_center => 64,
        bounds_radius => 76,
        depth => 80,
        geometric_error => 84,
    });

    assert_layout!(LogiGlobeFrame, size = 80, align = 8, {
        tiles => 0,
        added_indices => 8,
        removed_keys => 16,
        tile_count => 24,
        added_count => 28,
        removed_count => 32,
        resident_tiles => 36,
        in_flight => 40,
        _pad => 44,
        frame_id => 48,
        origin_ecef => 56,
    });

    assert_layout!(LogiGlobeView, size = 104, align = 8, {
        camera_x_cm => 0,
        camera_y_cm => 8,
        camera_z_cm => 16,
        dir_x => 24,
        dir_y => 32,
        dir_z => 40,
        up_x => 48,
        up_y => 56,
        up_z => 64,
        aspect => 72,
        fov_y_radians => 80,
        viewport_height => 88,
        max_sse_pixels => 96,
    });
}

#[test]
fn baked_structs_match_the_header() {
    assert_layout!(LogiBakedTile, size = 72, align = 8, {
        tile_key => 0,
        vertices => 8,
        vertices_len => 16,
        vertex_count => 24,
        vertex_stride => 28,
        indices => 32,
        index_count => 40,
        depth => 44,
        bounds_center => 48,
        bounds_radius => 60,
        geometric_error => 64,
    });

    assert_layout!(LogiBakedFrame, size = 40, align = 8, {
        tiles => 0,
        tile_count => 8,
        _pad => 12,
        origin_ecef => 16,
    });

    assert_layout!(LogiBakedCreateInfo, size = 12, align = 4, {
        meshing_mode => 0,
        tin_max_error_m => 4,
        skirt_depth_m => 8,
    });

    assert_layout!(LogiBakedProceduralInfo, size = 64, align = 8, {
        seed => 0,
        origin_lon_deg => 8,
        origin_lat_deg => 16,
        origin_height_m => 24,
        radius_m => 32,
        leaf_level => 40,
        grid_n => 44,
        meshing_mode => 48,
        tin_max_error_m => 52,
        skirt_depth_m => 56,
    });
}
