// Copyright LogiUE. All Rights Reserved.

#include "LogiBevyTypes.h"
#include "Modules/ModuleManager.h"
#include "Interfaces/IPluginManager.h"
#include "HAL/PlatformProcess.h"
#include "Misc/Paths.h"

#if LOGI_BEVY_HAVE_RUST
#include "logi_bevy_ffi.h"
#endif

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

		if (DllHandle != nullptr)
		{
			// Fail loud on a stale DLL (ABI drift) rather than crashing in a later call.
			const uint32 LibVersion = logi_abi_version();
			UE_CLOG(LibVersion != LOGI_ABI_VERSION, LogLogiBevy, Error,
				TEXT("logi_ffi ABI MISMATCH: header=%u, loaded DLL=%u. Rebuild the plugin (editor closed) to stage the matching DLL."),
				static_cast<uint32>(LOGI_ABI_VERSION), LibVersion);
			UE_CLOG(LibVersion == LOGI_ABI_VERSION, LogLogiBevy, Log, TEXT("logi_ffi ABI version %u OK."), LibVersion);
		}

#if LOGI_BEVY_HAVE_TERRAIN
		{
			// Terrain kernel DLL (ADR-0007), loaded the same way as logi_ffi.dll.
			FString TerrainPath = TEXT("logi_terrain.dll");
			if (TSharedPtr<IPlugin> Plugin = IPluginManager::Get().FindPlugin(TEXT("LogiBevy")))
			{
				const FString Staged = FPaths::Combine(Plugin->GetBaseDir(), TEXT("Binaries"), TEXT("Win64"), TEXT("logi_terrain.dll"));
				if (FPaths::FileExists(Staged))
				{
					TerrainPath = Staged;
				}
			}
			TerrainDllHandle = FPlatformProcess::GetDllHandle(*TerrainPath);
			UE_CLOG(TerrainDllHandle == nullptr, LogLogiBevy, Warning, TEXT("Failed to load logi_terrain.dll ('%s'); terrain unavailable."), *TerrainPath);
			UE_CLOG(TerrainDllHandle != nullptr, LogLogiBevy, Log, TEXT("Loaded logi_terrain.dll."));
		}
#endif
#endif
	}

	virtual void ShutdownModule() override
	{
		if (DllHandle != nullptr)
		{
			FPlatformProcess::FreeDllHandle(DllHandle);
			DllHandle = nullptr;
		}
		if (TerrainDllHandle != nullptr)
		{
			FPlatformProcess::FreeDllHandle(TerrainDllHandle);
			TerrainDllHandle = nullptr;
		}
	}

private:
	void* DllHandle = nullptr;
	void* TerrainDllHandle = nullptr;
};

IMPLEMENT_MODULE(FLogiBevyRuntimeModule, LogiBevyRuntime);
