//! Incremental indices — the first primitive of the **incremental World** program (`logi_ecs` fork
//! divergence D-incremental; see `FORK.md` and ADR-0020).
//!
//! The program, in one sentence: the `World` is a **cell** (ADR-0019) — bounded by budget, its
//! interior maintained **incrementally** (`O(Δ)` per step, where Δ is what actually changed), and
//! coupled to its siblings only through narrow, explicit links — so per-step cost decouples from
//! population. A flat per-step sweep over everything the World holds is the shape this module
//! exists to kill: a consumer that re-reads every row to answer "which K matter most?" puts the
//! whole population on the critical path every step (ADR-0015's named wall).
//!
//! [`BucketIndex`] is the first such structure: a bucketed top-K selector over dense slots whose
//! maintenance cost is `O(Δ)` (a slot is touched only when its score crosses a quantum boundary)
//! and whose selection cost is `O(K log K + boundary-bucket)` — independent of the population
//! pinned in the bottom bucket. It was proven as the Exp-D spike (`logi_sim::sig_index`,
//! 2026-06-09) and moved here as fork divergence work, with the boundary-bucket pathology fixed
//! (the spike cloned and fully sorted every contributing bucket — `O(P log P)` the moment the
//! descent reached the population-holding bottom bucket).

use alloc::vec::Vec;

/// Scores are in `[0, 1]`; quantize into this many buckets. 256 keeps the near-top buckets (where
/// `k` lives) small — cheap to sort at the budget boundary — while the far field collapses into
/// bucket 0.
pub const NUM_BUCKETS: usize = 256;

const ABSENT: u16 = u16::MAX;

/// Deterministic, integer-stable quantization of a score in `[0, 1]` to its bucket.
///
/// Round-half-up via `+ 0.5` and a truncating cast (identical on every platform; `f32::round`
/// itself is unavailable in `no_std`). Non-finite scores quantize to bucket 0, so a NaN can never
/// perturb the selection order.
#[inline]
pub fn bucket_of(score: f32) -> u16 {
    let s = if score.is_finite() { score.clamp(0.0, 1.0) } else { 0.0 };
    (s * (NUM_BUCKETS as f32 - 1.0) + 0.5) as u16
}

/// An incremental bucketed top-K index over dense slots `0..n`.
///
/// Slots are bucketed by quantized score and the buckets are maintained **incrementally**: an
/// [`upsert`](Self::upsert) is a true no-op unless the slot's score crossed a quantum boundary, so
/// a step's maintenance cost is `O(Δ)` — bounded by how much the world *changed*, never by how
/// much it *holds*. [`select_top_k`](Self::select_top_k) then descends from the highest non-empty
/// bucket and stops once `k` is met: the deep tail (bucket 0, holding ~all of the population) is
/// never iterated.
///
/// This is a *quantized* top-K (a selected slot is never more than one quantum below a rejected
/// one) — exactly ADR-0015's band-selection contract; it is intentionally not bit-identical to an
/// exact ranking. Determinism: bucketing is an integer quantile of a (deterministic) score, and
/// ties within a bucket break by ascending slot — a total, replay-stable order.
pub struct BucketIndex {
    /// `buckets[b]` = the slots whose score currently quantizes to `b`.
    buckets: Vec<Vec<u32>>,
    /// `cur_bucket[slot]` = the slot's current bucket (`ABSENT` until first `upsert`).
    cur_bucket: Vec<u16>,
    /// `pos[slot]` = the slot's index within `buckets[cur_bucket[slot]]`, for O(1) swap-removal.
    pos: Vec<u32>,
    /// Highest non-empty bucket, or `-1` if empty. Lazily corrected downward in `select_top_k`.
    nonempty_hi: i32,
    /// Reused boundary-bucket scratch for `select_top_k` (no per-call allocation once warm).
    scratch: Vec<u32>,
    /// Number of slots currently present (upserted and not removed). O(1) to read; the
    /// consumer's drift check (`occupied == live population`) rides on it.
    occupied: usize,
}

impl BucketIndex {
    /// An index sized for `n` dense slots. Allocates the bucket vectors + the per-slot maps once;
    /// `upsert` is allocation-free thereafter (amortized) on the hot path.
    pub fn with_capacity(n: usize) -> Self {
        Self {
            buckets: (0..NUM_BUCKETS).map(|_| Vec::new()).collect(),
            cur_bucket: alloc::vec![ABSENT; n],
            pos: alloc::vec![0; n],
            nonempty_hi: -1,
            scratch: Vec::new(),
            occupied: 0,
        }
    }

    /// Grow the slot space to at least `n` (new slots start absent). Amortized O(growth);
    /// a no-op when already large enough. Lets a consumer whose slot ids are allocated
    /// externally (e.g. ECS entity indices) extend the index lazily instead of sizing it
    /// up front.
    pub fn ensure_slots(&mut self, n: usize) {
        if n > self.cur_bucket.len() {
            self.cur_bucket.resize(n, ABSENT);
            self.pos.resize(n, 0);
        }
    }

    /// Number of slots currently present (upserted and not removed). O(1).
    #[inline]
    pub fn occupied(&self) -> usize {
        self.occupied
    }

    /// Number of dense slots the index is sized for.
    #[inline]
    pub fn slots(&self) -> usize {
        self.cur_bucket.len()
    }

    /// The bucket `slot` currently lives in, or `None` if it has never been upserted.
    #[inline]
    pub fn bucket_of_slot(&self, slot: u32) -> Option<u16> {
        let b = self.cur_bucket[slot as usize];
        (b != ABSENT).then_some(b)
    }

    /// Insert or move `slot` to the bucket for `score`. **O(1) amortized**, and a true **no-op
    /// when the bucket is unchanged** — the common case, since most slots don't cross a quantum
    /// boundary in a step. That no-op is what makes a step `O(Δ)` rather than `O(P)`.
    pub fn upsert(&mut self, slot: u32, score: f32) {
        let nb = bucket_of(score);
        let ob = self.cur_bucket[slot as usize];
        if ob == nb {
            return;
        }
        if ob != ABSENT {
            self.remove_from(ob, slot);
        } else {
            self.occupied += 1;
        }
        let v = &mut self.buckets[nb as usize];
        self.pos[slot as usize] = v.len() as u32;
        v.push(slot);
        self.cur_bucket[slot as usize] = nb;
        if nb as i32 > self.nonempty_hi {
            self.nonempty_hi = nb as i32;
        }
    }

    /// Remove `slot` from the index entirely (it stops being a selection candidate). O(1).
    /// Defensive on purpose: a slot that is out of range or already absent is a no-op, so an
    /// event-driven caller can forward removal events without pre-filtering (the event may
    /// concern a slot that was never upserted). Returns whether the slot was present.
    pub fn remove(&mut self, slot: u32) -> bool {
        let Some(&b) = self.cur_bucket.get(slot as usize) else {
            return false;
        };
        if b == ABSENT {
            return false;
        }
        self.remove_from(b, slot);
        self.cur_bucket[slot as usize] = ABSENT;
        self.occupied -= 1;
        true
    }

    /// Swap-remove `slot` from bucket `b`, keeping the moved tenant's `pos` correct. O(1).
    #[inline]
    fn remove_from(&mut self, b: u16, slot: u32) {
        let v = &mut self.buckets[b as usize];
        let i = self.pos[slot as usize] as usize;
        let last = v.len() - 1;
        v.swap(i, last);
        if i != last {
            let moved = v[i];
            self.pos[moved as usize] = i as u32;
        }
        v.pop();
        // `nonempty_hi` is corrected lazily in `select_top_k` (a removal that empties the top
        // bucket is rare and the watermark is only read there).
    }

    /// Select the top-`k` slots by `(score desc, slot asc)` into `out`.
    ///
    /// Descends from the highest non-empty bucket: every bucket above the boundary contributes
    /// **whole** (their sizes sum to `< k`, so sorting them is within `O(k log k)`); the one
    /// **boundary bucket** — which may be the population-holding bottom bucket — contributes only
    /// its `remaining` smallest slots via a partial select, `O(b + r log r)`, never a full sort.
    /// Total: `O(k log k + boundary + buckets_scanned)` — independent of how many slots sit below
    /// the boundary. The fork fix over the Exp-D spike, which cloned + fully sorted every
    /// contributing bucket (`O(P log P)` whenever the descent reached bucket 0).
    pub fn select_top_k(&mut self, k: usize, out: &mut Vec<u32>) {
        out.clear();
        if k == 0 {
            return;
        }
        // Correct the lazy watermark past any now-empty top buckets.
        while self.nonempty_hi >= 0 && self.buckets[self.nonempty_hi as usize].is_empty() {
            self.nonempty_hi -= 1;
        }
        let mut b = self.nonempty_hi;
        while b >= 0 && out.len() < k {
            let bucket = &self.buckets[b as usize];
            let remaining = k - out.len();
            if !bucket.is_empty() {
                if bucket.len() <= remaining {
                    // Whole bucket contributes; sort its run in place for the deterministic
                    // (score desc, slot asc) output order.
                    let start = out.len();
                    out.extend_from_slice(bucket);
                    out[start..].sort_unstable();
                } else {
                    // Boundary bucket: partial-select the `remaining` smallest slots, sort only
                    // those. The rest of the bucket — potentially ~the whole population — is
                    // moved past, never ordered.
                    self.scratch.clear();
                    self.scratch.extend_from_slice(bucket);
                    self.scratch.select_nth_unstable(remaining - 1);
                    let taken = &mut self.scratch[..remaining];
                    taken.sort_unstable();
                    out.extend_from_slice(taken);
                }
            }
            b -= 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `SplitMix64` finalizer — a deterministic, dependency-free pseudo-random score generator for
    /// the tests (mirrors the project's sanctioned counter-based mixer; test scaffolding only).
    #[inline]
    fn score_of(slot: u32, frame: u32) -> f32 {
        let mut z = (slot as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ ((frame as u64) << 32);
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        // Bias toward the far field (most slots low score), like a real planet: square it.
        let u = (z >> 40) as f32 / (1u64 << 24) as f32; // [0,1)
        u * u
    }

    /// Correctness: the index selects a valid *quantized* top-K (no rejected slot's bucket exceeds
    /// any selected slot's bucket), exactly `k` slots, deterministically.
    #[test]
    fn selects_a_valid_quantized_top_k() {
        let n = 5_000usize;
        let k = 200usize;
        let scores: Vec<f32> = (0..n).map(|i| score_of(i as u32, 0)).collect();
        let mut idx = BucketIndex::with_capacity(n);
        for (slot, &s) in scores.iter().enumerate() {
            idx.upsert(slot as u32, s);
        }
        let mut out = Vec::new();
        idx.select_top_k(k, &mut out);
        assert_eq!(out.len(), k, "selects exactly k");

        // Determinism: same inputs -> same output.
        let mut out2 = Vec::new();
        idx.select_top_k(k, &mut out2);
        assert_eq!(out, out2, "selection is deterministic");

        // Quantized-top-K property: no rejected slot outranks a selected one by more than the
        // quantization.
        let selected: alloc::collections::BTreeSet<u32> = out.iter().copied().collect();
        let min_selected_bucket = out.iter().map(|&s| bucket_of(scores[s as usize])).min().unwrap();
        let max_rejected_bucket = (0..n as u32)
            .filter(|s| !selected.contains(s))
            .map(|s| bucket_of(scores[s as usize]))
            .max()
            .unwrap_or(0);
        assert!(
            max_rejected_bucket <= min_selected_bucket,
            "a rejected slot outranked a selected one by more than the quantization \
             (rejected hi={max_rejected_bucket}, selected lo={min_selected_bucket})"
        );
    }

    /// Incremental upsert tracks the buckets correctly under repeated score changes (the O(Δ)
    /// path).
    #[test]
    fn upsert_is_consistent_after_churn() {
        let n = 2_000usize;
        let mut idx = BucketIndex::with_capacity(n);
        let mut scores = alloc::vec![0.0f32; n];
        for (slot, s) in scores.iter_mut().enumerate() {
            *s = score_of(slot as u32, 0);
            idx.upsert(slot as u32, *s);
        }
        // 50 frames of churn: 200 slots change score each frame.
        for frame in 1..=50u32 {
            for d in 0..200u32 {
                let slot = (frame.wrapping_mul(2_654_435_761).wrapping_add(d * 7) % n as u32) as usize;
                scores[slot] = score_of(slot as u32, frame);
                idx.upsert(slot as u32, scores[slot]);
            }
        }
        // Every slot's stored bucket matches its current score; membership is consistent.
        for slot in 0..n as u32 {
            assert_eq!(idx.bucket_of_slot(slot), Some(bucket_of(scores[slot as usize])));
        }
        let total: usize = idx.buckets.iter().map(Vec::len).sum();
        assert_eq!(total, n, "every slot lives in exactly one bucket");
    }

    /// `ensure_slots` + `remove` + `occupied`: the index grows lazily, removal evicts a slot
    /// from candidacy entirely (defensively a no-op for absent/out-of-range slots, so an
    /// event-driven caller can forward removal events unfiltered), and `occupied` tracks the
    /// live count O(1) through upsert/remove/re-upsert churn (the entity-index-reuse case).
    #[test]
    fn grows_removes_and_counts_occupancy() {
        let mut idx = BucketIndex::with_capacity(0);
        assert_eq!(idx.occupied(), 0);
        assert!(!idx.remove(3), "removing from an empty index is a defensive no-op");

        idx.ensure_slots(4);
        idx.upsert(0, 0.9);
        idx.upsert(3, 0.5);
        assert_eq!(idx.occupied(), 2);

        // Lazy growth keeps existing entries intact.
        idx.ensure_slots(100);
        idx.upsert(42, 0.7);
        assert_eq!(idx.occupied(), 3);

        let mut out = Vec::new();
        idx.select_top_k(3, &mut out);
        assert_eq!(out, [0, 42, 3], "(score desc, slot asc)");

        // Removal evicts from candidacy; double-remove and out-of-range are no-ops.
        assert!(idx.remove(42));
        assert!(!idx.remove(42));
        assert!(!idx.remove(9_999));
        assert_eq!(idx.occupied(), 2);
        idx.select_top_k(3, &mut out);
        assert_eq!(out, [0, 3], "a removed slot is no longer selectable");

        // Re-upsert after removal re-enters cleanly.
        idx.upsert(42, 0.95);
        assert_eq!(idx.occupied(), 3);
        idx.select_top_k(1, &mut out);
        assert_eq!(out, [42]);

        // Bucket membership stays consistent with the O(1) count.
        let total: usize = idx.buckets.iter().map(Vec::len).sum();
        assert_eq!(total, idx.occupied());
    }

    /// The boundary-bucket fix: when `k` exceeds the above-zero population, the descent reaches
    /// the population-holding bucket 0 and must still produce the correct selection — the high
    /// slots first, then the `remaining` *smallest* bucket-0 slots — via the partial select (the
    /// spike fully sorted bucket 0 here).
    #[test]
    fn boundary_bucket_selects_smallest_slots_without_full_sort() {
        let n = 10_000usize;
        let mut idx = BucketIndex::with_capacity(n);
        // Slots 100 and 7 are the only above-zero scores; everyone else pins bucket 0.
        for slot in 0..n as u32 {
            idx.upsert(slot, 0.0);
        }
        idx.upsert(100, 0.9);
        idx.upsert(7, 0.5);

        let k = 10;
        let mut out = Vec::new();
        idx.select_top_k(k, &mut out);
        assert_eq!(out.len(), k);
        // Score-desc first: 0.9 then 0.5.
        assert_eq!(&out[..2], &[100, 7]);
        // Then the 8 smallest bucket-0 slots, ascending — slots 7 and 100 live in the upper
        // buckets, so bucket 0's smallest are 0,1,2,3,4,5,6,8.
        assert_eq!(&out[2..], &[0, 1, 2, 3, 4, 5, 6, 8]);
    }

    /// Faithful reimplementation of a full-rescan ranking core (the consumer-side `O(P)` scan +
    /// `select_nth_unstable` top-K the index replaces — `logi_sim`'s
    /// `allocate_representation_budget`). Kept as the bench baseline; the comparator mirrors the
    /// production (score desc, key asc) tie-break shape over bare slots.
    #[cfg(feature = "std")]
    fn rank_topk_full(scores: &[f32], k: usize, scratch: &mut Vec<(f32, u32)>, out: &mut Vec<u32>) {
        use core::cmp::Ordering;
        scratch.clear();
        for (i, &s) in scores.iter().enumerate() {
            scratch.push((if s.is_finite() { s } else { 0.0 }, i as u32));
        }
        let cmp = |a: &(f32, u32), b: &(f32, u32)| b.0.partial_cmp(&a.0).unwrap_or(Ordering::Equal).then(a.1.cmp(&b.1));
        let k = k.min(scratch.len());
        if k > 0 {
            scratch.select_nth_unstable_by(k - 1, cmp);
            scratch[..k].sort_unstable_by(cmp);
        }
        out.clear();
        out.extend(scratch[..k].iter().map(|e| e.1));
    }

    /// **Exp D — the thesis-prover (ported from the `logi_sim` spike).** Per-frame band-selection
    /// cost must DECOUPLE from population `P`: the O(P) full rescan grows linearly with `N`; the
    /// incremental index (a fixed Δ of score changes + an O(k) top-K) stays ~flat. Run with
    /// `--nocapture` to see the table.
    #[cfg(feature = "std")]
    #[test]
    #[expect(
        clippy::print_stderr,
        reason = "benchmark table is the test's output, read via --nocapture"
    )]
    fn bench_decouples_from_population() {
        use std::time::Instant;

        const K: usize = 4_096; // a realistic active-set cap (≪ N)
        const DELTA: usize = 2_000; // slots crossing a quantum per frame (bounded by change, not N)
        const FRAMES: usize = 40;
        const WARMUP: usize = 5;

        std::eprintln!("\n=== Exp D: band-selection, per-frame cost vs population ===");
        std::eprintln!("  K (top-K) = {K}, Δ (changes/frame) = {DELTA}, frames = {FRAMES}");
        std::eprintln!(
            "  {:>12} | {:>16} | {:>17} | {:>8}",
            "N",
            "O(P) scan (µs)",
            "O(K+Δ) index (µs)",
            "speedup"
        );

        for &n in &[10_000usize, 100_000, 1_000_000] {
            if K >= n {
                continue;
            }
            let mut scores: Vec<f32> = (0..n).map(|i| score_of(i as u32, 0)).collect();
            let mut idx = BucketIndex::with_capacity(n);
            for (slot, &s) in scores.iter().enumerate() {
                idx.upsert(slot as u32, s);
            }

            let mut scratch: Vec<(f32, u32)> = Vec::with_capacity(n);
            let mut out_full: Vec<u32> = Vec::with_capacity(K);
            let mut out_idx: Vec<u32> = Vec::with_capacity(K);

            let mut full_us = Vec::with_capacity(FRAMES);
            let mut idx_us = Vec::with_capacity(FRAMES);
            for frame in 1..=(FRAMES + WARMUP) as u32 {
                // Apply the same Δ changes to BOTH structures.
                let mut changes: Vec<(u32, f32)> = Vec::with_capacity(DELTA);
                for d in 0..DELTA as u32 {
                    let slot = (frame.wrapping_mul(2_654_435_761).wrapping_add(d * 7) % n as u32) as usize;
                    let s = score_of(slot as u32, frame);
                    scores[slot] = s;
                    changes.push((slot as u32, s));
                }

                // O(P): full rescan + select_nth (reads ALL N scores).
                let t0 = Instant::now();
                rank_topk_full(&scores, K, &mut scratch, &mut out_full);
                let dt_full = t0.elapsed();

                // O(K+Δ): apply just the Δ changed slots, then top-K descent.
                let t1 = Instant::now();
                for &(slot, s) in &changes {
                    idx.upsert(slot, s);
                }
                idx.select_top_k(K, &mut out_idx);
                let dt_idx = t1.elapsed();

                if frame as usize > WARMUP {
                    full_us.push(dt_full.as_secs_f64() * 1e6);
                    idx_us.push(dt_idx.as_secs_f64() * 1e6);
                }
            }
            let median = |v: &mut Vec<f64>| {
                v.sort_by(|a, b| a.partial_cmp(b).unwrap());
                v[v.len() / 2]
            };
            let f = median(&mut full_us);
            let i = median(&mut idx_us);
            std::eprintln!("  {n:>12} | {f:>16.1} | {i:>17.1} | {:>7.1}x", f / i);
            assert_eq!(out_full.len(), K);
            assert_eq!(out_idx.len(), K);
        }
        std::eprintln!("  → O(P) grows ~linearly with N; O(K+Δ) stays ~flat ⇒ selection decouples from population.\n");
    }
}
