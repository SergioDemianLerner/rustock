//! Remembering which key signed which transaction.
//!
//! A transaction's sender is recovered from its signature, and the same
//! transaction has its sender recovered several times over its life: once when
//! the pool admits it, again when the block carrying it executes, again on
//! every RPC render, and again on each re-execution after a reorg. The
//! recovery is identical every time and it is the most expensive arithmetic
//! the node does per transaction.
//!
//! ## Keyed by the transaction hash, and why that is safe here
//!
//! The key is `Transaction::tx_hash()`. A cache keyed by a hash is only sound
//! if the hash separates every pair of transactions that could recover to
//! different senders — so in particular it must separate two transactions that
//! differ **only** in their `v` byte, since `v` selects both the signing hash
//! (legacy or EIP-155) and the recovery id. Two wire forms that hashed alike
//! but recovered differently would let the first one seen decide the sender of
//! the second.
//!
//! That separation holds, for two independent reasons, and
//! `the_hash_separates_transactions_that_differ_only_in_v` in
//! `rustock_core::types::transaction` pins it:
//!
//! - `v` is stored verbatim as a `u64` and re-encoded verbatim by
//!   `#[derive(RlpEncodable)]`. Nothing reconstructs it from a chain id, so
//!   the encode/decode round trip cannot fold two values onto one.
//! - `tx_hash()` goes through `rlp_for_trie()`, which prefers `cached_rlp` —
//!   the bytes as the peer sent them — and `Decodable::decode` always records
//!   those. Two different wire forms therefore hash differently whatever the
//!   fields decode to.
//!
//! Anything that changes either of those invalidates this cache, which is why
//! both are stated here rather than left to be rediscovered.
//!
//! ## The locking shape
//!
//! The obvious implementation — consult a `Mutex<HashMap>` once per
//! transaction — would serialise the one region `recover_senders` exists to
//! parallelise, and would do it invisibly: the work still happens, just one
//! thread at a time, with no error and nothing in the logs.
//!
//! So the lock is never taken from a worker. A block is looked up **once**,
//! before the threads start; the misses are recovered in parallel with no
//! cache access at all; and the results are written back **once**, after the
//! threads have joined. Two lock acquisitions per block, not one per
//! transaction.

use alloy_primitives::{Address, B256};
use lru::LruCache;
use std::num::NonZeroUsize;
use std::sync::Mutex;

/// How many senders to remember.
///
/// rskj keeps 6,000 in its `ReceivedTxSignatureCache`, described there as
/// "txs in three blocks". The same reasoning gives a different number here
/// because the bound that matters is the pool-to-execution hop: a transaction
/// admitted to the pool should still be remembered when the block carrying it
/// arrives. 8,192 covers a full pool twice over and costs about 400 KB.
pub const DEFAULT_CAPACITY: usize = 8_192;

/// Senders already recovered, keyed by transaction hash.
///
/// Cloneable handles share one cache: the node has a single pool feeding a
/// single executor, and a per-component cache would miss exactly the hop the
/// cache exists for.
#[derive(Debug)]
pub struct SenderCache {
    inner: Mutex<LruCache<B256, Address>>,
    hits: std::sync::atomic::AtomicU64,
    misses: std::sync::atomic::AtomicU64,
}

impl Default for SenderCache {
    fn default() -> Self {
        Self::new(DEFAULT_CAPACITY)
    }
}

impl SenderCache {
    pub fn new(capacity: usize) -> Self {
        let capacity = NonZeroUsize::new(capacity.max(1)).expect("max(1) is non-zero");
        Self {
            inner: Mutex::new(LruCache::new(capacity)),
            hits: std::sync::atomic::AtomicU64::new(0),
            misses: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Look up a whole block in one acquisition.
    ///
    /// Returns a slot per transaction, `Some` where the sender was already
    /// known. The caller recovers the `None`s itself; this type deliberately
    /// does not offer a one-transaction lookup, because offering one would
    /// invite calling it from inside the parallel region.
    pub fn get_many(&self, hashes: &[B256]) -> Vec<Option<Address>> {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let mut out = Vec::with_capacity(hashes.len());
        let (mut hit, mut miss) = (0u64, 0u64);
        for h in hashes {
            // `get` rather than `peek`: a hit is a use, and should keep the
            // entry alive. A transaction seen by the pool and then by
            // execution is exactly the entry worth keeping.
            match guard.get(h) {
                Some(a) => {
                    hit += 1;
                    out.push(Some(*a));
                }
                None => {
                    miss += 1;
                    out.push(None);
                }
            }
        }
        drop(guard);
        use std::sync::atomic::Ordering::Relaxed;
        self.hits.fetch_add(hit, Relaxed);
        self.misses.fetch_add(miss, Relaxed);
        out
    }

    /// Record what was recovered, in one acquisition.
    pub fn put_many(&self, entries: impl IntoIterator<Item = (B256, Address)>) {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        for (hash, address) in entries {
            guard.put(hash, address);
        }
    }

    /// Hits and misses since start, for the metrics endpoint.
    pub fn stats(&self) -> (u64, u64) {
        use std::sync::atomic::Ordering::Relaxed;
        (self.hits.load(Relaxed), self.misses.load(Relaxed))
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.inner.lock().unwrap().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(b: u8) -> Address {
        Address::repeat_byte(b)
    }
    fn hash(b: u8) -> B256 {
        B256::repeat_byte(b)
    }

    #[test]
    fn a_miss_is_reported_per_slot_and_in_order() {
        let c = SenderCache::new(8);
        c.put_many([(hash(2), addr(0x22))]);
        let got = c.get_many(&[hash(1), hash(2), hash(3)]);
        assert_eq!(got, vec![None, Some(addr(0x22)), None]);
    }

    #[test]
    fn what_is_written_is_what_is_read_back() {
        let c = SenderCache::new(8);
        c.put_many([(hash(1), addr(0x11)), (hash(2), addr(0x22))]);
        assert_eq!(c.get_many(&[hash(1), hash(2)]), vec![Some(addr(0x11)), Some(addr(0x22))]);
    }

    /// The bound has to hold, or a busy node grows without limit.
    #[test]
    fn the_oldest_entry_is_evicted_at_capacity() {
        let c = SenderCache::new(2);
        c.put_many([(hash(1), addr(1)), (hash(2), addr(2)), (hash(3), addr(3))]);
        assert_eq!(c.len(), 2);
        assert_eq!(c.get_many(&[hash(1)]), vec![None], "the oldest should have gone");
        assert_eq!(c.get_many(&[hash(3)]), vec![Some(addr(3))]);
    }

    /// A hit refreshes the entry, so the pool-to-execution hop survives a
    /// burst of unrelated traffic.
    #[test]
    fn a_hit_keeps_the_entry_alive() {
        let c = SenderCache::new(2);
        c.put_many([(hash(1), addr(1)), (hash(2), addr(2))]);
        assert_eq!(c.get_many(&[hash(1)]), vec![Some(addr(1))]); // refresh 1
        c.put_many([(hash(3), addr(3))]); // evicts the least recently used
        assert_eq!(c.get_many(&[hash(1)]), vec![Some(addr(1))], "1 was just used");
        assert_eq!(c.get_many(&[hash(2)]), vec![None], "2 was the stale one");
    }

    #[test]
    fn hits_and_misses_are_counted() {
        let c = SenderCache::new(8);
        c.put_many([(hash(1), addr(1))]);
        c.get_many(&[hash(1), hash(2), hash(3)]);
        assert_eq!(c.stats(), (1, 2));
    }

    /// A panic while a caller held the lock must not take the node down with
    /// it; a sender cache is an optimisation and has no business being fatal.
    #[test]
    fn a_poisoned_lock_is_recovered_from() {
        let c = std::sync::Arc::new(SenderCache::new(8));
        let c2 = c.clone();
        let _ = std::thread::spawn(move || {
            let _g = c2.inner.lock().unwrap();
            panic!("holder panics");
        })
        .join();
        c.put_many([(hash(9), addr(9))]);
        assert_eq!(c.get_many(&[hash(9)]), vec![Some(addr(9))]);
    }
}
