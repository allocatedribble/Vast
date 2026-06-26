# ADR-0022 — LogiSky: the Rust-Modeled Atmosphere

- **Status:** Accepted (2026-06-12) — owner directive: remake the stock `SkyAtmosphere` actor. Build
  phases in [Docs/Plans/LogiSky-Atmosphere-Remake.md](../Plans/LogiSky-Atmosphere-Remake.md).
- **Date:** 2026-06-12
- **Deciders:** Vast architecture (owner-decreed goals: 1 efficiency, 2 visuals ≥ stock, 3 seamless with
  the Geo terrain engine — kill the tile quilting, 4 generalize the render-abstraction data in the ECS).
- **Supersedes (on completion):** the stock-`SkyAtmosphere` reliance of the planet-true sky stack (the
  ABI v15 `sky_anchor` lane anchored the STOCK actor; LogiSky replaces the actor and absorbs the anchor's
  job). The v15 entry point remains the planet-truth link until S2 retires the stock actor.
- **Refines:** ADR-0005 (the sky MODEL becomes Rust-owned state behind the narrow ABI; UE keeps GPU
  evaluation), ADR-0007/0019 (the sky and the streamed globe become one planet-truth cell, properly
  interlinked), ADR-0012 (sky data rides the existing poll, event-gated).
- **Replaces as the plan of record:** "Fix B" (the ellipsoid-depth aerial-perspective backstop designed
  2026-06-08). Fix B patched the stock actor's AP from outside; LogiSky builds that idea in as a law of
  the new system (§ Decision 3) instead of a bolt-on.

## Context

### Why the stock actor cannot serve a streamed planet

Engine-source findings (2026-06-08, verified) + the 2026-06-12 orbital screenshots:

1. **Aerial perspective on opaque is depth-keyed and tile-blind.** It is a separate full-screen pass
   reading per-pixel scene depth; ANY per-tile depth variation (Draco quantization grids, LOD steps)
   becomes a tile-aligned step in the haze — the quilt. No per-primitive data, no stencil: not tunable.
2. **The camera AP froxel volume is a 32×32×16 LUT over 96 km** with a sqrt-depth axis (far slice ≈
   11.6 km, 31:1 vs near). From orbit, EVERY ground pixel lands past/in the far slice — the orbital
   checkerboard in the 2026-06-12 shots is this LUT's quantization shaded onto tile depth steps. The
   actor was designed for in-atmosphere cameras; we fly from 1 m to 18,000 km.
3. **One spherical planet at one radius** vs the WGS84 ellipsoid the Geo engine streams (±7–15 km by
   latitude even after the v15 anchor) — the sphere-vs-ellipsoid residual (the tokyo veil suspect; the
   ladder's "two cells, one truth, no link" finding, only half-closed by anchoring).
4. **Per-frame cost is structural:** transmittance + multi-scattering + sky-view LUTs + the froxel
   volume build + a full-screen apply, every frame, even with a parked camera and unchanged sun —
   exactly the un-gated cadence ADR-0018 killed in the Geo step.

### Why the model belongs in Rust

ADR-0005's law: Rust owns truth, Unreal renders. The atmosphere has been UE-owned *truth* (planet
radius, sun direction, scattering parameters live on the actor) — which is how it diverged from the
streamed planet in the first place. The atmosphere model is pure math (Hillaire-style scattering — the
same family the engine implements): deterministic, headless-testable, renderer-agnostic — the kind of
state goal 4 wants generalized into the ECS render abstraction.

## Decision

Build **LogiSky**: the atmosphere as a Rust ECS cell that owns the model and bakes the data; Unreal as a
thin GPU-evaluation shell. **Scope (owner amendment, 2026-06-12): LogiSky encompasses ALL atmospheric
rendering** — sky and limb, aerial perspective, clouds, haze/fog, twilight: every participating-media
effect routes through this one cell; no second atmospheric system may grow beside it. Six laws:

1. **Rust owns the sky model.** A `SkyState` cell beside the Geo world (same ENU/WGS84 geodesy,
   single-homed — the planet model can never diverge from the terrain again): atmosphere physical
   parameters (Rayleigh/Mie/ozone coefficients + scale heights), the sun policy (zenith-follow today,
   time-of-day later), and the planet ellipsoid. Components/Resources in the ECS; deterministic;
   unit-tested headless.
2. **Rust bakes the LUTs; the GPU evaluates them.** The camera-independent tables (transmittance,
   multi-scattering) are baked in Rust — pure functions of the atmosphere parameters, recomputed only
   when parameters change (≈ never per frame) — and cross the ABI as byte buffers exactly like tile
   textures do. Per-frame, Rust derives a small POD `LogiSkyFrame` (camera geocentric up + altitude
   above the ELLIPSOID, sun vectors, horizon angle, LUT metadata) riding the existing poll. No Unreal
   types cross; Rust never calls Unreal (ADR-0005 §1C holds).
3. **Aerial perspective is analytic from ellipsoid depth — never from scene depth.** The UE shell
   applies AP in one post pass that intersects each view ray with the WGS84 ellipsoid (LWC-safe,
   camera-relative) and evaluates scattering from the Rust LUTs at that smooth distance. Per-tile depth
   steps never enter the computation, so the quilt and the orbital checkerboard are impossible *by
   construction* — not suppressed, unrepresentable (goal 3). Terrain above/below the ellipsoid gets a
   bounded analytic height correction, not a depth read.
4. **Event-gated cadence (ADR-0018's law, applied to the sky).** LUTs rebuild on parameter change; the
   sky-view evaluation updates on camera/sun deltas past thresholds; a parked camera under an unchanged
   sun costs ~zero sky work. Per-frame cost target: one post pass + one sky-dome evaluation, no 3D
   volume build (goal 1).
5. **The data generalizes the render abstraction (goal 4).** `LogiSkyFrame` + the LUT buffers are
   renderer-agnostic plain data — any future renderer (or offline tool, or the bench harness) consumes
   the same block. The bench sidecars gain the sky fields so visual regressions become diffable, the
   same trick that root-caused four streaming bugs.
6. **The atmosphere ships a formula ladder ([ADR-0023](ADR-0023-One-Truth-Many-Formulas.md)).** Every
   atmospheric quantity is a FAMILY of evaluators at declared (cost, error) tiers off the one parameter
   truth: the exact/reference numerical form (in-repo always — the test oracle and bake source), the
   baked-LUT runtime tier, and cheaper analytic tiers (e.g. Chapman-function transmittance) as measured
   need adds them. Tiers state and TEST their error bounds against the reference (agreement gates);
   handoffs happen inside overlapping validity domains — quality levels fall out of the family, and the
   system stays seamless at every scale from orbit to on-foot.

**Bi-tree shape (ADR-0019 § 4):** bottom shaped for the top — LUT byte layouts chosen so the UE upload
is one flat copy into a `UTexture2D`, `LogiSkyFrame` offset-locked like every ABI struct; top shaped for
the bottom — the event-gated cadence keeps the per-pixel shader the only hot path, and the analytic-AP
law keeps that shader branch-light (no depth fetch, no froxel sampling).

**Visual bar (goal 2):** Hillaire single + multi-scattering at ellipsoid-true geometry must match or beat
the stock actor on the bench stations — judged by A/B screenshots at the eight stations plus the orbital
limb, with the stock actor kept behind a knob until the A/B passes (it is then deleted, ADR-0004).

## Consequences

- **Positive:** the quilt/checkerboard class of artifact becomes unrepresentable; sky cost becomes
  event-gated instead of per-frame; the planet model is single-homed in Rust end to end (terrain, sky,
  sun, geodesy); the sky becomes deterministic + headless-testable + replayable; the render abstraction
  gains its first non-tile data lane (the template for water/weather/foliage lanes after it).
- **Negative / costs:** we own scattering math the engine used to own (mitigated: the model is published
  and the engine's implementation is reference-readable); volumetric-cloud lighting must be re-fed (the
  stock cloud component reads the stock atmosphere — S3 scope); a new shader surface to maintain; the
  A/B window runs both systems behind a knob until parity (temporary cost, then the stock path is
  deleted per ADR-0004).
- **Risks:** UE post-process integration depth (the SVE seam is engine-proven but ours to maintain);
  matching stock's perceptual tuning may take iterations — hence the knob + station A/B harness rather
  than a flag-day swap.

## Alternatives considered

- **Keep tuning the stock actor** (anchor + Fix B + knobs): the AP pass stays depth-keyed and the froxel
  stays 96 km — the orbital regime stays out of design range. Rejected: the artifacts are architectural.
- **Fix B alone** (ellipsoid-depth AP bolt-on over the stock actor): fixes the quilt, keeps UE-owned sky
  truth, per-frame LUT costs, and the sphere model underneath. Rejected as an end state; absorbed as law 3.
- **Full GPU atmosphere in Rust (wgpu) shared to UE:** ADR-0017's buffer-export wall applies — wgpu
  cannot export the textures to UE today. Rejected on the same evidence; CPU-baked LUTs cross as bytes
  instead, which the tile-texture lane already proves out.

## References

- Engine findings + Fix B design + planet-model mismatch history: the 2026-06-08/11 sky-stack record
  (operational memory; key facts restated in *Context* so this ADR stands alone).
- The v15 planet-truth link: `logi_globe_world_sky_anchor` + `AnchorSkyToPlanet`/`UpdateSunZenith`.
- Build phases, budgets, and A/B gates: [Docs/Plans/LogiSky-Atmosphere-Remake.md](../Plans/LogiSky-Atmosphere-Remake.md).
