//! Global/singleton simulation state held as Bevy Resources.

use crate::terrain::Terrain;
use bevy_ecs::incremental::BucketIndex;
use bevy_ecs::prelude::*;
use logi_schema::{LogiCameraView, LogiEntityId, LogiPlayerIntent};

/// Fixed-step clock. `tick` advances exactly once per fixed step.
#[derive(Resource, Clone, Copy, Debug)]
pub struct SimClock {
    pub tick: u64,
    pub fixed_hz: u32,
}

impl Default for SimClock {
    fn default() -> Self {
        Self { tick: 0, fixed_hz: 30 }
    }
}

impl SimClock {
    #[inline]
    pub fn seconds_per_tick(&self) -> f32 {
        if self.fixed_hz > 0 {
            1.0 / self.fixed_hz as f32
        } else {
            0.0
        }
    }
}

/// SplitMix64 finalizer (the mixer half — no additive stepping). A pure,
/// dependency-free bijection from a 64-bit input to a well-distributed 64-bit
/// output; the keyed RNG below feeds it `key ^ counter`. These are the canonical
/// SplitMix64 constants (audit M23 / ADR-0016) and the ONLY sanctioned mixer.
#[inline]
fn splitmix64_mix(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// One keyed draw: `splitmix64_mix(key ⊕ (counter · GOLDEN))`. Counter-based, so
/// the `i`-th value of a stream is a *pure function of (key, counter)* with no
/// carried state — the property that makes the RNG embarrassingly parallel
/// (ADR-0016): a million `par_iter` lanes can each draw without a shared `&mut`.
/// Stirring the counter through the golden ratio before the XOR decorrelates
/// adjacent counters so consecutive draws don't share high bits.
#[inline]
fn splitmix_keyed(key: u64, counter: u64) -> u64 {
    splitmix64_mix(key ^ counter.wrapping_mul(0x9E37_79B9_7F4A_7C15))
}

/// Deterministic keyed RNG (counter-based SplitMix64), seeded from
/// `deterministic_seed` at world creation. **This is the ONLY sanctioned source of
/// randomness in the simulation** (audit M23 / ADR-0016): every stochastic system
/// MUST draw via a [`DrawKey`] minted from this resource by [`SimRng::for_entity`]
/// or [`SimRng::for_domain`]. Seeded from sim inputs only — **never** `thread_rng`,
/// `rand::random`, a wall-clock, or any per-thread/global entropy, all of which
/// break replay/snapshot determinism (§8 / ADR-0001 Determinism, in force under
/// ADR-0005).
///
/// The resource is *immutable* for the run: it carries only the seed and hands out
/// per-(entity|domain, tick) `DrawKey`s. There is no shared mutable stream — that
/// was the point of ADR-0016. A single `&mut SimRng` stream is a serialization
/// point: every drawing system would conflict on it and it can't be reached from
/// inside `par_iter`. Folding the full stable identity + tick into a key, then
/// running a counter-based PRNG off that key, gives each lane its own independent,
/// reproducible substream — concurrent AND bit-identical to any draw order.
///
/// Held in the `World` from creation but not yet consumed by any system: the
/// current sim is deterministic without randomness. The resource exists so the
/// first stochastic feature reaches for the sanctioned path instead of introducing
/// `thread_rng`. The keyed contract is locked by `tests::sim_rng_is_seed_deterministic`.
#[derive(Resource, Clone, Copy, Debug)]
pub struct SimRng {
    pub seed: u64,
}

impl SimRng {
    #[inline]
    pub fn new(seed: u64) -> Self {
        Self { seed }
    }

    /// Mint a `DrawKey` for one entity at one tick. Folds the FULL stable identity
    /// (index/generation/shard/kind — matching `systems::stable_key`, so two
    /// entities that ever share an `index` across shards/kinds get distinct
    /// substreams) plus the tick and the run seed into the key. Takes `&self`: no
    /// `&mut SimRng`, so callable from inside `par_iter`.
    #[inline]
    pub fn for_entity(&self, id: LogiEntityId, tick: u64) -> DrawKey {
        // Pack generation/shard/kind (32+16+16 bits) into one lane, then fold each
        // part through the mixer so every field contributes avalanche. `index` and
        // `tick` are full u64s and fold in directly.
        let packed = (id.generation as u64) << 32 | (id.shard as u64) << 16 | id.kind as u64;
        let mut key = splitmix64_mix(self.seed);
        key = splitmix64_mix(key ^ id.index);
        key = splitmix64_mix(key ^ packed);
        key = splitmix64_mix(key ^ tick);
        DrawKey { key, counter: 0 }
    }

    /// Mint a `DrawKey` for a non-entity domain (e.g. a global stochastic process)
    /// at one tick. Same fold as [`Self::for_entity`] over `(seed, domain, tick)`.
    #[inline]
    pub fn for_domain(&self, domain: u64, tick: u64) -> DrawKey {
        let mut key = splitmix64_mix(self.seed);
        key = splitmix64_mix(key ^ domain);
        key = splitmix64_mix(key ^ tick);
        DrawKey { key, counter: 0 }
    }
}

/// An independent, reproducible draw stream for one (entity|domain, tick), minted
/// by [`SimRng`]. `Copy`, carries no borrow — pass it by value into a `par_iter`
/// closure and draw freely; two lanes with different keys never collide. The
/// `counter` makes successive draws within the stream distinct while keeping each
/// one a pure function of `(key, counter)` (counter-based PRNG, ADR-0016).
#[derive(Clone, Copy, Debug)]
pub struct DrawKey {
    key: u64,
    counter: u64,
}

impl DrawKey {
    /// Next 64 bits of this stream: `splitmix_keyed(key, counter)`, then advance
    /// the counter. Pure in `(key, counter)`, so the sequence is fixed by the key
    /// regardless of when or on which thread it's pulled.
    #[inline]
    pub fn next_u64(&mut self) -> u64 {
        let v = splitmix_keyed(self.key, self.counter);
        self.counter = self.counter.wrapping_add(1);
        v
    }

    /// Next draw as an `f32` in `[0, 1)`. Uses the top 24 bits (the f32 mantissa
    /// width) so the result is uniform and free of low-bit bias.
    #[inline]
    pub fn next_f32_unit(&mut self) -> f32 {
        (self.next_u64() >> 40) as f32 / (1u64 << 24) as f32
    }
}

/// The terrain authority, held as a simulation Resource.
#[derive(Resource, Default)]
pub struct TerrainResource(pub Terrain);

/// Latest per-frame Unreal input, ingested at the `InputIngest` phase. Read by
/// significance/representation systems; non-authoritative.
#[derive(Resource, Default)]
pub struct UnrealFrameInput {
    pub cameras: Vec<LogiCameraView>,
    pub intents: Vec<LogiPlayerIntent>,
}

/// Per-frame representation budgets (ADR-0003). Fidelity is allocated TOP-DOWN by
/// significance against these caps — never by raw distance.
#[derive(Resource, Clone, Copy, Debug)]
pub struct RepresentationBudget {
    pub high_res_actors: u32,
    pub low_res_actors: u32,
    pub instanced_meshes: u32,
}

impl Default for RepresentationBudget {
    fn default() -> Self {
        Self {
            high_res_actors: 64,
            low_res_actors: 256,
            instanced_meshes: 100_000,
        }
    }
}

/// The persistent significance index (ADR-0020 R1 / ADR-0015 §3): the incremental,
/// bucketed top-K selector `allocate_representation_budget` ranks against, instead of
/// re-ranking the whole population every frame.
///
/// **The cell's invariants** (all maintained by `allocate_representation_budget`, its only
/// writer):
/// - **Slot = `Entity` index** (`Entity::index_u32`) — unique among live entities by
///   construction, replay-stable because spawning is deterministic. Index reuse after a
///   despawn is safe: the upsert pass re-scores the slot for its new tenant every frame.
/// - **Occupancy mirrors the live `Representation` population.** Upserts come from the
///   per-frame feed over the live query; removals are event-driven from
///   `RemovedComponents<Representation>` (which includes despawns). Drift is debug-asserted
///   in the system each frame (`occupied == population`, the `LiveSet` discipline).
/// - **Selection is quantized top-K** (ADR-0015 §3): a selected entity is never more than
///   one score quantum below a rejected one; *boundary* ties within a quantum resolve by
///   ascending slot (deterministic, replay-stable), and the selected set is then re-ranked
///   **exactly** by `(significance desc, stable_key asc)` before tier assignment — so
///   quantization decides only who crosses the budget boundary, never how the selected
///   are tiered.
#[derive(Resource)]
pub struct SignificanceIndex {
    index: BucketIndex,
}

impl Default for SignificanceIndex {
    fn default() -> Self {
        Self {
            // Grows lazily with the entity index space (`ensure_slots` in `upsert`).
            index: BucketIndex::with_capacity(0),
        }
    }
}

impl SignificanceIndex {
    /// Insert or re-score `entity`. O(1) amortized; a true no-op unless the score crossed a
    /// quantum boundary. Non-finite scores quantize to the bottom bucket (never selected
    /// above genuine scores), matching the budget system's NaN sanitisation.
    #[inline]
    pub fn upsert(&mut self, entity: Entity, score: f32) {
        let slot = entity.index_u32();
        self.index.ensure_slots(slot as usize + 1);
        self.index.upsert(slot, score);
    }

    /// Evict `entity` from selection candidacy. O(1); defensively a no-op when absent, so
    /// removal events can be forwarded unfiltered.
    #[inline]
    pub fn remove(&mut self, entity: Entity) -> bool {
        self.index.remove(entity.index_u32())
    }

    /// Select the top-`k` slots by `(score desc, slot asc)` into `out`
    /// (`O(k log k + boundary)`, independent of population). Slots map back to entities via
    /// `Entities::resolve_from_index`.
    #[inline]
    pub fn select_top_k(&mut self, k: usize, out: &mut Vec<u32>) {
        self.index.select_top_k(k, out);
    }

    /// Number of entities currently indexed. O(1). The system's drift check compares this
    /// against the live population each frame (debug builds).
    #[inline]
    pub fn occupied(&self) -> usize {
        self.index.occupied()
    }
}

#[cfg(test)]
mod tests {
    use super::SimRng;
    use logi_schema::LogiEntityId;

    fn entity(index: u64) -> LogiEntityId {
        LogiEntityId {
            index,
            generation: 1,
            shard: 0,
            kind: 7,
        }
    }

    /// M23 / ADR-0016: the sanctioned RNG is purely KEYED-deterministic. A draw is a
    /// pure function of `(seed, identity, tick, counter)` with no carried stream state
    /// and no wall-clock / thread-local entropy — so a million `par_iter` lanes draw
    /// concurrently AND reproducibly. Locks the keyed contract so the first stochastic
    /// system reaches for `SimRng::for_entity` instead of `thread_rng`.
    #[test]
    fn sim_rng_is_seed_deterministic() {
        // Draw a fixed-length stream from one entity's per-tick key.
        let draw = |seed: u64, id: LogiEntityId, tick: u64| {
            let mut k = SimRng::new(seed).for_entity(id, tick);
            [k.next_u64(), k.next_u64(), k.next_u64(), k.next_u64()]
        };

        // `for_entity` needs no `&mut SimRng` — the resource is immutable and shareable
        // (this line wouldn't compile if it took `&mut self`).
        let rng = SimRng::new(0xDEAD_BEEF);
        let _ = rng.for_entity(entity(1), 0);
        let _ = rng.for_entity(entity(2), 0); // same `&rng`, no re-borrow needed

        // Same (seed, id, tick) -> identical stream (replay-stable).
        assert_eq!(draw(0xDEAD_BEEF, entity(1), 0), draw(0xDEAD_BEEF, entity(1), 0));
        // Different seed / id / tick each diverge the stream.
        assert_ne!(draw(1, entity(1), 0), draw(2, entity(1), 0), "seed splits streams");
        assert_ne!(draw(7, entity(1), 0), draw(7, entity(2), 0), "id splits streams");
        assert_ne!(draw(7, entity(1), 0), draw(7, entity(1), 1), "tick splits streams");

        // The full identity matters, not just `index`: differing generation/shard/kind
        // each give distinct substreams (no index-only collisions).
        let base = LogiEntityId {
            index: 5,
            generation: 1,
            shard: 2,
            kind: 3,
        };
        let draw_id = |id: LogiEntityId| {
            let mut k = SimRng::new(99).for_entity(id, 0);
            [k.next_u64(), k.next_u64()]
        };
        assert_ne!(draw_id(base), draw_id(LogiEntityId { generation: 2, ..base }));
        assert_ne!(draw_id(base), draw_id(LogiEntityId { shard: 9, ..base }));
        assert_ne!(draw_id(base), draw_id(LogiEntityId { kind: 9, ..base }));

        // Within a stream the counter advances: consecutive draws differ. Each draw is a
        // pure function of (key, counter), so the n-th value is fixed by the key alone —
        // two independently-minted keys for the same (seed, id, tick) produce the SAME
        // sequence (the "order-independent" / lane-local-substream property of ADR-0016).
        let mut a = SimRng::new(42).for_entity(entity(3), 11);
        let a_seq: [u64; 4] = [a.next_u64(), a.next_u64(), a.next_u64(), a.next_u64()];
        assert_ne!(a_seq[0], a_seq[1], "consecutive draws advance the counter");
        let mut b = SimRng::new(42).for_entity(entity(3), 11);
        let b_seq: [u64; 4] = [b.next_u64(), b.next_u64(), b.next_u64(), b.next_u64()];
        assert_eq!(a_seq, b_seq, "same-key streams draw an identical sequence");

        // `for_domain` is keyed the same way: reproducible, and domain/tick split it.
        let dom = |d: u64, t: u64| SimRng::new(5).for_domain(d, t).next_u64();
        assert_eq!(dom(1, 0), dom(1, 0));
        assert_ne!(dom(1, 0), dom(2, 0));
        assert_ne!(dom(1, 0), dom(1, 1));

        // `next_f32_unit` stays in [0, 1).
        let u = SimRng::new(1).for_entity(entity(1), 0).next_f32_unit();
        assert!((0.0..1.0).contains(&u), "f32 draw in [0,1): {u}");
    }
}
