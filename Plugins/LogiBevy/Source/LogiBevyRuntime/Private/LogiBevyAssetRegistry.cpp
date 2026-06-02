// Copyright LogiUE. All Rights Reserved.

#include "LogiBevyAssetRegistry.h"
#include "Engine/StaticMesh.h"
#include "Engine/SkeletalMesh.h"
#include "Materials/MaterialInterface.h"
#include "Animation/AnimInstance.h"
#include "GameFramework/Actor.h"

UStaticMesh* ULogiBevyAssetRegistry::ResolveStaticMesh(uint64 StableHash) const
{
	if (const TSoftObjectPtr<UStaticMesh>* Found = StaticMeshes.Find(static_cast<int64>(StableHash)))
	{
		return Found->LoadSynchronous();
	}
	return nullptr;
}

USkeletalMesh* ULogiBevyAssetRegistry::ResolveSkeletalMesh(uint64 StableHash) const
{
	if (const TSoftObjectPtr<USkeletalMesh>* Found = SkeletalMeshes.Find(static_cast<int64>(StableHash)))
	{
		return Found->LoadSynchronous();
	}
	return nullptr;
}

UMaterialInterface* ULogiBevyAssetRegistry::ResolveMaterial(uint64 StableHash) const
{
	if (const TSoftObjectPtr<UMaterialInterface>* Found = Materials.Find(static_cast<int64>(StableHash)))
	{
		return Found->LoadSynchronous();
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
