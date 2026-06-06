// Copyright LogiUE. All Rights Reserved.

#include "LogiBevyInstancedRenderer.h"
#include "LogiBevyAssetRegistry.h"
#include "LogiBevyTransform.h" // LogiBevy::LogiWorldTransformFrom3x4 (shared with the actor pool, audit M1)
#include "Engine/World.h"
#include "GameFramework/Actor.h"
#include "Engine/StaticMesh.h"
#include "Materials/MaterialInterface.h"
#include "Components/SceneComponent.h"
#include "Components/InstancedStaticMeshComponent.h"

namespace
{
	/// Zero-scale transform: collapses a recycled/despawned instance to a point.
	FTransform HiddenTransform()
	{
		return FTransform(FQuat::Identity, FVector::ZeroVector, FVector::ZeroVector);
	}
}

void FLogiInstancedRenderer::Initialize(UWorld* InWorld)
{
	World = InWorld;
	if (InWorld == nullptr)
	{
		return;
	}

	FActorSpawnParameters Params;
	Params.ObjectFlags |= RF_Transient;
	AActor* Host = InWorld->SpawnActor<AActor>(AActor::StaticClass(), FTransform::Identity, Params);
	if (Host != nullptr)
	{
		USceneComponent* Root = NewObject<USceneComponent>(Host, TEXT("LogiInstancedRoot"));
		Host->SetRootComponent(Root);
		Root->RegisterComponent();
	}
	HostActor = Host;
}

void FLogiInstancedRenderer::Shutdown()
{
	if (AActor* Host = HostActor.Get())
	{
		Host->Destroy();
	}
	HostActor.Reset();
	Buckets.Reset();
	EntityToSlot.Reset();
	DirtyIsms.Reset();
	CubeFallback.Reset(); // drop the GC root on the cached engine cube
	AssetRegistry = nullptr; // borrowed; clear so a stale ptr is never read post-shutdown
	World.Reset();
}

UStaticMesh* FLogiInstancedRenderer::GetCubeFallback()
{
	if (!CubeFallback.IsValid())
	{
		CubeFallback.Reset(LoadObject<UStaticMesh>(nullptr, TEXT("/Engine/BasicShapes/Cube.Cube")));
	}
	return CubeFallback.Get();
}

UInstancedStaticMeshComponent* FLogiInstancedRenderer::GetOrCreateComponent(const FLogiBucketKey& Key)
{
	FBucket& Bucket = Buckets.FindOrAdd(Key);
	if (Bucket.Ism.IsValid())
	{
		return Bucket.Ism.Get();
	}

	// The bucket has no live ISM. With the strong ref (audit H11) this only happens on the FIRST request
	// for a key; the ISM cannot vanish under a surviving bucket afterwards. Reset the slot bookkeeping
	// anyway so a fresh ISM always starts dense from index 0 -- defends the "AddInstance index == Slot"
	// invariant even if a future change ever recreates a bucket's component.
	Bucket.HighWater = 0;
	Bucket.FreeSlots.Reset();
	for (auto It = EntityToSlot.CreateIterator(); It; ++It)
	{
		if (It.Value().Key == Key)
		{
			It.RemoveCurrent(); // drop entity->slot maps into the now-gone ISM (they re-acquire on next ApplyBatch)
		}
	}

	AActor* Host = HostActor.Get();
	if (Host == nullptr)
	{
		return nullptr;
	}

	UInstancedStaticMeshComponent* Ism = NewObject<UInstancedStaticMeshComponent>(Host);
	Ism->SetMobility(EComponentMobility::Movable);
	Ism->SetupAttachment(Host->GetRootComponent());
	Ism->NumCustomDataFloats = 8;

	// Resolve mesh/material from the registry (non-blocking after PreloadAll);
	// fall back to the engine cube so the bucket is always visible.
	UStaticMesh* Mesh = nullptr;
	UMaterialInterface* Material = nullptr;
	if (ULogiBevyAssetRegistry* Reg = AssetRegistry)
	{
		Mesh = Reg->ResolveStaticMesh(Key.Mesh);
		Material = Reg->ResolveMaterial(Key.Material);
	}
	if (Mesh == nullptr)
	{
		Mesh = GetCubeFallback(); // cached engine cube (audit C++ L6)
	}
	if (Mesh != nullptr)
	{
		Ism->SetStaticMesh(Mesh);
	}
	if (Material != nullptr)
	{
		Ism->SetMaterial(0, Material);
	}
	Ism->RegisterComponent();
	Bucket.Ism.Reset(Ism);
	return Ism;
}

void FLogiInstancedRenderer::ApplyBatch(const LogiStaticBatch& Batch)
{
	TRACE_CPUPROFILER_EVENT_SCOPE(LogiBevy_IsmUpload);

	const FLogiBucketKey Key{ Batch.mesh.stable_hash, Batch.material.stable_hash, Batch.lod };
	UInstancedStaticMeshComponent* Ism = GetOrCreateComponent(Key);
	if (Ism == nullptr)
	{
		return;
	}
	FBucket& Bucket = Buckets.FindChecked(Key);

	for (uint32 Index = 0; Index < Batch.instance_count; ++Index)
	{
		const LogiInstanceUpdate& Inst = Batch.instances[Index];
		const uint64 Entity = Inst.entity.index;
		const FTransform Xform = LogiBevy::LogiWorldTransformFrom3x4(Inst.transform_3x4, FrameOrigin);

		int32 Slot = INDEX_NONE;
		bool bIsNew = false;
		if (FSlotRef* Existing = EntityToSlot.Find(Entity))
		{
			if (Existing->Key == Key)
			{
				Slot = Existing->Slot; // retained: keep the slot
			}
			else
			{
				// Entity changed bucket: hide + release the old slot, acquire a new one.
				if (FBucket* OldBucket = Buckets.Find(Existing->Key))
				{
					if (UInstancedStaticMeshComponent* OldIsm = OldBucket->Ism.Get())
					{
						OldIsm->UpdateInstanceTransform(Existing->Slot, HiddenTransform(), /*bWorldSpace=*/true, /*bMarkRenderStateDirty=*/false, /*bTeleport=*/true);
						DirtyIsms.Add(OldIsm); // defer the flush (audit H4)
					}
					OldBucket->Release(Existing->Slot);
				}
				Slot = Bucket.Acquire(bIsNew);
				Existing->Key = Key;
				Existing->Slot = Slot;
			}
		}
		else
		{
			Slot = Bucket.Acquire(bIsNew);
			EntityToSlot.Add(Entity, FSlotRef{ Key, Slot });
		}

		if (bIsNew)
		{
			// New slots are acquired densely (HighWater), so AddInstance's returned
			// index matches Slot (we never RemoveInstance, only hide+recycle).
			const int32 Added = Ism->AddInstance(Xform, /*bWorldSpace=*/true);
			checkSlow(Added == Slot);
			(void)Added;
		}
		else
		{
			Ism->UpdateInstanceTransform(Slot, Xform, /*bWorldSpace=*/true, /*bMarkRenderStateDirty=*/false, /*bTeleport=*/true);
		}

		// Bulk-write all 8 custom-data floats in one call (audit H4): eight per-instance virtual
		// SetCustomDataValue calls × up to 100k instances/frame was a dominant per-frame cost.
		Ism->SetCustomData(Slot, MakeArrayView(Inst.custom_data, 8), /*bMarkRenderStateDirty=*/false);
	}

	// One render-state flush per bucket per frame (not per instance). This ISM is now clean, so drop it
	// from the deferred set (a despawn earlier this frame may have added it).
	Ism->MarkRenderStateDirty();
	DirtyIsms.Remove(Ism);
}

void FLogiInstancedRenderer::ApplyDespawn(uint64 EntityIndex)
{
	FSlotRef Ref;
	if (EntityToSlot.RemoveAndCopyValue(EntityIndex, Ref))
	{
		if (FBucket* Bucket = Buckets.Find(Ref.Key))
		{
			if (UInstancedStaticMeshComponent* Ism = Bucket->Ism.Get())
			{
				Ism->UpdateInstanceTransform(Ref.Slot, HiddenTransform(), /*bWorldSpace=*/true, /*bMarkRenderStateDirty=*/false, /*bTeleport=*/true);
					DirtyIsms.Add(Ism); // defer the flush (audit H4): no per-instance render-state recreate
			}
			Bucket->Release(Ref.Slot);
		}
	}
}

void FLogiInstancedRenderer::FlushDirty()
{
	// One MarkRenderStateDirty per touched ISM (audit H4), not one per hidden/despawned instance.
	for (const TWeakObjectPtr<UInstancedStaticMeshComponent>& Weak : DirtyIsms)
	{
		if (UInstancedStaticMeshComponent* Ism = Weak.Get())
		{
			Ism->MarkRenderStateDirty();
		}
	}
	DirtyIsms.Reset();
}

void FLogiInstancedRenderer::Reset()
{
	Buckets.Reset();
	EntityToSlot.Reset();
	DirtyIsms.Reset();
}
