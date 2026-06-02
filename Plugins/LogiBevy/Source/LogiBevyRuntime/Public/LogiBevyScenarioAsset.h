// Copyright LogiUE. All Rights Reserved.

#pragma once

#include "CoreMinimal.h"
#include "Engine/DataAsset.h"
#include "LogiBevyScenarioAsset.generated.h"

/** One initial spawn entry referencing a recipe by id (§9). */
USTRUCT(BlueprintType)
struct FLogiBevyInitialSpawn
{
	GENERATED_BODY()

	UPROPERTY(EditAnywhere, Category = "LogiBevy")
	int64 RecipeId = 0;

	UPROPERTY(EditAnywhere, Category = "LogiBevy")
	FVector Position = FVector::ZeroVector;

	UPROPERTY(EditAnywhere, Category = "LogiBevy", meta = (ClampMin = "1"))
	int32 Count = 1;
};

/**
 * Editor-authored scenario (§9): seed, world/terrain config, initial spawn
 * batches, and budget config. Serialized to a Rust-readable schema at cook/save
 * time; the runtime initializes a LogiWorld from it.
 */
UCLASS(BlueprintType)
class LOGIBEVYRUNTIME_API ULogiBevyScenarioAsset : public UPrimaryDataAsset
{
	GENERATED_BODY()

public:
	UPROPERTY(EditAnywhere, Category = "LogiBevy|Scenario")
	int64 Seed = 0;

	UPROPERTY(EditAnywhere, Category = "LogiBevy|Scenario", meta = (ClampMin = "1"))
	int32 FixedStepHz = 30;

	/** Handle/id of the terrain + world config (resolved by the terrain kernel). */
	UPROPERTY(EditAnywhere, Category = "LogiBevy|Scenario")
	int64 TerrainWorldConfig = 0;

	UPROPERTY(EditAnywhere, Category = "LogiBevy|Scenario")
	TArray<FLogiBevyInitialSpawn> InitialSpawns;

	UPROPERTY(EditAnywhere, Category = "LogiBevy|Budget")
	int32 HighResActorBudget = 64;

	UPROPERTY(EditAnywhere, Category = "LogiBevy|Budget")
	int32 LowResActorBudget = 256;

	UPROPERTY(EditAnywhere, Category = "LogiBevy|Budget")
	int32 InstancedMeshBudget = 100000;
};
