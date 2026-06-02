// Copyright LogiUE. All Rights Reserved.

#pragma once

#include "CoreMinimal.h"
#include "LogiBevyTypes.generated.h"

DECLARE_LOG_CATEGORY_EXTERN(LogLogiBevy, Log, All);

/**
 * Unreal-side mirror of the Rust `AgentInstance` snapshot row (logi_render_extract).
 * Presentation/debug only — this is a copy of Rust-owned truth, never authority.
 * (u64 ids are carried as int64 since UE reflection has no uint64 property type.)
 */
USTRUCT(BlueprintType)
struct FLogiAgentInstance
{
	GENERATED_BODY()

	UPROPERTY(BlueprintReadOnly, Category = "LogiBevy")
	int64 Entity = 0;

	UPROPERTY(BlueprintReadOnly, Category = "LogiBevy")
	FVector Location = FVector::ZeroVector;

	UPROPERTY(BlueprintReadOnly, Category = "LogiBevy")
	int64 AgentId = 0;

	/** 0 = High, 1 = Medium, 2 = Low, 3 = Off (ADR-0003). */
	UPROPERTY(BlueprintReadOnly, Category = "LogiBevy")
	uint8 SimLOD = 0;
};
