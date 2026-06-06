// Copyright LogiUE. All Rights Reserved.

#pragma once

#include "CoreMinimal.h"
#include "Engine/DeveloperSettings.h"
#include "LogiBevyDeveloperSettings.generated.h"

class ULogiBevyAssetRegistry;

/** How a baked terrain tile is meshed. Used by the placeable Logi Streamed Tiles Terrain actor. */
UENUM(BlueprintType)
enum class EBakedTerrainMeshing : uint8
{
	Uniform UMETA(DisplayName = "Uniform grid"),
	TIN     UMETA(DisplayName = "Adaptive TIN")
};

/**
 * Runtime settings for LogiUE (Project Settings -> Plugins -> LogiUE).
 *
 * Engine-side configuration for the simulation, its on-screen representation, and the sample building. None
 * of this is gameplay authority -- it is configuration the C++ host passes to the Rust runtime at world
 * creation. In the editor, changing the sample-building setting reloads it live via `OnSettingsChanged`;
 * **Simulation** and **Representation** settings are read when the sim world is created, so they take effect
 * the next time play starts (live-recreating the running sim world would discard its state).
 *
 * Terrain — procedural, baked tiles, or streamed Google 3D Tiles — now lives on the placeable **Logi
 * Streamed Tiles Terrain** actor (drop one into a level), not here.
 */
UCLASS(config = Game, defaultconfig, meta = (DisplayName = "LogiUE"))
class ULogiBevyDeveloperSettings : public UDeveloperSettings
{
	GENERATED_BODY()

public:
	virtual FName GetCategoryName() const override { return TEXT("Plugins"); }

#if WITH_EDITOR
	/** Broadcast (editor only) after a setting changes, carrying the changed property's name, so the live
	 *  world subsystems can reload just the system it affects (falling back to a full reload). */
	DECLARE_MULTICAST_DELEGATE_OneParam(FOnLogiSettingsChanged, FName /*PropertyName*/);
	static FOnLogiSettingsChanged OnSettingsChanged;

	virtual void PostEditChangeProperty(struct FPropertyChangedEvent& PropertyChangedEvent) override;
#endif

	// ----------------------------------------------------------------------------------------------------
	// Simulation
	// ----------------------------------------------------------------------------------------------------

	/** Fixed simulation step rate (Hz), decoupled from the render frame rate. */
	UPROPERTY(EditAnywhere, config, Category = "Simulation", meta = (DisplayName = "Fixed Step Rate (Hz)", ClampMin = 1, ClampMax = 240))
	int32 FixedStepHz = 30;

	/** Seed for the deterministic random number generator. The same seed reproduces the same run. */
	UPROPERTY(EditAnywhere, config, Category = "Simulation", meta = (DisplayName = "Deterministic Seed"))
	int64 DeterministicSeed = 0x4C4F4749; // 'LOGI'

	/** Worker threads for the parallel simulation executor. 0 = automatic (cores minus a small reserve). */
	UPROPERTY(EditAnywhere, config, Category = "Simulation", meta = (DisplayName = "Worker Thread Count", ClampMin = 0, ClampMax = 64))
	int32 WorkerThreadCount = 0;

	/** Force the single-threaded scheduler. Slower, but gives strict run-to-run reproducibility for replay
	 *  and tests. Leave off for normal use -- the multi-threaded scheduler is still deterministic by design. */
	UPROPERTY(EditAnywhere, config, Category = "Simulation", meta = (DisplayName = "Force Single-Threaded"))
	bool bForceSingleThreaded = false;

	// ----------------------------------------------------------------------------------------------------
	// Representation (how many entities render at each detail tier)
	// ----------------------------------------------------------------------------------------------------

	/** The most significant entities rendered as full-detail actors. */
	UPROPERTY(EditAnywhere, config, Category = "Representation", meta = (DisplayName = "High-Res Actor Budget", ClampMin = 0))
	int32 HighResActorBudget = 64;

	/** The next tier of entities rendered as low-detail actors. */
	UPROPERTY(EditAnywhere, config, Category = "Representation", meta = (DisplayName = "Low-Res Actor Budget", ClampMin = 0))
	int32 LowResActorBudget = 256;

	/** The remaining significant entities rendered as instanced static meshes. */
	UPROPERTY(EditAnywhere, config, Category = "Representation", meta = (DisplayName = "Instanced Mesh Budget", ClampMin = 0))
	int32 InstancedMeshBudget = 100000;

	// ----------------------------------------------------------------------------------------------------
	// Assets
	// ----------------------------------------------------------------------------------------------------

	/** Maps asset handles to meshes / materials / actor classes. Optional -- an engine cube is used as a
	 *  fallback when a handle is unmapped. */
	UPROPERTY(EditAnywhere, config, Category = "Assets", meta = (DisplayName = "Asset Registry"))
	TSoftObjectPtr<ULogiBevyAssetRegistry> AssetRegistry;

	// ----------------------------------------------------------------------------------------------------
	// Startup
	// ----------------------------------------------------------------------------------------------------

	/** Placeholder agents spawned at startup, for quick visual testing before a real scenario is loaded. */
	UPROPERTY(EditAnywhere, config, Category = "Startup", meta = (DisplayName = "Demo Agent Count", ClampMin = 0, ClampMax = 1000000))
	int32 DemoAgentCount = 4;

	// ----------------------------------------------------------------------------------------------------
	// Geo | Sample Building
	// ----------------------------------------------------------------------------------------------------
	// Real-world terrain -- procedural / baked tiles / streamed Google 3D Tiles -- now lives on the placeable
	// **Logi Streamed Tiles Terrain** actor (drop one into a level), configured on its own Details panel.

	/** Load and render the sample building from Content/AGI_HQ (the textured-mesh + geodetic-placement
	 *  proof). Spawns a mesh at the origin; off by default. Reloads live via OnSettingsChanged. */
	UPROPERTY(EditAnywhere, config, Category = "Geo|Sample Building", meta = (DisplayName = "Show Sample Building"))
	bool bShowAgiBuilding = false;
};
