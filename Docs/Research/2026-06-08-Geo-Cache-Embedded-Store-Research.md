<!-- Generated 2026-06-08 by a background opus research agent: embedded-store evaluation for the Geo tile cache + the new decoded-geometry tier. Condensed from the full agent report. -->

# Embedded-store evaluation for the Geo tile cache (redb & alternatives)

**Question.** Replace the hand-rolled `DiskCache` (`Rust/crates/geo/logi_3dtiles_io/src/lib.rs`) with an embedded store, and/or back the new **decoded-geometry tier** (M3, ~85 KB blobs keyed by `(origin, tile_key)`) with one?

**Constraints that decide it:** pure-Rust strongly preferred (one C exception already spent on Draco); `#![forbid(unsafe_code)]` in the IO crate; **no tokio** anywhere in Geo; **no-compat (ADR-0004)** → format churn handled by discard-and-rebuild, not migration; workspace on hashbrown/flume/mimalloc; this cache is **non-authoritative render data** (determinism not required). Today's pain: `open()` does a full dir-scan `O(F log F)` + ~F `stat()`s; LRU recency not persisted; NTFS small-file/MFT tax at 100k–millions of files; integrity is the just-added filename CRC.

> ⚠️ **No public benchmark exists at our exact scale** (100k–millions of ~85 KB blobs, 50–200 GB, NTFS, 32 readers). Every verdict below is reasoned from each engine's storage model, **not measured** — a `divan` spike is the only way to get real numbers. Verdicts lean **PROTOTYPE**, not adopt-on-faith.

## Comparison

| Engine | Pure Rust | mmap/unsafe | Sync (no tokio) | Built-in checksum | Avoids open-scan | 32-thread fit | NTFS small-file | Verdict |
|---|---|---|---|---|---|---|---|---|
| **redb 4.1** | ✅ | mmap **removed** (memory-safe) | ✅ | ✅ XXH3 Merkle | ✅ root read | reads lock-free / writes serialize | ✅ one file | **PROTOTYPE (byte store lead)** |
| **fjall 3.0** | ✅ | tiny | ✅ | ✅ XXH3 block+blob | ✅ manifest | ✅ lock-free clone | ✅ few big files | **PROTOTYPE (geom-tier lead)** |
| **foyer 0.22** | ✅ | — | ❌ disk path **async** | ❌ | ✅ | ✅ | ✅ | PROTOTYPE *iff* tokio allowed (ADR) |
| heed/LMDB | ❌ C | mmap | ✅ | ❌ | ✅ | reads / writes-serialize | mmap; Win map-size | CONSIDER (2nd C dep + mmap) |
| persy / sanakirja | ✅ | safe / mmap | ✅ | ❌ | ✅ | ✅ | ✅ | CONSIDER (weak, behind redb/fjall) |
| sled | ✅ | some | ✅ | partial | ✅ | ✅ | ✅ | **REJECT** (alpha; non-forward-compat format) |
| rocksdb | ❌ C++ | — | ✅ | ✅ | ✅ | ✅ | ✅ | **REJECT** (heavy C++; slowest reads in redb's bench) |
| native_db | ✅(on redb) | safe | ✅ | ✅ | ✅ | as redb | ✅ | **REJECT** (ORM/migrations we don't want) |
| moka / quick_cache | ✅ | safe | ✅/async | n/a | RAM only | ✅ | n/a | **REJECT as disk**; use `quick_cache` as RAM front tier |
| custom pack-file | ✅ | none | ✅ | DIY (crc32fast) | ✅ if index persisted | ✅ | ✅ | CONSIDER (fallback / spike to benchmark against) |

## Recommendation

**Don't keep hand-rolling. Move BOTH stores to ONE embedded engine (two tables/partitions). Primary pick: fjall 3.0; race redb 4.1 behind the same trait and let `divan` decide.**

- **fjall** — LSM + **key-value separation (value-log)** is purpose-built for immutable fat blobs (our GLBs + decoded geometry); **lock-free clone-per-thread** API is the cleanest fit for the 32-worker pool; fully **synchronous** (no tokio); **XXH3 block+blob checksums** subsume our filename CRC.
- **redb** — **memory-safe pure Rust, mmap removed** (best `#![forbid(unsafe_code)]` story); **single file** (cleanest NTFS answer); **XXH3 Merkle** checksums + crash rollback; root-read open kills the `O(F log F)` scan; fastest point reads in published numbers (reads are our dominant path).
- **foyer** is the only candidate that *is* an LRU disk cache (ships LRU/S3-FIFO + byte budget + region-packing for small entries) — it would **delete our `CacheIndex`/`evict_to`/budget code** — but its disk path is **async/tokio**, an ADR-level decision for the deliberately-tokio-free Geo IO crate.

**LRU on a KV store with no native LRU:** keep the existing `CacheIndex` (`HashMap{key→size,used,crc}` + `BTreeMap{tick→key}` + total), but (1) **persist the index** in a `meta` table so `open()` never scans data and **LRU recency survives restart** (fixes the two startup pains); (2) `get` reads value (engine's XXH3 verifies → drop the filename-CRC), lazily bumps recency; (3) `put` writes in one txn, and over budget `pop_first()` the coldest keys and `remove` them (batched) — our `evict_to` minus the `remove_file`s. Wrap a small **`quick_cache`** in-RAM LRU in front. With fjall, budget on *logical* bytes at ~70–80% of the disk ceiling (LSM transient space-amp); with redb the file sits at high-water after eviction (COW free-list) — budget logical, accept high-water or periodically compact.

**The combined move (items 1+2+8 of the audit roadmap) is delivered by adopting one engine:** persisted index + integrity + pack-file consolidation in a single decision. This is **ADR-worthy** (a vendored-dep class decision like Draco) and should be a measured spike, not a silent import. Note: this would **supersede the M1 filename-CRC + M2 tmp-reclaim** machinery we just landed (the engine owns integrity + atomic commit) — fine, they're cheap and correct in the interim.

## Sources (selected)
- redb: [design.md](https://github.com/cberner/redb/blob/master/docs/design.md) · [CHANGELOG](https://github.com/cberner/redb/blob/master/CHANGELOG.md) · [1.0 post + benchmarks](https://www.redb.org/post/2023/06/16/1-0-stable-release/) · [HN: mmap removal / memory-safety (cberner)](https://news.ycombinator.com/item?id=39323183)
- fjall: [Fjall 3.0 post](https://fjall-rs.github.io/post/fjall-3/) · [GitHub](https://github.com/fjall-rs/fjall) · [Phoronix](https://www.phoronix.com/news/Fjall-3.0-Released)
- foyer: [overview](https://foyer.rs/docs/overview) · [GitHub](https://github.com/foyer-rs/foyer)
- heed/LMDB: [docs.rs/heed](https://docs.rs/heed) · sled: [sled.rs](http://sled.rs/) · persy: [persy.rs](https://persy.rs/) · sanakirja: [crates.io](https://crates.io/crates/sanakirja) · quick_cache: [lib.rs](https://lib.rs/crates/quick_cache) · B-tree vs LSM: [TiKV deep-dive](https://tikv.org/deep-dive/key-value-engine/b-tree-vs-lsm/)
