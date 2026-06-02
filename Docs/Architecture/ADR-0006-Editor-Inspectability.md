# ADR-0006 — Editor inspectability for Rust-owned worlds

- **Status:** Accepted (2026-06-02)
- **Relates to:** [ADR-0005](ADR-0005-Bevy-ECS-Authority.md) (Bevy ECS authority), [ADR-0003](ADR-0003-Representation-LOD.md)
- **Supersedes:** nothing (additive)

## Context

ADR-0005 makes Rust Bevy ECS the owner of runtime gameplay truth; Unreal is the
renderer / host. But a Rust-owned `World` is opaque to the Unreal editor — there
is no Outliner row, no Details panel, no selection for a Bevy entity or a Rust
terrain tile. Developers need to *see and poke* the simulation in-editor without:

- re-introducing per-entity `UObject`/`AActor` shells (would violate ADR-0005 and
  destroy the scaling story — millions of entities),
- adding any cost to packaged (Shipping/Test) builds,
- letting the editor mutate mirror state and pretend the Bevy entity changed.

## Decision

Editor-only Unreal tooling **may** create transient `UObject`s, Slate widgets,
tree items, and selection/debug proxies that **mirror** Rust-owned entities and
terrain tiles. These are **inspection shells only** — they own no gameplay or
terrain truth and are **excluded from packaged runtime builds**.

Rules that make this safe:

1. **Not a per-entity UObject model.** The Outliner shows *virtual rows* keyed by
   `LogiEntityId` / `LogiTerrainTileId`. The Details panel binds **one transient
   selection proxy** `UObject`, never one proxy per entity. 100k entities ⇒ 0
   per-entity UObjects.
2. **Editor-only introspection ABI, isolated from the hot ABI.** Introspection
   exports live behind a Rust `editor_inspector` cargo feature and a separate
   header `logi_bevy_editor_ffi.h`; the C++ compiles them only under `WITH_EDITOR`.
   The runtime hot ABI (`logi_bevy_ffi.h`) stays clean.
3. **Paged + sampled, never per-row.** All queries are paged snapshot packets with
   explicit release. No per-row FFI from Slate paint. Default inspector refresh is
   5–10 Hz, not every frame.
4. **Edits are commands, not memory pokes.** Component edits route through an
   explicit Rust command (`logi_editor_apply_component_patch`) validated against a
   Rust-generated, stable component schema — never a direct write into Bevy storage
   and never a write to the mirror proxy.

## Consequences

- The editor gets a first-class Logi Outliner + Details experience over a Rust
  world, with virtualized rows and a single selection proxy.
- **Zero shipping cost:** no `logi_editor_*` symbols and no editor Slate code in
  Shipping/Test. Enforced by CI:
  - Shipping build contains no `logi_editor_*` symbols.
  - `LogiBevyEditor` is not loaded outside the editor.
  - With no Logi panel open, zero editor-introspection FFI calls occur.
  - The Outliner pages 100k entities without allocating 100k UObjects.
- Introspection is sampling-based, so it never perturbs determinism or the fixed
  step; it observes published snapshots, it does not drive the sim.

This ADR governs Milestone B and the terrain editor work in Milestone D+.
