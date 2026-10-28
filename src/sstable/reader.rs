use super::block::{decode_block, get_varint, strip_crc};
use super::{FOOTER_LEN, MAGIC};
use crate::bloom::Bloom;
use crate::cache::{Block, BlockCache};
use crate::stats::{add, Stats};
use crate::{invalid, Entry};
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

struct IndexEntry {
    last_key: Vec<u8>,
    offset: u64,
    len: u32,
}

pub struct SstReader {
    pub id: u64,
    pub smallest: Vec<u8>,
    pub largest: Vec<u8>,
    pub file_size: u64,
    pub count: u64,
    path: PathBuf,
    file: Mutex<Option<File>>,
    index: Vec<IndexEntry>,
    bloom: Bloom,
    cache: Arc<BlockCache>,
    stats: Arc<Stats>,
    obsolete: AtomicBool,
}

fn read_range(f: &mut File, off: u64, len: usize) -> io::Result<Vec<u8>> {
    f.seek(SeekFrom::Start(off))?;
    let mut buf = vec![0u8; len];
    f.read_exact(&mut buf)?;
    Ok(buf)
}

impl SstReader {
    pub fn open(
        path: &Path,
        id: u64,
        cache: Arc<BlockCache>,
        stats: Arc<Stats>,
    ) -> io::Result<Self> {
        let mut f = File::open(path)?;
        let size = f.metadata()?.len();
        if size < FOOTER_LEN as u64 {
            return Err(invalid("sstable too small"));
        }
        let footer = read_range(&mut f, size - FOOTER_LEN as u64, FOOTER_LEN)?;
        let n = |i: usize| u64::from_le_bytes(footer[i * 8..i * 8 + 8].try_into().unwrap());
        let (index_off, index_len, bloom_off, bloom_len, count, magic) =
            (n(0), n(1), n(2), n(3), n(4), n(5));
        if magic != MAGIC {
            return Err(invalid("bad sstable magic"));
        }
        let idx_raw = read_range(&mut f, index_off, index_len as usize)?;
        let idx = strip_crc(&idx_raw)?;
        let mut index = Vec::new();
        let mut pos = 0;
        while pos < idx.len() {
            let klen = get_varint(idx, &mut pos)? as usize;
            if pos + klen + 12 > idx.len() {
                return Err(invalid("corrupt index"));
            }
            let last_key = idx[pos..pos + klen].to_vec();
            pos += klen;
            let offset = u64::from_le_bytes(idx[pos..pos + 8].try_into().unwrap());
            let len = u32::from_le_bytes(idx[pos + 8..pos + 12].try_into().unwrap());
            pos += 12;
            index.push(IndexEntry {
                last_key,
                offset,
                len,
            });
        }
        let bloom_raw = read_range(&mut f, bloom_off, bloom_len as usize)?;
        let bloom = Bloom::decode(strip_crc(&bloom_raw)?);
        let (first, last) = match (index.first(), index.last()) {
            (Some(a), Some(b)) => (a, b),
            _ => return Err(invalid("empty sstable")),
        };
        let first_block = decode_block(&read_range(&mut f, first.offset, first.len as usize)?)?;
        let smallest = first_block
            .first()
            .ok_or_else(|| invalid("empty block"))?
            .0
            .clone();
        let largest = last.last_key.clone();
        Ok(SstReader {
            id,
            smallest,
            largest,
            file_size: size,
            count,
            path: path.to_path_buf(),
            file: Mutex::new(Some(f)),
            index,
            bloom,
            cache,
            stats,
            obsolete: AtomicBool::new(false),
        })
    }

    /// Schedules the file for deletion once the last reference to this reader is dropped.
    pub fn mark_obsolete(&self) {
        self.obsolete.store(true, Ordering::SeqCst);
    }

    pub fn overlaps(&self, lo: &[u8], hi: &[u8]) -> bool {
        !(self.largest.as_slice() < lo || self.smallest.as_slice() > hi)
    }

    fn read_block(&self, i: usize, use_cache: bool) -> io::Result<Block> {
        if use_cache {
            if let Some(b) = self.cache.get((self.id, i)) {
                return Ok(b);
            }
        }
        let e = &self.index[i];
        let raw = {
            let mut g = self.file.lock().unwrap();
            let f = g.as_mut().ok_or_else(|| invalid("sstable closed"))?;
            read_range(f, e.offset, e.len as usize)?
        };
        add(&self.stats.block_reads, 1);
        let block = Arc::new(decode_block(&raw)?);
        if use_cache {
            self.cache.insert((self.id, i), block.clone());
        }
        Ok(block)
    }

    /// `Ok(None)` = not in this table, `Ok(Some(None))` = tombstone.
    pub fn get(&self, key: &[u8]) -> io::Result<Option<Option<Vec<u8>>>> {
        if key < self.smallest.as_slice() || key > self.largest.as_slice() {
            return Ok(None);
        }
        let has_bloom = !self.bloom.is_empty();
        if has_bloom {
            add(&self.stats.bloom_checks, 1);
            if !self.bloom.may_contain(key) {
                add(&self.stats.bloom_negatives, 1);
                return Ok(None);
            }
        }
        let bi = self.index.partition_point(|e| e.last_key.as_slice() < key);
        if bi >= self.index.len() {
            return Ok(None);
        }
        let block = self.read_block(bi, true)?;
        match block.binary_search_by(|e| e.0.as_slice().cmp(key)) {
            Ok(i) => Ok(Some(block[i].1.clone())),
            Err(_) => {
                if has_bloom {
                    add(&self.stats.bloom_false_positives, 1);
                }
                Ok(None)
            }
        }
    }

    /// Iterates entries with key >= `start` (all entries if `None`), one block at a time.
    pub fn iter_from(self: &Arc<Self>, start: Option<&[u8]>, use_cache: bool) -> SstIter {
        let block = start.map_or(0, |s| {
            self.index.partition_point(|e| e.last_key.as_slice() < s)
        });
        SstIter {
            r: self.clone(),
            block,
            entries: None,
            pos: 0,
            start: start.map(|s| s.to_vec()),
            use_cache,
        }
    }
}

impl Drop for SstReader {
    fn drop(&mut self) {
        if self.obsolete.load(Ordering::SeqCst) {
            // Close the handle first: Windows refuses to delete open files.
            self.file.lock().unwrap().take();
            let _ = fs::remove_file(&self.path);
        }
    }
}

pub struct SstIter {
    r: Arc<SstReader>,
    block: usize,
    entries: Option<Block>,
    pos: usize,
    start: Option<Vec<u8>>,
    use_cache: bool,
}

impl Iterator for SstIter {
    type Item = io::Result<Entry>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if self.entries.is_none() {
                if self.block >= self.r.index.len() {
                    return None;
                }
                match self.r.read_block(self.block, self.use_cache) {
                    Ok(b) => {
                        self.pos = self
                            .start
                            .take()
                            .map_or(0, |s| b.partition_point(|e| e.0 < s));
                        self.entries = Some(b);
                    }
                    Err(e) => {
                        self.block = usize::MAX; // stop after reporting the error
                        return Some(Err(e));
                    }
                }
            }
            let es = self.entries.as_ref().unwrap();
            if self.pos < es.len() {
                let e = es[self.pos].clone();
                self.pos += 1;
                return Some(Ok(e));
            }
            self.entries = None;
            self.block += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::SstBuilder;
    use super::*;

    #[test]
    fn build_read_iterate() {
        let dir = std::env::temp_dir().join(format!("lsm-sst-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("000001.sst");
        let mut b = SstBuilder::create(&path, 256, 10).unwrap();
        for i in 0..1000u32 {
            let k = format!("key{:05}", i * 2);
            if i % 10 == 0 {
                b.add(k.as_bytes(), None).unwrap();
            } else {
                b.add(k.as_bytes(), Some(format!("val{i}").as_bytes()))
                    .unwrap();
            }
        }
        b.finish().unwrap();
        let r = Arc::new(
            SstReader::open(
                &path,
                1,
                Arc::new(BlockCache::new(1 << 20)),
                Arc::new(Stats::default()),
            )
            .unwrap(),
        );
        assert_eq!(r.count, 1000);
        assert_eq!(r.smallest, b"key00000");
        assert_eq!(r.largest, b"key01998");
        assert_eq!(r.get(b"key00004").unwrap(), Some(Some(b"val2".to_vec())));
        assert_eq!(r.get(b"key00020").unwrap(), Some(None)); // tombstone
        assert_eq!(r.get(b"key00003").unwrap(), None); // absent (odd)
        assert_eq!(r.get(b"zzz").unwrap(), None);
        let all: Vec<_> = r.iter_from(None, false).map(|e| e.unwrap()).collect();
        assert_eq!(all.len(), 1000);
        let from: Vec<_> = r
            .iter_from(Some(b"key01001"), true)
            .map(|e| e.unwrap())
            .collect();
        assert_eq!(from[0].0, b"key01002");
        assert_eq!(from.len(), 499);
        r.mark_obsolete();
        drop(r);
        assert!(!path.exists());
        fs::remove_dir_all(&dir).unwrap();
    }
}
