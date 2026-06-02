// Copyright LogiUE. All Rights Reserved.

#pragma once

#include "CoreMinimal.h"
#include "Subsystems/WorldSubsystem.h"
#include "LogiBevyTypes.h"
#include "LogiBevySubsystem.generated.h"

/**
 * Owns the Rust `bevy_ecs` runtime for a game world. Advances the deterministic
 * fixed-step simulation on a frame-rate-independent accumulator and caches a
 * per-frame agent snapshot pulled across the batched FFI boundary.
 *
 * The Rust `World` is the gameplay authority (ADR-0005); this subsystem is a
 * shell that drives it and mirrors its output for representation.
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

	/** Latest agent snapshot, rebuilt whenever the sim advances. Read-only mirror. */
	const TArray<FLogiAgentInstance>& GetAgentInstances() const { return AgentInstances; }

	/** Whether the Rust runtime handle is live. */
	bool IsRuntimeValid() const { return RuntimeHandle != nullptr; }

	/** Current fixed-step tick of the Rust sim (0 if unavailable). */
	UFUNCTION(BlueprintCallable, Category = "LogiBevy")
	int64 GetSimTick() const;

private:
	/** Opaque LogiRuntimeHandle (a boxed Rust `LogiRuntime`). */
	void* RuntimeHandle = nullptr;

	float FixedStepSeconds = 1.0f / 30.0f;
	float Accumulator = 0.0f;

	TArray<FLogiAgentInstance> AgentInstances;

	void AdvanceAndExtract(int32 Steps);
};
