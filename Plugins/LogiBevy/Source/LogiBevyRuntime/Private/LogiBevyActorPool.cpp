// Copyright LogiUE. All Rights Reserved.

#include "LogiBevyActorPool.h"
#include "LogiBevyAssetRegistry.h"
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
	for (TWeakObjectPtr<AStaticMeshActor>& Weak : Pool)
	{
		if (AStaticMeshActor* Actor = Weak.Get())
		{
			Actor->Destroy();
		}
	}
	EntityToActor.Reset();
	Pool.Reset();
	AssetRegistry = nullptr; // borrowed; clear so a stale ptr is never read post-shutdown
	World.Reset();
}

FVector FLogiActorPool::TranslationOf(const LogiActorUpdate& Update)
{
	// Row-major 3x4 affine: translation is column 3 of each row -> indices 3, 7, 11.
	return FVector(Update.transform_3x4[3], Update.transform_3x4[7], Update.transform_3x4[11]);
}

UStaticMesh* FLogiActorPool::ResolveMeshFor(const LogiActorUpdate& Update) const
{
	// Best-effort static-mesh resolution for the actor tier through the GC-rooted
	// registry (audit H4): meshes authored for static-actor-tier entities are
	// registered under the same stable hash the Rust side emits as `actor_class`.
	// Returns null (caller falls back to the engine cube) when unregistered or not
	// yet resident. Skeletal / full actor_class spawning remains a separate TODO.
	if (AssetRegistry != nullptr && Update.actor_class.stable_hash != 0)
	{
		return AssetRegistry->ResolveStaticMesh(Update.actor_class.stable_hash);
	}
	return nullptr;
}

AStaticMeshActor* FLogiActorPool::SpawnActor(const FVector& Location, const LogiActorUpdate& Update)
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
		MeshComp->SetMobility(EComponentMobility::Movable);
		// Resolve from the registry (non-blocking after PreloadAll); fall back to the
		// engine cube so the actor is always visible. TODO: skeletal_mesh / anim path.
		UStaticMesh* Mesh = ResolveMeshFor(Update);
		if (Mesh == nullptr)
		{
			Mesh = LoadObject<UStaticMesh>(nullptr, TEXT("/Engine/BasicShapes/Cube.Cube"));
		}
		if (Mesh != nullptr)
		{
			MeshComp->SetStaticMesh(Mesh);
		}
	}

	Actor->FinishSpawning(SpawnTransform);
	return Actor;
}

AStaticMeshActor* FLogiActorPool::AcquireActor(const FVector& Location, const LogiActorUpdate& Update)
{
	// Reuse a pooled (hidden) actor if available; skip any GC'd entries.
	while (Pool.Num() > 0)
	{
		TWeakObjectPtr<AStaticMeshActor> Weak = Pool.Pop(EAllowShrinking::No);
		if (AStaticMeshActor* Actor = Weak.Get())
		{
			Actor->SetActorHiddenInGame(false);
			Actor->SetActorLocation(Location);
			return Actor;
		}
	}
	return SpawnActor(Location, Update);
}

void FLogiActorPool::HideAndPool(AStaticMeshActor* Actor)
{
	if (Actor == nullptr)
	{
		return;
	}
	// Cap the free pool (audit): without a bound, a long session of promote→demote churn accumulates
	// unbounded hidden actors. Steady state never needs more pooled actors than the actor budget
	// (HighRes+LowRes, ~320 by default), so destroy beyond a generous cap rather than leak. The rare
	// destroys at the cap boundary are far cheaper than the per-frame spawn/destroy churn the pool avoids.
	// (Hidden — not destroyed — actors stay in the world's actor list, so the world GC-roots them; the
	// TWeakObjectPtrs are defensive against world teardown, not load-bearing.)
	constexpr int32 MaxPooledActors = 512;
	if (Pool.Num() >= MaxPooledActors)
	{
		Actor->Destroy();
		return;
	}
	Actor->SetActorHiddenInGame(true);
	Pool.Add(Actor);
}

void FLogiActorPool::ApplyActorUpdate(const LogiActorUpdate& Update)
{
	const uint64 Entity = Update.entity.index;
	// Snapshot transforms are local to the frame origin (P0 #6); rebase to world.
	const FVector Location = FrameOrigin + TranslationOf(Update);

	TWeakObjectPtr<AStaticMeshActor>& Slot = EntityToActor.FindOrAdd(Entity);
	if (!Slot.IsValid())
	{
		Slot = AcquireActor(Location, Update);
	}
	if (AStaticMeshActor* Actor = Slot.Get())
	{
		Actor->SetActorLocation(Location);
		// TODO: rotation/scale from transform_3x4; skeletal mesh from Update.skeletal_mesh.
	}
}

void FLogiActorPool::ReleaseEntity(uint64 EntityIndex)
{
	TWeakObjectPtr<AStaticMeshActor> Removed;
	if (EntityToActor.RemoveAndCopyValue(EntityIndex, Removed))
	{
		HideAndPool(Removed.Get());
	}
}
