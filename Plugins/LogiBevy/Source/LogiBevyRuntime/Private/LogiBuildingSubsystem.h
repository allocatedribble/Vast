// Copyright LogiUE. All Rights Reserved.

#pragma once

#include "CoreMinimal.h"
#include "Subsystems/WorldSubsystem.h"
#include "LogiBuildingSubsystem.generated.h"

/**
 * M1 (ADR-0009, Milestone F): loads the AGI_HQ sample building from Content/AGI_HQ through the Rust
 * `logi_building_*` FFI (decode .dae + transform into Unreal render space) and draws it as a
 * UProceduralMeshComponent under a transient host actor. A keystone proof of the native 3D-Tiles
 * decode + geodetic-placement (+ texture) path -- not gameplay authority. Gated on the
 * `bShowAgiBuilding` dev setting; loads once on the first tick (game/PIE + the editor level-viewport).
 */
UCLASS()
class LOGIBEVYRUNTIME_API ULogiBuildingSubsystem : public UTickableWorldSubsystem
{
	GENERATED_BODY()

public:
	virtual void Initialize(FSubsystemCollectionBase& Collection) override;
	virtual void Deinitialize() override;
	virtual bool ShouldCreateSubsystem(UObject* Outer) const override;

	virtual void Tick(float DeltaTime) override;
	virtual TStatId GetStatId() const override;
	virtual bool IsTickableInEditor() const override { return true; }

private:
	void LoadAndBuild();
	/** Destroy the building host so the next tick reloads from current settings (or stays down if hidden). */
	void TearDown();

	bool bLoaded = false;
	TWeakObjectPtr<AActor> HostActor;

#if WITH_EDITOR
	/** Bound to ULogiBevyDeveloperSettings::OnSettingsChanged (editor): flags a reload when a building setting changes. */
	void OnSettingsChanged(FName ChangedProperty);
	bool bReloadRequested = false;
#endif
};
