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
 * One row of the editor Entity Outliner (mirrors `LogiEntitySummary`; editor-only,
 * ADR-0006). Plain data so the Public header stays free of the FFI types.
 */
struct FLogiBevyEntityRow
{
	uint64 Index = 0;
	uint32 Generation = 0;
	uint16 Shard = 0;
	uint16 Kind = 0;
	FVector Position = FVector::ZeroVector;
	float Significance = 0.0f;
	uint8 Tier = 0;
	bool bPinned = false;
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

#if WITH_EDITOR
	/** Editor introspection (ADR-0006): paged entity query. Empty without the DLL
	 *  `editor_inspector` feature. Sampled by the editor at 5-10 Hz, never per-row. */
	void EditorQueryEntities(int32 Offset, int32 Limit, TArray<FLogiBevyEntityRow>& OutRows, int32& OutTotal) const;
	/** Pin/unpin an entity for debug focus (routes to logi_editor_set_debug_pin). */
	void EditorSetPin(const FLogiBevyEntityRow& Row, bool bPinned);
#endif

private:
	UPROPERTY()
	FLogiBevySimTickFunction SimTickFunction;

	TPimplPtr<FLogiBevyWorld> BevyWorld;
	int64 LastSimTick = 0;
};
