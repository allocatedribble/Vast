// Copyright LogiUE. All Rights Reserved.

#pragma once

#include "CoreMinimal.h"
#include "Engine/DataAsset.h"
#include "Templates/SubclassOf.h"
#include "LogiBevyAssetRegistry.generated.h"

class UStaticMesh;
class USkeletalMesh;
class UMaterialInterface;
class UAnimInstance;

/**
 * Owns the AssetHandle -> UObject mapping (§9). Rust stores only handles
 * { stable_hash, kind } -- never `UObject*`; this registry resolves them to
 * soft-referenced assets / classes. The handle's `kind` selects the map;
 * `stable_hash` keys it. Authored in the editor, loaded at runtime.
 */
UCLASS(BlueprintType)
class LOGIBEVYRUNTIME_API ULogiBevyAssetRegistry : public UDataAsset
{
	GENERATED_BODY()

public:
	UPROPERTY(EditAnywhere, Category = "LogiBevy|Assets")
	TMap<int64, TSoftObjectPtr<UStaticMesh>> StaticMeshes;

	UPROPERTY(EditAnywhere, Category = "LogiBevy|Assets")
	TMap<int64, TSoftObjectPtr<USkeletalMesh>> SkeletalMeshes;

	UPROPERTY(EditAnywhere, Category = "LogiBevy|Assets")
	TMap<int64, TSoftObjectPtr<UMaterialInterface>> Materials;

	UPROPERTY(EditAnywhere, Category = "LogiBevy|Assets")
	TMap<int64, TSubclassOf<AActor>> ActorClasses;

	UPROPERTY(EditAnywhere, Category = "LogiBevy|Assets")
	TMap<int64, TSubclassOf<UAnimInstance>> AnimInstances;

	// Resolve a handle's stable hash to the concrete asset/class (null if absent).
	UStaticMesh* ResolveStaticMesh(uint64 StableHash) const;
	USkeletalMesh* ResolveSkeletalMesh(uint64 StableHash) const;
	UMaterialInterface* ResolveMaterial(uint64 StableHash) const;
	UClass* ResolveActorClass(uint64 StableHash) const;
	UClass* ResolveAnimInstance(uint64 StableHash) const;
};
