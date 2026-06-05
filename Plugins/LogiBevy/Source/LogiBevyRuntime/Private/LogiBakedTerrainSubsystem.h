// Copyright LogiUE. All Rights Reserved.

#pragma once

#include "CoreMinimal.h"
#include "Subsystems/WorldSubsystem.h"
#include "LogiBakedTerrainSubsystem.generated.h"

class AActor;
class UProceduralMeshComponent;

// Opaque Rust baked-world handle (logi_terrain_ffi.h). Forward-declared so this header stays free of the
// C ABI; the .cpp includes the real header under LOGI_BEVY_HAVE_TERRAIN.
struct LogiBakedWorld;

/**
 * HE-0 (ADR-0011, Milestone HE): loads an offline-baked **terrain** tile store (the `Tools/logi_bake`
 * output) through the native Rust `logi_baked_*` FFI and draws each tile as a UProceduralMeshComponent
 * section under a transient host actor. A render/host SHELL over the Rust-owned baked tiles -- not
 * gameplay authority (ADR-0005/0007). HE-0 serves the whole AOI statically (no camera streaming yet), so
 * this loads once on the first tick. Gated on the `bShowBakedTerrain` dev setting; runs in game/PIE and
 * the editor level-viewport. Tiles are already in Unreal render space (cm, Z-up LH), pinned at the ENU
 * origin (world 0,0,0) -- copy straight in (the globe/building lanes' winding lesson).
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
	void LoadAndBuild();

	bool bLoaded = false;
	TWeakObjectPtr<AActor> HostActor;
};
