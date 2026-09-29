//! A trie store that records what a block actually read.
//!
//! # Why
//!
//! A stateless verifier needs the nodes a block touches, plus the hashes of
//! the siblings along the way. Nothing in the node records that, and it cannot
//! be derived from the trie's shape: the answer depends on which accounts and
//! storage slots the block's transactions happen to reach, and on how much
//! their paths overlap.
//!
//! So measure it. Every `get` is recorded, distinctly -- a node read twice
//! travels once -- and the bytes are the encoded sizes of exactly those nodes,
//! which is what a witness would carry.
//!
//! # What this is not
//!
//! Not an accounting of a *minimal* witness. It counts every node the executor
//! resolved, which includes the interior nodes on the path to each value. That
//! is the right number for a witness that ships the subtrie, and an
//! over-count for one that ships only sibling hashes -- both are reported, so
//! the difference is visible rather than assumed.

use rustock_trie::TrieStore;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

#[derive(Debug, Default, Clone)]
pub struct ReadStats {
    /// Distinct keys resolved out of the store.
    pub distinct: u64,
    /// Total `get` calls, including repeats the cache would have served.
    pub calls: u64,
    /// Encoded bytes of the distinct nodes: what shipping them would cost.
    pub bytes: u64,
    /// Keys asked for that the store did not have.
    pub misses: u64,
}

/// Wraps a store, records its reads, and keeps writes in memory.
///
/// Writes are buffered rather than forwarded so this can sit over a store
/// opened read-only. Executing a block writes the state it produces; that
/// state has to be readable back within the block, but it must not reach the
/// disk of a database another process owns.
pub struct CountingTrieStore {
    inner: Arc<dyn TrieStore>,
    overlay: Mutex<HashMap<Vec<u8>, Vec<u8>>>,
    seen: Mutex<HashSet<Vec<u8>>>,
    stats: Mutex<ReadStats>,
}

impl CountingTrieStore {
    pub fn new(inner: Arc<dyn TrieStore>) -> Self {
        Self {
            inner,
            overlay: Mutex::new(HashMap::new()),
            seen: Mutex::new(HashSet::new()),
            stats: Mutex::new(ReadStats::default()),
        }
    }

    /// Drops what execution wrote. The overlay would otherwise grow across
    /// every block measured.
    pub fn clear_overlay(&self) {
        self.overlay.lock().unwrap().clear();
    }

    /// Returns what has been read since the last call, and starts again.
    ///
    /// Called at a block boundary, so each sample is one block's reads.
    pub fn take(&self) -> ReadStats {
        self.seen.lock().unwrap().clear();
        std::mem::take(&mut *self.stats.lock().unwrap())
    }
}

impl TrieStore for CountingTrieStore {
    fn get(&self, key: &[u8]) -> Option<Vec<u8>> {
        // Something this block wrote is not something it had to be given, so
        // it is not part of the witness and is not counted.
        if let Some(v) = self.overlay.lock().unwrap().get(key) {
            return Some(v.clone());
        }
        let value = self.inner.get(key);
        let mut stats = self.stats.lock().unwrap();
        stats.calls += 1;
        match &value {
            Some(v) => {
                if self.seen.lock().unwrap().insert(key.to_vec()) {
                    stats.distinct += 1;
                    stats.bytes += v.len() as u64;
                }
            }
            None => stats.misses += 1,
        }
        value
    }

    fn put(&self, key: &[u8], value: &[u8]) {
        self.overlay.lock().unwrap().insert(key.to_vec(), value.to_vec());
    }

    /// Deliberately nothing: the inner store may be read-only, and there is
    /// nothing here worth keeping anyway.
    fn flush(&self) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustock_trie::store::MemoryTrieStore;

    #[test]
    fn it_counts_distinct_reads_and_their_bytes() {
        let inner = Arc::new(MemoryTrieStore::new());
        inner.put(b"a", &[1u8; 10]);
        inner.put(b"b", &[2u8; 20]);
        let counting = CountingTrieStore::new(inner);

        counting.get(b"a");
        counting.get(b"a"); // a repeat travels once
        counting.get(b"b");
        counting.get(b"missing");

        let stats = counting.take();
        assert_eq!(stats.distinct, 2);
        assert_eq!(stats.calls, 4);
        assert_eq!(stats.bytes, 30);
        assert_eq!(stats.misses, 1);
    }

    /// What a block wrote, it was not given, so it must not be counted as
    /// read -- and it must not reach the inner store either.
    #[test]
    fn writes_stay_in_the_overlay_and_are_not_counted() {
        let inner = Arc::new(MemoryTrieStore::new());
        let counting = CountingTrieStore::new(inner.clone());

        counting.put(b"fresh", &[9u8; 40]);
        assert_eq!(counting.get(b"fresh"), Some(vec![9u8; 40]), "readable within the block");
        assert_eq!(inner.get(b"fresh"), None, "but never written through");

        let stats = counting.take();
        assert_eq!(stats.distinct, 0, "a node this block produced is not a node it needed");
        assert_eq!(stats.bytes, 0);
    }

    /// Each sample is one block, so taking the stats must reset the set too --
    /// otherwise the second block's repeats would be counted as free.
    #[test]
    fn taking_the_stats_starts_the_next_sample_clean() {
        let inner = Arc::new(MemoryTrieStore::new());
        inner.put(b"a", &[1u8; 10]);
        let counting = CountingTrieStore::new(inner);

        counting.get(b"a");
        assert_eq!(counting.take().distinct, 1);

        counting.get(b"a");
        let second = counting.take();
        assert_eq!(second.distinct, 1, "the same node in a later block is read again");
        assert_eq!(second.bytes, 10);
    }
}
