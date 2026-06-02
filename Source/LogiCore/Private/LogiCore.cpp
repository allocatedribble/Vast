// Copyright LogiUE. All Rights Reserved.

#include "LogiCore.h"
#include "Modules/ModuleManager.h"
#include "HAL/IConsoleManager.h"

DEFINE_LOG_CATEGORY(LogLogi);
DEFINE_LOG_CATEGORY(LogLogiCore);

// --- Foundational console variables owned by LogiCore -----------------------
// The authoritative simulation runs on a fixed step; this is its rate. Other
// modules read it but do not own it. (See ADR-0001 Determinism.)
static TAutoConsoleVariable<int32> CVarLogiSimFixedStepHz(
	TEXT("logi.Sim.FixedStepHz"),
	30,
	TEXT("Fixed simulation step rate in Hz for the deterministic Logi sim core."),
	ECVF_Default);

// Global debug toggle for significance/LOD overlays (consumed by LogiPresentation).
static TAutoConsoleVariable<int32> CVarLogiDebugSignificance(
	TEXT("logi.Debug.Significance"),
	0,
	TEXT("Draw the unified significance score that drives Mass/representation LOD."),
	ECVF_Default);

IMPLEMENT_MODULE(FDefaultModuleImpl, LogiCore);
