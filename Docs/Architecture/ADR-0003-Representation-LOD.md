# ADR-0003 — Representation & Simulation LOD via a Unified Significance Score

- **Status:** Accepted
- **Date:** 2026-06-01
- **Deciders:** LogiUE architecture
- **Related:** [ADR-0005](ADR-0005-Bevy-ECS-Authority.md), [ADR-0001](ADR-0001-Mass-First-Unreal.md), [Budgets](../Performance/Budgets.md)

> **Still in force under [ADR-0005](ADR-0005-Bevy-ECS-Authority.md).** The unified significance-score model
> is unchanged. Only the owner moves: **simulation LOD is now computed in Bevy (Rust)** and feeds a batched
> representation snapshot; Unreal consumes it for representation / MetaHuman LODSync / Animation Budget
> Allocator. Read "Mass simulation LOD" below as "the sim's LOD, computed in Bevy."

## Context

"Real where you look, faked where you don't" is the core illusion. At millions of agents we cannot
afford a naive per-agent distance check: distance alone mis-ranks a profitable hub two kilometers away
against an irrelevant agent ten meters behind the camera. We need **one ranking signal** that decides
who gets fidelity, and we need fidelity to be **a budget that is allocated**, not a threshold that is
independently tripped per agent.

MassGameplay already provides the machinery: a representation stack with **High-resolution Actor,
Low-resolution Actor, Instanced Static Mesh, and No representation**, with Mass LOD producing **High /
Medium / Low / Off** states and *separate* representation, simulation, and replication LOD clients.
MetaHuman adds **LODSync** (coordinating face/body/hair/clothing LODs, with Forced LOD and Min LOD for
distant crowds) and the **Animation Budget Allocator** (dynamically throttling skeletal-mesh ticking to
a fixed CPU budget).

## Decision

**A single unified significance score per agent drives all LOD decisions, and fidelity is allocated
against fixed budgets — not gated by raw distance.**

### Significance inputs
The score is a weighted function of at least:
- **Camera distance** and **screen size / projected footprint**
- **Frustum state** (in view, peripheral, behind camera)
- **Player interaction / selection** (the courier you clicked is always significant)
- **Hub importance** (major logistics hubs outrank empty road segments)
- **Route profitability** (economically load-bearing flows outrank noise)
- **Debug pinning** (a developer-pinned entity is force-significant)

### What the score drives
| Consumer | Effect |
|---|---|
| **Mass Simulation LOD** | High / Medium / Low / Off update rates & model fidelity for the simulation step. |
| **Mass Representation LOD** | High-res Actor → Low-res Actor → ISM → No representation. |
| **MetaHuman LODSync** | Forced LOD / Min LOD for crowds; coordinated face/body/hair/cloth LOD. |
| **Animation Budget Allocator** | Distributes a fixed skeletal-mesh tick budget to the most significant agents first. |

### Budget, not distance — the operative rule
- There is a **fixed budget** of high-res actors, of animated skeletal meshes, of full-rate simulation
  slots, etc. (see [Budgets](../Performance/Budgets.md)).
- The significance score **ranks** agents; the budget is filled **top-down** until exhausted. An agent
  is high-fidelity because it *won a budgeted slot*, not because it crossed a distance line.
- Therefore the same agent at the same distance may be High in a quiet scene and Low in a crowded one.
  That is correct and intended.
- Representation, simulation, and replication budgets are **separate** (MassGameplay exposes separate
  LOD clients). An agent can be simulated at Medium while represented as an ISM.

### MetaHuman LOD specs become hard data
MetaHuman's published per-LOD drops (head vertices, joints, hair geometry, physics support) are not
flavor text — they are **inputs to the budget model**. Each LOD's real cost feeds the per-agent cost
estimate used when filling the animation/representation budgets. These figures live in
[Budgets](../Performance/Budgets.md) and must be populated from the official MetaHuman LOD spec, not
guessed.

## Consequences

**Positive**
- One signal to tune instead of many independent distance thresholds.
- Hard, predictable frame cost: fidelity can never exceed the budget regardless of scene density.
- Maps cleanly onto MassGameplay's existing separate LOD clients and MetaHuman tooling.

**Negative / costs**
- The scoring weights need tuning and profiling per world view (orbital vs. street level).
- Budget starvation must degrade gracefully (no popping/thrash); hysteresis is required so agents don't
  oscillate across budget boundaries frame to frame.
- Cross-system coordination (Mass LOD ↔ LODSync ↔ Animation Budget Allocator) must agree on the score.

## Alternatives considered

- **Per-agent distance LOD.** Rejected: mis-ranks importance, has no global cost ceiling, thrashes in
  dense scenes.
- **Independent budgets per subsystem with no shared score.** Rejected: subsystems disagree about which
  agent matters; wasted budget on agents one system thinks are important and another doesn't.

## References

- Unreal: MassGameplay representation & LOD (High-res/Low-res Actor, ISM, None; High/Medium/Low/Off;
  separate representation/simulation/replication LOD clients).
- Unreal: MetaHuman LODSync (Forced LOD, Min LOD), MetaHuman LOD specs, Animation Budget Allocator.
- LogiUE [`AGENTS.md`](../../AGENTS.md) § 3 rule 11; [Budgets](../Performance/Budgets.md).
