// Copyright LogiUE. All Rights Reserved.

#pragma once

#include "CoreMinimal.h"
#include "Subsystems/WorldSubsystem.h"
#include "LogiBevyRepresentationSubsystem.generated.h"

class FLogiBevyInstancedRenderer;
class FLogiBevyActorPool;
class FLogiBevyTelemetry;

/**
 * Consumes the per-frame agent snapshot from ULogiBevySubsystem and drives the
 * Unreal representation: instanced meshes for the many, pooled actors for the
 * significant few (ADR-0003 / ADR-0005). Non-authoritative — pure presentation.
 */
UCLASS()
class LOGIBEVYRUNTIME_API ULogiBevyRepresentationSubsystem : public UTickableWorldSubsystem
{
	GENERATED_BODY()

public:
	ULogiBevyRepresentationSubsystem();
	// Defined in the .cpp so TUniquePtr<incomplete-type> members destruct where complete.
	virtual ~ULogiBevyRepresentationSubsystem() override;

	// USubsystem
	virtual void Initialize(FSubsystemCollectionBase& Collection) override;
	virtual void Deinitialize() override;
	virtual bool ShouldCreateSubsystem(UObject* Outer) const override;

	// FTickableGameObject / UTickableWorldSubsystem
	virtual void Tick(float DeltaTime) override;
	virtual TStatId GetStatId() const override;

private:
	TUniquePtr<FLogiBevyInstancedRenderer> InstancedRenderer;
	TUniquePtr<FLogiBevyActorPool> ActorPool;
	TUniquePtr<FLogiBevyTelemetry> Telemetry;
};
