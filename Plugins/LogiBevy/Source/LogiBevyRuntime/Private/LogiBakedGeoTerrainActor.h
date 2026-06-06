// Copyright LogiUE. All Rights Reserved.

#pragma once

#include "CoreMinimal.h"
#include "GameFramework/Actor.h"
#include "UObject/StrongObjectPtr.h"
#include "LogiBevyDeveloperSettings.h" // EBakedTerrainMeshing
#include "LogiBakedGeoTerrainActor.generated.h"

class UProceduralMeshComponent;
class USceneComponent;
class UStaticMeshComponent;
class UMaterialInterface;
class UMaterialInstanceDynamic;
class UTexture2D;
struct FLogiStreamView;

// Opaque Rust world handles (logi_geo_ffi.h). Forward-declared so this header stays free of the C ABI;
// the .cpp includes the real header under LOGI_BEVY_HAVE_GEO. Exactly one is non-null per `Source`.
struct LogiBakedWorld;
struct LogiGlobeWorld;

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
	 *  coarse tiles are used in the distance. (Google tiles cap this at 64 px internally.) */
	UPROPERTY(EditAnywhere, Category = "Terrain", meta = (DisplayName = "Detail Target (px)", ClampMin = 1.0, ClampMax = 256.0))
	float DetailTargetPx = 16.0f;

	/** Maximum new tiles meshed/built per step, so a burst of arrivals never spikes a frame. */
	UPROPERTY(EditAnywhere, Category = "Terrain", meta = (DisplayName = "Tile Builds Per Step", ClampMin = 1, ClampMax = 64))
	int32 TileBuildsPerStep = 8;

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
	int32 GlobeFetchWorkers = 8;

	/** Keep a persistent on-disk tile cache (gitignored Saved/P3DTCache) so revisited areas load from disk
	 *  instead of re-downloading. NOTE: persistently caching Google Photorealistic 3D Tiles violates Google's
	 *  Map Tiles terms (tiles may only be cached for the live session) — use only for self-hosted tilesets. */
	UPROPERTY(EditAnywhere, Category = "Terrain|Google Tiles", meta = (DisplayName = "Persistent Disk Cache", EditCondition = "Source == ETerrainSource::GoogleTiles"))
	bool bGlobeDiskCache = false;

	/** Disk-cache size budget (GB); the oldest tiles are evicted past this. */
	UPROPERTY(EditAnywhere, Category = "Terrain|Google Tiles", meta = (DisplayName = "Disk Cache Budget (GB)", ClampMin = 1, ClampMax = 2048, EditCondition = "Source == ETerrainSource::GoogleTiles && bGlobeDiskCache"))
	int32 GlobeCacheBudgetGB = 20;

	// ---- Rendering -------------------------------------------------------------------------------------

	/** Build a Nanite static mesh per tile (native Nanite level-of-detail, GPU culling, virtual shadow
	 *  maps) instead of a procedural-mesh component. Editor / PIE only. Not used by the Google-tiles source
	 *  (its photoreal tiles are textured proc-mesh sections). */
	UPROPERTY(EditAnywhere, Category = "Terrain|Rendering", meta = (DisplayName = "Use Nanite Mesh Lane (Editor only)", EditCondition = "Source != ETerrainSource::GoogleTiles"))
	bool bUseNaniteMeshLane = false;

	/** Maximum Nanite tile-mesh builds submitted per step (the build is asynchronous; this caps how many are
	 *  in flight, bounding worker load and transient build memory). */
	UPROPERTY(EditAnywhere, Category = "Terrain|Rendering", meta = (DisplayName = "Nanite Builds Per Step", ClampMin = 1, ClampMax = 32, EditCondition = "Source != ETerrainSource::GoogleTiles && bUseNaniteMeshLane"))
	int32 NaniteTileBuildsPerStep = 2;

	/** How many recently-evicted Nanite tile meshes to keep encoded, so returning to an area reuses them.
	 *  0 disables. */
	UPROPERTY(EditAnywhere, Category = "Terrain|Rendering", meta = (DisplayName = "Nanite Reuse Cache Size", ClampMin = 0, ClampMax = 8192, EditCondition = "Source != ETerrainSource::GoogleTiles && bUseNaniteMeshLane"))
	int32 NaniteReentryCacheSize = 4096;

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

	// ---- AActor ----------------------------------------------------------------------------------------

	virtual void Tick(float DeltaTime) override;
	virtual bool ShouldTickIfViewportsOnly() const override { return true; } // stream in the editor viewport
	virtual void EndPlay(const EEndPlayReason::Type EndPlayReason) override;
	virtual void BeginDestroy() override;
#if WITH_EDITOR
	virtual void PostEditChangeProperty(struct FPropertyChangedEvent& PropertyChangedEvent) override;
#endif

private:
	/** One resident tile: a proc-mesh section (optionally with a per-tile textured MID, for Google tiles),
	 *  or a static-mesh component on the Nanite lane. */
	struct FTileEntry
	{
		int32 Section = INDEX_NONE; // proc-mesh section index, or INDEX_NONE on the Nanite lane
		TStrongObjectPtr<UStaticMeshComponent> NaniteComp;     // GC-rooted (Nanite lane only)
		TStrongObjectPtr<UMaterialInstanceDynamic> Material;   // GC-rooted per-tile MID (Google-tiles lane)
		TStrongObjectPtr<UTexture2D> Texture;                  // GC-rooted decoded atlas (Google-tiles lane)
	};

	bool EnsureWorld();
	void EnsureRenderComponents();
	void StepAndBuildBaked(const FLogiStreamView& View);  // procedural / baked-store lane
	void StepAndBuildGlobe(const FLogiStreamView& View);  // Google 3D Tiles (ion) lane
	void EvictDeparted(const TSet<uint64>& Resident);
	void ReleaseWorld();
	/** Tear down the Rust world + all built tiles so the next tick rebuilds from the current settings. */
	void ReloadTerrain();

	UPROPERTY(Transient)
	TObjectPtr<UProceduralMeshComponent> ProcMesh;

	TStrongObjectPtr<UMaterialInterface> TerrainMaterial; // procedural/baked: displacement MID or the asset
	TStrongObjectPtr<UMaterialInterface> GlobeBaseMaterial; // Google tiles: base material for per-tile MIDs
	LogiBakedWorld* BakedWorld = nullptr;   // procedural / baked source
	LogiGlobeWorld* GlobeWorld = nullptr;   // Google-tiles source

	TMap<uint64, FTileEntry> Tiles;
	TArray<int32> FreeSections;
	int32 NextSection = 0;
	float StepAccum = 0.0f;
	bool bCreateAttempted = false;
	bool bMaterialResolveAttempted = false; // resolve the source material once per world (reset on ReloadTerrain)
	bool bWarnedNoGlobeMaterial = false;    // warn once if the Google-tiles base material is unauthored
	bool bUseNaniteLaneResolved = false;    // resolved at world creation (toggle + source + editor support)

	// Reused per-step scratch (keeps capacity).
	TSet<uint64> ResidentScratch;
	TArray<uint64> DepartedScratch;
};
