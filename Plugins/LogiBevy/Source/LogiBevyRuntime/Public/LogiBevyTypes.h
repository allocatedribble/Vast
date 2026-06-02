// Copyright LogiUE. All Rights Reserved.

#pragma once

#include "CoreMinimal.h"
#include "LogiBevyTypes.generated.h"

DECLARE_LOG_CATEGORY_EXTERN(LogLogiBevy, Log, All);

/**
 * Unreal-side mirror of the Rust `LogiAgentInstance` snapshot row
 * (logi_render_extract). Presentation/debug only — a copy of Rust-owned truth,
 * never authority. Identity is the stable `LogiEntityId`; its 64-bit index is
 * carried as int64 since UE reflection has no uint64 property type.
 */
USTRUCT(BlueprintType)
struct FLogiAgentInstance
{
	GENERATED_BODY()

	UPROPERTY(BlueprintReadOnly, Category = "LogiBevy")
	int64 EntityIndex = 0;

	UPROPERTY(BlueprintReadOnly, Category = "LogiBevy")
	int32 EntityGeneration = 0;

	UPROPERTY(BlueprintReadOnly, Category = "LogiBevy")
	int32 EntityKind = 0;

	/** World position in cm (f64 / large-world coordinates). */
	UPROPERTY(BlueprintReadOnly, Category = "LogiBevy")
	FVector Location = FVector::ZeroVector;

	/** Representation tier — Bevy decides, Unreal executes (ADR-0003):
	 *  0 = None, 1 = InstancedStaticMesh, 2 = LowResActor, 3 = HighResActor. */
	UPROPERTY(BlueprintReadOnly, Category = "LogiBevy")
	uint8 Tier = 0;
};
