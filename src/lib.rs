//! A LevelDB-style LSM-tree storage engine: WAL, memtable, SSTables with bloom
//! filters, a MANIFEST, leveled background compaction and an LRU block cache.
//! Zero third-party dependencies.

pub mod bloom;
pub mod cache;
pub mod crc;
pub mod db;
pub mod manifest;
pub mod memtable;
pub mod merge;
pub mod sstable;
pub mod stats;
pub mod wal;

pub use db::{Db, Options};
pub use stats::StatsSnapshot;

/// A decoded key/value entry. `None` is a tombstone.
pub type Entry = (Vec<u8>, Option<Vec<u8>>);

pub(crate) fn invalid(msg: impl Into<String>) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, msg.into())
}
