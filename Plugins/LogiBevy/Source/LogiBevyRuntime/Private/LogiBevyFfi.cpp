// Copyright LogiUE. All Rights Reserved.

#include "LogiBevyTypes.h"
#include "Modules/ModuleManager.h"
#include "Interfaces/IPluginManager.h"
#include "HAL/PlatformProcess.h"
#include "Misc/Paths.h"

DEFINE_LOG_CATEGORY(LogLogiBevy);

/**
 * LogiBevyRuntime module. Owns the lifetime of the Rust `logi_ffi` DLL handle:
 * the DLL is staged next to this module's binary by RuntimeDependencies, and we
 * load it explicitly so the delay-loaded imports resolve.
 */
class FLogiBevyRuntimeModule : public IModuleInterface
{
public:
	virtual void StartupModule() override
	{
#if PLATFORM_WINDOWS && LOGI_BEVY_HAVE_RUST
		FString DllPath = TEXT("logi_ffi.dll");
		if (TSharedPtr<IPlugin> Plugin = IPluginManager::Get().FindPlugin(TEXT("LogiBevy")))
		{
			const FString Staged = FPaths::Combine(Plugin->GetBaseDir(), TEXT("Binaries"), TEXT("Win64"), TEXT("logi_ffi.dll"));
			if (FPaths::FileExists(Staged))
			{
				DllPath = Staged;
			}
		}

		DllHandle = FPlatformProcess::GetDllHandle(*DllPath);
		UE_CLOG(DllHandle == nullptr, LogLogiBevy, Error,
			TEXT("Failed to load logi_ffi.dll (looked for '%s'). The Bevy sim will be unavailable."), *DllPath);
		UE_CLOG(DllHandle != nullptr, LogLogiBevy, Log, TEXT("Loaded logi_ffi.dll: '%s'."), *DllPath);
#endif
	}

	virtual void ShutdownModule() override
	{
		if (DllHandle != nullptr)
		{
			FPlatformProcess::FreeDllHandle(DllHandle);
			DllHandle = nullptr;
		}
	}

private:
	void* DllHandle = nullptr;
};

IMPLEMENT_MODULE(FLogiBevyRuntimeModule, LogiBevyRuntime);
