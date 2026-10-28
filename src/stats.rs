use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Default)]
pub struct Stats {
    pub user_bytes: AtomicU64,
    pub wal_bytes: AtomicU64,
    pub flush_bytes: AtomicU64,
    pub compaction_read: AtomicU64,
    pub compaction_written: AtomicU64,
    pub compactions: AtomicU64,
    pub flushes: AtomicU64,
    pub stalls: AtomicU64,
    pub bloom_checks: AtomicU64,
    pub bloom_negatives: AtomicU64,
    pub bloom_false_positives: AtomicU64,
    pub block_reads: AtomicU64,
}

pub fn add(c: &AtomicU64, n: u64) {
    c.fetch_add(n, Ordering::Relaxed);
}

#[derive(Debug, Clone, Default)]
pub struct StatsSnapshot {
    pub user_bytes: u64,
    pub wal_bytes: u64,
    pub flush_bytes: u64,
    pub compaction_read: u64,
    pub compaction_written: u64,
    pub compactions: u64,
    pub flushes: u64,
    pub stalls: u64,
    pub bloom_checks: u64,
    pub bloom_negatives: u64,
    pub bloom_false_positives: u64,
    pub block_reads: u64,
    pub cache_hits: u64,
    pub cache_misses: u64,
}

impl Stats {
    pub fn snapshot(&self, cache_hits: u64, cache_misses: u64) -> StatsSnapshot {
        let l = |c: &AtomicU64| c.load(Ordering::Relaxed);
        StatsSnapshot {
            user_bytes: l(&self.user_bytes),
            wal_bytes: l(&self.wal_bytes),
            flush_bytes: l(&self.flush_bytes),
            compaction_read: l(&self.compaction_read),
            compaction_written: l(&self.compaction_written),
            compactions: l(&self.compactions),
            flushes: l(&self.flushes),
            stalls: l(&self.stalls),
            bloom_checks: l(&self.bloom_checks),
            bloom_negatives: l(&self.bloom_negatives),
            bloom_false_positives: l(&self.bloom_false_positives),
            block_reads: l(&self.block_reads),
            cache_hits,
            cache_misses,
        }
    }
}

impl StatsSnapshot {
    /// Bytes written to SSTables (flush + compaction) per user byte written.
    pub fn write_amplification(&self) -> f64 {
        if self.user_bytes == 0 {
            return 0.0;
        }
        (self.flush_bytes + self.compaction_written) as f64 / self.user_bytes as f64
    }

    /// Same, but also counting the write-ahead log.
    pub fn write_amplification_with_wal(&self) -> f64 {
        if self.user_bytes == 0 {
            return 0.0;
        }
        (self.wal_bytes + self.flush_bytes + self.compaction_written) as f64
            / self.user_bytes as f64
    }
}
