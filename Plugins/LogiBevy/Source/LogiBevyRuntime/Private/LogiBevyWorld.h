// Copyright LogiUE. All Rights Reserved.

#pragma once

#include "CoreMinimal.h"

#if LOGI_BEVY_HAVE_RUST
#include "logi_bevy_ffi.h"
#endif
#if WITH_EDITOR && LOGI_BEVY_HAVE_EDITOR_INSPECTOR
#include "logi_bevy_editor_ffi.h"
#endif

/**
 * RAII owner of the opaque Rust `LogiWorld*` handle (§11: single mutable owner).
 * Move-only; destroys the world on scope exit. ALL world FFI funnels through here,
 * so the subsystems hold no raw `void*` and cannot leak or double-free the handle.
 */
class FLogiBevyWorld
{
public:
	FLogiBevyWorld() = default;
	~FLogiBevyWorld() { Destroy(); }

	FLogiBevyWorld(const FLogiBevyWorld&) = delete;
	FLogiBevyWorld& operator=(const FLogiBevyWorld&) = delete;

#if LOGI_BEVY_HAVE_RUST
	bool Create(const LogiInitDesc& Desc, LogiError& OutErr)
	{
		Destroy();
		Handle = logi_world_create(&Desc, &OutErr);
		return Handle != nullptr;
	}

	void Destroy()
	{
		if (Handle != nullptr)
		{
			logi_world_destroy(Handle);
			Handle = nullptr;
		}
	}

	bool IsValid() const { return Handle != nullptr; }

	LogiTickResult Tick(const LogiFrameInput& Input, LogiError& OutErr)
	{
		return logi_world_tick(Handle, &Input, &OutErr);
	}

	uint32 SubmitSpawns(const LogiSpawnCommand* Commands, uint32 Count, LogiError& OutErr)
	{
		return logi_submit_spawn_commands(Handle, Commands, Count, &OutErr);
	}

	uint32 GetSnapshot(LogiRenderSnapshot& OutSnapshot, LogiError& OutErr) const
	{
		return logi_world_get_render_snapshot(Handle, 0 /*latest*/, &OutSnapshot, &OutErr);
	}

	void ReleaseSnapshot(uint64 SnapshotId) const
	{
		logi_buffer_release(Handle, SnapshotId);
	}

#if WITH_EDITOR && LOGI_BEVY_HAVE_EDITOR_INSPECTOR
	uint32 EditorQuery(const LogiEntityQueryDesc& Query, LogiEntityInspectorPacket& OutPacket, LogiError& OutErr) const
	{
		return logi_editor_query_entities(Handle, &Query, &OutPacket, &OutErr);
	}

	void EditorSetPin(LogiEntityId Id, uint8 Pinned, LogiError& OutErr)
	{
		logi_editor_set_debug_pin(Handle, Id, Pinned, &OutErr);
	}
#endif

private:
	LogiWorld* Handle = nullptr;
#else
public:
	void Destroy() {}
	bool IsValid() const { return false; }
#endif
};
