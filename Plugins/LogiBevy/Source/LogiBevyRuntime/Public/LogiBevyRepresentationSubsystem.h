// Copyright LogiUE. All Rights Reserved.

#pragma once

#include "CoreMinimal.h"
#include "Subsystems/WorldSubsystem.h"
#include "Engine/EngineBaseTypes.h"
#include "LogiBevyRepresentationSubsystem.generated.h"

class ULogiBevyRepresentationSubsystem;
class FLogiBevyInstancedRenderer;
class FLogiBevyActorPool;
class FLogiBevyTelemetry;

/**
 * Tick function that drives representation. Placed in TG_PostPhysics with an
 * explicit prerequisite on the sim's PrePhysics tick, so it always consumes a
 * snapshot produced earlier in the same frame (ADR-0005 scheduling treaty).
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
 * Consumes the per-frame agent snapshot from ULogiBevySubsystem and drives the
 * Unreal representation: ISM/HISM for the many, pooled actors for the
 * significant few (ADR-0003/0005). Non-authoritative — pure presentation.
 */
UCLASS()
class LOGIBEVYRUNTIME_API ULogiBevyRepresentationSubsystem : public UWorldSubsystem
{
	GENERATED_BODY()

public:
	ULogiBevyRepresentationSubsystem();
	// Defined in the .cpp so TUniquePtr<incomplete-type> members destruct where complete.
	virtual ~ULogiBevyRepresentationSubsystem() override;

	// USubsystem / UWorldSubsystem
	virtual void Initialize(FSubsystemCollectionBase& Collection) override;
	virtual void OnWorldBeginPlay(UWorld& InWorld) override;
	virtual void Deinitialize() override;
	virtual bool ShouldCreateSubsystem(UObject* Outer) const override;

	/** Driven by the PostPhysics tick function. */
	void TickRepresentation(float DeltaTime);

private:
	UPROPERTY()
	FLogiBevyRepresentationTickFunction RepTickFunction;

	TUniquePtr<FLogiBevyInstancedRenderer> InstancedRenderer;
	TUniquePtr<FLogiBevyActorPool> ActorPool;
	TUniquePtr<FLogiBevyTelemetry> Telemetry;
};
