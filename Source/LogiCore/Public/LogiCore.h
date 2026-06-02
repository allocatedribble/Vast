// Copyright LogiUE. All Rights Reserved.

#pragma once

#include "CoreMinimal.h"

// General-purpose, cross-cutting Logi log category (exported for project-wide use).
// Per-subsystem modules also declare their own categories (LogLogiMass, etc.).
LOGICORE_API DECLARE_LOG_CATEGORY_EXTERN(LogLogi, Log, All);

// LogiCore's own module-local category.
DECLARE_LOG_CATEGORY_EXTERN(LogLogiCore, Log, All);
