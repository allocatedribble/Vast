// Copyright LogiUE. All Rights Reserved.

#pragma once

#include "CoreMinimal.h"
#include "Subsystems/WorldSubsystem.h"
#include "Engine/EngineBaseTypes.h"
#include "LogiBevyTypes.h"
#include "Templates/PimplPtr.h"
#include "LogiBevySubsystem.generated.h"

class ULogiBevySubsystem;
class FLogiBevyWorld;

/**
 * PrePhysics tick that advances the Rust simulation (ADR-0005 scheduling treaty).
 */
USTRUCT()
struct FLogiBevySimTickFunction : public FTickFunction
{
	GENERATED_BODY()

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
 * Owns the Rust `bevy_ecs` world for a game world. Its PrePhysics tick advances
 * the fixed-step sim (the accumulator is in Rust). The representation subsystem
 * pulls the batched render snapshot from the world handle in PostPhysics (§8).
 * This subsystem is a shell; the Rust world is the authority.
 */
UCLASS()
class LOGIBEVYRUNTIME_API ULogiBevySubsystem : public UWorldSubsystem
{
	GENERATED_BODY()

public:
	virtual void Initialize(FSubsystemCollectionBase& Collection) override;
	virtual void OnWorldBeginPlay(UWorld& InWorld) override;
	virtual void Deinitialize() override;
	virtual bool ShouldCreateSubsystem(UObject* Outer) const override;

	/** Driven by the PrePhysics tick function. */
	void TickSimulation(float DeltaTime);

	/** The sim tick function, so the representation subsystem can depend on it. */
	FLogiBevySimTickFunction& GetSimTickFunction() { return SimTickFunction; }

	/** The RAII-owned Rust world, for the representation subsystem to read snapshots. */
	FLogiBevyWorld* GetBevyWorld() const { return BevyWorld.Get(); }

	bool IsRuntimeValid() const;

	UFUNCTION(BlueprintCallable, Category = "LogiBevy")
	int64 GetSimTick() const { return LastSimTick; }

private:
	UPROPERTY()
	FLogiBevySimTickFunction SimTickFunction;

	TPimplPtr<FLogiBevyWorld> BevyWorld;
	int64 LastSimTick = 0;
};
