//! K-way merge over sorted sources. Sources earlier in the list are *newer*: when
//! several sources hold the same key, only the newest version is yielded.

use crate::Entry;
use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::io;

pub type Source = Box<dyn Iterator<Item = io::Result<Entry>> + Send>;

struct HeapItem {
    key: Vec<u8>,
    val: Option<Vec<u8>>,
    src: usize,
}

impl PartialEq for HeapItem {
    fn eq(&self, o: &Self) -> bool {
        self.key == o.key && self.src == o.src
    }
}
impl Eq for HeapItem {}
impl PartialOrd for HeapItem {
    fn partial_cmp(&self, o: &Self) -> Option<Ordering> {
        Some(self.cmp(o))
    }
}
impl Ord for HeapItem {
    // BinaryHeap is a max-heap; reverse so the smallest key (then newest source) pops first.
    fn cmp(&self, o: &Self) -> Ordering {
        o.key.cmp(&self.key).then(o.src.cmp(&self.src))
    }
}

pub struct MergeIter {
    sources: Vec<Source>,
    heap: BinaryHeap<HeapItem>,
    pending_err: Option<io::Error>,
}

impl MergeIter {
    pub fn new(sources: Vec<Source>) -> Self {
        let mut m = MergeIter {
            sources,
            heap: BinaryHeap::new(),
            pending_err: None,
        };
        for i in 0..m.sources.len() {
            m.refill(i);
        }
        m
    }

    fn refill(&mut self, i: usize) {
        match self.sources[i].next() {
            Some(Ok((key, val))) => self.heap.push(HeapItem { key, val, src: i }),
            Some(Err(e)) => self.pending_err = Some(e),
            None => {}
        }
    }
}

impl Iterator for MergeIter {
    type Item = io::Result<Entry>;

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(e) = self.pending_err.take() {
            return Some(Err(e));
        }
        let top = self.heap.pop()?;
        self.refill(top.src);
        while self.heap.peek().is_some_and(|p| p.key == top.key) {
            let shadowed = self.heap.pop().unwrap();
            self.refill(shadowed.src);
        }
        Some(Ok((top.key, top.val)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn src(items: &[(&str, Option<&str>)]) -> Source {
        let v: Vec<io::Result<Entry>> = items
            .iter()
            .map(|(k, v)| Ok((k.as_bytes().to_vec(), v.map(|s| s.as_bytes().to_vec()))))
            .collect();
        Box::new(v.into_iter())
    }

    #[test]
    fn sorted_output_newest_wins() {
        let newest = src(&[("b", Some("new")), ("d", None)]);
        let older = src(&[
            ("a", Some("1")),
            ("b", Some("old")),
            ("d", Some("4")),
            ("e", Some("5")),
        ]);
        let oldest = src(&[("b", Some("older")), ("c", Some("3"))]);
        let out: Vec<_> = MergeIter::new(vec![newest, older, oldest])
            .map(|r| r.unwrap())
            .collect();
        let keys: Vec<_> = out
            .iter()
            .map(|(k, _)| String::from_utf8(k.clone()).unwrap())
            .collect();
        assert_eq!(keys, ["a", "b", "c", "d", "e"]);
        assert_eq!(out[1].1, Some(b"new".to_vec()));
        assert_eq!(out[3].1, None); // tombstone from the newest source shadows the older value
    }

    #[test]
    fn propagates_errors() {
        let bad: Source = Box::new(vec![Err(io::Error::other("boom"))].into_iter());
        let mut m = MergeIter::new(vec![bad]);
        assert!(m.next().unwrap().is_err());
    }
}
