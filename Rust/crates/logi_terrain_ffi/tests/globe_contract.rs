//! Exercises the streaming-globe C-ABI surface (ADR-0009, Milestone F) exactly as the Unreal plugin will.
//! The layout + null-arg checks are deterministic (offline). The end-to-end stream is `#[ignore]`d and
//! gated on `CESIUM_ION_TOKEN` (live network); run it with:
//!   `$env:CESIUM_ION_TOKEN="<token>"; cargo test -p logi_terrain_ffi --release -- --ignored globe_streams_live`

use logi_terrain::ffi::{
    logi_globe_world_cached_bytes, logi_globe_world_create, logi_globe_world_destroy, logi_globe_world_step,
    LogiGlobeCreateInfo, LogiGlobeFrame, LogiGlobeTile, LogiStatus,
};
use std::mem::size_of;

/// The Rust `#[repr(C)]` layout must match the byte-for-byte sizes documented in `logi_terrain_ffi.h`.
/// A mismatch means the header drifted from the Rust struct — the classic silent-ABI-corruption bug.
#[test]
fn globe_structs_have_the_documented_abi_layout() {
    assert_eq!(size_of::<LogiGlobeCreateInfo>(), 56, "LogiGlobeCreateInfo");
    assert_eq!(size_of::<LogiGlobeTile>(), 80, "LogiGlobeTile");
    assert_eq!(size_of::<LogiGlobeFrame>(), 48, "LogiGlobeFrame");
}

#[test]
fn globe_null_args_rejected_not_crashed() {
    // Null info / null token → null handle (no panic across the boundary).
    assert!(unsafe { logi_globe_world_create(std::ptr::null(), std::ptr::null(), std::ptr::null()) }.is_null());

    let info = LogiGlobeCreateInfo {
        ion_asset_id: 2275207,
        origin_longitude_deg: 0.0,
        origin_latitude_deg: 0.0,
        origin_height_m: 0.0,
        max_tile_distance_m: 6000.0,
        workers: 4,
        _reserved: 0,
        cache_budget_bytes: 0,
    };
    // Non-null info but null token is still rejected (no resolve attempted).
    assert!(unsafe { logi_globe_world_create(&info as *const _, std::ptr::null(), std::ptr::null()) }.is_null());

    // Step on a null world → InvalidHandle; never dereferenced.
    let mut frame: LogiGlobeFrame = unsafe { std::mem::zeroed() };
    assert_eq!(
        unsafe {
            logi_globe_world_step(
                std::ptr::null_mut(),
                0.0,
                0.0,
                0.0,
                1.0,
                1080.0,
                16.0,
                &mut frame as *mut _,
            )
        },
        LogiStatus::InvalidHandle
    );

    // Destroy on null is a no-op (must not crash).
    unsafe { logi_globe_world_destroy(std::ptr::null_mut()) };
}

/// Live end-to-end: resolve the Google P3DT ion asset, stream over Manhattan, and confirm tiles decode
/// into Unreal-space geometry + textures through the C ABI. Opt-in (network + token).
#[test]
#[ignore = "live network; needs CESIUM_ION_TOKEN"]
fn globe_streams_live() {
    let Ok(token) = std::env::var("CESIUM_ION_TOKEN") else {
        eprintln!("CESIUM_ION_TOKEN not set — skipping live globe stream");
        return;
    };
    let token = std::ffi::CString::new(token).expect("token has no NUL");

    // Exercise the persistent disk cache too: a temp dir + a 512 MB budget.
    let cache_dir = std::env::temp_dir().join("logi_p3dt_cache_test");
    let cache_dir_c = std::ffi::CString::new(cache_dir.to_string_lossy().as_ref()).expect("cache dir");

    let info = LogiGlobeCreateInfo {
        ion_asset_id: 2275207, // Google Photorealistic 3D Tiles
        origin_longitude_deg: -73.9857,
        origin_latitude_deg: 40.7484, // Empire State Building
        origin_height_m: 0.0,
        max_tile_distance_m: 6000.0,
        workers: 8,
        _reserved: 0,
        cache_budget_bytes: 512 * 1024 * 1024,
    };
    let world = unsafe { logi_globe_world_create(&info as *const _, token.as_ptr(), cache_dir_c.as_ptr()) };
    assert!(!world.is_null(), "ion resolve / root fetch failed");

    // Camera 800 m south, 1.5 km up: ENU (0,-800,1500) m → Unreal (0, 80000, 150000) cm (negate North).
    let (cx, cy, cz) = (0.0_f64, 80_000.0_f64, 150_000.0_f64);
    let mut frame: LogiGlobeFrame = unsafe { std::mem::zeroed() };
    for _ in 0..80 {
        assert_eq!(
            unsafe {
                logi_globe_world_step(
                    world,
                    cx,
                    cy,
                    cz,
                    60f64.to_radians(),
                    1080.0,
                    16.0,
                    &mut frame as *mut _,
                )
            },
            LogiStatus::Ok
        );
        std::thread::sleep(std::time::Duration::from_millis(70));
    }

    assert!(frame.resident_tiles > 0, "tiles should be resident after warmup");
    assert!(frame.tile_count > 0, "frame should list render tiles");
    assert!(!frame.tiles.is_null());

    // Inspect the first tile: Unreal-space geometry (32-byte stride) + an embedded texture.
    let t0 = unsafe { &*frame.tiles };
    assert!(t0.vertex_count > 0 && t0.index_count > 0);
    assert_eq!(t0.vertex_stride, 32);
    assert_eq!(t0.vertices_len, t0.vertex_count as u64 * 32);
    assert!(!t0.vertices.is_null() && !t0.indices.is_null());
    assert!(
        t0.texture_jpeg_len > 0 && !t0.texture_jpeg.is_null(),
        "P3DT tiles carry a texture"
    );

    // Null out-frame is rejected on a valid world.
    assert_eq!(
        unsafe { logi_globe_world_step(world, cx, cy, cz, 1.0, 1080.0, 16.0, std::ptr::null_mut()) },
        LogiStatus::NullArg
    );

    // The persistent disk cache populated as tiles streamed.
    let cached = unsafe { logi_globe_world_cached_bytes(world) };
    assert!(cached > 0, "disk cache should have populated while streaming");
    eprintln!("disk cache resident: {} MB", cached / (1024 * 1024));

    unsafe { logi_globe_world_destroy(world) };
}
