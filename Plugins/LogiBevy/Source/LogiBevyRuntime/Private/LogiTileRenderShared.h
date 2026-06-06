// Copyright LogiUE. All Rights Reserved.
#pragma once

#include "CoreMinimal.h"

class UProceduralMeshComponent;
class UStaticMesh;
class UMaterialInterface;
class UWorld;

// === Shared tile-render plumbing — ONE terrain/tile render core ===
// The baked-terrain, photoreal-globe (and the future unified terrain) subsystems all turn a Rust render
// frame into Unreal geometry the same way. This is that single implementation, instead of a hand-copied
// clone in each subsystem:
//   - LogiResolveStreamView   — the streaming viewpoint + lens for SSE selection,
//   - LogiBuildTileSection     — a ProceduralMeshComponent section from the 32-byte vertex contract,
//   - LogiBuildTileNaniteMesh  — a transient Nanite UStaticMesh from the same contract (editor-only).
// All consume vertices ALREADY in Unreal render space (cm, Z-up LH, correct winding) — the Rust render
// lanes emit exactly that — so there is NO coordinate swap here. (The old kernel-space synthetic-terrain
// renderer that needed a Y<->Z swap was retired in the one-engine merge.)

/// The streaming viewpoint + lens for SSE-driven tile selection.
struct FLogiStreamView
{
	FVector Loc = FVector::ZeroVector;          // Unreal cm
	FVector Dir = FVector(1.0, 0.0, 0.0);       // camera forward (unit, UE space) — globe fetch priority + frustum
	FVector Up = FVector(0.0, 0.0, 1.0);        // camera up (unit, UE space) — globe view frustum
	float FovY = 1.05f;                         // vertical FOV (radians)
	float ViewportHeight = 1080.0f;
	float ViewportWidth = 1920.0f;
};

/// Resolve the streaming viewpoint: the PIE/game player camera, else (editor, no PIE) the active
/// level-viewport. Fills `Loc`, the real viewport size, and the vertical FOV (converted from the camera's
/// horizontal FOV via the viewport aspect). Returns false when neither is available (no viewport this tick).
bool LogiResolveStreamView(UWorld* World, FLogiStreamView& Out);

/// Build one ProceduralMeshComponent section from interleaved 32-byte verts (pos f3 | normal f3 | uv f2) +
/// u32 indices, already in Unreal render space. Triangles referencing out-of-range verts are dropped; a
/// malformed (stride < 32, or null) view builds nothing.
void LogiBuildTileSection(
	UProceduralMeshComponent* Mesh, int32 Section,
	const uint8* Vertices, uint32 Stride, uint64 VerticesLen, uint32 VertexCount,
	const uint8* Indices, uint32 IndexCount);

#if LOGI_GEO_NANITE_LANE
/// Build a transient, Nanite-enabled UStaticMesh from the same 32-byte contract (editor-only encode path,
/// ADR-0008). Kernel-supplied normals are preserved (no per-tile recompute → no cross-tile lighting
/// seams). Returns nullptr on a degenerate/empty tile. The build is ASYNC (encode on the static-mesh
/// worker pool); the caller's component self-heals when the compile finishes.
UStaticMesh* LogiBuildTileNaniteMesh(
	UObject* Outer,
	const uint8* Vertices, uint32 Stride, uint64 VerticesLen, uint32 VertexCount,
	const uint8* Indices, uint32 IndexCount,
	UMaterialInterface* Material, const TCHAR* SlotName);
#endif
