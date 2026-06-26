//! Exercises the streaming-globe C-ABI surface (ADR-0009, Milestone F) exactly as the Unreal plugin will.
//! The layout + null-arg checks are deterministic (offline). The end-to-end stream is `#[ignore]`d and
//! gated on `CESIUM_ION_TOKEN` (live network); run it with:
//!   `$env:CESIUM_ION_TOKEN="<token>"; cargo test -p logi_geo_ffi --release -- --ignored globe_streams_live`

use logi_geo::ffi::{
    logi_globe_world_create, logi_globe_world_destroy, logi_globe_world_poll, logi_globe_world_set_view,
    logi_globe_world_telemetry, LogiClipBox, LogiGlobeCreateInfo, LogiGlobeFrame, LogiGlobeTelemetry, LogiGlobeTile,
    LogiGlobeView, LogiStatus,
};
use std::mem::size_of;

/// A camera view looking down-ish from the Manhattan camera, for the set_view/poll calls.
fn sample_view(cx: f64, cy: f64, cz: f64) -> LogiGlobeView {
    LogiGlobeView {
        camera_x_cm: cx,
        camera_y_cm: cy,
        camera_z_cm: cz,
        dir_x: 0.0,
        dir_y: 0.0,
        dir_z: -1.0, // looking down
        up_x: 0.0,
        up_y: 1.0,
        up_z: 0.0,
        aspect: 16.0 / 9.0,
        fov_y_radians: 60f64.to_radians(),
        viewport_height: 1080.0,
        max_sse_pixels: 16.0,
    }
}

/// The Rust `#[repr(C)]` layout must match the byte-for-byte sizes documented in `logi_geo_ffi.h`.
/// A mismatch means the header drifted from the Rust struct — the classic silent-ABI-corruption bug.
#[test]
fn globe_structs_have_the_documented_abi_layout() {
    assert_eq!(size_of::<LogiGlobeCreateInfo>(), 64, "LogiGlobeCreateInfo"); // v11: +fetch_backend +_reserved (was 56)
    assert_eq!(size_of::<LogiGlobeTile>(), 104, "LogiGlobeTile"); // v12: +clip_boxes +clip_box_count +_pad_clip (was 88)
    assert_eq!(size_of::<LogiClipBox>(), 48, "LogiClipBox"); // v12: hole-clip OBB (center + 3 half-axes)
    assert_eq!(size_of::<LogiGlobeFrame>(), 80, "LogiGlobeFrame"); // v8: +delta lists +counts +frame_id (was 48)
    assert_eq!(size_of::<LogiGlobeView>(), 104, "LogiGlobeView"); // 13 × f64
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
        flash_cache_budget_mb: 0, // 0 = leave the in-memory flash cache at its default budget
        cache_budget_bytes: 0,
        fetch_backend: 0, // Auto
        _reserved: 0,
    };
    // Non-null info but null token is still rejected (no resolve attempted).
    assert!(unsafe { logi_globe_world_create(&info as *const _, std::ptr::null(), std::ptr::null()) }.is_null());

    // The poll-based surface (ADR-0012) rejects a null world, no deref. A null world is caught before the
    // view is read, so even a valid view pointer returns InvalidHandle.
    let mut frame: LogiGlobeFrame = unsafe { std::mem::zeroed() };
    let view = sample_view(0.0, 0.0, 0.0);
    assert_eq!(
        unsafe { logi_globe_world_set_view(std::ptr::null_mut(), &view as *const _) },
        LogiStatus::InvalidHandle
    );
    assert_eq!(
        unsafe { logi_globe_world_poll(std::ptr::null_mut(), &mut frame as *mut _) },
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
        flash_cache_budget_mb: 256, // exercise the in-memory flash-cache budget path alongside the disk budget
        cache_budget_bytes: 512 * 1024 * 1024,
        fetch_backend: 0, // Auto → CyperSource (HTTP/3/QUIC) for this Google P3DT asset when built with cyper-h2 (default)
        _reserved: 0,
    };
    let world = unsafe { logi_globe_world_create(&info as *const _, token.as_ptr(), cache_dir_c.as_ptr()) };
    assert!(!world.is_null(), "ion resolve / root fetch failed");

    // Camera 800 m south, 1.5 km up: ENU (0,-800,1500) m → Unreal (0, 80000, 150000) cm (negate North).
    let (cx, cy, cz) = (0.0_f64, 80_000.0_f64, 150_000.0_f64);
    let view = sample_view(cx, cy, cz);
    let mut frame: LogiGlobeFrame = unsafe { std::mem::zeroed() };
    for _ in 0..80 {
        // The poll-based surface: push the view, then pull the latest async frame (ADR-0012).
        assert_eq!(
            unsafe { logi_globe_world_set_view(world, &view as *const _) },
            LogiStatus::Ok
        );
        assert_eq!(
            unsafe { logi_globe_world_poll(world, &mut frame as *mut _) },
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

    // v12 hole-clip plumbing, validated WHEN present (presence is scene-dependent — a fully settled or fully
    // unsettled frame may legitimately carry none — so this never *requires* a clip): a stand-in with
    // `clip_box_count > 0` must expose a valid arena slice of sane, finite OBBs; a count of 0 must carry the
    // documented NULL pointer (`build_globe_frame`'s contract).
    let tiles = unsafe { std::slice::from_raw_parts(frame.tiles, frame.tile_count as usize) };
    let mut clipped_tiles = 0u32;
    for t in tiles {
        if t.clip_box_count > 0 {
            clipped_tiles += 1;
            assert!(!t.clip_boxes.is_null(), "clip_box_count > 0 ⇒ non-null clip_boxes");
            assert!(
                t.clip_box_count <= 8,
                "the selector caps clip children at 8 (LOGI_CLIP_MAX / MAX_CLIP_BOXES_PER_TILE)"
            );
            let boxes = unsafe { std::slice::from_raw_parts(t.clip_boxes, t.clip_box_count as usize) };
            for b in boxes {
                assert!(b.center.iter().all(|c| c.is_finite()), "clip center is finite");
                assert!(
                    b.half_axes.iter().flatten().all(|c| c.is_finite()),
                    "clip half-axes are finite"
                );
            }
        } else {
            assert!(
                t.clip_boxes.is_null(),
                "clip_box_count == 0 ⇒ NULL clip_boxes (documented contract)"
            );
        }
        assert_eq!(t._pad_clip, 0, "the v12 alignment pad is always written 0");
    }
    eprintln!(
        "tiles carrying v12 clip boxes this frame: {clipped_tiles}/{}",
        frame.tile_count
    );

    // Null out-frame is rejected on a valid world.
    assert_eq!(
        unsafe { logi_globe_world_poll(world, std::ptr::null_mut()) },
        LogiStatus::NullArg
    );

    // The v16 telemetry block fills whole (the size return is the layout handshake), and the persistent
    // disk cache populated as tiles streamed.
    let mut telemetry = unsafe { std::mem::zeroed::<LogiGlobeTelemetry>() };
    let written = unsafe { logi_globe_world_telemetry(world, &mut telemetry) };
    assert_eq!(written, size_of::<LogiGlobeTelemetry>(), "telemetry size handshake");
    assert!(
        telemetry.disk_bytes > 0,
        "disk cache should have populated while streaming"
    );
    eprintln!("disk cache resident: {} MB", telemetry.disk_bytes / (1024 * 1024));

    unsafe { logi_globe_world_destroy(world) };
}
