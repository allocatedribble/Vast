# CLAUDE.md

**Read [`AGENTS.md`](AGENTS.md) first. It is the architectural constitution for this project and it
governs every change you make.** This file adds Claude Code–specific operational notes; it does not
override `AGENTS.md`.

## The one rule that matters most

**Mass owns runtime gameplay state. Actors are shells.** Delivery / agent / route / economy / terrain
state goes in **Mass Fragments**, mutated by **Processors**, assembled by **Traits**. `AActor`,
`UObject`, components, Blueprints, and widgets are presentation / authoring / integration only. If you
are about to store simulation truth on an Actor, stop — see [`AGENTS.md`](AGENTS.md) § 1.

## Architecture decision records (read before designing)

- [ADR-0001 — Mass-First Unreal](Docs/Architecture/ADR-0001-Mass-First-Unreal.md) — Mass is the
  runtime authority; the exact **allowed Actor zones** are listed here.
- [ADR-0002 — Rust Terrain Kernel](Docs/Architecture/ADR-0002-Rust-Terrain-Kernel.md) — terrain truth
  is native Rust; Unreal only renders it.
- [ADR-0003 — Representation & Simulation LOD](Docs/Architecture/ADR-0003-Representation-LOD.md) —
  one unified significance score drives Mass LOD, MetaHuman LODSync, and the Animation Budget Allocator.
- [ADR-0004 — No Compatibility Code](Docs/Architecture/ADR-0004-No-Compatibility-Code.md) — UE 5.8
  only; no Unity/DOTS shims; no backward-compat branches.

## Performance docs

- [Docs/Performance/Budgets.md](Docs/Performance/Budgets.md)
- [Docs/Performance/Profiling-Checklist.md](Docs/Performance/Profiling-Checklist.md)

## Environment & tooling

- **Engine:** Unreal Engine 5.8. Plugins enabled: `ModelingToolsEditorMode`, `ModelContextProtocol`,
  `MassAI`, `MassGameplay` (see `LogiUE.uproject`).
- **MCP bridge:** a **running** Unreal Editor exposes MCP tools at `http://localhost:3000/mcp`
  (`unreal-engine` server, provided by `Plugins/McpAutomationBridge`). Use these tools to inspect the
  live editor, run console commands, profile, and validate — but treat the editor as a shared, live
  session: prefer non-destructive operations and Live Coding over forced UBT relinks.
- **Verify a clean compile** by confirming the editor loaded the target receipt and that
  `Saved/Logs/LogiUE.log` has no `Error:`/`Fatal` lines (do not assume — check the log).
- **Platform:** Windows. Use PowerShell syntax in the shell.

## Git conventions

- The repo uses **Git LFS** for binary assets and ignores all build output
  (`Binaries/`, `Intermediate/`, `Saved/`, `DerivedDataCache/`, generated `.sln/.slnx`).
- Commits are authored with the GitHub `noreply` email (set repo-locally) to satisfy account email
  privacy — do not "fix" this back to a private address.
- Commit or push **only when asked**.

## House style

- Match the surrounding code's naming and idiom. Unreal C++ conventions (`F`/`U`/`A`/`E` prefixes,
  `UPROPERTY`/`USTRUCT`/`UCLASS` where needed).
- No Unity/DOTS vocabulary in runtime code (`GameObject`, `MonoBehaviour`, `SubScene`, `Burst`, …) —
  migration notes only.
- When a task would violate `AGENTS.md`, surface it and propose an ADR instead of quietly diverging.
