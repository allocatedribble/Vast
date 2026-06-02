// Copyright LogiUE. All Rights Reserved.

#pragma once

#include "CoreMinimal.h"

/**
 * Pool of high-resolution representation actors for the significant few
 * (ADR-0003 significance budget). Actors are pooled shells that mirror Rust
 * state; they never own gameplay truth. Plain C++ helper.
 */
class FLogiBevyActorPool
{
public:
	/** Reserve capacity for the pool. (Stub: records the budget.) */
	void Reserve(int32 Count) { Capacity = Count; }

	int32 GetCapacity() const { return Capacity; }

private:
	int32 Capacity = 0;
};
