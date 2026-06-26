# LogiSky — the atmosphere remake, build plan

Plan of record for [ADR-0022](../Architecture/ADR-0022-LogiSky-Rust-Modeled-Atmosphere.md). Goals
(owner, 2026-06-12): **1** more efficient than stock `SkyAtmosphere`, **2** visuals ≥ stock, **3**
seamless with the Geo engine — the tile quilt and the orbital froxel checkerboard become
unrepresentable, **4** the sky data generalizes the ECS render abstraction.

The stock actor stays in the level behind a knob (`bUseLogiSky`, default OFF) until the S4 A/B passes;
then the stock actor + the knob are deleted (ADR-0004 — no dual systems shipped).

## Phases

### S1 — the Rust sky cell (headless, no FFI)
- `logi_sky` crate (under `Rust/crates/geo/`): atmosphere parameter set (Rayleigh β + scale height,
  Mie β + g + scale height, ozone absorption profile), WGS84 ellipsoid shared from `logi_geospatial`
  (single-homed geodesy — never a second planet model).
- **Transmittance LUT baker** (256×64, Bruneton parameterization) + **multi-scattering LUT baker**
  (32×32, Hillaire) in pure Rust. Deterministic; golden-value unit tests against published reference
  numbers; `f32` output buffers laid out for one flat upload (ADR-0019 §4: bottom shaped for the top).
- `SkyState` cell: parameters + sun policy (zenith-follow now, time-of-day seam left explicit) +
  derived per-frame block (camera ellipsoid altitude, geocentric up, sun dir, horizon angle, LUT dirty
  flags). Event-gated: derived block recomputes on camera/sun delta thresholds, LUTs only on parameter
  change.
- Exit gate: `cargo test` golden LUT values + derived-frame math vs `logi_geospatial` cross-checks.

### S2 — the ABI lane + the UE shell (the visual core)

**Design deltas from the 2026-06-12 ADR re-evaluation (ADR-0019 §§8–9, ADR-0023 §7):**
- *Observer-indexed tiers:* the FFI exports the un-collapsed family (LUTs + params + derived frame);
  NO quality field crosses the ABI — tier selection happens in the shader/pass, per view.
- *Store the amplitude:* LUT bytes bake lazily on first FFI observation, generation-stamped; the UE
  side re-uploads textures only when the generation changes.
- *The ABI is a language:* the sky speaks its own sentences — `LogiSkyFrame` + `LogiSkyLuts` PODs with
  two additive entry points (not a widened `LogiGlobeFrame`); resolves this phase's open question.
- *S2 placement:* `SkyState` lives on `LogiGlobeWorld` (FFI-side, game-thread cadence) — the sky cell
  is camera-coupled and the camera arrives on the poll; the stream thread is untouched. The ECS-resident
  move happens when the sim consumes sky data (S3+). Sun policy in S2 = zenith-follow, computed in Rust
  (geodetic up; the C++ light aim migrates to read the frame in S3).
- FFI: `LogiSkyFrame` POD + LUT byte-buffer accessors (offset-locked; additive entries — the globe
  lane is untouched). The sky data rides the existing poll cadence (ADR-0012; no new threads).
- UE: `ULogiSkyComponent` (one component on the existing terrain actor or a `ALogiSky` actor — ONE
  per world, mirroring the actorless law's spirit: a shell, no truth):
  - LUT upload → two small `UTexture2D`s (flat copy, the tile-atlas lane's pattern).
  - **Sky + limb**: sky-dome material evaluating Hillaire sky-view from the LUTs + `LogiSkyFrame`
    (camera-relative, LWC-safe via translated world).
  - **Aerial perspective**: ONE post-process pass (SceneViewExtension + post-process material) —
    per-pixel ray ∩ WGS84 ellipsoid (analytic, camera-relative), scattering from the LUTs at that
    smooth distance + bounded height correction. **No scene-depth read** (ADR-0022 law 3). Engine
    AP-on-opaque suppressed per-view while LogiSky is on.
- Exit gate: editor A/B at giza + low_orbit + orbit_na_coarse — no quilt, no checkerboard, limb
  smooth; `stat gpu` sky cost ≤ stock.

### S3 — integration: clouds, sun, exposure
- VolumetricCloud re-fed (it reads the stock atmosphere's planet/lighting): either parameterized from
  `LogiSkyFrame` or replaced by a simpler planet-true cloud shell — decide on measured visuals.
- Sun: `UpdateSunZenith` migrates into the LogiSky lane (sun truth = Rust `SkyState`); PP_Awe exposure
  re-baselined against the new sky.
- The v15 `sky_anchor` + `AnchorSkyToPlanet` retire (LogiSky IS the anchor); height-fog stays deleted.
- Exit gate: tokyo_bay / grand_canyon / mont_blanc — no mid-latitude veil (this also settles the open
  tokyo-wash attribution: if the veil survives LogiSky, it was PP_Awe all along).

### S4 — the A/B gate + deletion of the stock path
- Full 8-station bench run + flyby, LogiSky vs stock (the knob): sidecars gain `sky_*` fields
  (sun pose already recorded; add LUT generation counts + sky pass ms). Visual parity/judgment by the
  owner on the station shots.
- On pass: stock `SkyAtmosphere` actor removed from the level, knob + dual path deleted (ADR-0004),
  ADR-0022 status updated to "implemented + verified".

## Budgets / non-goals
- Per-frame sky cost target: ≤ stock actor's measured ms at every station (S2 gate measures both).
- Memory: LUTs ≤ 1 MB total resident.
- Non-goals now: time-of-day simulation (seam reserved), weather systems, night sky/stars, ozone
  twilight tuning beyond parity — each is a later cell on this lane.

## Open questions (decide at the phase, not before)
- S2: post-process material vs global shader for the AP pass (material is faster to iterate; global
  shader is cheaper — start material, promote if the ms gate fails).
- S3: keep vs replace VolumetricCloud (measure first).
- S2: whether `LogiSkyFrame` rides `LogiGlobeFrame` (one poll struct) or its own accessor (additive) —
  pick at implementation against the ABI-lock ergonomics.
