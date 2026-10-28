//! The database: ties WAL, memtables, SSTables, MANIFEST and background
//! flush/compaction together.
//!
//! Concurrency model: one `Mutex<State>` guards the active memtable, the immutable
//! memtable, the WAL and an `Arc<Version>` (the set of live SSTables). Readers copy
//! the `Arc`s under the lock and then read without it. A single background thread
//! flushes the immutable memtable (priority) and runs leveled compactions, swapping
//! in a new `Version` copy-on-write.

use crate::cache::BlockCache;
use crate::manifest::{self, ManifestData};
use crate::memtable::Memtable;
use crate::merge::{MergeIter, Source};
use crate::sstable::{SstBuilder, SstReader};
use crate::stats::{add, Stats, StatsSnapshot};
use crate::wal::{self, WalWriter};
use crate::Entry;
use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

pub const NUM_LEVELS: usize = 7;

#[derive(Clone, Debug)]
pub struct Options {
    /// Active memtable is frozen and flushed once it reaches this many bytes.
    pub memtable_size: usize,
    pub block_size: usize,
    /// 0 disables bloom filters (new SSTables are written without one).
    pub bloom_bits_per_key: usize,
    /// Number of L0 files that triggers an L0 -> L1 compaction.
    pub l0_compaction_trigger: usize,
    /// Writers stall while L0 holds at least this many files.
    pub l0_stop_writes: usize,
    /// Target size of L1; level N holds `level_base_size * size_ratio^(N-1)`.
    pub level_base_size: u64,
    pub size_ratio: u64,
    /// Compaction output files are split at about this size.
    pub target_file_size: u64,
    pub block_cache_bytes: usize,
    /// fsync the WAL on every write (durable) or leave it to the OS (fast).
    pub sync_writes: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            memtable_size: 4 << 20,
            block_size: 4096,
            bloom_bits_per_key: 10,
            l0_compaction_trigger: 4,
            l0_stop_writes: 12,
            level_base_size: 10 << 20,
            size_ratio: 10,
            target_file_size: 2 << 20,
            block_cache_bytes: 8 << 20,
            sync_writes: false,
        }
    }
}

struct Version {
    levels: Vec<Vec<Arc<SstReader>>>,
}

struct State {
    mem: Memtable,
    imm: Option<Arc<Memtable>>,
    imm_wal: Option<u64>,
    version: Arc<Version>,
    wal: WalWriter,
    wal_id: u64,
    log_number: u64,
    compact_ptr: Vec<Vec<u8>>,
    running: bool,
    force: bool,
    shutdown: bool,
    bg_error: Option<String>,
}

struct Shared {
    opts: Options,
    dir: PathBuf,
    state: Mutex<State>,
    work_cv: Condvar,
    done_cv: Condvar,
    cache: Arc<BlockCache>,
    stats: Arc<Stats>,
    next_id: AtomicU64,
}

pub struct Db {
    shared: Arc<Shared>,
    worker: Option<JoinHandle<()>>,
}

fn sst_path(dir: &Path, id: u64) -> PathBuf {
    dir.join(format!("{id:06}.sst"))
}

fn wal_path(dir: &Path, id: u64) -> PathBuf {
    dir.join(format!("{id:06}.wal"))
}

struct Compaction {
    level: usize,
    inputs: Vec<Arc<SstReader>>,
    next: Vec<Arc<SstReader>>,
    drop_tombstones: bool,
}

fn level_limit(opts: &Options, level: usize) -> u64 {
    opts.level_base_size * opts.size_ratio.pow(level as u32 - 1)
}

fn key_range(files: &[&Arc<SstReader>]) -> (Vec<u8>, Vec<u8>) {
    let lo = files.iter().map(|f| &f.smallest).min().unwrap().clone();
    let hi = files.iter().map(|f| &f.largest).max().unwrap().clone();
    (lo, hi)
}

fn pick_compaction(
    v: &Version,
    opts: &Options,
    ptrs: &[Vec<u8>],
    force: bool,
) -> Option<Compaction> {
    let trigger = if force { 1 } else { opts.l0_compaction_trigger };
    let (level, inputs) = if v.levels[0].len() >= trigger {
        (0, v.levels[0].clone())
    } else {
        let level = (1..NUM_LEVELS - 1).find(|&l| {
            let limit = if force { 0 } else { level_limit(opts, l) };
            v.levels[l].iter().map(|f| f.file_size).sum::<u64>() > limit
        })?;
        // Round-robin: continue after the key we compacted last time at this level.
        let files = &v.levels[level];
        let f = files
            .iter()
            .find(|f| f.smallest > ptrs[level])
            .unwrap_or(&files[0]);
        (level, vec![f.clone()])
    };
    let refs: Vec<&Arc<SstReader>> = inputs.iter().collect();
    let (lo, hi) = key_range(&refs);
    let next: Vec<_> = v.levels[level + 1]
        .iter()
        .filter(|f| f.overlaps(&lo, &hi))
        .cloned()
        .collect();
    let all: Vec<&Arc<SstReader>> = inputs.iter().chain(next.iter()).collect();
    let (lo, hi) = key_range(&all);
    // Tombstones can be discarded only if nothing deeper could still hold the key.
    let drop_tombstones =
        (level + 2..NUM_LEVELS).all(|l| v.levels[l].iter().all(|f| !f.overlaps(&lo, &hi)));
    Some(Compaction {
        level,
        inputs,
        next,
        drop_tombstones,
    })
}

impl Shared {
    fn save_manifest(&self, st: &State) -> io::Result<()> {
        let files = st
            .version
            .levels
            .iter()
            .enumerate()
            .flat_map(|(l, fs)| fs.iter().map(move |f| (l, f.id)))
            .collect();
        manifest::save(
            &self.dir,
            &ManifestData {
                next_id: self.next_id.load(Ordering::SeqCst),
                log_number: st.log_number,
                files,
            },
        )
    }

    fn open_sst(&self, id: u64) -> io::Result<Arc<SstReader>> {
        Ok(Arc::new(SstReader::open(
            &sst_path(&self.dir, id),
            id,
            self.cache.clone(),
            self.stats.clone(),
        )?))
    }

    /// Writes a sorted stream of entries into one or more SSTables, splitting at
    /// `split` bytes. Returns `(id, size)` for each file.
    fn write_run(
        &self,
        iter: impl Iterator<Item = io::Result<Entry>>,
        drop_tombstones: bool,
        split: u64,
    ) -> io::Result<Vec<(u64, u64)>> {
        let mut out = Vec::new();
        let mut cur: Option<(u64, SstBuilder)> = None;
        for item in iter {
            let (k, v) = item?;
            if v.is_none() && drop_tombstones {
                continue;
            }
            if cur.is_none() {
                let id = self.next_id.fetch_add(1, Ordering::SeqCst);
                let b = SstBuilder::create(
                    &sst_path(&self.dir, id),
                    self.opts.block_size,
                    self.opts.bloom_bits_per_key,
                )?;
                cur = Some((id, b));
            }
            let (_, b) = cur.as_mut().unwrap();
            b.add(&k, v.as_deref())?;
            if b.estimated_size() >= split {
                let (id, b) = cur.take().unwrap();
                out.push((id, b.finish()?));
            }
        }
        if let Some((id, b)) = cur {
            out.push((id, b.finish()?));
        }
        Ok(out)
    }

    fn rotate(&self, st: &mut State) -> io::Result<()> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let wal = WalWriter::create(&wal_path(&self.dir, id), self.opts.sync_writes)?;
        st.imm_wal = Some(st.wal_id);
        st.wal_id = id;
        st.wal = wal;
        st.imm = Some(Arc::new(std::mem::take(&mut st.mem)));
        self.work_cv.notify_all();
        Ok(())
    }

    fn do_flush(&self, imm: Arc<Memtable>) -> io::Result<()> {
        let files = self.write_run(imm.iter().map(Ok), false, u64::MAX)?;
        let mut readers = Vec::new();
        for (id, size) in &files {
            add(&self.stats.flush_bytes, *size);
            readers.push(self.open_sst(*id)?);
        }
        let mut st = self.state.lock().unwrap();
        let mut levels = st.version.levels.clone();
        levels[0].extend(readers);
        st.version = Arc::new(Version { levels });
        st.imm = None;
        st.log_number = st.wal_id;
        self.save_manifest(&st)?;
        if let Some(old) = st.imm_wal.take() {
            let _ = fs::remove_file(wal_path(&self.dir, old));
        }
        add(&self.stats.flushes, 1);
        Ok(())
    }

    fn do_compact(&self, c: Compaction) -> io::Result<()> {
        // Newer data first: L0 files are ordered oldest -> newest.
        let ordered: Vec<&Arc<SstReader>> = if c.level == 0 {
            c.inputs.iter().rev().chain(c.next.iter()).collect()
        } else {
            c.inputs.iter().chain(c.next.iter()).collect()
        };
        let read: u64 = ordered.iter().map(|f| f.file_size).sum();
        let sources: Vec<Source> = ordered
            .iter()
            .map(|f| Box::new(f.iter_from(None, false)) as Source)
            .collect();
        let outs = self.write_run(
            MergeIter::new(sources),
            c.drop_tombstones,
            self.opts.target_file_size,
        )?;
        let mut readers = Vec::new();
        let mut written = 0;
        for (id, size) in &outs {
            written += size;
            readers.push(self.open_sst(*id)?);
        }
        let mut st = self.state.lock().unwrap();
        let dead: HashSet<u64> = c.inputs.iter().chain(c.next.iter()).map(|f| f.id).collect();
        let mut levels = st.version.levels.clone();
        levels[c.level].retain(|f| !dead.contains(&f.id));
        levels[c.level + 1].retain(|f| !dead.contains(&f.id));
        levels[c.level + 1].extend(readers);
        levels[c.level + 1].sort_by(|a, b| a.smallest.cmp(&b.smallest));
        st.version = Arc::new(Version { levels });
        if c.level > 0 {
            st.compact_ptr[c.level] = c.inputs.last().unwrap().largest.clone();
        }
        self.save_manifest(&st)?;
        for f in c.inputs.iter().chain(c.next.iter()) {
            f.mark_obsolete();
        }
        add(&self.stats.compactions, 1);
        add(&self.stats.compaction_read, read);
        add(&self.stats.compaction_written, written);
        Ok(())
    }
}

enum Job {
    Flush(Arc<Memtable>),
    Compact(Compaction),
}

fn background(sh: Arc<Shared>) {
    loop {
        let job = {
            let mut st = sh.state.lock().unwrap();
            loop {
                if st.shutdown || st.bg_error.is_some() {
                    return;
                }
                if let Some(imm) = st.imm.clone() {
                    st.running = true;
                    break Job::Flush(imm);
                }
                if let Some(c) = pick_compaction(&st.version, &sh.opts, &st.compact_ptr, st.force) {
                    st.running = true;
                    break Job::Compact(c);
                }
                st = sh.work_cv.wait(st).unwrap();
            }
        };
        let res = match job {
            Job::Flush(imm) => sh.do_flush(imm),
            Job::Compact(c) => sh.do_compact(c),
        };
        let mut st = sh.state.lock().unwrap();
        st.running = false;
        if let Err(e) = res {
            st.bg_error = Some(e.to_string());
        }
        sh.done_cv.notify_all();
    }
}

impl Db {
    pub fn open(dir: impl AsRef<Path>, opts: Options) -> io::Result<Db> {
        let dir = dir.as_ref().to_path_buf();
        fs::create_dir_all(&dir)?;
        let cache = Arc::new(BlockCache::new(opts.block_cache_bytes));
        let stats = Arc::new(Stats::default());
        let man = manifest::load(&dir)?.unwrap_or_default();
        let live: HashSet<u64> = man.files.iter().map(|(_, id)| *id).collect();

        let mut levels: Vec<Vec<Arc<SstReader>>> = vec![Vec::new(); NUM_LEVELS];
        for (level, id) in &man.files {
            let r = SstReader::open(&sst_path(&dir, *id), *id, cache.clone(), stats.clone())?;
            levels[*level].push(Arc::new(r));
        }
        levels[0].sort_by_key(|r| r.id);
        for l in levels.iter_mut().skip(1) {
            l.sort_by(|a, b| a.smallest.cmp(&b.smallest));
        }

        // Anything not referenced by the MANIFEST is garbage from an interrupted flush/compaction.
        let mut wals = Vec::new();
        for entry in fs::read_dir(&dir)? {
            let path = entry?.path();
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_string();
            let id = name.split('.').next().and_then(|s| s.parse::<u64>().ok());
            match (name.rsplit('.').next(), id) {
                (Some("sst"), Some(id)) if !live.contains(&id) => {
                    let _ = fs::remove_file(&path);
                }
                (Some("wal"), Some(id)) if id < man.log_number => {
                    let _ = fs::remove_file(&path);
                }
                (Some("wal"), Some(id)) => wals.push(id),
                (Some("tmp"), _) => {
                    let _ = fs::remove_file(&path);
                }
                _ => {}
            }
        }
        wals.sort_unstable();

        let mut mem = Memtable::new();
        for id in &wals {
            for (k, v) in wal::read_wal(&wal_path(&dir, *id))? {
                mem.apply(k, v);
            }
        }

        let next_id = AtomicU64::new(man.next_id.max(wals.last().map_or(0, |w| w + 1)).max(1));
        // Recovered writes are flushed straight to L0 so the old WALs can be retired.
        let mut recovered = Vec::new();
        if !mem.is_empty() {
            let tmp = RecoveryFlush {
                dir: &dir,
                opts: &opts,
                next_id: &next_id,
            };
            for (id, size) in tmp.flush(&mem)? {
                add(&stats.flush_bytes, size);
                recovered.push(Arc::new(SstReader::open(
                    &sst_path(&dir, id),
                    id,
                    cache.clone(),
                    stats.clone(),
                )?));
            }
        }
        levels[0].extend(recovered);

        let wal_id = next_id.fetch_add(1, Ordering::SeqCst);
        let state = State {
            mem: Memtable::new(),
            imm: None,
            imm_wal: None,
            version: Arc::new(Version { levels }),
            wal: WalWriter::create(&wal_path(&dir, wal_id), opts.sync_writes)?,
            wal_id,
            log_number: wal_id,
            compact_ptr: vec![Vec::new(); NUM_LEVELS],
            running: false,
            force: false,
            shutdown: false,
            bg_error: None,
        };
        let shared = Arc::new(Shared {
            opts,
            dir: dir.clone(),
            state: Mutex::new(state),
            work_cv: Condvar::new(),
            done_cv: Condvar::new(),
            cache,
            stats,
            next_id,
        });
        shared.save_manifest(&shared.state.lock().unwrap())?;
        for id in wals {
            let _ = fs::remove_file(wal_path(&dir, id));
        }
        let sh = shared.clone();
        let worker = std::thread::Builder::new()
            .name("lsm-background".into())
            .spawn(move || background(sh))?;
        shared.work_cv.notify_all();
        Ok(Db {
            shared,
            worker: Some(worker),
        })
    }

    pub fn put(&self, key: &[u8], value: &[u8]) -> io::Result<()> {
        self.write(key, Some(value))
    }

    pub fn delete(&self, key: &[u8]) -> io::Result<()> {
        self.write(key, None)
    }

    fn write(&self, key: &[u8], val: Option<&[u8]>) -> io::Result<()> {
        let sh = &self.shared;
        let mut st = sh.state.lock().unwrap();
        loop {
            if let Some(e) = &st.bg_error {
                return Err(io::Error::other(e.clone()));
            }
            let mem_full = st.mem.size() >= sh.opts.memtable_size && st.imm.is_some();
            if mem_full || st.version.levels[0].len() >= sh.opts.l0_stop_writes {
                add(&sh.stats.stalls, 1);
                st = sh.done_cv.wait(st).unwrap();
            } else {
                break;
            }
        }
        let n = st.wal.append(key, val)?;
        add(&sh.stats.wal_bytes, n as u64);
        add(
            &sh.stats.user_bytes,
            (key.len() + val.map_or(0, |v| v.len())) as u64,
        );
        st.mem.apply(key.to_vec(), val.map(|v| v.to_vec()));
        if st.mem.size() >= sh.opts.memtable_size && st.imm.is_none() {
            sh.rotate(&mut st)?;
        }
        Ok(())
    }

    pub fn get(&self, key: &[u8]) -> io::Result<Option<Vec<u8>>> {
        let (mem_hit, imm, version) = {
            let st = self.shared.state.lock().unwrap();
            (st.mem.get(key).cloned(), st.imm.clone(), st.version.clone())
        };
        if let Some(v) = mem_hit {
            return Ok(v);
        }
        if let Some(v) = imm.as_ref().and_then(|m| m.get(key)) {
            return Ok(v.clone());
        }
        for f in version.levels[0].iter().rev() {
            if let Some(v) = f.get(key)? {
                return Ok(v);
            }
        }
        for files in &version.levels[1..] {
            let i = files.partition_point(|f| f.largest.as_slice() < key);
            if let Some(f) = files.get(i) {
                if let Some(v) = f.get(key)? {
                    return Ok(v);
                }
            }
        }
        Ok(None)
    }

    /// Live key/value pairs with `start <= key < end`, in key order.
    pub fn scan(&self, start: &[u8], end: &[u8]) -> io::Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let (mem, imm, version) = {
            let st = self.shared.state.lock().unwrap();
            (st.mem.range_from(start), st.imm.clone(), st.version.clone())
        };
        let ok = |v: Vec<Entry>| -> Source { Box::new(v.into_iter().map(Ok)) };
        let mut sources: Vec<Source> = vec![ok(mem)];
        if let Some(m) = imm {
            sources.push(ok(m.range_from(start)));
        }
        for f in version.levels[0]
            .iter()
            .rev()
            .filter(|f| f.largest.as_slice() >= start)
        {
            sources.push(Box::new(f.iter_from(Some(start), true)));
        }
        for files in &version.levels[1..] {
            for f in files
                .iter()
                .filter(|f| f.largest.as_slice() >= start && f.smallest.as_slice() < end)
            {
                sources.push(Box::new(f.iter_from(Some(start), true)));
            }
        }
        let mut out = Vec::new();
        for item in MergeIter::new(sources) {
            let (k, v) = item?;
            if k.as_slice() >= end {
                break;
            }
            if let Some(v) = v {
                out.push((k, v));
            }
        }
        Ok(out)
    }

    /// Freezes the active memtable and waits until it is on disk.
    pub fn flush(&self) -> io::Result<()> {
        let sh = &self.shared;
        let mut st = sh.state.lock().unwrap();
        loop {
            if let Some(e) = &st.bg_error {
                return Err(io::Error::other(e.clone()));
            }
            if st.imm.is_none() {
                break;
            }
            st = sh.done_cv.wait(st).unwrap();
        }
        if !st.mem.is_empty() {
            sh.rotate(&mut st)?;
        }
        loop {
            if let Some(e) = &st.bg_error {
                return Err(io::Error::other(e.clone()));
            }
            if st.imm.is_none() {
                return Ok(());
            }
            st = sh.done_cv.wait(st).unwrap();
        }
    }

    /// Blocks until no flush is pending and no compaction is needed or running.
    pub fn wait_for_compactions(&self) -> io::Result<()> {
        let sh = &self.shared;
        let mut st = sh.state.lock().unwrap();
        loop {
            if let Some(e) = &st.bg_error {
                return Err(io::Error::other(e.clone()));
            }
            if st.imm.is_none()
                && !st.running
                && pick_compaction(&st.version, &sh.opts, &st.compact_ptr, st.force).is_none()
            {
                return Ok(());
            }
            st = sh
                .done_cv
                .wait_timeout(st, Duration::from_millis(20))
                .unwrap()
                .0;
        }
    }

    /// Manual full compaction: flushes, then pushes every file down to the bottom
    /// level, discarding shadowed versions and tombstones along the way.
    pub fn compact_all(&self) -> io::Result<()> {
        self.flush()?;
        self.shared.state.lock().unwrap().force = true;
        self.shared.work_cv.notify_all();
        let res = self.wait_for_compactions();
        self.shared.state.lock().unwrap().force = false;
        res
    }

    pub fn stats(&self) -> StatsSnapshot {
        let (h, m) = self.shared.cache.hits_misses();
        self.shared.stats.snapshot(h, m)
    }

    /// Number of SSTables at each level.
    pub fn level_file_counts(&self) -> Vec<usize> {
        let st = self.shared.state.lock().unwrap();
        st.version.levels.iter().map(|l| l.len()).collect()
    }

    /// Total bytes of SSTables on disk.
    pub fn disk_bytes(&self) -> u64 {
        let st = self.shared.state.lock().unwrap();
        st.version
            .levels
            .iter()
            .flatten()
            .map(|f| f.file_size)
            .sum()
    }
}

impl Drop for Db {
    /// Stops the background thread. Unflushed writes stay in the WAL and are
    /// replayed on the next `open`, exactly as after a crash.
    fn drop(&mut self) {
        self.shared.state.lock().unwrap().shutdown = true;
        self.shared.work_cv.notify_all();
        if let Some(h) = self.worker.take() {
            let _ = h.join();
        }
    }
}

/// Minimal flush helper usable before the `Shared` struct exists (during recovery).
struct RecoveryFlush<'a> {
    dir: &'a Path,
    opts: &'a Options,
    next_id: &'a AtomicU64,
}

impl RecoveryFlush<'_> {
    fn flush(&self, mem: &Memtable) -> io::Result<Vec<(u64, u64)>> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let mut b = SstBuilder::create(
            &sst_path(self.dir, id),
            self.opts.block_size,
            self.opts.bloom_bits_per_key,
        )?;
        for (k, v) in mem.iter() {
            b.add(&k, v.as_deref())?;
        }
        Ok(vec![(id, b.finish()?)])
    }
}
