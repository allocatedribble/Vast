// Copyright LogiUE. All Rights Reserved.

#pragma once

#include "CoreMinimal.h"
#include "Engine/DeveloperSettings.h"
#include "LogiBevyDeveloperSettings.generated.h"

class ULogiBevyAssetRegistry;

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
};
