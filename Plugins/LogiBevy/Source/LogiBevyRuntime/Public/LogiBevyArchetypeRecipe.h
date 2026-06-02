// Copyright LogiUE. All Rights Reserved.

#pragma once

#include "CoreMinimal.h"
#include "Engine/DataAsset.h"
#include "Templates/SubclassOf.h"
#include "LogiBevyArchetypeRecipe.generated.h"

class AActor;

/**
 * Editor-authored recipe for an entity archetype (§9). Runtime spawn commands
 * reference `StableRecipeId` (not Blueprint classes); the asset handles are
 * stable hashes resolved via ULogiBevyAssetRegistry. Serialized to a
 * Rust-readable schema at cook/save time.
 */
UCLASS(BlueprintType)
class LOGIBEVYRUNTIME_API ULogiBevyArchetypeRecipe : public UPrimaryDataAsset
{
	GENERATED_BODY()

public:
	/** Stable recipe id referenced by runtime spawn commands. */
	UPROPERTY(EditAnywhere, Category = "LogiBevy|Recipe")
	int64 StableRecipeId = 0;

	/** Opaque default component-data blob applied on spawn (schema-defined). */
	UPROPERTY(EditAnywhere, Category = "LogiBevy|Recipe")
	TArray<uint8> DefaultComponentData;

	/** Asset handle stable hashes (kind implied by field). */
	UPROPERTY(EditAnywhere, Category = "LogiBevy|Recipe")
	int64 MeshHandle = 0;

	UPROPERTY(EditAnywhere, Category = "LogiBevy|Recipe")
	int64 MaterialHandle = 0;

	UPROPERTY(EditAnywhere, Category = "LogiBevy|Recipe")
	int64 SkeletonHandle = 0;

	UPROPERTY(EditAnywhere, Category = "LogiBevy|Recipe")
	int32 SpawnCategory = 0;

	/** Editor-only preview actor class (not used at runtime). */
	UPROPERTY(EditAnywhere, Category = "LogiBevy|Recipe")
	TSubclassOf<AActor> EditorPreviewActorClass;
};
