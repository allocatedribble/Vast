// Copyright LogiUE. All Rights Reserved.

#pragma once

#include "CoreMinimal.h"
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
	 *  batch — see Tick). Each build is O(1) (the decode + mip + BC1 run OFF the game thread; each tile owns a
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

	/** LOD cross-fade ("rack-focus bloom"): refined tiles dither in smoothly instead of popping. Off = instant swap. */
	UPROPERTY(EditAnywhere, Category = "Terrain|Google Tiles|Fade", meta = (DisplayName = "Tile Cross-Fade"))
	bool bGlobeTileFade = true;
	/** Seconds for a tile to bloom in (opacity dither 0→1) once its atlas is confirmed. */
	UPROPERTY(EditAnywhere, Category = "Terrain|Google Tiles|Fade", meta = (DisplayName = "Fade-In Seconds", ClampMin = 0.05, ClampMax = 3.0, EditCondition = "bGlobeTileFade"))
	float GlobeFadeInSec = 0.4f;
	/** Seconds to keep a refined-away coarse tile visible (covering the bloom) before recycling it. Should exceed
	 *  Fade-In Seconds + typical atlas-decode latency so the bloom completes under cover (no gray flash / hole). */
	UPROPERTY(EditAnywhere, Category = "Terrain|Google Tiles|Fade", meta = (DisplayName = "Parent Hold Seconds", ClampMin = 0.05, ClampMax = 4.0, EditCondition = "bGlobeTileFade"))
	float GlobeParentHoldSec = 0.7f;
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
	UPROPERTY(EditAnywhere, Category = "Terrain|Google Tiles", meta = (DisplayName = "Fetch Workers", ClampMin = 1, ClampMax = 32, EditCondition = "Source == ETerrainSource::GoogleTiles"))
	int32 GlobeFetchWorkers = 16; // raised with the finer Detail Target so a new view (or teleport) fills faster

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
	UPROPERTY(EditAnywhere, Category = "Terrain|Google Tiles", meta = (DisplayName = "Mesh Cache Budget (MB)", ClampMin = 0, ClampMax = 16384, EditCondition = "Source == ETerrainSource::GoogleTiles"))
	int32 GlobeMeshCacheBudgetMB = 1024; // larger warm-restore buffer so orbit/zoom re-entry restores without re-decode

	/** Lighting model for Google tiles. Auto honours each tile's `KHR_materials_unlit` flag (P3DT is all
	 *  unlit ⇒ unlit), which avoids the double-lighting / washed-out look of shading already-lit photoreal
	 *  imagery. Force-Lit needs an authored M_LogiGlobeTileLit (falls back to unlit if absent). */
	UPROPERTY(EditAnywhere, Category = "Terrain|Google Tiles", meta = (DisplayName = "Tile Lighting", EditCondition = "Source == ETerrainSource::GoogleTiles"))
	EGoogleTileLighting GoogleTileLighting = EGoogleTileLighting::Auto;

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

	// ---- AActor ----------------------------------------------------------------------------------------

	virtual void Tick(float DeltaTime) override;
	virtual bool ShouldTickIfViewportsOnly() const override { return true; } // stream in the editor viewport
	virtual void EndPlay(const EEndPlayReason::Type EndPlayReason) override;
	virtual void BeginDestroy() override;
#if WITH_EDITOR
	virtual void PostEditChangeProperty(struct FPropertyChangedEvent& PropertyChangedEvent) override;
#endif

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

		// Debug-visualization metadata (Geo FFI ABI 7): the per-tile refinement quality the heatmaps colour by,
		// cached at build (intrinsic to a tile, so set once). `DebugMid` is the per-tile flat-colour MID, held
		// only while a debug view is active and recycled to DebugMidPool when it turns off.
		uint16 Depth = 0;                                      // refinement tree depth / pyramid level (root = 0)
		float GeometricError = 0.0f;                           // 3D Tiles geometric error (metres; lower = finer)
		FVector3f BoundsCenter = FVector3f::ZeroVector;        // tile centre (UE cm) — for the screen-space-error view
		TStrongObjectPtr<UMaterialInstanceDynamic> DebugMid;   // per-tile debug heatmap MID (only while a view is active)

		// LOD cross-fade: a refined-in tile blooms in via a dithered opacity mask (FadeAlpha 0→1) once its atlas is
		// confirmed, while the refined-away coarse tile is held briefly (HoldRemaining, in FadingOut) so it covers
		// the bloom — no pop, no hole, no gray flash. FadeMode None ⇒ resident, no per-frame work.
		EGlobeFade FadeMode = EGlobeFade::None;
		float FadeT = 1.0f;          // bloom progress: 0 = invisible/just-appeared, 1 = fully in
		float HoldRemaining = 0.0f;  // FadingOut tiles only: seconds to keep covering before recycle
	};

	/** A worker-decoded tile atlas (JPEG -> BGRA8 -> BC1 + full mip chain), handed back to the game thread to
	 *  become a UTexture2D. The decode + box-filter + BC1 compress run OFF the game thread; the game thread only
	 *  creates/acquires the UObject — the BC1 mip-byte upload is then enqueued to the RENDER thread (audit T4). */
	struct FLogiTileDecode
	{
		uint64 Key = 0;
		int32 W = 0;
		int32 H = 0;
		TArray<TArray64<uint8>> Mips; // mip 0..N-1, each tightly packed BC1/PF_DXT1 (empty => decode failed)
	};

	/** The BC1 mip chain for one tile, owned by a thread-safe shared pointer so it can be handed across the
	 *  game/render-thread boundary (audit T4). The game thread creates/acquires the UTexture2D, then enqueues a
	 *  render command that copies these BC1 bytes into the live RHI mips — so this payload MUST outlive that
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
	void RecycleByKey(uint64 Key);               // per-tile teardown by key (delta removed[] path)
	/** Park a refined-away/departed tile in FadingOut (kept visible + opaque) so it covers its replacements' bloom;
	 *  recycled by TickTileFades once HoldRemaining elapses. */
	void HoldFadingOut(uint64 Key, FTileEntry&& Entry);
	void DriveTileTextures(int32 Budget);        // bounded atlas re-drive for built-but-untextured tiles
	/** Advance the per-tile LOD cross-fade each step: bloom FadingIn tiles' FadeAlpha 0→1 (eased), and recycle held
	 *  FadingOut tiles once their cover time elapses. Cheap — one scalar MID-param update per transitioning tile. */
	void TickTileFades(float DeltaSeconds);
	/** Recycle departed tiles (those not in `Resident`); returns true if any teardown was DEFERRED past the
	 *  per-frame budget — a backlog that keeps the globe lane on the full-reconcile path until it drains. */
	bool EvictDeparted(const TSet<uint64>& Resident);
	void ReleaseWorld();
	/** Tear down the Rust world + all built tiles so the next tick rebuilds from the current settings. */
	void ReloadTerrain();

	// --- Per-tile render resource lifecycle (pooled; all game-thread) -----------------------------------
	/** A per-tile tile-mesh component (visible, attached, registered), reused from the pool or freshly made. */
	ULogiTileMeshComponent* AcquireProcComp();
	/** A per-tile MID over `Base`, reused from the pool (same-base MIDs only) or freshly created. */
	UMaterialInstanceDynamic* AcquireMid(UMaterialInterface* Base);
#if WITH_EDITOR
	/** One-time (per session): wire a `DepthBiasCm` scalar parameter into the globe tile material's Pixel Depth
	 *  Offset so finer tiles occlude coarser ones (depth-priority refinement; the per-tile DepthBiasCm is set in
	 *  the build loop). In-memory + recompiled, not saved. Editor-only — the Python material API can't reach PDO. */
	void EnsureGlobeMaterialPdo(class UMaterial* M);
#endif
	/** The base material a Google tile should use given its render flags (LogiGlobeTile.flags) and the
	 *  GoogleTileLighting setting: the unlit material, or the lit one (falls back to unlit + warn-once if the
	 *  lit material is unauthored). Never null when GlobeBaseMaterial resolved. */
	UMaterialInterface* GlobeMaterialForTile(uint32 TileFlags);
	/** Texture-pool bucket key for a (W,H) atlas: width in the high 16 bits, height in the low 16. The single
	 *  source of truth shared by AcquireTexture (lookup) and RecycleTile (insert), so the two can never drift. */
	static FORCEINLINE uint32 TilePoolKey(int32 W, int32 H)
	{
		return (static_cast<uint32>(W) << 16) | static_cast<uint32>(H);
	}
	/** A UTexture2D of (W,H) whose BC1 mip chain is uploaded ON THE RENDER THREAD (audit T4) — reused from the
	 *  size pool or created fresh. The game thread only creates/acquires the UObject + hands the (shared) BC1
	 *  bytes to a render command; it does NOT memcpy the mip pixels. `Mips` is consumed (its bytes are moved into
	 *  the render command), so the caller must not reuse it afterwards. Returns null on a bad size/empty chain. */
	UTexture2D* AcquireTexture(const TSharedRef<FLogiTileMipChain, ESPMode::ThreadSafe>& Mips);
	/** Game-thread: ensure a freshly-created texture's full mip-descriptor chain exists with correctly-sized (but
	 *  UNINITIALISED) BulkData per mip — no pixel copy, so UpdateResource() builds a correctly-laid-out RHI texture
	 *  while the per-mip Memcpy is deferred to the render thread (audit T4). Static: touches no instance state. */
	static void AllocateMipDescriptors(UTexture2D* Tex, int32 W, int32 H, int32 NumMips);
	/** Game-thread: enqueue the per-tile BC1 mip-byte upload onto the RENDER thread (audit T4). Captures a shared
	 *  ref to the BC1 bytes (so they outlive the async upload) + the texture's render resource; touches no UObject
	 *  off the game thread. Static: touches no instance state. */
	static void EnqueueTileMipUploadRT(UTexture2D* Tex, const TSharedRef<FLogiTileMipChain, ESPMode::ThreadSafe>& Mips);
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
	TArray<TStrongObjectPtr<UMaterialInstanceDynamic>> MidPool;
	TMap<uint32, TArray<TStrongObjectPtr<UTexture2D>>> TexturePool; // keyed by TilePoolKey(W, H)
	int32 PooledTextureCount = 0;                       // live total across TexturePool buckets (bounded by GlobeMaxPooledTextures)

	// Atlas uploads awaiting GPU-resident confirmation before they're shown (the anti-corruption gate). A decoded
	// tile is parked here with its upload in flight; ProcessPendingTextureBinds binds it once UploadState confirms.
	// Keys in AwaitingTextureConfirm are skipped by the re-drive so a pending bind is never redundantly re-decoded.
	TArray<FPendingTextureBind> PendingBinds;
	TSet<uint64> AwaitingTextureConfirm;

	// LOD cross-fade ("rack-focus bloom"): refined-away / departed tiles parked here, still visible, so they cover
	// the bloom-in of their replacements; recycled once HoldRemaining elapses. Kept OUT of `Tiles` so the delta
	// protocol's mirror invariant (bDeltaInSync) stays honest.
	TMap<uint64, FTileEntry> FadingOut;
	TSet<uint64> FadingKeys;          // tiles currently blooming in — keeps the per-step tick O(fading), not O(resident)
	double LastFadeStepSeconds = 0.0; // origin for the per-step fade delta-time

	// Async atlas decode: workers push finished decodes to this MPSC inbox; the game thread drains it. Shared
	// (ThreadSafe) so a worker that finishes after teardown delivers into a still-alive queue, never a freed actor.
	TSharedPtr<TQueue<FLogiTileDecode, EQueueMode::Mpsc>, ESPMode::ThreadSafe> DecodeQueue;
	TSet<uint64> DecodingKeys;                          // keys with a decode in flight (no duplicate dispatch)
	IImageWrapperModule* ImageWrapperModule = nullptr;  // resolved once on the game thread; used by the workers

	// Reused per-step scratch (keeps capacity).
	TSet<uint64> ResidentScratch;
	TArray<uint64> DepartedScratch;

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

	// Geo game-thread step cost, rolling (promoted from former function-local statics so the panel reads them).
	double GeoStepSumMs = 0.0;       // accumulator over the current 30-step window
	double GeoStepWindowMaxMs = 0.0; // peak over the current window
	int32 GeoStepN = 0;              // steps in the current window
	double DbgGeoStepAvgMs = 0.0;    // last reported window average (ms)
	double DbgGeoStepPeakMs = 0.0;   // last reported window peak (ms)
};
