// Copyright LogiUE. All Rights Reserved.

#pragma once

#include "CoreMinimal.h"
#include "logi_bevy_ffi.h" // LogiActorUpdate (cbindgen typedef'd anonymous struct -- not forward-declarable)

/**
 * Animation presentation seam (§8): Bevy computes per-agent animation params + significance; Unreal would
 * feed them to the Animation Budget Allocator and drive a pooled skeletal AnimInstance.
 *
 * DORMANT (audit M2): the actor tier currently spawns STATIC-mesh actors (FLogiActorPool) — there is no
 * pooled USkeletalMeshComponent / AnimInstance to drive yet, so this lane intentionally does nothing rather
 * than accumulate a per-entity map that nothing reads (which is what it used to do). Kept as the single
 * integration point so wiring the lane later is a localized change: when skeletal actors land, push
 * `Update.animation` into the AnimInstance and register/deregister the component with the budget allocator
 * (priority = `Update.significance`) here, against the actor FLogiActorPool keys.
 */
class FLogiAnimationBridge
{
public:
	/// No-op while dormant; the integration point for AnimInstance push + budget registration (audit M2).
	void ApplyActorUpdate(const LogiActorUpdate& Update);
	/// No-op while dormant; the integration point for budget deregistration (audit M2).
	void ReleaseEntity(uint64 EntityIndex);
	void Reset();
};
