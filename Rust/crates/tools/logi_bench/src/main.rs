//! `geo-bench` — the Geo streaming test/bench harness CLI (Docs/Testing/Geo-Bench.md).
//!
//! The reproducible loop the hand-driven eyeball sessions lacked: `run` rebuilds the DLLs + editor
//! modules, relaunches the editor on the Logi map, drops a scenario request into `Bench/_inbox/`
//! (the in-editor runner in `LogiBevyEditor` watches it — no MCP, no window focus required), waits
//! for `run_complete.json`, and prints a metrics summary. Every screenshot gets a JSON sidecar with
//! the full streaming-pipeline state (rendered/resident/built/decoding/textured/pending…), cache
//! hit/miss counters, aggregate verts/tris, and frame costs (draw calls, primitives, thread/GPU ms).
//!
//! Runs are foldered untracked under `Bench/<UTC stamp>_<branch>_<sha>[-dirty][_label]/`. `annotate`
//! appends the post-run interpretation paragraph (the context that turns numbers into a record),
//! `list`/`compare` organize and diff runs, `add` files external artifacts (flamegraphs, captures).
//!
//! The default scenario is the standardized three-station descent:
//!   1. `orbit_na_coarse` — high orbit framing North America at the coarsest tile level,
//!   2. `low_orbit`      — ~1,000 km over the US east,
//!   3. `new_castle_de`  — New Castle, Delaware at ~2.5 km, the ground-detail station.
//!
//! Stations are authored geodetically here and resolved to UE render-space centimetres through the
//! kernel's own `logi_geospatial`/`logi_geo_mesh` (the geodesy stays single-homed in Rust).

use logi_geospatial::{Cartographic, Ellipsoid, EnuFrame};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// The streamed-globe actor's ENU origin (lon°, lat°, h m) — MUST match the actor's Origin* properties
/// (currently London, the map's configured hero origin). Override with `--origin lon,lat,h`.
const DEFAULT_ORIGIN: (f64, f64, f64) = (0.1181, 51.5099, 0.0);

const EDITOR_EXE: &str = r"C:\Program Files\Epic Games\UE_5.8\Engine\Binaries\Win64\UnrealEditor.exe";
const RUN_TIMEOUT: Duration = Duration::from_secs(35 * 60); // 8 stations × up to 3 min each + boot + slack

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match args.first().map(String::as_str) {
        Some("run") => cmd_run(&args[1..]),
        Some("fly") => cmd_fly(&args[1..]),
        Some("snap") => cmd_snap(&args[1..]),
        Some("list") => cmd_list(),
        Some("annotate") => cmd_annotate(&args[1..]),
        Some("compare") => cmd_compare(&args[1..]),
        Some("add") => cmd_add(&args[1..]),
        Some("origin-info") => cmd_origin_info(&args[1..]),
        _ => {
            eprintln!(
                "geo-bench — Geo streaming test/bench harness (Docs/Testing/Geo-Bench.md)\n\
                 \n\
                 USAGE:\n\
                 geo-bench run [--label L] [--skip-build] [--no-cache] [--origin lon,lat,h]   rebuild, relaunch, run the station scenario\n\
                 \x20             [--attach] [--stations a,b] [--snaps s1,s2,…]                use the RUNNING editor / station subset / interim-shot override\n\
                 geo-bench fly [--label L] [--skip-build] [--no-cache] [--rate HZ]            rebuild, relaunch, fly the landmark route (PNG+telemetry per frame)\n\
                 geo-bench snap <name>                                          one-shot screenshot+metrics of the CURRENT editor view\n\
                 geo-bench list                                                 runs, newest first, one summary line each\n\
                 geo-bench annotate [run] --note \"...\"                          append interpretation to a run's notes.md (default: latest)\n\
                 geo-bench compare <run-a> <run-b>                              settled-metrics delta table\n\
                 geo-bench add [run] <file>                                     file an external artifact (flamegraph, capture) into a run"
            );
            2
        }
    };
    std::process::exit(code);
}

// ---- paths + small helpers --------------------------------------------------------------------------

/// Project root: the CLI lives at Rust/crates/tools/logi_bench — walk up until LogiUE.uproject.
fn project_root() -> PathBuf {
    let mut dir = std::env::current_dir().expect("cwd");
    loop {
        if dir.join("LogiUE.uproject").exists() {
            return dir;
        }
        if !dir.pop() {
            panic!("LogiUE.uproject not found above the current directory — run from inside the repo");
        }
    }
}

fn bench_dir() -> PathBuf {
    project_root().join("Bench")
}

fn git(args: &[&str]) -> String {
    Command::new("git")
        .args(args)
        .current_dir(project_root())
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

fn utc_stamp() -> String {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
    let (date, h, m) = civil_from_unix(secs);
    format!("{date}_{h:02}{m:02}")
}

/// Unix seconds → (YYYY-MM-DD, hour, minute), UTC. Howard Hinnant's days→civil algorithm (epoch
/// shifted to 0000-03-01 so leap days land at era boundaries) — no chrono dependency.
fn civil_from_unix(secs: u64) -> (String, u64, u64) {
    let (h, m) = ((secs % 86_400) / 3600, (secs % 3600) / 60);
    let z = secs as i64 / 86_400 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let mut y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    if mo <= 2 {
        y += 1;
    }
    (format!("{y:04}-{mo:02}-{d:02}"), h, m)
}

fn read_json(path: &Path) -> Option<Value> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

/// Why [`wait_for_run`] returned — every terminal state is DIAGNOSED, never silently absorbed into a
/// generic timeout (the 2026-06-11 lesson: an editor that died mid-run left external watchers polling a
/// marker that could never appear, for 20+ minutes).
enum RunOutcome {
    Complete,
    /// The spawned editor process exited before writing the completion marker (crash / closed by hand).
    EditorExited(Option<i32>),
    /// The editor is alive but produced NO new artifact for the stall window — wedged, not slow.
    Stalled {
        artifacts: usize,
        idle_s: u64,
    },
    /// The absolute ceiling (an editor alive and trickling artifacts, but past any reasonable run length).
    HardTimeout,
}

/// The CLI's OWN run watcher — the single source of truth on run health, so nothing outside it ever needs
/// to poll. Three signals, in tune with the run's actual shape:
/// 1. **Completion**: the runner's `run_complete.json` marker.
/// 2. **Liveness**: the spawned editor's process handle (`try_wait`) — an editor that dies fails the run
///    in ≤2 s with its exit status, instead of the marker-wait burning the whole timeout.
/// 3. **Progress**: new artifacts in `watch_dir` (top-level sidecars for station runs; `frames/` for fly).
///    A live editor that stops producing for `stall_limit` is WEDGED (the runner max station dwell is
///    bounded, and a fly emits ~10 sidecars/s) — fail fast with how long it sat idle. New station shots
///    are echoed as they land (a fly prints a count line instead — ~2,900 names is noise).
fn wait_for_run(
    watch_dir: &Path,
    marker: &Path,
    mut editor: Option<std::process::Child>, // None = --attach (no handle); stall still bounds a dead editor
    hard_timeout: Duration,
    stall_limit: Duration,
    echo_names: bool,
) -> RunOutcome {
    let started = Instant::now();
    let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut last_progress = Instant::now();
    let mut last_count_echo = Instant::now();
    loop {
        if marker.exists() {
            return RunOutcome::Complete;
        }
        // Liveness: the editor exiting without the marker is a FAILED run, known within one poll.
        if let Some(ed) = editor.as_mut() {
            if let Ok(Some(status)) = ed.try_wait() {
                // One more marker check — the editor may have written it in the instant before exiting.
                if marker.exists() {
                    return RunOutcome::Complete;
                }
                return RunOutcome::EditorExited(status.code());
            }
        }
        // Progress: any new sidecar resets the stall clock.
        let mut new_any = false;
        if let Ok(entries) = std::fs::read_dir(watch_dir) {
            for e in entries.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                if name.ends_with(".json") && seen.insert(name.clone()) {
                    new_any = true;
                    if echo_names {
                        println!("[geo-bench]   {name}");
                    }
                }
            }
        }
        if new_any {
            last_progress = Instant::now();
            if !echo_names && last_count_echo.elapsed() > Duration::from_secs(30) {
                println!("[geo-bench]   {} frames…", seen.len());
                last_count_echo = Instant::now();
            }
        } else if last_progress.elapsed() > stall_limit {
            return RunOutcome::Stalled {
                artifacts: seen.len(),
                idle_s: last_progress.elapsed().as_secs(),
            };
        }
        if started.elapsed() > hard_timeout {
            return RunOutcome::HardTimeout;
        }
        std::thread::sleep(Duration::from_secs(2));
    }
}

/// Map a [`RunOutcome`] to the exit code + the user-facing diagnosis. `0` only for a completed run.
fn report_outcome(outcome: RunOutcome, run_dir: &Path) -> i32 {
    match outcome {
        RunOutcome::Complete => 0,
        RunOutcome::EditorExited(code) => {
            eprintln!(
                "[geo-bench] FAILED: the editor exited ({}) before completing the run — partial artifacts in {}",
                code.map_or("killed".into(), |c| format!("code {c}")),
                run_dir.display()
            );
            3
        }
        RunOutcome::Stalled { artifacts, idle_s } => {
            eprintln!(
                "[geo-bench] FAILED: run STALLED — editor alive but no new artifact for {idle_s}s \
                 ({artifacts} captured) — partial artifacts in {}",
                run_dir.display()
            );
            4
        }
        RunOutcome::HardTimeout => {
            eprintln!(
                "[geo-bench] FAILED: hard timeout — partial artifacts in {}",
                run_dir.display()
            );
            1
        }
    }
}

/// Print the render-frame anchors the planet-true SKY STACK needs (ADR task 2026-06-11): the earth center
/// (ECEF origin) in UE render-space cm under the given ENU origin, and the origin's geocentric surface
/// radius — the numbers a `PlanetCenterAtComponentTransform` SkyAtmosphere + cloud layer must be anchored
/// with. Single-homed in the same kernel geodesy every station/flyby request uses.
fn cmd_origin_info(args: &[String]) -> i32 {
    let mut origin = DEFAULT_ORIGIN;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--origin" {
            let v = it.next().cloned().unwrap_or_default();
            let p: Vec<f64> = v.split(',').filter_map(|s| s.trim().parse().ok()).collect();
            if p.len() != 3 {
                eprintln!("--origin expects lon,lat,h");
                return 2;
            }
            origin = (p[0], p[1], p[2]);
        }
    }
    let ellipsoid = Ellipsoid::wgs84();
    let enu = EnuFrame::from_cartographic(&ellipsoid, Cartographic::from_degrees(origin.0, origin.1, origin.2));
    // The ECEF origin as "the surface point scaled to radius 0" — avoids a direct glam dependency here.
    let ecef_zero = ellipsoid.cartographic_to_cartesian(Cartographic::from_degrees(0.0, 0.0, 0.0)) * 0.0;
    let ec = logi_geo_mesh::enu_pos_to_ue(enu.ecef_to_local(ecef_zero));
    let r_cm = (f64::from(ec[0]).powi(2) + f64::from(ec[1]).powi(2) + f64::from(ec[2]).powi(2)).sqrt();
    println!("origin lon,lat,h        : {}, {}, {}", origin.0, origin.1, origin.2);
    println!("earth center (UE cm)    : [{:.1}, {:.1}, {:.1}]", ec[0], ec[1], ec[2]);
    println!("geocentric radius (km)  : {:.4}", r_cm / 100_000.0);
    0
}

/// Geodetic (lon°, lat°, h m) → UE render-space cm under the actor's ENU origin.
fn geo_to_ue(enu: &EnuFrame, ellipsoid: &Ellipsoid, lon: f64, lat: f64, h: f64) -> [f64; 3] {
    let ecef = ellipsoid.cartographic_to_cartesian(Cartographic::from_degrees(lon, lat, h));
    let p = logi_geo_mesh::enu_pos_to_ue(enu.ecef_to_local(ecef));
    [p[0] as f64, p[1] as f64, p[2] as f64]
}

/// Local geodetic UP at (lon°, lat°) as a UE render-space direction. The render frame is one ENU plane
/// pinned at the actor's origin (London), so away from the origin "up" is NOT +Z — a camera oriented with
/// world roll = 0 appears rolled against the local horizon (the tilted-limb shots of the first 8-station
/// run). The runner uses this as the look-at's up axis.
fn geo_up_ue(enu: &EnuFrame, ellipsoid: &Ellipsoid, lon: f64, lat: f64, h: f64) -> [f64; 3] {
    let a = geo_to_ue(enu, ellipsoid, lon, lat, h);
    let b = geo_to_ue(enu, ellipsoid, lon, lat, h + 1_000.0);
    let d = [b[0] - a[0], b[1] - a[1], b[2] - a[2]];
    let len = (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt().max(1e-9);
    [d[0] / len, d[1] / len, d[2] / len]
}

// ---- run ---------------------------------------------------------------------------------------------

fn cmd_run(args: &[String]) -> i32 {
    let mut label = String::new();
    let mut skip_build = false;
    let mut disk_cache = true; // default: CACHED (the steady-state development loop)
    let mut origin = DEFAULT_ORIGIN;
    let mut attach = false;
    let mut station_filter: Option<Vec<String>> = None;
    let mut snaps_override: Option<Vec<f64>> = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--label" => label = it.next().cloned().unwrap_or_default(),
            "--skip-build" => skip_build = true,
            "--no-cache" => disk_cache = false,
            // Attach to the ALREADY-RUNNING editor: no rebuild, no relaunch — the focused-iteration loop
            // (one station, current binaries). The watcher loses the process-liveness signal (no child
            // handle); the stall limit still catches a dead/wedged editor, just slower.
            "--attach" => attach = true,
            // Station subset by exact name (comma-separated) — bisect ONE station without the full scenario.
            "--stations" => {
                station_filter = Some(
                    it.next()
                        .cloned()
                        .unwrap_or_default()
                        .split(',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect(),
                );
            }
            // Interim-shot offsets (seconds) for every selected station — a dense time-lapse around settle.
            "--snaps" => {
                snaps_override = Some(
                    it.next()
                        .cloned()
                        .unwrap_or_default()
                        .split(',')
                        .filter_map(|s| s.trim().parse().ok())
                        .collect(),
                );
            }
            "--origin" => {
                let v = it.next().cloned().unwrap_or_default();
                let p: Vec<f64> = v.split(',').filter_map(|s| s.trim().parse().ok()).collect();
                if p.len() != 3 {
                    eprintln!("--origin expects lon,lat,h");
                    return 2;
                }
                origin = (p[0], p[1], p[2]);
            }
            other => {
                eprintln!("unknown flag {other}");
                return 2;
            }
        }
    }

    let root = project_root();
    let branch = git(&["rev-parse", "--abbrev-ref", "HEAD"]);
    let sha = git(&["rev-parse", "--short", "HEAD"]);
    let dirty = !git(&["status", "--porcelain"]).is_empty();

    // Self-describing run folders: a cold run is a different experiment — say so in the name.
    if !disk_cache {
        label = if label.is_empty() {
            "nocache".into()
        } else {
            format!("{label}-nocache")
        };
    }
    // Run folder: date + git state (+ label) — self-describing and sortable.
    let mut run_name = format!(
        "{}_{}_{}{}",
        utc_stamp(),
        if branch.is_empty() { "nobranch" } else { &branch },
        if sha.is_empty() { "nosha" } else { &sha },
        if dirty { "-dirty" } else { "" }
    );
    if !label.is_empty() {
        run_name.push('_');
        run_name.push_str(&label);
    }
    let run_dir = bench_dir().join(&run_name);
    std::fs::create_dir_all(run_dir.join("artifacts")).expect("create run dir");

    // 1) Rebuild (closes the editor) unless told otherwise — the same path every manual session used.
    if !skip_build && !attach {
        println!("[geo-bench] rebuilding (rebuild-editor.ps1 -NoLaunch)…");
        let ok = Command::new("powershell")
            .args(["-ExecutionPolicy", "Bypass", "-File", "rebuild-editor.ps1", "-NoLaunch"])
            .current_dir(&root)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            eprintln!(
                "[geo-bench] rebuild FAILED — aborting (run dir kept: {})",
                run_dir.display()
            );
            return 1;
        }
    }

    // 2) Scenario: stations resolved to UE cm via the kernel's geodesy.
    let ellipsoid = Ellipsoid::wgs84();
    let enu = EnuFrame::from_cartographic(&ellipsoid, Cartographic::from_degrees(origin.0, origin.1, origin.2));
    let station = |name: &str, cam: (f64, f64, f64), look: (f64, f64, f64), snaps: &[f64], timeout: f64| {
        json!({
            "name": name,
            "cam": geo_to_ue(&enu, &ellipsoid, cam.0, cam.1, cam.2),
            "look": geo_to_ue(&enu, &ellipsoid, look.0, look.1, look.2),
            // The camera's LOCAL geodetic up (render space): the runner builds the view rotation from
            // forward+up, keeping the horizon level at stations far from the ENU origin.
            "up": geo_up_ue(&enu, &ellipsoid, cam.0, cam.1, cam.2),
            // --snaps replaces the default interim offsets on EVERY selected station (time-lapse mode).
            "snapshots_s": snaps_override.as_deref().unwrap_or(snaps),
            "settle_timeout_s": timeout,
        })
    };
    let all_stations = vec![
        // 1. North America from high orbit — the COARSEST level the selector should settle at.
        station(
            "orbit_na_coarse",
            (-100.0, 42.0, 18_000_000.0),
            (-100.0, 42.0, 0.0),
            &[10.0, 30.0],
            150.0,
        ),
        // 2. Low orbit over the US east coast.
        station(
            "low_orbit",
            (-87.0, 41.0, 1_000_000.0),
            (-87.0, 41.0, 0.0),
            &[10.0, 30.0],
            150.0,
        ),
        // 3. New Castle, Delaware — the ground-detail station (camera S of town, looking N-down).
        station(
            "new_castle_de",
            (-75.566, 39.625, 2_500.0),
            (-75.566, 39.662, 0.0),
            &[10.0, 30.0],
            180.0,
        ),
        // 4–8. Five FIXED global ground stations (varied terrain / hemisphere / longitude) so every run
        // exercises cold-region teleports around the planet. Fixed — not re-randomized per run — so any
        // two runs' settled sidecars stay directly comparable (the harness's core property).
        // Tokyo Bay — dense megacity coastline.
        station(
            "tokyo_bay",
            (139.74, 35.60, 3_000.0),
            (139.74, 35.66, 0.0),
            &[10.0, 30.0],
            180.0,
        ),
        // Grand Canyon — extreme vertical relief.
        station(
            "grand_canyon",
            (-112.11, 36.02, 4_000.0),
            (-112.11, 36.10, 0.0),
            &[10.0, 30.0],
            180.0,
        ),
        // Mont Blanc — high-alpine peaks (camera above the massif, looking at the summit ridge).
        station(
            "mont_blanc",
            (6.86, 45.76, 7_000.0),
            (6.86, 45.83, 3_500.0),
            &[10.0, 30.0],
            180.0,
        ),
        // Giza — low-contrast desert + monuments (texture-quality torture test).
        station(
            "giza",
            (31.13, 29.92, 2_500.0),
            (31.13, 29.98, 0.0),
            &[10.0, 30.0],
            180.0,
        ),
        // Rio de Janeiro — southern hemisphere, steep coastal relief.
        station(
            "rio",
            (-43.21, -23.01, 3_000.0),
            (-43.21, -22.95, 0.0),
            &[10.0, 30.0],
            180.0,
        ),
    ];
    let stations: Vec<Value> = match &station_filter {
        Some(names) => {
            let picked: Vec<Value> = all_stations
                .into_iter()
                .filter(|s| names.iter().any(|n| s["name"] == n.as_str()))
                .collect();
            if picked.len() != names.len() {
                eprintln!(
                    "--stations: unknown name(s) — valid: orbit_na_coarse, low_orbit, new_castle_de, \
                     tokyo_bay, grand_canyon, mont_blanc, giza, rio"
                );
                return 2;
            }
            picked
        }
        None => all_stations,
    };
    let request = json!({
        "cmd": "run",
        "out_dir": run_dir.to_string_lossy(),
        "label": label,
        "git": { "branch": branch, "sha": sha, "dirty": dirty },
        "origin": { "lon": origin.0, "lat": origin.1, "h": origin.2 },
        // Disk-cache mode: the runner flips the actor's Persistent Disk Cache to this (rebuilding the Rust
        // world) and restores the editor's prior setting when the run ends. `--no-cache` = a true
        // cold-network run (no byte cache AND no persisted-geometry tier — both live under cache_dir).
        "disk_cache": disk_cache,
        "stations": stations,
    });

    // 3) Launch the editor (the throttle-off ini is project config; the runner also forces realtime).
    // A force-closed previous session leaves PackageRestoreData.json behind, and the editor then blocks
    // STARTUP on a modal "restore packages?" prompt (AddModalWindow inside EditorInit) that no one is at
    // the desktop to click — the run would sit at frame 0 until timeout. The rebuild step auto-saves all
    // dirty packages before closing, so for a bench launch there is never anything to restore: clear it.
    let restore = root.join("Saved/Autosaves/PackageRestoreData.json");
    if restore.exists() {
        match std::fs::remove_file(&restore) {
            Ok(()) => println!("[geo-bench] cleared stale package-restore prompt (would block startup)"),
            Err(e) => eprintln!("[geo-bench] warning: couldn't clear {}: {e}", restore.display()),
        }
    }
    let editor = if attach {
        // --attach: the running editor's idle runner picks the request off the inbox ticker. No child
        // handle → the watcher's liveness signal is off; the stall limit still bounds a dead editor.
        println!("[geo-bench] attaching to the running editor…");
        None
    } else {
        println!("[geo-bench] launching editor…");
        let uproject = root.join("LogiUE.uproject");
        match Command::new(EDITOR_EXE)
            .arg(&uproject)
            .arg("/Game/Logi")
            .current_dir(&root)
            .spawn()
        {
            Ok(child) => Some(child),
            Err(_) => {
                eprintln!("[geo-bench] failed to launch {EDITOR_EXE}");
                return 1;
            }
        }
    };

    // 4) Drop the request in the inbox; the in-editor runner picks it up once the geo actor exists.
    let inbox = bench_dir().join("_inbox");
    std::fs::create_dir_all(&inbox).expect("create inbox");
    std::fs::write(
        inbox.join(format!("{run_name}.json")),
        serde_json::to_string_pretty(&request).unwrap(),
    )
    .expect("write request");

    // 5) Watch the run (the CLI is the single source of truth — completion, editor liveness, artifact
    // progress). Stall limit: the longest legal silent gap is one station's settle timeout (180 s) +
    // editor boot before station 0; 240 s flags a wedge without false-failing a slow settle.
    println!("[geo-bench] watching (≤{}s, stall ≤240s)…", RUN_TIMEOUT.as_secs());
    let marker = run_dir.join("run_complete.json");
    let outcome = wait_for_run(&run_dir, &marker, editor, RUN_TIMEOUT, Duration::from_secs(240), true);
    let code = report_outcome(outcome, &run_dir);
    if code != 0 {
        return code;
    }
    println!("[geo-bench] run complete → {}\n", run_dir.display());
    print_run_summary(&run_dir);
    println!("\n[geo-bench] annotate it:  geo-bench annotate --note \"…\"");
    0
}

// ---- fly (the landmark flyby / "video" path) ---------------------------------------------------------

/// One flyby waypoint, authored geodetically: where the camera ARRIVES (and what it looks at), how long
/// the eased travel from the previous waypoint takes, and how long it dwells there (converging on camera).
struct Waypoint {
    name: &'static str,
    cam: (f64, f64, f64),  // lon°, lat°, height m
    look: (f64, f64, f64), // lon°, lat°, height m
    travel_s: f64,
    dwell_s: f64,
}

/// The landmark route: Golden Gate → orbit → Statue of Liberty → orbit → White House → orbit → each of
/// the New7Wonders + the Great Pyramid, zipping to orbit between every one. Orbit hops sit directly above
/// the NEXT target (the geodetic-space easing arcs the camera up, across, and back down — interpolating
/// lon/lat/h NEVER cuts a chord through the planet the way cartesian lerp between antipodes would).
fn flyby_route() -> Vec<Waypoint> {
    const ORBIT_H: f64 = 1_200_000.0;
    let mut wp: Vec<Waypoint> = Vec::new();
    let mut orbit_then = |name: &'static str, cam: (f64, f64, f64), look: (f64, f64, f64)| {
        wp.push(Waypoint {
            name: "orbit",
            cam: (look.0, look.1, ORBIT_H),
            look,
            travel_s: 9.0,
            dwell_s: 3.0,
        });
        wp.push(Waypoint {
            name,
            cam,
            look,
            travel_s: 8.0,
            dwell_s: 6.0,
        });
    };
    // Start: orbit over San Francisco, then the Golden Gate Bridge.
    orbit_then("golden_gate", (-122.4830, 37.8050, 700.0), (-122.4783, 37.8199, 80.0));
    orbit_then(
        "statue_of_liberty",
        (-74.0470, 40.6800, 450.0),
        (-74.0445, 40.6892, 93.0),
    );
    orbit_then("white_house", (-77.0365, 38.8860, 650.0), (-77.0365, 38.8977, 25.0));
    // The New7Wonders (+ Giza, the honorary ancient wonder).
    orbit_then("great_wall", (116.5640, 40.4250, 1_050.0), (116.5704, 40.4319, 620.0));
    orbit_then("petra", (35.4520, 30.3220, 1_350.0), (35.4444, 30.3285, 850.0));
    orbit_then(
        "christ_redeemer",
        (-43.2180, -22.9600, 1_050.0),
        (-43.2105, -22.9519, 715.0),
    );
    orbit_then(
        "machu_picchu",
        (-72.5530, -13.1710, 2_950.0),
        (-72.5450, -13.1631, 2_440.0),
    );
    orbit_then("chichen_itza", (-88.5740, 20.6760, 480.0), (-88.5678, 20.6843, 35.0));
    orbit_then("colosseum", (12.4990, 41.8830, 500.0), (12.4922, 41.8902, 55.0));
    orbit_then("taj_mahal", (78.0480, 27.1680, 560.0), (78.0421, 27.1751, 175.0));
    orbit_then("giza_pyramids", (31.1410, 29.9710, 520.0), (31.1342, 29.9792, 80.0));
    wp
}

/// Geodetic lerp: lon takes the shortest wrap; lat/h are plain lerps.
fn geo_lerp(a: (f64, f64, f64), b: (f64, f64, f64), s: f64) -> (f64, f64, f64) {
    let dlon = (b.0 - a.0 + 540.0).rem_euclid(360.0) - 180.0;
    (a.0 + dlon * s, a.1 + (b.1 - a.1) * s, a.2 + (b.2 - a.2) * s)
}

fn cmd_fly(args: &[String]) -> i32 {
    let mut label = String::new();
    let mut skip_build = false;
    let mut disk_cache = true; // default: CACHED (matches `run`)
    let mut rate_hz: f64 = 10.0;
    let mut origin = DEFAULT_ORIGIN;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--label" => label = it.next().cloned().unwrap_or_default(),
            "--skip-build" => skip_build = true,
            "--no-cache" => disk_cache = false,
            "--rate" => {
                rate_hz = it.next().and_then(|v| v.parse().ok()).unwrap_or(10.0);
                if !(0.1..=60.0).contains(&rate_hz) {
                    eprintln!("--rate must be 0.1..=60 Hz");
                    return 2;
                }
            }
            "--origin" => {
                let v = it.next().cloned().unwrap_or_default();
                let p: Vec<f64> = v.split(',').filter_map(|s| s.trim().parse().ok()).collect();
                if p.len() != 3 {
                    eprintln!("--origin expects lon,lat,h");
                    return 2;
                }
                origin = (p[0], p[1], p[2]);
            }
            other => {
                eprintln!("unknown flag {other}");
                return 2;
            }
        }
    }

    let root = project_root();
    let branch = git(&["rev-parse", "--abbrev-ref", "HEAD"]);
    let sha = git(&["rev-parse", "--short", "HEAD"]);
    let dirty = !git(&["status", "--porcelain"]).is_empty();
    // Self-describing run folders: a cold run is a different experiment — say so in the name.
    if !disk_cache {
        label = if label.is_empty() {
            "nocache".into()
        } else {
            format!("{label}-nocache")
        };
    }
    let mut run_name = format!(
        "{}_{}_{}{}_fly",
        utc_stamp(),
        if branch.is_empty() { "nobranch" } else { &branch },
        if sha.is_empty() { "nosha" } else { &sha },
        if dirty { "-dirty" } else { "" }
    );
    if !label.is_empty() {
        run_name.push('_');
        run_name.push_str(&label);
    }
    let run_dir = bench_dir().join(&run_name);
    std::fs::create_dir_all(run_dir.join("frames")).expect("create run dir");

    if !skip_build {
        println!("[geo-bench] rebuilding (rebuild-editor.ps1 -NoLaunch)…");
        let ok = Command::new("powershell")
            .args(["-ExecutionPolicy", "Bypass", "-File", "rebuild-editor.ps1", "-NoLaunch"])
            .current_dir(&root)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            eprintln!(
                "[geo-bench] rebuild FAILED — aborting (run dir kept: {})",
                run_dir.display()
            );
            return 1;
        }
    }

    // Generate the dense camera TRACK: the whole timeline sampled at the capture rate, all geodesy here
    // (single-homed in Rust — the runner just plays UE-space samples and never does math on the globe).
    // Travel eases with smoothstep in GEODETIC space; dwells hold the waypoint (the captures during a dwell
    // record convergence). Each sample carries its own local-up so the horizon stays level worldwide.
    let ellipsoid = Ellipsoid::wgs84();
    let enu = EnuFrame::from_cartographic(&ellipsoid, Cartographic::from_degrees(origin.0, origin.1, origin.2));
    let route = flyby_route();
    let dt = 1.0 / rate_hz;
    let mut samples: Vec<Value> = Vec::new();
    let mut t = 0.0_f64;
    let mut prev_cam = route[0].cam; // leg 0 starts AT its destination (travel time still eases the look)
    let mut prev_look = route[0].look;
    for w in &route {
        let travel = w.travel_s.max(dt);
        let mut leg_t = 0.0;
        while leg_t < travel + w.dwell_s {
            let s = (leg_t / travel).min(1.0);
            let e = s * s * (3.0 - 2.0 * s); // smoothstep ease in/out
            let cam = geo_lerp(prev_cam, w.cam, e);
            let look = geo_lerp(prev_look, w.look, e);
            samples.push(json!({
                "t": (t * 1000.0).round() / 1000.0,
                "cam": geo_to_ue(&enu, &ellipsoid, cam.0, cam.1, cam.2),
                "look": geo_to_ue(&enu, &ellipsoid, look.0, look.1, look.2),
                "up": geo_up_ue(&enu, &ellipsoid, cam.0, cam.1, cam.2),
                "leg": w.name,
            }));
            leg_t += dt;
            t += dt;
        }
        prev_cam = w.cam;
        prev_look = w.look;
    }
    let total_s = t;
    println!(
        "[geo-bench] track: {} samples over {:.0}s at {:.1} Hz (~{:.1} GB of PNGs at viewport res)",
        samples.len(),
        total_s,
        rate_hz,
        samples.len() as f64 * 1.8 / 1024.0
    );

    let request = json!({
        "cmd": "fly",
        "out_dir": run_dir.to_string_lossy(),
        "label": label,
        "git": { "branch": branch, "sha": sha, "dirty": dirty },
        "origin": { "lon": origin.0, "lat": origin.1, "h": origin.2 },
        // See `run`: the runner flips the actor's Persistent Disk Cache to this and restores it after.
        "disk_cache": disk_cache,
        "rate_hz": rate_hz,
        "samples": samples,
    });

    let restore = root.join("Saved/Autosaves/PackageRestoreData.json");
    if restore.exists() && std::fs::remove_file(&restore).is_ok() {
        println!("[geo-bench] cleared stale package-restore prompt (would block startup)");
    }
    println!("[geo-bench] launching editor…");
    let uproject = root.join("LogiUE.uproject");
    let editor = match Command::new(EDITOR_EXE)
        .arg(&uproject)
        .arg("/Game/Logi")
        .current_dir(&root)
        .spawn()
    {
        Ok(child) => child,
        Err(_) => {
            eprintln!("[geo-bench] failed to launch {EDITOR_EXE}");
            return 1;
        }
    };
    let inbox = bench_dir().join("_inbox");
    std::fs::create_dir_all(&inbox).expect("create inbox");
    std::fs::write(
        inbox.join(format!("{run_name}.json")),
        serde_json::to_string(&request).unwrap(),
    )
    .expect("write request");

    // Watch the flight (the CLI is the single source of truth — completion, editor liveness, frame
    // progress in `frames/`). Hard ceiling = boot + 2× the flight; stall = 90 s (the runner emits ~10
    // sidecars/s once flying, and editor boot itself stays under this — anything quieter is wedged).
    let timeout = Duration::from_secs(240 + total_s as u64 * 2);
    println!("[geo-bench] watching (≤{}s, stall ≤90s)…", timeout.as_secs());
    let marker = run_dir.join("run_complete.json");
    let outcome = wait_for_run(
        &run_dir.join("frames"),
        &marker,
        Some(editor),
        timeout,
        Duration::from_secs(90),
        false,
    );
    let code = report_outcome(outcome, &run_dir);
    if code != 0 {
        return code;
    }
    if let Some(done) = read_json(&marker) {
        println!(
            "[geo-bench] fly complete → {}  (frames={} dropped={})",
            run_dir.display(),
            done.get("frames").and_then(Value::as_f64).unwrap_or(0.0),
            done.get("frames_dropped").and_then(Value::as_f64).unwrap_or(0.0)
        );
    }
    println!("[geo-bench] annotate it:  geo-bench annotate {run_name} --note \"…\"");
    0
}

// ---- snap / list / annotate / compare / add ----------------------------------------------------------

fn cmd_snap(args: &[String]) -> i32 {
    let name = args.first().cloned().unwrap_or_else(|| "adhoc".into());
    let out = bench_dir().join("_interactive").join(utc_stamp());
    std::fs::create_dir_all(&out).expect("create out dir");
    let inbox = bench_dir().join("_inbox");
    std::fs::create_dir_all(&inbox).expect("create inbox");
    let request = json!({ "cmd": "snap", "name": name, "out_dir": out.to_string_lossy() });
    std::fs::write(inbox.join(format!("snap_{name}.json")), request.to_string()).expect("write request");
    println!(
        "[geo-bench] snap requested → {} (editor must be running)",
        out.display()
    );
    0
}

fn run_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(bench_dir())
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir() && !p.file_name().is_some_and(|n| n.to_string_lossy().starts_with('_')))
        .collect();
    dirs.sort();
    dirs.reverse(); // newest (stamp-prefixed) first
    dirs
}

fn sidecars(run: &Path) -> Vec<(String, Value)> {
    let mut out: Vec<(String, Value)> = std::fs::read_dir(run)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .filter(|p| p.file_name().is_some_and(|n| n != "run_complete.json"))
        .filter_map(|p| Some((p.file_stem()?.to_string_lossy().into_owned(), read_json(&p)?)))
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

fn print_run_summary(run: &Path) {
    println!(
        "{:<28} {:>5} {:>6} {:>6} {:>5} {:>5} {:>6} {:>8} {:>9} {:>8} {:>6}",
        "shot", "rend", "built", "texd", "infl", "dec", "draw", "prims", "vis_tris", "settle", "gpu"
    );
    for (name, m) in sidecars(run) {
        let g = |k: &str| m.get(k).and_then(Value::as_f64).unwrap_or(0.0);
        println!(
            "{:<28} {:>5} {:>6} {:>6} {:>5} {:>5} {:>6} {:>7.1}K {:>8.1}K {:>7.1}s {:>5.1}ms",
            name,
            g("rendered"),
            g("built"),
            g("textured"),
            g("in_flight"),
            g("decoding"),
            g("draw_calls"),
            g("primitives_drawn") / 1.0e3,
            g("visible_tris") / 1.0e3,
            g("station_elapsed_s"),
            g("gpu_ms"),
        );
    }
}

fn cmd_list() -> i32 {
    for run in run_dirs() {
        let shots = sidecars(&run).len();
        let done = run.join("run_complete.json").exists();
        let noted = run.join("notes.md").exists();
        println!(
            "{}  shots={:<3} {}{}",
            run.file_name().unwrap_or_default().to_string_lossy(),
            shots,
            if done { "complete" } else { "PARTIAL" },
            if noted { " +notes" } else { "" }
        );
    }
    0
}

fn resolve_run(arg: Option<&String>) -> Option<PathBuf> {
    match arg {
        Some(name) => {
            let p = bench_dir().join(name);
            p.is_dir().then_some(p)
        }
        None => run_dirs().into_iter().next(),
    }
}

fn cmd_annotate(args: &[String]) -> i32 {
    let mut note = String::new();
    let mut run_arg: Option<String> = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--note" => note = it.next().cloned().unwrap_or_default(),
            other => run_arg = Some(other.to_string()),
        }
    }
    if note.is_empty() {
        eprintln!("annotate requires --note \"…\"");
        return 2;
    }
    let Some(run) = resolve_run(run_arg.as_ref()) else {
        eprintln!("no runs found");
        return 1;
    };
    let notes = run.join("notes.md");
    let mut body = std::fs::read_to_string(&notes)
        .unwrap_or_else(|_| format!("# {} — notes\n", run.file_name().unwrap_or_default().to_string_lossy()));
    body.push_str(&format!("\n## {}\n{}\n", utc_stamp(), note));
    std::fs::write(&notes, body).expect("write notes");
    println!("[geo-bench] noted → {}", notes.display());
    0
}

fn cmd_compare(args: &[String]) -> i32 {
    let (Some(a), Some(b)) = (args.first(), args.get(1)) else {
        eprintln!("compare <run-a> <run-b>");
        return 2;
    };
    let (Some(ra), Some(rb)) = (resolve_run(Some(a)), resolve_run(Some(b))) else {
        eprintln!("run dir(s) not found under Bench/");
        return 1;
    };
    // Compare the SETTLED (or timeout) shot of stations both runs share.
    let pick = |run: &Path| -> Vec<(String, Value)> {
        sidecars(run)
            .into_iter()
            .filter(|(n, _)| n.contains("settled") || n.contains("timeout"))
            .collect()
    };
    let keys = [
        "rendered",
        "built",
        "textured",
        "visible_tris",
        "draw_calls",
        "gpu_ms",
        "render_thread_ms",
        "station_elapsed_s",
        "flash_hits",
        "disk_hits",
    ];
    for (name_a, ma) in pick(&ra) {
        let station = ma.get("station").and_then(Value::as_str).unwrap_or(&name_a).to_string();
        if let Some((_, mb)) = pick(&rb)
            .into_iter()
            .find(|(_, m)| m.get("station").and_then(Value::as_str) == Some(station.as_str()))
        {
            println!("== {station} ==");
            for k in keys {
                let (va, vb) = (
                    ma.get(k).and_then(Value::as_f64).unwrap_or(0.0),
                    mb.get(k).and_then(Value::as_f64).unwrap_or(0.0),
                );
                println!("  {:<20} {:>14.1} -> {:>14.1}  ({:+.1})", k, va, vb, vb - va);
            }
        }
    }
    0
}

fn cmd_add(args: &[String]) -> i32 {
    let (run_arg, file) = match args {
        [f] => (None, f),
        [r, f] => (Some(r.clone()), f),
        _ => {
            eprintln!("add [run] <file>");
            return 2;
        }
    };
    let Some(run) = resolve_run(run_arg.as_ref()) else {
        eprintln!("no runs found");
        return 1;
    };
    let src = PathBuf::from(file);
    let Some(fname) = src.file_name() else {
        eprintln!("bad file path");
        return 2;
    };
    let dst = run.join("artifacts").join(fname);
    std::fs::create_dir_all(run.join("artifacts")).ok();
    match std::fs::copy(&src, &dst) {
        Ok(_) => {
            println!("[geo-bench] filed → {}", dst.display());
            0
        }
        Err(e) => {
            eprintln!("copy failed: {e}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::civil_from_unix;

    #[test]
    fn civil_dates_are_correct() {
        // Independently-known UTC instants (the first run's folder said 2026-08-08 on 2026-06-10 —
        // the original implementation skipped the March-epoch shift; this guards the fix).
        assert_eq!(civil_from_unix(0), ("1970-01-01".into(), 0, 0));
        assert_eq!(civil_from_unix(1_700_000_000), ("2023-11-14".into(), 22, 13));
        assert_eq!(civil_from_unix(951_782_400), ("2000-02-29".into(), 0, 0)); // leap day
        assert_eq!(civil_from_unix(1_781_136_000), ("2026-06-11".into(), 0, 0));
    }
}
