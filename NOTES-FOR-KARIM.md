# Notes for Karim: lsm-tree-storage-engine (Project 16)

## Resume bullets (numbers measured on my Ryzen 5 7520U / NVMe / Windows 11)

- Built a LevelDB-style **LSM-tree key-value store in zero-dependency Rust** (WAL, memtable, SSTables, bloom filters, MANIFEST, leveled background compaction, LRU block cache); sustains **~84k sequential / ~64k random writes/s** (fsync off) over 1M keys, within ~14% of an in-memory BTreeMap + append-log baseline.
- Implemented **bloom filters** (double hashing, 10 bits/key) that cut absent-key lookups from **13k to 585k ops/s (44x)** and disk block reads by 99%, with a measured **0.88% false-positive rate vs 0.82% theoretical**.
- Verified correctness with a **model-based randomized test** (12k ops vs a BTreeMap across close/reopen) and a **crash-recovery test** that truncates the WAL at random bytes and proves recovery equals a prefix of acknowledged writes; **22 tests**, clippy-clean, CI on Linux and Windows.

## Likely interview questions and pointers

1. **LSM tree vs B-tree?** LSM: sequential writes, higher write/read amplification, great for write-heavy loads; B-tree: in-place updates, 1 read path, worse for random writes. Point to `db.rs` write path + the benchmark table.
2. **Leveled vs size-tiered compaction?** Leveled: one sorted run per level, low read/space amp, higher write amp (3.56x measured). Tiered: lower write amp, more runs to check, more space. Mention it's on the Roadmap.
3. **Why bloom filters and how big is the false-positive rate?** Skip files that cannot contain the key. FP = (1 - e^(-kn/m))^k; with 10 bits/key and k=6 that is ~0.82%, I measured 0.88% (`bloom.rs`, `bench`).
4. **How do you guarantee durability and crash safety?** WAL first, CRC-framed, fsync optional; replay stops at first bad record; MANIFEST replaced by atomic rename; files deleted only after the new MANIFEST; orphan files cleaned at open. Tests: `crash_recovery_...`.
5. **How does concurrency work / what does RocksDB do differently?** One mutex for the small state, immutable `Arc<Version>` snapshots for readers, a single background thread. RocksDB: lock-free skip list memtable, group commit with a write queue, multiple compaction threads, MVCC via sequence numbers, column families.

Bonus: *Why can't you drop a tombstone anywhere?* Because an older value in a deeper level would reappear (`pick_compaction` checks deeper levels for overlap).

## Understand this before interviews

- **`db.rs::write`**: lock, wait if stalled, append WAL, insert memtable, rotate when full. `rotate` creates the new WAL *before* parking the old memtable as `imm`, and records `imm_wal` so it can be deleted after the flush lands. `log_number = wal_id` after a flush says "every WAL older than the active one is safe to ignore".
- **`db.rs::background`**: single worker, flush before compaction. Heavy work (building files, merging) happens *outside* the lock; the lock is only retaken to swap the `Version`, save the MANIFEST and mark inputs obsolete.
- **`pick_compaction`**: L0 compacts as a group (files overlap), deeper levels one file at a time (round-robin pointer). `drop_tombstones` is true only if levels deeper than the output level have no overlap with the key range.
- **`merge.rs`**: a min-heap ordered by (key, source index). Sources are ordered newest-first, so when the same key appears in several sources the first popped is the newest and the rest are discarded. For L0 the files are reversed (higher id = newer).
- **Reader/Drop**: `SstReader` holds an `obsolete` flag; compaction sets it, the file is unlinked only when the last `Arc` dies (so in-flight reads stay valid). The handle is closed first because Windows cannot delete open files.
- **`wal.rs::read_wal`**: the invariant "a record is valid only if length fits and CRC matches" is what makes truncation at any byte safe.
- **Honest limits to state yourself**: no snapshots/MVCC, one version per key, `scan` materialises a Vec, no compression, not benchmarked against RocksDB.
