// Copyright LogiUE. All Rights Reserved.

#pragma once

#include "CoreMinimal.h"
#include "UObject/WeakObjectPtr.h"
#include "logi_bevy_ffi.h"

class UWorld;
class AStaticMeshActor;

/**
 * Pooled actor representation for entities promoted to actor tiers (significant ->
 * full actor; everything else stays Bevy-only as ISM/data; ADR-0003). Actors are
 * non-authoritative shells synced from the snapshot. Demotion/despawn HIDES the
 * actor and returns it to a free pool (no spawn/destroy churn as entities cross the
 * budget boundary while the camera moves); acquisition reuses a pooled actor or
 * spawns one on demand. No gameplay writes back to Rust.
 */
class FLogiActorPool
{
public:
	void Initialize(UWorld* InWorld);
	void Shutdown();

	/// Local-frame origin (world cm) for the current snapshot (P0 #6).
	void SetFrameOrigin(const FVector& InOrigin) { FrameOrigin = InOrigin; }

	void ApplyActorUpdate(const LogiActorUpdate& Update);
	void ReleaseEntity(uint64 EntityIndex);

	int32 AssignedCount() const { return EntityToActor.Num(); }
	int32 PooledCount() const { return Pool.Num(); }

private:
	TWeakObjectPtr<UWorld> World;
	FVector FrameOrigin = FVector::ZeroVector;
	TMap<uint64 /*entity index*/, TWeakObjectPtr<AStaticMeshActor>> EntityToActor;
	TArray<TWeakObjectPtr<AStaticMeshActor>> Pool;

	AStaticMeshActor* AcquireActor(const FVector& Location);
	void HideAndPool(AStaticMeshActor* Actor);
	AStaticMeshActor* SpawnActor(const FVector& Location);
	static FVector TranslationOf(const LogiActorUpdate& Update);
};
