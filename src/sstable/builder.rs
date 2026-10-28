use super::block::{put_varint, with_crc, BlockBuilder};
use super::MAGIC;
use crate::bloom::{hash, Bloom};
use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::Path;

pub struct SstBuilder {
    w: BufWriter<File>,
    offset: u64,
    block: BlockBuilder,
    block_size: usize,
    index: Vec<(Vec<u8>, u64, u32)>,
    hashes: Vec<u64>,
    bits_per_key: usize,
    last_key: Vec<u8>,
    count: u64,
}

impl SstBuilder {
    pub fn create(path: &Path, block_size: usize, bits_per_key: usize) -> io::Result<Self> {
        Ok(SstBuilder {
            w: BufWriter::with_capacity(64 << 10, File::create(path)?),
            offset: 0,
            block: BlockBuilder::default(),
            block_size,
            index: Vec::new(),
            hashes: Vec::new(),
            bits_per_key,
            last_key: Vec::new(),
            count: 0,
        })
    }

    /// Keys must be added in strictly increasing order.
    pub fn add(&mut self, key: &[u8], val: Option<&[u8]>) -> io::Result<()> {
        debug_assert!(
            self.count == 0 || key > self.last_key.as_slice(),
            "keys must be sorted"
        );
        if !self.block.is_empty() && self.block.size() >= self.block_size {
            self.flush_block()?;
        }
        self.block.add(key, val);
        self.last_key.clear();
        self.last_key.extend_from_slice(key);
        self.hashes.push(hash(key));
        self.count += 1;
        Ok(())
    }

    fn flush_block(&mut self) -> io::Result<()> {
        let data = self.block.finish();
        self.w.write_all(&data)?;
        self.index
            .push((self.last_key.clone(), self.offset, data.len() as u32));
        self.offset += data.len() as u64;
        Ok(())
    }

    /// Bytes written so far plus the pending block; used to split compaction output.
    pub fn estimated_size(&self) -> u64 {
        self.offset + self.block.size() as u64
    }

    /// Writes index, bloom filter and footer, fsyncs, and returns the file size.
    pub fn finish(mut self) -> io::Result<u64> {
        if !self.block.is_empty() {
            self.flush_block()?;
        }
        let mut idx = Vec::new();
        for (key, off, len) in &self.index {
            put_varint(&mut idx, key.len() as u32);
            idx.extend_from_slice(key);
            idx.extend_from_slice(&off.to_le_bytes());
            idx.extend_from_slice(&len.to_le_bytes());
        }
        let idx = with_crc(idx);
        let index_off = self.offset;
        self.w.write_all(&idx)?;
        self.offset += idx.len() as u64;

        let bloom = with_crc(Bloom::from_hashes(&self.hashes, self.bits_per_key).encode());
        let bloom_off = self.offset;
        self.w.write_all(&bloom)?;
        self.offset += bloom.len() as u64;

        for v in [
            index_off,
            idx.len() as u64,
            bloom_off,
            bloom.len() as u64,
            self.count,
            MAGIC,
        ] {
            self.w.write_all(&v.to_le_bytes())?;
        }
        self.offset += super::FOOTER_LEN as u64;
        self.w.flush()?;
        self.w.get_ref().sync_all()?;
        Ok(self.offset)
    }
}
