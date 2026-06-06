// Copyright LogiUE. All Rights Reserved.

#pragma once

#include "CoreMinimal.h"
#include "UObject/WeakObjectPtr.h"
#include "UObject/StrongObjectPtr.h"
#include "logi_bevy_ffi.h"

class UWorld;
class AStaticMeshActor;
class UStaticMesh;
class ULogiBevyAssetRegistry;

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

	/// Registry resolving asset handles -> meshes/classes (cube fallback). Borrowed:
	/// the owning subsystem GC-roots it (audit H4), so a raw pointer is safe here.
	void SetAssetRegistry(ULogiBevyAssetRegistry* InRegistry) { AssetRegistry = InRegistry; }

	void ApplyActorUpdate(const LogiActorUpdate& Update);
	void ReleaseEntity(uint64 EntityIndex);

	int32 AssignedCount() const { return EntityToActor.Num(); }
	int32 PooledCount() const { return Pool.Num(); }

private:
	TWeakObjectPtr<UWorld> World;
	ULogiBevyAssetRegistry* AssetRegistry = nullptr; // borrowed; subsystem GC-roots it (H4)
	FVector FrameOrigin = FVector::ZeroVector;
	TMap<uint64 /*entity index*/, TWeakObjectPtr<AStaticMeshActor>> EntityToActor;
	TArray<TWeakObjectPtr<AStaticMeshActor>> Pool;
	/// Engine-cube fallback, loaded once and GC-rooted (audit C++ L6): a LoadObject per spawned actor was a
	/// needless synchronous registry lookup. Resolved lazily on the first fallback.
	TStrongObjectPtr<UStaticMesh> CubeFallback;

	UStaticMesh* GetCubeFallback();
	AStaticMeshActor* AcquireActor(const FTransform& Xform, const LogiActorUpdate& Update);
	void HideAndPool(AStaticMeshActor* Actor);
	AStaticMeshActor* SpawnActor(const FTransform& Xform, const LogiActorUpdate& Update);
	UStaticMesh* ResolveMeshFor(const LogiActorUpdate& Update) const;
};
