// Copyright LogiUE. All Rights Reserved.

#pragma once

#include "CoreMinimal.h"
#include "logi_bevy_ffi.h"

/**
 * Feeds Bevy-computed animation params + significance to Unreal's animation
 * presentation (§8). Bevy computes significance; Unreal feeds it to the Animation
 * Budget Allocator and drives the AnimInstance. (AnimInstance writes + budget
 * registration are TODOs pending pooled skeletal-mesh components.)
 */
class FLogiAnimationBridge
{
public:
	void ApplyActorUpdate(const LogiActorUpdate& Update);
	void ReleaseEntity(uint64 EntityIndex);
	void Reset();

	int32 TrackedCount() const { return EntityParams.Num(); }

private:
	struct FTracked
	{
		float Significance = 0.0f;
		LogiAnimationParams Anim{};
	};

	TMap<uint64 /*entity index*/, FTracked> EntityParams;
};
