//! The decoupled **Geo stream thread** (ADR-0012). A Rust-owned thread takes ownership of a
//! [`GeoWorld`](crate::geo_world::GeoWorld) (the `bevy_ecs` orchestration World) and loops its schedule via
//! [`advance_from_ue`](crate::geo_world::GeoWorld::advance_from_ue), fed the newest camera view by the game
//! thread and publishing the latest render frame into a lock-light mailbox the game thread polls. The game
//! thread NEVER touches the world or blocks on streaming — it only [`set_view`] (a cheap newest-wins write)
//! and [`poll`] (the latest frame, retained for the renderer via `Arc` geometry).
//!
//! [`set_view`]: GlobeStreamer::set_view
//! [`poll`]: GlobeStreamer::poll
//!
//! ## Threading contract (ADR-0012)
//! - The **stream thread** is the single owner/mutator of the `GeoWorld` (the §11 owner-thread rule,
//!   relocated off the game thread). It is the only thread that decodes, rebases, traverses, or evicts.
//! - The **game thread** is a pure producer (`set_view`, newest-wins) + consumer (`poll`/`Drop`). It shares
//!   nothing mutable with the world — only the two brief-critical-section `Mutex`es and the lock-free
//!   telemetry atomics below.
//! - Geometry crosses as `Arc<TileGeometry>` (ADR-0012): a polled frame holds owned `Arc` clones, so the
//!   stream thread can evict/overwrite freely without ever dangling a frame the renderer still holds.

use crate::geo_world::{GeoRenderFrame, GeoWorld};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::Duration;

/// The newest desired view pushed by the game thread (newest-wins). Unreal-space camera (cm), the frame the
/// returned geometry is in (ENU origin at Unreal `(0,0,0)`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ViewParams {
    pub camera_ue_cm: [f64; 3],
    /// Camera **forward** direction in UE space (need not be normalized; `[0,0,0]` = unknown ⇒ no
    /// view-direction fetch weighting + an accept-all frustum). Drives the §9.2 look-direction load priority.
    pub camera_dir_ue: [f64; 3],
    /// Camera **up** direction in UE space (need not be normalized). With `camera_dir_ue` + `aspect` + the
    /// FOV it builds the view frustum for frustum-aware LOD (in-view full detail, off-view coarse shell).
    pub camera_up_ue: [f64; 3],
    /// Viewport aspect ratio (width / height) — gives the frustum its horizontal extent from the vertical FOV.
    pub aspect: f64,
    pub fov_y_radians: f64,
    pub viewport_height: f64,
    pub max_sse: f64,
}

/// The result of polling the streamer for the latest frame.
pub enum StreamPoll {
    /// A frame newer than the last poll — the renderer should (re)build from it. Owns its geometry via `Arc`,
    /// valid until the renderer's next poll.
    New(GeoRenderFrame),
    /// No new frame since the last poll — reuse what was already built (no work).
    Same,
    /// Nothing published yet (the stream thread has not produced a frame).
    Empty,
}

/// Cross-thread state between the game thread (produces views, consumes frames) and the stream thread
/// (consumes views, produces frames). Every field is either a brief-critical-section `Mutex` or a lock-free
/// atomic — never the world, which the stream thread owns exclusively.
struct StreamShared {
    /// Newest desired view (newest-wins). `None` until the first `set_view`.
    view: Mutex<Option<ViewParams>>,
    /// The latest published render frame (latest-wins mailbox). Geometry stays alive via `Arc` for whoever
    /// holds a clone, so overwriting here never dangles a frame the renderer is still using.
    latest: Mutex<Option<GeoRenderFrame>>,
    /// Lifecycle: cleared to ask the stream thread to finish the current iteration and exit.
    running: AtomicBool,
    /// Frames the stream thread published into the mailbox (telemetry; `produced - polled` ≈ frames
    /// superseded before the renderer saw them, which under latest-wins are intentional skips).
    produced: AtomicU64,
    resident_tiles: AtomicUsize,
    in_flight: AtomicUsize,
    cached_bytes: AtomicU64,
    /// Cumulative tiles that GAVE UP loading, mirrored from `GeoStats` for the game thread (lock-free
    /// telemetry). Split so a diagnostic can tell the two reasons a tile stays stuck at its coarse (blurry)
    /// ancestor apart: `fetch_failed` = the bytes never arrived (dead URL / network / expired session, after
    /// retries); `decode_failed` = bytes arrived but didn't parse (unsupported format like Draco/KTX2, or
    /// corrupt). A climbing `fetch_failed` points at the network; a climbing `decode_failed` at the codec.
    decode_failed: AtomicU64,
    fetch_failed: AtomicU64,
    /// External-tileset manifests that fetched but failed to GRAFT (subtree never appears → region stuck
    /// coarse), and selected tiles whose content URI failed to RESOLVE (never fetched → region stuck coarse).
    /// Both were silent before; a climbing count is the patchy-stuck-blur cause. Mirrored from `GeoStats`.
    graft_failed: AtomicU64,
    url_resolve_failed: AtomicU64,
}

/// The game-thread handle to a decoupled Geo stream. Owns the worker thread; dropping it stops + joins it.
pub struct GlobeStreamer {
    shared: Arc<StreamShared>,
    thread: Option<JoinHandle<()>>,
    last_polled_id: u64,
    polled: u64,
}

impl GlobeStreamer {
    /// Take ownership of `world` and spawn the stream thread that advances it continuously. The thread parks
    /// up to `idle_park` whenever there is no view change and nothing in flight, so a still camera over a
    /// fully-streamed scene costs ~nothing (no busy-spin, no pointless re-traversal). Drive it from the game
    /// thread with [`set_view`](Self::set_view) and [`poll`](Self::poll).
    pub fn spawn(world: GeoWorld, idle_park: Duration) -> Self {
        let shared = Arc::new(StreamShared {
            view: Mutex::new(None),
            latest: Mutex::new(None),
            running: AtomicBool::new(true),
            produced: AtomicU64::new(0),
            resident_tiles: AtomicUsize::new(0),
            in_flight: AtomicUsize::new(0),
            cached_bytes: AtomicU64::new(0),
            decode_failed: AtomicU64::new(0),
            fetch_failed: AtomicU64::new(0),
            graft_failed: AtomicU64::new(0),
            url_resolve_failed: AtomicU64::new(0),
        });
        let thread_shared = Arc::clone(&shared);
        let thread = std::thread::Builder::new()
            .name("LogiGeoStream".to_string())
            // Generous stack: the tileset traversal recurses up to `MAX_TILE_DEPTH` (512) levels for a deeply
            // grafted "tree of trees", and each level's frame × 512 exceeds the 2 MB default spawned-thread
            // stack. 32 MiB leaves ample margin so a pathologically deep (but capped) tileset can never overflow.
            .stack_size(32 * 1024 * 1024)
            .spawn(move || run_stream(world, thread_shared, idle_park))
            .expect("spawn Geo stream thread");
        Self {
            shared,
            thread: Some(thread),
            last_polled_id: 0,
            polled: 0,
        }
    }

    /// Push the newest desired view (newest-wins; cheap, non-blocking) and wake the stream thread so it
    /// advances promptly on camera motion instead of waiting out its idle park.
    pub fn set_view(&self, view: ViewParams) {
        *lock(&self.shared.view) = Some(view);
        if let Some(t) = &self.thread {
            t.thread().unpark();
        }
    }

    /// Poll the latest published frame. `New` carries an owned frame (geometry pinned via `Arc`) the renderer
    /// keeps until its next poll; `Same` means no new frame since the last poll; `Empty` means nothing yet.
    /// Non-blocking — a brief lock, then an `Arc`-bump clone out.
    pub fn poll(&mut self) -> StreamPoll {
        let poll = {
            let latest = lock(&self.shared.latest);
            match latest.as_ref() {
                Some(f) if f.frame != self.last_polled_id => StreamPoll::New(f.clone()),
                Some(_) => StreamPoll::Same,
                None => StreamPoll::Empty,
            }
        };
        if let StreamPoll::New(f) = &poll {
            self.last_polled_id = f.frame;
            self.polled += 1;
        }
        poll
    }

    /// Resident decoded tiles (telemetry, lock-free).
    pub fn resident_tiles(&self) -> usize {
        self.shared.resident_tiles.load(Ordering::Relaxed)
    }
    /// Outstanding/undrained tile fetches (telemetry, lock-free).
    pub fn in_flight(&self) -> usize {
        self.shared.in_flight.load(Ordering::Relaxed)
    }
    /// Bytes resident in the persistent disk cache (telemetry, lock-free).
    pub fn cached_bytes(&self) -> u64 {
        self.shared.cached_bytes.load(Ordering::Relaxed)
    }
    /// Cumulative tiles that gave up DECODING — bytes arrived but didn't parse (unsupported format like
    /// Draco/KTX2, or corrupt). Terminal; the tile stays at its coarse ancestor (telemetry, lock-free).
    pub fn decode_failed(&self) -> u64 {
        self.shared.decode_failed.load(Ordering::Relaxed)
    }
    /// Cumulative tiles that gave up FETCHING after retries — bytes never arrived (dead URL / network /
    /// expired session). Terminal; the tile stays at its coarse ancestor (telemetry, lock-free).
    pub fn fetch_failed(&self) -> u64 {
        self.shared.fetch_failed.load(Ordering::Relaxed)
    }
    /// Cumulative external-tileset manifests that fetched but FAILED TO GRAFT — the subtree never appears, so
    /// its region is stuck at the coarse parent (telemetry, lock-free).
    pub fn graft_failed(&self) -> u64 {
        self.shared.graft_failed.load(Ordering::Relaxed)
    }
    /// Cumulative selected tiles whose content URI FAILED TO RESOLVE — never fetched, region stuck coarse
    /// (telemetry, lock-free).
    pub fn url_resolve_failed(&self) -> u64 {
        self.shared.url_resolve_failed.load(Ordering::Relaxed)
    }
    /// Frames the stream thread has published (telemetry).
    pub fn produced(&self) -> u64 {
        self.shared.produced.load(Ordering::Relaxed)
    }
    /// Frames the game thread has consumed as `New` (telemetry).
    pub fn polled(&self) -> u64 {
        self.polled
    }

    /// Signal the stream thread to finish and join it. Idempotent; also run by `Drop`.
    pub fn stop(&mut self) {
        self.shared.running.store(false, Ordering::Release);
        if let Some(t) = self.thread.take() {
            t.thread().unpark(); // wake it if parked so it observes the stop promptly
            let _ = t.join();
        }
    }
}

impl Drop for GlobeStreamer {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Lock a `Mutex`, recovering the inner value if a previous holder panicked. The game/render path must not
/// die because the stream thread hiccuped (and at the FFI boundary the panic is already contained by
/// `catch_unwind`); poisoning here would only convert a contained fault into a second one.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Brief yield while actively streaming, so a burst of arriving tiles drains promptly without the thread
/// busy-spinning between iterations.
const SHORT_PARK: Duration = Duration::from_millis(1);

/// The stream-thread body: own `world`, advance it against the newest view, mirror the latest published
/// frame into the shared mailbox, and refresh telemetry. Advances only when the camera moved or fetches are
/// still draining; otherwise idles (no re-traversal of an unchanged scene). Exits when `running` clears; the
/// world (and its fetch workers) are dropped here on the way out.
fn run_stream(mut world: GeoWorld, shared: Arc<StreamShared>, idle_park: Duration) {
    let mut last_view: Option<ViewParams> = None;
    let mut frame: u64 = 0;
    while shared.running.load(Ordering::Acquire) {
        let view = *lock(&shared.view);
        let view_changed = view != last_view;
        // Advance when the camera moved OR fetches are still draining; else idle (no pointless re-traversal).
        let work = view.is_some() && (view_changed || world.in_flight() > 0);
        match (work, view) {
            (true, Some(v)) => {
                frame += 1;
                world.advance_from_ue(
                    v.camera_ue_cm,
                    v.camera_dir_ue,
                    v.camera_up_ue,
                    v.aspect,
                    v.fov_y_radians,
                    v.viewport_height,
                    v.max_sse,
                    frame,
                );
                last_view = Some(v);
                // Mirror the GeoWorld's freshly-published frame into the cross-thread mailbox (an `Arc`-bump
                // clone). `latest_frame` reads the schedule's `publish` output.
                *lock(&shared.latest) = Some(world.latest_frame());
                shared.produced.fetch_add(1, Ordering::Relaxed);
                shared.resident_tiles.store(world.resident_tiles(), Ordering::Relaxed);
                shared.in_flight.store(world.in_flight(), Ordering::Relaxed);
                shared.cached_bytes.store(world.cached_bytes(), Ordering::Relaxed);
                let stats = world.stats();
                shared
                    .decode_failed
                    .store(u64::from(stats.decode_failed_total), Ordering::Relaxed);
                shared
                    .fetch_failed
                    .store(u64::from(stats.fetch_failed_total), Ordering::Relaxed);
                shared
                    .graft_failed
                    .store(u64::from(stats.graft_failed_total), Ordering::Relaxed);
                shared
                    .url_resolve_failed
                    .store(u64::from(stats.url_resolve_failed_total), Ordering::Relaxed);
                std::thread::park_timeout(SHORT_PARK);
            }
            _ => std::thread::park_timeout(idle_park),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use logi_geospatial::Cartographic;

    /// A root tileset with a bounding sphere on the equator and NO content/children — so traversal selects
    /// the (contentless) root, requests nothing (no network), and publishes an empty render list. Lets us
    /// exercise the whole thread/handoff path deterministically and offline.
    fn minimal_world() -> GeoWorld {
        let root = br#"{"asset":{"version":"1.1"},"geometricError":100,
            "root":{"boundingVolume":{"sphere":[6378137,0,0,1]},"geometricError":10}}"#;
        GeoWorld::from_tileset(
            "https://example.invalid/tileset.json".to_string(),
            root,
            Cartographic::from_degrees(0.0, 0.0, 0.0),
            Box::new(logi_3dtiles_io::SyntheticSource::new()),
            1.0e9, // no distance cull
            64,    // pre-spawned tile pool
            4096,  // resident cap
        )
        .expect("minimal tileset parses")
    }

    #[test]
    fn streamer_publishes_a_frame_after_a_view_then_stops_cleanly() {
        let mut streamer = GlobeStreamer::spawn(minimal_world(), Duration::from_millis(5));
        // Before any view: nothing produced.
        assert!(matches!(streamer.poll(), StreamPoll::Empty));

        streamer.set_view(ViewParams {
            camera_ue_cm: [0.0, 0.0, 0.0],
            camera_dir_ue: [0.0, 0.0, -1.0],
            camera_up_ue: [0.0, 0.0, 1.0],
            aspect: 16.0 / 9.0,
            fov_y_radians: 1.0,
            viewport_height: 1080.0,
            max_sse: 16.0,
        });

        // The thread advances on the pushed view and publishes a frame; poll until we see it (generously
        // bounded — the thread parks ≤5 ms, so this resolves in a handful of iterations).
        let mut frame = None;
        for _ in 0..400 {
            if let StreamPoll::New(f) = streamer.poll() {
                frame = Some(f);
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let frame = frame.expect("a view push must yield a published frame");
        assert!(frame.tiles.is_empty(), "contentless root → empty render list");
        assert!(streamer.produced() >= 1);
        assert_eq!(streamer.polled(), 1);

        // A second poll with no newer frame is `Same` (no rebuild).
        assert!(matches!(streamer.poll(), StreamPoll::Same));

        // Drop stops + joins the thread; this returning at all proves a clean shutdown (no hang).
        drop(streamer);
    }

    #[test]
    fn explicit_stop_is_idempotent() {
        let mut streamer = GlobeStreamer::spawn(minimal_world(), Duration::from_millis(5));
        streamer.stop();
        streamer.stop(); // second stop is a no-op, must not panic or hang
    }
}
