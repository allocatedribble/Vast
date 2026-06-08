// Copyright LogiUE. All Rights Reserved.

#include "LogiTileMeshComponent.h"

#include "PrimitiveSceneProxy.h"
#include "PrimitiveViewRelevance.h"
#include "PrimitiveUniformShaderParametersBuilder.h"
#include "MaterialDomain.h"
#include "Materials/Material.h"
#include "Materials/MaterialRenderProxy.h"
#include "Materials/MaterialRelevance.h"
#include "Engine/Engine.h"
#include "SceneInterface.h"
#include "SceneView.h"
#include "SceneManagement.h"
#include "StaticMeshResources.h"          // FStaticMeshVertexBuffers
#include "DynamicMeshBuilder.h"           // FDynamicMeshVertex, FDynamicMeshIndexBuffer32
#include "LocalVertexFactory.h"
#include "RenderResource.h"
#include "RHICommandList.h"
#include "Engine/CollisionProfile.h"      // UCollisionProfile::NoCollision_ProfileName
#include "ProfilingDebugging/CpuProfilerTrace.h"
#include "HAL/IConsoleManager.h"              // TAutoConsoleVariable — r.LogiGeo.TerrainRayTracing opt-out

#if RHI_RAYTRACING
#include "RayTracingInstance.h"
#include "RayTracingGeometry.h"

// Per-tile ray-tracing opt-out (audit RT). r.RayTracing is enabled project-wide, but Lumen runs in SOFTWARE mode
// by default (no r.Lumen.HardwareRayTracing) — and software Lumen traces Mesh/Global Distance Fields + the Surface
// Cache; it NEVER consumes per-mesh BLAS. So a per-tile FRayTracingGeometry would build + keep resident thousands
// of BLAS (and add a TLAS instance every frame) for nothing. Default 0 ⇒ terrain is excluded from ray tracing: no
// BLAS is built and the proxy reports no RT representation, so it never enters the acceleration structure. Flip to
// 1 ONLY when switching to hardware-RT Lumen / RT reflections/shadows, where terrain must be in the TLAS. (Software
// Lumen GI is unaffected either way — that comes from distance fields + Lumen Scene, not RT.)
static TAutoConsoleVariable<int32> CVarLogiTerrainRayTracing(
	TEXT("r.LogiGeo.TerrainRayTracing"),
	0,
	TEXT("Geo terrain tiles build a per-tile ray-tracing BLAS (1) or are excluded from ray tracing (0, default).\n")
	TEXT("Software Lumen (the project default) uses distance fields, not BLAS, so 0 removes thousands of BLAS\n")
	TEXT("builds + resident VRAM with no visual change. Set 1 only for hardware-RT Lumen / RT reflections."),
	ECVF_RenderThreadSafe);
#endif

// ============================================================================================================
// FLogiTileSceneProxy — a minimal single-section static-geometry proxy whose GPU buffers are built (de-
// interleaved from the Rust 32-byte stream) and uploaded ON THE RENDER THREAD (audit T1).
//
// The proxy is constructed on the game thread (cheap: it just takes a shared pointer to the already-copied
// CPU bytes + the material + bounds). The engine then calls CreateRenderThreadResources() on the RENDER
// THREAD when the primitive is added to the scene — that is where the per-vertex de-interleave into the GPU
// vertex streams and the RHI buffer creation happen, off the game thread's critical path. Mirrors the stock
// FProceduralMeshSceneProxy resource shape (FStaticMeshVertexBuffers + FLocalVertexFactory +
// FDynamicMeshIndexBuffer32), so winding, bounds, materials, shadows and ray tracing behave identically.
// ============================================================================================================
class FLogiTileSceneProxy final : public FPrimitiveSceneProxy
{
public:
	SIZE_T GetTypeHash() const override
	{
		static size_t UniquePointer;
		return reinterpret_cast<size_t>(&UniquePointer);
	}

	FLogiTileSceneProxy(ULogiTileMeshComponent* Component)
		: FPrimitiveSceneProxy(Component)
		, MaterialRelevance(Component->GetMaterialRelevance(GetScene().GetShaderPlatform()))
		, VertexFactory(GetScene().GetFeatureLevel(), "FLogiTileSceneProxy")
		, Payload(Component->Payload) // shared CPU copy of the interleaved bytes (game thread already copied them)
	{
		Material = Component->GetMaterial(0);
		if (Material == nullptr)
		{
			Material = UMaterial::GetDefaultMaterial(MD_Surface);
		}
		// Render through CACHED static mesh draw commands (DrawStaticElements) instead of rebuilding an FMeshBatch
		// every frame for every visible tile — the geometry is immutable after CreateRenderThreadResources (a
		// content change recreates the whole proxy). Two engine contracts a static GPU-Scene proxy honours (both
		// done by FStaticMeshSceneProxy): EnableGPUSceneSupportFlags() makes the proxy a consistent GPU-Scene
		// participant (sets bSupportsGPUScene, clears bVFRequiresPrimitiveUniformBuffer) matching its GPU-Scene-
		// capable FLocalVertexFactory; bStaticElementsAlwaysUseProxyPrimitiveUniformBuffer lets a transform/instance
		// update skip a full static-draw-list rebuild (matters for these movable, pooled tiles). NOTE: the
		// long-hunted no-render bug was NEITHER of these — it was a missing Mesh.LODIndex in DrawStaticElements.
		EnableGPUSceneSupportFlags();
		bStaticElementsAlwaysUseProxyPrimitiveUniformBuffer = true;
	}

	virtual ~FLogiTileSceneProxy()
	{
		// Render-thread teardown (proxies are always destroyed on the render thread).
		VertexBuffers.PositionVertexBuffer.ReleaseResource();
		VertexBuffers.StaticMeshVertexBuffer.ReleaseResource();
		VertexBuffers.ColorVertexBuffer.ReleaseResource();
		IndexBuffer.ReleaseResource();
		VertexFactory.ReleaseResource();
#if RHI_RAYTRACING
		RayTracingGeometry.ReleaseResource();
#endif
	}

	// RENDER THREAD. The dominant per-tile vertex work lives here, NOT on the game thread (audit T1): de-
	// interleave the Rust 32-byte stream into FDynamicMeshVertex (the layout FStaticMeshVertexBuffers ingests
	// — packs the float normal into the tangent basis and the float UV into the texcoord buffer), then create
	// the RHI vertex/index buffers + bind the local vertex factory. The CPU byte copy (Payload) is released
	// the moment it has been uploaded, so it never outlives this call.
	virtual void CreateRenderThreadResources(FRHICommandListBase& RHICmdList) override
	{
		TRACE_CPUPROFILER_EVENT_SCOPE(LogiGeoTileUploadRT);

		const TSharedPtr<ULogiTileMeshComponent::FLogiTilePayload, ESPMode::ThreadSafe> Data = Payload;
		if (!Data.IsValid() || Data->VertexCount == 0 || Data->Indices.Num() < 3 || Data->Stride < 32)
		{
			Payload.Reset();
			return; // nothing to draw — bIsValid stays false, GetDynamicMeshElements skips this proxy
		}

		const uint32 VCount = Data->VertexCount;
		const uint32 Stride = Data->Stride;
		const uint8* const Base = Data->InterleavedVerts.GetData();

		// De-interleave pos/normal/uv from the packed 32-byte stream into the engine's dynamic-vertex form.
		// Aligned copy per field (the Rust buffer is byte-aligned `*const u8`; a direct float* deref is UB on
		// strict-alignment targets) — matches the retired LogiBuildTileSection's L1 discipline, just on the RT.
		TArray<FDynamicMeshVertex> Vertices;
		Vertices.SetNumUninitialized(VCount);
		for (uint32 v = 0; v < VCount; ++v)
		{
			float F[8];
			FMemory::Memcpy(F, Base + static_cast<uint64>(v) * Stride, sizeof(F));
			FDynamicMeshVertex& Out = Vertices[v];
			Out.Position = FVector3f(F[0], F[1], F[2]);         // already cm, Z-up LH
			// Preserve the kernel-supplied normal in the tangent-Z slot (recomputing per-tile would cause
			// cross-tile lighting seams). Tangent-X is the fixed (1,0,0) reference with a +1 basis determinant
			// — identical to what the retired proc-mesh path produced (it passed no tangents, so FProcMeshTangent
			// defaulted to exactly this). Terrain/atlas materials sample by explicit/world UV, not tangent-space
			// normal maps, so the exact tangent direction is immaterial; only the normal must survive.
			Out.TangentX = FVector3f(1.0f, 0.0f, 0.0f);
			Out.TangentZ = FVector3f(F[3], F[4], F[5]);
			Out.TangentZ.Vector.W = 127;                       // basis determinant sign = +1 (matches the old path)
			const FVector2f UV(F[6], F[7]);
			for (int32 c = 0; c < MAX_STATIC_TEXCOORDS; ++c)
			{
				Out.TextureCoordinate[c] = UV;
			}
			Out.Color = FColor::White;
		}

		// Indices: the payload holds the raw u32 stream (flat-copied on the game thread). Validate HERE (render
		// thread) — drop any triangle that references an out-of-range vertex (the bounds safety net; there is no
		// separate indices_len across the FFI). A trailing 1-or-2 index remainder (not a whole triangle) is
		// ignored. Self-copy is safe: we filter from Data->Indices into the separate IndexBuffer.Indices.
		const TArray<uint32>& Raw = Data->Indices;
		IndexBuffer.Indices.Reset(Raw.Num());
		for (int32 t = 0; t + 3 <= Raw.Num(); t += 3)
		{
			const uint32 I0 = Raw[t], I1 = Raw[t + 1], I2 = Raw[t + 2];
			if (I0 < VCount && I1 < VCount && I2 < VCount)
			{
				IndexBuffer.Indices.Add(I0);
				IndexBuffer.Indices.Add(I1);
				IndexBuffer.Indices.Add(I2);
			}
		}
		if (IndexBuffer.Indices.Num() < 3)
		{
			Payload.Reset();
			return; // every triangle was degenerate/out-of-range — nothing to draw
		}

		// Build the 3 vertex streams (position / tangents+texcoords / color) and bind the local vertex factory.
		// We are ON the render thread with a live command list, so passing it makes InitFromDynamicVertex run
		// SYNCHRONOUSLY: it fills the CPU vertex data AND initializes the position/static-mesh/color vertex
		// buffers' RHI AND binds + initializes the vertex factory — all here, off the game thread. (Passing a
		// null cmd list instead would defer that to an enqueued render command — we don't need to, we're already
		// on the render thread.) So we must NOT re-init those buffers/factory afterwards; only the index buffer
		// is left, which InitFromDynamicVertex does not touch.
		VertexBuffers.InitFromDynamicVertex(RHICmdList, &VertexFactory, Vertices, /*NumTexCoords=*/1);
		IndexBuffer.InitResource(RHICmdList);

#if RHI_RAYTRACING
		// Build a per-tile BLAS only when terrain RT is explicitly enabled (default off — software Lumen, the
		// project default, ignores per-mesh BLAS, so building one per tile is pure render-thread + VRAM waste).
		if (IsRayTracingEnabled() && CVarLogiTerrainRayTracing.GetValueOnRenderThread() != 0)
		{
			FRayTracingGeometryInitializer Initializer;
			Initializer.DebugName = FName(TEXT("LogiTile"));
			Initializer.IndexBuffer = IndexBuffer.IndexBufferRHI;
			Initializer.TotalPrimitiveCount = IndexBuffer.Indices.Num() / 3;
			Initializer.GeometryType = RTGT_Triangles;
			Initializer.bFastBuild = true;
			Initializer.bAllowUpdate = false;

			FRayTracingGeometrySegment Segment;
			Segment.VertexBuffer = VertexBuffers.PositionVertexBuffer.VertexBufferRHI;
			Segment.MaxVertices = VertexBuffers.PositionVertexBuffer.GetNumVertices();
			Segment.NumPrimitives = Initializer.TotalPrimitiveCount;
			Initializer.Segments.Add(Segment);

			RayTracingGeometry.SetInitializer(Initializer);
			RayTracingGeometry.InitResource(RHICmdList);
		}
#endif

		bIsValid = true;
		Payload.Reset(); // bytes are now in the RHI buffers — free the CPU copy promptly
	}

	virtual void GetDynamicMeshElements(const TArray<const FSceneView*>& Views, const FSceneViewFamily& ViewFamily,
		uint32 VisibilityMap, FMeshElementCollector& Collector) const override
	{
		if (!bIsValid)
		{
			return; // upload not finished / degenerate tile
		}

		const bool bWireframe = AllowDebugViewmodes() && ViewFamily.EngineShowFlags.Wireframe;
		FColoredMaterialRenderProxy* WireframeMaterialInstance = nullptr;
		if (bWireframe)
		{
			WireframeMaterialInstance = new FColoredMaterialRenderProxy(
				GEngine->WireframeMaterial ? GEngine->WireframeMaterial->GetRenderProxy() : nullptr,
				FLinearColor(0, 0.5f, 1.f));
			Collector.RegisterOneFrameMaterialProxy(WireframeMaterialInstance);
		}

		FMaterialRenderProxy* MaterialProxy = bWireframe ? WireframeMaterialInstance : Material->GetRenderProxy();

		for (int32 ViewIndex = 0; ViewIndex < Views.Num(); ViewIndex++)
		{
			if (VisibilityMap & (1 << ViewIndex))
			{
				FMeshBatch& Mesh = Collector.AllocateMesh();
				FMeshBatchElement& BatchElement = Mesh.Elements[0];
				BatchElement.IndexBuffer = &IndexBuffer;
				Mesh.bWireframe = bWireframe;
				Mesh.VertexFactory = &VertexFactory;
				Mesh.MaterialRenderProxy = MaterialProxy;

				FDynamicPrimitiveUniformBuffer& DynamicPrimitiveUniformBuffer = Collector.AllocateOneFrameResource<FDynamicPrimitiveUniformBuffer>();
				FPrimitiveUniformShaderParametersBuilder Builder;
				BuildUniformShaderParameters(Builder);
				DynamicPrimitiveUniformBuffer.Set(Collector.GetRHICommandList(), Builder);
				BatchElement.PrimitiveUniformBufferResource = &DynamicPrimitiveUniformBuffer.UniformBuffer;

				BatchElement.FirstIndex = 0;
				BatchElement.NumPrimitives = IndexBuffer.Indices.Num() / 3;
				BatchElement.MinVertexIndex = 0;
				BatchElement.MaxVertexIndex = VertexBuffers.PositionVertexBuffer.GetNumVertices() - 1;
				Mesh.ReverseCulling = IsLocalToWorldDeterminantNegative();
				Mesh.Type = PT_TriangleList;
				Mesh.DepthPriorityGroup = SDPG_World;
				Mesh.bCanApplyViewModeOverrides = false;
				Collector.AddMesh(ViewIndex, Mesh);
			}
		}
	}

	// RENDER THREAD, once: register this tile's CACHED static mesh draw command. Called when the primitive is added
	// to the scene (after CreateRenderThreadResources, so the GPU buffers are live) and whenever static draw lists
	// rebuild. Immutable geometry ⇒ this is the whole draw-submission cost; no per-frame FMeshBatch build
	// (GetDynamicMeshElements now only handles the wireframe view). Mirrors FStaticMeshSceneProxy::GetMeshElement:
	// bind the VERTEX-FACTORY uniform buffer (FLocalVertexFactory needs it for manual vertex fetch) and set NO
	// primitive UB — GPU-Scene supplies the per-primitive transform via the VF's primitive-id stream (the proxy is a
	// GPU-Scene participant via EnableGPUSceneSupportFlags in the ctor).
	virtual void DrawStaticElements(FStaticPrimitiveDrawInterface* PDI) override
	{
		if (!bIsValid)
		{
			return; // upload not finished / degenerate tile — nothing to cache
		}
		FMeshBatch Mesh;
		Mesh.VertexFactory = &VertexFactory;
		Mesh.MaterialRenderProxy = Material->GetRenderProxy();
		Mesh.ReverseCulling = IsLocalToWorldDeterminantNegative();
		Mesh.Type = PT_TriangleList;
		Mesh.DepthPriorityGroup = SDPG_World;
		Mesh.bCanApplyViewModeOverrides = false;
		// THE static-path no-render fix. A cached static batch is gated per-view by LOD selection
		// (FLODMask::ContainsLOD, SceneVisibility.cpp). FMeshBatch defaults LODIndex = INDEX_NONE (-1) ⇒
		// bInvalidLODIndex ⇒ GetLODIndex() returns -1 ⇒ ContainsLOD(-1) is ALWAYS false (LODs are unsigned 7-bit
		// fields) ⇒ the command is cached but NEVER marked visible ⇒ nothing draws, silently (LOD non-selection is
		// normal control flow, not an error — so no ensure fires). Single-LOD tile ⇒ LODIndex 0, exactly as
		// FStaticMeshSceneProxy::GetMeshElement sets it. FProceduralMeshSceneProxy (the shape we mirrored) is
		// dynamic-only, so it never had this line to copy — which is why every batch/flag/GPU-Scene fix missed it.
		Mesh.LODIndex = 0;

		FMeshBatchElement& BatchElement = Mesh.Elements[0];
		BatchElement.IndexBuffer = &IndexBuffer;
		BatchElement.VertexFactoryUserData = VertexFactory.GetUniformBuffer();
		BatchElement.FirstIndex = 0;
		BatchElement.NumPrimitives = IndexBuffer.Indices.Num() / 3;
		BatchElement.MinVertexIndex = 0;
		BatchElement.MaxVertexIndex = VertexBuffers.PositionVertexBuffer.GetNumVertices() - 1;
		PDI->DrawMesh(Mesh, FLT_MAX);
	}

	virtual FPrimitiveViewRelevance GetViewRelevance(const FSceneView* View) const override
	{
		// Terrain renders through the CACHED STATIC path; wireframe falls back to the dynamic path's per-view
		// wireframe material. Static and dynamic are mutually exclusive so a tile is never submitted twice.
		const bool bWireframe = AllowDebugViewmodes() && View->Family->EngineShowFlags.Wireframe;
		FPrimitiveViewRelevance Result;
		Result.bDrawRelevance = IsShown(View);
		Result.bShadowRelevance = IsShadowCast(View);
		Result.bStaticRelevance = !bWireframe;
		Result.bDynamicRelevance = bWireframe;
		Result.bRenderInMainPass = ShouldRenderInMainPass();
		Result.bUsesLightingChannels = GetLightingChannelMask() != GetDefaultLightingChannelMask();
		Result.bRenderCustomDepth = ShouldRenderCustomDepth();
		MaterialRelevance.SetPrimitiveViewRelevance(Result);
		Result.bVelocityRelevance = DrawsVelocity() && Result.bOpaque && Result.bRenderInMainPass;
		return Result;
	}

	virtual bool CanBeOccluded() const override { return !MaterialRelevance.bDisableDepthTest; }
	virtual uint32 GetMemoryFootprint() const override { return sizeof(*this) + GetAllocatedSize(); }

#if RHI_RAYTRACING
	// Excluded from ray tracing unless r.LogiGeo.TerrainRayTracing is set (default off — software Lumen ignores
	// per-mesh BLAS). When off, no BLAS was built (CreateRenderThreadResources) and the proxy advertises no RT
	// representation, so it never enters the TLAS. Consistent across all tile proxies at any instant (it reads the
	// same CVar), satisfying the engine's "IsRayTracingRelevant must be consistent per proxy type" rule.
	virtual bool IsRayTracingRelevant() const override { return CVarLogiTerrainRayTracing.GetValueOnRenderThread() != 0; }
	virtual bool HasRayTracingRepresentation() const override
	{
		return CVarLogiTerrainRayTracing.GetValueOnRenderThread() != 0 && RayTracingGeometry.IsValid();
	}

	virtual void GetDynamicRayTracingInstances(FRayTracingInstanceCollector& Collector) override
	{
		if (!bIsValid || !RayTracingGeometry.IsValid())
		{
			return;
		}
		const TConstArrayView<const FSceneView*> Views = Collector.GetViews();
		const uint32 VisibilityMap = Collector.GetVisibilityMap();
		const int32 FirstActiveViewIndex = FMath::CountTrailingZeros(VisibilityMap);
		if (!Views.IsValidIndex(FirstActiveViewIndex))
		{
			return;
		}

		FRayTracingInstance RayTracingInstance;
		RayTracingInstance.Geometry = &RayTracingGeometry;
		RayTracingInstance.InstanceTransforms.Add(GetLocalToWorld());

		FMeshBatch MeshBatch;
		MeshBatch.VertexFactory = &VertexFactory;
		MeshBatch.SegmentIndex = 0;
		MeshBatch.MaterialRenderProxy = Material->GetRenderProxy();
		MeshBatch.ReverseCulling = IsLocalToWorldDeterminantNegative();
		MeshBatch.Type = PT_TriangleList;
		MeshBatch.DepthPriorityGroup = SDPG_World;
		MeshBatch.bCanApplyViewModeOverrides = false;
		MeshBatch.CastRayTracedShadow = IsShadowCast(Views[FirstActiveViewIndex]);

		FMeshBatchElement& BatchElement = MeshBatch.Elements[0];
		BatchElement.IndexBuffer = &IndexBuffer;
		FDynamicPrimitiveUniformBuffer& DynamicPrimitiveUniformBuffer = Collector.AllocateOneFrameResource<FDynamicPrimitiveUniformBuffer>();
		FPrimitiveUniformShaderParametersBuilder Builder;
		BuildUniformShaderParameters(Builder);
		DynamicPrimitiveUniformBuffer.Set(Collector.GetRHICommandList(), Builder);
		BatchElement.PrimitiveUniformBufferResource = &DynamicPrimitiveUniformBuffer.UniformBuffer;
		BatchElement.FirstIndex = 0;
		BatchElement.NumPrimitives = IndexBuffer.Indices.Num() / 3;
		BatchElement.MinVertexIndex = 0;
		BatchElement.MaxVertexIndex = VertexBuffers.PositionVertexBuffer.GetNumVertices() - 1;
		RayTracingInstance.Materials.Add(MeshBatch);

		for (int32 ViewIndex = 0; ViewIndex < Views.Num(); ViewIndex++)
		{
			if (VisibilityMap & (1 << ViewIndex))
			{
				Collector.AddRayTracingInstance(ViewIndex, RayTracingInstance);
			}
		}
	}
#endif // RHI_RAYTRACING

private:
	UMaterialInterface* Material = nullptr;
	FMaterialRelevance MaterialRelevance;

	FStaticMeshVertexBuffers VertexBuffers;
	FDynamicMeshIndexBuffer32 IndexBuffer;
	FLocalVertexFactory VertexFactory;
#if RHI_RAYTRACING
	FRayTracingGeometry RayTracingGeometry;
#endif

	// CPU copy of the interleaved bytes, owned only between construction and the render-thread upload, then
	// released. Read/written only by this proxy (game thread sets it in the ctor; render thread consumes it).
	TSharedPtr<ULogiTileMeshComponent::FLogiTilePayload, ESPMode::ThreadSafe> Payload;

	// Render-thread flag: true once CreateRenderThreadResources has uploaded valid geometry. Gates draw/RT so a
	// proxy that hasn't finished uploading (or a degenerate tile) is silently skipped.
	bool bIsValid = false;
};

// ============================================================================================================
// ULogiTileMeshComponent
// ============================================================================================================

ULogiTileMeshComponent::ULogiTileMeshComponent()
{
	// Terrain render shell — no collision, no navigation, no ticking. Verts are absolute world cm, so the
	// owner sets SetAbsolute(true,...) on acquire; this component never participates in gameplay (ADR-0007).
	PrimaryComponentTick.bCanEverTick = false;
	CastShadow = true;
	bUseAsOccluder = true;
	SetCollisionEnabled(ECollisionEnabled::NoCollision);
	SetCollisionProfileName(UCollisionProfile::NoCollision_ProfileName);
}

void ULogiTileMeshComponent::SetTileGeometry(
	const uint8* Vertices, uint32 Stride, uint64 VerticesLen, uint32 VertexCount,
	const uint8* Indices, uint32 IndexCount,
	const FVector3f& BoundsCenter, float BoundsRadius)
{
	// A malformed/empty view renders nothing (matches the retired LogiBuildTileSection's guards).
	if (Stride < 32 || Vertices == nullptr || Indices == nullptr || VertexCount == 0 || IndexCount < 3)
	{
		ClearTileGeometry();
		return;
	}

	// Clamp the vertex count to what the buffer actually holds (the ABI says vertices_len = count*stride, but
	// defend in depth), then FLAT-copy the interleaved bytes out of the frame-transient Rust buffer. This is
	// the whole game-thread vertex cost now: one Memcpy, bandwidth-bound, no per-vertex work (audit T1).
	const uint32 VCount = FMath::Min<uint32>(VertexCount, static_cast<uint32>(VerticesLen / Stride));
	if (VCount == 0)
	{
		ClearTileGeometry();
		return;
	}

	TSharedPtr<FLogiTilePayload, ESPMode::ThreadSafe> NewPayload = MakeShared<FLogiTilePayload, ESPMode::ThreadSafe>();
	NewPayload->VertexCount = VCount;
	NewPayload->Stride = Stride;

	// FLAT-copy the interleaved verts AND the indices out of the frame-transient Rust buffers — two byte-level
	// Memcpys, bandwidth-bound, with NO per-element work on the game thread (audit T1). The index buffer is, by
	// the ABI contract, exactly IndexCount*4 tightly-packed little-endian u32 (our target endianness), so a raw
	// memcpy is valid; the per-triangle bounds-clamp (the safety net for out-of-range indices) is deferred to
	// the render thread, which has the vertex count in the payload.
	const int64 VertBytes = static_cast<int64>(VCount) * Stride;
	NewPayload->InterleavedVerts.SetNumUninitialized(VertBytes);
	FMemory::Memcpy(NewPayload->InterleavedVerts.GetData(), Vertices, VertBytes);

	const int32 ICount = static_cast<int32>(IndexCount);
	NewPayload->Indices.SetNumUninitialized(ICount);
	FMemory::Memcpy(NewPayload->Indices.GetData(), Indices, static_cast<int64>(ICount) * sizeof(uint32));

	Payload = MoveTemp(NewPayload);
	LocalBounds = FBoxSphereBounds(FVector(BoundsCenter), FVector(BoundsRadius), BoundsRadius);

	// Rebuild THIS tile's proxy only (O(1) — the documented per-tile-component design). MarkRenderStateDirty
	// recreates the scene proxy, which runs the render-thread bulk upload above. UpdateBounds first so the new
	// proxy is registered with the correct bounds (else a stale/zero bound could cull it the first frame).
	UpdateBounds();
	MarkRenderStateDirty();
}

void ULogiTileMeshComponent::ClearTileGeometry()
{
	if (!Payload.IsValid() && LocalBounds.SphereRadius == 0.0f)
	{
		return; // already empty — no proxy churn
	}
	Payload.Reset();
	LocalBounds = FBoxSphereBounds(ForceInit);
	MarkRenderStateDirty(); // drop the proxy (O(1): this tile only)
}

FPrimitiveSceneProxy* ULogiTileMeshComponent::CreateSceneProxy()
{
	// No geometry => no proxy (a pooled-but-idle component renders nothing without an empty proxy in the scene).
	if (!Payload.IsValid() || Payload->VertexCount == 0 || Payload->Indices.Num() < 3)
	{
		return nullptr;
	}
	return new FLogiTileSceneProxy(this);
}

FBoxSphereBounds ULogiTileMeshComponent::CalcBounds(const FTransform& LocalToWorld) const
{
	// Rust supplies exact tile bounds in UE cm (O(1) — no per-vertex scan). Verts are absolute world cm and
	// the component is SetAbsolute, so LocalToWorld is identity in practice; transform anyway for correctness.
	if (LocalBounds.SphereRadius <= 0.0f)
	{
		return FBoxSphereBounds(LocalToWorld.GetLocation(), FVector::ZeroVector, 0.0f);
	}
	return LocalBounds.TransformBy(LocalToWorld);
}
