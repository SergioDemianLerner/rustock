//! Tests for the ascending header sync.
//!
//! The ones that matter are the three ways a peer can lie about uncle work,
//! because that is the whole reason this walk exists: it totals cumulative
//! difficulty *exactly* where the descending walk can only bound it.

use super::forward::{AscentError, ForwardSync, Want, HEADER_CHUNK};
use alloy_primitives::{Address, B256, U256};
use rustock_core::validation::{HeaderValidator, HeaderVerifier, ValidationError};
use rustock_core::Header;
use rustock_networking::protocol::{BlockIdentifier, HeaderWithUncles};
use rustock_storage::BlockStore;
use std::sync::Arc;

fn header(number: u64, parent: B256, difficulty: u64) -> Header {
    Header {
        parent_hash: parent,
        ommers_hash: rustock_execution::processor::compute_ommers_hash(&[]),
        beneficiary: Address::ZERO,
        state_root: B256::ZERO,
        transactions_root: rustock_core::ordered_tx_trie_root(&[], true),
        receipts_root: B256::ZERO,
        logs_bloom: Default::default(),
        extension_data: None,
        difficulty: U256::from(difficulty),
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

/// An entry whose header honestly commits to the uncles beside it.
fn with_uncles(mut h: Header, uncles: Vec<Header>) -> HeaderWithUncles {
    h.uncle_count = uncles.len() as u64;
    h.ommers_hash = rustock_execution::processor::compute_ommers_hash(&uncles);
    h.cached_hash = None;
    HeaderWithUncles { header: h, uncles }
}

/// `n` headers ascending from `parent`, returned newest-first as the wire
/// delivers them. `uncles_at` decides what each block references.
fn run_of(
    first_number: u64,
    n: usize,
    parent: B256,
    difficulty: u64,
    mut uncles_at: impl FnMut(u64) -> Vec<Header>,
) -> Vec<HeaderWithUncles> {
    let mut out = Vec::new();
    let mut prev = parent;
    for i in 0..n {
        let number = first_number + i as u64;
        let e = with_uncles(header(number, prev, difficulty), uncles_at(number));
        prev = e.header.hash();
        out.push(e);
    }
    out.reverse();
    out
}

fn sync(anchor: &Header, target: u64, verifier: HeaderVerifier) -> (ForwardSync, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Arc::new(BlockStore::open(dir.path()).expect("open"));
    (
        ForwardSync::new(
            anchor.number,
            anchor.hash(),
            U256::ZERO,
            target,
            store,
            Arc::new(verifier),
        ),
        dir,
    )
}

/// A clean ascent links and totals trunk *plus* uncle difficulty exactly.
#[test]
fn an_honest_run_advances_the_prefix_and_totals_the_uncles() {
    let anchor = header(0, B256::ZERO, 100);
    let (mut s, _d) = sync(&anchor, 4, HeaderVerifier::new());

    // Two uncles on block 2, worth 50 each.
    let entries = run_of(1, 4, anchor.hash(), 100, |n| {
        if n == 2 { vec![header(1, B256::repeat_byte(9), 50), header(1, B256::repeat_byte(8), 50)] }
        else { Vec::new() }
    });
    s.on_headers_with_uncles(4, &entries).expect("honest run");

    assert_eq!(s.frontier(), 4, "the prefix absorbs the whole run");
    assert!(s.done());
    assert_eq!(
        s.work(),
        U256::from(4 * 100 + 2 * 50),
        "cumulative difficulty counts the uncles, which is the point of the walk"
    );
}

/// Uncles the trunk header does not commit to are refused.
///
/// `ommers_hash` is inside what the header's proof of work covers, so a
/// mismatch means the peer altered the list. Without this check the difficulty
/// is whatever the peer says it is.
#[test]
fn uncles_the_header_does_not_commit_to_are_refused() {
    let anchor = header(0, B256::ZERO, 100);
    let (mut s, _d) = sync(&anchor, 2, HeaderVerifier::new());

    let mut entries = run_of(1, 2, anchor.hash(), 100, |_| Vec::new());
    // Bolt a fat uncle onto a header that commits to an empty list.
    entries[0].uncles.push(header(1, B256::repeat_byte(7), 1_000_000));

    assert!(
        matches!(
            s.on_headers_with_uncles(2, &entries),
            Err(AscentError::UncleCommitment { .. })
        ),
        "a forged uncle list must not reach the total"
    );
    assert_eq!(s.frontier(), 0, "and the prefix does not move");
}

/// Rejects a header whose difficulty marks it as having no proof of work, so
/// the uncle-proof path can be exercised without a merged-mining fixture.
struct RejectMarked;
impl HeaderValidator for RejectMarked {
    fn validate(&self, header: &Header) -> Result<(), ValidationError> {
        if header.difficulty == U256::from(666u64) {
            return Err(ValidationError::BitcoinPowInvalid {
                hash: header.hash(),
                target: U256::ZERO,
            });
        }
        Ok(())
    }
}

/// An uncle with a valid commitment but no valid proof of work is refused.
///
/// The commitment only proves the block's miner chose this list. What makes an
/// uncle's difficulty *work* is its own merged-mining proof, and a miner is
/// free to commit to an uncle that never had one.
#[test]
fn an_uncle_without_proof_of_work_is_refused() {
    let anchor = header(0, B256::ZERO, 100);
    let (mut s, _d) = sync(&anchor, 2, HeaderVerifier::new().with_static_rule(RejectMarked));

    let entries = run_of(1, 2, anchor.hash(), 100, |n| {
        if n == 1 { vec![header(0, B256::repeat_byte(5), 666)] } else { Vec::new() }
    });

    assert!(
        matches!(
            s.on_headers_with_uncles(2, &entries),
            Err(AscentError::UncleProofOfWork { .. })
        ),
        "a committed uncle still has to have done the work it claims"
    );
}

/// The same uncle counted under two blocks is refused.
///
/// Consensus forbids reuse; without the check one uncle's difficulty is
/// counted as many times as a miner cares to reference it.
#[test]
fn an_uncle_counted_twice_is_refused() {
    let anchor = header(0, B256::ZERO, 100);
    let (mut s, _d) = sync(&anchor, 3, HeaderVerifier::new());

    let twice = header(1, B256::repeat_byte(4), 500);
    let entries = run_of(1, 3, anchor.hash(), 100, |n| {
        if n == 2 || n == 3 { vec![twice.clone()] } else { Vec::new() }
    });

    assert!(
        matches!(s.on_headers_with_uncles(3, &entries), Err(AscentError::UncleReused { .. })),
        "one uncle, one contribution"
    );
}

/// Reuse is caught across runs too, not just inside one.
#[test]
fn an_uncle_counted_in_an_earlier_run_is_refused() {
    let anchor = header(0, B256::ZERO, 100);
    let (mut s, _d) = sync(&anchor, 4, HeaderVerifier::new());

    let shared = header(1, B256::repeat_byte(3), 500);
    let first = run_of(1, 2, anchor.hash(), 100, |n| {
        if n == 2 { vec![shared.clone()] } else { Vec::new() }
    });
    s.on_headers_with_uncles(2, &first).expect("first run is honest");
    assert_eq!(s.frontier(), 2);

    let top = first[0].header.hash();
    let second = run_of(3, 2, top, 100, |n| {
        if n == 4 { vec![shared.clone()] } else { Vec::new() }
    });
    assert!(
        matches!(s.on_headers_with_uncles(4, &second), Err(AscentError::UncleReused { .. })),
        "the ledger of counted uncles outlives the run that filled it"
    );
}

/// A run at the right height from the wrong chain does not move the prefix.
///
/// Height alone is not a link: a peer can answer at exactly the height wanted
/// with headers from somewhere else, and the parent hash is what catches it.
#[test]
fn a_run_from_another_chain_does_not_advance_the_prefix() {
    let anchor = header(0, B256::ZERO, 100);
    let (mut s, _d) = sync(&anchor, 4, HeaderVerifier::new());

    let elsewhere = run_of(1, 2, B256::repeat_byte(0xaa), 100, |_| Vec::new());
    s.on_headers_with_uncles(2, &elsewhere).expect("internally consistent");

    assert_eq!(s.frontier(), 0, "it links to nothing this node trusts");
    assert_eq!(s.work(), U256::ZERO, "and contributes no work");
}

/// Requests go out lowest-first and never reach below the proven prefix.
#[test]
fn requests_are_lowest_first_and_do_not_overlap_the_prefix() {
    let anchor = header(100, B256::ZERO, 100);
    let (mut s, _d) = sync(&anchor, 100 + 3 * HEADER_CHUNK, HeaderVerifier::new());

    s.on_skeleton(&[
        BlockIdentifier { number: 100 + 2 * HEADER_CHUNK, hash: B256::repeat_byte(2) },
        BlockIdentifier { number: 100 + HEADER_CHUNK, hash: B256::repeat_byte(1) },
    ]);

    let wants = s.wants(2);
    let points: Vec<u64> = wants
        .iter()
        .filter_map(|w| match w {
            Want::Headers { point, .. } => Some(*point),
            _ => None,
        })
        .collect();
    assert_eq!(
        points,
        vec![100 + HEADER_CHUNK, 100 + 2 * HEADER_CHUNK],
        "the prefix grows from the bottom, so the bottom is asked for first"
    );

    // The first chunk must stop exactly at the anchor, not run past it.
    match wants[0] {
        Want::Headers { count, .. } => assert_eq!(count as u64, HEADER_CHUNK),
        _ => panic!("expected headers"),
    }
}
