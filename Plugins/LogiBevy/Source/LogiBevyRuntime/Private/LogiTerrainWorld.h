// Copyright LogiUE. All Rights Reserved.

#pragma once

#include "CoreMinimal.h"
#include "LogiBevyTypes.h" // LogLogiBevy (warn-once on non-OK kernel status)

#if LOGI_BEVY_HAVE_TERRAIN
#include "logi_terrain_ffi.h"

// Defined in LogiBevyFfi.cpp (the module owns the terrain DLL load + ABI check). Returns false
// once a terrain ABI mismatch has been detected at startup; because the terrain exports are
// delay-loaded, the bridge MUST consult this before any terrain call (freeing the DLL handle on
// mismatch does not stop the loader from re-resolving + re-loading the mismatched DLL on first use).
bool LogiBevy_IsTerrainAbiOk();
#endif

/**
 * RAII owner of the opaque Rust terrain kernel handle (`LogiTerrainWorld*`,
 * ADR-0007). Move-only; destroys on scope exit. ALL terrain FFI funnels through
 * here. The kernel owns streaming/memory/decode/LOD; Unreal only consumes frame
 * packets + zero-copy buffer views.
 */
class FLogiTerrainWorld
{
public:
	FLogiTerrainWorld() = default;
	~FLogiTerrainWorld() { Destroy(); }

	FLogiTerrainWorld(const FLogiTerrainWorld&) = delete;
	FLogiTerrainWorld& operator=(const FLogiTerrainWorld&) = delete;

#if LOGI_BEVY_HAVE_TERRAIN
	bool Create()
	{
		Destroy();
		// Refuse to enter the terrain DLL if its ABI did not match the header at load time. The
		// exports are delay-loaded, so calling anyway would re-load the mismatched DLL and crash.
		if (!LogiBevy_IsTerrainAbiOk())
		{
			if (!bAbiRefusalWarned)
			{
				bAbiRefusalWarned = true;
				UE_LOG(LogLogiBevy, Warning,
					TEXT("Skipping terrain kernel creation: logi_terrain ABI mismatch detected at load (see startup log)."));
			}
			return false;
		}
		Handle = logi_terrain_world_create(nullptr); // null => library defaults
		return Handle != nullptr;
	}

	void Destroy()
	{
		if (Handle != nullptr)
		{
			logi_terrain_world_destroy(Handle);
			Handle = nullptr;
		}
	}

	bool IsValid() const { return Handle != nullptr; }

	void SetCamera(const LogiCameraState& Camera)
	{
		if (Handle != nullptr)
		{
			// Surface a panicking/erroring kernel once instead of swallowing the status.
			const LogiStatus Status = logi_terrain_world_set_camera(Handle, &Camera);
			WarnOnce(Status, bSetCameraWarned, TEXT("logi_terrain_world_set_camera"));
		}
	}

	LogiFrameId Step(double DeltaSeconds)
	{
		LogiFrameId Frame = 0;
		if (Handle != nullptr)
		{
			// Surface a panicking/erroring kernel once instead of swallowing the status.
			const LogiStatus Status = logi_terrain_world_step(Handle, DeltaSeconds, &Frame);
			WarnOnce(Status, bStepWarned, TEXT("logi_terrain_world_step"));
		}
		return Frame;
	}

	bool GetFramePacket(LogiFrameId Frame, LogiFramePacket& OutPacket) const
	{
		return Handle != nullptr
			&& logi_terrain_world_get_frame_packet(Handle, Frame, &OutPacket) == LOGI_TERRAIN_OK;
	}

	void ReleaseFrame(LogiFrameId Frame)
	{
		if (Handle != nullptr)
		{
			// logi_terrain_world_release_frame returns void (no LogiStatus to capture).
			logi_terrain_world_release_frame(Handle, Frame);
		}
	}

	bool AcquireBufferView(LogiBufferHandle BufferHandle, LogiBufferView& OutView) const
	{
		return Handle != nullptr
			&& logi_terrain_world_acquire_buffer_view(Handle, BufferHandle, &OutView) == LOGI_TERRAIN_OK;
	}

private:
	// Logs once (per call site) when the kernel returns a non-OK status, to avoid per-frame spam.
	static void WarnOnce(LogiStatus Status, bool& bWarnedFlag, const TCHAR* What)
	{
		if (Status != LOGI_TERRAIN_OK && !bWarnedFlag)
		{
			bWarnedFlag = true;
			UE_LOG(LogLogiBevy, Warning, TEXT("%s returned non-OK terrain status %d; the kernel may be panicking."),
				What, static_cast<int32>(Status));
		}
	}

	LogiTerrainWorld* Handle = nullptr;
	bool bSetCameraWarned = false;
	bool bStepWarned = false;
	bool bAbiRefusalWarned = false; // log the ABI-mismatch refusal once, not per Create() attempt
#else
public:
	void Destroy() {}
	bool IsValid() const { return false; }
#endif
};
