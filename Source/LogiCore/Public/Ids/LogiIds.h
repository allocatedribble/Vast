// Copyright LogiUE. All Rights Reserved.

#pragma once

#include "CoreMinimal.h"
#include "LogiIds.generated.h"

// Deterministic, stable identity types. Each wraps a 64-bit handle so it is
// cheap to copy, hash, and compare, and so it survives replay/snapshot equality
// (see ADR-0001 Determinism). Values are produced deterministically upstream
// (e.g. tile identity from the Rust terrain kernel, ADR-0002).
//
// Note: the 64-bit handle is intentionally NOT a UPROPERTY (UE reflection has no
// uint64 property type); these are reflected struct *types* with custom identity.

#define LOGI_DECLARE_ID_BODY(TypeName)                                                  \
	uint64 Value = 0;                                                                    \
	FORCEINLINE bool IsValid() const { return Value != 0; }                             \
	FORCEINLINE bool operator==(const TypeName& Other) const { return Value == Other.Value; } \
	FORCEINLINE bool operator!=(const TypeName& Other) const { return Value != Other.Value; } \
	FORCEINLINE friend uint32 GetTypeHash(const TypeName& Id) { return GetTypeHash(Id.Value); } \
	FORCEINLINE FString ToString() const { return FString::Printf(TEXT(#TypeName "(%llu)"), Value); }

/** Deterministic identity of a terrain tile (truth owned by the Rust kernel; ADR-0002). */
USTRUCT(BlueprintType)
struct FLogiTileId
{
	GENERATED_BODY()
	LOGI_DECLARE_ID_BODY(FLogiTileId)
};

/** Identity of a logistics route. */
USTRUCT(BlueprintType)
struct FLogiRouteId
{
	GENERATED_BODY()
	LOGI_DECLARE_ID_BODY(FLogiRouteId)
};

/** Identity of a simulated agent (courier, vehicle, worker, ...). */
USTRUCT(BlueprintType)
struct FLogiAgentId
{
	GENERATED_BODY()
	LOGI_DECLARE_ID_BODY(FLogiAgentId)
};

/** Identity of a shipment / package. */
USTRUCT(BlueprintType)
struct FLogiShipmentId
{
	GENERATED_BODY()
	LOGI_DECLARE_ID_BODY(FLogiShipmentId)
};

#undef LOGI_DECLARE_ID_BODY
