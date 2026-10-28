use crate::Entry;
use std::collections::BTreeMap;
use std::ops::Bound;

/// In-memory sorted table; one (latest) value per key, `None` is a tombstone.
#[derive(Default)]
pub struct Memtable {
    map: BTreeMap<Vec<u8>, Option<Vec<u8>>>,
    size: usize,
}

impl Memtable {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn apply(&mut self, key: Vec<u8>, val: Option<Vec<u8>>) {
        let new_len = val.as_ref().map_or(0, |v| v.len());
        let fresh = key.len() + new_len + 16;
        match self.map.insert(key, val) {
            Some(old) => {
                self.size += new_len;
                self.size = self.size.saturating_sub(old.map_or(0, |v| v.len()));
            }
            None => self.size += fresh,
        }
    }

    /// `None` = key unknown here; `Some(None)` = deleted here.
    pub fn get(&self, key: &[u8]) -> Option<&Option<Vec<u8>>> {
        self.map.get(key)
    }

    pub fn size(&self) -> usize {
        self.size
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = Entry> + '_ {
        self.map.iter().map(|(k, v)| (k.clone(), v.clone()))
    }

    pub fn range_from(&self, start: &[u8]) -> Vec<Entry> {
        self.map
            .range::<[u8], _>((Bound::Included(start), Bound::Unbounded))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }
}
