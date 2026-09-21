//! Tests for merged mining.
//!
//! The load-bearing ones are the round trip (build a template, mine it, submit
//! it, have it accepted) and the replay (a template, put back through
//! `process_block`, validates against every root it filled in). Between them
//! they check the whole path with the same code a peer would use, rather than
//! checking the builder against a restatement of what the builder does.

use alloy_primitives::{Address, B256, Bytes, U256, address, b256};
use bitcoin::hashes::Hash as _;
use rustock_core::config::{ActivationHeights, ChainConfig};
use rustock_core::validation::merged_mining::{compress_coinbase, compute_coinbase_hash};
use rustock_core::validation::{HeaderValidator, MergedMiningRule};
use rustock_core::Header;
use rustock_storage::BlockStore;
use rustock_trie::{MemoryTrieStore, TrieNode, TrieStore};
use std::sync::Arc;

use super::*;
use crate::hardfork::RskHardforkConfig;
use crate::processor::BlockProcessor;

// ── fixtures ──────────────────────────────────────────────────────────

/// Activation heights with merged mining live from block 0 and UMM never, so
/// tests can work at low block numbers. `papyrus200` is raised out of reach
/// deliberately: a separate test covers the `ummRoot` a real chain carries.
fn test_chain_config() -> Arc<ChainConfig> {
    Arc::new(ChainConfig {
        chain_id: 33,
        network_id: 7771,
        duration_limit: 10,
        difficulty_divisor: U256::from(2048),
        min_difficulty: U256::from(1),
        max_future_block_time: 0,
        gas_limit_bound_divisor: 1024,
        min_gas_limit: 1,
        max_gas_limit: 10_000_000,
        activation_heights: ActivationHeights {
            orchid: 0,
            wasabi100: 0,
            papyrus200: u64::MAX,
        },
    })
}

fn header_at(number: u64, parent_hash: B256, state_root: B256) -> Header {
    Header {
        parent_hash,
        ommers_hash: b256!("1dcc4de8dec75d7aab85b567b6ccd41ad312451b948a7413f0a142fd40d49347"),
        beneficiary: Address::repeat_byte(0x11),
        state_root,
        transactions_root: B256::ZERO,
        receipts_root: B256::ZERO,
        logs_bloom: Default::default(),
        extension_data: None,
        // Small enough that every Bitcoin hash clears the target, so the tests
        // never have to search for a nonce.
        difficulty: U256::from(2),
        number,
        gas_limit: U256::from(6_800_000),
        gas_used: 0,
        timestamp: 1_700_000_000 + number,
        extra_data: Bytes::new(),
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

/// A node with a short chain already in its store, ready to mine on top.
struct MiningFixture {
    server: MinerServer,
    store: Arc<BlockStore>,
    trie_store: Arc<dyn TrieStore>,
    chain_config: Arc<ChainConfig>,
    parent: Header,
    _dir: tempfile::TempDir,
}

impl MiningFixture {
    /// The template the miner would hand out right now.
    fn build(&self) -> Result<BlockTemplate, TemplateError> {
        self.build_on(&self.parent)
    }

    /// A template on an arbitrary parent, for the cases that need a parent the
    /// store does not have (a timestamp far in the future, say).
    fn build_on(&self, parent: &Header) -> Result<BlockTemplate, TemplateError> {
        self.server
            .builder_for_test()
            .build(std::slice::from_ref(parent), Vec::new(), &NoPendingTransactions)
    }
}

/// Builds a chain of `height` empty headers, all canonical, and a miner on
/// top of it.
///
/// The chain is not decorative: REMASC reaches back `maturity` blocks for the
/// block whose fees it distributes, so a template built over an empty store
/// fails inside execution with an error that says nothing about mining.
fn fixture(height: u64) -> MiningFixture {
    fixture_with(height, test_chain_config(), U256::from(2))
}

fn fixture_with_papyrus_at(papyrus200: u64, height: u64) -> MiningFixture {
    let mut config = (*test_chain_config()).clone();
    config.activation_heights.papyrus200 = papyrus200;
    fixture_with(height, Arc::new(config), U256::from(2))
}

fn fixture_with_wasabi_at(wasabi100: u64, height: u64) -> MiningFixture {
    let mut config = (*test_chain_config()).clone();
    config.activation_heights.wasabi100 = wasabi100;
    fixture_with(height, Arc::new(config), U256::from(2))
}

/// A chain whose difficulty is so high that no Bitcoin block clears the
/// target, for checking that the node refuses a solution that misses.
fn fixture_with_difficulty(difficulty: U256, height: u64) -> MiningFixture {
    let mut config = (*test_chain_config()).clone();
    // The difficulty rule clamps up to the minimum, which is how the built
    // header ends up carrying it.
    config.min_difficulty = difficulty;
    fixture_with(height, Arc::new(config), difficulty)
}

fn fixture_with(
    height: u64,
    chain_config: Arc<ChainConfig>,
    parent_difficulty: U256,
) -> MiningFixture {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(BlockStore::open(dir.path()).unwrap());
    let trie_store: Arc<dyn TrieStore> = Arc::new(MemoryTrieStore::new());
    let empty_root = TrieNode::empty().compute_hash(trie_store.as_ref());

    let mut parent_hash = B256::ZERO;
    let mut parent = header_at(0, parent_hash, empty_root);
    for number in 0..=height {
        let mut header = header_at(number, parent_hash, empty_root);
        header.difficulty = parent_difficulty;
        let hash = header.hash();
        store.put_header_with_hash(hash, &header).unwrap();
        store.put_canonical_hash(number, hash).unwrap();
        // Not `difficulty * height`: the difficulty-refusal fixture uses
        // U256::MAX, and multiplying that overflows.
        store
            .put_total_difficulty(hash, U256::from(number + 1))
            .unwrap();
        parent_hash = hash;
        parent = header;
    }
    store.set_head(parent_hash).unwrap();
    store.set_exec_head(parent_hash, empty_root).unwrap();

    let hardfork_cfg = RskHardforkConfig::all_active(33);
    let builder = BlockTemplateBuilder::new(
        Arc::new(BlockProcessor::new(hardfork_cfg.clone(), store.clone())),
        trie_store.clone(),
        chain_config.clone(),
        hardfork_cfg,
        MiningConfig {
            coinbase_address: address!("00000000000000000000000000000000c0117b45"),
            ..Default::default()
        },
    );
    let chain = Arc::new(StoreChainAccess {
        store: store.clone(),
        trie_store: trie_store.clone(),
    });
    let server = MinerServer::new(
        builder,
        chain,
        Arc::new(NoPendingTransactions),
        chain_config.clone(),
    );

    MiningFixture { server, store, trie_store, chain_config, parent, _dir: dir }
}

/// A Bitcoin block committing to `work_hash` whose hash clears `target` --
/// which is to say, actually mined.
///
/// The search is the point: RSK compares the Bitcoin block hash, read
/// little-endian, against `U256::MAX / difficulty`, so a block built with a
/// fixed nonce clears the target only by luck. At these difficulties a handful
/// of nonces suffice, which is what makes the round trip a unit test.
fn solve(work: &MinerWork) -> bitcoin::Block {
    let coinbase = coinbase::build_coinbase(&work.block_hash_for_merged_mining, 0xDEAD_BEEF);
    for nonce in 0..100_000u32 {
        let block = coinbase::build_bitcoin_block(coinbase.clone(), nonce);
        let hash = block.header.block_hash();
        if U256::from_le_slice(hash.as_byte_array()) <= work.target {
            return block;
        }
    }
    panic!("no nonce cleared target {}", work.target);
}

/// A Bitcoin block committing to `work_hash` that was never actually mined:
/// correctly formed in every respect except the proof of work.
fn unmined(work_hash: B256) -> bitcoin::Block {
    coinbase::build_bitcoin_block(coinbase::build_coinbase(&work_hash, 0xDEAD_BEEF), 0)
}

// ── the REMASC transaction ────────────────────────────────────────────

/// Groundtruth: the REMASC transaction of RSK mainnet block #9,257,552
/// (`0xafbe23...`, from public-node.rsk.co).
///
/// Every zero field in it is encoded the way rskj encodes it and not the way
/// canonical RLP would: the gas price and gas limit are a literal `0x00` byte,
/// the value is `0x80`. Re-encode it canonically and the transaction hashes
/// differently, which moves the transactions root, which fails the block --
/// with an error about roots rather than about encoding.
#[test]
fn remasc_transaction_matches_mainnet() {
    let tx = remasc_transaction(9_257_552);

    assert_eq!(
        alloy_primitives::hex::encode(tx.rlp_for_trie()),
        "e0838d424f00009400000000000000000000000000000000010000088080808080"
    );
    assert_eq!(
        tx.tx_hash(),
        b256!("afbe2397d46addffea824f9964498e7f3eec79c9c540811bcc8b825224af8889")
    );
    // The nonce is the *parent's* height, not the block's.
    assert_eq!(tx.nonce, 9_257_551);
}

#[test]
fn remasc_transaction_of_block_one_has_an_empty_nonce() {
    let tx = remasc_transaction(1);
    assert_eq!(tx.nonce, 0);
    // rskj writes a zero nonce as the empty string, not as `0x00`.
    assert_eq!(tx.rlp_for_trie()[1], 0x80);
}

/// It has to be recognisable to the executor, which detects it by shape
/// rather than by position.
#[test]
fn remasc_transaction_is_recognised_as_remasc() {
    let tx = remasc_transaction(4_242);
    assert_eq!(tx.to.as_ref(), crate::precompiles::REMASC_ADDR.as_slice());
    assert!(tx.gas_limit.is_zero());
    assert_eq!(tx.v, 0);
    assert!(tx.r.is_zero() && tx.s.is_zero());
}

// ── merkle proofs ─────────────────────────────────────────────────────

/// A proof is only correct if it folds back to the root the Bitcoin header
/// carries, so that is what is checked -- for every transaction count from one
/// to sixteen, which covers both the odd levels (where Bitcoin duplicates the
/// last hash) and the even ones.
#[test]
fn merkle_proofs_fold_back_to_the_root() {
    for count in 1..=16usize {
        let txids: Vec<[u8; 32]> = (0..count).map(|i| [i as u8 + 1; 32]).collect();
        let proof = merkle::proof_from_txids(&txids).unwrap();
        assert_eq!(proof.len() % 32, 0);

        let root = merkle::root_from_proof(&txids[0], &proof);
        assert_eq!(root, reference_merkle_root(&txids), "{count} transactions");
    }
}

/// Bitcoin's merkle root, computed the long way round for comparison.
fn reference_merkle_root(txids: &[[u8; 32]]) -> [u8; 32] {
    use rustock_core::validation::merged_mining::combine_left_right;
    let mut level = txids.to_vec();
    while level.len() > 1 {
        let mut next = Vec::new();
        for pair in level.chunks(2) {
            let right = if pair.len() == 2 { &pair[1] } else { &pair[0] };
            next.push(combine_left_right(&pair[0], right));
        }
        level = next;
    }
    level[0]
}

#[test]
fn a_single_transaction_block_needs_no_proof() {
    let proof = merkle::proof_from_txids(&[[7u8; 32]]).unwrap();
    assert!(proof.is_empty());
    assert_eq!(merkle::root_from_proof(&[7u8; 32], &proof), [7u8; 32]);
}

#[test]
fn a_block_with_no_transactions_has_no_coinbase() {
    assert_eq!(
        merkle::proof_from_txids(&[]),
        Err(merkle::MerkleProofError::NoTransactions)
    );
}

/// A partial-merkle submission hands over the branch including the coinbase;
/// the verifier starts from a coinbase hash it computes itself, so folding the
/// supplied one in as well would double-count it.
#[test]
fn a_partial_merkle_proof_drops_the_coinbase() {
    let hashes = [[1u8; 32], [2u8; 32], [3u8; 32]];
    let proof = merkle::proof_from_merkle_hashes(&hashes).unwrap();
    assert_eq!(proof, [[2u8; 32], [3u8; 32]].concat());
}

/// The wire carries these hashes in consensus (little-endian) order, and
/// everything past this point works in display order.
#[test]
fn wire_hashes_are_reversed_on_the_way_in() {
    let mut wire = [0u8; 32];
    wire[0] = 0xAA;
    let hex = alloy_primitives::hex::encode(wire);
    let parsed = merkle::parse_wire_hashes(&format!("{hex} 0x{hex}")).unwrap();
    assert_eq!(parsed.len(), 2);
    assert_eq!(parsed[0][31], 0xAA);
    assert_eq!(parsed[0], parsed[1]);
}

#[test]
fn a_malformed_wire_hash_is_rejected() {
    assert!(merkle::parse_wire_hashes("abcd").is_err());
    assert!(merkle::parse_wire_hashes("zz").is_err());
}

// ── fork-detection data ───────────────────────────────────────────────

/// Below 449 mainchain blocks there is nothing to commit to, and rskj's own
/// rule rejects a header that carries fork-detection data anyway.
#[test]
fn fork_detection_data_is_empty_on_a_short_chain() {
    let chain: Vec<Header> = (0..448)
        .map(|i| header_at(448 - i, B256::ZERO, B256::ZERO))
        .collect();
    assert!(fork_detection::calculate(&chain).is_empty());
}

#[test]
fn fork_detection_data_commits_to_height_and_uncles() {
    let mut chain: Vec<Header> = (0..fork_detection::REQUIRED_MAINCHAIN_BLOCKS)
        .map(|i| header_at(5_000 - i as u64, B256::ZERO, B256::ZERO))
        .collect();
    // Uncles are summed over the last 32 blocks only.
    for header in chain.iter_mut().take(32) {
        header.uncle_count = 1;
    }
    chain[32].uncle_count = 99;

    let data = fork_detection::calculate(&chain);
    assert_eq!(data.len(), fork_detection::FORK_DETECTION_DATA_LENGTH);
    assert_eq!(data[7], 32, "uncles outside the 32-block window must not count");
    assert_eq!(&data[8..12], &5_001u32.to_be_bytes(), "the height being mined");
}

/// The uncle count is one byte, and a chain busy enough to overflow it must
/// saturate rather than wrap -- a wrapped count would read as a *lower* uncle
/// rate, which is the opposite of what happened.
#[test]
fn fork_detection_uncle_count_saturates() {
    let chain: Vec<Header> = (0..fork_detection::REQUIRED_MAINCHAIN_BLOCKS)
        .map(|i| {
            let mut h = header_at(5_000 - i as u64, B256::ZERO, B256::ZERO);
            h.uncle_count = 10;
            h
        })
        .collect();
    assert_eq!(fork_detection::calculate(&chain)[7], 255);
}

/// The overlay replaces the last 12 bytes and nothing else: the first 20 are
/// the only part of the hash a verifier ever compares.
#[test]
fn fork_detection_data_overlays_only_the_tail_of_the_hash() {
    let base = B256::repeat_byte(0xAA);
    let overlaid = fork_detection::apply_to_hash(base, &[0xBB; 12]);
    assert_eq!(&overlaid[..20], &[0xAA; 20]);
    assert_eq!(&overlaid[20..], &[0xBB; 12]);

    // Empty data -- a chain too short to have any -- leaves the hash whole,
    // which is also the pre-wasabi100 form.
    assert_eq!(fork_detection::apply_to_hash(base, &[]), base);
}

// ── the template builder ──────────────────────────────────────────────

/// The strongest single test available: everything the builder filled in --
/// transactions root, ommers hash, gas used, paid fees, state root, receipts
/// root, logs bloom -- put back through the validating path that a peer
/// receiving this block would run.
///
/// It is worth more than checking each field against a restatement of how the
/// builder computes it, because it fails for the one reason that matters: the
/// block would not be accepted.
#[test]
fn a_template_validates_against_the_processing_path() {
    let fx = fixture(20);
    let template = fx.build().unwrap();

    assert_eq!(template.block.header.number, fx.parent.number + 1);
    assert_eq!(template.block.header.parent_hash, fx.parent.hash());
    // REMASC and nothing else: the pool is empty.
    assert_eq!(template.block.transactions.len(), 1);
    assert_eq!(
        template.block.transactions[0].to.as_ref(),
        crate::precompiles::REMASC_ADDR.as_slice()
    );

    let processor = BlockProcessor::new(RskHardforkConfig::all_active(33), fx.store.clone());
    let parent_state = TrieNode::empty();
    processor
        .process_block(&template.block, &parent_state, fx.trie_store.clone())
        .expect("a freshly built template must satisfy every root it filled in");
}

/// A template without the REMASC transaction executes to a different state
/// root. Worth pinning because the failure gives no hint of its cause: the
/// executor does not append REMASC, so the block simply computes different
/// state and the error names a root.
#[test]
fn a_template_without_remasc_would_not_validate() {
    let fx = fixture(20);
    let template = fx.build().unwrap();

    let mut stripped = template.block.clone();
    stripped.transactions.clear();

    let processor = BlockProcessor::new(RskHardforkConfig::all_active(33), fx.store.clone());
    let err = processor
        .process_block(&stripped, &TrieNode::empty(), fx.trie_store.clone())
        .expect_err("a block missing REMASC must not validate");
    assert!(
        matches!(err, crate::processor::ProcessError::TransactionsRootMismatch { .. }),
        "unexpected error: {err}"
    );
}

/// The open question from the handover: `paid_fees` is filled in after
/// execution, and REMASC reads a `paid_fees` during execution -- but from the
/// *matured* block it fetches from storage, not from the block being built.
/// If that were wrong, the builder would need a two-pass fixed point.
///
/// Re-executing with the field filled in and comparing the roots settles it.
#[test]
fn filling_paid_fees_does_not_change_what_the_block_executes_to() {
    let fx = fixture(20);
    let template = fx.build().unwrap();

    let processor = BlockProcessor::new(RskHardforkConfig::all_active(33), fx.store.clone());
    let first = processor
        .execute_block(&template.block, &TrieNode::empty(), fx.trie_store.clone())
        .unwrap();

    // Execute again from a header whose paid_fees and gas_used are zeroed, the
    // way they were when the template was first executed.
    let mut zeroed = template.block.clone();
    zeroed.header.paid_fees = U256::ZERO;
    zeroed.header.gas_used = 0;
    zeroed.header.state_root = B256::ZERO;
    zeroed.header.receipts_root = B256::ZERO;
    let second = processor
        .execute_block(&zeroed, &TrieNode::empty(), fx.trie_store.clone())
        .unwrap();

    assert_eq!(first.state_root_hash, second.state_root_hash);
    assert_eq!(first.receipts_root, second.receipts_root);
    assert_eq!(first.paid_fees, second.paid_fees);
}

/// RSKIP92 keeps the merkle proof and coinbase out of the hashed prefix so a
/// solution can be attached to a block that was already committed to. If
/// filling them moved the merged-mining hash, every submitted proof would be
/// against a hash the finished header no longer has.
#[test]
fn filling_the_mining_fields_does_not_move_the_merged_mining_hash() {
    let fx = fixture(20);
    let template = fx.build().unwrap();
    let before = template.block.header.hash_for_merged_mining();

    let mut header = template.block.header.clone();
    header.bitcoin_merged_mining_header = Some(Bytes::from(vec![0xAA; 80]));
    header.bitcoin_merged_mining_merkle_proof = Some(Bytes::from(vec![0xBB; 64]));
    header.bitcoin_merged_mining_coinbase_transaction = Some(Bytes::from(vec![0xCC; 100]));

    assert_eq!(before, header.hash_for_merged_mining());
}

/// From papyrus200 rskj puts an empty `ummRoot` in every header it builds.
/// Its presence changes both hashes, so a miner that decided by activation
/// height differently from its peers produces blocks nobody accepts.
#[test]
fn umm_root_appears_once_papyrus_is_active() {
    let fx = fixture(20);
    assert!(fx.build().unwrap().block.header.umm_root.is_none());

    let fx = fixture_with_papyrus_at(0, 20);
    assert_eq!(fx.build().unwrap().block.header.umm_root, Some(Bytes::new()));
}

/// Inheriting the parent's gas limit and minimum gas price is always valid --
/// both rules are bounds around the parent's value, and the parent's own value
/// is in range. This is what the builder does when no target is configured.
#[test]
fn a_template_inherits_gas_limit_and_minimum_gas_price() {
    let fx = fixture(20);
    let header = fx.build().unwrap().block.header;
    assert_eq!(header.gas_limit, fx.parent.gas_limit);
    assert_eq!(header.minimum_gas_price, fx.parent.minimum_gas_price);
}

#[test]
fn a_template_is_never_timestamped_at_or_before_its_parent() {
    // The parent is dated far in the future, so wall-clock time cannot satisfy
    // the rule on its own.
    let mut fx = fixture(20);
    fx.parent.timestamp = u64::MAX / 2;
    let header = fx.build_on(&fx.parent).unwrap().block.header;
    assert!(header.timestamp > fx.parent.timestamp);
}

// ── transaction selection ─────────────────────────────────────────────

#[test]
fn transactions_are_ordered_by_price_but_never_out_of_nonce_order() {
    let rich = Address::repeat_byte(0xA1);
    let poor = Address::repeat_byte(0xB2);

    // The rich sender's cheap transaction comes first by nonce; ordering
    // purely by price would put its expensive nonce-1 ahead of nonce-0 and
    // strand both.
    let pending = vec![
        pending_tx(rich, 1, 1_000, 1),
        pending_tx(rich, 0, 10, 2),
        pending_tx(poor, 0, 500, 3),
    ];

    let ordered = super::template::order_by_price_sender_and_nonce_for_test(pending);
    let keys: Vec<(Address, u64)> = ordered.iter().map(|p| (p.sender, p.tx.nonce)).collect();
    assert_eq!(keys, vec![(poor, 0), (rich, 0), (rich, 1)]);
}

#[test]
fn transaction_ordering_is_stable_for_equal_prices() {
    let a = Address::repeat_byte(0xA1);
    let b = Address::repeat_byte(0xB2);
    let pending = vec![pending_tx(a, 0, 100, 1), pending_tx(b, 0, 100, 2)];

    let first = super::template::order_by_price_sender_and_nonce_for_test(pending.clone());
    // Reversed input, same output: a HashMap hands senders back in whatever
    // order it likes, and two rebuilds of the same template must agree.
    let mut reversed = pending;
    reversed.reverse();
    let second = super::template::order_by_price_sender_and_nonce_for_test(reversed);

    let key = |v: &Vec<PendingTransaction>| -> Vec<B256> { v.iter().map(|p| p.hash).collect() };
    assert_eq!(key(&first), key(&second));
}

fn pending_tx(sender: Address, nonce: u64, gas_price: u64, seed: u8) -> PendingTransaction {
    PendingTransaction {
        tx: rustock_core::Transaction {
            nonce,
            gas_price: U256::from(gas_price),
            gas_limit: U256::from(21_000),
            to: Bytes::from(vec![0u8; 20]),
            value: U256::ZERO,
            input: Bytes::new(),
            v: 27,
            r: U256::from(1),
            s: U256::from(1),
            cached_rlp: None,
        },
        sender,
        hash: B256::repeat_byte(seed),
    }
}

// ── work and submission ───────────────────────────────────────────────

/// The round trip, end to end: ask for work, build the Bitcoin block a miner
/// would build, submit it, and have the node accept the result as its own new
/// head -- with the completed header passing the very rule it would apply to a
/// block arriving from a peer.
#[test]
fn a_solution_completes_a_block_and_is_imported() {
    let fx = fixture(20);
    let work = fx.server.get_work().unwrap();
    assert_eq!(work.parent_block_hash, fx.parent.hash());
    assert!(work.notify, "the first work on a new parent is always a notify");

    let btc_block = solve(&work);
    let info = fx
        .server
        .submit_bitcoin_block(&bitcoin::consensus::serialize(&btc_block))
        .expect("a correctly tagged solution must be accepted");

    assert_eq!(info.block_imported_result, ImportResult::ImportedBest);
    assert_eq!(info.block_included_height, fx.parent.number + 1);

    let stored = fx.store.block(info.block_hash).unwrap().expect("block stored");
    assert_eq!(stored.header.number, fx.parent.number + 1);

    // The three fields a solution supplies are all there...
    assert!(stored.header.bitcoin_merged_mining_header.is_some());
    assert!(stored.header.bitcoin_merged_mining_coinbase_transaction.is_some());
    assert!(stored.header.bitcoin_merged_mining_merkle_proof.is_some());

    // ...and the completed header satisfies the merged-mining rule, which is
    // the only thing that decides whether a peer keeps it.
    MergedMiningRule { config: fx.chain_config.clone() }
        .validate(&stored.header)
        .expect("the completed header must satisfy the rule this node applies to peers");

    // The chain moved: the node now builds on its own block.
    assert_eq!(fx.store.head().unwrap(), Some(info.block_hash));
    assert_eq!(
        fx.store.exec_head().unwrap().map(|(h, _)| h),
        Some(info.block_hash),
        "the executed head must advance too, or the next template builds on stale state"
    );
}

/// Submitting the same solution twice is a race, not an error: two workers of
/// the same pool can both report it. The second must say so rather than claim
/// the work was never handed out.
#[test]
fn a_repeated_solution_reports_the_block_as_already_present() {
    let fx = fixture(20);
    let work = fx.server.get_work().unwrap();
    let raw = bitcoin::consensus::serialize(&solve(&work));

    assert_eq!(
        fx.server.submit_bitcoin_block(&raw).unwrap().block_imported_result,
        ImportResult::ImportedBest
    );
    // The head moved, so the work is now for a parent that is no longer the
    // executed head -- which is exactly what a duplicate looks like from here.
    let err = fx.server.submit_bitcoin_block(&raw).unwrap_err();
    assert!(
        matches!(err, SubmitError::ParentNoLongerHead { .. }),
        "unexpected error: {err}"
    );
}

/// A coinbase tagged with a hash this node never handed out cannot be matched
/// to a block, and the message has to say which of the two reasons it is.
#[test]
fn a_solution_to_unknown_work_is_refused() {
    let fx = fixture(20);
    // Real work, so the solution is a genuine one -- only the hash it commits
    // to was never handed out.
    let unknown = MinerWork {
        block_hash_for_merged_mining: B256::repeat_byte(0x77),
        ..fx.server.get_work().unwrap()
    };

    let raw = bitcoin::consensus::serialize(&solve(&unknown));
    let err = fx.server.submit_bitcoin_block(&raw).unwrap_err();
    match err {
        SubmitError::UnknownWork { hash } => assert_eq!(hash, B256::repeat_byte(0x77)),
        other => panic!("unexpected error: {other}"),
    }
}

#[test]
fn a_coinbase_without_a_tag_is_refused() {
    let fx = fixture(20);
    let work = fx.server.get_work().unwrap();

    let mut block = solve(&work);
    block.txdata[0].output.truncate(1); // drop the tagged output
    let err = fx
        .server
        .submit_bitcoin_block(&bitcoin::consensus::serialize(&block))
        .unwrap_err();
    assert!(matches!(err, SubmitError::NoTagInCoinbase), "unexpected error: {err}");
}

/// Work is cached, not just held: a solution arrives minutes after the work it
/// answers, by which time several newer templates exist. Keeping only the
/// newest would lose a block that was legitimately found.
#[test]
fn work_handed_out_earlier_can_still_be_answered() {
    let fx = fixture(20);
    let first = fx.server.get_work().unwrap();

    // Several rebuilds later -- same parent, so the templates differ only by
    // timestamp -- the original is still answerable.
    for _ in 0..5 {
        fx.server.build_work().unwrap();
    }

    let raw = bitcoin::consensus::serialize(&solve(&first));
    let info = fx.server.submit_bitcoin_block(&raw).expect("stale work is still work");
    assert_eq!(info.block_imported_result, ImportResult::ImportedBest);
}

/// Bounded, though: the cache is 20 deep, so work older than that is gone and
/// has to be reported as unknown rather than silently matched to something else.
#[test]
fn work_older_than_the_cache_is_forgotten() {
    let fx = fixture(20);
    let first = fx.server.get_work().unwrap();
    let mut seen = std::collections::HashSet::new();
    seen.insert(first.block_hash_for_merged_mining);

    // Each rebuild must produce a distinct template, or this proves nothing.
    while seen.len() <= MiningConfig::default().work_cache_size + 1 {
        let work = fx.server.build_work().unwrap();
        if !seen.insert(work.block_hash_for_merged_mining) {
            // Same second, same template: nudge the clock forward.
            std::thread::sleep(std::time::Duration::from_millis(1100));
        }
    }

    let raw = bitcoin::consensus::serialize(&solve(&first));
    assert!(matches!(
        fx.server.submit_bitcoin_block(&raw).unwrap_err(),
        SubmitError::UnknownWork { .. }
    ));
}

/// `notify` marks the transition to new work, not the work itself: a pool that
/// polls twice must not be told twice to push to its miners.
#[test]
fn notify_is_reported_once() {
    let fx = fixture(20);
    assert!(fx.server.get_work().unwrap().notify);
    assert!(!fx.server.get_work().unwrap().notify);
}

/// The three submit methods differ only in how much of the Bitcoin block the
/// miner sends; they must all complete the same header.
#[test]
fn all_three_submit_forms_accept_the_same_solution() {
    for form in ["block", "transactions", "partial"] {
        let fx = fixture(20);
        let work = fx.server.get_work().unwrap();
        let btc_block = solve(&work);

        let header = coinbase::encode_header(&btc_block.header);
        let raw_coinbase = coinbase::encode_transaction(&btc_block.txdata[0]);
        // The wire carries hashes little-endian; display order is the reverse.
        let wire_hashes: Vec<String> = coinbase::txids_display_order(&btc_block)
            .iter()
            .map(|h| {
                let mut wire = *h;
                wire.reverse();
                alloy_primitives::hex::encode(wire)
            })
            .collect();

        let info = match form {
            "block" => fx.server.submit_bitcoin_block(&bitcoin::consensus::serialize(&btc_block)),
            "transactions" => fx.server.submit_bitcoin_block_transactions(
                &header,
                &raw_coinbase,
                &wire_hashes.join(" "),
            ),
            _ => fx.server.submit_bitcoin_block_partial_merkle(
                &header,
                &raw_coinbase,
                &wire_hashes.join(" "),
                wire_hashes.len() as u32,
            ),
        }
        .unwrap_or_else(|e| panic!("{form}: {e}"));

        assert_eq!(info.block_imported_result, ImportResult::ImportedBest, "{form}");
        let stored = fx.store.block(info.block_hash).unwrap().unwrap();
        MergedMiningRule { config: fx.chain_config.clone() }
            .validate(&stored.header)
            .unwrap_or_else(|e| panic!("{form}: completed header rejected: {e}"));
    }
}

/// Before wasabi100 the coinbase carries the whole 32-byte hash and the
/// verifier compares all of it; from wasabi100 it compares only the first 20.
/// A miner must not hardcode either -- regtest and historical replay use the
/// other one.
#[test]
fn both_tag_forms_verify() {
    for wasabi100 in [0u64, u64::MAX] {
        let fx = fixture_with_wasabi_at(wasabi100, 20);
        let work = fx.server.get_work().unwrap();
        let raw = bitcoin::consensus::serialize(&solve(&work));
        let info = fx
            .server
            .submit_bitcoin_block(&raw)
            .unwrap_or_else(|e| panic!("wasabi100={wasabi100}: {e}"));

        let stored = fx.store.block(info.block_hash).unwrap().unwrap();
        MergedMiningRule { config: fx.chain_config.clone() }
            .validate(&stored.header)
            .unwrap_or_else(|e| panic!("wasabi100={wasabi100}: {e}"));
    }
}

/// The coinbase this node compresses must hash to what the verifier recomputes
/// from the compressed form -- checked here on a real completed header rather
/// than on a synthetic buffer.
#[test]
fn the_stored_coinbase_hashes_to_the_submitted_one() {
    let fx = fixture(20);
    let work = fx.server.get_work().unwrap();
    let btc_block = solve(&work);
    let raw_coinbase = coinbase::encode_transaction(&btc_block.txdata[0]);

    let info = fx
        .server
        .submit_bitcoin_block(&bitcoin::consensus::serialize(&btc_block))
        .unwrap();
    let stored = fx.store.block(info.block_hash).unwrap().unwrap();
    let compressed = stored.header.bitcoin_merged_mining_coinbase_transaction.unwrap();

    let mut expected = btc_block.txdata[0].compute_txid().to_byte_array();
    expected.reverse();
    assert_eq!(compute_coinbase_hash(&compressed), expected);
    // And the compression is the one the format prescribes.
    assert_eq!(&compressed[..], &compress_coinbase(&raw_coinbase, true).unwrap()[..]);
}

/// A Bitcoin block whose hash does not clear RSK's target is not a solution.
/// The node checks this itself before storing, rather than finding out when a
/// peer rejects the block.
#[test]
fn a_solution_that_misses_the_target_is_refused() {
    // A difficulty no Bitcoin block will ever satisfy, so the submission below
    // is a correctly formed solution to the wrong amount of work.
    let fx = fixture_with_difficulty(U256::MAX, 20);
    let work = fx.server.get_work().unwrap();
    let raw = bitcoin::consensus::serialize(&unmined(work.block_hash_for_merged_mining));

    let err = fx.server.submit_bitcoin_block(&raw).unwrap_err();
    assert!(matches!(err, SubmitError::SelfCheck(_)), "unexpected error: {err}");
    // Nothing was stored: a refused solution must not leave a block behind.
    assert_eq!(fx.store.head().unwrap(), Some(fx.parent.hash()));
}

/// `mnr_getWork` hands out the target the Bitcoin hash is compared against,
/// and it has to be the same number the verifier derives from the difficulty.
#[test]
fn the_advertised_target_matches_the_difficulty() {
    let fx = fixture(20);
    let template = fx.build().unwrap();
    assert_eq!(template.target, U256::MAX / template.block.header.difficulty);
    assert_eq!(difficulty_to_target(U256::from(1)), U256::MAX);
}

/// A mined block that loses the total-difficulty comparison is still stored --
/// it may win a later reorganisation -- but it must not take the canonical
/// pointer for its height. Storing a block and making it canonical are
/// separate decisions, and conflating them leaves the canonical chain naming a
/// block that is not on it.
#[test]
fn a_losing_block_is_stored_but_not_made_canonical() {
    let fx = fixture(20);
    let work = fx.server.get_work().unwrap();
    let mined_height = fx.parent.number + 1;

    // A competing head with far more work behind it. The executed head is
    // still our parent, so the submission is accepted and executed; only the
    // comparison at the end goes the other way.
    let rival = header_at(mined_height, B256::repeat_byte(0x99), B256::ZERO);
    let rival_hash = rival.hash();
    fx.store.put_header_with_hash(rival_hash, &rival).unwrap();
    fx.store.put_total_difficulty(rival_hash, U256::from(1_000_000)).unwrap();
    fx.store.set_head(rival_hash).unwrap();

    let raw = bitcoin::consensus::serialize(&solve(&work));
    let info = fx.server.submit_bitcoin_block(&raw).expect("a valid solution is still valid");

    assert_eq!(info.block_imported_result, ImportResult::ImportedNotBest);
    assert!(
        fx.store.header(info.block_hash).unwrap().is_some(),
        "the block is kept: it may yet win a reorganisation"
    );
    assert_eq!(
        fx.store.canonical_hash(mined_height).unwrap(),
        None,
        "a block that lost must not hold the canonical pointer for its height"
    );
    assert_eq!(fx.store.head().unwrap(), Some(rival_hash), "the head must not move");
}

/// The round trip again, but deep enough that REMASC actually pays out.
///
/// Everywhere else these tests mine block #21, and REMASC's
/// `process_miners_fees` returns immediately there: the executor is built with
/// `RemascConfig::mainnet()`, whose maturity is 4,000, so a chain that short
/// has no matured block to distribute. That leaves the interesting half of
/// REMASC -- fetching the matured header, collecting siblings, paying the
/// miner -- unexercised, while the test still passes and looks like it covers
/// block production.
///
/// Mining above both the maturity window and the synthetic span runs it for
/// real. Kept separate because seeding four thousand headers costs a second.
#[test]
fn a_solution_is_imported_on_a_chain_deep_enough_for_remasc_to_pay() {
    // maturity 4,000 + synthetic span 10, so the matured block is #11.
    let fx = fixture(4_010);
    let work = fx.server.get_work().unwrap();

    let raw = bitcoin::consensus::serialize(&solve(&work));
    let info = fx
        .server
        .submit_bitcoin_block(&raw)
        .expect("a solution must be accepted on a chain deep enough for REMASC to pay out");

    assert_eq!(info.block_imported_result, ImportResult::ImportedBest);
    assert_eq!(info.block_included_height, 4_011);

    let stored = fx.store.block(info.block_hash).unwrap().unwrap();
    MergedMiningRule { config: fx.chain_config.clone() }
        .validate(&stored.header)
        .expect("the completed header must satisfy the rule this node applies to peers");
}

/// And the round trip with `ummRoot` present, which is what mainnet actually
/// carries. Elsewhere these tests keep papyrus200 out of reach so they can
/// work at low block numbers, which means the field every real header has is
/// absent from the block they mine.
#[test]
fn a_solution_is_imported_with_umm_root_present() {
    let fx = fixture_with_papyrus_at(0, 20);
    let work = fx.server.get_work().unwrap();

    let raw = bitcoin::consensus::serialize(&solve(&work));
    let info = fx.server.submit_bitcoin_block(&raw).expect("solution accepted");

    let stored = fx.store.block(info.block_hash).unwrap().unwrap();
    assert_eq!(stored.header.umm_root, Some(Bytes::new()));
    MergedMiningRule { config: fx.chain_config.clone() }
        .validate(&stored.header)
        .expect("a header carrying ummRoot must still verify");
}
