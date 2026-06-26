// Copyright LogiUE. All Rights Reserved.
#pragma once

#include "CoreMinimal.h"

class UStaticMesh;
class UMaterialInterface;
class UWorld;

// === Shared tile-render plumbing — ONE terrain/tile render core ===
// The baked-terrain, photoreal-globe (and the future unified terrain) subsystems all turn a Rust render
// frame into Unreal geometry the same way. This is the shared plumbing for that, instead of a hand-copied
// clone in each subsystem:
//   - LogiResolveStreamView   — the streaming viewpoint + lens for SSE selection,
//   - LogiBuildTileNaniteMesh  — a transient Nanite UStaticMesh from the 32-byte contract (editor-only).
// The runtime (non-Nanite) per-tile mesh upload now lives in ULogiTileMeshComponent, whose scene proxy
// de-interleaves the 32-byte stream and creates its RHI buffers ON THE RENDER THREAD (audit T1) — the
// game thread only hands off the pointer + counts. All paths consume vertices ALREADY in Unreal render
// space (cm, Z-up LH, correct winding) — the Rust render lanes emit exactly that — so there is NO
// coordinate swap here. (The old kernel-space synthetic-terrain renderer that needed a Y<->Z swap was
// retired in the one-engine merge.)

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

/// Resolve the streaming viewpoint: the PIE/game player camera, else (editor, no PIE) a PERSPECTIVE
/// level viewport — the focused pane when it is perspective, otherwise the first visible perspective one,
/// NEVER an ortho pane (streaming authority must not follow editor focus; the zenith sun keys off this
/// view). Fills `Loc`, the real viewport size, and the vertical FOV (converted from the camera's
/// horizontal FOV via the viewport aspect). Returns false when neither is available (no viewport this tick).
bool LogiResolveStreamView(UWorld* World, FLogiStreamView& Out);

#if LOGI_GEO_NANITE_LANE
/// Build a transient, Nanite-enabled UStaticMesh from the same 32-byte contract (editor-only encode path,
/// ADR-0008). Kernel-supplied normals are preserved (no per-tile recompute → no cross-tile lighting seams).
/// Returns nullptr on a degenerate/empty tile. The heavy Nanite encode is DEFERRED to the async static-mesh
/// compile manager — this path only runs in-editor, where async compilation is enabled — so the caller's
/// component self-heals when the compile finishes. Only a bounded mesh-description commit is synchronous here;
/// that game-thread portion is capped per step by the actor's "Nanite Builds Per Step" so a burst can't spike.
UStaticMesh* LogiBuildTileNaniteMesh(
	UObject* Outer,
	const uint8* Vertices, uint32 Stride, uint64 VerticesLen, uint32 VertexCount,
	const uint8* Indices, uint32 IndexCount,
	UMaterialInterface* Material, const TCHAR* SlotName);
#endif
