// Copyright LogiUE. All Rights Reserved.

#include "LogiBevyActorPool.h"
#include "Engine/World.h"
#include "Engine/StaticMeshActor.h"
#include "Engine/StaticMesh.h"
#include "Components/StaticMeshComponent.h"

void FLogiActorPool::Initialize(UWorld* InWorld)
{
	World = InWorld;
}

void FLogiActorPool::Shutdown()
{
	for (TPair<uint64, TWeakObjectPtr<AStaticMeshActor>>& Pair : EntityToActor)
	{
		if (AStaticMeshActor* Actor = Pair.Value.Get())
		{
			Actor->Destroy();
		}
	}
	EntityToActor.Reset();
	World.Reset();
}

FVector FLogiActorPool::TranslationOf(const LogiActorUpdate& Update)
{
	// Row-major 3x4 affine: translation is column 3 of each row -> indices 3, 7, 11.
	return FVector(Update.transform_3x4[3], Update.transform_3x4[7], Update.transform_3x4[11]);
}

AStaticMeshActor* FLogiActorPool::SpawnActor(const FVector& Location)
{
	UWorld* W = World.Get();
	if (W == nullptr)
	{
		return nullptr;
	}

	const FTransform SpawnTransform(Location);
	AStaticMeshActor* Actor = W->SpawnActorDeferred<AStaticMeshActor>(
		AStaticMeshActor::StaticClass(), SpawnTransform, nullptr, nullptr,
		ESpawnActorCollisionHandlingMethod::AlwaysSpawn);
	if (Actor == nullptr)
	{
		return nullptr;
	}

	if (UStaticMeshComponent* MeshComp = Actor->GetStaticMeshComponent())
	{
		// Configure before FinishSpawning so a Movable static-mesh actor is valid.
		MeshComp->SetMobility(EComponentMobility::Movable);
		if (UStaticMesh* Cube = LoadObject<UStaticMesh>(nullptr, TEXT("/Engine/BasicShapes/Cube.Cube")))
		{
			MeshComp->SetStaticMesh(Cube);
		}
	}

	Actor->FinishSpawning(SpawnTransform);
	return Actor;
}

void FLogiActorPool::ApplyActorUpdate(const LogiActorUpdate& Update)
{
	const uint64 Entity = Update.entity.index;
	const FVector Location = TranslationOf(Update);

	TWeakObjectPtr<AStaticMeshActor>& Slot = EntityToActor.FindOrAdd(Entity);
	if (!Slot.IsValid())
	{
		Slot = SpawnActor(Location);
	}
	if (AStaticMeshActor* Actor = Slot.Get())
	{
		Actor->SetActorLocation(Location);
		// TODO: rotation/scale from transform_3x4; skeletal mesh from Update.skeletal_mesh
		// via ULogiBevyAssetRegistry; animation deferred (per user request).
	}
}

void FLogiActorPool::ReleaseEntity(uint64 EntityIndex)
{
	TWeakObjectPtr<AStaticMeshActor> Removed;
	if (EntityToActor.RemoveAndCopyValue(EntityIndex, Removed))
	{
		if (AStaticMeshActor* Actor = Removed.Get())
		{
			Actor->Destroy();
		}
	}
}
