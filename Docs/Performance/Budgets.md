# Performance Budgets

> Budgets are **contracts**, not aspirations. Fidelity is allocated against these numbers top-down by
> the significance score (see [ADR-0003](../Architecture/ADR-0003-Representation-LOD.md)); it is never
> allowed to exceed them. Numbers marked **(TODO: profile)** or **(TODO: from spec)** are placeholders
> that must be replaced with measured/official values before they are treated as authoritative — do not
> ship guesses as facts.

> **Updated by [ADR-0005](../Architecture/ADR-0005-Bevy-ECS-Authority.md):** the simulation is now **Rust
> `bevy_ecs`**, not Mass. Read the "Mass simulation" rows below as the **Bevy fixed-step sim**, profiled
> with a Rust profiler (Tracy / `puffin`) under `cargo` — not MassInsights. The **FFI snapshot crossing**
> (Rust → Unreal, per frame) is a new budgeted line item. Unreal Insights still covers the bridge + rendering.

## 1. Frame budget

Target interactive frame rate: **60 fps → 16.6 ms** per frame at street-level views.
(Orbital / overview views may target a different rate; record it here once decided.)

| Thread / phase | Budget (ms) | Notes |
|---|---|---|
| Game thread (total) | **(TODO: profile)** | Includes Mass simulation + Actor ticks + gameplay. |
| └ Mass simulation step | **(TODO: profile)** | Fixed-step deterministic core; see § 2. |
| └ Representation / Actor ticks | **(TODO: profile)** | Pooled shells only; throttled by Animation Budget Allocator. |
| Render thread | **(TODO: profile)** | Draw submission; favor cached static draws over dynamic. |
| GPU | **(TODO: profile)** | Lumen + VSM + Nanite; profile **per view** (orbital vs. street). |

> The deterministic simulation step is decoupled from frame rate
> ([ADR-0001 § Determinism](../Architecture/ADR-0001-Mass-First-Unreal.md)); its budget is per
> **sim tick**, which may run at a different cadence than rendering.

## 2. Bevy simulation budgets (Rust)

| Quantity | Target | Notes |
|---|---|---|
| Total active entities | **millions** (design ceiling: 1M+ meaningfully simulated) | Most at Low/Off sim LOD. |
| Full-rate (High sim LOD) entities | **(TODO: profile)** | Hard cap; significance fills it top-down. |
| Medium sim LOD entities | **(TODO: profile)** | Reduced update rate / simplified model. |
| Sim-step time budget | **(TODO: profile)** ms | Per fixed tick; see § 1. |
| Fragment memory / entity | **(TODO: profile)** bytes | Keep Fragments lean; archetypes cache-friendly. |
| Command-buffer ops / tick | **(TODO: profile)** | Structural churn is expensive; batch and bound it. |

## 3. Representation budgets (separate from simulation — see ADR-0003)

| Representation tier | Budget (count) | Backed by |
|---|---|---|
| High-resolution Actor | **(TODO: profile)** | Pooled skeletal-mesh actors; the scarcest budget. |
| Low-resolution Actor | **(TODO: profile)** | Pooled, cheaper mesh/anim. |
| Instanced Static Mesh | **(TODO: profile)** | Large counts; one draw per instance batch. |
| No representation | unbounded | Pure Mass; no scene cost. |

## 4. Animation budget

- **Animation Budget Allocator** caps total skeletal-mesh tick cost at a fixed CPU budget and
  distributes it to the most significant agents first (see
  [ADR-0003](../Architecture/ADR-0003-Representation-LOD.md)).
- Target animation CPU budget: **(TODO: profile)** ms/frame on the game thread.

### MetaHuman LOD cost table — *hard data for the significance model*

Populate each cell from the **official MetaHuman LOD specification** (do not guess). These per-LOD costs
feed the per-agent cost estimate used when filling the animation/representation budgets.

| LOD | Head verts | Joints | Hair geometry | Cloth/physics | Intended range |
|---|---|---|---|---|---|
| LOD0 | (TODO: from spec) | (TODO) | (TODO) | (TODO) | Hero / interaction |
| LOD1 | (TODO) | (TODO) | (TODO) | (TODO) | Near |
| LOD2 | (TODO) | (TODO) | (TODO) | (TODO) | Mid |
| LOD3 | (TODO) | (TODO) | (TODO) | (TODO) | Mid-far |
| LOD4 | (TODO) | (TODO) | (TODO) | (TODO) | Far |
| LOD5–7 | (TODO) | (TODO) | (TODO) | minimal/none | Distant crowd (Forced/Min LOD) |

> The published spec shows **large drops** in head vertices, joints, hair geometry, and physics support
> across LODs — that steep curve is exactly why MetaHumans can appear in crowds at all. Capture the real
> numbers here so the budget allocator can reason about true cost.

## 5. Rendering posture & cost knobs (profile per view)

| System | Posture | Watch |
|---|---|---|
| **Nanite** | Default for supported geometry; **Nanite Landscape** for terrain. | Nanite landscapes still stream non-Nanite data (needed by RVT, water). Budget both. |
| **Lumen** | Default dynamic GI/reflections (mm → km). | Quality knobs get expensive fast; profile each world view separately. |
| **Virtual Shadow Maps** | Default shadowing, pairs with Nanite. | Page cache pressure in dense scenes. |
| **Runtime Virtual Texturing** | Terrain-material compositing, decals, terrain-conforming splines. | Cache size vs. update cost. |
| **World Partition / HLOD** | Authored world scale; HLOD proxies for unloaded cells. | Cell size & streaming distance are budgets — record chosen values. |

| World Partition knob | Value | Notes |
|---|---|---|
| Cell size | **(TODO: decide)** | |
| Streaming distance | **(TODO: decide)** | |
| HLOD layers | **(TODO: decide)** | Proxy draw-call reduction for unloaded cells. |

## 6. Custom terrain rendering (only if higher-level systems cannot express it)

If a custom terrain renderer is required (see [ADR-0002](../Architecture/ADR-0002-Rust-Terrain-Kernel.md)),
prefer **retained / static mesh** paths over dynamic per-frame mesh generation: Unreal caches and merges
static draw work, whereas dynamic mesh batches are rebuilt every frame. Use **RDG/RHI** only where
higher-level systems genuinely cannot express the renderer efficiently.

- Dynamic per-frame mesh rebuilds: **avoid** (flag any in review).
- RDG passes: justify with a profile capture; exploit transient resources, pass culling, async compute.

## Revision

Replace placeholders as profiling lands. Every number that becomes real should cite the capture or spec
it came from. See [Profiling-Checklist.md](Profiling-Checklist.md) for how to measure each row.
