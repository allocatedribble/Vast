// Copyright LogiUE. All Rights Reserved.

#pragma once

#include "CoreMinimal.h"
#include "Subsystems/WorldSubsystem.h"
#include "UObject/StrongObjectPtr.h"
#include "LogiGlobeSubsystem.generated.h"

class AActor;
class UProceduralMeshComponent;
class UMaterialInterface;
class UMaterialInstanceDynamic;
class UTexture2D;

// Opaque Rust streaming-globe handle (logi_terrain_ffi.h). Forward-declared so this header stays free of
// the C ABI; the .cpp includes the real header under LOGI_BEVY_HAVE_TERRAIN.
struct LogiGlobeWorld;

/**
 * M2 (ADR-0009, Milestone F): streams Google Photorealistic 3D Tiles through the native Rust stack
 * (logi_globe_* FFI -- resolve ion, fetch + decode glTF on a worker pool, rebase into Unreal render
 * space) and draws each decoded tile as a UProceduralMeshComponent section with its own runtime-decoded
 * texture. A render/host SHELL over the Rust-owned tile world -- not gameplay authority (ADR-0005).
 *
 * Each step feeds the active viewport camera (PIE player, else the editor level-viewport) to Rust and
 * receives a zero-copy multi-tile frame; the subsystem caches one section + material per `tile_key` and
 * builds only newly-arrived tiles (throttled per step). Gated on `bShowPhotorealGlobe`; runs in game/PIE
 * and the editor level-viewport. The ion token is read transiently (env var / gitignored file).
 */
UCLASS()
class LOGIBEVYRUNTIME_API ULogiGlobeSubsystem : public UTickableWorldSubsystem
{
	GENERATED_BODY()

public:
	virtual void Deinitialize() override;
	virtual bool ShouldCreateSubsystem(UObject* Outer) const override;

	virtual void Tick(float DeltaTime) override;
	virtual TStatId GetStatId() const override;
	virtual bool IsTickableInEditor() const override { return true; }

private:
	/** One resident tile: its proc-mesh section + the texture/material keeping it textured (GC-rooted). */
	struct FGlobeTileEntry
	{
		int32 Section = 0;
		uint64 LastSeen = 0;
		TStrongObjectPtr<UTexture2D> Texture;
		TStrongObjectPtr<UMaterialInstanceDynamic> Material;
	};

	void EnsureHost();
	bool EnsureWorld();   // lazy-create the streaming world once (resolve ion + fetch root)
	void ReleaseWorld();
	void EvictIfOverBudget();

	TMap<uint64, FGlobeTileEntry> TileSection;
	TArray<int32> FreeSections;
	int32 NextSection = 0;
	uint64 FrameCounter = 0;

	TWeakObjectPtr<AActor> HostActor;
	TWeakObjectPtr<UProceduralMeshComponent> GlobeMesh;
	TWeakObjectPtr<UMaterialInterface> BaseMaterial;

	LogiGlobeWorld* GlobeWorld = nullptr;
	bool bCreateAttempted = false;
	bool bWarnedNoMaterial = false;
	float StepThrottleAccum = 0.0f; // ~30 Hz streaming step throttle, ALL worlds (audit H5)

	// Reused per-frame scratch (keeps capacity; no per-tick allocation).
	TSet<uint64> ResidentScratch;
	TArray<uint64> DepartedScratch;
};
