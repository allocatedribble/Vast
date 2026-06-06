// Copyright LogiUE. All Rights Reserved.

#include "LogiTileRenderShared.h"
#include "Engine/World.h"
#include "Engine/GameViewportClient.h"      // UGameViewportClient::GetViewportSize
#include "GameFramework/PlayerController.h"
#include "Camera/PlayerCameraManager.h"     // APlayerCameraManager::GetFOVAngle
#include "ProceduralMeshComponent.h"

#if WITH_EDITOR
#include "LevelEditorViewport.h"            // GCurrentLevelEditingViewportClient
#endif

#if LOGI_GEO_NANITE_LANE
#include "Engine/StaticMesh.h"
#include "StaticMeshAttributes.h"
#include "MeshDescription.h"
#include "Materials/Material.h"
#include "MaterialDomain.h"                 // UMaterial::GetDefaultMaterial(MD_Surface)
#endif

bool LogiResolveStreamView(UWorld* World, FLogiStreamView& Out)
{
	if (World == nullptr)
	{
		return false;
	}
	// GetFOVAngle() is the HORIZONTAL FOV in degrees; convert to vertical via the (real) viewport aspect.
	auto FovYFromHorizontal = [&Out](float HFovDeg)
	{
		if (HFovDeg <= 1.0f) { return; }
		const float Aspect = Out.ViewportWidth / FMath::Max(Out.ViewportHeight, 1.0f);
		Out.FovY = 2.0f * FMath::Atan(FMath::Tan(FMath::DegreesToRadians(HFovDeg) * 0.5f) / FMath::Max(Aspect, 0.001f));
	};

	if (APlayerController* PC = World->GetFirstPlayerController())
	{
		FRotator Rot;
		PC->GetPlayerViewPoint(Out.Loc, Rot);
		if (UGameViewportClient* VPC = World->GetGameViewport())
		{
			FVector2D Sz;
			VPC->GetViewportSize(Sz);
			if (Sz.X > 0 && Sz.Y > 0) { Out.ViewportWidth = static_cast<float>(Sz.X); Out.ViewportHeight = static_cast<float>(Sz.Y); }
		}
		if (PC->PlayerCameraManager) { FovYFromHorizontal(PC->PlayerCameraManager->GetFOVAngle()); }
		return true;
	}
#if WITH_EDITOR
	if (GCurrentLevelEditingViewportClient != nullptr)
	{
		Out.Loc = GCurrentLevelEditingViewportClient->GetViewLocation();
		if (FViewport* VP = GCurrentLevelEditingViewportClient->Viewport)
		{
			const FIntPoint Sz = VP->GetSizeXY();
			if (Sz.X > 0 && Sz.Y > 0) { Out.ViewportWidth = static_cast<float>(Sz.X); Out.ViewportHeight = static_cast<float>(Sz.Y); }
		}
		FovYFromHorizontal(GCurrentLevelEditingViewportClient->ViewFOV);
		return true;
	}
#endif
	return false;
}

void LogiBuildTileSection(UProceduralMeshComponent* Mesh, int32 Section,
	const uint8* Vertices, uint32 Stride, uint64 VerticesLen, uint32 VertexCount,
	const uint8* Indices, uint32 IndexCount)
{
	if (Mesh == nullptr || Stride < 32 || Vertices == nullptr || Indices == nullptr) { return; }
	const uint32 VCount = FMath::Min<uint32>(VertexCount, static_cast<uint32>(VerticesLen / Stride));

	TArray<FVector> Positions;
	TArray<FVector> Normals;
	TArray<FVector2D> UVs;
	Positions.Reserve(VCount);
	Normals.Reserve(VCount);
	UVs.Reserve(VCount);
	for (uint32 v = 0; v < VCount; ++v)
	{
		// Aligned copy (audit C++ L1): the Rust vertex buffer is byte-aligned (`*const u8`); a direct
		// `(const float*)` deref is UB on strict-alignment targets. Matches the building lane's Memcpy.
		float F[8]; FMemory::Memcpy(F, Vertices + static_cast<uint64>(v) * Stride, sizeof(F));
		Positions.Add(FVector(F[0], F[1], F[2])); // already cm, Z-up LH
		Normals.Add(FVector(F[3], F[4], F[5]));
		UVs.Add(FVector2D(F[6], F[7]));
	}

	// Indices are `index_count` tightly-packed little-endian u32 (the ABI contract: buffer = index_count*4
	// bytes, Rust-guaranteed). No separate indices_len crosses the FFI to clamp against (audit C++ L2), so
	// the vertex-range check below is the bounds safety net; each index is read via an aligned Memcpy (L1).
	TArray<int32> Triangles;
	Triangles.Reserve(IndexCount);
	for (uint32 t = 0; t + 3 <= IndexCount; t += 3)
	{
		uint32 Tri[3]; FMemory::Memcpy(Tri, Indices + static_cast<uint64>(t) * 4, sizeof(Tri));
		if (Tri[0] >= VCount || Tri[1] >= VCount || Tri[2] >= VCount) { continue; }
		Triangles.Add(static_cast<int32>(Tri[0]));
		Triangles.Add(static_cast<int32>(Tri[1]));
		Triangles.Add(static_cast<int32>(Tri[2]));
	}

	Mesh->CreateMeshSection_LinearColor(Section, Positions, Triangles, Normals, UVs,
		TArray<FLinearColor>(), TArray<FProcMeshTangent>(), /*bCreateCollision=*/false);
}

#if LOGI_GEO_NANITE_LANE
UStaticMesh* LogiBuildTileNaniteMesh(UObject* Outer,
	const uint8* Vertices, uint32 Stride, uint64 VerticesLen, uint32 VertexCount,
	const uint8* Indices, uint32 IndexCount,
	UMaterialInterface* Material, const TCHAR* SlotName)
{
	if (Stride < 32 || Vertices == nullptr || Indices == nullptr) { return nullptr; }
	const uint32 VCount = FMath::Min<uint32>(VertexCount, static_cast<uint32>(VerticesLen / Stride));
	const uint32 ICount = IndexCount;
	if (VCount < 3 || ICount < 3) { return nullptr; }

	FMeshDescription MeshDesc;
	FStaticMeshAttributes Attrs(MeshDesc);
	Attrs.Register();
	TVertexAttributesRef<FVector3f> Positions = Attrs.GetVertexPositions();
	TVertexInstanceAttributesRef<FVector3f> Normals = Attrs.GetVertexInstanceNormals();
	TVertexInstanceAttributesRef<FVector2f> UVs = Attrs.GetVertexInstanceUVs();
	UVs.SetNumChannels(1);

	MeshDesc.ReserveNewVertices(VCount);
	MeshDesc.ReserveNewVertexInstances(ICount);
	MeshDesc.ReserveNewPolygons(ICount / 3);
	MeshDesc.ReserveNewEdges(ICount);

	const FName Slot(SlotName != nullptr ? SlotName : TEXT("LogiTile"));
	const FPolygonGroupID Group = MeshDesc.CreatePolygonGroup();
	Attrs.GetPolygonGroupMaterialSlotNames()[Group] = Slot;

	TArray<FVertexID> VIDs;
	VIDs.SetNumUninitialized(VCount);
	for (uint32 v = 0; v < VCount; ++v)
	{
		float F[8]; FMemory::Memcpy(F, Vertices + static_cast<uint64>(v) * Stride, sizeof(F)); // aligned copy (audit C++ L1)
		const FVertexID Vid = MeshDesc.CreateVertex();
		Positions[Vid] = FVector3f(F[0], F[1], F[2]); // already Unreal cm
		VIDs[v] = Vid;
	}

	TArray<FVertexInstanceID> Corners;
	Corners.SetNum(3);
	for (uint32 t = 0; t + 3 <= ICount; t += 3)
	{
		uint32 Tri[3]; FMemory::Memcpy(Tri, Indices + static_cast<uint64>(t) * 4, sizeof(Tri)); // aligned copy (audit C++ L1/L2)
		if (Tri[0] >= VCount || Tri[1] >= VCount || Tri[2] >= VCount) { continue; }
		for (int32 c = 0; c < 3; ++c)
		{
			const uint32 Vi = Tri[c];
			float F[8]; FMemory::Memcpy(F, Vertices + static_cast<uint64>(Vi) * Stride, sizeof(F)); // aligned copy (audit C++ L1)
			const FVertexInstanceID Iid = MeshDesc.CreateVertexInstance(VIDs[Vi]);
			Normals[Iid] = FVector3f(F[3], F[4], F[5]);
			UVs.Set(Iid, 0, FVector2f(F[6], F[7]));
			Corners[c] = Iid;
		}
		MeshDesc.CreatePolygon(Group, Corners);
	}
	if (MeshDesc.Triangles().Num() == 0) { return nullptr; }

	UStaticMesh* Mesh = NewObject<UStaticMesh>(Outer, NAME_None, RF_Transient);
	Mesh->bSupportRayTracing = 0; // terrain/tile meshes don't need a per-tile BLAS
	Mesh->GetStaticMaterials().Add(FStaticMaterial(
		Material ? Material : UMaterial::GetDefaultMaterial(MD_Surface), Slot, Slot));

	FMeshNaniteSettings Nanite = Mesh->GetNaniteSettings();
	Nanite.bEnabled = true;
	Mesh->SetNaniteSettings(Nanite);

	// Keep the kernel-supplied normals (FMeshBuildSettings defaults bRecomputeNormals=true, which would
	// discard them and recompute per-tile → visible cross-tile lighting seams). Pre-create source model 0
	// with recompute disabled; BuildFromMeshDescriptions reuses it.
	Mesh->SetNumSourceModels(1);
	Mesh->GetSourceModel(0).BuildSettings.bRecomputeNormals = false;

	UStaticMesh::FBuildMeshDescriptionsParams Params;
	Params.bMarkPackageDirty = false;     // transient: never dirties a package
	Params.bUseHashAsGuid = true;         // identical tile content reuses the cached build
	Params.bFastBuild = false;            // full build → reaches the editor-only Nanite encode
	Params.bCommitMeshDescription = true; // required when not fast-building
	Params.bBuildSimpleCollision = false;
	Mesh->BuildFromMeshDescriptions({ &MeshDesc }, Params);
	return Mesh;
}
#endif // LOGI_GEO_NANITE_LANE
