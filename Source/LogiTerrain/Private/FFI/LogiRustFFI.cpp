// Copyright LogiUE. All Rights Reserved.

#include "FFI/LogiRustFFI.h"

// Until the Rust kernel is linked (LOGI_WITH_RUST_KERNEL=1), the FFI is provided
// by these neutral stubs so the module links and runs. When the kernel is
// integrated, link it via PublicAdditionalLibraries and these compile out.

#ifndef LOGI_WITH_RUST_KERNEL
#define LOGI_WITH_RUST_KERNEL 0
#endif

#if !LOGI_WITH_RUST_KERNEL

extern "C"
{
	LogiFfiTerrainSample LogiRust_SampleSurface(uint64 /*TileId*/, double /*LocalX*/, double /*LocalY*/)
	{
		// Flat ground, not resident — a clearly-fake placeholder.
		return LogiFfiTerrainSample{ /*HeightCm*/ 0.0, /*NormalX*/ 0.0, /*NormalY*/ 0.0, /*NormalZ*/ 1.0, /*bResident*/ 0 };
	}

	int32 LogiRust_IsTileResident(uint64 /*TileId*/)
	{
		return 0;
	}

	void LogiRust_RequestTile(uint64 /*TileId*/)
	{
		// No-op until the kernel exists.
	}
}

#endif // !LOGI_WITH_RUST_KERNEL
