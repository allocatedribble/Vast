// Copyright LogiUE. All Rights Reserved.

#pragma once

#include "CoreMinimal.h"
#include "LogiSimClock.generated.h"

// Fixed-step simulation clock types. The authoritative sim advances in integer
// ticks at a fixed rate (logi.Sim.FixedStepHz), decoupled from frame rate, so it
// is reproducible for replay/snapshot equality (ADR-0001 Determinism).

/** A monotonically increasing fixed-step tick count. */
USTRUCT(BlueprintType)
struct FLogiSimTick
{
	GENERATED_BODY()

	UPROPERTY(EditAnywhere, BlueprintReadWrite, Category = "Logi|Sim")
	int64 Tick = 0;

	FORCEINLINE bool operator==(const FLogiSimTick& Other) const { return Tick == Other.Tick; }
	FORCEINLINE bool operator<(const FLogiSimTick& Other) const { return Tick < Other.Tick; }
	FORCEINLINE FLogiSimTick& operator++() { ++Tick; return *this; }
};

/** Integer-rational fixed timestep. Kept rational (not a float) to stay deterministic. */
USTRUCT(BlueprintType)
struct FLogiFixedStep
{
	GENERATED_BODY()

	/** Steps per second. Mirrors the logi.Sim.FixedStepHz console variable. */
	UPROPERTY(EditAnywhere, BlueprintReadWrite, Category = "Logi|Sim", meta = (ClampMin = "1"))
	int32 StepHz = 30;

	/** Seconds advanced by a single tick. Derived; safe for presentation/interpolation only. */
	FORCEINLINE double SecondsPerTick() const { return StepHz > 0 ? 1.0 / static_cast<double>(StepHz) : 0.0; }

	/** Whole seconds represented by a tick count (presentation helper). */
	FORCEINLINE double TicksToSeconds(const FLogiSimTick& InTick) const { return static_cast<double>(InTick.Tick) * SecondsPerTick(); }
};
