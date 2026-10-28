//! Sorted String Table: immutable on-disk sorted run.
//!
//! ```text
//! [data block]* [index block] [bloom block] [footer: 6 x u64 LE]
//! data block  = entries (varint shared, varint unshared, varint vlen+1 | 0=tombstone,
//!               key suffix, value) + crc32
//! index block = (varint klen, last key of block, u64 offset, u32 len)* + crc32
//! bloom block = bits ++ k, + crc32 (empty body when bloom is disabled)
//! footer      = index_off, index_len, bloom_off, bloom_len, entry_count, MAGIC
//! ```

pub mod block;
pub mod builder;
pub mod reader;

pub const MAGIC: u64 = 0x4c53_4d54_5245_4531; // "LSMTREE1"
pub const FOOTER_LEN: usize = 48;

pub use builder::SstBuilder;
pub use reader::{SstIter, SstReader};
