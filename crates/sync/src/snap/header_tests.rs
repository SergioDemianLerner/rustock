//! The pipelined header walk, and what it refuses.
//!
//! The walk is what makes a checkpoint worth downloading state for, so these
//! are written from the attacker's side: a skeleton that points at the wrong
//! chain, a run that does not link, a chain back to a genesis nobody has seen.

use super::headers::{HeaderWalk, Want, WalkError, HEADER_CHUNK};
use alloy_primitives::{Address, B256, U256};
use rustock_core::validation::{HeaderValidator, HeaderVerifier, ValidationError};
use rustock_core::Header;
use rustock_networking::protocol::BlockIdentifier;
use rustock_storage::BlockStore;
use std::sync::Arc;

fn header(number: u64, parent: B256) -> Header {
    Header {
        parent_hash: parent,
        ommers_hash: B256::ZERO,
        beneficiary: Address::ZERO,
        state_root: B256::repeat_byte(4),
        transactions_root: B256::ZERO,
        receipts_root: B256::ZERO,
        logs_bloom: Default::default(),
        extension_data: None,
        difficulty: U256::from(100u64),
        number,
        gas_limit: U256::from(6_800_000u64),
        gas_used: 0,
        timestamp: 1_700_000_000 + number,
        extra_data: Default::default(),
        paid_fees: U256::ZERO,
        minimum_gas_price: U256::ZERO,
        uncle_count: 0,
        umm_root: None,
        bitcoin_merged_mining_header: None,
        bitcoin_merged_mining_merkle_proof: None,
        bitcoin_merged_mining_coinbase_transaction: None,
        cached_hash: None,
        cached_hash_for_merged_mining: None,
    }
}

/// A chain of `n+1` headers, 0..=n, each linked to the last.
fn chain(n: u64) -> Vec<Header> {
    let mut out = vec![header(0, B256::ZERO)];
    for i in 1..=n {
        let parent = out[(i - 1) as usize].hash();
        out.push(header(i, parent));
    }
    out
}

struct Fixture {
    store: Arc<BlockStore>,
    chain: Vec<Header>,
    _dir: tempfile::TempDir,
}

/// A node that knows only its genesis, and a chain it has never seen.
fn fixture(n: u64) -> Fixture {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Arc::new(BlockStore::open(dir.path()).expect("open"));
    let chain = chain(n);

    let genesis = &chain[0];
    store.put_header_with_hash(genesis.hash(), genesis).expect("genesis");
    store.put_canonical_hash(0, genesis.hash()).expect("index");

    Fixture { store, chain, _dir: dir }
}

impl Fixture {
    fn walk(&self, top: u64, verifier: HeaderVerifier) -> HeaderWalk {
        HeaderWalk::new(
            self.chain[top as usize].clone(),
            self.store.clone(),
            Arc::new(verifier),
        )
    }

    /// What an honest peer answers a skeleton request with.
    fn skeleton(&self, start: u64) -> Vec<BlockIdentifier> {
        let mut out = Vec::new();
        let mut n = (start / HEADER_CHUNK) * HEADER_CHUNK;
        for _ in 0..20 {
            if n as usize >= self.chain.len() {
                break;
            }
            out.push(BlockIdentifier { hash: self.chain[n as usize].hash(), number: n });
            n += HEADER_CHUNK;
        }
        out
    }

    /// What an honest peer answers a header request with: newest first.
    fn headers(&self, point: u64, count: u32) -> Vec<Header> {
        let mut out = Vec::new();
        let mut n = point as i64;
        for _ in 0..count {
            if n < 0 {
                break;
            }
            out.push(self.chain[n as usize].clone());
            n -= 1;
        }
        out
    }

    /// Drive a walk to completion against honest answers, returning how many
    /// requests it took and the most it ever had outstanding.
    fn run(&self, walk: &mut HeaderWalk, budget: usize) -> (usize, usize) {
        let mut requests = 0;
        let mut widest = 0;
        for _ in 0..10_000 {
            if walk.is_done() {
                break;
            }
            let wants = walk.wants(budget);
            if wants.is_empty() {
                break;
            }
            widest = widest.max(wants.len());
            requests += wants.len();
            for want in wants {
                match want {
                    Want::Skeleton { start } => walk.on_skeleton(&self.skeleton(start)),
                    Want::Headers { point, count, .. } => {
                        walk.on_headers(point, &self.headers(point, count), &Default::default()).expect("honest")
                    }
                }
            }
            walk.advance().expect("honest answers link");
        }
        (requests, widest)
    }
}

#[test]
fn an_honest_walk_reaches_our_genesis() {
    for top in [1u64, 191, 192, 193, 500, 2000] {
        let f = fixture(top);
        let mut walk = f.walk(top, HeaderVerifier::new());
        f.run(&mut walk, 8);
        assert!(walk.is_done(), "top={top}: the walk did not anchor");
        assert_eq!(walk.frontier(), 0, "top={top}: did not reach genesis");
    }
}

/// **The point of the change.** More than one request goes out at a time,
/// which the serial walk could never do.
#[test]
fn the_walk_asks_for_many_things_at_once() {
    let f = fixture(4000);
    let mut walk = f.walk(4000, HeaderVerifier::new());
    let (_, widest) = f.run(&mut walk, 8);
    assert!(walk.is_done());
    assert!(widest >= 8, "only {widest} requests were ever in flight at once");
}

/// **A skeleton identifier is never believed.** It says where to ask; a run
/// that does not link is discarded however confidently it was pointed at.
///
/// The fork is offered at exactly the height the walk is waiting for, so this
/// tests the rejection rather than the walk merely not having got there.
#[test]
fn a_skeleton_pointing_at_another_chain_does_not_move_the_walk() {
    let f = fixture(1000);

    // A different chain of the same shape: different genesis, so every hash
    // below differs, but each run is internally perfect.
    let mut forked = chain(1000);
    forked[0] = header(0, B256::repeat_byte(0x99));
    for i in 1..=1000usize {
        let parent = forked[i - 1].hash();
        forked[i] = header(i as u64, parent);
    }
    assert_ne!(forked[960].hash(), f.chain[960].hash());

    let mut walk = f.walk(1000, HeaderVerifier::new());

    // Take the honest run above the grid, so the walk is now waiting for the
    // header at 960 and naming the hash it must have.
    walk.on_headers(1000, &f.headers(1000, (1000 - 960) as u32), &Default::default()).expect("honest");
    walk.advance().expect("links");
    assert_eq!(walk.frontier(), 960, "the walk should be waiting at the grid top");

    // Now answer 960 from the fork: right height, wrong chain.
    let run: Vec<Header> = (769..=960).rev().map(|i| forked[i as usize].clone()).collect();
    walk.on_headers(960, &run, &Default::default()).expect("internally valid");
    walk.advance().expect("no error, just no progress");

    assert_eq!(walk.frontier(), 960, "a foreign run moved the walk");
    assert!(!walk.is_done());

    // And the honest run for the same height is still accepted afterwards.
    walk.on_headers(960, &f.headers(960, HEADER_CHUNK as u32), &Default::default()).expect("honest");
    walk.advance().expect("links");
    assert_eq!(walk.frontier(), 768, "the honest run was not taken after the fork");
}

/// A run whose headers do not link to each other is refused outright.
#[test]
fn a_run_that_is_not_a_chain_is_refused() {
    let f = fixture(500);
    let mut walk = f.walk(500, HeaderVerifier::new());

    let mut run = f.headers(384, 192);
    run[5] = header(379, B256::repeat_byte(0x55));

    assert!(matches!(
        walk.on_headers(384, &run, &Default::default()),
        Err(WalkError::BrokenChunk { point: 384 })
    ));
}

/// Refuses everything, standing in for proof of work failing.
struct RefuseEverything;

impl HeaderValidator for RefuseEverything {
    fn validate(&self, _header: &Header) -> Result<(), ValidationError> {
        Err(ValidationError::BitcoinPowInvalid { hash: B256::ZERO, target: U256::ZERO })
    }
}

/// Every header still faces the full consensus rules. Pipelining changes when
/// the questions are asked, not which answers are accepted.
#[test]
fn every_header_is_still_validated() {
    let f = fixture(500);
    let mut walk = f.walk(500, HeaderVerifier::new().with_static_rule(RefuseEverything));

    assert!(matches!(
        walk.on_headers(384, &f.headers(384, 192), &Default::default()),
        Err(WalkError::InvalidHeader { .. })
    ));
}

/// A chain that links all the way down to a genesis this node has never seen
/// is refused, not accepted for having reached the bottom.
#[test]
fn a_foreign_genesis_is_refused() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Arc::new(BlockStore::open(dir.path()).expect("open"));
    // This node has a genesis, but not the one the chain leads to.
    let ours = header(0, B256::repeat_byte(0x11));
    store.put_header_with_hash(ours.hash(), &ours).expect("genesis");
    store.put_canonical_hash(0, ours.hash()).expect("index");

    let theirs = chain(300);
    let f = Fixture { store: store.clone(), chain: theirs, _dir: dir };
    let mut walk = f.walk(300, HeaderVerifier::new());

    let mut error = None;
    for _ in 0..200 {
        if walk.is_done() {
            break;
        }
        let wants = walk.wants(8);
        if wants.is_empty() {
            break;
        }
        for want in wants {
            match want {
                Want::Skeleton { start } => walk.on_skeleton(&f.skeleton(start)),
                Want::Headers { point, count, .. } => {
                    walk.on_headers(point, &f.headers(point, count), &Default::default()).expect("valid")
                }
            }
        }
        if let Err(e) = walk.advance() {
            error = Some(e);
            break;
        }
    }

    assert_eq!(error, Some(WalkError::ForeignGenesis), "walked into another network");
    assert!(!walk.is_done());
}

/// A released request comes back to be asked again, so a peer that never
/// answers costs a retry rather than the walk.
#[test]
fn a_released_request_is_asked_again() {
    let f = fixture(2000);
    let mut walk = f.walk(2000, HeaderVerifier::new());

    let first = walk.wants(4);
    assert!(!first.is_empty());
    for want in &first {
        walk.release(want);
    }
    let again = walk.wants(4);
    assert_eq!(again.len(), first.len(), "released work was not offered again");
}

/// Answers may arrive in any order; the walk links what it can and waits for
/// the rest.
#[test]
fn answers_may_arrive_out_of_order() {
    let f = fixture(1000);
    let mut walk = f.walk(1000, HeaderVerifier::new());

    // Learn every point first.
    for start in (0..=960).step_by((HEADER_CHUNK * 20) as usize) {
        walk.on_skeleton(&f.skeleton(start as u64));
    }
    walk.wants(64);

    // Feed the runs bottom-up, the opposite of the order the walk needs.
    let mut points: Vec<u64> = (0..=1000).step_by(HEADER_CHUNK as usize).collect();
    points.push(1000);
    points.sort_unstable();
    for point in points {
        if point == 0 {
            continue;
        }
        let count = if point == 1000 { 1000 - 960 } else { HEADER_CHUNK };
        let _ = walk.on_headers(point, &f.headers(point, count as u32), &Default::default());
    }
    walk.advance().expect("links");

    assert!(walk.is_done(), "out-of-order answers did not assemble");
}
/// The walk stops at the first block this node already holds, which on a node
/// that already has part of the chain is not genesis.
///
/// `walked_difficulty` is then the work of the walked *segment* only. The
/// claim it gets checked against -- a peer's cumulative difficulty at the
/// checkpoint -- is measured from genesis, so the two are different quantities
/// and comparing them directly refuses an honest peer by everything this node
/// already had below the anchor. On a real node that is nearly the whole chain.
#[test]
fn the_established_difficulty_counts_from_genesis_not_from_the_anchor() {
    let f = fixture(1000);

    // This node already holds the chain up to the grid point at #768, with the
    // total difficulties it established for it.
    let anchor = 768u64;
    let mut td = U256::from(100u64); // genesis
    for i in 1..=anchor as usize {
        let h = &f.chain[i];
        td += h.difficulty;
        f.store.put_header_with_hash(h.hash(), h).expect("header");
        f.store.put_canonical_hash(h.number, h.hash()).expect("index");
        f.store.put_total_difficulty(h.hash(), td).expect("td");
    }
    let anchor_td = td;

    let mut walk = f.walk(1000, HeaderVerifier::new());
    assert_eq!(walk.established_difficulty(), None, "nothing is established yet");

    walk.on_headers(1000, &f.headers(1000, (1000 - 960) as u32), &Default::default()).expect("honest");
    walk.advance().expect("links");
    walk.on_headers(960, &f.headers(960, HEADER_CHUNK as u32), &Default::default()).expect("honest");
    walk.advance().expect("links");

    assert!(walk.is_done(), "the walk should have anchored at #{anchor}");

    let walked = walk.walked_difficulty();
    let established = walk.established_difficulty().expect("anchored");

    assert_eq!(
        established,
        anchor_td + walked,
        "established work must include what this node already had at the anchor"
    );
    assert!(
        established > walked * U256::from(3),
        "the segment walked ({walked}) is a small part of the work established \
         ({established}); using it alone would refuse an honest peer"
    );
}
