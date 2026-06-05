// Copyright LogiUE. All Rights Reserved.

#pragma once

#include "CoreMinimal.h"
#include "UObject/WeakObjectPtr.h"
#include "logi_bevy_ffi.h"

class UWorld;
class AActor;
class UInstancedStaticMeshComponent;
class ULogiBevyAssetRegistry;

/** Identifies a retained instancing bucket: one ISM per mesh/material/LOD. */
struct FLogiBucketKey
{
	uint64 Mesh = 0;
	uint64 Material = 0;
	uint32 Lod = 0;

	bool operator==(const FLogiBucketKey& Other) const
	{
		return Mesh == Other.Mesh && Material == Other.Material && Lod == Other.Lod;
	}
};

FORCEINLINE uint32 GetTypeHash(const FLogiBucketKey& Key)
{
	return HashCombine(HashCombine(GetTypeHash(Key.Mesh), GetTypeHash(Key.Material)), GetTypeHash(Key.Lod));
}

/**
 * Retained instanced-static-mesh renderer (§8 / audit #2): one ISM component per
 * mesh/material/LOD bucket, all owned by a single transient host actor (no
 * per-entity actor). A stable entity->slot table + per-bucket free-list keeps
 * instance indices stable across frames; despawns HIDE (zero-scale) and recycle a
 * slot rather than RemoveInstance (which would renumber every later instance).
 * Instances are uploaded in world space = frame origin + rebased local (P0 #6).
 *
 * Mesh/material come from the bucket handle via ULogiBevyAssetRegistry (TODO:
 * wired next); a default cube stands in until then.
 */
class FLogiInstancedRenderer
{
public:
	void Initialize(UWorld* InWorld);
	void Shutdown();

	/// Local-frame origin (world cm) for the current snapshot (P0 #6).
	void SetFrameOrigin(const FVector& InOrigin) { FrameOrigin = InOrigin; }

	/// Registry resolving bucket handles -> UStaticMesh / UMaterial (cube fallback).
	/// Borrowed: the owning subsystem GC-roots it (audit H4), so a raw pointer is
	/// safe for this pimpl's lifetime -- no weak ptr (which would NOT keep it alive).
	void SetAssetRegistry(ULogiBevyAssetRegistry* InRegistry) { AssetRegistry = InRegistry; }

	void ApplyBatch(const LogiStaticBatch& Batch);
	void ApplyDespawn(uint64 EntityIndex);
	/// Flush deferred render-state dirties once per touched ISM. Call after ALL ApplyBatch/ApplyDespawn
	/// for the frame (audit H4: despawn/bucket-switch must not flush render state per hidden instance).
	void FlushDirty();
	void Reset();

	int32 LiveInstanceCount() const { return EntityToSlot.Num(); }
	int32 BucketCount() const { return Buckets.Num(); }

private:
	struct FBucket
	{
		TWeakObjectPtr<UInstancedStaticMeshComponent> Ism;
		int32 HighWater = 0;     // next never-used instance index
		TArray<int32> FreeSlots; // hidden, recycled indices

		int32 Acquire(bool& bOutIsNew)
		{
			if (FreeSlots.Num() > 0) { bOutIsNew = false; return FreeSlots.Pop(); }
			bOutIsNew = true;
			return HighWater++;
		}
		void Release(int32 Slot) { FreeSlots.Add(Slot); }
	};

	struct FSlotRef
	{
		FLogiBucketKey Key;
		int32 Slot = INDEX_NONE;
	};

	UInstancedStaticMeshComponent* GetOrCreateComponent(const FLogiBucketKey& Key);

	TWeakObjectPtr<UWorld> World;
	TWeakObjectPtr<AActor> HostActor;
	ULogiBevyAssetRegistry* AssetRegistry = nullptr; // borrowed; subsystem GC-roots it (H4)
	FVector FrameOrigin = FVector::ZeroVector;
	TMap<FLogiBucketKey, FBucket> Buckets;
	TMap<uint64 /*entity index*/, FSlotRef> EntityToSlot;
	/// ISMs touched by a hide this frame; FlushDirty marks each render-state dirty exactly once
	/// (audit H4) instead of once per hidden/despawned instance.
	TSet<TWeakObjectPtr<UInstancedStaticMeshComponent>> DirtyIsms;
};
