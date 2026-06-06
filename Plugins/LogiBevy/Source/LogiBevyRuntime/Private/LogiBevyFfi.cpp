// Copyright LogiUE. All Rights Reserved.

#include "LogiBevyTypes.h"
#include "Modules/ModuleManager.h"
#include "Interfaces/IPluginManager.h"
#include "HAL/PlatformProcess.h"
#include "Misc/Paths.h"

#if LOGI_BEVY_HAVE_RUST
#include "logi_bevy_ffi.h"
#endif

#if LOGI_BEVY_HAVE_GEO
#include "logi_geo_ffi.h" // logi_geo_abi_version() + LOGI_GEO_ABI_VERSION (ABI drift check)
#endif

DEFINE_LOG_CATEGORY(LogLogiBevy);

#if LOGI_BEVY_HAVE_GEO
namespace
{
	// Runtime gate for the terrain DLL. The terrain exports are delay-loaded, so freeing the
	// DLL handle on an ABI mismatch does NOT stop a later call: the loader re-resolves and
	// re-loads the (mismatched) DLL on first use, then crashes. We flip this false on mismatch
	// and have the terrain subsystem consult it before EVER calling into the terrain DLL.
	// Defaults true so a clean load needs no opt-in.
	bool GLogiTerrainAbiOk = true;
}

// Defined here (the module owns the terrain-DLL load + ABI check). The globe + baked-terrain subsystems
// forward-declare this locally and consult it before any terrain-DLL call (the exports are delay-loaded).
bool LogiBevy_IsTerrainAbiOk()
{
	return GLogiTerrainAbiOk;
}
#endif // LOGI_BEVY_HAVE_GEO

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
#endif // PLATFORM_WINDOWS && LOGI_BEVY_HAVE_RUST

#if PLATFORM_WINDOWS && LOGI_BEVY_HAVE_GEO
		// Sibling of the logi_ffi load (not nested under HAVE_RUST) so the terrain feature
		// keys on LOGI_BEVY_HAVE_GEO alone — matching the Geo terrain/globe/building gating.
		{
			// Terrain kernel DLL (ADR-0007), loaded the same way as logi_ffi.dll.
			FString TerrainPath = TEXT("logi_geo.dll");
			if (TSharedPtr<IPlugin> Plugin = IPluginManager::Get().FindPlugin(TEXT("LogiBevy")))
			{
				const FString Staged = FPaths::Combine(Plugin->GetBaseDir(), TEXT("Binaries"), TEXT("Win64"), TEXT("logi_geo.dll"));
				if (FPaths::FileExists(Staged))
				{
					TerrainPath = Staged;
				}
			}
			TerrainDllHandle = FPlatformProcess::GetDllHandle(*TerrainPath);
			UE_CLOG(TerrainDllHandle == nullptr, LogLogiBevy, Warning, TEXT("Failed to load logi_geo.dll ('%s'); terrain unavailable."), *TerrainPath);
			UE_CLOG(TerrainDllHandle != nullptr, LogLogiBevy, Log, TEXT("Loaded logi_geo.dll."));

			if (TerrainDllHandle != nullptr)
			{
				// Fail loud on a stale terrain DLL (ABI drift) rather than crashing in a later call.
				const uint32 TerrainVersion = logi_geo_abi_version();
				if (TerrainVersion != LOGI_GEO_ABI_VERSION)
				{
					UE_LOG(LogLogiBevy, Error,
						TEXT("logi_geo ABI MISMATCH: header=%u, loaded DLL=%u. Rebuild the plugin (editor closed) to stage the matching DLL. Terrain unavailable."),
						static_cast<uint32>(LOGI_GEO_ABI_VERSION), TerrainVersion);
					// Treat terrain as unavailable. Freeing the handle is NOT enough: the terrain exports are
					// delay-loaded, so the first terrain call would re-resolve + re-load the mismatched DLL and
					// crash. Latch a runtime flag the subsystem checks before any terrain call, then drop the handle.
					GLogiTerrainAbiOk = false;
					FPlatformProcess::FreeDllHandle(TerrainDllHandle);
					TerrainDllHandle = nullptr;
				}
				else
				{
					UE_LOG(LogLogiBevy, Log, TEXT("logi_geo ABI version %u OK."), TerrainVersion);
				}
			}
		}
#endif // PLATFORM_WINDOWS && LOGI_BEVY_HAVE_GEO
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
