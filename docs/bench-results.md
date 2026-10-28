# Raw benchmark output (cargo run --release --example bench -- 1000000)

Hardware: AMD Ryzen 5 7520U (4C/8T), 15.2 GB RAM, SK hynix BC901 NVMe SSD, Windows 11. Single run, not averaged.

    Finished `release` profile [optimized] target(s) in 9.86s
     Running `target\release\examples\bench.exe 1000000`
N = 1000000 keys, value = 100 B, memtable = 4 MiB, block = 4096 B, bloom = 10 bits/key, cache = 8 MiB

| Benchmark | Result |
|---|---|
| Sequential writes (fsync off) | 84510 ops/s |
| Random writes (fsync off) | 63918 ops/s |
| Write amplification, random overwrite (SSTable bytes / user bytes) | 3.56x |
| Write amplification incl. WAL | 4.70x |
| Flushes / compactions / write stalls | 31 / 42 / 11 |
| Files per level (L0..L6) | [3, 5, 29, 0, 0, 0, 0] |
| Space amplification (disk bytes / live key+value bytes) | 1.05x |
| Random reads, hits (100000/100000 found) | 12551 ops/s |
| Block cache hit ratio during hit reads | 7.2% |
| Range scan, 1000-key ranges | 295266 keys/s (200 scans) |
| Random misses, with bloom (10 bits/key) | 585390 ops/s; 868 block reads from disk/file, 99112 bloom-negatives, 882 false positives |
| Measured bloom false-positive rate (theory ~0.82% at 10 bits/key, k=6) | 0.88% |
| Random misses, without bloom | 13197 ops/s; 99394 block reads from disk/file, 0 bloom-negatives, 0 false positives |
| Sequential writes (fsync on every write, 5000 ops) | 1012 ops/s |
| Baseline: BTreeMap + append-only log, sequential writes (fsync off) | 97881 ops/s |
| Baseline: in-memory BTreeMap random reads (100000 hits) | 285926 ops/s |
