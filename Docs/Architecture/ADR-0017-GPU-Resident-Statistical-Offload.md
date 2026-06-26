# ADR-0017 — GPU-Resident Statistical Agent Offload

- **Status:** Proposed — **Deferred** (dark by default, behind a cargo feature + CVar; built only when the
  profiler shows the active-set budget is bandwidth-bound on the branch-free bands — see Trigger)
- **Date:** 2026-06-09
- **Deciders:** Vast architecture
- **Amends:** [ADR-0005](ADR-0005-Bevy-ECS-Authority.md) — "Rust owns runtime gameplay truth" is refined by
  splitting **authority** (who dispatches/decides — Rust keeps) from **reproducibility** (bit-exact replay —
  forfeited per-individual on the GPU), permitting GPU-resident state that is *authoritative for aggregates,
  non-authoritative for individuals*.
- **Relies on:** [ADR-0003](ADR-0003-Representation-LOD.md) / [ADR-0015](ADR-0015-Simulation-LOD.md) (the LOD
  bands — only Warm/Cold offload) and [ADR-0001](ADR-0001-Mass-First-Unreal.md) (determinism — GPU floats are
  not bit-reproducible across vendors, which is *why* the GPU lane is non-authoritative-per-individual).
- **Most affects:** the Warm/Cold simulation bands and the renderer interop (UE reads the agent buffers).
- **Plan:** [Docs/Plans/LogiRuntime-Design.md](../Plans/LogiRuntime-Design.md) § GPU Escalation.

## Context

Simulation-LOD (ADR-0015) bounds the CPU active set to ~10⁶–10⁷ and ships the game to the **~10–30 M
fully-simulated** ceiling set by CPU DRAM bandwidth. The lever to raise the *statistical* Warm/Cold ceiling
to **~100–500 M** is GPU memory bandwidth (~5–20× CPU). But three hard constraints shape any GPU design:

- **Full readback is impossible.** 10⁹ × 64 B = 64 GB/frame, and even 10⁸ × 64 B = 6.4 GB/frame ≫ PCIe
  (~25 GB/s usable). The simulation loop must stay **resident in VRAM**; only a tiny Hot subset is read back.
- **GPU floats are not deterministic** across vendors (FMA contraction, reassociation, transcendental ULPs),
  so GPU state cannot be replay-authoritative (ADR-0001).
- **We are a Rust cdylib hosted by UE 5.8** (Rust never calls Unreal; no Unreal types cross the C ABI), so
  the compute device and its buffers must be Rust-owned, yet UE's renderer must read the agent transforms.

## Decision — the thesis (deferred)

> **The Warm/Cold statistical bands may live resident in a Rust-owned `wgpu` compute device's VRAM, advanced
> in-place by Rust compute dispatch, and shared READ-ONLY to Unreal's D3D12 renderer via an OS shared
> handle. Rust is the sole dispatcher (authority unbroken). Per-individual bit-reproducibility is forfeited
> on the GPU (the band is authoritative for AGGREGATES, non-authoritative for INDIVIDUALS). The Hot band, all
> discrete/RNG decisions, and all structural mutation stay CPU-authoritative and deterministic. Promotion to
> Hot re-materializes from seed, discarding the GPU's drifted floats — so the authoritative timeline never
> ingests a non-reproducible value. This ships dark, gated on a measured trigger.**

Concretely:

1. **Resident SoA, async Hot-set readback.** Agent fields live as per-field GPU storage buffers (true SoA →
   coalesced 1-transaction-per-warp; AoS is forbidden). A GPU stream-compaction selects the Hot set
   (`sig > threshold`) and a tiny gather (~10⁴–10⁵ × 32 B ≈ 3.2 MB) is **triple-buffered, fenced, async**
   copied to CPU at 1–2 frames latency — mirroring the existing snapshot ring. The full buffer is never
   mapped.

2. **A Rust-owned `wgpu` device, shared to UE via NT handle.** Rust owns the `wgpu` device, queue, and
   buffers (ADR-0005-clean — sole dispatcher), and dispatches on its own thread (zero per-tick C++↔Rust
   crossing — the spirit of ADR-0012). The display-visible buffer is exported as a Windows **NT shared
   handle** (`CreateSharedHandle`), imported by UE (`OpenSharedHandle` → `FRDGBuilder::RegisterExternalBuffer`)
   and a shared D3D12 fence gates UE's read on Rust's compute. **No copy, no Unreal type crosses** (a HANDLE
   is an OS primitive). *Not* UE-RHI compute (UE would own the buffers → ADR-0005 violation) and *not* CUDA
   (NVIDIA-only — unacceptable for a shipping game).

3. **The authority/reproducibility split (the ADR-0005 amendment).** Allowed on GPU: continuous, branch-free
   per-agent kinematics of Warm/Cold agents and aggregate flow/density advection. Must stay CPU: the Hot
   band in full, **all discrete decisions** (route/contract choice, spawn/despawn, promote-to-Hot) even for
   Warm/Cold agents (the GPU may compute decision *inputs*; the CPU *makes* and seeds the decision), and all
   structural mutation. Promotion re-derives the Hot individual deterministically from seed + a coarse
   aggregate-safe summary; the GPU's exact position is at most a rendering hint.

4. **Divergence is handled by sorting, not branching.** Agents are kept sorted by behaviour class (GPU radix
   sort, amortized) and dispatched per-class (indirect dispatch / wavefront) so each 32-lane warp is
   behaviour-coherent — never one megakernel `switch(behav)`. RNG-driven branches stay off-GPU (Decision 3),
   removing the worst divergence source.

5. **One kernel logic, two backends — not via ISPC-GPU.** ISPC's GPU target is Intel-Level-Zero-only
   (disqualified for a vendor-portable game). The per-agent kernel is authored as **WGSL (GPU) + ISPC (CPU
   morsel)**, kept in lockstep by a golden differential test (aggregate match within epsilon, never
   bit-exact across CPU/GPU).

**Trigger (when this leaves "deferred"):** a profiler shows the simulation-LOD active-set budget is
**bandwidth-bound on the branch-free Warm/Cold kinematic kernels** on the target hardware, AND the P0 spike
(below) confirms the GPU crossover. Until then this is **not built** — the CPU morsel engine + simulation-LOD
is the shipping path (YAGNI).

## Consequences

**Positive**
- **Raises the branch-free-band ceiling ~10–50×** (from ~10–30 M CPU to ~100–500 M GPU-resident), the only
  path to literal billions of *visibly-moving* agents.
- **Zero-copy display + zero per-tick crossing** — Rust computes, UE reads the same VRAM, fenced.
- **ADR-0005 stays intact** under the authority/reproducibility refinement; replay/netcode are untouched
  (they never depend on GPU floats).

**Negative / costs**
- **A second GPU device** in-process alongside UE's D3D12 (VRAM + driver context-switch cost; shared memory
  is referenced, not duplicated). Fallback: same-adapter-LUID, or wgpu-Vulkan-export → UE-D3D12-import (the
  better-documented direction).
- **`wgpu-hal` external-memory export maturity is the real risk** — import (Vulkan←D3D) is proven (PR #6161);
  native D3D12 export may need a `wgpu-hal` patch. This is what the P1 spike de-risks before any commitment.
- **Only branch-free, binnable workloads qualify** — heavy un-binnable branching (divergence > ~4×) or tight
  per-agent CPU coupling stays CPU regardless of N.
- **A new authored-twice kernel surface** (WGSL + ISPC) kept in sync by a golden test.

## Alternatives considered

- **CPU-only (ship ADR-0014 + ADR-0015, no GPU).** Caps fully-simulated agents at ~10–30 M. The default
  until the trigger fires — this ADR is the *escalation*, not a replacement.
- **UE RHI compute.** UE would own the device and buffers → Rust no longer owns truth (ADR-0005 violation),
  and every dispatch crosses the C ABI, chaining the sim to the render thread. Rejected.
- **CUDA via a vendored C lib.** Mature interop, best raw perf — but NVIDIA-only, a hard no for a game that
  must run on AMD/Intel/consoles. Rejected.
- **ISPC GPU target (single-source CPU+GPU).** Intel-Xe/Level-Zero only, not vendor-portable, FP not
  reproducible CPU↔GPU. Rejected for the GPU path (ISPC stays the CPU-morsel authoring tool).

## References

- Plan: [Docs/Plans/LogiRuntime-Design.md](../Plans/LogiRuntime-Design.md) § GPU Escalation (the resident
  SoA, the shared-handle interop, the cost model + crossover N, and the P0/P1 spikes).
- [ADR-0005](ADR-0005-Bevy-ECS-Authority.md) (the authority/reproducibility refinement),
  [ADR-0015](ADR-0015-Simulation-LOD.md) (the Warm/Cold bands that offload),
  [ADR-0001](ADR-0001-Mass-First-Unreal.md) (determinism — why GPU is non-authoritative-per-individual),
  [ADR-0014](ADR-0014-LogiRuntime-Unified-Executor-Reactor.md) (the CPU lane that stays primary).
- External: `wgpu` (vendor-neutral compute), D3D12 shared-handle interop (`CreateSharedHandle` /
  `OpenSharedHandle` / `FRDGBuilder::RegisterExternalBuffer`), `wgpu` PR #6161 (D3D→Vulkan shared-handle
  import, the proven direction), ISPC "for Xe" (the disqualified GPU target).
