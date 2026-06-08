# Geo Pipeline — Deep Microscopic Audit (2026-06-07, second pass)

> **Method.** Seven Opus subagents, one per pipeline slice, read-only and code-cited, run in parallel:
> download/IO · 3D-Tiles protocol+extensions · selection/refinement/SSE · glTF+Draco · scene/streaming ·
> FFI boundary · C++ render. Scope = the ~5,300 uncommitted insertions across the geo stack
> (`logi_3dtiles_*`, `logi_gltf`, `logi_draco`, `logi_geo_ffi`, `LogiBakedGeoTerrainActor`). Supersedes the
> first-pass [geo audit](../../Rust/crates/geo) notes; companion to
> [2026-06-05-Deep-Quality-Audit.md](2026-06-05-Deep-Quality-Audit.md).

## Executive summary

The architecture is sound (actorless ADR-0007, poll-FFI ADR-0012, Arc-retained geometry, pre-spawned tile
pool, decode-only vendored Draco, correct SSE math). The defects are **leaks at the seams** and a
**render-side texture pipeline that strands tiles on the default material**.

**The headline "extremely low-quality terrain texture that never sharpens" is the render side, not selection.**
Multi-agent consensus: a tile is added to the scene the moment its **geometry** decodes and is drawn with a
material whose atlas is decoded **asynchronously and only dispatched once**. If that one decode is starved by
the shared mesh/texture budget, fails (corrupt/unsupported image), or the tile was built before its texture
was ready, **nothing ever re-drives it** — the tile shows the engine's gray default material forever. The
SSE/refine engine is correct; the texture never arrives. **Fixed in this pass** (see §1).

A **second, independent** refinement defect makes genuinely-coarse patches persist: a terminally-failed child
(404 / max-retries / unresolvable URI) is permanently skipped by the scene but **never signalled to the
selector**, so its REPLACE parent's subtree stays `all_renderable == false` and the parent renders coarse
forever. This + the unused `childSubtreeAvailability` bit are the refinement backlog (§2).

## Remediation status (2026-06-07, second session — build-verified)

Verified = Rust clippy `-D warnings` + geo tests green; C++ `Build.bat LogiUEEditor` Result: Succeeded; the
v6 `logi_geo.dll` rebuilt (release) and staged.

| Finding | Status |
|---|---|
| Texture re-drive + retry + neutral placeholder (the headline) | ✅ landed (first session) |
| **`KHR_materials_unlit` option** — flag plumbed glTF→`TileGeometry`→`LogiGlobeTile.flags` bit 0 (ABI **v6**, repurposed `_pad`); `EGoogleTileLighting {Auto,ForceUnlit,ForceLit}` actor setting; per-tile material select + lit fallback | ✅ landed + verified |
| Vertex-buffer copy elimination (`streaming.rs` builds bytes directly, no `cast_slice().to_vec()`) | ✅ landed + verified |
| Memory caps — `subtree_cache` FIFO-evicted; `subtree_failed`/`decode_failed`/`fetch_attempts` bounded | ✅ landed + verified |
| O(1) subtree-fetch dedup (HashSet, was O(in-flight) string scan) | ✅ landed + verified |
| `extensionsRequired` enforced (refuse a tileset requiring an unsupported extension) | ✅ landed + verified |
| `set_view` reads `LogiGlobeView` unaligned (UB fix); `LogiStatus` ABI size/discriminant guard | ✅ landed + verified |
| Eviction-overlap — hide departed coarse sections immediately when over teardown budget; re-show on re-select | ✅ landed + verified |
| ureq `gzip` feature pinned (+ doc) | ✅ landed + verified |
| Doc-drift reconciled (plan §6/§3a: Geo handoff is a Mutex mailbox + Arc-clone, not a ring/`release`); **lock-free FFI explicitly DEFERRED** | ✅ landed |
| **R1** dead-child → `select` `is_dead` predicate (a terminally-dead tile is absent, so it stops drawing the whole coarse parent over its loaded siblings; a fully-dead subtree still falls back to coarse) | ✅ landed + verified (2 select tests) |
| **R2** `childSubtreeAvailability` consulted before fetching a child `.subtree` (prune non-existent vs 404) | ✅ landed + verified (1 scene test) |
| `poll` hands `Arc<[GeoRenderTile]>` (kill per-tile atomic storm); `LogiGlobeWorld` single-consumer debug-assert; baked-lane `frame_scratch` lifetime | ⏳ next increment |
| IO worker returns `Arc<[u8]>` (drop the `into_boxed_slice` realloc); pool-sizing comment | ⏳ next increment |
| Stream-thread panic guard | ⚠️ not effective under `panic=abort` (release aborts) — real fix is a panic-free-schedule audit, not `catch_unwind` |
| Anisotropic sampler | ⚠️ governed by `r.MaxAnisotropy` + World texture group + the material's from-texture sampler (likely already active) — verify/author in the material, not a speculative code change |
| Per-tile `asset.copyright` (ToS) | ⛔ excluded by request |

## §1 — The texture bug (root cause + fix applied)

**Confirmed by reading** `LogiBakedGeoTerrainActor.cpp`:
- A new tile is added to `Tiles` with its **geometry only**; the atlas decode is dispatched **once**, inside
  the `if (!Tiles.Contains(Key))` branch (`cpp:953-996`). Every later frame hits `continue`, so the dispatch
  never repeats.
- `DrainDecodedTextures` shares **one** `GeoBuildBudgetMs` (2 ms) deadline with the mesh-build loop
  (`cpp:883-888,967`); under a streaming burst it can land as few as one texture/frame against up to 32 new
  untextured tiles/frame.
- A failed decode is terminal: `if (Done.Mips.Num()==0) continue;` clears the in-flight key with **no retry**
  (`cpp:721-722`). `DecodeJpegToBgraMips` only recognizes JPEG/PNG magic bytes (`cpp:183-185`) — a WebP atlas
  would be permanently untextured.
- A material-less tile falls back to `UMaterial::GetDefaultMaterial(MD_Surface)` — the **gray checker**
  (`LogiTileMeshComponent.cpp:55-59`). So "untextured" reads as broken, not just low-res.
- **Refuted:** the double-lit/washed-out theory. `M_LogiGlobeTile.uasset` is authored **unlit**
  (`ShadingModelField=1`, `BaseColorTex`→`EmissiveColor`), so dropping `KHR_materials_unlit` is currently
  harmless (latent only — see §3).

**Fix applied (this commit):**
1. **Texture re-drive + retry.** The build pass now re-dispatches the decode for any already-resident tile
   whose texture never landed (`!Texture.IsValid() && texture_jpeg_len>0`), bounded per frame and capped per
   tile (`MaxTextureAttempts = 4`). `publish()` re-publishes every resident tile each frame with its
   Arc-retained JPEG bytes, so the retry reads straight from the current frame's pointer. This closes the
   starvation, the one-shot, and the no-retry holes together.
2. **Neutral placeholder material.** A new tile is now assigned an unbound MID of the globe base material
   (BaseColorTex cleared) at build time, so an un-textured tile shows a flat colour, not the gray checker.
   `DrainDecodedTextures` **reuses that same MID** (just points BaseColorTex at the decoded atlas), so the
   textured path costs no extra MID.
3. Per-tile `FTileEntry::TextureAttempts` counter (`.h`) bounds retries so a permanently-bad atlas can't
   thrash the worker pool.

**Not yet done (recommended follow-ups):** anisotropic sampler on the streamed `UTexture2D`
(`AcquireTexture`, `cpp:677-687` sets no `Filter`/`LODGroup`/anisotropy → blurry at grazing angles); WebP
decode (enable `EXT_texture_webp` in `logi_gltf` + an `image/webp` branch in `DecodeJpegToBgraMips`).

## §2 — Refinement defects (the "coarse never sharpens" backlog)

| ID | Severity | Defect | Location | Fix |
|----|----------|--------|----------|-----|
| R1 | High | Terminally-dead child (404 / `MAX_FETCH_ATTEMPTS` / unresolvable URI) is permanently skipped by the scene but **never signalled to `select`**, so its REPLACE parent's subtree stays `all_renderable=false` → parent renders **coarse forever** (localized when ≥1 sibling renders; whole-subtree when not). | `geo_world.rs:1269-1314` (skip) vs `select/lib.rs:737-755` (kick) via `is_loaded` `geo_world.rs:1180-1184` | Add an `is_dead(path)` predicate across the select↔scene seam; `decide` treats a dead child like `Unavailable`/leaf so siblings satisfy `all_renderable` and the parent de-selects. Needs unit tests. |
| R2 | High | `childSubtreeAvailability` is **decoded but never consulted** (`Subtree::is_child_subtree_available` has zero production callers). The provider issues a `.subtree` fetch at every subtree boundary even where the parent says no child subtree exists → wasted round-trips, feeds the unbounded `subtree_failed` set, holds regions coarse one extra round-trip. | `geo_world.rs:739-776` (provider) vs `implicit/lib.rs:154-156` | In `covering`/`tile_availability`, when crossing into a child subtree consult the **parent** subtree's `is_child_subtree_available(child_root_local_coords)`; return `Unavailable` (prune) instead of `Pending`+fetch when the bit is clear. Careful Morton coord mapping + tests (mis-mapping → holes). |

SSE/refine/kick engine itself is **correct** (spec-exact formula, nearest-surface distance, meter-consistent
units, proven parent de-selection + atomic kick handoff, hysteresis only resists *coarsening*). Do **not**
change the formula.

## §3 — 3D Tiles extension scorecard (Google P3DT)

| Extension | P3DT serves | We handle | Verdict |
|---|---|---|---|
| `3DTILES_implicit_tiling` | yes (core) | yes — **but `childSubtreeAvailability` unused** | **R2** |
| `KHR_draco_mesh_compression` | yes | yes (vendored decode-only Draco) | good |
| `KHR_materials_unlit` | yes (required) | parse-accepted; **flag dropped** | latent — material is unlit today; plumb the flag if it ever goes lit |
| glTF `asset.copyright` (per-tile) | **yes (required)** | **no** | **Critical (ToS):** per-tile attribution never extracted/displayed — Google Map Tiles terms breach |
| `extensionsRequired` enforcement | yes (sends it) | **no (ignored)** | High: unknown required ext → silent garbage instead of refuse-to-render |
| `EXT_texture_webp` | possibly (region) | **no (feature off)** | Medium: webp atlas → untextured |
| `EXT_mesh_features` / `EXT_structural_metadata` | sometimes | **no** | Medium: ADR-0011 enrichment gets nothing from the stream |
| `EXT_meshopt_compression` | unlikely (uses Draco) | no | Low (defer; reject via `extensionsRequired`) |
| `CESIUM_RTC`, S2 bounds, multi-content ext, `3DTILES_metadata` | no (P3DT) | partial/no | Low (matters only for non-Google OGC sources) |

## §4 — Memory-over-a-long-session scorecard

Big buffers (mesh/texture) are correctly bounded (`MeshCache` byte budget + resident cap + Arc-drop-on-evict,
all verified). The genuine **monotonic creep** is the implicit-tiling bookkeeping, none of which the cold-graft
prune covers:

| Structure | Bounded? | Driver | Fix |
|---|---|---|---|
| `subtree_cache` (`geo_world.rs:658-660`) | **no** | distinct `.subtree` URLs over explored extent | LRU / add to `prune_cold_grafts` |
| `subtree_failed` (`:666`) | **no** | dead/absent subtree URLs | age-out in the periodic prune |
| `decode_failed` / `fetch_attempts` (explicit tree) (`:646,:652`) | **no** | dead URLs not under a graft | bound by last-touched frame |
| per-tile `bounds_memo` `OnceCell` | no (tiny) | every region tile ever visited | acceptable; note only |

Entries are small (URLs/paths), so this is slow RSS creep, not the multi-hundred-MB kind — but it is the
implicit analogue of the graft leak already fixed for explicit tilesets, and worth closing for long flights.

## §5 — Other notable findings (by severity)

**Critical**
- **C++ texture strand** — §1. **FIXED.**
- **Per-tile `asset.copyright` not displayed** — Google ToS breach (`logi_gltf/src/lib.rs`). Plumb
  `asset.copyright` → `TileGeometry` → C++ attribution HUD.
- **FFI baked-lane dangling pointers** — `logi_baked_world_step`/`select` reuse one `frame_scratch` `Vec`;
  the previous frame's pointers are invalidated by the *next call of any kind*, weaker than the globe lane's
  Arc retention and undocumented (`ffi_baked.rs:294-379`). Document the consume-immediately contract or
  retain a second buffer.

**High**
- **`poll` per-frame cost** — `GlobeStreamer::poll` clones the whole `GeoRenderFrame` (Vec alloc + an Arc
  atomic-bump **per tile**) on the game thread every camera-moving frame (`stream_thread.rs:142`,
  `ffi.rs:533-566`). Hand the published tile vec across as `Arc<[GeoRenderTile]>` so poll clones one Arc.
- **`LogiGlobeWorld` not safely `Send`** — the C ABI lets C++ alias `&mut` from two threads (UB); safe only by
  an unstated single-consumer-thread invariant (`ffi.rs:277-284,633`). Document + debug-assert the consumer
  `ThreadId`.
- **Stream-thread panic is silent** — a panic in the schedule kills the producer; the globe freezes with no
  status/telemetry (`stream_thread.rs:226-273`). Wrap `advance_from_ue` in `catch_unwind` + surface a poisoned
  status.
- **`extensionsRequired` not enforced** — §3.

**Medium**
- **Doc/reality drift** — the plan/header/MEMORY describe a "FrameRing (N≥3, publish/retire fence)" +
  `release(frame_id)` + "lock-free FFI" that **does not exist**; the shipped handoff is a `Mutex<Option<frame>>`
  mailbox + per-poll Arc-clone (safe but allocates). Reconcile the docs.
- **`set_view` reads `LogiGlobeView` via `as_ref()`** (requires alignment) while every other inbound struct
  uses `read_unaligned` (`ffi.rs:606`). Latent UB on a misaligned pointer; make it uniform.
- **`LogiStatus` enum size/discriminants** unguarded by the otherwise-complete `offset_of!` suite (`ffi.rs:9-24`).
- **Copy waste out of Draco/glTF** — `bytemuck::cast_slice().to_vec()` full vertex copy (`streaming.rs:146`);
  Draco typed `Vec`s materialized only to be re-read once; **every glTF image copied incl. unused maps**
  (`logi_gltf/lib.rs:234-269`). Cluster into one flat-slice / `Arc<[u8]>` shared-buffer refactor (plan §3b).
- **Slow eviction overlap** — `TileEvictionsPerStep=32` lets a de-selected coarse parent draw over its fine
  children for a few frames during fast descent (`cpp:1022-1049`). Hide departed sections the same frame.
- **O(in-flight) subtree-dedup string scan** (`geo_world.rs:1210`) — use a `HashSet`/hash like tiles do.

**Low / confirmations (don't re-flag)**: gzip is handled once (ureq default feature — pin it); HTTP agent is
reused with a keep-alive pool; disk cache is bounded+evicted (re-reads each hit — no in-mem raw tier); cancel
registry leak-free; ABI offset guards complete on all 8 structs; no structural ECS churn in the hot loop;
par_iter race-free; stream-thread lifecycle clean (start/stop/join, no PIE leak); Draco attribute-id mapping,
normalized-int dequant, decode limits, and decode-only optimized build all correct; texture pointer retained
on the same Arc as geometry (no UAF).

## §6 — Recommended sequence

1. ✅ **Texture re-drive + placeholder** (§1) — done this pass.
2. **R1 + R2 refinement** (§2) — the "coarse never sharpens" pair; own increment with unit tests.
3. **Memory caps** (§4) — extend `prune_cold_grafts` to the implicit bookkeeping sets.
4. **`poll` Arc-vec handoff** + **FFI `Send`/alignment/panic hardening** (§5 High).
5. **ToS copyright** + **`extensionsRequired` enforcement** (§3) — compliance + robustness.
6. **Copy-elimination refactor** (§5 Medium) — flat-slice / `Arc<[u8]>` out of Draco/glTF.
7. **Anisotropy + WebP** (§1 follow-ups), **doc reconciliation** (§5 Medium).
