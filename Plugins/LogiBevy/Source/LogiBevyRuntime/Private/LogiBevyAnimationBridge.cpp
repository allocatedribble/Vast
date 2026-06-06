// Copyright LogiUE. All Rights Reserved.

#include "LogiBevyAnimationBridge.h"

// DORMANT lane (audit M2): see LogiBevyAnimationBridge.h. These are deliberately no-ops — the actor tier
// has no pooled skeletal AnimInstance to drive, so there is nothing to push or register. They exist as the
// single seam to wire when skeletal actors land; they must NOT accumulate per-entity state (the prior
// implementation kept a TMap nothing read, pure overhead implying functionality that wasn't there).

void FLogiAnimationBridge::ApplyActorUpdate(const LogiActorUpdate& /*Update*/)
{
}

void FLogiAnimationBridge::ReleaseEntity(uint64 /*EntityIndex*/)
{
}

void FLogiAnimationBridge::Reset()
{
}
