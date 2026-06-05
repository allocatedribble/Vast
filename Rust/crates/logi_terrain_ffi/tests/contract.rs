//! Exercises the C-ABI `LogiTerrainWorld` surface exactly as Unity will. Run with `cargo test`.
//!
//! The exported entry points are `unsafe` (they take raw pointers from the C caller); each call below is
//! wrapped in `unsafe { .. }` to honour that contract — the pointers are all valid by construction here.

// The ffi crate's lib is named `logi_terrain` (so the plugin is logi_terrain.dll).
use logi_terrain::ffi::{
    logi_terrain_abi_version, logi_terrain_world_acquire_buffer_view, logi_terrain_world_create, logi_terrain_world_destroy,
    logi_terrain_world_get_frame_packet, logi_terrain_world_release_frame, logi_terrain_world_set_camera, logi_terrain_world_step,
    LogiStatus,
};
use logi_terrain_core::{DVec3, Vec3};
use logi_terrain_scene::{LogiBufferView, LogiCameraState, LogiFramePacket, LogiTerrainPatchDraw};

fn camera_over_world() -> LogiCameraState {
    LogiCameraState {
        position: DVec3::new(0.0, 300.0, 0.0),
        forward: Vec3::new(0.0, -0.4, 1.0),
        up: Vec3::new(0.0, 1.0, 0.0),
        fov_y_radians: 1.0,
        viewport_width: 1920.0,
        viewport_height: 1080.0,
        sse_target_pixels: 2.0,
    }
}

#[test]
fn world_streams_tiles_and_serves_buffer_views() {
    assert_eq!(logi_terrain_abi_version(), 3, "ABI v3");

    let world = unsafe { logi_terrain_world_create(std::ptr::null()) };
    assert!(!world.is_null());

    let camera = camera_over_world();
    assert_eq!(unsafe { logi_terrain_world_set_camera(world, &camera as *const _) }, LogiStatus::Ok);

    let mut frame_id: u64 = 0;
    let mut packet: LogiFramePacket = unsafe { std::mem::zeroed() };

    // Warm up: full step -> get -> release cycle so synthetic tiles stream in.
    for _ in 0..40 {
        assert_eq!(unsafe { logi_terrain_world_step(world, 0.016, &mut frame_id as *mut u64) }, LogiStatus::Ok);
        assert_ne!(frame_id, 0);
        assert_eq!(
            unsafe { logi_terrain_world_get_frame_packet(world, frame_id, &mut packet as *mut _) },
            LogiStatus::Ok
        );
        unsafe { logi_terrain_world_release_frame(world, frame_id) };
    }

    // Final frame: inspect before releasing.
    assert_eq!(unsafe { logi_terrain_world_step(world, 0.016, &mut frame_id as *mut u64) }, LogiStatus::Ok);
    assert_eq!(
        unsafe { logi_terrain_world_get_frame_packet(world, frame_id, &mut packet as *mut _) },
        LogiStatus::Ok
    );

    assert_eq!(packet.frame_id, frame_id);
    assert!(packet.terrain_patch_count >= 1, "streamed patches after warmup: {}", packet.terrain_patch_count);
    assert!(!packet.terrain_patches.is_null());
    // The not-yet-implemented descriptor arrays are present-but-empty.
    assert_eq!(packet.instance_batch_count, 0);
    assert_eq!(packet.collider_command_count, 0);
    assert_eq!(packet.attribution_count, 0);
    assert!(packet.telemetry.frames > 0);

    // The first patch references geometry by handle; resolve both buffers to zero-copy views.
    let patch = unsafe { *packet.terrain_patches };
    assert!(patch.vertex_count > 0 && patch.index_count > 0);
    assert_eq!(patch.vertex_stride, 32);

    let mut vview: LogiBufferView = unsafe { std::mem::zeroed() };
    assert_eq!(
        unsafe { logi_terrain_world_acquire_buffer_view(world, patch.vertex_buffer, &mut vview as *mut _) },
        LogiStatus::Ok
    );
    assert!(!vview.data.is_null());
    assert_eq!(vview.element_stride, 32);
    assert_eq!(vview.byte_length, (patch.vertex_count as u64) * (patch.vertex_stride as u64));

    let mut iview: LogiBufferView = unsafe { std::mem::zeroed() };
    assert_eq!(
        unsafe { logi_terrain_world_acquire_buffer_view(world, patch.index_buffer, &mut iview as *mut _) },
        LogiStatus::Ok
    );
    assert!(!iview.data.is_null());
    assert_eq!(iview.byte_length, (patch.index_count as u64) * 4);

    unsafe { logi_terrain_world_release_frame(world, frame_id) };
    unsafe { logi_terrain_world_destroy(world) };
}

/// Max patch LOD selected after warmup, for a camera at altitude 2000 with the given SSE target.
fn max_lod_for_sse(sse_target_pixels: f32) -> u32 {
    let world = unsafe { logi_terrain_world_create(std::ptr::null()) };
    assert!(!world.is_null());

    let camera = LogiCameraState {
        position: DVec3::new(0.0, 2000.0, 0.0),
        forward: Vec3::new(0.0, -1.0, 0.0),
        up: Vec3::new(0.0, 0.0, 1.0),
        fov_y_radians: 1.0,
        viewport_width: 1920.0,
        viewport_height: 1080.0,
        sse_target_pixels,
    };
    assert_eq!(unsafe { logi_terrain_world_set_camera(world, &camera as *const _) }, LogiStatus::Ok);

    let mut frame_id: u64 = 0;
    let mut packet: LogiFramePacket = unsafe { std::mem::zeroed() };
    for _ in 0..60 {
        assert_eq!(unsafe { logi_terrain_world_step(world, 0.016, &mut frame_id as *mut u64) }, LogiStatus::Ok);
        assert_eq!(
            unsafe { logi_terrain_world_get_frame_packet(world, frame_id, &mut packet as *mut _) },
            LogiStatus::Ok
        );
        unsafe { logi_terrain_world_release_frame(world, frame_id) };
    }
    assert_eq!(unsafe { logi_terrain_world_step(world, 0.016, &mut frame_id as *mut u64) }, LogiStatus::Ok);
    assert_eq!(
        unsafe { logi_terrain_world_get_frame_packet(world, frame_id, &mut packet as *mut _) },
        LogiStatus::Ok
    );

    let stride = std::mem::size_of::<LogiTerrainPatchDraw>();
    let mut max_lod = 0u32;
    for i in 0..packet.terrain_patch_count as usize {
        let p = unsafe { &*(packet.terrain_patches.cast::<u8>().add(i * stride) as *const LogiTerrainPatchDraw) };
        max_lod = max_lod.max(p.lod);
    }
    unsafe { logi_terrain_world_release_frame(world, frame_id) };
    unsafe { logi_terrain_world_destroy(world) };
    max_lod
}

/// Regression for the "camera state only partially used" bug: the LIVE camera's SSE target must
/// drive LOD, not the hardcoded config. Before the fix both cameras used cfg.sse_target_pixels=2.0
/// and selected the SAME level, so this assertion failed.
#[test]
fn camera_sse_target_drives_lod() {
    let fine = max_lod_for_sse(1.0);
    let coarse = max_lod_for_sse(64.0);
    assert!(
        fine > coarse,
        "smaller SSE target must select finer LOD: fine(sse=1)={fine} vs coarse(sse=64)={coarse}"
    );
}

/// End-to-end frustum culling (§2.3): with a forward-looking camera, no rendered patch may sit behind
/// the camera. Before view-aware selection the omnidirectional window streamed tiles on all sides, so
/// patches well behind the camera would appear and trip this.
#[test]
fn rendered_patches_are_in_front_of_camera() {
    let world = unsafe { logi_terrain_world_create(std::ptr::null()) };
    assert!(!world.is_null());

    let fwd = Vec3::new(0.0, -0.25, 1.0); // look +Z, slightly down; behind = −Z
    let camera = LogiCameraState {
        position: DVec3::new(0.0, 600.0, 0.0),
        forward: fwd,
        up: Vec3::new(0.0, 1.0, 0.0),
        fov_y_radians: 1.0,
        viewport_width: 1920.0,
        viewport_height: 1080.0,
        sse_target_pixels: 2.0,
    };
    assert_eq!(unsafe { logi_terrain_world_set_camera(world, &camera as *const _) }, LogiStatus::Ok);

    let mut frame_id: u64 = 0;
    let mut packet: LogiFramePacket = unsafe { std::mem::zeroed() };
    for _ in 0..60 {
        assert_eq!(unsafe { logi_terrain_world_step(world, 0.016, &mut frame_id as *mut u64) }, LogiStatus::Ok);
        assert_eq!(
            unsafe { logi_terrain_world_get_frame_packet(world, frame_id, &mut packet as *mut _) },
            LogiStatus::Ok
        );
        unsafe { logi_terrain_world_release_frame(world, frame_id) };
    }
    assert_eq!(unsafe { logi_terrain_world_step(world, 0.016, &mut frame_id as *mut u64) }, LogiStatus::Ok);
    assert_eq!(
        unsafe { logi_terrain_world_get_frame_packet(world, frame_id, &mut packet as *mut _) },
        LogiStatus::Ok
    );
    assert!(packet.terrain_patch_count >= 1, "something visible after warmup");

    let f = fwd.normalize();
    // local_position is relative to local_frame.origin; put the camera in the same local space.
    let origin = packet.local_frame.origin;
    let camera_local = (camera.position - origin).as_vec3();
    let stride = std::mem::size_of::<LogiTerrainPatchDraw>();
    for i in 0..packet.terrain_patch_count as usize {
        let p = unsafe { &*(packet.terrain_patches.cast::<u8>().add(i * stride) as *const LogiTerrainPatchDraw) };
        let ahead = (p.local_position - camera_local).dot(f);
        let r = p.bounds.radius;
        assert!(
            ahead >= -2.0 * r - 1.0,
            "patch behind camera not frustum-culled: ahead={ahead}, radius={r}"
        );
    }
    unsafe { logi_terrain_world_release_frame(world, frame_id) };
    unsafe { logi_terrain_world_destroy(world) };
}

/// Procedural elevation is wired in: streamed tiles are displaced, not flat at y=0. `bounds.center.y`
/// is the tile's mean elevation, so a non-trivial spread proves relief.
#[test]
fn terrain_has_relief() {
    let world = unsafe { logi_terrain_world_create(std::ptr::null()) };
    assert!(!world.is_null());
    let camera = camera_over_world();
    assert_eq!(unsafe { logi_terrain_world_set_camera(world, &camera as *const _) }, LogiStatus::Ok);

    let mut frame_id: u64 = 0;
    let mut packet: LogiFramePacket = unsafe { std::mem::zeroed() };
    for _ in 0..60 {
        unsafe { logi_terrain_world_step(world, 0.016, &mut frame_id as *mut u64) };
        unsafe { logi_terrain_world_get_frame_packet(world, frame_id, &mut packet as *mut _) };
        unsafe { logi_terrain_world_release_frame(world, frame_id) };
    }
    unsafe { logi_terrain_world_step(world, 0.016, &mut frame_id as *mut u64) };
    unsafe { logi_terrain_world_get_frame_packet(world, frame_id, &mut packet as *mut _) };
    assert!(packet.terrain_patch_count >= 1);

    let stride = std::mem::size_of::<LogiTerrainPatchDraw>();
    let mut max_abs_y = 0.0f32;
    for i in 0..packet.terrain_patch_count as usize {
        let p = unsafe { &*(packet.terrain_patches.cast::<u8>().add(i * stride) as *const LogiTerrainPatchDraw) };
        max_abs_y = max_abs_y.max(p.bounds.center.y.abs());
    }
    unsafe { logi_terrain_world_release_frame(world, frame_id) };
    unsafe { logi_terrain_world_destroy(world) };
    assert!(max_abs_y > 1.0, "terrain should be displaced, not flat (max |mean elevation| = {max_abs_y})");
}

#[test]
fn world_null_args_rejected_not_crashed() {
    let world = unsafe { logi_terrain_world_create(std::ptr::null()) };
    assert!(!world.is_null());

    // Valid world, null arg.
    assert_eq!(unsafe { logi_terrain_world_set_camera(world, std::ptr::null()) }, LogiStatus::NullArg);
    let mut fid: u64 = 0;
    assert_eq!(unsafe { logi_terrain_world_step(world, 0.016, std::ptr::null_mut()) }, LogiStatus::NullArg);

    // Null world.
    assert_eq!(
        unsafe { logi_terrain_world_set_camera(std::ptr::null_mut(), std::ptr::null()) },
        LogiStatus::InvalidHandle
    );
    assert_eq!(
        unsafe { logi_terrain_world_step(std::ptr::null_mut(), 0.0, &mut fid as *mut u64) },
        LogiStatus::InvalidHandle
    );

    // Stale/unknown frame id.
    let mut packet: LogiFramePacket = unsafe { std::mem::zeroed() };
    assert_eq!(
        unsafe { logi_terrain_world_get_frame_packet(world, 999_999, &mut packet as *mut _) },
        LogiStatus::InvalidHandle
    );

    unsafe { logi_terrain_world_destroy(world) };
    // Destroy on null is a no-op (must not crash).
    unsafe { logi_terrain_world_destroy(std::ptr::null_mut()) };
}
