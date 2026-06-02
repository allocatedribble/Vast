// Copyright LogiUE. All Rights Reserved.

#pragma once

#include "CoreMinimal.h"

#if LOGI_BEVY_HAVE_TERRAIN
#include "logi_terrain_ffi.h"
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
			logi_terrain_world_set_camera(Handle, &Camera);
		}
	}

	LogiFrameId Step(double DeltaSeconds)
	{
		LogiFrameId Frame = 0;
		if (Handle != nullptr)
		{
			logi_terrain_world_step(Handle, DeltaSeconds, &Frame);
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
			logi_terrain_world_release_frame(Handle, Frame);
		}
	}

private:
	LogiTerrainWorld* Handle = nullptr;
#else
public:
	void Destroy() {}
	bool IsValid() const { return false; }
#endif
};
