# Geo Highway Trace — blurry/sharp patchwork, hard seams, stippled textures (2026-06-09)

**Trigger.** A low-altitude city view (after the A–E LOD concentration redesign) showed: the focal
city CRISP, but (1) a **blurry/sharp patchwork** of adjacent tiles, (2) **hard diagonal SEAMS** across
coarse/water areas, and (3) a regular dark **STIPPLE / dither** on coarse/peripheral/water tiles —
"obviously not just the knobs." User asked for a microscopic, highway-style trace of the entire
request→render path with intended-behavior verification, using opus subagents.

**Method.** A 9-stage request→render trace (workflow `wv1caom38`, 27 agents): one agent traced each
stage end-to-end and verified its INTENDED behavior, hunting the three symptoms; every suspected cause
was then handed to an independent adversarial verifier that defaulted to *not-real* unless the cited
code proved the symptom. Plus a manual trace of `decide` (selection) and the C++ depth-bias / texture
state machine.

## Result: stages 2–7 are CLEAN

`intended_ok = true`, no confirmed defects:

| Stage | Verdict |
|---|---|
| 2 — request / dead / backoff (`geo_world.rs`) | clean: dead/absent contract holds; no parent-pinned-coarse |
| 3 — fetch / cache (`logi_3dtiles_io`) | clean: bytes that exit == bytes for this tile's URL; CRC ok |
| 4 — decode geometry (`logi_gltf`/`logi_draco`) | clean: **f64** node transform; edges align — **no geometry crack** |
| 5 — decode texture / rebase | clean: correct atlas bound, UVs correct |
| 6 — FFI boundary | clean: mesh↔texture↔key association exact |
| 7 — C++ build / recycle mesh | clean: no stale recycled slot, no shared section |

⇒ The seams are **not** a geometry crack and **not** a texture mis-association. Everything converges on
the **selection** and **C++ render/texture** ends.

## Root cause #1 — STIPPLE + SEAMS (HIGH): the partial-reveal coplanar overlap

The LOD-concentration "lever A" dropped the kick's `|| loaded` disjunct, turning a resident parent's
refinement into a **partial reveal**: it renders its loaded fine children AND pushes its **own coarse
mesh** to hole-fill the still-unloaded children.

- `logi_3dtiles_select/src/lib.rs`: a loaded child renders its own mesh (672–674); each unloaded child
  fills its hole with `coverage` = the parent `P` (691–694). `P` spans the **whole** footprint, so it
  sits **coplanar over the loaded children** — two opaque, depth-writing surfaces for the same ground.
- The kick that would collapse this to just-`P` is gated OFF in steady state
  (`kick = … && !sub.any_rendered_last_frame`), so the overlap is **sustained**, not transient.
- The only depth separation is the per-tile Pixel Depth Offset in
  `LogiBakedGeoTerrainActor.cpp:1721`: `DepthBiasCm = log2(geometric_error)·2cm` ≈ **2 cm per LOD
  level** (≤ ~40 cm). A coarse mesh interpolated over a 2×–32× larger cell pokes **metres** above the
  fine surface on convex terrain (ridges, rooftops, water edges) — far more than 2 cm — so coarse
  fragments win the depth test on scattered pixels = the **stipple**; the coarse-vs-fine edge = the
  **seam**. Worst on coarse/peripheral/water tiles, where the LOD gap is largest.
- The PDO is trapped between two failures: large enough to separate the overlap ⇒ it shifts
  SkyAtmosphere's scene depth and re-creates the blue tile-quilt; small enough to be
  atmosphere-invisible ⇒ it cannot separate the overlap. There is no value that works.

**Fix (revert A).** Restore `will_kick = kick && (sub.not_yet_renderable > LOADING_DESCENDANT_LIMIT ||
loaded)`. A resident parent now renders **coarse-OR-fine atomically**: it kicks to its own coarse mesh
until **all** descendants load, then swaps in one frame. No overlap ⇒ no z-fight, no LOD seam. The
"stuck coarse from orbit" problem A was meant to cure was the **fetch budget spreading thin**, already
fixed independently by levers B + E (distance² priority, foveation lobe, per-step fetch cap,
parent-resident depth-first boost) — so the focal region loads fast and the atomic swap is quick. Test
reverted to `one_unloaded_grandchild_kicks_its_subtree_to_the_loaded_parent`. The PDO is left in place
(≤40 cm, atmosphere-invisible; inert now that the overlap is gone — it was never the seam cause).

## Root cause #2 — BLURRY PATCHWORK (MEDIUM): texture surrender under upload pressure

A resident tile whose GPU upload loses the D3D12 `LockTexture2D` heap race repeatedly is stranded on a
flat placeholder:

- `TextureAttempts` is bumped per decode **dispatch** and is meant to retire a genuinely-undecodable
  atlas. But an upload failure (`ProcessPendingTextureBinds`, State == −1, past `MaxUploadRetries`)
  returns the texture to the pool and lets `DriveTileTextures` **re-decode** — burning one
  `TextureAttempts` each cycle even though the bytes decode fine.
- At the cap (`GMaxTextureAttempts = 4`), `DriveTileTextures` **surrenders**: it un-hides the tile on
  the **null-texture placeholder** (a flat patch), reports it textured (so the real coarse cover
  drops), and `Done`-removes it from `PendingTexture` — never re-driven for the life of that resident
  tile. The verifier narrowed it: pure **decode**-failure surrender is correct (undecodable bytes);
  only the **upload**-pressure path (decode-OK) is the no-recovery bug. Trigger is a corner case
  (sustained heap saturation through the `GlobeTextureUploadsPerStep` throttle), hence MEDIUM.

**Fix (LogiBakedGeoTerrainActor.cpp).**
1. Reset `Entry->TextureAttempts = 0` on a **successful decode** (`Mips.Num() > 0`, ~line 903) — so
   upload-pressure cycles never accrue toward the cap; only true decode failures count.
2. The `ProcessPendingTextureBinds` upload-fail surrender (~line 1007) **no longer `MarkTileTextured`** —
   it keeps the tile HIDDEN under its real-textured coarse parent and in `PendingTexture`, so
   `DriveTileTextures` re-drives it until the upload lands for real. Only a genuinely-undecodable atlas
   still surrenders to the placeholder (in `DriveTileTextures`, bounded + rare).

## Verification

- Rust: `cargo test --workspace` 0 failures (select 27, scene 36); `cargo clippy --workspace
  --all-targets -- -D warnings` clean.
- C++: editor rebuild (`rebuild-editor.ps1`, run b3d56r1sw) → `Result: Succeeded`, editor relaunched.
- **PIE eyeball pending**: confirm the stipple and diagonal seams are gone, no flat placeholder patches,
  and the focal region stays crisp with a quick coarse→fine swap on descent.

## Notes / remaining

- B / D / E (budget concentration) are KEPT; C remains reverted (the fixed `DETAIL_BAND_M` soft band).
- If convergence still lags after these fixes, the remaining lever is over-demand / pool eviction churn:
  `DetailTargetPx` 1 → ~4 (and/or a focal-aware eviction policy), per the LOD-redesign memo.
- Workflow had a few verifier agents fail to emit structured output (mid-run); the two confirmed root
  causes above were each independently adversarially verified.
