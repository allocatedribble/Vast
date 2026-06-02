// Copyright LogiUE. All Rights Reserved.

#pragma once

#include "CoreMinimal.h"
#include "UObject/Object.h"
#include "LogiBevyEntitySelectionProxy.generated.h"

/**
 * The ONE transient Details proxy for the Bevy Entity Outliner (ADR-0006). It
 * mirrors the latest inspector packet for the selected entity -- it is NOT
 * authoritative and there is exactly one of these, never one per entity. Editing
 * its fields does nothing to the Bevy entity (edits route through explicit
 * commands, not this mirror).
 */
UCLASS(Transient)
class ULogiBevyEntitySelectionProxy : public UObject
{
	GENERATED_BODY()

public:
	UPROPERTY(VisibleAnywhere, Category = "Identity")
	FString EntityId;

	UPROPERTY(VisibleAnywhere, Category = "Identity")
	int64 Index = 0;

	UPROPERTY(VisibleAnywhere, Category = "Identity")
	int32 Kind = 0;

	UPROPERTY(VisibleAnywhere, Category = "Representation")
	float Significance = 0.0f;

	UPROPERTY(VisibleAnywhere, Category = "Representation")
	FString RepresentationTier;

	UPROPERTY(VisibleAnywhere, Category = "Transform")
	FVector Position = FVector::ZeroVector;

	UPROPERTY(VisibleAnywhere, Category = "State")
	bool bPinned = false;
};
