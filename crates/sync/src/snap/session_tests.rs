//! The session's judgement, tested without a network.
//!
//! These are about *what the client agrees to believe*, so they are written
//! from the attacker's side: a peer offering a chain that does not link, a
//! header that does not validate, a genesis nobody has seen.

use super::server::SnapServer;
use super::session::{Action, Phase, SnapFailure, SnapSession};
use super::SnapConfig;
use alloy_primitives::{Address, B256, U256};
use rustock_core::validation::{HeaderValidator, HeaderVerifier, ValidationError};
use rustock_core::{Block, Header};
use rustock_networking::protocol::snap::{ChunkPayload, SnapChunkRequest};
use rustock_storage::BlockStore;
use rustock_trie::{MemoryTrieStore, TrieKeySlice, TrieNode, TrieStore};
use std::sync::Arc;

/// A rule that refuses everything, standing in for proof of work failing.
struct RefuseEverything;

impl HeaderValidator for RefuseEverything {
    fn validate(&self, _header: &Header) -> Result<(), ValidationError> {
        Err(ValidationError::BitcoinPowInvalid {
            hash: B256::ZERO,
            target: U256::ZERO,
        })
    }
}

fn header(number: u64, parent: B256, state_root: B256, difficulty: u64) -> Header {
    Header {
        parent_hash: parent,
        ommers_hash: B256::ZERO,
        beneficiary: Address::ZERO,
        state_root,
        transactions_root: B256::ZERO,
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

fn block(h: Header) -> Block {
    Block { header: h, transactions: Vec::new(), ommers: Vec::new() }
}

/// A chain of `n` blocks from `first_number`, each worth `difficulty`, with
/// their cumulative difficulties.
fn chain(
    first_number: u64,
    n: usize,
    from_parent: B256,
    base_td: u64,
    state_root: B256,
) -> (Vec<Block>, Vec<U256>) {
    let difficulty = 100u64;
    let mut blocks = Vec::new();
    let mut tds = Vec::new();
    let mut parent = from_parent;
    let mut td = base_td;

    for i in 0..n {
        let h = header(first_number + i as u64, parent, state_root, difficulty);
        parent = h.hash();
        td += difficulty;
        tds.push(U256::from(td));
        blocks.push(block(h));
    }
    (blocks, tds)
}

struct Fixture {
    store: Arc<BlockStore>,
    trie: Arc<MemoryTrieStore>,
    state_root: B256,
    trie_root: TrieNode,
    _dir: tempfile::TempDir,
}

/// A node with a genesis and a small state trie to download.
fn fixture(state_keys: usize) -> Fixture {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Arc::new(BlockStore::open(dir.path()).expect("open"));

    let trie = Arc::new(MemoryTrieStore::new());
    let mut root = TrieNode::empty();
    for i in 0..state_keys {
        let key = [(i >> 8) as u8, i as u8, (i * 3) as u8, (i * 11) as u8];
        let value: Vec<u8> =
            if i % 4 == 0 { vec![i as u8; 80] } else { vec![i as u8, 2, 3] };
        root = root.put(&TrieKeySlice::from_key(&key), &value, trie.as_ref());
    }
    root.save(trie.as_ref(), true);
    let state_root = root.compute_hash(trie.as_ref());

    Fixture { store, trie, state_root, trie_root: root, _dir: dir }
}

impl Fixture {
    /// Records a genesis this node accepts, returning its hash.
    fn with_genesis(&self) -> B256 {
        let genesis = header(0, B256::ZERO, B256::ZERO, 100);
        let hash = genesis.hash();
        self.store.put_header_with_hash(hash, &genesis).expect("genesis");
        self.store.put_canonical_hash(0, hash).expect("index");
        hash
    }

    fn session(&self, verifier: HeaderVerifier) -> SnapSession {
        // A client store: the download writes here, not into the peer's.
        SnapSession::new(
            SnapConfig { client_enabled: true, max_in_flight: 2, chunk_bytes: 600, ..SnapConfig::default() },
            self.store.clone(),
            Arc::new(MemoryTrieStore::new()) as Arc<dyn TrieStore>,
            Arc::new(verifier),
        )
    }
}

#[test]
fn a_fresh_session_asks_what_peers_can_serve() {
    let f = fixture(10);
    let mut session = f.session(HeaderVerifier::new());
    assert_eq!(session.poll(), vec![Action::RequestStatus]);
    assert_eq!(session.phase(), Phase::AwaitingStatus);
}

/// **The trust ordering.** No state is requested until the header chain has
/// been verified back to something this node already had. A client that got
/// this backwards would spend the download on a stranger's word.
#[test]
fn no_state_is_requested_before_the_headers_are_verified() {
    let f = fixture(60);
    let genesis = f.with_genesis();
    let (blocks, tds) = chain(1, 5, genesis, 100, f.state_root);

    let mut session = f.session(HeaderVerifier::new());
    let actions = session.on_status(&blocks, &tds, 5_000);

    assert_eq!(session.phase(), Phase::VerifyingHeaders);
    assert!(
        actions.iter().all(|a| matches!(a, Action::RequestHeaders { .. })),
        "asked for state before verifying headers: {actions:?}"
    );
}

/// A peer whose headers fail the consensus rules is refused, and nothing it
/// offered is used.
#[test]
fn a_header_that_fails_validation_ends_the_session() {
    let f = fixture(20);
    let genesis = f.with_genesis();
    let (blocks, tds) = chain(1, 3, genesis, 100, f.state_root);

    let verifier = HeaderVerifier::new().with_static_rule(RefuseEverything);
    let mut session = f.session(verifier);
    session.on_status(&blocks, &tds, 5_000);

    // The walk asks for the parent of the checkpoint; answer with a genuine
    // header that the (refusing) verifier will reject.
    let parent = blocks[blocks.len() - 2].header.clone();
    session.on_headers(&[parent]);

    assert_eq!(session.phase(), Phase::Failed);
    assert!(matches!(session.failure(), Some(SnapFailure::InvalidHeader { .. })));
}

/// Headers that do not link to what has already been verified are refused,
/// even if each one is individually well-formed.
#[test]
fn a_spliced_header_chain_is_refused() {
    let f = fixture(20);
    let genesis = f.with_genesis();
    let (blocks, tds) = chain(1, 3, genesis, 100, f.state_root);

    let mut session = f.session(HeaderVerifier::new());
    session.on_status(&blocks, &tds, 5_000);

    // A header from another chain entirely: valid on its own, wrong parent.
    let stranger = header(2, B256::repeat_byte(0x77), f.state_root, 100);
    session.on_headers(&[stranger]);

    assert_eq!(session.phase(), Phase::Failed);
    assert_eq!(session.failure(), Some(&SnapFailure::BrokenChain));
}

/// **Another network's chain is not this one's**, however much work is behind
/// it. Walking back to a genesis this node has never seen must fail, not
/// succeed for having reached block zero.
#[test]
fn a_chain_back_to_a_foreign_genesis_is_refused() {
    let f = fixture(20);
    // Deliberately no genesis recorded: this node knows nothing yet.
    let foreign_genesis = header(0, B256::ZERO, B256::repeat_byte(9), 100);
    let (blocks, tds) = chain(1, 2, foreign_genesis.hash(), 100, f.state_root);

    let mut session = f.session(HeaderVerifier::new());
    session.on_status(&blocks, &tds, 5_000);

    session.on_headers(&[blocks[0].header.clone()]);
    session.on_headers(&[foreign_genesis]);

    assert_eq!(session.phase(), Phase::Failed);
    assert_eq!(session.failure(), Some(&SnapFailure::NoCommonAncestor));
}

/// Blocks that do not form a chain are refused before anything expensive
/// happens.
#[test]
fn a_status_whose_blocks_do_not_link_is_refused() {
    let f = fixture(20);
    let (mut blocks, tds) = chain(1, 4, B256::ZERO, 100, f.state_root);
    blocks[2] = block(header(3, B256::repeat_byte(0x55), f.state_root, 100));

    let mut session = f.session(HeaderVerifier::new());
    session.on_status(&blocks, &tds, 5_000);
    assert_eq!(session.failure(), Some(&SnapFailure::BrokenChain));
}

/// Cumulative difficulty has to account for each block's own difficulty. A
/// peer inflating it is claiming work it did not do.
#[test]
fn inflated_difficulty_is_refused() {
    let f = fixture(20);
    let (blocks, mut tds) = chain(1, 4, B256::ZERO, 100, f.state_root);
    tds[2] = tds[2] + U256::from(1_000_000u64);

    let mut session = f.session(HeaderVerifier::new());
    session.on_status(&blocks, &tds, 5_000);
    assert_eq!(session.failure(), Some(&SnapFailure::BadDifficulty));
}

#[test]
fn an_empty_status_is_refused() {
    let f = fixture(10);
    let mut session = f.session(HeaderVerifier::new());
    session.on_status(&[], &[], 0);
    assert_eq!(session.failure(), Some(&SnapFailure::NoCheckpoint));
}

/// The whole sequence: status, header walk to a known block, the state, then
/// the blocks behind the checkpoint.
#[test]
fn a_session_runs_to_completion() {
    let f = fixture(250);
    let genesis = f.with_genesis();
    let (blocks, tds) = chain(1, 4, genesis, 100, f.state_root);
    let checkpoint = blocks.last().unwrap().header.clone();

    // The peer's own store knows the chain it is offering, so its server can
    // find the state by block number.
    for (b, td) in blocks.iter().zip(tds.iter()) {
        let hash = b.header.hash();
        f.store.put_header_with_hash(hash, &b.header).expect("header");
        f.store.put_canonical_hash(b.header.number, hash).expect("index");
        f.store.put_total_difficulty(hash, *td).expect("td");
    }

    let mut session = f.session(HeaderVerifier::new());
    let config = SnapConfig {
        server_enabled: true,
        blocks_required: 2,
        block_chunk_size: 2,
        ..SnapConfig::default()
    };

    // A server backed by the peer's copy of the state.
    let server = SnapServer::new(
        f.store.clone(),
        f.trie.clone() as Arc<dyn TrieStore>,
        config.clone(),
    );

    let mut actions = session.on_status(&blocks, &tds, 8_000);
    assert_eq!(session.phase(), Phase::VerifyingHeaders);

    let mut guard = 0;
    while session.phase() != Phase::Done && session.phase() != Phase::Failed {
        guard += 1;
        assert!(guard < 10_000, "session did not terminate in phase {:?}", session.phase());

        if actions.is_empty() {
            actions = session.poll();
            assert!(!actions.is_empty(), "stuck in phase {:?}", session.phase());
        }

        let action = actions.remove(0);
        let next = match action {
            Action::RequestStatus => session.on_status(&blocks, &tds, 8_000),
            Action::RequestHeaders { from, .. } => {
                // Answer from the offered chain, newest first, as a peer does.
                let answer: Vec<Header> = blocks
                    .iter()
                    .rev()
                    .map(|b| b.header.clone())
                    .filter(|h| h.hash() == from)
                    .collect();
                session.on_headers(&answer)
            }
            Action::RequestChunk { from, budget, .. } => {
                let response = server
                    .chunk(&SnapChunkRequest {
                        id: 1,
                        block_number: checkpoint.number,
                        from,
                        chunk_size: budget,
                        state_root: Some(f.state_root),
                    })
                    .expect("server answers");
                session.on_chunk(from, &response.payload)
            }
            Action::RequestBlocks { block_number } => {
                // Two blocks below the checkpoint, from the offered chain.
                let answer: Vec<Block> = blocks
                    .iter()
                    .filter(|b| b.header.number < block_number)
                    .cloned()
                    .collect();
                let answer_tds: Vec<U256> = blocks
                    .iter()
                    .zip(tds.iter())
                    .filter(|(b, _)| b.header.number < block_number)
                    .map(|(_, td)| *td)
                    .collect();
                session.on_blocks(&answer, &answer_tds)
            }
        };
        actions.extend(next);
    }

    assert_eq!(session.phase(), Phase::Done, "failure: {:?}", session.failure());

    // The checkpoint's state is on disk and whole.
    let (covered, total) = session.state_progress();
    assert_eq!(Some(covered), total);
    assert_eq!(session.checkpoint().map(|h| h.number), Some(checkpoint.number));
}

/// The server-side chunk request for a root this node does not have at that
/// height is declined, not answered with a different state.
#[test]
fn the_server_will_not_substitute_a_different_state() {
    let f = fixture(30);
    let number = 20_000u64;
    let h = header(number, B256::repeat_byte(1), f.state_root, 100);
    let hash = h.hash();
    f.store.put_header_with_hash(hash, &h).expect("header");
    f.store.put_canonical_hash(number, hash).expect("index");

    let server = SnapServer::new(
        f.store.clone(),
        f.trie.clone() as Arc<dyn TrieStore>,
        SnapConfig { server_enabled: true, ..SnapConfig::default() },
    );

    // Asking for a root we do not have at that height.
    let response = server
        .chunk(&SnapChunkRequest {
            id: 1,
            block_number: number,
            from: 0,
            chunk_size: 512,
            state_root: Some(B256::repeat_byte(0xAB)),
        })
        .expect("an answer");
    match response.payload {
        ChunkPayload::Proved { entries, .. } => {
            assert!(entries.is_empty(), "served a state nobody asked for")
        }
        other => panic!("unexpected payload {other:?}"),
    }

    // The same request naming the right root is served.
    let response = server
        .chunk(&SnapChunkRequest {
            id: 1,
            block_number: number,
            from: 0,
            chunk_size: 512,
            state_root: Some(f.state_root),
        })
        .expect("an answer");
    match response.payload {
        ChunkPayload::Proved { entries, .. } => assert!(!entries.is_empty()),
        other => panic!("unexpected payload {other:?}"),
    }
    let _ = &f.trie_root;
}
