// Copyright LogiUE. All Rights Reserved.

#pragma once

#include "CoreMinimal.h"
#include "Subsystems/WorldSubsystem.h"
#include "LogiBevyTypes.h"
#include "LogiBevySubsystem.generated.h"

/**
 * Owns the Rust `bevy_ecs` world for a game world (the opaque `LogiWorld`).
 * Each frame it hands Unreal's frame delta to `logi_world_tick` — the
 * deterministic fixed-step accumulator lives in Rust (ADR-0001/0005) — then
 * pulls the batched render snapshot. This subsystem is a shell; the Rust world
 * is the gameplay authority.
 */
UCLASS()
class LOGIBEVYRUNTIME_API ULogiBevySubsystem : public UTickableWorldSubsystem
{
	GENERATED_BODY()

public:
	// USubsystem
	virtual void Initialize(FSubsystemCollectionBase& Collection) override;
	virtual void Deinitialize() override;
	virtual bool ShouldCreateSubsystem(UObject* Outer) const override;

	// FTickableGameObject / UTickableWorldSubsystem
	virtual void Tick(float DeltaTime) override;
	virtual TStatId GetStatId() const override;

	/** Latest agent snapshot, rebuilt each tick. Read-only mirror of Rust state. */
	const TArray<FLogiAgentInstance>& GetAgentInstances() const { return AgentInstances; }

	/** Whether the Rust world handle is live. */
	bool IsRuntimeValid() const { return WorldHandle != nullptr; }

	/** Last reported fixed-step tick of the Rust sim. */
	UFUNCTION(BlueprintCallable, Category = "LogiBevy")
	int64 GetSimTick() const { return LastSimTick; }

private:
	/** Opaque `LogiWorld*` (a boxed Rust world). */
	void* WorldHandle = nullptr;

	int64 LastSimTick = 0;

	TArray<FLogiAgentInstance> AgentInstances;
};
