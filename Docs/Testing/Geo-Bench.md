# Geo-Bench — the standardized Geo test/bench harness

A reproducible, focus-independent test loop for the Geo streaming stack. One command rebuilds the
DLLs and editor modules, relaunches the editor on the Logi map, flies the standardized three-station
descent, and captures screenshots **with the full pipeline state attached as JSON metadata** — so a
regression report is a diffable artifact, not an eyeball impression.

## Architecture

Two halves, joined by a **file inbox** (no MCP, no window focus, no sockets):

| Half | Where | Role |
|---|---|---|
| `geo-bench` CLI | `Rust/crates/tools/logi_bench` (binary `geo-bench`) | Orchestrates: rebuild → launch → request → wait → summarize. Owns run foldering, geodesy (stations are authored in lon/lat/height and resolved through `logi_geospatial`/`logi_geo_mesh` — single-homed in Rust), annotation, comparison, artifact filing. |
| `FLogiGeoBenchRunner` | `Plugins/LogiBevy/Source/LogiBevyEditor` (editor-only module) | Watches `Bench/_inbox/*.json` (0.25 s ticker). Per station: sets the level-viewport camera, forces realtime (override + `bThrottleCPUWhenNotForeground=false`), samples `FLogiGeoBenchMetrics` every tick, screenshots at timed offsets and at **settle** (or timeout), writes a JSON sidecar per PNG, ends with `run_complete.json`. |

The contract is the JSON files, so a run can also be driven by hand: drop a request file in
`Bench/_inbox/` while the editor is up.

## Usage (from anywhere inside the repo)

```
cargo run -q -p logi_bench --manifest-path Rust/Cargo.toml --release -- <cmd>

geo-bench run [--label L] [--skip-build] [--no-cache] [--origin lon,lat,h]   # the full standardized run
              [--attach] [--stations a,b] [--snaps s1,s2,…]                  # focused-iteration knobs (below)
geo-bench fly [--label L] [--skip-build] [--no-cache] [--rate HZ]            # the landmark flyby (video path)
geo-bench snap <name>          # one screenshot+metrics of the CURRENT view (editor running)
geo-bench list                 # runs, newest first
geo-bench annotate [run] --note "…"   # append interpretation to notes.md (default: latest run)
geo-bench compare <run-a> <run-b>     # settled-metrics delta table
geo-bench add [run] <file>     # file an external artifact (flamegraph, trace, capture)
```

`run` aborts (run dir kept) if the rebuild fails; `--skip-build` when the binaries are already fresh.
The editor is **closed by the rebuild** (Live Coding holds the DLLs) and relaunched by the CLI.

**Focused iteration** — `--attach` skips the rebuild AND the relaunch and drops the request on the
RUNNING editor's inbox (stale-binary risk is yours; the watcher loses its process-liveness signal but
the stall limit still bounds a dead editor). `--stations giza,rio` runs a subset by exact name;
`--snaps 6,12,18,24,30` replaces the interim-shot offsets on every selected station (a dense time-lapse
around an artifact). Caveat: an attached run shares the live viewport — if a human is navigating, the
camera fights `BeginStation`; every station sidecar records the ACTUAL capture pose (`cam_ue`,
`cam_yaw`, `cam_pitch`) precisely so such shots are attributable on sight.

**Cache mode** — cached by default (the steady development loop). `--no-cache` makes the runner flip
the actor's *Persistent Disk Cache* off for the run (a world-defining edit → the Rust world rebuilds
without its cache_dir, which disables BOTH the byte cache and the persisted-geometry tier) and restores
the editor's prior setting when the run ends — a true cold-network measurement, named `…_nocache` so
cached/cold runs never get compared by accident.

### `fly` — the landmark flyby (the "video" path)

`geo-bench fly [--rate HZ]` (default 10 Hz) flies a fixed landmark route — Golden Gate Bridge → orbit →
Statue of Liberty → orbit → White House → orbit → each New7Wonder + the Great Pyramid, zipping to a
1,200 km orbit directly above each next target between stops. The CLI generates the ENTIRE camera track
pre-sampled at the capture rate (smoothstep easing in **geodetic** lon/lat/h space — a cartesian lerp
between far-apart surface points would chord through the planet), with a local-up per sample; the runner
just plays it: per-frame camera interpolation, and at every sample crossing a PNG (skipped + counted as
dropped if the previous screenshot is still pending) plus a **full-telemetry sidecar, always** —
timestamped `t_s` (track time) + `captured_at_s` (actual) + `leg`, with the complete metrics set
(pipeline, caches incl. warm occupancy, pool, texture pipeline). Output: `<run>/frames/NNNNN_<leg>.png/.json`.
Disk note: ~280 s at 10 Hz ≈ 2,800 PNGs ≈ 4–5 GB per run. The frame sidecars make any artifact in the
flight attributable to the exact pipeline state at that instant.

## The standardized scenario

Eight stations — each holds until the pipeline **settles** (in-flight = decoding = pending-binds =
pending-texture = fading = 0 AND the rendered count flat over 16 samples ≈ 4 s) or times out, with
interim shots at t=10 s and t=30 s to capture convergence shape. First the descent:

1. **`orbit_na_coarse`** — 18,000 km over (-100°, 42°): North America framed at the coarsest level.
2. **`low_orbit`** — 1,000 km over (-87°, 41°): the mid-ladder.
3. **`new_castle_de`** — 2.5 km over New Castle, Delaware (-75.566°, 39.625°), looking north at the
   town: the ground-detail station.

Then five fixed global ground stations (cold-region teleports, varied terrain and hemisphere):

4. **`tokyo_bay`** — 3 km over Tokyo Bay (139.74°, 35.60°): dense megacity coast.
5. **`grand_canyon`** — 4 km over the Grand Canyon (-112.11°, 36.02°): extreme vertical relief.
6. **`mont_blanc`** — 7 km over the Mont Blanc massif (6.86°, 45.76°): high-alpine peaks.
7. **`giza`** — 2.5 km over Giza (31.13°, 29.92°): low-contrast desert + monuments.
8. **`rio`** — 3 km over Rio de Janeiro (-43.21°, -23.01°): southern hemisphere, steep coast.

The camera path is the *same every run* (the global stations are fixed, not re-randomized), so any
two runs' settled sidecars are directly comparable.
The actor's ENU origin must match `--origin` (default: the map's London origin `0.1181, 51.5099, 0`).

## Run folders (untracked — `/Bench/` is gitignored)

```
Bench/
  _inbox/                                  # request files (consumed by the runner)
  _interactive/<stamp>/                    # `snap` output
  2026-06-10_1630_main_0f59017-dirty_label/
    00_orbit_na_coarse_t10s.png            # + .json sidecar per shot
    00_orbit_na_coarse_t10s.json
    …
    02_new_castle_de_settled.png
    run_complete.json
    notes.md                               # `annotate` appends here — the interpretation record
    artifacts/                             # `add` files flamegraphs/traces here
```

The folder name carries date (UTC), branch, short SHA, dirty flag, and optional label — the git
state IS the experiment id.

## Sidecar schema (per screenshot)

Pipeline state: `rendered, resident, built, retained, visible_comps, in_flight, decoding,
pending_texture, pending_binds, awaiting_confirm, fading, textured, frame_id` · failures:
`fetch_failed, decode_failed, graft_failed, url_resolve_failed` · the TWO caches (v17):
`flash_hits/_misses, flash_mb` (the in-memory cell) + `disk_hits/_misses, disk_cached_mb` (the ONE
disk cell over its byte + decoded-geometry tiers: hit = any tier served, miss = the network was
needed, mb = both tiers) · pool: `free_slots, resident_cap, evicted_total, pool_starved` · **the
selection X-ray** (why coarser-than-SSE regions are held; snapshot of the last full step):
`hold_kick, hold_dead, hold_loading, hold_leaf_under_sse, hold_pending_subtree, fetch_skip_grafted`
— a stuck-coarse scar is `hold_leaf_under_sse`/`fetch_skip_grafted` sustained at quiescence, while
`hold_kick`/`hold_loading` are normal convergence and settle to zero · texture pipeline:
`decode_dispatched/_job_ok/_job_empty, drain_drop_evicted/_dup, upload_confirmed/...` · geometry:
`built_verts, built_tris, visible_verts, visible_tris` · frame costs: `draw_calls,
primitives_drawn, game_thread_ms, render_thread_ms, gpu_ms, geo_step_avg_ms, geo_step_peak_ms` ·
provenance: `station, tag, captured_utc, station_elapsed_s, git, label` · capture pose: `cam_ue
[x,y,z cm], cam_yaw, cam_pitch` (the viewport camera AT capture — compare against the request's
station to detect anything that fought the placement).

Filled by `ALogiBakedGeoTerrain::GetBenchMetrics` (LogiBakedGeoTerrainActor.cpp). **It never calls
`logi_globe_world_poll`** — the poll is the single-consumer frame intake; an extra poll would consume
a frame behind the actor's delta-reconcile and silently corrupt it. Only cached step telemetry +
lock-free FFI atomic accessors are read.

## Focus independence

Two layers (both required — the editor otherwise throttles to ~1 Hz and editor-world actors stop
ticking when another window is active):

1. `Config/DefaultEditorSettings.ini` → `[/Script/UnrealEd.EditorPerformanceSettings]
   bThrottleCPUWhenNotForeground=False` (note: UE 5.8 keeps this class under `config=EditorSettings`,
   NOT `EditorPerProjectUserSettings`).
2. The runner adds a realtime viewport override (`AddRealtimeOverride`) for the run's duration and
   re-forces the throttle setting at run start, so a stale per-user Saved/ config can't break a run.

## The built-in run watcher

The CLI is the **single source of truth** on run health — nothing outside it should ever poll. While
waiting it watches three signals, each with a distinct exit:

| Signal | Mechanism | On failure |
|---|---|---|
| Completion | `run_complete.json` | exit 0, summary printed |
| Editor liveness | the spawned process handle (`try_wait`) | exit 3 within ~2 s: "editor exited (code X)" |
| Artifact progress | new sidecars (top-level for `run`, `frames/` for `fly`) | exit 4: "stalled — no new artifact for Ns" |

Stall limits: 240 s for station runs (one settle-timeout + boot margin), 90 s for fly (~10 sidecars/s
when healthy). A hard ceiling still backstops everything (exit 1). Station shots echo as they land;
fly prints a frame-count line every ~30 s. Born of the 2026-06-11 incident where an externally-closed
editor left a marker-only wait polling for 20+ minutes.

## Gotchas

- **A force-closed editor blocks the NEXT bench launch.** `rebuild-editor.ps1 -SkipSave` (or a crash)
  leaves `Saved/Autosaves/PackageRestoreData.json`; the next editor start then blocks on a modal
  "restore packages?" prompt *inside EditorInit* — the run sits at frame 0 until timeout with nothing
  in the log. The CLI clears the stale file before each launch (the rebuild step's auto-save means a
  bench launch never has anything to restore). Diagnostic signature: log frozen at `[  0]` frames, one
  game thread parked in `FSlateApplication::AddModalWindow` under `UPackageTools::LoadPackage`.
- `GetBenchMetrics` reads cached step telemetry + lock-free FFI accessors only — never add a
  `logi_globe_world_poll` call to it (single-consumer; it would silently corrupt the delta reconcile).

## Workflow for an investigation

1. `geo-bench run --label before-fix` — capture the baseline (the bug as data).
2. Make the change; `geo-bench run --label after-fix`.
3. `geo-bench compare <before> <after>` — the settled-station delta table.
4. `geo-bench annotate --note "…"` — write down what the numbers mean while it's fresh.
5. `geo-bench add <flamegraph.svg>` — file any supporting captures with the run they describe.
