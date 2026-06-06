// Copyright LogiUE. All Rights Reserved.

#include "LogiBevyActorPool.h"
#include "LogiBevyAssetRegistry.h"
#include "LogiBevyTransform.h" // LogiBevy::LogiWorldTransformFrom3x4 (shared with the ISM lane, audit M1)
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
	CubeFallback.Reset(); // drop the GC root on the cached engine cube
	AssetRegistry = nullptr; // borrowed; clear so a stale ptr is never read post-shutdown
	World.Reset();
}

UStaticMesh* FLogiActorPool::GetCubeFallback()
{
	if (!CubeFallback.IsValid())
	{
		CubeFallback.Reset(LoadObject<UStaticMesh>(nullptr, TEXT("/Engine/BasicShapes/Cube.Cube")));
	}
	return CubeFallback.Get();
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

AStaticMeshActor* FLogiActorPool::SpawnActor(const FTransform& Xform, const LogiActorUpdate& Update)
{
	UWorld* W = World.Get();
	if (W == nullptr)
	{
		return nullptr;
	}

	AStaticMeshActor* Actor = W->SpawnActorDeferred<AStaticMeshActor>(
		AStaticMeshActor::StaticClass(), Xform, nullptr, nullptr,
		ESpawnActorCollisionHandlingMethod::AlwaysSpawn);
	if (Actor == nullptr)
	{
		return nullptr;
	}

	if (UStaticMeshComponent* MeshComp = Actor->GetStaticMeshComponent())
	{
		MeshComp->SetMobility(EComponentMobility::Movable);
		// Resolve from the registry (non-blocking after PreloadAll); fall back to the cached engine cube
		// (audit C++ L6) so the actor is always visible.
		UStaticMesh* Mesh = ResolveMeshFor(Update);
		if (Mesh == nullptr)
		{
			Mesh = GetCubeFallback();
		}
		if (Mesh != nullptr)
		{
			MeshComp->SetStaticMesh(Mesh);
		}
	}

	Actor->FinishSpawning(Xform);
	return Actor;
}

AStaticMeshActor* FLogiActorPool::AcquireActor(const FTransform& Xform, const LogiActorUpdate& Update)
{
	// Reuse a pooled (hidden) actor if available; skip any GC'd entries.
	while (Pool.Num() > 0)
	{
		TWeakObjectPtr<AStaticMeshActor> Weak = Pool.Pop(EAllowShrinking::No);
		if (AStaticMeshActor* Actor = Weak.Get())
		{
			Actor->SetActorHiddenInGame(false);
			Actor->SetActorTransform(Xform); // full rotation+scale (audit M1), not just location
			return Actor;
		}
	}
	return SpawnActor(Xform, Update);
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
	// Snapshot transforms are local to the frame origin (P0 #6); the shared decoder rebases translation by
	// the origin and applies the full rotation*scale (audit M1).
	const FTransform Xform = LogiBevy::LogiWorldTransformFrom3x4(Update.transform_3x4, FrameOrigin);

	if (TWeakObjectPtr<AStaticMeshActor>* Existing = EntityToActor.Find(Entity))
	{
		if (AStaticMeshActor* Actor = Existing->Get())
		{
			Actor->SetActorTransform(Xform);
			return;
		}
		// The tracked actor was GC'd out from under us; re-acquire below and refresh the map entry.
		EntityToActor.Remove(Entity);
	}

	// Acquire into a local and only record it on success (audit M3): a failed acquire (no world / spawn
	// refused) must NOT leave a permanent null entry that suppresses re-acquire and re-spawns every frame.
	// AcquireActor already places the actor at Xform (pooled reuse + deferred spawn both do).
	if (AStaticMeshActor* Actor = AcquireActor(Xform, Update))
	{
		EntityToActor.Add(Entity, Actor);
	}
	// TODO(skeletal): drive a USkeletalMeshComponent from Update.skeletal_mesh + Update.anim_blueprint
	// once the actor tier supports skeletal actors; the static-mesh tier needs no anim wiring.
}

void FLogiActorPool::ReleaseEntity(uint64 EntityIndex)
{
	TWeakObjectPtr<AStaticMeshActor> Removed;
	if (EntityToActor.RemoveAndCopyValue(EntityIndex, Removed))
	{
		HideAndPool(Removed.Get());
	}
}
