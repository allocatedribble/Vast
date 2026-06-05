// Copyright LogiUE. All Rights Reserved.

#pragma once

#include "CoreMinimal.h"
#include "Subsystems/WorldSubsystem.h"
#include "UObject/StrongObjectPtr.h"
#include "LogiBakedTerrainSubsystem.generated.h"

class AActor;
class UProceduralMeshComponent;
class UStaticMeshComponent;
class UMaterialInterface;

// Opaque Rust baked-world handle (logi_terrain_ffi.h). Forward-declared so this header stays free of the
// C ABI; the .cpp includes the real header under LOGI_BEVY_HAVE_TERRAIN.
struct LogiBakedWorld;

/**
 * HE-0 (ADR-0011): renders an offline-baked terrain tile store (the `Tools/logi_bake` output) through the
 * native Rust `logi_baked_*` FFI. A render/host SHELL over the Rust-owned tiles — not gameplay authority
 * (ADR-0005/0007). Gated on `bShowBakedTerrain`; runs in game/PIE + the editor level-viewport.
 *
 * Streams **per tick**: each step feeds the active camera to `logi_baked_world_step` (distance-culled to
 * `BakedTerrainMaxDistanceM`) and caches one render unit per `tile_key`, building newly-arrived tiles and
 * evicting departed ones. Two render lanes, by `bBakedTerrainNaniteDisplacement`:
 *   - **proc-mesh** (default): a section on one UProceduralMeshComponent.
 *   - **Nanite** (editor-only, ADR-0008): a transient Nanite UStaticMesh per tile via a pooled
 *     UStaticMeshComponent — so a Nanite-tessellation displacement material actually tessellates.
 * Meshing mode + TIN error + skirt depth are passed to Rust at world creation (LogiBakedCreateInfo).
 */
UCLASS()
class LOGIBEVYRUNTIME_API ULogiBakedTerrainSubsystem : public UTickableWorldSubsystem
{
	GENERATED_BODY()

public:
	virtual void Deinitialize() override;
	virtual bool ShouldCreateSubsystem(UObject* Outer) const override;

	virtual void Tick(float DeltaTime) override;
	virtual TStatId GetStatId() const override;
	virtual bool IsTickableInEditor() const override { return true; }

private:
	/** One resident tile: a proc-mesh section, or (Nanite lane) a static-mesh component. */
	struct FBakedTileEntry
	{
		int32 Section = INDEX_NONE; // proc-mesh section index, or INDEX_NONE on the Nanite lane
		uint64 LastSeen = 0;
		TStrongObjectPtr<UStaticMeshComponent> NaniteComp; // GC-rooted (Nanite lane only)
	};

	bool EnsureWorld();
	void EnsureHost();
	void ReleaseWorld();
	void EvictDeparted(const TSet<uint64>& Resident);

	TMap<uint64, FBakedTileEntry> Tiles;
	TArray<int32> FreeSections;
	int32 NextSection = 0;
	uint64 FrameCounter = 0;
	float StepAccum = 0.0f;
	bool bCreateAttempted = false;
	bool bUseNaniteLane = false; // resolved at world creation (toggle + editor support)

	TWeakObjectPtr<AActor> HostActor;
	TWeakObjectPtr<UProceduralMeshComponent> ProcMesh;
	TStrongObjectPtr<UMaterialInterface> TerrainMaterial; // GC-roots a displacement MID (or the asset)

	LogiBakedWorld* BakedWorld = nullptr;

	// Reused per-step scratch (keeps capacity).
	TSet<uint64> ResidentScratch;
	TArray<uint64> DepartedScratch;
};
