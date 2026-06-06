// Copyright LogiUE. All Rights Reserved.

#pragma once

#include "CoreMinimal.h"
#include "UObject/WeakObjectPtr.h"
#include "UObject/StrongObjectPtr.h"
#include "logi_bevy_ffi.h"

class UWorld;
class AActor;
class UStaticMesh;
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
 * Mesh/material are resolved from the bucket handle via ULogiBevyAssetRegistry;
 * a default engine cube stands in for any handle not yet resident.
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

	/// Entities currently mapped to a slot. NOT the live instance count: hidden/recycled (HighWater +
	/// FreeSlots) slots are not subtracted, so this is the tracked-entity count, not rendered instances.
	int32 TrackedEntityCount() const { return EntityToSlot.Num(); }
	int32 BucketCount() const { return Buckets.Num(); }

private:
	struct FBucket
	{
		// Strong ref (audit H11): the host actor already GC-roots the ISM, so a weak ptr bought nothing
		// but exposed a desync -- if the ISM were collected while the bucket survived, GetOrCreateComponent
		// would mint a fresh 0-instance ISM while HighWater/FreeSlots/EntityToSlot still pointed at the old
		// (now-gone) slots, breaking the "AddInstance index == Slot" invariant. Owning it here (in addition
		// to the host) keeps slot indices and the component in lockstep for the bucket's whole lifetime.
		TStrongObjectPtr<UInstancedStaticMeshComponent> Ism;
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
	UStaticMesh* GetCubeFallback();

	TWeakObjectPtr<UWorld> World;
	TWeakObjectPtr<AActor> HostActor;
	ULogiBevyAssetRegistry* AssetRegistry = nullptr; // borrowed; subsystem GC-roots it (H4)
	/// Engine-cube fallback, loaded once and GC-rooted (audit C++ L6): a LoadObject per new bucket was a
	/// needless synchronous lookup. Resolved lazily on the first fallback.
	TStrongObjectPtr<UStaticMesh> CubeFallback;
	FVector FrameOrigin = FVector::ZeroVector;
	TMap<FLogiBucketKey, FBucket> Buckets;
	TMap<uint64 /*entity index*/, FSlotRef> EntityToSlot;
	/// ISMs touched by a hide this frame; FlushDirty marks each render-state dirty exactly once
	/// (audit H4) instead of once per hidden/despawned instance.
	TSet<TWeakObjectPtr<UInstancedStaticMeshComponent>> DirtyIsms;
};
