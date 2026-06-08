# Research: Can Vast represent entities without Unreal Actors?

**Question.** Bevy owns entity truth. Unreal renders. Can we render entities with
**no `AActor` per entity** — and could we go to **zero actors** entirely (and would
that need engine changes)?

**Date:** 2026-06-02 · **Status:** research note (feeds ADR-0003 / the representation work)

## TL;DR

- **Per-entity actors: avoid them — proven and supported.** The right model is
  ISM/HISM for the many + a small *pool* of full actors for the significant few,
  promoted/demoted by significance. This is exactly Epic's **Mass** representation
  model, and it's what the user asked for.
- **Zero actors entirely: not really.** ISM/HISM are `UInstancedStaticMeshComponent`s,
  and a `UActorComponent` must be owned by *some* `AActor`. So "no actors" means
  **a handful of manager/host actors, none per entity** — not literally zero.
- **True actor-zero** (custom `FPrimitiveSceneProxy` registered directly to the
  render scene, bypassing components) **is possible without modifying engine
  source**, but it is deep render-thread work that re-implements culling / LOD /
  shadows yourself. Reserve it as a later optimization, not the starting point.

## What the engine gives us

**The render thread already doesn't care about actors.** A component's
`FPrimitiveSceneProxy` lives only on the render thread and holds just mesh +
material + per-instance transforms; it "knows nothing about gameplay code," so
rendering runs independently of the owning actor. An ISM uploads all instance
transforms into one GPU buffer and issues **a single draw call** for all
instances — the scaling primitive we want.

**Mass already does our exact pattern.** Mass's representation subsystem renders
the crowd via an Instanced Static Mesh Component and **swaps entities to full
Unreal actors at close range**, recycling actors through **actor pooling** (e.g.
~20k agents simulated, only the nearest ~35 shown as full actors, the rest as ISM
crowd). Processors drive the ISM↔actor transition by distance/significance. That
is precisely "real where you look": **significant → pooled actor; everything else
→ ISM instance, simulated as data.**

## The three tiers, mapped to Vast

| Tier | Unreal mechanism | Owns an actor? |
|---|---|---|
| **HighRes / LowRes** (significant few) | Pooled `AActor` + skeletal/static mesh | One pooled actor each (recycled) |
| **InstancedStaticMesh** (the many) | `UHISMComponent` instances | No per-entity actor — shares a host |
| **None** (unobserved) | Not rendered; pure Bevy entity | No |

This maps 1:1 onto the components/tiers already built (`Representation`,
`RepresentationTier`, `FLogiInstancedRenderer`, `FLogiActorPool`).

## Routes to "fewer/zero actors", ranked

1. **ISM held by one host actor + pooled actors (recommended now).** A single
   `ALogiBevyRenderHost` owns the HISM components; significant entities draw from
   an actor pool. Per-entity actors: **zero**. Total actors: **a handful**. Fully
   supported; keeps Nanite, culling, shadows, VSM "for free".
2. **Custom `UPrimitiveComponent` (no per-entity actor, lighter than ISMC).** A
   bespoke component implementing the primitive interface, still owned by one host
   actor, with a custom scene proxy — squeezes overhead the generic ISMC carries.
   More code; still needs a host actor.
3. **Custom `FPrimitiveSceneProxy` registered to `FScene` directly (true actor-zero).**
   Bypass the component/actor path entirely and push proxies + a per-instance GPU
   buffer to the render scene yourself. **No engine source changes required** (the
   `FScene`/`FPrimitiveSceneProxy`/RDG APIs are public), but you re-own culling,
   LOD selection, shadow registration, and Nanite interop — a custom renderer.
   High risk/effort; only worth it if (1) and (2) become the bottleneck.

## Recommendation

Build **route 1** now (it's the Mass-proven pattern and matches the ask), keep the
boundary clean so the renderer is swappable, and treat **route 3** as a profiled
future optimization — *not* an engine fork. "Zero per-entity actors" is achievable
today; "literally zero actors" buys little over a single host actor and costs a
custom renderer.

> Note: Bevy still owns all truth in every route. Actors/ISM/proxies remain
> non-authoritative shells fed by the batched snapshot (ADR-0005 / §8).

## Sources

- [Instanced Static Mesh Component — UE Docs](https://dev.epicgames.com/documentation/en-us/unreal-engine/instanced-static-mesh-component-in-unreal-engine)
- [ISM "Magic" devlog (scene proxy / single draw call)](https://medium.com/mossyblog/devlog-ism-instance-static-mesh-magic-part-1-d448cd020564)
- [Overview of Mass Gameplay — UE Docs](https://dev.epicgames.com/documentation/unreal-engine/overview-of-mass-gameplay-in-unreal-engine)
- [Mass Actor Pooling — UE Docs](https://dev.epicgames.com/documentation/en-us/unreal-engine/BlueprintAPI/Mass/ActorPooling)
- [Crowds & Traffic in UE5: Mass AI, 10k NPCs at 60fps — StraySpark](https://www.strayspark.studio/blog/crowd-traffic-simulation-ue5-mass-ai)
- [FPrimitiveSceneProxy — UE Docs](https://dev.epicgames.com/documentation/en-us/unreal-engine/API/Runtime/Engine/FPrimitiveSceneProxy)
- [Scene Proxies in Unreal Engine — Dr. Elliot](https://www.dr-elliot.com/posts/general/unrealsceneproxies/)
- [Custom Mesh Component scene proxy — Realities.io](https://medium.com/realities-io/creating-a-custom-mesh-component-in-ue4-part-3-the-mesh-components-scene-proxy-6965a3ea4cc9)
