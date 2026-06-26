// Copyright LogiUE. All Rights Reserved.

#include "LogiBevyEditor.h"
#include "LogiGeoBenchRunner.h"
#include "Modules/ModuleManager.h"

DEFINE_LOG_CATEGORY(LogLogiBevyEditor);

/**
 * LogiBevyEditor module — authoring/import/editor tooling for the Bevy bridge.
 * Editor-only; excluded from packaged builds. Tooling shell, no gameplay authority.
 */
class FLogiBevyEditorModule : public IModuleInterface
{
public:
	virtual void StartupModule() override
	{
		UE_LOG(LogLogiBevyEditor, Log, TEXT("LogiBevyEditor started (authoring/import tooling)."));
		FLogiGeoBenchRunner::Init(); // geo bench harness — Bench/_inbox file watcher (Docs/Testing/Geo-Bench.md)
	}

	virtual void ShutdownModule() override
	{
		FLogiGeoBenchRunner::Shutdown();
	}
};

IMPLEMENT_MODULE(FLogiBevyEditorModule, LogiBevyEditor);
