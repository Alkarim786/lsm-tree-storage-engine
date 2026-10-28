//! Benchmark harness. Run with `cargo run --release --example bench [-- N]`.
//! Prints a Markdown table of measured results; nothing here is estimated.

use lsm_engine::{Db, Options};
use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::time::Instant;

const VALUE_LEN: usize = 100;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
}

fn key(i: u64) -> Vec<u8> {
    format!("user{i:012}").into_bytes()
}

fn tmp(name: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("lsm-bench-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&p);
    p
}

fn ops_per_sec(n: u64, secs: f64) -> String {
    format!("{:.0}", n as f64 / secs)
}

fn dir_size(p: &PathBuf) -> u64 {
    fs::read_dir(p)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter_map(|e| e.metadata().ok())
        .map(|m| m.len())
        .sum()
}

fn main() {
    let n: u64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(200_000);
    let value = vec![b'v'; VALUE_LEN];
    let opts = Options::default();
    println!("N = {n} keys, value = {VALUE_LEN} B, memtable = {} MiB, block = {} B, bloom = {} bits/key, cache = {} MiB\n",
        opts.memtable_size >> 20, opts.block_size, opts.bloom_bits_per_key, opts.block_cache_bytes >> 20);
    println!("| Benchmark | Result |\n|---|---|");

    // ---- sequential writes, fsync off ----
    let dir = tmp("seq");
    let db = Db::open(&dir, opts.clone()).unwrap();
    let t = Instant::now();
    for i in 0..n {
        db.put(&key(i), &value).unwrap();
    }
    let secs = t.elapsed().as_secs_f64();
    println!(
        "| Sequential writes (fsync off) | {} ops/s |",
        ops_per_sec(n, secs)
    );
    db.flush().unwrap();
    db.wait_for_compactions().unwrap();
    drop(db);
    let _ = fs::remove_dir_all(&dir);

    // ---- random writes, fsync off; also write amplification / space ----
    let dir = tmp("rand");
    let db = Db::open(&dir, opts.clone()).unwrap();
    let mut rng = Rng(42);
    let mut present = vec![false; n as usize];
    let t = Instant::now();
    for _ in 0..n {
        let k = rng.next() % n;
        present[k as usize] = true;
        db.put(&key(k), &value).unwrap();
    }
    let distinct: Vec<u64> = (0..n).filter(|&i| present[i as usize]).collect();
    let secs = t.elapsed().as_secs_f64();
    println!(
        "| Random writes (fsync off) | {} ops/s |",
        ops_per_sec(n, secs)
    );
    db.flush().unwrap();
    db.wait_for_compactions().unwrap();
    let s = db.stats();
    println!(
        "| Write amplification, random overwrite (SSTable bytes / user bytes) | {:.2}x |",
        s.write_amplification()
    );
    println!(
        "| Write amplification incl. WAL | {:.2}x |",
        s.write_amplification_with_wal()
    );
    println!(
        "| Flushes / compactions / write stalls | {} / {} / {} |",
        s.flushes, s.compactions, s.stalls
    );
    println!(
        "| Files per level (L0..L6) | {:?} |",
        db.level_file_counts()
    );
    let live = distinct.len() as u64 * (VALUE_LEN as u64 + 16);
    println!(
        "| Space amplification (disk bytes / live key+value bytes) | {:.2}x |",
        dir_size(&dir) as f64 / live as f64
    );

    // ---- random reads: hits ----
    let reads = n.min(100_000);
    let mut rng = Rng(7);
    let t = Instant::now();
    let mut found = 0;
    for _ in 0..reads {
        if db
            .get(&key(distinct[(rng.next() % distinct.len() as u64) as usize]))
            .unwrap()
            .is_some()
        {
            found += 1;
        }
    }
    let secs = t.elapsed().as_secs_f64();
    println!(
        "| Random reads, hits ({found}/{reads} found) | {} ops/s |",
        ops_per_sec(reads, secs)
    );
    let s2 = db.stats();
    println!("| Block cache hit ratio during hit reads | {:.1}% |", {
        let h = (s2.cache_hits - s.cache_hits) as f64;
        let m = (s2.cache_misses - s.cache_misses) as f64;
        100.0 * h / (h + m).max(1.0)
    });

    // ---- range scan ----
    let t = Instant::now();
    let mut scanned = 0usize;
    let scans = 200;
    for _ in 0..scans {
        let lo = rng.next() % n.saturating_sub(1000).max(1);
        scanned += db.scan(&key(lo), &key(lo + 1000)).unwrap().len();
    }
    let secs = t.elapsed().as_secs_f64();
    println!(
        "| Range scan, 1000-key ranges | {} keys/s ({scans} scans) |",
        ops_per_sec(scanned as u64, secs)
    );
    drop(db);

    // ---- misses with and without bloom filters (separate DBs, same data) ----
    // Load even keys only, probe odd keys so every probe lands inside some file's key range.
    for (label, bits) in [("with bloom (10 bits/key)", 10usize), ("without bloom", 0)] {
        let dir2 = tmp(&format!("miss{bits}"));
        let o = Options {
            bloom_bits_per_key: bits,
            block_cache_bytes: 1 << 20,
            ..opts.clone()
        };
        let db = Db::open(&dir2, o).unwrap();
        for i in 0..n {
            db.put(&key(i * 2), &value).unwrap();
        }
        db.flush().unwrap();
        db.compact_all().unwrap();
        let before = db.stats();
        let probes = n.min(100_000);
        let mut rng = Rng(99);
        let t = Instant::now();
        for _ in 0..probes {
            assert!(db.get(&key((rng.next() % n) * 2 + 1)).unwrap().is_none());
        }
        let secs = t.elapsed().as_secs_f64();
        let after = db.stats();
        println!(
            "| Random misses, {label} | {} ops/s; {} block reads from disk/file, {} bloom-negatives, {} false positives |",
            ops_per_sec(probes, secs),
            after.block_reads - before.block_reads,
            after.bloom_negatives - before.bloom_negatives,
            after.bloom_false_positives - before.bloom_false_positives,
        );
        if bits > 0 {
            let checks = (after.bloom_checks - before.bloom_checks) as f64;
            let fps = (after.bloom_false_positives - before.bloom_false_positives) as f64;
            println!("| Measured bloom false-positive rate (theory ~0.82% at 10 bits/key, k=6) | {:.2}% |", 100.0 * fps / checks.max(1.0));
        }
        drop(db);
        let _ = fs::remove_dir_all(&dir2);
    }
    let _ = fs::remove_dir_all(&dir);

    // ---- durability cost: fsync on every write ----
    let sync_n = (n / 40).clamp(500, 5_000);
    let dir3 = tmp("sync");
    let db = Db::open(
        &dir3,
        Options {
            sync_writes: true,
            ..opts.clone()
        },
    )
    .unwrap();
    let t = Instant::now();
    for i in 0..sync_n {
        db.put(&key(i), &value).unwrap();
    }
    println!(
        "| Sequential writes (fsync on every write, {sync_n} ops) | {} ops/s |",
        ops_per_sec(sync_n, t.elapsed().as_secs_f64())
    );
    drop(db);
    let _ = fs::remove_dir_all(&dir3);

    // ---- baseline: BTreeMap + append-only log (no compaction, no SSTables) ----
    let dir4 = tmp("base");
    fs::create_dir_all(&dir4).unwrap();
    let mut log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir4.join("log"))
        .unwrap();
    let mut map: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
    let t = Instant::now();
    for i in 0..n {
        let k = key(i);
        let mut rec = Vec::with_capacity(8 + k.len() + VALUE_LEN);
        rec.extend_from_slice(&(k.len() as u32).to_le_bytes());
        rec.extend_from_slice(&(VALUE_LEN as u32).to_le_bytes());
        rec.extend_from_slice(&k);
        rec.extend_from_slice(&value);
        log.write_all(&rec).unwrap();
        map.insert(k, value.clone());
    }
    println!(
        "| Baseline: BTreeMap + append-only log, sequential writes (fsync off) | {} ops/s |",
        ops_per_sec(n, t.elapsed().as_secs_f64())
    );
    let mut rng = Rng(7);
    let t = Instant::now();
    let mut hits = 0;
    for _ in 0..n.min(100_000) {
        if map.contains_key(&key(rng.next() % n)) {
            hits += 1;
        }
    }
    println!(
        "| Baseline: in-memory BTreeMap random reads ({hits} hits) | {} ops/s |",
        ops_per_sec(n.min(100_000), t.elapsed().as_secs_f64())
    );
    let _ = fs::remove_dir_all(&dir4);
}
