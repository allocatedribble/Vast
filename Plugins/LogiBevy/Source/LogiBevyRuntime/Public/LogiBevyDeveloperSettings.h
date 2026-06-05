// Copyright LogiUE. All Rights Reserved.

#pragma once

#include "CoreMinimal.h"
#include "Engine/DeveloperSettings.h"
#include "LogiBevyDeveloperSettings.generated.h"

class ULogiBevyAssetRegistry;
class UMaterialInterface;

/** HE-0 baked-terrain meshing toggle (ADR-0011 / terrain-representation research). */
UENUM(BlueprintType)
enum class EBakedTerrainMeshing : uint8
{
	Uniform UMETA(DisplayName = "Uniform grid"),
	TIN     UMETA(DisplayName = "Adaptive TIN")
};

/**
 * Project settings for the LogiBevy simulation (Project Settings -> Plugins ->
 * LogiBevy Simulation). Drives world creation, representation budgets, asset
 * resolution, and the temporary demo spawns. None of this is gameplay authority --
 * it is engine-side configuration passed to Rust at world creation.
 */
UCLASS(config = Game, defaultconfig, meta = (DisplayName = "LogiBevy Simulation"))
class ULogiBevyDeveloperSettings : public UDeveloperSettings
{
	GENERATED_BODY()

public:
	virtual FName GetCategoryName() const override { return TEXT("Plugins"); }

	/** Fixed simulation step (Hz), decoupled from frame rate. */
	UPROPERTY(EditAnywhere, config, Category = "Simulation", meta = (ClampMin = 1, ClampMax = 240))
	int32 FixedStepHz = 30;

	/** Seed for the deterministic RNG. */
	UPROPERTY(EditAnywhere, config, Category = "Simulation")
	int64 DeterministicSeed = 0x4C4F4749; // 'LOGI'

	/** Worker threads for the parallel executor. 0 = auto (clamp to cores - 2). */
	UPROPERTY(EditAnywhere, config, Category = "Simulation", meta = (ClampMin = 0, ClampMax = 64))
	int32 WorkerThreadCount = 0;

	/** Force the single-threaded deterministic scheduler (strict replay / tests). */
	UPROPERTY(EditAnywhere, config, Category = "Simulation")
	bool bForceSingleThreaded = false;

	/** Top-N entities by significance rendered as high-res actors (ADR-0003). */
	UPROPERTY(EditAnywhere, config, Category = "Representation", meta = (ClampMin = 0))
	int32 HighResActorBudget = 64;

	/** Next-N entities rendered as low-res actors. */
	UPROPERTY(EditAnywhere, config, Category = "Representation", meta = (ClampMin = 0))
	int32 LowResActorBudget = 256;

	/** Remaining significant entities rendered as instanced static meshes. */
	UPROPERTY(EditAnywhere, config, Category = "Representation", meta = (ClampMin = 0))
	int32 InstancedMeshBudget = 100000;

	/** Maps LogiAssetHandle -> UStaticMesh / UMaterial / AActor (optional; cube fallback). */
	UPROPERTY(EditAnywhere, config, Category = "Assets")
	TSoftObjectPtr<ULogiBevyAssetRegistry> AssetRegistry;

	/** Temporary demo agents spawned at startup (until scenario assets land). */
	UPROPERTY(EditAnywhere, config, Category = "Demo", meta = (ClampMin = 0, ClampMax = 1000000))
	int32 DemoAgentCount = 4;

	/** Enable the actorless terrain renderer (ADR-0007 / Milestone D). OFF by default
	 *  while the debug-lane renderer is tuned -- when on it streams + builds tile mesh
	 *  in game/PIE worlds and the editor level-viewport. */
	UPROPERTY(EditAnywhere, config, Category = "Terrain")
	bool bEnableTerrain = false;

	/** Milestone E production lane: build a real Nanite UStaticMesh per tile (engine build
	 *  path) instead of the UProceduralMeshComponent debug lane -- gets native Nanite LOD /
	 *  GPU culling / VSM (no shadows-off hack). EDITOR/PIE-only: the Nanite encoder is an
	 *  editor module, so this does nothing in a packaged build. Toggle live to A/B the lanes. */
	UPROPERTY(EditAnywhere, config, Category = "Terrain", meta = (DisplayName = "Use Nanite Terrain Lane (Editor only)"))
	bool bUseNaniteTerrainLane = false;

	/** Max Nanite tile-mesh builds to SUBMIT per kernel step. The Nanite build is ASYNC -- the encode
	 *  runs on the static-mesh worker pool and the tile pops in a few frames late (self-healing); this
	 *  is NOT a game-thread-cost throttle but a cap on how many builds are in flight at once (bounding
	 *  worker load + transient encode memory). Lower it if streaming spikes memory or worker contention. */
	UPROPERTY(EditAnywhere, config, Category = "Terrain", meta = (ClampMin = 1, ClampMax = 32))
	int32 NaniteTileBuildsPerStep = 2;

	/** Nanite re-entry cache: how many recently-EVICTED tile meshes to keep encoded so re-visiting an
	 *  area reuses them instead of re-running the (expensive, async) Nanite build. Trades memory for far
	 *  fewer rebuilds when panning around; 0 disables. Survives origin rebases (tile-local meshes;
	 *  cleared only on lane switch / shutdown). */
	UPROPERTY(EditAnywhere, config, Category = "Terrain", meta = (ClampMin = 0, ClampMax = 8192))
	int32 NaniteReentryCacheSize = 4096; // >= the resident cap (MESH_CACHE_CAP/MaxNaniteTiles) so a pan-away-and-back fits

	/** M1 (ADR-0009, Milestone F): load + render the AGI_HQ sample building from Content/AGI_HQ via the
	 *  Rust logi_building_* FFI -- the keystone proof of the 3D-Tiles decode + placement (+ texture) path.
	 *  Spawns a UProceduralMeshComponent at the origin; off by default. */
	UPROPERTY(EditAnywhere, config, Category = "3D Tiles (Milestone F)", meta = (DisplayName = "Show AGI_HQ Building"))
	bool bShowAgiBuilding = false;

	/** M2 (ADR-0009, Milestone F): stream Google Photorealistic 3D Tiles through the native Rust stack
	 *  (logi_globe_* FFI) and draw the decoded, textured tiles as procedural-mesh sections in the level
	 *  viewport. Off by default. The Cesium ion token is read at create time from the CESIUM_ION_TOKEN
	 *  environment variable, else from <Project>/Intermediate/CesiumIonToken.txt (both gitignored) --
	 *  never stored in committed config. */
	UPROPERTY(EditAnywhere, config, Category = "3D Tiles (Milestone F)", meta = (DisplayName = "Show Photorealistic Globe"))
	bool bShowPhotorealGlobe = false;

	/** Cesium ion asset id to stream (2275207 = Google Photorealistic 3D Tiles). */
	UPROPERTY(EditAnywhere, config, Category = "3D Tiles (Milestone F)", meta = (DisplayName = "Globe ion Asset Id", ClampMin = 1))
	int64 GlobeIonAssetId = 2275207;

	/** ENU origin longitude (degrees) the globe tiles are rebased around (pinned at Unreal world 0,0,0). */
	UPROPERTY(EditAnywhere, config, Category = "3D Tiles (Milestone F)", meta = (DisplayName = "Globe Origin Longitude", ClampMin = -180.0, ClampMax = 180.0))
	double GlobeOriginLongitude = -73.9857; // Empire State Building, Manhattan (dense P3DT coverage)

	/** ENU origin latitude (degrees). */
	UPROPERTY(EditAnywhere, config, Category = "3D Tiles (Milestone F)", meta = (DisplayName = "Globe Origin Latitude", ClampMin = -90.0, ClampMax = 90.0))
	double GlobeOriginLatitude = 40.7484;

	/** ENU origin height (metres above the WGS84 ellipsoid). */
	UPROPERTY(EditAnywhere, config, Category = "3D Tiles (Milestone F)", meta = (DisplayName = "Globe Origin Height (m)"))
	double GlobeOriginHeight = 0.0;

	/** Spatial cull radius (metres) around the camera; caps the streamed extent. */
	UPROPERTY(EditAnywhere, config, Category = "3D Tiles (Milestone F)", meta = (DisplayName = "Globe Max Tile Distance (m)", ClampMin = 100.0))
	double GlobeMaxTileDistance = 6000.0;

	/** Worker threads for the globe tile fetch pool. */
	UPROPERTY(EditAnywhere, config, Category = "3D Tiles (Milestone F)", meta = (DisplayName = "Globe Fetch Workers", ClampMin = 1, ClampMax = 32))
	int32 GlobeFetchWorkers = 8;

	/** Max NEW tiles to decode + build per step (throttle so a burst of arrivals never spikes the frame;
	 *  remaining tiles stay resident in Rust and build on later steps -- self-healing). */
	UPROPERTY(EditAnywhere, config, Category = "3D Tiles (Milestone F)", meta = (DisplayName = "Globe Tile Builds Per Step", ClampMin = 1, ClampMax = 64))
	int32 GlobeTileBuildsPerStep = 8;

	/** Screen-space-error target (pixels) for globe tile LOD: LOWER pulls finer tiles = more detail, at
	 *  the cost of more tiles + memory. 16 = Cesium's balanced default; 8 = high; ~4 = near the dataset's
	 *  finest LOD. Tunable live. */
	UPROPERTY(EditAnywhere, config, Category = "3D Tiles (Milestone F)", meta = (DisplayName = "Globe Max Screen-Space Error (px)", ClampMin = 1.0, ClampMax = 64.0))
	double GlobeMaxScreenSpaceError = 8.0;

	/** Enable a PERSISTENT on-disk tile cache (survives restarts) so revisited areas load from disk
	 *  instead of re-downloading. Cache dir: <Project>/Saved/P3DTCache (gitignored), LRU-evicted to the
	 *  budget below. WARNING: persistently caching Google Photorealistic 3D Tiles violates Google's Map
	 *  Tiles ToS (tiles may only be cached transiently for the live session) -- use only for self-hosted /
	 *  open tilesets or private R&D. */
	UPROPERTY(EditAnywhere, config, Category = "3D Tiles (Milestone F)", meta = (DisplayName = "Globe Disk Cache (persistent)"))
	bool bGlobeDiskCache = true;

	/** Disk-cache size budget in GB (oldest tiles LRU-evicted past this). */
	UPROPERTY(EditAnywhere, config, Category = "3D Tiles (Milestone F)", meta = (DisplayName = "Globe Disk Cache Budget (GB)", ClampMin = 1, ClampMax = 2048, EditCondition = "bGlobeDiskCache"))
	int32 GlobeCacheBudgetGB = 20;

	/** HE-0 (ADR-0011): load an offline-baked TERRAIN tile store (the Tools/logi_bake output) via the Rust
	 *  logi_baked_* FFI and draw each tile as a procedural-mesh section. Off by default. HE-0 serves the
	 *  whole AOI statically (no camera streaming yet). */
	UPROPERTY(EditAnywhere, config, Category = "Semantic Tiles (HE / ADR-0011)", meta = (DisplayName = "Show Baked Terrain"))
	bool bShowBakedTerrain = false;

	/** Baked tile-store directory. A relative path resolves under <Project>/Saved (e.g. "LogiTiles/hero",
	 *  the default `python -m logi_bake bake` output); an absolute path is used as-is. */
	UPROPERTY(EditAnywhere, config, Category = "Semantic Tiles (HE / ADR-0011)", meta = (DisplayName = "Baked Tile Store Dir"))
	FString BakedTileStoreDir = TEXT("LogiTiles/hero");

	/** Meshing toggle: Uniform grid, or adaptive TIN (fewer triangles on flats, dense on slopes). Selected
	 *  in Rust at world creation via logi_baked_world_create's LogiBakedCreateInfo. */
	UPROPERTY(EditAnywhere, config, Category = "Semantic Tiles (HE / ADR-0011)", meta = (DisplayName = "Baked Terrain Meshing"))
	EBakedTerrainMeshing BakedTerrainMeshing = EBakedTerrainMeshing::Uniform;

	/** TIN max vertical error (metres): higher = fewer triangles. Ignored for Uniform meshing. */
	UPROPERTY(EditAnywhere, config, Category = "Semantic Tiles (HE / ADR-0011)", meta = (DisplayName = "Baked Terrain TIN Error (m)", ClampMin = 0.0, EditCondition = "BakedTerrainMeshing == EBakedTerrainMeshing::TIN"))
	float BakedTerrainTinErrorM = 1.0f;

	/** Apply a DISPLACEMENT material to the baked terrain for first-person micro-detail. For true Nanite
	 *  tessellation the material must enable Nanite displacement AND the mesh must ride the Nanite
	 *  static-mesh lane (ADR-0008, editor-only); on the current proc-mesh lane the material's
	 *  displacement / World Position Offset is used as supported. */
	UPROPERTY(EditAnywhere, config, Category = "Semantic Tiles (HE / ADR-0011)", meta = (DisplayName = "Baked Terrain Nanite Displacement"))
	bool bBakedTerrainNaniteDisplacement = false;

	/** Displacement material applied when the toggle above is on (a MID is created and its
	 *  "DisplacementScale" scalar parameter set). Author it with Nanite tessellation displacement enabled. */
	UPROPERTY(EditAnywhere, config, Category = "Semantic Tiles (HE / ADR-0011)", meta = (DisplayName = "Baked Terrain Displacement Material", EditCondition = "bBakedTerrainNaniteDisplacement"))
	TSoftObjectPtr<UMaterialInterface> BakedTerrainDisplacementMaterial;

	/** Displacement magnitude (cm) fed to the material's "DisplacementScale" parameter. */
	UPROPERTY(EditAnywhere, config, Category = "Semantic Tiles (HE / ADR-0011)", meta = (DisplayName = "Baked Terrain Displacement Scale (cm)", ClampMin = 0.0, EditCondition = "bBakedTerrainNaniteDisplacement"))
	float BakedTerrainDisplacementScale = 20.0f;
};
