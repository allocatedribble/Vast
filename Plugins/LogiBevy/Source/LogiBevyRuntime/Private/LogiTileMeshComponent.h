// Copyright LogiUE. All Rights Reserved.

#pragma once

#include "CoreMinimal.h"
#include "Components/MeshComponent.h"
#include "LogiTileMeshComponent.generated.h"

// === Per-tile render shell with a RENDER-THREAD bulk geometry upload (audit T1) ===
//
// This is the per-tile renderer for the Rust Geo stream (ADR-0007/0012). Each resident tile owns ONE of
// these components; adding/removing a tile only creates/destroys that tile's tiny scene proxy (O(1)), never
// a whole-set rebuild — the documented reason tiles each own a component.
//
// THE T1 WIN — where the per-tile vertex work runs:
//   The Rust poll hands us a 32-byte INTERLEAVED vertex buffer (pos f32x3 | normal f32x3 | uv f32x2, already
//   in Unreal render space — cm, Z-up LH) + a u32 index buffer, valid only until the next poll(). The OLD
//   path (LogiBuildTileSection -> UProceduralMeshComponent::CreateMeshSection_LinearColor) de-interleaved
//   every vertex into 3 separate TArray<FVector>/<FVector2D> and rebuilt a TArray<int32> ON THE GAME THREAD,
//   then the proc-mesh copied it AGAIN into the vertex factory: an O(V)+O(I) scatter + double conversion +
//   a second copy, all on the frame's critical path.
//
//   SetTileGeometry() instead does ONLY a flat byte Memcpy of the interleaved verts + indices into a small
//   ref-counted payload (no per-vertex FVector construction, no de-interleave, no double-precision widening,
//   no 3x scattered allocations) and marks the render state dirty. The expensive de-interleave into the GPU
//   vertex streams + the RHI buffer creation then run on the RENDER THREAD in the proxy's
//   CreateRenderThreadResources(). Net: the game-thread per-tile cost drops from O(V)+O(I) scatter to a
//   bandwidth-bound flat copy + an O(1) proxy hand-off.
//
//   LIFETIME: the Rust buffer is pinned by an Arc only until the next poll(), and the render command runs
//   asynchronously (it may execute AFTER the next game tick has polled again). So SetTileGeometry copies the
//   bytes out on the game thread — exactly the discipline the JPEG-atlas path already uses (DispatchTileDecode
//   copies the frame-transient bytes before handing them to a worker). The proxy then owns that CPU copy and
//   frees it the moment the render thread has uploaded it to the RHI buffers.

class FLogiTileSceneProxy;

/**
 * One Rust-streamed terrain/globe tile, rendered as a single static section with a render-thread bulk upload.
 * A SHELL only (ADR-0007): it mirrors Rust-owned geometry and holds no gameplay truth. Verts are absolute
 * world cm, so the component ignores the actor transform (SetAbsolute) exactly like the proc-mesh it replaces.
 */
UCLASS(ClassGroup = (Rendering), hidecategories = (Object, LOD, Physics, Collision), MinimalAPI)
class ULogiTileMeshComponent : public UMeshComponent
{
	GENERATED_BODY()

public:
	ULogiTileMeshComponent();

	/**
	 * Hand this tile's geometry to the renderer (game thread, ~O(bytes) flat copy — no per-vertex scatter).
	 * `Vertices` is `VertexCount` interleaved 32-byte verts (pos f3 | normal f3 | uv f2) in Unreal render
	 * space; `Indices` is `IndexCount` little-endian u32 (the ABI contract: buffer = IndexCount*4 bytes). Both
	 * pointers are only valid until the next Rust poll(), so we copy here and the render thread uploads the
	 * copy. `BoundsCenter`/`BoundsRadius` are the Rust-supplied tile bounds in UE cm (exact ⇒ O(1) bounds, no
	 * per-vertex AABB scan). A malformed view (stride < 32, null, or zero verts/indices) clears the section.
	 */
	void SetTileGeometry(
		const uint8* Vertices, uint32 Stride, uint64 VerticesLen, uint32 VertexCount,
		const uint8* Indices, uint32 IndexCount,
		const FVector3f& BoundsCenter, float BoundsRadius);

	/** Drop this tile's geometry (recycle path): empties the section so the pooled component renders nothing. */
	void ClearTileGeometry();

	//~ UPrimitiveComponent / UMeshComponent
	virtual FPrimitiveSceneProxy* CreateSceneProxy() override;
	virtual FBoxSphereBounds CalcBounds(const FTransform& LocalToWorld) const override;
	virtual int32 GetNumMaterials() const override { return 1; }

private:
	friend class FLogiTileSceneProxy;

	// CPU-side copy of the interleaved tile bytes + indices, handed to the proxy and freed once uploaded. Held
	// as a shared, thread-safe pointer so the proxy can take ownership across the game/render-thread boundary
	// without the game thread blocking on the upload. Empty => the component has no geometry (clears to nothing).
	struct FLogiTilePayload
	{
		TArray<uint8> InterleavedVerts;  // VertexCount * Stride bytes, exactly as Rust emitted them (flat copy)
		TArray<uint32> Indices;          // raw IndexCount u32, flat-copied (FFI is little-endian == our target);
		                                 // bounds-validated on the render thread, not here
		uint32 VertexCount = 0;
		uint32 Stride = 0;               // = 32 (carried so the proxy de-interleave is self-describing)
	};

	// The current tile geometry (game-thread authoritative). Reset by SetTileGeometry/ClearTileGeometry; copied
	// (shared) into the proxy at CreateSceneProxy time. TSharedPtr (ThreadSafe) so the proxy can outlive a
	// subsequent SetTileGeometry without a use-after-free of the bytes.
	TSharedPtr<FLogiTilePayload, ESPMode::ThreadSafe> Payload;

	// Rust-supplied local-space bounds (UE cm). Exact, so CalcBounds is O(1) — no per-vertex AABB scan.
	FBoxSphereBounds LocalBounds = FBoxSphereBounds(ForceInit);
};
