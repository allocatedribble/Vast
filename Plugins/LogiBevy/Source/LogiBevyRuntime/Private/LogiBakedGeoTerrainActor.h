// Copyright LogiUE. All Rights Reserved.

#pragma once

#include "CoreMinimal.h"
#include "UObject/ObjectKey.h" // TObjectKey — the per-parent MID-pool bucket key (C3)
#include "GameFramework/Actor.h"
#include "UObject/StrongObjectPtr.h"
#include "Containers/Queue.h"          // TQueue — game-thread inbox for worker-decoded tile atlases
#include <atomic>                       // std::atomic — render->game-thread atlas-upload confirmation (FLogiTileMipChain)
#include "LogiBevyDeveloperSettings.h" // EBakedTerrainMeshing
#include "LogiBakedGeoTerrainActor.generated.h"

class ULogiTileMeshComponent;
class USceneComponent;
class UStaticMeshComponent;
class UMeshComponent;
class UMaterial;
class UMaterialInterface;
class UMaterialInstanceDynamic;
class UTexture2D;
class UCanvas;
class APlayerController;
class IImageWrapperModule;
struct FLogiStreamView;

// Opaque Rust world handles (logi_geo_ffi.h). Forward-declared so this header stays free of the C ABI;
// the .cpp includes the real header under LOGI_BEVY_HAVE_GEO. Exactly one is non-null per `Source`.
struct LogiBakedWorld;
struct LogiGlobeWorld;
struct LogiGlobeFrame; // FFI poll result — passed by const-ref to the globe reconcile helpers (delta protocol)
struct LogiGlobeTile;  // FFI per-tile descriptor — passed by const-ref to BuildGlobeTile

/** Which Rust terrain/tile source a Logi Streamed Tiles Terrain actor streams. */
UENUM(BlueprintType)
enum class ETerrainSource : uint8
{
	/** Generate terrain from the canonical fractal field (the surface gameplay stands on). Always available. */
	Procedural  UMETA(DisplayName = "Procedural (canonical field)"),
	/** Read our offline-baked tile store from disk (the WIP in-dev tile system). */
	BakedTiles  UMETA(DisplayName = "Baked tiles (store)"),
	/** Stream Google Photorealistic 3D Tiles over Cesium ion. Needs a CESIUM_ION_TOKEN. */
	GoogleTiles UMETA(DisplayName = "Google 3D Tiles (Cesium ion)")
};

/** How Google-tile materials are lit. P3DT bakes real-world sun/shadow into the albedo and marks every
 *  material `KHR_materials_unlit`, so the default (Auto) renders them UNLIT — applying engine lighting on top
 *  double-lights them (washed-out / muddy). Force-Lit needs an authored lit globe material; without one it
 *  falls back to the unlit material. */
UENUM(BlueprintType)
enum class EGoogleTileLighting : uint8
{
	/** Honour each tile's `KHR_materials_unlit` flag (P3DT tiles are all unlit ⇒ unlit). Recommended. */
	Auto       UMETA(DisplayName = "Auto (from KHR_materials_unlit)"),
	/** Always render Google tiles unlit (emissive base colour), regardless of the per-tile flag. */
	ForceUnlit UMETA(DisplayName = "Force Unlit"),
	/** Always render lit (needs an authored M_LogiGlobeTileLit; falls back to unlit if absent). */
	ForceLit   UMETA(DisplayName = "Force Lit")
};

/** Tile-fetch transport for Google tiles (ADR-0014 I/O lane; FFI LogiGlobeCreateInfo.fetch_backend, ABI v11).
 *  The CyperSource lane negotiates HTTP/3 over QUIC (compio-quic = quinn-proto on the compio reactor, no
 *  tokio) for Google P3DT via Alt-Svc auto-upgrade, with HTTP/2 as the bootstrap + fallback; it is the default
 *  (Auto) when the logi_geo DLL was built with the cyper-h2 feature. Force ureq / Force Cyper pin a backend for
 *  the live A/B. The enum order MUST match LOGI_GLOBE_FETCH_BACKEND_* in logi_geo_ffi.h (0 = Auto, 1 = ureq,
 *  2 = Cyper) — it is cast straight to LogiGlobeCreateInfo.fetch_backend. */
UENUM(BlueprintType)
enum class ELogiFetchBackend : uint8
{
	/** CyperSource (HTTP/3/QUIC) for Google P3DT when the DLL carries the cyper-h2 lane; ureq HttpSource otherwise. */
	Auto    UMETA(DisplayName = "Auto (HTTP/3/QUIC for Google P3DT)"),
	/** Force the blocking ureq HttpSource worker pool (the A/B's HTTP/1.1 arm). */
	Ureq    UMETA(DisplayName = "Force ureq (HTTP/1.1 pool)"),
	/** Force the CyperSource lane (HTTP/3 over QUIC; no effect if the DLL lacks cyper-h2 — degrades to ureq, logged). */
	Cyper   UMETA(DisplayName = "Force HTTP/3 (CyperSource/QUIC)")
};

/** A Geo debug visualization mode. Each non-`None` mode draws every resident tile as a FLAT, UNLIT colour
 *  blended white (low) → red (high) by a per-tile "quality" axis — with the real textures SUPPRESSED — so you
 *  can see refinement / streaming health at a glance. Selecting a mode (or switching between them) recolours
 *  the live tiles on the next frame; it does NOT rebuild the Rust world. Editor / PIE only (the heatmap
 *  material is built at runtime, which needs the shader compiler). */
UENUM(BlueprintType)
enum class EGeoDebugView : uint8
{
	/** Off — normal textured rendering. */
	None             UMETA(DisplayName = "None (normal render)"),
	/** Colour by refinement tree depth: red = the deepest / most-refined tiles, white = the coarsest. The
	 *  direct read on "is this region fully refined?". */
	RefinementDepth  UMETA(DisplayName = "Refinement Depth (red = deepest)"),
	/** Colour by 3D Tiles geometric error (metres): red = the finest (lowest error), white = the coarsest.
	 *  The canonical OGC quality measure; a log scale over the in-view range. */
	GeometricError   UMETA(DisplayName = "Geometric Error (red = finest)"),
	/** Colour by streaming/texture state: red = the full-res atlas has decoded & is resident, white = a
	 *  placeholder / not-yet-loaded tile (warming through the decode attempts in between). Streaming health. */
	StreamingState   UMETA(DisplayName = "Streaming State (red = textured)"),
	/** Colour by screen-space error (pixels), derived from geometric error + camera: red = the tile is at or
	 *  over the pixel-error target (refinement straining), white = comfortably under. Where LOD is struggling. */
	ScreenSpaceError UMETA(DisplayName = "Screen-Space Error (red = at target)")
};

/** Colour ramp for the debug views. */
UENUM(BlueprintType)
enum class EGeoDebugPalette : uint8
{
	/** The requested white → red blend (white = low quality, red = high). */
	WhiteToRed UMETA(DisplayName = "White -> Red"),
	/** A richer perceptual ramp (blue → cyan → green → yellow → red) for finer discrimination. */
	Turbo      UMETA(DisplayName = "Turbo (blue->green->red)")
};

/**
 * A **placeable** streamed-tiles terrain actor: drop one into a level to render terrain from any of three
 * Rust sources — the canonical procedural field, our offline-baked tile store, or streamed Google
 * Photorealistic 3D Tiles (Cesium ion). It owns its settings (this Details panel), drives the streaming +
 * meshing itself each tick through the native logi_geo FFI, and is a render/host SHELL over the Rust-owned
 * tiles — not gameplay authority (ADR-0005/0007). One host actor for the whole AOI (not one per tile), so
 * it stays within the actorless-terrain rule.
 *
 * Editing any property reloads this actor live (PostEditChangeProperty -> ReloadTerrain), in the editor
 * viewport and in PIE. Saved with the level, so its settings persist per level. The Google-tiles source
 * reads its Cesium ion token from `$CESIUM_ION_TOKEN` or `<Project>/Intermediate/CesiumIonToken.txt`
 * (never committed) — without one it logs a clear error and renders nothing.
 */
/**
 * One sample of everything the geo bench harness records per screenshot (Docs/Testing/Geo-Bench.md):
 * the streaming pipeline's full state (so a wedge is attributable to a specific stage), the cache
 * effectiveness counters, aggregate geometry actually uploaded, and the engine frame costs. Plain data —
 * filled by ALogiBakedGeoTerrain::GetBenchMetrics, serialized to the screenshot's JSON sidecar by the
 * LogiBevyEditor bench runner.
 */
struct FLogiGeoBenchMetrics
{
	// Streaming pipeline (Rust side via FFI + this actor's bookkeeping). A healthy converged station is
	// InFlight == Decoding == PendingTexture == PendingBinds == 0 and Textured ~= Built.
	int32 Rendered = 0;        // tiles in the last published frame (visible + hidden prepares)
	int32 Resident = 0;        // decoded tiles resident in Rust
	int32 Built = 0;           // tiles with a live C++ component (includes Retained)
	int32 Retained = 0;        // departed-grace tiles (left the cut, kept hidden+textured pending expiry)
	int32 VisibleComps = 0;    // components currently shown (the actual draw set)
	int32 InFlight = 0;        // outstanding network fetches
	int32 Decoding = 0;        // atlas decodes outstanding (dispatched, result not yet drained)
	int32 PendingTexture = 0;  // built-but-untextured tiles in the bounded re-drive set
	int32 PendingBinds = 0;    // uploads enqueued, awaiting GPU-resident confirmation
	int32 AwaitingConfirm = 0; // keys gated behind an in-flight upload confirm
	int32 Fading = 0;          // tiles mid focus-pull reveal
	int32 Textured = 0;        // keys reported textured to the Rust gate
	uint64 FrameId = 0;        // last applied publish id
	// Failure counters (cumulative; the four reasons a region can stick coarse).
	uint32 FetchFailed = 0, DecodeFailed = 0, GraftFailed = 0, UrlResolveFailed = 0;
	// THE TWO CACHES (v17): memory (flash) + disk (one cell over its byte + decoded-geometry tiers).
	// hits/misses cumulative; *MB = occupancy (disk = both tiers).
	uint64 FlashHits = 0, FlashMisses = 0, DiskHits = 0, DiskMisses = 0;
	uint64 DiskCachedMB = 0;
	uint32 FlashMB = 0; // the memory cell's occupancy
	// Aggregate uploaded geometry (sum over built tiles; Visible* = currently-shown subset).
	uint64 BuiltVerts = 0, BuiltTris = 0, VisibleVerts = 0, VisibleTris = 0;
	// Geo step game-thread cost (rolling 30-step window avg/peak, ms).
	double GeoStepAvgMs = 0.0, GeoStepPeakMs = 0.0;
	// Engine frame: RHI draw calls + primitives last frame, and the thread/GPU times (ms).
	int32 DrawCalls = 0;
	int64 PrimitivesDrawn = 0;
	double GameThreadMs = 0.0, RenderThreadMs = 0.0, GpuMs = 0.0;
	// Texture pipeline outcome counters (cumulative this session) — added to attribute the texture wedge
	// (2026-06-10: textured froze while 20 workers spun in BC7) to a specific stage. Healthy steady state:
	// DecodeDispatched ~= DecodeJobOk ~= UploadConfirmed and the failure counters flat.
	uint32 DecodeDispatched = 0;  // DispatchTileDecode jobs handed to the pool
	uint32 DecodeJobOk = 0;       // worker decodes that produced a mip chain
	uint32 DecodeJobEmpty = 0;    // worker decodes that returned empty (bad/unsupported image)
	uint32 DrainDropEvicted = 0;  // completed decodes dropped: tile gone by drain time
	uint32 DrainDropDup = 0;      // completed decodes dropped: already textured / bind in flight
	uint32 UploadConfirmed = 0;   // binds confirmed GPU-resident (the only path to Textured)
	uint32 UploadNoRhi = 0;       // upload aborted: no texture resource / RHI texture
	uint32 UploadMipMismatch = 0; // upload incomplete: source chain vs RHI mip-count mismatch
	uint32 UploadShortMip = 0;    // upload incomplete: short/empty source mip
	uint32 UploadLockFailed = 0;  // upload incomplete: LockTexture2D returned null (heap pressure)
	uint32 UploadRetried = 0;     // bounded upload retries issued
	uint32 UploadSurrendered = 0; // binds recycled after the retry cap (tile back to re-drive)
	// Pool pressure (ABI v13 — the 2026-06-11 starvation stall's signature: FreeSlots 0, PoolStarved
	// climbing, EvictedTotal flat at a full pool).
	uint32 FreeSlots = 0;         // Rust pool admission headroom as of the last step
	uint32 ResidentCap = 0;       // the LRU evict target the pool is sized above
	uint32 EvictedTotal = 0;      // cumulative Resident->Free LRU reclaims
	uint32 PoolStarved = 0;       // cumulative admissions dropped for want of a free slot
	// The selection X-ray (ABI v14): SNAPSHOT of the last full selection step's hold reasons — why each
	// coarser-than-SSE region is held there. A stuck-coarse SCAR = HoldLeafUnderSse / FetchSkipGrafted
	// sustained at quiescence; HoldKick/HoldLoading are normal convergence and fall to zero at settle.
	uint32 HoldKick = 0;          // coarse stand-ins held until their subtree is fully renderable
	uint32 HoldDead = 0;          // dead footprints covered by an ancestor (terminal failures below)
	uint32 HoldLoading = 0;       // unloaded tiles queued this pass (covered by an ancestor meanwhile)
	uint32 HoldLeafUnderSse = 0;  // LOADED tiles coarser than wanted with NOTHING to refine into (scar)
	uint32 HoldPendingSubtree = 0; // implicit children waiting on an unfetched .subtree
	uint32 FetchSkipGrafted = 0;  // load candidates short-circuited by a graft record (stale = scar)
};

UCLASS(HideCategories = (Physics, Collision, LOD, Cooking, Replication, Networking, Input, Actor,
	HLOD, WorldPartition, DataLayers, Navigation, Mobile, RayTracing, TextureStreaming, Activation, Streaming,
	Tags, ComponentReplication, Events, AssetUserData, ComponentTick),
	meta = (DisplayName = "Logi Streamed Tiles Terrain"))
class LOGIBEVYRUNTIME_API ALogiBakedGeoTerrain : public AActor
{
	GENERATED_BODY()

public:
	ALogiBakedGeoTerrain();

	// ---- Source ----------------------------------------------------------------------------------------

	/** Where this terrain comes from: the procedural field, a baked tile store, or streamed Google tiles. */
	UPROPERTY(EditAnywhere, Category = "Terrain", meta = (DisplayName = "Source"))
	ETerrainSource Source = ETerrainSource::Procedural;

	/** Baked tile-store directory. A relative path resolves under the project's Saved folder (e.g.
	 *  "LogiTiles/hero"); an absolute path is used as-is. Produced by the offline terrain bake. */
	UPROPERTY(EditAnywhere, Category = "Terrain", meta = (DisplayName = "Tile Store Directory", EditCondition = "Source == ETerrainSource::BakedTiles"))
	FString TileStoreDir = TEXT("LogiTiles/hero");

	// ---- Origin (procedural + Google tiles; the baked store carries its own) ---------------------------

	/** Area-of-interest / rebase origin latitude (degrees). Used by the procedural and Google-tiles
	 *  sources; the baked store carries its own origin. */
	UPROPERTY(EditAnywhere, Category = "Terrain", meta = (DisplayName = "Origin Latitude", ClampMin = -90.0, ClampMax = 90.0, EditCondition = "Source != ETerrainSource::BakedTiles"))
	double OriginLatitude = 39.643178;

	/** Area-of-interest / rebase origin longitude (degrees). */
	UPROPERTY(EditAnywhere, Category = "Terrain", meta = (DisplayName = "Origin Longitude", ClampMin = -180.0, ClampMax = 180.0, EditCondition = "Source != ETerrainSource::BakedTiles"))
	double OriginLongitude = -75.617567;

	/** Origin height (metres above the WGS84 ellipsoid). */
	UPROPERTY(EditAnywhere, Category = "Terrain", meta = (DisplayName = "Origin Height (m)", EditCondition = "Source != ETerrainSource::BakedTiles"))
	double OriginHeight = 0.0;

	// ---- Meshing (procedural + baked; Google tiles arrive pre-meshed) ----------------------------------

	/** Meshing mode: a uniform grid, or an adaptive triangulation (fewer triangles on flat ground, denser
	 *  on slopes). Applies to our mesher (procedural / baked); Google tiles arrive already meshed. */
	UPROPERTY(EditAnywhere, Category = "Terrain", meta = (DisplayName = "Meshing Mode", EditCondition = "Source != ETerrainSource::GoogleTiles"))
	EBakedTerrainMeshing MeshingMode = EBakedTerrainMeshing::Uniform;

	/** Adaptive-meshing maximum vertical error (metres): higher values produce fewer triangles. Ignored in
	 *  uniform mode. */
	UPROPERTY(EditAnywhere, Category = "Terrain", meta = (DisplayName = "Adaptive Error (m)", ClampMin = 0.0, EditCondition = "Source != ETerrainSource::GoogleTiles && MeshingMode == EBakedTerrainMeshing::TIN"))
	float TinErrorM = 1.0f;

	/** Edge-skirt depth (metres): a downward wall on each tile border that hides seams between adjacent
	 *  tiles. 0 = no skirt. */
	UPROPERTY(EditAnywhere, Category = "Terrain", meta = (DisplayName = "Edge Skirt Depth (m)", ClampMin = 0.0, EditCondition = "Source != ETerrainSource::GoogleTiles"))
	float SkirtDepthM = 2.0f;

	// ---- Level of detail (all sources) -----------------------------------------------------------------

	/** Level-of-detail target in screen-space error (pixels): lower pulls finer tiles near the camera;
	 *  coarse tiles are used in the distance. (The globe lane clamps Detail Target to 1–64 px before pushing it
	 *  to Rust.) */
	UPROPERTY(EditAnywhere, Category = "Terrain", meta = (DisplayName = "Detail Target (px)", ClampMin = 1.0, ClampMax = 64.0))
	float DetailTargetPx = 6.0f; // 6px (was 8): finer near-camera ground — refinement descends ~one-third of a
	                             // level deeper before settling on a coarse ancestor, the direct lever against the
	                             // "blurry coarse parent" report. ~1.8× the 8px tile count → paired with the raised
	                             // pool/resident caps (logi_geo_ffi). Lower toward 4–5 for more if RAM/pool allow.

	/** Hard ceiling on new tiles meshed/built per step (the geo step now runs EVERY frame, not on a ~30 Hz
	 *  batch — see Tick). Each build is O(1) (the decode + mip + BC7 compress run OFF the game thread; each tile owns a
	 *  1-section component, so no whole-set proxy rebuild). This count is the *upper* bound; the real per-frame
	 *  throttle is `GeoBuildBudgetMs` below (whichever hits first wins), so this can stay generous — it only
	 *  caps a single frame's catch-up after a long stall. */
	UPROPERTY(EditAnywhere, Category = "Terrain", meta = (DisplayName = "Tile Builds Per Step", ClampMin = 1, ClampMax = 1024))
	int32 TileBuildsPerStep = 32;

	/** Max atlas GPU uploads enqueued per step (Google-tiles lane). Bounds how much the D3D12 upload heap is asked
	 *  for in one frame so a region-entry burst can't cause correlated LockTexture2D failures (the cause of the
	 *  tile-corruption band); the surplus decodes wait for the next step. */
	UPROPERTY(EditAnywhere, Category = "Terrain|Google Tiles", meta = (DisplayName = "Texture Uploads Per Step", ClampMin = 1, ClampMax = 1024))
	int32 GlobeTextureUploadsPerStep = 48;

	/** Cap on recycled atlas textures kept warm in the pool (Google-tiles lane). These are NeverStream (they bypass
	 *  UE's texture streamer), so without a cap the pool's VRAM grows unbounded across a long fly-around; past the
	 *  cap a recycled texture is dropped (GC reclaims). The resident set is bounded separately by the cull radius. */
	UPROPERTY(EditAnywhere, Category = "Terrain|Google Tiles", meta = (DisplayName = "Max Pooled Textures", ClampMin = 0, ClampMax = 65536))
	int32 GlobeMaxPooledTextures = 4096;

	/** LOD reveal (focus-pull): a refined-in tile is shown only once it's textured (the Rust texture gate holds its
	 *  coarse parent over it until then — invisible loading) and crisps in from soft to sharp instead of popping.
	 *  Off = instant sharp swap once textured. */
	UPROPERTY(EditAnywhere, Category = "Terrain|Google Tiles|Fade", meta = (DisplayName = "Tile Reveal Focus-Pull"))
	bool bGlobeTileFade = true;
	/** Seconds for a tile to focus-pull in (soft→sharp) once shown. ~0.1 s keeps the reveal quick + imperceptible. */
	UPROPERTY(EditAnywhere, Category = "Terrain|Google Tiles|Fade", meta = (DisplayName = "Reveal Seconds", ClampMin = 0.05, ClampMax = 3.0, EditCondition = "bGlobeTileFade"))
	float GlobeFadeInSec = 0.1f;
	/** Focus-pull START softness: mips coarser than sharp a tile begins at. Higher = softer start (matches a coarser
	 *  parent → hides a bigger LOD jump) but a longer-looking crisp-up. ~3 ≈ the coarse parent's apparent detail. */
	UPROPERTY(EditAnywhere, Category = "Terrain|Google Tiles|Fade", meta = (DisplayName = "Reveal Soft Mip Bias", ClampMin = 0.0, ClampMax = 6.0, EditCondition = "bGlobeTileFade"))
	float GlobeFadeSoftBias = 3.0f;
	/** Ease-out exponent for the bloom curve (1 = linear; higher = snappier start, softer settle). */
	UPROPERTY(EditAnywhere, Category = "Terrain|Google Tiles|Fade", meta = (DisplayName = "Fade Ease Exponent", ClampMin = 1.0, ClampMax = 6.0, EditCondition = "bGlobeTileFade"))
	float GlobeFadeEaseExp = 2.5f;

	/** Per-FRAME elapsed-time budget (ms) for the whole geo game-thread step — the texture-upload drain AND the
	 *  mesh-build loop share it, measured together from the top of the step. Because the step now runs every
	 *  frame (not every ~33 ms), the work spreads as many small slices instead of one heavy 30 Hz burst: each
	 *  frame does a *little* and yields the moment it crosses this budget (the drain runs first so already-decoded
	 *  textures land promptly, then builds fill whatever time is left). This auto-adapts to frame rate — a fast
	 *  machine simply gets more slices per second. Raise it to drain a backlog faster at the cost of a bigger
	 *  per-frame slice; lower it to keep geo off the frame's critical path. Watch the "Geo-step game-thread cost
	 *  (avg/max ms)" log: max should track this value. */
	UPROPERTY(EditAnywhere, Category = "Terrain", meta = (DisplayName = "Geo Build Budget (ms/frame)", ClampMin = 0.1, ClampMax = 8.0))
	float GeoBuildBudgetMs = 2.0f;

	/** Maximum tiles torn down per step, so a camera move that departs hundreds of tiles can't evict them all
	 *  in one frame. Each eviction is O(1) (a per-tile component recycled to the pool), so this can be generous. */
	UPROPERTY(EditAnywhere, Category = "Terrain", meta = (DisplayName = "Tile Evictions Per Step", ClampMin = 1, ClampMax = 512))
	int32 TileEvictionsPerStep = 32;

	/** Departed-grace retention (2026-06-10 oscillation fix): a tile that leaves the published cut is HIDDEN and
	 *  RETAINED — component, geometry, texture AND its TexturedKeys report — for this long before it is recycled.
	 *  Instant recycle un-textured the tile in the Rust gate's view, which re-held its ancestor, which flipped the
	 *  cut back: a self-sustaining C++<->Rust oscillation that re-decoded the flap edge at ~800 tiles/s of BC7,
	 *  starved the (LIFO) worker pool, and froze convergence (geo-bench run 2026-06-10 texpipe-counters:
	 *  118k dispatches / 117k confirms for ~900 built, textured pinned at ~25). Retention makes departure
	 *  side-effect-free: a returning tile re-shows instantly, no re-decode, no gate churn. */
	UPROPERTY(EditAnywhere, Category = "Terrain", meta = (DisplayName = "Departed Tile Retain (s)", ClampMin = 0.0, ClampMax = 120.0))
	float TileRetainSeconds = 10.0f;

	/** Cap on retained departed tiles (memory bound: each holds its mesh payload + BC7 atlas, ~0.2 MB typical).
	 *  Over budget, the oldest are recycled first regardless of age. 4096 (was 2048): a ground teleport's
	 *  descent retires enough tiles to PIN 2048, and a saturated grace cache force-recycles early — early
	 *  TexturedKeys withdrawal is what re-arms the gate-feedback churn (geo-bench select-distant-floor run:
	 *  new_castle disp=112k, drop_evicted=30k, retained pinned). */
	UPROPERTY(EditAnywhere, Category = "Terrain", meta = (DisplayName = "Departed Tile Retain Budget", ClampMin = 0, ClampMax = 16384))
	int32 TileRetainBudget = 4096;

	// ---- Procedural source params ----------------------------------------------------------------------

	/** Procedural field seed. 0 = the canonical gameplay field, so the rendered terrain matches the
	 *  simulation surface. */
	UPROPERTY(EditAnywhere, Category = "Terrain|Procedural", meta = (DisplayName = "Seed (0 = canonical)", ClampMin = 0, EditCondition = "Source == ETerrainSource::Procedural"))
	int32 ProceduralSeed = 0;

	/** Area-of-interest radius (metres): the world is the 2*radius square centred on the origin. */
	UPROPERTY(EditAnywhere, Category = "Terrain|Procedural", meta = (DisplayName = "Radius (m)", ClampMin = 1.0, EditCondition = "Source == ETerrainSource::Procedural"))
	double ProceduralRadiusM = 1609.34;

	/** Pyramid leaf level (depth): levels 0..leaf, with 4^level tiles each. Built eagerly, so keep modest. */
	UPROPERTY(EditAnywhere, Category = "Terrain|Procedural", meta = (DisplayName = "Leaf Level", ClampMin = 0, ClampMax = 10, EditCondition = "Source == ETerrainSource::Procedural"))
	int32 ProceduralLeafLevel = 4;

	/** Height samples per tile edge (grid size N): N^2 samples per tile. */
	UPROPERTY(EditAnywhere, Category = "Terrain|Procedural", meta = (DisplayName = "Grid Size (N)", ClampMin = 2, ClampMax = 513, EditCondition = "Source == ETerrainSource::Procedural"))
	int32 ProceduralGridN = 33;

	// ---- Google 3D Tiles (Cesium ion) source params ----------------------------------------------------

	/** Cesium ion asset id to stream (2275207 = Google Photorealistic 3D Tiles). */
	UPROPERTY(EditAnywhere, Category = "Terrain|Google Tiles", meta = (DisplayName = "ion Asset Id", ClampMin = 1, EditCondition = "Source == ETerrainSource::GoogleTiles"))
	int64 GlobeIonAssetId = 2275207;

	/** Distance cull radius (metres) around the camera; caps the streamed extent. */
	UPROPERTY(EditAnywhere, Category = "Terrain|Google Tiles", meta = (DisplayName = "Max Tile Distance (m)", ClampMin = 100.0, EditCondition = "Source == ETerrainSource::GoogleTiles"))
	double GlobeMaxTileDistanceM = 6000.0;

	/** Worker threads for the tile fetch pool. */
	UPROPERTY(EditAnywhere, Category = "Terrain|Google Tiles", meta = (DisplayName = "Fetch Workers", ClampMin = 1, ClampMax = 64, EditCondition = "Source == ETerrainSource::GoogleTiles"))
	int32 GlobeFetchWorkers = 16; // ClampMax unified with the Rust FFI cap (info.workers.clamp(1, 64)); the
	                              // priority-heap scheduler keeps a deeper pool priority-ordered, so raising
	                              // toward 64 just fills a new view / teleport faster (no FIFO-inversion risk).

	/** Anchor the level's SkyAtmosphere planet model to the streamed WGS84 globe at world create:
	 *  PlanetCenterAtComponentTransform at the true earth center + the origin's geocentric ground radius
	 *  (the VolumetricCloud layer follows the atmosphere's planet). Without this the default sky is a
	 *  6,360 km sphere pinned at the UE origin — ±15 km off the real terrain, varying by latitude — which
	 *  drops the cloud deck onto low-altitude cameras and detaches the atmospheric limb from the horizon.
	 *  The anchor numbers come from the Rust kernel geodesy (ABI v15 sky-anchor accessor). */
	UPROPERTY(EditAnywhere, Category = "Terrain|Google Tiles", meta = (DisplayName = "Anchor Sky To Planet", EditCondition = "Source == ETerrainSource::GoogleTiles"))
	bool bAnchorSkyToPlanet = true;

	/** Keep the sun at the CAMERA's zenith (local solar noon wherever the camera is). A planet-true
	 *  atmosphere (Anchor Sky To Planet) makes a static sun direction put most of the globe in night or
	 *  twilight — the camera flies into a deep-blue inscatter wash with a red horizon band the moment it
	 *  leaves the sun's hemisphere. P3DT albedo bakes ~noon sun/shadow, so zenith sun everywhere is also the
	 *  most albedo-consistent sky. Re-orients the level's DirectionalLight as the camera moves (promoted to
	 *  Movable); a real time-of-day model can replace this later. */
	UPROPERTY(EditAnywhere, Category = "Terrain|Google Tiles", meta = (DisplayName = "Sun At Camera Zenith", EditCondition = "Source == ETerrainSource::GoogleTiles && bAnchorSkyToPlanet"))
	bool bSunAtCameraZenith = true;

	/** Keep a persistent on-disk tile cache (gitignored Saved/P3DTCache) so revisited areas load from disk
	 *  instead of re-downloading. NOTE: persistently caching Google Photorealistic 3D Tiles violates Google's
	 *  Map Tiles terms (tiles may only be cached for the live session) — use only for self-hosted tilesets. */
	UPROPERTY(EditAnywhere, Category = "Terrain|Google Tiles", meta = (DisplayName = "Persistent Disk Cache", EditCondition = "Source == ETerrainSource::GoogleTiles"))
	bool bGlobeDiskCache = false;

	/** Persistent disk-cache size budget (GB); the coldest tiles are evicted past this. Default 200 GB — the
	 *  P3DT planet is huge, and disk is cheap relative to re-downloading; a big cache makes revisited areas
	 *  load from disk instead of the network (LRU by last-use; survives restarts). */
	UPROPERTY(EditAnywhere, Category = "Terrain|Google Tiles", meta = (DisplayName = "Disk Cache Budget (GB)", ClampMin = 1, ClampMax = 4096, EditCondition = "Source == ETerrainSource::GoogleTiles && bGlobeDiskCache"))
	int32 GlobeCacheBudgetGB = 200;

	/** In-memory decoded-tile cache budget (MB): the warm-restore buffer of decoded meshes+textures held in
	 *  front of the resident set, so a camera reversing/orbiting restores tiles WITHOUT re-fetch or re-decode.
	 *  Larger = smoother revisits at the cost of RAM. 0 = use the engine default. */
	UPROPERTY(EditAnywhere, Category = "Terrain|Google Tiles", meta = (DisplayName = "Flash Cache Budget (MB)", ClampMin = 0, ClampMax = 16384, EditCondition = "Source == ETerrainSource::GoogleTiles"))
	int32 GlobeFlashCacheBudgetMB = 1024; // larger warm-restore buffer so orbit/zoom re-entry restores without re-decode

	/** Lighting model for Google tiles. Auto honours each tile's `KHR_materials_unlit` flag (P3DT is all
	 *  unlit ⇒ unlit), which avoids the double-lighting / washed-out look of shading already-lit photoreal
	 *  imagery. Force-Lit needs an authored M_LogiGlobeTileLit (falls back to unlit if absent). */
	UPROPERTY(EditAnywhere, Category = "Terrain|Google Tiles", meta = (DisplayName = "Tile Lighting", EditCondition = "Source == ETerrainSource::GoogleTiles"))
	EGoogleTileLighting GoogleTileLighting = EGoogleTileLighting::Auto;

	/** Tile-fetch transport (ADR-0014 I/O lane). Auto = HTTP/2 CyperSource for Google P3DT when the logi_geo
	 *  DLL was built with the cyper-h2 feature (the default), else the ureq pool; Force ureq / Force HTTP-2 pin
	 *  a backend for the live A/B. Set at world create only — changing it re-creates the streamed world. */
	UPROPERTY(EditAnywhere, Category = "Terrain|Google Tiles", meta = (DisplayName = "Fetch Backend", EditCondition = "Source == ETerrainSource::GoogleTiles"))
	ELogiFetchBackend GlobeFetchBackend = ELogiFetchBackend::Auto;

	// ---- Rendering -------------------------------------------------------------------------------------

	/** Build a Nanite static mesh per tile (native Nanite level-of-detail, GPU culling, virtual shadow
	 *  maps) instead of a procedural-mesh component. Editor / PIE only. Not used by the Google-tiles source
	 *  (its photoreal tiles are textured proc-mesh sections). */
	UPROPERTY(EditAnywhere, Category = "Terrain|Rendering", meta = (DisplayName = "Use Nanite Mesh Lane (Editor only)", EditCondition = "Source != ETerrainSource::GoogleTiles"))
	bool bUseNaniteMeshLane = false;

	/** Apply a displacement material to the terrain for close-up micro-detail (procedural / baked only). */
	UPROPERTY(EditAnywhere, Category = "Terrain|Rendering", meta = (DisplayName = "Use Displacement Material", EditCondition = "Source != ETerrainSource::GoogleTiles"))
	bool bUseDisplacementMaterial = false;

	/** Displacement material applied when the toggle above is on. A dynamic instance is created and its
	 *  "DisplacementScale" scalar parameter is set. */
	UPROPERTY(EditAnywhere, Category = "Terrain|Rendering", meta = (DisplayName = "Displacement Material", EditCondition = "Source != ETerrainSource::GoogleTiles && bUseDisplacementMaterial"))
	TSoftObjectPtr<UMaterialInterface> DisplacementMaterial;

	/** Displacement magnitude (centimetres) fed to the material's "DisplacementScale" parameter. */
	UPROPERTY(EditAnywhere, Category = "Terrain|Rendering", meta = (DisplayName = "Displacement Scale (cm)", ClampMin = 0.0, EditCondition = "Source != ETerrainSource::GoogleTiles && bUseDisplacementMaterial"))
	float DisplacementScale = 20.0f;

	// ---- Debug visualization (all sources) -------------------------------------------------------------
	// These are HOT-APPLIED: changing them recolours / re-reads on the next frame and never rebuilds the Rust
	// world (unlike the source/geometry settings above). The heatmaps need the per-tile depth + geometric
	// error the Geo FFI now carries (ABI 7); they are editor/PIE only (the colour material is built at runtime).

	/** Debug visualization mode. Non-`None` draws every tile as a flat white→red "quality" heatmap with the
	 *  real textures suppressed — switches live, no rebuild. See `EGeoDebugView`. */
	UPROPERTY(EditAnywhere, Category = "Terrain|Debug", meta = (DisplayName = "Debug View"))
	EGeoDebugView DebugView = EGeoDebugView::None;

	/** Colour ramp used by the debug views: the white→red blend, or a richer Turbo ramp. */
	UPROPERTY(EditAnywhere, Category = "Terrain|Debug", meta = (DisplayName = "Debug Palette", EditCondition = "DebugView != EGeoDebugView::None"))
	EGeoDebugPalette DebugPalette = EGeoDebugPalette::WhiteToRed;

	/** Refinement-Depth view only: the depth mapped to full red. 0 = auto (track the deepest tile in view).
	 *  Pin it to a fixed value for a stable colour scale that doesn't rescale as the camera moves. */
	UPROPERTY(EditAnywhere, Category = "Terrain|Debug", meta = (DisplayName = "Debug Max Depth (0 = auto)", ClampMin = 0, ClampMax = 32, EditCondition = "DebugView == EGeoDebugView::RefinementDepth"))
	int32 DebugMaxDepth = 0;

	/** Draw a live telemetry overlay (tiles rendered / resident / loading, pool sizes, geo-step ms, …) in the
	 *  viewport. The same numbers also mirror into the read-only Telemetry fields below. */
	UPROPERTY(EditAnywhere, Category = "Terrain|Debug", meta = (DisplayName = "Show Debug Panel"))
	bool bShowDebugPanel = false;

	// ---- Live telemetry (read-only; refreshed every tick while a world is live) -------------------------
	// A snapshot of the streaming state, mirrored from the same source the viewport overlay draws. Read-only
	// (VisibleAnywhere) + Transient (never saved). The Details panel repaints periodically, so these track the
	// live numbers a beat behind the viewport overlay.

	UPROPERTY(VisibleAnywhere, Transient, Category = "Terrain|Debug|Telemetry", meta = (DisplayName = "Status"))
	FString DbgStatus;
	/** Tiles in the frame Rust published this step (what's actually drawn). */
	UPROPERTY(VisibleAnywhere, Transient, Category = "Terrain|Debug|Telemetry", meta = (DisplayName = "Tiles Rendered"))
	int32 DbgTilesRendered = 0;
	/** Decoded tiles Rust holds resident (globe lane). */
	UPROPERTY(VisibleAnywhere, Transient, Category = "Terrain|Debug|Telemetry", meta = (DisplayName = "Tiles Resident"))
	int32 DbgTilesResident = 0;
	/** Tiles this actor has built render components for. */
	UPROPERTY(VisibleAnywhere, Transient, Category = "Terrain|Debug|Telemetry", meta = (DisplayName = "Tiles Built"))
	int32 DbgTilesBuilt = 0;
	/** Outstanding tile fetches right now (globe lane). */
	UPROPERTY(VisibleAnywhere, Transient, Category = "Terrain|Debug|Telemetry", meta = (DisplayName = "Tiles In-Flight"))
	int32 DbgTilesInFlight = 0;
	/** Atlas decodes in flight on the worker pool (globe lane). */
	UPROPERTY(VisibleAnywhere, Transient, Category = "Terrain|Debug|Telemetry", meta = (DisplayName = "Tiles Decoding"))
	int32 DbgTilesDecoding = 0;
	/** Built tiles whose full-res atlas has landed (globe lane). */
	UPROPERTY(VisibleAnywhere, Transient, Category = "Terrain|Debug|Telemetry", meta = (DisplayName = "Tiles Textured"))
	int32 DbgTilesTextured = 0;
	/** Recycle-pool occupancy: "comps / mids / textures". */
	UPROPERTY(VisibleAnywhere, Transient, Category = "Terrain|Debug|Telemetry", meta = (DisplayName = "Pools (comp/mid/tex)"))
	FString DbgPools;
	/** Geo game-thread step cost, rolling avg / max (ms). */
	UPROPERTY(VisibleAnywhere, Transient, Category = "Terrain|Debug|Telemetry", meta = (DisplayName = "Geo Step (avg/max ms)"))
	FString DbgGeoStepMs;
	/** Refinement depth range across the resident tiles ("min – max"). */
	UPROPERTY(VisibleAnywhere, Transient, Category = "Terrain|Debug|Telemetry", meta = (DisplayName = "Depth Range"))
	FString DbgDepthRange;
	/** Geometric-error range across the resident tiles ("min – max m"). */
	UPROPERTY(VisibleAnywhere, Transient, Category = "Terrain|Debug|Telemetry", meta = (DisplayName = "Geo Error Range"))
	FString DbgGeoErrorRange;
	/** Streaming give-ups: "fetch / decode / graft / url" (globe lane). */
	UPROPERTY(VisibleAnywhere, Transient, Category = "Terrain|Debug|Telemetry", meta = (DisplayName = "Failures (fetch/decode/graft/url)"))
	FString DbgFailures;
	/** Persistent disk-cache resident bytes (MB; globe lane). */
	UPROPERTY(VisibleAnywhere, Transient, Category = "Terrain|Debug|Telemetry", meta = (DisplayName = "Disk Cache (MB)"))
	int32 DbgCachedMB = 0;

	// --- THE TWO CACHES (Terrain|Debug|Telemetry|Caches — collapsible subtree; v17 two-cache model) -------
	/** The MEMORY cache (in-RAM decoded-mesh LRU, fed by every dropped resident mesh):
	 *  "hits / misses (rate%) | MB". A high rate = camera revisits served with no fetch + no decode;
	 *  occupancy saturated at the flash budget = budget too small, slack = oversized. */
	UPROPERTY(VisibleAnywhere, Transient, Category = "Terrain|Debug|Telemetry|Caches", meta = (DisplayName = "Memory (hit/miss | MB)"))
	FString DbgFlashCache;
	/** The DISK cache (ONE cell: raw-bytes + decoded-geometry tiers under one budget):
	 *  "hits / misses (rate%) | MB". A hit = any tier served (a geometry restore skipping the Draco decode,
	 *  or cached bytes skipping the network); a miss = the network was needed. */
	UPROPERTY(VisibleAnywhere, Transient, Category = "Terrain|Debug|Telemetry|Caches", meta = (DisplayName = "Disk (hit/miss | MB)"))
	FString DbgDiskCache;

	// --- Pool pressure (Terrain|Debug|Telemetry|Pool — collapsible subtree) -------------------------------
	/** Rust tile-pool admission headroom: "free / resident / cap". free = 0 means every new tile is being
	 *  DROPPED (the 2026-06-11 starvation stall: zoom in and nothing loads). */
	UPROPERTY(VisibleAnywhere, Transient, Category = "Terrain|Debug|Telemetry|Pool", meta = (DisplayName = "Slots (free/resident/cap)"))
	FString DbgPoolSlots;
	/** Cumulative LRU evictions vs slot-starved admissions: "evicted / starved". starved climbing while
	 *  evicted stays flat at a full pool = eviction is dead and the streamer is starving. */
	UPROPERTY(VisibleAnywhere, Transient, Category = "Terrain|Debug|Telemetry|Pool", meta = (DisplayName = "Evicted / Starved"))
	FString DbgPoolEvict;
	/** Departed-grace tiles retained C++-side (hidden, fully built, pending expiry/budget). */
	UPROPERTY(VisibleAnywhere, Transient, Category = "Terrain|Debug|Telemetry|Pool", meta = (DisplayName = "Retained (departed grace)"))
	int32 DbgRetained = 0;

	// --- Selection X-ray (Terrain|Debug|Telemetry|Selection — collapsible subtree) ------------------------
	/** Why coarser-than-SSE regions are held there, last full selection step (snapshot):
	 *  "kick / dead / loading / leaf / subtree / graft-skip". A stuck-coarse SCAR = leaf or graft-skip
	 *  sustained while the pipeline is quiescent; kick/loading are normal convergence and settle to zero. */
	UPROPERTY(VisibleAnywhere, Transient, Category = "Terrain|Debug|Telemetry|Selection", meta = (DisplayName = "Holds (kick/dead/load/leaf/sub/graft)"))
	FString DbgHolds;

	// --- Texture pipeline (Terrain|Debug|Telemetry|Textures — collapsible subtree) ------------------------
	/** Atlas decode jobs: "dispatched / decoded / confirmed". The three should track each other; a widening
	 *  dispatched−confirmed gap = re-decode churn or a wedged confirm path. */
	UPROPERTY(VisibleAnywhere, Transient, Category = "Terrain|Debug|Telemetry|Textures", meta = (DisplayName = "Decodes (disp/ok/conf)"))
	FString DbgTexPipeline;
	/** Completed decodes dropped at the drain: "evicted / duplicate" — high evicted = tile lifecycle churn. */
	UPROPERTY(VisibleAnywhere, Transient, Category = "Terrain|Debug|Telemetry|Textures", meta = (DisplayName = "Drain Drops (evict/dup)"))
	FString DbgTexDrops;

	// ---- AActor ----------------------------------------------------------------------------------------

	virtual void Tick(float DeltaTime) override;
	virtual bool ShouldTickIfViewportsOnly() const override { return true; } // stream in the editor viewport
	virtual void EndPlay(const EEndPlayReason::Type EndPlayReason) override;
	virtual void BeginDestroy() override;
#if WITH_EDITOR
	virtual void PostEditChangeProperty(struct FPropertyChangedEvent& PropertyChangedEvent) override;
#endif

	/** Fill `Out` with the current bench metrics sample (see FLogiGeoBenchMetrics). Cheap (O(built) sums +
	 *  a handful of FFI atomic reads); callable every tick by the LogiBevyEditor bench runner. Returns false
	 *  when no globe world is live (metrics then carry only the engine-frame fields). */
	bool GetBenchMetrics(FLogiGeoBenchMetrics& Out);

private:
	/** One resident tile. Each tile owns its OWN render component so adding/removing a tile only touches that
	 *  tile's tiny scene proxy — never the whole-component O(n) proxy recreate a shared mesh component forces on
	 *  every section add/remove (the source of the multi-second streaming hitches). Components, MIDs and atlases
	 *  are recycled through the pools below, so after warm-up streaming churns zero UObjects. */
	/** Per-tile LOD cross-fade state (the "rack-focus bloom" transition). None = fully resident (no per-frame work);
	 *  WaitTexture = built but invisible, waiting for its atlas to confirm before blooming (a coarse held tile covers
	 *  it meanwhile); FadingIn = dithering in via the material's FadeAlpha opacity mask. */
	enum class EGlobeFade : uint8 { None, WaitTexture, FadingIn };

	struct FTileEntry
	{
		TStrongObjectPtr<ULogiTileMeshComponent> ProcComp;     // per-tile render shell (globe + baked non-Nanite)
		TStrongObjectPtr<UStaticMeshComponent> NaniteComp;     // per-tile static mesh (Nanite lane)
		TStrongObjectPtr<UMaterialInstanceDynamic> Material;   // per-tile MID (Google-tiles lane)
		TStrongObjectPtr<UTexture2D> Texture;                  // per-tile decoded atlas (Google-tiles lane)
		uint8 TextureAttempts = 0;                             // atlas decode dispatches so far (bounds re-drive retries)
		uint8 ClipBoxCount = 0;                                // hole-clip OBBs APPLIED to the MID (0 when the clip material is unset); >0 ⇒ on the MASKED clip base
		uint32 ClipHash = 0;                                   // CRC of the applied clip set — RefreshTileClips skips a bit-identical re-push

		// Debug-visualization metadata (Geo FFI ABI 7): the per-tile refinement quality the heatmaps colour by,
		// cached at build (intrinsic to a tile, so set once). `DebugMid` is the per-tile flat-colour MID, held
		// only while a debug view is active and recycled to DebugMidPool when it turns off.
		uint16 Depth = 0;                                      // refinement tree depth / pyramid level (root = 0)
		float GeometricError = 0.0f;                           // 3D Tiles geometric error (metres; lower = finer)
		FVector3f BoundsCenter = FVector3f::ZeroVector;        // tile centre (UE cm) — for the screen-space-error view
		TStrongObjectPtr<UMaterialInstanceDynamic> DebugMid;   // per-tile debug heatmap MID (only while a view is active)

		// LOD cross-fade: a refined-in tile blooms in via a dithered opacity mask (FadeAlpha 0→1) once its atlas
		// is confirmed; the Rust texture gate keeps the coarse stand-in rendered (and hole-clipped) under it, so
		// the bloom is always covered — no pop, no hole, no gray flash. FadeMode None ⇒ resident, no per-frame
		// work. (A C++-side "hold the refined-away parent" lane existed before the gate + hole-clip subsumed it.)
		EGlobeFade FadeMode = EGlobeFade::None;
		float FadeT = 1.0f;          // bloom progress: 0 = invisible/just-appeared, 1 = fully in
	};

	/** A worker-decoded tile atlas (JPEG -> RGBA8 -> BC7 + full mip chain), handed back to the game thread to
	 *  become a UTexture2D. The decode + box-filter + BC7 compress run OFF the game thread; the game thread only
	 *  creates/acquires the UObject — the BC7 mip-byte upload is then enqueued to the RENDER thread (audit T4). */
	struct FLogiTileDecode
	{
		uint64 Key = 0;
		int32 W = 0;
		int32 H = 0;
		TArray<TArray64<uint8>> Mips; // mip 0..N-1, each tightly packed BC7/PF_BC7 (empty => decode failed)
	};

	/** The BC7 mip chain for one tile, owned by a thread-safe shared pointer so it can be handed across the
	 *  game/render-thread boundary (audit T4). The game thread creates/acquires the UTexture2D, then enqueues a
	 *  render command that copies these BC7 bytes into the live RHI mips — so this payload MUST outlive that
	 *  async command. Holding it shared (and capturing the shared ref in the render lambda) guarantees exactly
	 *  that: the bytes are freed only once the render thread has finished the upload and drops its reference.
	 *  Mirrors the lifetime discipline of the T1 mesh proxy's FLogiTilePayload and the JPEG-copy in DispatchTileDecode. */
	struct FLogiTileMipChain
	{
		int32 W = 0;
		int32 H = 0;
		TArray<TArray64<uint8>> Mips; // mip 0..N-1, each tightly packed BC7/PF_BC7 (moved out of FLogiTileDecode)
		// Render->game-thread handoff of the GPU upload RESULT: 0 = pending, 1 = every RHI mip got its real bytes
		// (safe to show), -1 = a mip was dropped (LockTexture2D failed under upload-heap pressure, or a short/empty
		// mip). The game thread (ProcessPendingTextureBinds) binds the atlas to the tile ONLY once this reads 1, so a
		// dropped upload is never shown as garbage/black or another tile's atlas — the tile keeps its gray placeholder.
		std::atomic<int32> UploadState{0};
	};

	/** Texture-pipeline outcome counters (cumulative this session) — the attribution data for a texture wedge
	 *  (see FLogiGeoBenchMetrics). Held by a thread-safe shared ref so the worker decode jobs and the
	 *  render-thread upload command can bump their outcomes; GetBenchMetrics reads them lock-free. */
	struct FLogiTexPipeStats
	{
		std::atomic<uint32> DecodeDispatched{0}, DecodeJobOk{0}, DecodeJobEmpty{0};
		std::atomic<uint32> DrainDropEvicted{0}, DrainDropDup{0};
		std::atomic<uint32> UploadConfirmed{0}, UploadNoRhi{0}, UploadMipMismatch{0}, UploadShortMip{0},
			UploadLockFailed{0}, UploadRetried{0}, UploadSurrendered{0};
	};

	/** A decoded atlas awaiting CONFIRMATION that its render-thread upload actually landed before it is shown. The
	 *  texture + its mip chain (which carries the atomic UploadState the render command sets) are kept alive here
	 *  until ProcessPendingTextureBinds binds it (upload confirmed), retries the upload, or returns it to the pool
	 *  (gave up / tile evicted). This is the gate that stops a dropped upload from showing as wrong-atlas or black. */
	struct FPendingTextureBind
	{
		uint64 Key = 0;
		TStrongObjectPtr<UTexture2D> Texture;                       // the atlas UObject (kept alive until bound/dropped)
		TSharedPtr<FLogiTileMipChain, ESPMode::ThreadSafe> Chain;   // carries UploadState (+ the bytes, for a retry)
		uint8 UploadRetries = 0;                                    // bounded re-enqueue of a dropped upload
	};

	bool EnsureWorld();
	void EnsureRenderComponents();
	void StepAndBuildBaked(const FLogiStreamView& View);  // procedural / baked-store lane
	void StepAndBuildGlobe(const FLogiStreamView& View);  // Google 3D Tiles (ion) lane
	// Globe-lane reconcile helpers (ADR-0012 delta protocol). StepAndBuildGlobe polls the latest frame, then either
	// SKIPS (unchanged frame + in sync → O(1) steady state), applies the O(Δ) delta (ApplyGlobeDelta: recycle
	// removed[] then build added[]), or re-scans the full visible list (ReconcileGlobeFull: first frame / large
	// burst / backlog resync — it defers safely via the retained full list and updates bDeltaInSync).
	void ReconcileGlobeFull(const LogiGlobeFrame& Frame, int32 BuildBudget, double GeoDeadline);
	void ApplyGlobeDelta(const LogiGlobeFrame& Frame);
	void BuildGlobeTile(const LogiGlobeTile& T); // shared per-tile build (delta + full paths)
	/** Geometric hole-clip: set this tile's clip OBB params on its MID, swapping the Opaque base <-> the MASKED
	 *  clip base on the 0<->nonzero transition (preserving its bound atlas + fade). A cheap param-only update when
	 *  the base is unchanged. Called at build and by RefreshTileClips each poll the scene changes. */
	void ApplyTileClip(FTileEntry& Entry, const LogiGlobeTile& T);
	/** Refresh every visible tile's clip params from `Frame` — the delta path never revisits a RETAINED coarse
	 *  stand-in whose clip set grows as its children texture in. Skipped in the O(1) unchanged-frame steady state. */
	void RefreshTileClips(const LogiGlobeFrame& Frame);
	void RecycleByKey(uint64 Key);               // per-tile teardown by key (delta removed[] path)
	void MarkTileTextured(uint64 Key, FTileEntry& Entry); // texture-ready: report to the Rust gate (visibility mirrors the published flag)
	void ApplyTileVisibility(uint64 Key, FTileEntry& Entry, uint32 TileFlags); // mirror the published HIDDEN flag; arms the reveal fade
	void DriveTileTextures(int32 Budget);        // bounded atlas re-drive for built-but-untextured tiles
	/** Advance the per-tile LOD cross-fade each step: bloom FadingIn tiles' FadeAlpha 0→1 (eased), and recycle held
	 *  FadingOut tiles once their cover time elapses. Cheap — one scalar MID-param update per transitioning tile. */
	void TickTileFades(float DeltaSeconds);
	/** Retire departed tiles (those not in `Resident`) into the departed-grace cache: hide in place, RETAIN the
	 *  component/texture/TexturedKeys report (see TileRetainSeconds — instant recycle fed the cut-oscillation).
	 *  Tiles back in `Resident` leave grace. Actual recycling happens in SweepRetainedTiles. Returns false: with
	 *  the self-driving sweep there is no eviction backlog that needs the full-reconcile path. */
	bool EvictDeparted(const TSet<uint64>& Resident);
	/** Recycle retained tiles whose grace expired (or oldest-first over TileRetainBudget), bounded by
	 *  TileEvictionsPerStep per call. Runs EVERY globe step (both reconcile paths) — the grace cache is
	 *  self-draining, never a reconcile backlog. The full recycle here is the ONLY place a departed tile's
	 *  TexturedKeys report is finally withdrawn (one dirty push, batched, long after the cut moved on). */
	void SweepRetainedTiles(double Now);
	/** Anchor the level's SkyAtmosphere planet model to the streamed globe (see bAnchorSkyToPlanet): move the
	 *  sky actor to the WGS84 earth center (Rust kernel geodesy via the ABI v15 sky-anchor accessor), switch it
	 *  to PlanetCenterAtComponentTransform, and set the geocentric ground radius. Called once per globe-world
	 *  create. The VolumetricCloud layer follows the atmosphere's planet, so the cloud deck re-anchors too. */
	void AnchorSkyToPlanet();
	/** Sun-at-camera-zenith (see bSunAtCameraZenith): orient the level's DirectionalLight straight down the
	 *  camera's geocentric up, every globe step the direction moved > ~0.1° (parked camera = zero churn). */
	void UpdateSunZenith(const FVector& CamUE);
	/** The anchored planet center in UE cm (set by AnchorSkyToPlanet; ZeroVector until anchored) — shared by
	 *  the sun-zenith update so both planet models agree. */
	FVector SkyEarthCenterUE = FVector::ZeroVector;
	bool bSkyAnchored = false;
	/** The level's sun (first DirectionalLight), cached after the first sun-zenith lookup. */
	TWeakObjectPtr<class ADirectionalLight> SunLightCache;
	void ReleaseWorld();
	/** Tear down the Rust world + all built tiles so the next tick rebuilds from the current settings. */
	void ReloadTerrain();

	// --- Per-tile render resource lifecycle (pooled; all game-thread) -----------------------------------
	/** A per-tile tile-mesh component (visible, attached, registered), reused from the pool or freshly made. */
	ULogiTileMeshComponent* AcquireProcComp();
	/** A per-tile MID over `Base`, reused from the pool (same-base MIDs only) or freshly created. */
	UMaterialInstanceDynamic* AcquireMid(UMaterialInterface* Base);
	/** Return an idle MID to its per-parent pool bucket (audit 2026-06-10 C3 — see MidPool). */
	void RecycleMid(TStrongObjectPtr<UMaterialInstanceDynamic>&& Mid);
#if WITH_EDITOR
	/** One-time (per session): wire the globe tile material's LOD-reveal FOCUS-PULL (FadeAlpha → soft-to-sharp mip
	 *  bias). In-memory + recompiled, not saved. Editor-only — the Python material API can't reach these props.
	 *  (The former DepthBiasCm → Pixel Depth Offset is removed: the selector emits a non-overlapping LOD cut.) */
	void EnsureGlobeMaterialFocusPull(class UMaterial* M);
	/** Build the MASKED hole-clip material variant at runtime (mirrors GetOrCreateDebugMaterial): unlit,
	 *  BLEND_Masked, emissive = the atlas, OpacityMask = NOT inside any of up to LOGI_CLIP_MAX clip OBBs (a Custom
	 *  HLSL node over per-tile MID params). The runtime fallback when /Game/Globe/M_LogiGlobeTileClip is not
	 *  authored. Editor/PIE only (runtime material compilation needs the shader compiler). */
	UMaterialInterface* GetOrCreateGlobeClipMaterial();
#endif
	/** The base material a Google tile should use given its render flags (LogiGlobeTile.flags), the
	 *  GoogleTileLighting setting, and whether it needs the hole-clip mask (`bWantClip`): the MASKED clip variant
	 *  when clipping + available, else the unlit material, or the lit one (falls back to unlit + warn-once if the
	 *  lit material is unauthored). Never null when GlobeBaseMaterial resolved. */
	UMaterialInterface* GlobeMaterialForTile(uint32 TileFlags, bool bWantClip);
	/** Texture-pool bucket key for a (W,H) atlas: width in the high 16 bits, height in the low 16. The single
	 *  source of truth shared by AcquireTexture (lookup) and RecycleTile (insert), so the two can never drift. */
	static FORCEINLINE uint32 TilePoolKey(int32 W, int32 H)
	{
		return (static_cast<uint32>(W) << 16) | static_cast<uint32>(H);
	}
	/** A UTexture2D of (W,H) whose BC7 mip chain is uploaded ON THE RENDER THREAD (audit T4) — reused from the
	 *  size pool or created fresh. The game thread only creates/acquires the UObject + hands the (shared) BC7
	 *  bytes to a render command; it does NOT memcpy the mip pixels. `Mips` is consumed (its bytes are moved into
	 *  the render command), so the caller must not reuse it afterwards. Returns null on a bad size/empty chain. */
	UTexture2D* AcquireTexture(const TSharedRef<FLogiTileMipChain, ESPMode::ThreadSafe>& Mips);
	/** Game-thread: ensure a freshly-created texture's full mip-descriptor chain exists with correctly-sized (but
	 *  UNINITIALISED) BulkData per mip — no pixel copy, so UpdateResource() builds a correctly-laid-out RHI texture
	 *  while the per-mip Memcpy is deferred to the render thread (audit T4). Static: touches no instance state. */
	static void AllocateMipDescriptors(UTexture2D* Tex, int32 W, int32 H, int32 NumMips);
	/** Game-thread: enqueue the per-tile BC7 mip-byte upload onto the RENDER thread (audit T4). Captures a shared
	 *  ref to the BC7 bytes (so they outlive the async upload) + the texture's render resource; touches no UObject
	 *  off the game thread. Static: touches no instance state beyond the shared outcome counters. */
	static void EnqueueTileMipUploadRT(UTexture2D* Tex, const TSharedRef<FLogiTileMipChain, ESPMode::ThreadSafe>& Mips,
		const TSharedRef<FLogiTexPipeStats, ESPMode::ThreadSafe>& Stats);
	/** Recycle one departed tile's component/MID/texture back to the pools (Nanite comps are destroyed). */
	void RecycleTile(FTileEntry& Entry);
	/** Game-thread: turn finished worker decodes into textures + MIDs on their still-resident tiles. Stops once
	 *  FPlatformTime::Seconds() reaches DeadlineSec (after processing at least one, so it always makes progress);
	 *  pass 0.0 to drain the whole queue unbounded. */
	void DrainDecodedTextures(double DeadlineSec);
	/** Confirm-or-retry the deferred atlas uploads parked from prior steps: bind the tile's BaseColorTex ONLY once
	 *  the render command reports the bytes are GPU-resident (UploadState == 1); re-enqueue a dropped upload
	 *  (bounded); or return the texture to the pool if it gave up / the tile was evicted. Runs each step (top of
	 *  DrainDecodedTextures). This is the anti-corruption gate: nothing unconfirmed is ever shown. */
	void ProcessPendingTextureBinds();
	/** Return a recycled atlas UTexture2D to TexturePool, bounded by GlobeMaxPooledTextures — past the cap it is
	 *  dropped (GC reclaims) so the NeverStream atlas pool can't grow VRAM without limit across a long fly-around. */
	void ReturnTextureToPool(uint32 PoolKey, TStrongObjectPtr<UTexture2D>&& Tex);
	/** Dispatch a worker decode of a tile atlas (copies the frame-transient JPEG bytes first). */
	void DispatchTileDecode(uint64 Key, const uint8* Jpeg, int64 Len);
	/** Destroy every live tile's components and empty the tile map (used on teardown/reload). */
	void DestroyAllTiles();
	/** Destroy pooled components, drop pooled MIDs/textures, reset decode state with a fresh inbox. */
	void ClearPools();

	// --- Debug visualization + telemetry (editor / PIE) -------------------------------------------------
	/** True when a heatmap view is selected (and the runtime debug material resolved). */
	bool IsDebugViewActive();
	/** The flat unlit "DebugColor" material, built once at runtime. Editor only — null otherwise. */
	UMaterialInterface* GetOrCreateDebugMaterial();
	/** A per-tile debug-colour MID over the debug material, reused from the pool or freshly created. */
	UMaterialInstanceDynamic* AcquireDebugMid();
	/** The render component a tile draws through (per-tile proc-mesh, or the Nanite static mesh). */
	UMeshComponent* TileComponent(const FTileEntry& Entry) const;
	/** A tile's real (non-debug) material: its textured/placeholder MID (globe) or the shared terrain material. */
	UMaterialInterface* RealMaterialForTile(const FTileEntry& Entry) const;
	/** The heatmap colour for a tile under the active view + palette (uses the per-frame ranges below). */
	FLinearColor DebugColorForTile(const FTileEntry& Entry, const FLogiStreamView& View) const;
	/** Each step: bind + colour debug MIDs while a view is active, or restore real materials the step a view
	 *  turns off. Cheap once bound (a colour-parameter update per tile — no proxy churn). */
	void UpdateTileDebugDisplay(const FLogiStreamView& View);
	/** Restore one tile's real material to its component and recycle its debug MID (view-off / teardown). */
	void RestoreTileMaterial(FTileEntry& Entry);
	/** Refresh the read-only telemetry snapshot (Dbg* fields) + the per-frame debug ranges over the resident
	 *  tiles. `*Lane*` args are the lane-specific live numbers (0 / empty where a lane has no such concept). */
	void UpdateDebugTelemetry(const TCHAR* SourceLabel, int32 Rendered, int32 Resident, int32 InFlight,
		const FString& Failures, int32 CachedMB);
	/** Fold one geo step's game-thread cost into the rolling avg/peak the panel shows (logs every 30 steps). */
	void RecordGeoStepMs(double Ms);
	/** Register / unregister the viewport telemetry overlay to match `bShowDebugPanel`. */
	void SyncDebugPanelRegistration();
	/** UDebugDrawService callback: draw the telemetry overlay onto the editor / PIE viewport canvas. */
	void DrawDebugPanel(UCanvas* Canvas, APlayerController* PC);

	TStrongObjectPtr<UMaterialInterface> TerrainMaterial; // procedural/baked: displacement MID or the asset
	TStrongObjectPtr<UMaterialInterface> GlobeBaseMaterial; // Google tiles: UNLIT base material (M_LogiGlobeTile)
	TStrongObjectPtr<UMaterialInterface> GlobeLitMaterial;  // Google tiles: optional LIT base (M_LogiGlobeTileLit); null if unauthored
	TStrongObjectPtr<UMaterialInterface> GlobeClipMaterial; // Google tiles: MASKED hole-clip variant (authored M_LogiGlobeTileClip, or built at runtime); null if neither
	LogiBakedWorld* BakedWorld = nullptr;   // procedural / baked source
	LogiGlobeWorld* GlobeWorld = nullptr;   // Google-tiles source

	TMap<uint64, FTileEntry> Tiles;
	// Delta-reconcile state (ADR-0012 globe lane). `LastFrameId` is the render frame id last applied (compared to
	// LogiGlobeFrame::frame_id — equal ⇒ unchanged scene ⇒ skip the reconcile, O(1)). `bDeltaInSync` is true when
	// `Tiles` exactly mirrors the last emitted frame, so the O(Δ) delta path is safe; a budget-deferred build or
	// eviction clears it and forces the full-reconcile path until the backlog drains. `PendingTexture` is the
	// (small) set of built tiles still awaiting a resident atlas — DriveTileTextures retries them by key, bounded.
	uint64 LastFrameId = 0;
	bool bDeltaInSync = false;
	TSet<uint64> PendingTexture;
	bool bCreateAttempted = false;
	bool bMaterialResolveAttempted = false; // resolve the source material once per world (reset on ReloadTerrain)
	bool bWarnedNoGlobeMaterial = false;    // warn once if the Google-tiles base material is unauthored
	bool bWarnedNoLitGlobeMaterial = false; // warn once if Force/Auto-Lit is requested but no lit material exists
	bool bUseNaniteLaneResolved = false;    // resolved at world creation (toggle + source + editor support)

	// Recycle pools — idle per-tile resources kept GC-rooted and reused, so steady-state streaming allocates
	// no UObjects. Total count is bounded by PEAK simultaneous residency (a recycled item was once live), not
	// by lifetime tile throughput, so the pools are self-bounding and need no cap.
	TArray<TStrongObjectPtr<ULogiTileMeshComponent>> ProcCompPool;
	// Keyed by PARENT material (audit 2026-06-10 C3): a MID can't be re-parented, and the hole-clip lane made
	// the pool dual-base (opaque <-> masked swaps recycle BOTH kinds) — a flat pool popped-and-DROPPED every
	// non-matching MID while hunting a match, garbage-collecting pooled MIDs of the other base exactly during
	// LOD convergence. Per-parent buckets make acquire O(1) with no cross-base destruction.
	TMap<TObjectKey<UMaterialInterface>, TArray<TStrongObjectPtr<UMaterialInstanceDynamic>>> MidPool;
	TMap<uint32, TArray<TStrongObjectPtr<UTexture2D>>> TexturePool; // keyed by TilePoolKey(W, H)
	int32 PooledTextureCount = 0;                       // live total across TexturePool buckets (bounded by GlobeMaxPooledTextures)

	// Atlas uploads awaiting GPU-resident confirmation before they're shown (the anti-corruption gate). A decoded
	// tile is parked here with its upload in flight; ProcessPendingTextureBinds binds it once UploadState confirms.
	// Keys in AwaitingTextureConfirm are skipped by the re-drive so a pending bind is never redundantly re-decoded.
	TArray<FPendingTextureBind> PendingBinds;
	TSet<uint64> AwaitingTextureConfirm;

	// LOD cross-fade ("rack-focus bloom") bookkeeping. (The old C++ FadingOut hold-the-departed-parent map is
	// gone: the Rust texture gate + geometric hole-clip keep the coarse stand-in covering, so departed tiles
	// recycle immediately — see RecycleByKey / EvictDeparted.)
	TSet<uint64> FadingKeys;          // tiles currently focus-pulling in — keeps the per-step tick O(fading), not O(resident)
	double LastFadeStepSeconds = 0.0; // origin for the per-step fade delta-time

	// Invisible LOD loading (the Rust texture gate, Geo ABI v9): the set of tile keys we've fully TEXTURED, reported
	// to Rust via logi_globe_world_set_textured_keys so the selector holds a tile's coarse parent rendered until we've
	// textured it (no gray flash, no sky). Pushed only when it CHANGED (bTexturedKeysDirty); flattened through
	// TexturedKeysScratch (kept capacity) to avoid a per-push allocation.
	TSet<uint64> TexturedKeys;
	bool bTexturedKeysDirty = true;   // force the first push (report whatever we have, even empty, to prime the gate)
	TArray<uint64> TexturedKeysScratch;

	// Async atlas decode: workers push finished decodes to this MPSC inbox; the game thread drains it. Shared
	// (ThreadSafe) so a worker that finishes after teardown delivers into a still-alive queue, never a freed actor.
	TSharedPtr<TQueue<FLogiTileDecode, EQueueMode::Mpsc>, ESPMode::ThreadSafe> DecodeQueue;
	TSet<uint64> DecodingKeys;                          // keys with a decode in flight (no duplicate dispatch)
	IImageWrapperModule* ImageWrapperModule = nullptr;  // resolved once on the game thread; used by the workers
	// Texture-pipeline outcome counters (shared with worker jobs + the render-thread upload command).
	TSharedRef<FLogiTexPipeStats, ESPMode::ThreadSafe> TexStats = MakeShared<FLogiTexPipeStats, ESPMode::ThreadSafe>();

	// Reused per-step scratch (keeps capacity).
	TSet<uint64> ResidentScratch;
	TArray<uint64> DepartedScratch;

	// Departed-grace cache (key -> departure time, FPlatformTime seconds): tiles that left the published cut,
	// retained hidden-but-fully-built until SweepRetainedTiles expires them. See TileRetainSeconds for why.
	TMap<uint64, double> DepartedGrace;

	// --- Debug visualization + telemetry state ----------------------------------------------------------
	EGeoDebugView ActiveDebugView = EGeoDebugView::None;      // the view currently BOUND on the tiles (vs the
	                                                         // edited DebugView) — drives the on/off transition
	TStrongObjectPtr<UMaterialInterface> DebugMaterial;       // runtime-built flat unlit "DebugColor" base (editor)
	TArray<TStrongObjectPtr<UMaterialInstanceDynamic>> DebugMidPool; // idle per-tile debug MIDs, reused
	FDelegateHandle DebugPanelHandle;                         // UDebugDrawService overlay registration (if shown)

	// Per-frame normalization ranges over the resident tiles (computed in UpdateDebugTelemetry, read by
	// DebugColorForTile) so the depth / geometric-error heatmaps span the in-view spread.
	uint16 DebugDepthMin = 0;
	uint16 DebugDepthMax = 0;
	float DebugGeoErrMin = 0.0f;
	float DebugGeoErrMax = 0.0f;

	// Failure/heartbeat change-detection state (audit 2026-06-10 C8 — promoted from function-local statics:
	// those were shared across ALL actor instances AND persisted across PIE sessions, so a second globe actor
	// or a world re-create cross-talked / spuriously fired the change logs).
	uint64 LastFailedCounts = 0;
	uint64 LastRefineCounts = 0;
	uint32 HeartbeatCounter = 0;

	// Geo game-thread step cost, rolling (promoted from former function-local statics so the panel reads them).
	double GeoStepSumMs = 0.0;       // accumulator over the current 30-step window
	double GeoStepWindowMaxMs = 0.0; // peak over the current window
	int32 GeoStepN = 0;              // steps in the current window
	double DbgGeoStepAvgMs = 0.0;    // last reported window average (ms)
	double DbgGeoStepPeakMs = 0.0;   // last reported window peak (ms)
};
