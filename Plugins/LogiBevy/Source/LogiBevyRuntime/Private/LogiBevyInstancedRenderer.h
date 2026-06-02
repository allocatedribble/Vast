// Copyright LogiUE. All Rights Reserved.

#pragma once

#include "CoreMinimal.h"
#include "logi_bevy_ffi.h"

/** Identifies a retained instancing bucket: one ISM/HISM per mesh/material/LOD. */
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
 * Retained instanced-static-mesh renderer (§8): one logical bucket per
 * mesh/material/LOD, a stable entity->slot table, and a per-bucket free-list.
 * Applies batched instance deltas; keeps slots stable across frames.
 *
 * (UObject HISM creation, asset resolution, and transform upload are TODOs
 * pending the asset-handle->UObject bridge; the retained bookkeeping is real.)
 */
class FLogiInstancedRenderer
{
public:
	void ApplyBatch(const LogiStaticBatch& Batch);
	void ApplyDespawn(uint64 EntityIndex);
	void Reset();

	int32 LiveInstanceCount() const { return EntityToSlot.Num(); }
	int32 BucketCount() const { return Buckets.Num(); }

private:
	struct FBucket
	{
		int32 HighWater = 0;     // next never-used slot
		TArray<int32> FreeSlots; // recycled slots

		int32 Acquire() { return FreeSlots.Num() > 0 ? FreeSlots.Pop() : HighWater++; }
		void Release(int32 Slot) { FreeSlots.Add(Slot); }
	};

	struct FSlotRef
	{
		FLogiBucketKey Key;
		int32 Slot = INDEX_NONE;
	};

	TMap<FLogiBucketKey, FBucket> Buckets;
	TMap<uint64 /*entity index*/, FSlotRef> EntityToSlot;
};
