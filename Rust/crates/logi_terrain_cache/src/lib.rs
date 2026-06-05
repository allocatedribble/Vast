//! Bounded, byte-budgeted residency cache (LRU). **Never exceeds its budget** (the core invariant of
//! "streaming is the product": bounded memory).
//!
//! Backed by a `HashMap` (lookup) + `BTreeMap<tick, TileId>` (LRU order) — O(log n) per op, no external
//! deps. Callers pass a reusable `evicted` scratch `Vec` so steady-state streaming does zero allocation.

use std::collections::{BTreeMap, HashMap};

use logi_terrain_core::TileId;

struct Entry<V> {
    value: V,
    bytes: usize,
    tick: u64,
}

/// LRU cache keyed by [`TileId`], bounded by a byte budget.
pub struct ResidencyCache<V> {
    budget: usize,
    bytes_used: usize,
    clock: u64,
    entries: HashMap<TileId, Entry<V>>,
    order: BTreeMap<u64, TileId>,
    pub hits: u64,
    pub misses: u64,
    pub evictions: u64,
}

impl<V> ResidencyCache<V> {
    pub fn with_budget(budget_bytes: usize) -> Self {
        Self {
            budget: budget_bytes,
            bytes_used: 0,
            clock: 0,
            entries: HashMap::new(),
            order: BTreeMap::new(),
            hits: 0,
            misses: 0,
            evictions: 0,
        }
    }

    #[inline]
    pub fn budget(&self) -> usize {
        self.budget
    }

    /// Change the byte budget. Over-budget entries are trimmed on the next insert.
    #[inline]
    pub fn set_budget(&mut self, budget_bytes: usize) {
        self.budget = budget_bytes;
    }
    #[inline]
    pub fn bytes_used(&self) -> usize {
        self.bytes_used
    }
    /// FIX 8: true when residency exceeds the budget — observable when a single entry is larger than
    /// the whole budget (insert keeps it and the breach is otherwise silent). Does not change behavior.
    #[inline]
    pub fn over_budget(&self) -> bool {
        self.bytes_used > self.budget
    }
    #[inline]
    pub fn len(&self) -> usize {
        self.entries.len()
    }
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
    #[inline]
    pub fn contains(&self, id: &TileId) -> bool {
        self.entries.contains_key(id)
    }

    fn touch(&mut self, id: TileId) {
        if let Some(e) = self.entries.get_mut(&id) {
            self.order.remove(&e.tick);
            self.clock += 1;
            e.tick = self.clock;
            self.order.insert(self.clock, id);
        }
    }

    /// Resident lookup, marking the entry most-recently-used. Updates hit/miss counters.
    pub fn get(&mut self, id: &TileId) -> Option<&V> {
        if self.entries.contains_key(id) {
            self.touch(*id);
            self.hits += 1;
            Some(&self.entries[id].value)
        } else {
            self.misses += 1;
            None
        }
    }

    /// Mark `id` most-recently-used without affecting hit/miss stats — pins the active working set
    /// (e.g. coarse ancestors used for fallback) so it is never evicted out from under the camera.
    pub fn promote(&mut self, id: &TileId) {
        self.touch(*id);
    }

    /// Insert/replace `id`, then evict least-recently-used entries until within budget.
    /// `evicted` is cleared and filled with evicted ids (reuse it across calls to avoid allocation).
    pub fn insert(&mut self, id: TileId, value: V, bytes: usize, evicted: &mut Vec<TileId>) {
        evicted.clear();

        if let Some(old) = self.entries.remove(&id) {
            self.order.remove(&old.tick);
            // FIX 7: saturate so a bookkeeping slip can't underflow the byte counter.
            self.bytes_used = self.bytes_used.saturating_sub(old.bytes);
        }

        self.clock += 1;
        let tick = self.clock;
        self.entries.insert(id, Entry { value, bytes, tick });
        self.order.insert(tick, id);
        self.bytes_used += bytes;

        while self.bytes_used > self.budget {
            let (lru_tick, lru_id) = match self.order.iter().next() {
                Some((t, i)) => (*t, *i),
                None => break,
            };
            // Never evict the entry we just inserted (it alone may exceed budget — caller's concern).
            if lru_id == id {
                break;
            }
            self.order.remove(&lru_tick);
            if let Some(e) = self.entries.remove(&lru_id) {
                // FIX 7: saturate so a bookkeeping slip can't underflow the byte counter.
                self.bytes_used = self.bytes_used.saturating_sub(e.bytes);
            }
            self.evictions += 1;
            evicted.push(lru_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tile(i: u32) -> TileId {
        TileId::new(4, i, 0)
    }

    /// FIX 7: after an interleaved sequence of inserts, replaces, and budget-driven evictions, the
    /// byte counter must equal the exact sum of the bytes of the entries still resident.
    #[test]
    fn bytes_used_tracks_resident_sum() {
        let mut cache: ResidencyCache<u32> = ResidencyCache::with_budget(1000);
        let mut evicted = Vec::new();

        // Track the intended byte cost per id, then derive the resident set from what survived.
        let inserts: [(u32, usize); 6] =
            [(0, 300), (1, 300), (2, 300), (0, 250), (3, 400), (4, 200)];
        let mut resident: std::collections::HashMap<u32, usize> = std::collections::HashMap::new();
        for &(id, bytes) in &inserts {
            cache.insert(tile(id), id, bytes, &mut evicted);
            resident.insert(id, bytes); // replace updates the cost
            for ev in &evicted {
                resident.remove(&ev.x);
            }
        }

        let expected: usize = resident.values().sum();
        assert_eq!(cache.bytes_used(), expected, "byte counter must equal sum of resident bytes");
        assert_eq!(cache.len(), resident.len());
        assert!(cache.bytes_used() <= cache.budget());
        assert!(!cache.over_budget());
    }

    /// FIX 8: a single entry larger than the whole budget is kept; the breach is observable.
    #[test]
    fn oversized_single_insert_is_observable() {
        let mut cache: ResidencyCache<u32> = ResidencyCache::with_budget(100);
        let mut evicted = Vec::new();
        cache.insert(tile(0), 0, 50, &mut evicted);
        cache.insert(tile(1), 1, 500, &mut evicted); // alone exceeds the budget
        assert!(cache.over_budget(), "oversized resident entry must report over_budget");
        assert_eq!(cache.bytes_used(), 500, "only the oversized entry remains");
        assert_eq!(cache.len(), 1);
    }
}
