//! Byte-bounded LRU cache of decoded SSTable blocks.

use crate::Entry;
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

pub type BlockKey = (u64, usize);
pub type Block = Arc<Vec<Entry>>;

struct Inner {
    map: HashMap<BlockKey, (Block, u64, usize)>,
    order: BTreeMap<u64, BlockKey>,
    tick: u64,
    used: usize,
}

pub struct BlockCache {
    inner: Mutex<Inner>,
    capacity: usize,
    hits: AtomicU64,
    misses: AtomicU64,
}

fn charge(b: &[Entry]) -> usize {
    b.iter()
        .map(|(k, v)| k.len() + v.as_ref().map_or(0, |v| v.len()) + 48)
        .sum()
}

impl BlockCache {
    pub fn new(capacity: usize) -> Self {
        BlockCache {
            inner: Mutex::new(Inner {
                map: HashMap::new(),
                order: BTreeMap::new(),
                tick: 0,
                used: 0,
            }),
            capacity,
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
        }
    }

    pub fn get(&self, key: BlockKey) -> Option<Block> {
        let mut g = self.inner.lock().unwrap();
        g.tick += 1;
        let tick = g.tick;
        let found = g.map.get_mut(&key).map(|e| {
            let old = e.1;
            e.1 = tick;
            (e.0.clone(), old)
        });
        match found {
            Some((b, old)) => {
                g.order.remove(&old);
                g.order.insert(tick, key);
                self.hits.fetch_add(1, Ordering::Relaxed);
                Some(b)
            }
            None => {
                self.misses.fetch_add(1, Ordering::Relaxed);
                None
            }
        }
    }

    pub fn insert(&self, key: BlockKey, block: Block) {
        let size = charge(&block);
        if size > self.capacity {
            return;
        }
        let mut g = self.inner.lock().unwrap();
        g.tick += 1;
        let tick = g.tick;
        if let Some((_, old_tick, old_size)) = g.map.insert(key, (block, tick, size)) {
            g.order.remove(&old_tick);
            g.used -= old_size;
        }
        g.order.insert(tick, key);
        g.used += size;
        while g.used > self.capacity {
            let Some((&t, &k)) = g.order.iter().next() else {
                break;
            };
            g.order.remove(&t);
            if let Some((_, _, sz)) = g.map.remove(&k) {
                g.used -= sz;
            }
        }
    }

    pub fn hits_misses(&self) -> (u64, u64) {
        (
            self.hits.load(Ordering::Relaxed),
            self.misses.load(Ordering::Relaxed),
        )
    }

    pub fn used(&self) -> usize {
        self.inner.lock().unwrap().used
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blk(n: usize) -> Block {
        Arc::new(vec![(vec![0u8; 10], Some(vec![0u8; n]))])
    }

    #[test]
    fn evicts_least_recently_used() {
        let one = charge(&blk(100));
        let c = BlockCache::new(one * 2 + 1);
        c.insert((1, 0), blk(100));
        c.insert((1, 1), blk(100));
        assert!(c.get((1, 0)).is_some()); // touch 0 so 1 is the LRU
        c.insert((1, 2), blk(100));
        assert!(c.get((1, 1)).is_none());
        assert!(c.get((1, 0)).is_some());
        assert!(c.get((1, 2)).is_some());
        assert!(c.used() <= one * 2 + 1);
    }
}
