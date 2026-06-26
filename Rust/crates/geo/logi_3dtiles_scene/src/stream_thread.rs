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
//! - The **game thread** is a pure producer (`set_view`/`set_textured_keys`, newest-wins) + consumer
//!   (`poll`/`Drop`). It shares nothing mutable with the world — only the three brief-critical-section
//!   `Mutex`es (newest view, latest frame, textured set) and the lock-free telemetry atomics below.
//! - Geometry crosses as `Arc<TileGeometry>` (ADR-0012): a polled frame holds owned `Arc` clones, so the
//!   stream thread can evict/overwrite freely without ever dangling a frame the renderer still holds.

use crate::geo_world::{GeoRenderFrame, GeoWorld};
use hashbrown::HashSet;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc as StdArc;
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

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
    /// A frame newer than the last poll — the renderer should (re)build from it. Shared via `Arc` (an O(1)
    /// clone out of the mailbox — audit 2026-06-10 §2); geometry stays valid until the renderer's next poll.
    New(StdArc<GeoRenderFrame>),
    /// No new frame since the last poll — reuse what was already built (no work).
    Same,
    /// Nothing published yet (the stream thread has not produced a frame).
    Empty,
}

/// The Geo streaming lane's telemetry, as ONE plain block (the cells-precedent fix for the hand-mirrored
/// counter sprawl: `GeoStats` → ~25 individual atomics → 8 packed FFI accessors became this struct crossing
/// each seam whole). Written by the stream thread once per full step ([`run_stream`]'s mirror), read by the
/// game thread via [`GlobeStreamer::telemetry`]; the FFI copies it across the C ABI as `LogiGlobeTelemetry`
/// (same fields, same order — ABI v17). All counts widen to `u64` at this membrane regardless of their
/// source width. Adding a counter = one field here + one in the FFI mirror + an offset lock, NOT a new
/// entry point.
#[derive(Clone, Copy, Debug, Default)]
pub struct GeoTelemetry {
    /// Resident decoded tiles after the last step.
    pub resident_tiles: u64,
    /// Outstanding/undrained tile fetches.
    pub in_flight: u64,
    /// Frames published into the mailbox (`produced - polled` ≈ frames superseded before the renderer saw
    /// them — intentional skips under latest-wins). Composed from the live counter at read time.
    pub produced_frames: u64,
    /// Stream-thread steps whose body panicked and was caught (the loop continues; see `run_stream`).
    /// Non-zero = the streaming lane hit at least one poisoned step but kept running; rapidly climbing =
    /// a deterministically-bad tile/input. Composed from the live counter at read time.
    pub step_panics: u64,
    // --- Pool pressure (2026-06-11 starvation stall): evicted flat-zero + pool_exhausted climbing at a
    // full pool = eviction is dead and the streamer is starved.
    pub free_slots: u64,
    pub max_resident: u64,
    pub evicted_total: u64,
    pub pool_exhausted_total: u64,
    // --- Give-ups: why a tile stays stuck at its coarse (blurry) ancestor. `fetch_failed` = bytes never
    // arrived (dead URL / network / expired session, after retries); `decode_failed` = bytes arrived but
    // didn't parse; `graft_failed` = a child manifest fetched but never grafted (region stuck coarse);
    // `url_resolve_failed` = a content URI never resolved (never even fetched).
    pub decode_failed_total: u64,
    pub fetch_failed_total: u64,
    pub graft_failed_total: u64,
    pub url_resolve_failed_total: u64,
    /// In-flight fetches pre-emptively cancelled because their tile left the selection (fast camera motion).
    pub fetches_cancelled_total: u64,
    /// Google P3DT session refreshes (root re-fetched for a fresh `?session=…` after a 401/403).
    pub session_refreshes_total: u64,
    // --- The TWO caches (owner directive 2026-06-12 + ADR-0019): hit-rate = hits / (hits + misses);
    // `*_bytes` = occupancy. `flash` = the in-memory cell (the decoded-geometry LRU). `disk` = the disk
    // cell's MEMBRANE aggregate over its two internal tiers (raw bytes + persisted decoded geometry,
    // `TileDiskCache`): a disk hit is ANY tier serving (geometry restore or cached bytes), a disk miss is
    // a request that ultimately needed the network. Per-tier detail stays inside the cell (`GeoStats`).
    pub flash_hits: u64,
    pub flash_misses: u64,
    pub flash_bytes: u64,
    pub disk_hits: u64,
    pub disk_misses: u64,
    pub disk_bytes: u64,
    // --- The selection X-ray (snapshot of the last full step's hold-reason counts; see
    // `logi_3dtiles_select::HoldCounts`). A scar = `hold_leaf_under_sse` / `fetch_skip_grafted` sustained
    // at quiescence.
    pub hold_kick: u64,
    pub hold_dead: u64,
    pub hold_loading: u64,
    pub hold_leaf_under_sse: u64,
    pub hold_pending_subtree: u64,
    pub fetch_skip_grafted: u64,
}

/// Cross-thread state between the game thread (produces views, consumes frames) and the stream thread
/// (consumes views, produces frames). Every field is either a brief-critical-section `Mutex` or a lock-free
/// atomic — never the world, which the stream thread owns exclusively.
struct StreamShared {
    /// Newest desired view (newest-wins). `None` until the first `set_view`.
    view: Mutex<Option<ViewParams>>,
    /// The latest published render frame (latest-wins mailbox), shared as an `Arc`: both the stream thread's
    /// store and the game thread's poll are O(1) refcount bumps — the lock is held for pointer-sized work, not
    /// an O(R) clone (audit 2026-06-10 §2). Geometry stays alive for whoever holds a clone.
    latest: Mutex<Option<StdArc<GeoRenderFrame>>>,
    /// Lifecycle: cleared to ask the stream thread to finish the current iteration and exit.
    running: AtomicBool,
    /// Frames the stream thread published into the mailbox — the live counter behind
    /// [`GeoTelemetry::produced_frames`] (bumped at mirror time, outside the per-step telemetry store).
    produced: AtomicU64,
    /// The telemetry block, stored whole once per full step (single writer: the stream thread; the lock is
    /// held for a 208-byte copy). The game thread reads it whole via [`GlobeStreamer::telemetry`].
    telemetry: Mutex<GeoTelemetry>,
    /// Tile keys the RENDERER (C++) has fully textured — the producer half of the texture gate that makes LOD
    /// loading invisible. Newest-wins COMPLETE set, pushed by [`set_textured_keys`](GlobeStreamer::set_textured_keys).
    /// The selector treats a tile as renderable (eligible to replace its coarse parent) only when it is
    /// geometry-`Resident` AND in this set, so the kick holds the coarse parent rendered until the fine tile is
    /// textured — never a gray placeholder. Empty until the renderer's first report (a brief startup where only
    /// the coarsest already-textured tiles show).
    textured: Mutex<HashSet<u64>>,
    /// Raised when `textured` changed since the last advance, so the stream thread re-traverses (re-evaluates
    /// the gate → promotes a now-textured tile out of its hidden "prepare" state) even with a still camera and
    /// nothing in flight. Without it, a texture confirmed while the camera is parked would never promote.
    textured_dirty: AtomicBool,
    /// Steps whose body panicked and was caught (the loop continues; see `run_stream`). A non-zero value is
    /// the "streaming hiccuped" signal: without the catch a single bad tile would kill the thread while
    /// `running` stayed true, and the renderer would poll `Same` forever — a silent permanent freeze.
    step_panics: AtomicU64,
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
            telemetry: Mutex::new(GeoTelemetry::default()),
            textured: Mutex::new(HashSet::new()),
            textured_dirty: AtomicBool::new(false),
            step_panics: AtomicU64::new(0),
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

    /// Report the COMPLETE set of tile keys the renderer has fully textured (newest-wins; producer half, like
    /// [`set_view`](Self::set_view)). The selector's texture gate keeps a tile's coarse parent rendered until the
    /// tile appears here, so LOD refinement never flashes an untextured (gray) tile. Wakes the stream thread so a
    /// just-textured tile promotes promptly even if the camera is still. Cheap: collects the slice into the
    /// shared set under a brief lock.
    pub fn set_textured_keys(&self, keys: &[u64]) {
        *lock(&self.shared.textured) = keys.iter().copied().collect();
        self.shared.textured_dirty.store(true, Ordering::Release);
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
                Some(f) if f.frame != self.last_polled_id => StreamPoll::New(StdArc::clone(f)),
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

    /// The streaming lane's telemetry, read WHOLE (one brief lock + a 208-byte copy): the per-step block the
    /// stream thread last stored, with the two live counters (`produced_frames`, `step_panics`) composed in
    /// at read time so they are current even mid-step. This is the single game-thread telemetry surface —
    /// the FFI mirrors the block across the C ABI as `LogiGlobeTelemetry` (ABI v16).
    pub fn telemetry(&self) -> GeoTelemetry {
        let mut t = *lock(&self.shared.telemetry);
        t.produced_frames = self.shared.produced.load(Ordering::Relaxed);
        t.step_panics = self.shared.step_panics.load(Ordering::Relaxed);
        t
    }
    /// Frames the stream thread has published (telemetry; also in [`telemetry`](Self::telemetry)).
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
    let mut last_mirrored: u64 = 0; // mailbox frame id last stored into `shared.latest` (0 = none)
    while shared.running.load(Ordering::Acquire) {
        let view = *lock(&shared.view);
        let view_changed = view != last_view;
        // The renderer reported a new textured-tile set → re-traverse so the gate promotes it (even a still
        // camera). LOAD here (don't clear yet) so a report that lands before any view is set isn't lost; the
        // clear happens inside the work branch, where it's actually consumed.
        let textured_dirty = shared.textured_dirty.load(Ordering::Acquire);
        // Advance when the camera moved, a texture confirmed, fetches are still draining, OR a backoff/
        // session-refresh timer is still pending (audit H4/M1a) — the last term keeps `frame` advancing to the
        // retry deadline even over a parked camera with nothing in flight, or a 429/5xx wave (or an expired
        // session) that drained in-flight to 0 would freeze `frame` and strand the region at coarse LOD. Else
        // idle (no pointless re-traversal of an unchanged, fully-textured scene).
        let work =
            view.is_some() && (view_changed || textured_dirty || world.in_flight() > 0 || world.has_pending_retry());
        match (work, view) {
            (true, Some(v)) => {
                frame += 1;
                // The whole step body is unwind-caught (re-eval 2026-06-09): a panicking step (malformed
                // tile, codec edge, arithmetic corner) must not kill this thread — `running` would stay
                // true and the game thread would poll `Same` forever, a silent permanent freeze of the
                // streaming lane with zero signal to C++. Geo is non-authoritative (ADR-0007), so
                // continuing past a poisoned step is best-effort by design; `step_panics` + the stderr
                // line make it observable. NOTE for the future gameplay `LogiSimRunner` mirror: the
                // deterministic sim must NOT copy this skip-and-continue shape — a panicked fixed step
                // there must hard-stop (replay integrity), not continue.
                let step = catch_unwind(AssertUnwindSafe(|| {
                    // Snapshot the renderer's textured set into the world before traversal (the gate reads
                    // it) — only when it changed, so a still-but-refining scene doesn't pay a per-frame set
                    // clone. The world keeps the last set between updates.
                    if shared.textured_dirty.swap(false, Ordering::Acquire) {
                        world.set_textured_keys(lock(&shared.textured).clone());
                    }
                    // H5: stamp this step's wall-clock — the retry-backoff + session-refresh deadlines read
                    // it, so a backoff is real time, independent of the loop's variable park cadence.
                    world.set_now(Instant::now());
                    // Speculative frame id: the world consumes it only if the re-selection gate (§3.1) runs a
                    // FULL step (returning `true`); a drain-only wake leaves the published frame — and the
                    // counter — untouched, so frame ids keep counting full steps (LRU staleness semantics).
                    let ran_full = world.advance_from_ue(
                        v.camera_ue_cm,
                        v.camera_dir_ue,
                        v.camera_up_ue,
                        v.aspect,
                        v.fov_y_radians,
                        v.viewport_height,
                        v.max_sse,
                        frame + 1,
                    );
                    if ran_full {
                        // Mirror the freshly-published frame into the cross-thread mailbox — an O(1) Arc
                        // clone, and only when its id actually advanced (the C9 fingerprint keeps the id
                        // stable for identical content, so an unchanged publish skips the store + counter).
                        let latest = world.latest_frame();
                        if latest.frame != last_mirrored {
                            last_mirrored = latest.frame;
                            *lock(&shared.latest) = Some(latest);
                            shared.produced.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    ran_full
                }));
                // Consume the view even when the step panicked: re-advancing the identical view would just
                // repeat a deterministic panic at work cadence instead of idling until something changes.
                last_view = Some(v);
                if let Ok(true) = &step {
                    frame += 1; // commit the speculative id — a full step ran and (possibly) published it
                }
                if let Err(_panic) = &step {
                    let n = shared.step_panics.fetch_add(1, Ordering::Relaxed) + 1;
                    // Rate-limit the log: first few, then powers of two — a deterministic per-step panic
                    // must not flood stderr at streaming cadence.
                    if n <= 3 || n.is_power_of_two() {
                        eprintln!("[logi_geo] stream step panicked (total {n}); step skipped, streaming continues");
                    }
                    std::thread::park_timeout(idle_park);
                    continue;
                }
                // Mirror the step's telemetry as ONE block (flash/geom hit+miss from GeoStats, disk hit+miss
                // from the source's DiskCache counters, occupancy bytes from the world). `produced_frames` /
                // `step_panics` are composed from their live atomics at read time, not stored here.
                let stats = world.stats();
                *lock(&shared.telemetry) = GeoTelemetry {
                    resident_tiles: world.resident_tiles() as u64,
                    in_flight: world.in_flight() as u64,
                    produced_frames: 0,
                    step_panics: 0,
                    free_slots: world.free_slots(),
                    max_resident: world.max_resident(),
                    evicted_total: u64::from(stats.evicted_total),
                    pool_exhausted_total: u64::from(stats.pool_exhausted_total),
                    decode_failed_total: u64::from(stats.decode_failed_total),
                    fetch_failed_total: u64::from(stats.fetch_failed_total),
                    graft_failed_total: u64::from(stats.graft_failed_total),
                    url_resolve_failed_total: u64::from(stats.url_resolve_failed_total),
                    fetches_cancelled_total: u64::from(stats.fetches_cancelled_total),
                    session_refreshes_total: u64::from(stats.session_refreshes_total),
                    flash_hits: u64::from(stats.flash_cache_hits_total),
                    flash_misses: u64::from(stats.flash_cache_miss_total),
                    flash_bytes: world.flash_cache().1 as u64,
                    // The disk cell's membrane aggregate: hit = any tier served (a geometry restore that
                    // skipped fetch+decode, or cached bytes that skipped the network); miss = the byte
                    // tier's misses ONLY (a geometry miss that then hit bytes already counted as a hit).
                    disk_hits: u64::from(stats.geom_cache_hits_total) + world.disk_cache_hits(),
                    disk_misses: world.disk_cache_misses(),
                    disk_bytes: world.cached_bytes() + world.geom_cache_bytes(),
                    hold_kick: u64::from(stats.hold_kick),
                    hold_dead: u64::from(stats.hold_dead),
                    hold_loading: u64::from(stats.hold_loading),
                    hold_leaf_under_sse: u64::from(stats.hold_leaf_under_sse),
                    hold_pending_subtree: u64::from(stats.hold_pending_subtree),
                    fetch_skip_grafted: u64::from(stats.fetch_skip_grafted),
                };
                // Brief park to drain arriving tiles while fetches are in flight; otherwise (this advance only
                // serviced a backoff/refresh timer, or the scene settled) the full idle park — so waiting out a
                // retry deadline costs idle-cadence re-traversals, not a 1 ms busy-spin. A `set_view` / texture
                // report unparks immediately regardless (audit H4).
                let park = if world.in_flight() > 0 { SHORT_PARK } else { idle_park };
                std::thread::park_timeout(park);
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
