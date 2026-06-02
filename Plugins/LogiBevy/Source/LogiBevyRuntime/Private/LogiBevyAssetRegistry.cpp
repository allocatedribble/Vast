// Copyright LogiUE. All Rights Reserved.

#include "LogiBevyAssetRegistry.h"
#include "Engine/StaticMesh.h"
#include "Engine/SkeletalMesh.h"
#include "Materials/MaterialInterface.h"
#include "Animation/AnimInstance.h"
#include "GameFramework/Actor.h"
#include "Engine/AssetManager.h"
#include "Engine/StreamableManager.h"

UStaticMesh* ULogiBevyAssetRegistry::ResolveStaticMesh(uint64 StableHash) const
{
	if (const TSoftObjectPtr<UStaticMesh>* Found = StaticMeshes.Find(static_cast<int64>(StableHash)))
	{
		return Found->Get(); // non-blocking: resident after PreloadAll, else null (caller falls back)
	}
	return nullptr;
}

USkeletalMesh* ULogiBevyAssetRegistry::ResolveSkeletalMesh(uint64 StableHash) const
{
	if (const TSoftObjectPtr<USkeletalMesh>* Found = SkeletalMeshes.Find(static_cast<int64>(StableHash)))
	{
		return Found->Get(); // non-blocking: resident after PreloadAll, else null (caller falls back)
	}
	return nullptr;
}

UMaterialInterface* ULogiBevyAssetRegistry::ResolveMaterial(uint64 StableHash) const
{
	if (const TSoftObjectPtr<UMaterialInterface>* Found = Materials.Find(static_cast<int64>(StableHash)))
	{
		return Found->Get(); // non-blocking: resident after PreloadAll, else null (caller falls back)
	}
	return nullptr;
}

UClass* ULogiBevyAssetRegistry::ResolveActorClass(uint64 StableHash) const
{
	if (const TSubclassOf<AActor>* Found = ActorClasses.Find(static_cast<int64>(StableHash)))
	{
		return Found->Get();
	}
	return nullptr;
}

UClass* ULogiBevyAssetRegistry::ResolveAnimInstance(uint64 StableHash) const
{
	if (const TSubclassOf<UAnimInstance>* Found = AnimInstances.Find(static_cast<int64>(StableHash)))
	{
		return Found->Get();
	}
	return nullptr;
}

void ULogiBevyAssetRegistry::PreloadAll()
{
	TArray<FSoftObjectPath> Paths;
	auto Gather = [&Paths](const auto& Map)
	{
		for (const auto& Pair : Map)
		{
			if (!Pair.Value.IsNull())
			{
				Paths.Add(Pair.Value.ToSoftObjectPath());
			}
		}
	};
	Gather(StaticMeshes);
	Gather(SkeletalMeshes);
	Gather(Materials);

	if (Paths.Num() > 0)
	{
		// Keep the handle alive so the assets stay resident; Resolve* then returns
		// them via Get() with no synchronous load on the hot path.
		PreloadHandle = UAssetManager::GetStreamableManager().RequestAsyncLoad(Paths);
	}
}
