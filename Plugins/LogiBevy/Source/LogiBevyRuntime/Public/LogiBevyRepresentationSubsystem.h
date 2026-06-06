// Copyright LogiUE. All Rights Reserved.

#pragma once

#include "CoreMinimal.h"
#include "Subsystems/WorldSubsystem.h"
#include "Engine/EngineBaseTypes.h"
#include "Templates/PimplPtr.h"
#include "LogiBevyRepresentationSubsystem.generated.h"

class ULogiBevyRepresentationSubsystem;
class ULogiBevyAssetRegistry;
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

	// Raw, deliberately NOT a UPROPERTY (audit C++ L-doc): this tick function is a member of its Target
	// subsystem, so the Target always outlives it, and the subsystem unregisters the tick in Deinitialize.
	// A UPROPERTY/TObjectPtr here would form a self-referential GC edge for no benefit.
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
 * (FLogiInstancedRenderer) and pooled actors for the significant few
 * (FLogiActorPool). The animation seam (FLogiAnimationBridge) is dormant until the
 * actor tier gains skeletal actors (audit M2). Non-authoritative -- pure presentation.
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

	/**
	 * GC root for the asset registry (audit H4). The registry is LoadSynchronous'd
	 * once at world begin play and held here so it -- and the FStreamableHandle
	 * preload it owns -- stay alive for the subsystem's lifetime. The renderer /
	 * actor pool are non-UObject pimpls and cannot root it; without this strong
	 * reference the registry is GC'd and every bucket silently falls back to the
	 * engine cube. Raw pointers handed to the renderer/pool borrow from this.
	 */
	UPROPERTY()
	TObjectPtr<ULogiBevyAssetRegistry> AssetRegistry;

	TPimplPtr<FLogiInstancedRenderer> InstancedRenderer;
	TPimplPtr<FLogiActorPool> ActorPool;
	TPimplPtr<FLogiAnimationBridge> AnimationBridge;
	TPimplPtr<FLogiBevyTelemetry> Telemetry;
};
