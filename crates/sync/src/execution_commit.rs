//! When executed state becomes durable, and when the head may say so.
//!
//! The executed head is a claim that the state it names is on disk. Trie
//! nodes are **derivable** — lose them and re-executing the block
//! recomputes them — so the store is allowed to flush less often than the
//! node executes. What it is not allowed to do is advance the head past
//! what a crash would keep: a restart then resumes above state it does not
//! have, and every read below it is a hole.
//!
//! That ordering was already respected at both call sites, by each of them
//! happening to flush first. This makes it structural: the head moves only
//! through [`ExecutionCommitter::after_block`], which runs the caller's
//! advance closure only after the flush has returned. A future call site
//! cannot get the order wrong without going out of its way.
//!
//! # On the cadence
//!
//! It is worth being precise about where flushing costs anything, because
//! it is easy to measure the wrong thing — `docs/flatdb.md` did.
//!
//! Catching up executes thousands of blocks back to back, so a flush per
//! block is paid thousands of times with nothing else happening in
//! between. That is the path this cadence is for, and it has batched at
//! 100 since before the setting existed.
//!
//! Following the chain executes one block every thirty seconds or so. A
//! flush there costs whatever it costs against a thirty-second budget, and
//! per-block durability is worth having, so follow mode forces one every
//! block.

use rustock_trie::TrieStore;

/// Blocks executed per durability point while catching up.
pub const DEFAULT_FLUSH_EVERY: u64 = 100;

pub struct ExecutionCommitter {
    every: u64,
    pending: u64,
}

impl ExecutionCommitter {
    pub fn new(every: u64) -> Self {
        Self { every: every.max(1), pending: 0 }
    }

    /// Resume with blocks already executed but not yet made durable,
    /// carried over from a previous batch.
    pub fn resuming(every: u64, pending: u64) -> Self {
        Self { every: every.max(1), pending }
    }

    /// Blocks executed since the last durability point.
    pub fn pending(&self) -> u64 {
        self.pending
    }

    /// Record that a block executed, and make it durable if it is time.
    ///
    /// Returns `None` when the block was executed but the head was left
    /// where it was, and `Some(advanced)` when this call flushed the trie
    /// and then ran `advance`.
    ///
    /// `advance` is a closure and not a value because that is what makes
    /// the ordering impossible to get wrong: it cannot run before the
    /// flush, because it does not exist until after it.
    pub fn after_block(
        &mut self,
        trie: &dyn TrieStore,
        force: bool,
        advance: impl FnOnce() -> bool,
    ) -> Option<bool> {
        self.pending += 1;
        if !force && self.pending < self.every {
            return None;
        }
        trie.flush();
        self.pending = 0;
        Some(advance())
    }

    /// Make everything executed so far durable and advance the head, if
    /// anything is waiting.
    ///
    /// Called when a batch ends: the next one may be a long time coming,
    /// and leaving executed blocks unrecorded would make the node redo
    /// them on restart for no reason.
    pub fn finish(
        &mut self,
        trie: &dyn TrieStore,
        advance: impl FnOnce() -> bool,
    ) -> Option<bool> {
        trie.flush();
        self.pending = 0;
        Some(advance())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Records what happened to it, in order.
    ///
    /// A `Mutex` rather than a `RefCell`: `TrieStore` is `Send + Sync`, and
    /// a test double that needed an `unsafe impl Sync` to satisfy that
    /// would be asserting something about the real trait rather than
    /// standing in for it.
    #[derive(Default)]
    struct Recorder {
        events: Mutex<Vec<&'static str>>,
    }

    impl Recorder {
        fn note(&self, what: &'static str) {
            self.events.lock().expect("recorder").push(what);
        }

        fn seen(&self) -> Vec<&'static str> {
            self.events.lock().expect("recorder").clone()
        }
    }

    impl TrieStore for Recorder {
        fn get(&self, _key: &[u8]) -> Option<Vec<u8>> {
            None
        }
        fn put(&self, _key: &[u8], _value: &[u8]) {
            self.note("put");
        }
        fn flush(&self) {
            self.note("flush");
        }
    }

    #[test]
    fn the_head_advances_only_at_a_durability_point() {
        let trie = Recorder::default();
        let mut committer = ExecutionCommitter::new(3);

        assert_eq!(committer.after_block(&trie, false, || true), None, "block 1");
        assert_eq!(committer.after_block(&trie, false, || true), None, "block 2");
        assert_eq!(committer.after_block(&trie, false, || true), Some(true), "block 3");
        assert_eq!(committer.pending(), 0, "the counter resets at the point");
        assert_eq!(committer.after_block(&trie, false, || true), None, "block 4");
    }

    /// The reason the API takes a closure: `advance` cannot run before the
    /// flush has returned, so no call site can invert the order.
    #[test]
    fn the_flush_always_precedes_the_advance() {
        let trie = Recorder::default();
        let mut committer = ExecutionCommitter::new(2);
        committer.after_block(&trie, false, || {
            trie.note("advance");
            true
        });
        committer.after_block(&trie, false, || {
            trie.note("advance");
            true
        });
        assert_eq!(trie.seen(), vec!["flush", "advance"]);
    }

    #[test]
    fn forcing_makes_every_block_a_durability_point() {
        let trie = Recorder::default();
        let mut committer = ExecutionCommitter::new(100);
        for i in 0..5 {
            assert_eq!(
                committer.after_block(&trie, true, || true),
                Some(true),
                "block {i} with force"
            );
        }
        assert_eq!(trie.seen(), vec!["flush"; 5]);
    }

    /// A failed advance is reported, not swallowed. The caller stops the
    /// batch on it, because executing further would build on a block the
    /// node is no longer on.
    #[test]
    fn a_refused_advance_is_reported() {
        let trie = Recorder::default();
        let mut committer = ExecutionCommitter::new(1);
        assert_eq!(committer.after_block(&trie, false, || false), Some(false));
    }

    #[test]
    fn finishing_flushes_whatever_is_pending() {
        let trie = Recorder::default();
        let mut committer = ExecutionCommitter::new(100);
        committer.after_block(&trie, false, || true);
        committer.after_block(&trie, false, || true);
        assert_eq!(committer.pending(), 2);

        assert_eq!(committer.finish(&trie, || true), Some(true));
        assert_eq!(committer.pending(), 0);
        assert_eq!(trie.seen(), vec!["flush"]);
    }

    /// The cadence survives a batch boundary: blocks executed but not made
    /// durable in one batch still count towards the next point.
    #[test]
    fn a_resumed_committer_keeps_counting() {
        let trie = Recorder::default();
        let mut committer = ExecutionCommitter::resuming(3, 2);
        assert_eq!(
            committer.after_block(&trie, false, || true),
            Some(true),
            "the third block overall, not the third of this batch"
        );
    }

    /// Zero would mean "never flush", which is not a cadence anyone can
    /// have meant, and would let the head run away from the disk.
    #[test]
    fn a_cadence_of_zero_is_read_as_every_block() {
        let trie = Recorder::default();
        let mut committer = ExecutionCommitter::new(0);
        assert_eq!(committer.after_block(&trie, false, || true), Some(true));
    }
}
