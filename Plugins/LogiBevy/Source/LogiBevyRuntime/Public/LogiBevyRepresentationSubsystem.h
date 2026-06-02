// Copyright LogiUE. All Rights Reserved.

#pragma once

#include "CoreMinimal.h"
#include "Subsystems/WorldSubsystem.h"
#include "Engine/EngineBaseTypes.h"
#include "Templates/PimplPtr.h"
#include "LogiBevyRepresentationSubsystem.generated.h"

class ULogiBevyRepresentationSubsystem;
class FLogiInstancedRenderer;
class FLogiActorPool;
class FLogiAnimationBridge;
class FLogiBevyTelemetry;

/**
 * PostPhysics tick that applies the batched render snapshot, with an explicit
 * prerequisite on the sim's PrePhysics tick (ADR-0005 scheduling treaty).
 */
USTRUCT()
struct FLogiBevyRepresentationTickFunction : public FTickFunction
{
	GENERATED_BODY()

	ULogiBevyRepresentationSubsystem* Target = nullptr;

	virtual void ExecuteTick(float DeltaTime, ELevelTick TickType, ENamedThreads::Type CurrentThread, const FGraphEventRef& MyCompletionGraphEvent) override;
	virtual FString DiagnosticMessage() override;
};

template<>
struct TStructOpsTypeTraits<FLogiBevyRepresentationTickFunction> : public TStructOpsTypeTraitsBase2<FLogiBevyRepresentationTickFunction>
{
	enum { WithCopy = false };
};

/**
 * Consumes the per-frame batched snapshot from the Rust world (§8) and applies
 * deltas to retained Unreal representation: instanced meshes for the many
 * (FLogiInstancedRenderer), pooled actors for the significant few (FLogiActorPool),
 * and animation params fed to the budget allocator (FLogiAnimationBridge).
 * Non-authoritative -- pure presentation.
 */
UCLASS()
class LOGIBEVYRUNTIME_API ULogiBevyRepresentationSubsystem : public UWorldSubsystem
{
	GENERATED_BODY()

public:
	virtual void Initialize(FSubsystemCollectionBase& Collection) override;
	virtual void OnWorldBeginPlay(UWorld& InWorld) override;
	virtual void Deinitialize() override;
	virtual bool ShouldCreateSubsystem(UObject* Outer) const override;

	/** Driven by the PostPhysics tick function. */
	void TickRepresentation(float DeltaTime);

private:
	UPROPERTY()
	FLogiBevyRepresentationTickFunction RepTickFunction;

	TPimplPtr<FLogiInstancedRenderer> InstancedRenderer;
	TPimplPtr<FLogiActorPool> ActorPool;
	TPimplPtr<FLogiAnimationBridge> AnimationBridge;
	TPimplPtr<FLogiBevyTelemetry> Telemetry;
};
