//! Evidence that a peer is offering a chain this node cannot connect to.
//!
//! # What this is for
//!
//! A header arrives whose `parent_hash` names a block this node does not hold.
//! That is what a node on a minority chain sees from every peer: headers built
//! on the other chain's block.
//!
//! The instinct is to keep those headers so the chain can be assembled when the
//! missing parent turns up. It never turns up. A node asks for blocks *above
//! its own head*, so the other chain's fork block is never requested, and held
//! headers never link. Keeping them assembles nothing.
//!
//! What matters is noticing, and then going to look for where the two chains
//! diverge — which `FindingConnectionPoint` already does, and everything it
//! fetches afterwards is verified normally.
//!
//! So this holds **evidence, not chain**: hashes, counted per peer. Nothing
//! here is stored, weighted, eligible for the head, or served onward.
//!
//! # Why hashes, and why per peer
//!
//! The hashes distinguish repeats. A stalled node provokes the same batch
//! every few seconds, and without identity those repeats would inflate the
//! count until any threshold fired. That is their only purpose, together with
//! the lowest height, which says where to start probing. Nothing reads them to
//! assemble a chain or to judge one.
//!
//! Per peer, because a shared pool has no answerable owner: one peer's flood
//! displaces another's evidence and nobody is at fault for it. Here a peer
//! fills only its own quota, evicts only its own entries, and is the peer
//! searched against and charged.
//!
//! The bound is a count of slots, not anything the peer supplies. That
//! distinction matters: a header with no parent **declares its own
//! difficulty**, so a proof-of-work requirement would cost an attacker only
//! what it chose to claim. A slot quota cannot be argued with.

use alloy_primitives::{B256, B512};
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

/// Hashes retained per peer. At 32 bytes that is 2 KB each, 50 KB across a
/// 25-peer outbound target.
///
/// Generous for a distinctness counter — the decision needs far fewer — and
/// sized by what a burst from one peer looks like rather than by what the
/// trigger requires.
pub const MAX_UNLINKED_PER_PEER: usize = 64;

/// Distinct unlinked headers from one peer before a fork is suspected.
///
/// More than one, so a single out-of-order announcement does not provoke a
/// search. Small, because the evidence is conclusive quickly.
pub const UNLINKED_BEFORE_SEARCH: usize = 4;

/// How long the evidence must have been accumulating.
///
/// Without this a burst of four arriving together would trigger immediately,
/// which is the same mistake as a threshold of one wearing a larger number.
pub const UNLINKED_SETTLE: Duration = Duration::from_secs(10);

/// Peers tracked at once.
///
/// There is no disconnect signal reaching this code, so a peer's evidence is
/// not released when it goes away. Over months of churn that would grow
/// without limit, which is the same unbounded-growth problem as the per-peer
/// quota one level up. Well above the 25-peer outbound target, so a node with
/// a normal peer set never evicts; at 2 KB each the whole structure is capped
/// near 256 KB whatever the network does.
pub const MAX_TRACKED_PEERS: usize = 128;

/// How long a peer's evidence survives without anything new arriving.
///
/// A peer that stopped sending unlinkable headers has either been disconnected
/// or is now serving a chain we can follow. Either way its evidence is stale,
/// and stale evidence should not provoke a search later.
pub const EVIDENCE_TTL: Duration = Duration::from_secs(600);

/// Minimum spacing between searches provoked by one peer.
///
/// A search is ~23 requests in the worst case, so a peer able to provoke them
/// freely would hold a 23x amplifier.
pub const SEARCH_COOLDOWN: Duration = Duration::from_secs(60);

/// What one peer has shown us that we cannot connect to.
#[derive(Debug)]
struct PeerEvidence {
    seen: HashSet<B256>,
    /// Insertion order, with each hash's height, so the quota evicts the
    /// oldest rather than an arbitrary entry — a live fork's evidence should
    /// not be displaced by junk that arrived after it.
    ///
    /// The height rides along so that `lowest` can be recomputed when a slot
    /// is evicted. Without it `lowest` could only ever fall, and one header
    /// claiming height 0 would pin every later probe to a full-chain search
    /// long after the header itself was gone — a peer-supplied value setting
    /// this node's cost, which is the thing the slot quota exists to avoid.
    order: std::collections::VecDeque<(B256, u64)>,
    /// The lowest height among the slots currently held: where a probe should
    /// start looking. A hint, bounded by what is retained, not a guarantee
    /// that the fork is no deeper.
    lowest: u64,
    /// When the first arrived: what the settle window is measured from.
    since: Instant,
    /// When the last arrived: what staleness is measured from.
    last: Instant,
    /// When this peer last provoked a search, if ever.
    last_search: Option<Instant>,
}

impl PeerEvidence {
    fn new(now: Instant) -> Self {
        Self {
            seen: HashSet::new(),
            order: std::collections::VecDeque::new(),
            lowest: u64::MAX,
            since: now,
            last: now,
            last_search: None,
        }
    }
}

/// Per-peer evidence of chains this node cannot connect to.
#[derive(Debug, Default)]
pub struct UnlinkedHeaders {
    by_peer: HashMap<B512, PeerEvidence>,
}

/// What the evidence says should happen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Not enough yet, or too soon after the last search.
    Wait,
    /// This peer is offering a chain we cannot connect to. Look for the fork,
    /// starting near `from`.
    SearchForFork { from: u64 },
}

impl UnlinkedHeaders {
    pub fn new() -> Self {
        Self::default()
    }

    /// Records one header this node could not verify for want of a parent.
    ///
    /// Takes the hash and height only. The header itself is deliberately not
    /// accepted: nothing here should be able to grow into a store.
    pub fn record(&mut self, peer: B512, hash: B256, number: u64, now: Instant) {
        let entry = self.by_peer.entry(peer).or_insert_with(|| PeerEvidence::new(now));
        entry.last = now;
        if !entry.seen.insert(hash) {
            return; // a repeat is not new evidence
        }
        entry.order.push_back((hash, number));
        entry.lowest = entry.lowest.min(number);

        let mut evicted_the_lowest = false;
        while entry.order.len() > MAX_UNLINKED_PER_PEER {
            if let Some((old, old_number)) = entry.order.pop_front() {
                entry.seen.remove(&old);
                evicted_the_lowest |= old_number == entry.lowest;
            }
        }
        if evicted_the_lowest {
            // Only on eviction, and only when the departing slot held the
            // floor: a scan of 64 entries, not something the steady path pays.
            entry.lowest = entry.order.iter().map(|&(_, n)| n).min().unwrap_or(u64::MAX);
        }

        // After the insert, not before, or the bound is exceeded by the very
        // entry that triggered the check.
        self.expire(now);
    }

    /// Drops evidence that has gone stale, and keeps the number of tracked
    /// peers bounded even though nothing tells this code when one disconnects.
    fn expire(&mut self, now: Instant) {
        self.by_peer
            .retain(|_, e| now.duration_since(e.last) < EVIDENCE_TTL);

        while self.by_peer.len() > MAX_TRACKED_PEERS {
            // Evict the peer heard from least recently: the one whose evidence
            // is closest to expiring anyway.
            let Some(stalest) = self
                .by_peer
                .iter()
                .min_by_key(|(_, e)| e.last)
                .map(|(p, _)| *p)
            else {
                break;
            };
            self.by_peer.remove(&stalest);
        }
    }

    /// Whether this peer's evidence now justifies looking for a fork.
    ///
    /// Marks the search as taken, so a caller acting on this does not have to
    /// remember to.
    pub fn consider(&mut self, peer: &B512, now: Instant) -> Verdict {
        let Some(entry) = self.by_peer.get_mut(peer) else { return Verdict::Wait };
        if entry.seen.len() < UNLINKED_BEFORE_SEARCH {
            return Verdict::Wait;
        }
        if now.duration_since(entry.since) < UNLINKED_SETTLE {
            return Verdict::Wait;
        }
        if let Some(last) = entry.last_search {
            if now.duration_since(last) < SEARCH_COOLDOWN {
                return Verdict::Wait;
            }
        }
        entry.last_search = Some(now);
        Verdict::SearchForFork { from: entry.lowest }
    }

    /// Forgets everything from a peer. Called when its chain is resolved, and
    /// when it disconnects.
    pub fn clear_peer(&mut self, peer: &B512) {
        self.by_peer.remove(peer);
    }

    /// Whether this peer is still offering something unconnectable *after* a
    /// search already resolved its chain — which is a peer not serving what it
    /// agreed on, rather than a peer on a different fork.
    pub fn persisted_after_search(&self, peer: &B512) -> bool {
        self.by_peer
            .get(peer)
            .is_some_and(|e| e.last_search.is_some() && e.seen.len() >= UNLINKED_BEFORE_SEARCH)
    }

    /// Moves a peer's evidence back in time, so a test can reach the settle
    /// window without sleeping through it.
    #[cfg(test)]
    pub fn backdate(&mut self, peer: &B512, by: Duration) {
        if let Some(e) = self.by_peer.get_mut(peer) {
            e.since -= by;
        }
    }

    /// Hashes held for one peer.
    #[allow(dead_code)] // read by tests and worth a metric later
    pub fn len_for(&self, peer: &B512) -> usize {
        self.by_peer.get(peer).map_or(0, |e| e.seen.len())
    }

    /// Hashes held across every peer — the number the bound is on.
    #[allow(dead_code)]
    pub fn total(&self) -> usize {
        self.by_peer.values().map(|e| e.seen.len()).sum()
    }

    /// Peers with evidence against them.
    #[allow(dead_code)]
    pub fn peers(&self) -> usize {
        self.by_peer.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(n: u8) -> B512 {
        B512::repeat_byte(n)
    }
    fn hash(n: u64) -> B256 {
        B256::from(alloy_primitives::U256::from(n))
    }
    /// Far enough past that the settle window and any cooldown have elapsed.
    fn later(base: Instant, secs: u64) -> Instant {
        base + Duration::from_secs(secs)
    }

    // -- the trigger ------------------------------------------------------

    /// One stray header is not a fork. A single out-of-order announcement must
    /// not send the node looking.
    #[test]
    fn a_single_unlinked_header_triggers_nothing() {
        let t0 = Instant::now();
        let mut u = UnlinkedHeaders::new();
        u.record(peer(1), hash(1), 100, t0);
        assert_eq!(u.consider(&peer(1), later(t0, 60)), Verdict::Wait);
    }

    /// Enough distinct evidence, settled, means go and look.
    #[test]
    fn enough_settled_evidence_starts_a_search() {
        let t0 = Instant::now();
        let mut u = UnlinkedHeaders::new();
        for i in 0..UNLINKED_BEFORE_SEARCH as u64 {
            u.record(peer(1), hash(i), 100 + i, t0);
        }
        assert_eq!(
            u.consider(&peer(1), later(t0, 30)),
            Verdict::SearchForFork { from: 100 },
            "the probe should start at the lowest height seen"
        );
    }

    /// A burst arriving together is still a burst. Waiting is what separates
    /// four headers in one packet from four over ten seconds.
    #[test]
    fn a_burst_does_not_trigger_before_it_settles() {
        let t0 = Instant::now();
        let mut u = UnlinkedHeaders::new();
        for i in 0..20u64 {
            u.record(peer(1), hash(i), 100 + i, t0);
        }
        assert_eq!(
            u.consider(&peer(1), later(t0, 1)),
            Verdict::Wait,
            "twenty headers in one instant is one burst, not twenty reasons"
        );
        assert!(matches!(
            u.consider(&peer(1), later(t0, 30)),
            Verdict::SearchForFork { .. }
        ));
    }

    /// A peer cannot provoke searches faster than the cooldown: a search is
    /// ~23 requests, so free provocation would be an amplifier.
    #[test]
    fn a_peer_cannot_provoke_searches_faster_than_the_cooldown() {
        let t0 = Instant::now();
        let mut u = UnlinkedHeaders::new();
        for i in 0..20u64 {
            u.record(peer(1), hash(i), 100 + i, t0);
        }
        assert!(matches!(u.consider(&peer(1), later(t0, 30)), Verdict::SearchForFork { .. }));
        assert_eq!(
            u.consider(&peer(1), later(t0, 31)),
            Verdict::Wait,
            "a second search one second later is the amplifier this prevents"
        );
        assert!(
            matches!(u.consider(&peer(1), later(t0, 120)), Verdict::SearchForFork { .. }),
            "and it is a delay, not a ban"
        );
    }

    // -- bounds and attribution -------------------------------------------

    /// A peer flooding fills only its own quota.
    #[test]
    fn one_peers_flood_is_bounded_to_its_own_quota() {
        let t0 = Instant::now();
        let mut u = UnlinkedHeaders::new();
        for i in 0..(MAX_UNLINKED_PER_PEER as u64 * 10) {
            u.record(peer(1), hash(i), 1000 + i, t0);
        }
        assert_eq!(u.len_for(&peer(1)), MAX_UNLINKED_PER_PEER);
        assert_eq!(u.total(), MAX_UNLINKED_PER_PEER);
    }

    /// And cannot displace another peer's evidence. Under a shared pool this
    /// was precisely the attack: flood to evict a genuine fork's evidence,
    /// with nobody answerable for it.
    #[test]
    fn a_flood_cannot_displace_another_peers_evidence() {
        let t0 = Instant::now();
        let mut u = UnlinkedHeaders::new();

        // An honest peer reports a real fork.
        for i in 0..UNLINKED_BEFORE_SEARCH as u64 {
            u.record(peer(2), hash(90_000 + i), 500 + i, t0);
        }
        // A hostile one floods.
        for i in 0..(MAX_UNLINKED_PER_PEER as u64 * 10) {
            u.record(peer(1), hash(i), 1000 + i, t0);
        }

        assert_eq!(
            u.len_for(&peer(2)),
            UNLINKED_BEFORE_SEARCH,
            "the honest peer's evidence must survive the flood untouched"
        );
        assert!(matches!(
            u.consider(&peer(2), later(t0, 30)),
            Verdict::SearchForFork { from: 500 }
        ));
    }

    /// Total retention stays bounded across every peer.
    #[test]
    fn total_retention_is_bounded_across_peers() {
        let t0 = Instant::now();
        let mut u = UnlinkedHeaders::new();
        for p in 0..50u8 {
            for i in 0..(MAX_UNLINKED_PER_PEER as u64 * 3) {
                u.record(peer(p), hash(i + p as u64 * 100_000), 1000 + i, t0);
            }
        }
        assert_eq!(u.total(), u.peers() * MAX_UNLINKED_PER_PEER);
        assert!(u.total() <= 50 * MAX_UNLINKED_PER_PEER);
    }

    /// A disconnected peer's evidence is released.
    #[test]
    fn clearing_a_peer_releases_its_evidence() {
        let t0 = Instant::now();
        let mut u = UnlinkedHeaders::new();
        for i in 0..10u64 {
            u.record(peer(1), hash(i), 100 + i, t0);
        }
        u.clear_peer(&peer(1));
        assert_eq!(u.len_for(&peer(1)), 0);
        assert_eq!(u.total(), 0);
        assert_eq!(u.consider(&peer(1), later(t0, 60)), Verdict::Wait);
    }

    // -- shapes that must not break it ------------------------------------

    /// A stalled node provokes the same batch every few seconds. Repeats are
    /// not new evidence, or any threshold fires eventually on one header.
    #[test]
    fn repeats_are_not_new_evidence() {
        let t0 = Instant::now();
        let mut u = UnlinkedHeaders::new();
        for round in 0..500u64 {
            u.record(peer(1), hash(7), 100, later(t0, round));
        }
        assert_eq!(u.len_for(&peer(1)), 1);
        assert_eq!(
            u.consider(&peer(1), later(t0, 600)),
            Verdict::Wait,
            "one header re-sent five hundred times is still one header"
        );
    }

    /// Headers arriving newest-first must still report the lowest height, or
    /// the probe starts in the wrong place.
    #[test]
    fn descending_arrival_still_finds_the_lowest() {
        let t0 = Instant::now();
        let mut u = UnlinkedHeaders::new();
        for i in (0..10u64).rev() {
            u.record(peer(1), hash(i), 1000 + i, t0);
        }
        assert_eq!(
            u.consider(&peer(1), later(t0, 30)),
            Verdict::SearchForFork { from: 1000 }
        );
    }

    /// Siblings at one height are a real fork, and both are evidence.
    #[test]
    fn siblings_at_one_height_both_count() {
        let t0 = Instant::now();
        let mut u = UnlinkedHeaders::new();
        u.record(peer(1), hash(1), 500, t0);
        u.record(peer(1), hash(2), 500, t0);
        assert_eq!(u.len_for(&peer(1)), 2, "same height, different blocks");
    }

    /// Eviction takes the oldest, so a live fork is not displaced by junk that
    /// arrived after it.
    #[test]
    fn eviction_takes_the_oldest() {
        let t0 = Instant::now();
        let mut u = UnlinkedHeaders::new();
        u.record(peer(1), hash(999_999), 42, t0);
        for i in 0..(MAX_UNLINKED_PER_PEER as u64) {
            u.record(peer(1), hash(i), 1000 + i, t0);
        }
        assert_eq!(u.len_for(&peer(1)), MAX_UNLINKED_PER_PEER);
        assert_ne!(
            u.consider(&peer(1), later(t0, 30)),
            Verdict::SearchForFork { from: 42 },
            "the oldest entry should have been evicted, taking its height with it"
        );
    }

    /// Evidence continuing after a search resolved the peer's chain is a peer
    /// not serving what it agreed on.
    #[test]
    fn evidence_after_a_search_is_distinguishable() {
        let t0 = Instant::now();
        let mut u = UnlinkedHeaders::new();
        for i in 0..20u64 {
            u.record(peer(1), hash(i), 100 + i, t0);
        }
        assert!(!u.persisted_after_search(&peer(1)), "no search has happened yet");
        assert!(matches!(u.consider(&peer(1), later(t0, 30)), Verdict::SearchForFork { .. }));
        assert!(
            u.persisted_after_search(&peer(1)),
            "still unconnectable after its chain was resolved"
        );
    }

    /// A peer nobody has heard from is not evidence of anything.
    #[test]
    fn an_unknown_peer_yields_nothing() {
        let mut u = UnlinkedHeaders::new();
        assert_eq!(u.consider(&peer(9), Instant::now()), Verdict::Wait);
        assert_eq!(u.len_for(&peer(9)), 0);
        assert!(!u.persisted_after_search(&peer(9)));
    }
}

#[cfg(test)]
mod bounding_tests {
    use super::*;

    fn peer_n(n: u32) -> B512 {
        let mut b = [0u8; 64];
        b[..4].copy_from_slice(&n.to_be_bytes());
        B512::from(b)
    }
    fn hash(n: u64) -> B256 {
        B256::from(alloy_primitives::U256::from(n))
    }

    /// Nothing tells this code when a peer disconnects, so churn must not grow
    /// the structure without limit.
    #[test]
    fn peer_churn_does_not_grow_without_limit() {
        let t0 = Instant::now();
        let mut u = UnlinkedHeaders::new();
        // Ten thousand short-lived peers, each leaving evidence behind.
        for p in 0..10_000u32 {
            for i in 0..8u64 {
                u.record(peer_n(p), hash(p as u64 * 100 + i), 1000 + i, t0);
            }
        }
        assert!(
            u.peers() <= MAX_TRACKED_PEERS,
            "{} peers tracked after churn",
            u.peers()
        );
        assert!(u.total() <= MAX_TRACKED_PEERS * MAX_UNLINKED_PER_PEER);
    }

    /// A peer that stopped sending unlinkable headers should not provoke a
    /// search much later on evidence nothing has reinforced.
    #[test]
    fn stale_evidence_expires() {
        let t0 = Instant::now();
        let mut u = UnlinkedHeaders::new();
        for i in 0..UNLINKED_BEFORE_SEARCH as u64 {
            u.record(peer_n(1), hash(i), 100 + i, t0);
        }
        assert_eq!(u.len_for(&peer_n(1)), UNLINKED_BEFORE_SEARCH);

        // Something else happens long after, which is what drives expiry.
        let much_later = t0 + EVIDENCE_TTL + Duration::from_secs(1);
        u.record(peer_n(2), hash(9_999), 500, much_later);

        assert_eq!(u.len_for(&peer_n(1)), 0, "stale evidence should be gone");
        assert_eq!(
            u.consider(&peer_n(1), much_later),
            Verdict::Wait,
            "and must not provoke a search"
        );
    }

    /// Expiry must not throw away a peer that is still actively sending.
    #[test]
    fn active_evidence_survives_expiry() {
        let t0 = Instant::now();
        let mut u = UnlinkedHeaders::new();
        for round in 0..100u64 {
            // Well inside the TTL each time, so it is never stale.
            let t = t0 + Duration::from_secs(round * 60);
            u.record(peer_n(1), hash(round), 1000 + round, t);
            u.record(peer_n(2), hash(50_000 + round), 2000 + round, t);
        }
        let end = t0 + Duration::from_secs(99 * 60);
        assert!(u.len_for(&peer_n(1)) >= UNLINKED_BEFORE_SEARCH);
        assert!(matches!(
            u.consider(&peer_n(1), end),
            Verdict::SearchForFork { .. }
        ));
    }

    /// Eviction under churn must not be a way for one peer to clear another's
    /// evidence: the peer evicted is the stalest, and an active peer never is.
    #[test]
    fn churn_cannot_evict_an_active_peer() {
        let t0 = Instant::now();
        let mut u = UnlinkedHeaders::new();
        let victim = peer_n(u32::MAX);

        for round in 0..20u64 {
            let t = t0 + Duration::from_secs(round * 10);
            // The victim keeps reporting throughout.
            u.record(victim, hash(round), 500 + round, t);
            // A flood of fresh peer identities arrives alongside it.
            for p in 0..100u32 {
                u.record(peer_n(round as u32 * 1000 + p), hash(p as u64), 9_000, t);
            }
        }

        assert!(
            u.len_for(&victim) >= UNLINKED_BEFORE_SEARCH,
            "an actively-reporting peer was evicted by churn from others"
        );
        assert!(matches!(
            u.consider(&victim, t0 + Duration::from_secs(300)),
            Verdict::SearchForFork { .. }
        ));
    }
}
