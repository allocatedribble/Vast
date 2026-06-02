// Copyright LogiUE. All Rights Reserved.

#pragma once

#include "CoreMinimal.h"
#include "Subsystems/WorldSubsystem.h"
#include "Engine/EngineBaseTypes.h"
#include "LogiBevyTypes.h"
#include "LogiBevySubsystem.generated.h"

class ULogiBevySubsystem;

/**
 * Tick function that drives the Rust simulation. Placed in TG_PrePhysics so
 * physics/collision proxies can read current sim transforms in the same frame
 * (ADR-0005 scheduling treaty: Unreal owns the frame, Bevy owns the step).
 */
USTRUCT()
struct FLogiBevySimTickFunction : public FTickFunction
{
	GENERATED_BODY()

	/** Owning subsystem (plain pointer; the subsystem owns this tick function). */
	ULogiBevySubsystem* Target = nullptr;

	virtual void ExecuteTick(float DeltaTime, ELevelTick TickType, ENamedThreads::Type CurrentThread, const FGraphEventRef& MyCompletionGraphEvent) override;
	virtual FString DiagnosticMessage() override;
};

template<>
struct TStructOpsTypeTraits<FLogiBevySimTickFunction> : public TStructOpsTypeTraitsBase2<FLogiBevySimTickFunction>
{
	enum { WithCopy = false };
};

/**
 * Owns the Rust `bevy_ecs` world for a game world. Its PrePhysics tick hands the
 * frame delta to `logi_world_tick` (the fixed-step accumulator is in Rust) and
 * pulls the batched render snapshot. The Rust world is the authority; this is a shell.
 */
UCLASS()
class LOGIBEVYRUNTIME_API ULogiBevySubsystem : public UWorldSubsystem
{
	GENERATED_BODY()

public:
	// USubsystem / UWorldSubsystem
	virtual void Initialize(FSubsystemCollectionBase& Collection) override;
	virtual void OnWorldBeginPlay(UWorld& InWorld) override;
	virtual void Deinitialize() override;
	virtual bool ShouldCreateSubsystem(UObject* Outer) const override;

	/** Driven by the PrePhysics tick function. */
	void TickSimulation(float DeltaTime);

	/** The sim tick function, so the representation subsystem can depend on it. */
	FLogiBevySimTickFunction& GetSimTickFunction() { return SimTickFunction; }

	/** Latest agent snapshot, rebuilt each tick. Read-only mirror of Rust state. */
	const TArray<FLogiAgentInstance>& GetAgentInstances() const { return AgentInstances; }

	bool IsRuntimeValid() const { return WorldHandle != nullptr; }

	UFUNCTION(BlueprintCallable, Category = "LogiBevy")
	int64 GetSimTick() const { return LastSimTick; }

private:
	UPROPERTY()
	FLogiBevySimTickFunction SimTickFunction;

	/** Opaque `LogiWorld*` (a boxed Rust world). */
	void* WorldHandle = nullptr;

	int64 LastSimTick = 0;

	TArray<FLogiAgentInstance> AgentInstances;
};
