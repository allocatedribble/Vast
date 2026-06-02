// Copyright LogiUE. All Rights Reserved.

#include "LogiBevyInstancedRenderer.h"

void FLogiInstancedRenderer::ApplyBatch(const LogiStaticBatch& Batch)
{
	const FLogiBucketKey Key{ Batch.mesh.stable_hash, Batch.material.stable_hash, Batch.lod };
	FBucket& Bucket = Buckets.FindOrAdd(Key);

	for (uint32 Index = 0; Index < Batch.instance_count; ++Index)
	{
		const LogiInstanceUpdate& Inst = Batch.instances[Index];
		const uint64 Entity = Inst.entity.index;

		int32 Slot;
		if (FSlotRef* Existing = EntityToSlot.Find(Entity))
		{
			if (Existing->Key == Key)
			{
				Slot = Existing->Slot; // retained: keep the slot
			}
			else
			{
				// Entity changed bucket: release the old slot, acquire a new one.
				if (FBucket* OldBucket = Buckets.Find(Existing->Key))
				{
					OldBucket->Release(Existing->Slot);
				}
				Slot = Bucket.Acquire();
				Existing->Key = Key;
				Existing->Slot = Slot;
			}
		}
		else
		{
			Slot = Bucket.Acquire();
			EntityToSlot.Add(Entity, FSlotRef{ Key, Slot });
		}

		// TODO: build an FMatrix from Inst.transform_3x4 and
		// Hism->UpdateInstanceTransform(Slot, Xform, true, false), then
		// Hism->SetCustomData(Slot, Inst.custom_data). Requires the HISM component
		// (created per bucket) + asset-handle->UStaticMesh/UMaterial resolution.
		(void)Slot;
	}
}

void FLogiInstancedRenderer::ApplyDespawn(uint64 EntityIndex)
{
	FSlotRef Ref;
	if (EntityToSlot.RemoveAndCopyValue(EntityIndex, Ref))
	{
		if (FBucket* Bucket = Buckets.Find(Ref.Key))
		{
			Bucket->Release(Ref.Slot);
			// TODO: zero/hide the instance transform at Ref.Slot.
		}
	}
}

void FLogiInstancedRenderer::Reset()
{
	Buckets.Reset();
	EntityToSlot.Reset();
}
