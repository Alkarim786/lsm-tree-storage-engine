use lsm_engine::{Db, Options};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let p = std::env::temp_dir().join(format!("lsm-it-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        TempDir(p)
    }
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Tiny sizes so that a few hundred writes exercise flushes and multi-level compaction.
fn small() -> Options {
    Options {
        memtable_size: 4 << 10,
        block_size: 512,
        l0_compaction_trigger: 2,
        l0_stop_writes: 8,
        level_base_size: 16 << 10,
        target_file_size: 8 << 10,
        ..Options::default()
    }
}

/// xorshift64* so the model tests are deterministic without external crates.
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
    format!("key{i:06}").into_bytes()
}

#[test]
fn put_get_delete_overwrite() {
    let t = TempDir::new("crud");
    let db = Db::open(t.path(), small()).unwrap();
    assert_eq!(db.get(b"a").unwrap(), None);
    db.put(b"a", b"1").unwrap();
    db.put(b"a", b"2").unwrap();
    assert_eq!(db.get(b"a").unwrap(), Some(b"2".to_vec()));
    db.delete(b"a").unwrap();
    assert_eq!(db.get(b"a").unwrap(), None);
    db.put(b"empty", b"").unwrap();
    assert_eq!(db.get(b"empty").unwrap(), Some(vec![]));
}

#[test]
fn survives_reopen_from_wal_and_from_sstables() {
    let t = TempDir::new("reopen");
    {
        let db = Db::open(t.path(), small()).unwrap();
        for i in 0..50 {
            db.put(&key(i), b"wal-only").unwrap();
        }
    } // dropped without flush: data lives only in the WAL
    {
        let db = Db::open(t.path(), small()).unwrap();
        for i in 0..50 {
            assert_eq!(
                db.get(&key(i)).unwrap(),
                Some(b"wal-only".to_vec()),
                "key {i}"
            );
        }
        for i in 0..2000 {
            db.put(&key(i), format!("v{i}").as_bytes()).unwrap();
        }
        db.delete(&key(7)).unwrap();
        db.flush().unwrap();
        db.wait_for_compactions().unwrap();
    }
    let db = Db::open(t.path(), small()).unwrap();
    assert_eq!(db.get(&key(7)).unwrap(), None);
    assert_eq!(db.get(&key(1999)).unwrap(), Some(b"v1999".to_vec()));
    assert_eq!(db.get(&key(500)).unwrap(), Some(b"v500".to_vec()));
}

#[test]
fn scan_merges_all_sources_in_order_and_hides_tombstones() {
    let t = TempDir::new("scan");
    let db = Db::open(t.path(), small()).unwrap();
    for i in 0..1000 {
        db.put(&key(i), b"old").unwrap();
    }
    db.flush().unwrap();
    for i in (0..1000).step_by(3) {
        db.delete(&key(i)).unwrap();
    }
    for i in (1..1000).step_by(10) {
        db.put(&key(i), b"new").unwrap();
    }
    let got = db.scan(&key(100), &key(200)).unwrap();
    let mut model = BTreeMap::new();
    for i in 100..200u64 {
        if i % 10 == 1 {
            model.insert(key(i), b"new".to_vec()); // re-put after the delete pass resurrects it
        } else if i % 3 != 0 {
            model.insert(key(i), b"old".to_vec());
        }
    }
    assert_eq!(got, model.into_iter().collect::<Vec<_>>());
}

#[test]
fn compaction_keeps_latest_values_and_drops_tombstones() {
    let t = TempDir::new("compact");
    let db = Db::open(t.path(), small()).unwrap();
    let mut model = BTreeMap::new();
    for round in 0..6u32 {
        for i in 0..800u64 {
            let v = format!("round{round}-{i}").into_bytes();
            db.put(&key(i), &v).unwrap();
            model.insert(key(i), v);
        }
    }
    for i in (0..800u64).filter(|i| i % 2 == 0) {
        db.delete(&key(i)).unwrap();
        model.remove(&key(i));
    }
    db.flush().unwrap();
    db.wait_for_compactions().unwrap();

    let counts = db.level_file_counts();
    assert!(
        counts[0] < 2,
        "L0 should have been compacted away: {counts:?}"
    );
    assert!(
        counts[1..].iter().sum::<usize>() > 0,
        "data should live in deeper levels: {counts:?}"
    );
    assert!(db.stats().compactions > 0);
    for i in 0..800u64 {
        assert_eq!(
            db.get(&key(i)).unwrap(),
            model.get(&key(i)).cloned(),
            "key {i}"
        );
    }
    let all = db.scan(b"", b"\xff").unwrap();
    assert_eq!(all, model.into_iter().collect::<Vec<_>>());

    // Obsolete files are physically deleted: only live SSTables remain on disk.
    let ssts = std::fs::read_dir(t.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|x| x == "sst"))
        .count();
    assert_eq!(ssts, db.level_file_counts().iter().sum::<usize>());
}

#[test]
fn tombstones_are_physically_removed_at_the_bottom_level() {
    let t = TempDir::new("tomb");
    let db = Db::open(t.path(), small()).unwrap();
    for i in 0..1500u64 {
        db.put(&key(i), &[b'x'; 40]).unwrap();
    }
    db.flush().unwrap();
    db.wait_for_compactions().unwrap();
    let before = db.disk_bytes();
    for i in 0..1500u64 {
        db.delete(&key(i)).unwrap();
    }
    db.compact_all().unwrap();
    assert!(db.scan(b"", b"\xff").unwrap().is_empty());
    assert_eq!(
        db.disk_bytes(),
        0,
        "every tombstone and shadowed value should be gone"
    );
    assert!(
        db.disk_bytes() < before,
        "disk usage should shrink: {} -> {}",
        before,
        db.disk_bytes()
    );
}

#[test]
fn model_based_random_operations_with_reopens() {
    let t = TempDir::new("model");
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let mut model: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
    let mut db = Some(Db::open(t.path(), small()).unwrap());
    for step in 0..12_000u32 {
        let d = db.as_ref().unwrap();
        let k = key(rng.next() % 600);
        match rng.next() % 10 {
            0..=5 => {
                let v = format!("v{step}-{}", rng.next() % 1000).into_bytes();
                d.put(&k, &v).unwrap();
                model.insert(k, v);
            }
            6 | 7 => {
                d.delete(&k).unwrap();
                model.remove(&k);
            }
            8 => assert_eq!(d.get(&k).unwrap(), model.get(&k).cloned(), "step {step}"),
            _ => {
                let lo = key(rng.next() % 600);
                let hi = key(rng.next() % 600);
                let got = d.scan(&lo, &hi).unwrap();
                let want: Vec<_> = if lo < hi {
                    model
                        .range(lo.clone()..hi.clone())
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect()
                } else {
                    vec![]
                };
                assert_eq!(got, want, "scan at step {step}");
            }
        }
        if step % 3000 == 2999 {
            drop(db.take()); // close (writes stay in the WAL) and recover
            db = Some(Db::open(t.path(), small()).unwrap());
        }
    }
    let d = db.as_ref().unwrap();
    d.flush().unwrap();
    d.wait_for_compactions().unwrap();
    assert_eq!(
        d.scan(b"", b"\xff").unwrap(),
        model.into_iter().collect::<Vec<_>>()
    );
}

#[test]
fn crash_recovery_after_wal_truncation_yields_a_prefix_of_acknowledged_writes() {
    let mut rng = Rng(12345);
    for trial in 0..25u32 {
        let t = TempDir::new(&format!("crash{trial}"));
        let opts = Options {
            memtable_size: 64 << 20,
            ..small()
        }; // never flushes: everything is in the WAL
        let n = 80usize;
        let mut ops: Vec<(Vec<u8>, Option<Vec<u8>>)> = Vec::new();
        {
            let db = Db::open(t.path(), opts.clone()).unwrap();
            for i in 0..n {
                let k = key(rng.next() % 20);
                if rng.next().is_multiple_of(4) {
                    db.delete(&k).unwrap();
                    ops.push((k, None));
                } else {
                    let v = format!("value-{i}").into_bytes();
                    db.put(&k, &v).unwrap();
                    ops.push((k, Some(v)));
                }
            }
        }
        // Simulate a crash mid-write: chop the newest WAL at a random byte offset.
        let wal = std::fs::read_dir(t.path())
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.extension().is_some_and(|x| x == "wal"))
            .max()
            .unwrap();
        let bytes = std::fs::read(&wal).unwrap();
        let cut = (rng.next() as usize) % (bytes.len() + 1);
        std::fs::write(&wal, &bytes[..cut]).unwrap();

        let db = Db::open(t.path(), opts).unwrap();
        let recovered = db.scan(b"", b"\xff").unwrap();
        // The recovered state must equal the model after exactly some prefix of the ops.
        let matches_prefix = (0..=n).any(|p| {
            let mut m = BTreeMap::new();
            for (k, v) in &ops[..p] {
                match v {
                    Some(v) => m.insert(k.clone(), v.clone()),
                    None => m.remove(k),
                };
            }
            m.into_iter().collect::<Vec<_>>() == recovered
        });
        assert!(
            matches_prefix,
            "trial {trial}: cut {cut}/{} not a prefix state",
            bytes.len()
        );
    }
}

#[test]
fn unreferenced_files_are_cleaned_up_on_open() {
    let t = TempDir::new("orphan");
    {
        let db = Db::open(t.path(), small()).unwrap();
        db.put(b"k", b"v").unwrap();
        db.flush().unwrap();
    }
    std::fs::write(
        t.path().join("999999.sst"),
        b"garbage from an interrupted compaction",
    )
    .unwrap();
    let db = Db::open(t.path(), small()).unwrap();
    assert!(!t.path().join("999999.sst").exists());
    assert_eq!(db.get(b"k").unwrap(), Some(b"v".to_vec()));
}

#[test]
fn concurrent_writers_and_readers() {
    let t = TempDir::new("concurrent");
    let db = Arc::new(Db::open(t.path(), small()).unwrap());
    let writers: Vec<_> = (0..4u64)
        .map(|w| {
            let db = db.clone();
            std::thread::spawn(move || {
                for i in 0..1500u64 {
                    db.put(&key(w * 10_000 + i), format!("w{w}-{i}").as_bytes())
                        .unwrap();
                }
            })
        })
        .collect();
    let readers: Vec<_> = (0..2u64)
        .map(|_| {
            let db = db.clone();
            std::thread::spawn(move || {
                for i in 0..3000u64 {
                    // Anything read must be a complete, correct value or absent (never torn).
                    if let Some(v) = db.get(&key((i % 4) * 10_000 + i % 1500)).unwrap() {
                        assert!(v.starts_with(b"w"));
                    }
                }
            })
        })
        .collect();
    for h in writers.into_iter().chain(readers) {
        h.join().unwrap();
    }
    db.wait_for_compactions().unwrap();
    for w in 0..4u64 {
        for i in (0..1500u64).step_by(37) {
            assert_eq!(
                db.get(&key(w * 10_000 + i)).unwrap(),
                Some(format!("w{w}-{i}").into_bytes())
            );
        }
    }
}

#[test]
fn bloom_filters_skip_disk_reads_for_missing_keys() {
    let t = TempDir::new("bloom");
    let db = Db::open(t.path(), small()).unwrap();
    for i in 0..3000u64 {
        db.put(&key(i * 2), b"value").unwrap(); // even keys only
    }
    db.flush().unwrap();
    db.wait_for_compactions().unwrap();
    let before = db.stats();
    for i in 0..3000u64 {
        assert_eq!(db.get(&key(i * 2 + 1)).unwrap(), None); // odd keys: inside file ranges, absent
    }
    let after = db.stats();
    let negatives = after.bloom_negatives - before.bloom_negatives;
    let false_pos = after.bloom_false_positives - before.bloom_false_positives;
    assert!(negatives > 0);
    assert!(
        false_pos * 20 < negatives + false_pos,
        "FP rate too high: {false_pos} vs {negatives}"
    );
}
