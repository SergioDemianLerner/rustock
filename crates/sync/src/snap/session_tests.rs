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

/// Refuses only headers below a height, so a checkpoint can pass while a
/// header further down the walk does not.
struct RefuseBelow(u64);

impl HeaderValidator for RefuseBelow {
    fn validate(&self, header: &Header) -> Result<(), ValidationError> {
        if header.number < self.0 {
            return Err(ValidationError::BitcoinPowInvalid {
                hash: header.hash(),
                target: U256::ZERO,
            });
        }
        Ok(())
    }
}

fn header(number: u64, parent: B256, state_root: B256, difficulty: u64) -> Header {
    Header {
        parent_hash: parent,
        ommers_hash: rustock_execution::processor::compute_ommers_hash(&[]),
        beneficiary: Address::ZERO,
        state_root,
        // The roots an empty body actually produces. A header claiming ZERO
        // alongside no transactions is not a block that could exist, and the
        // body check below is right to refuse it.
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
    let actions = session.on_status(&blocks, &tds, 5_000, 0);

    assert_eq!(session.phase(), Phase::VerifyingHeaders);
    assert!(
        actions.iter().all(|a| matches!(a, Action::RequestHeaders { .. })),
        "asked for state before verifying headers: {actions:?}"
    );
}

/// **The checkpoint's own proof of work is checked the moment it is offered.**
/// It is the one header the walk never visits as a candidate -- it is only
/// ever the child -- so nothing else would check it.
#[test]
fn a_checkpoint_that_fails_validation_is_refused_at_once() {
    let f = fixture(20);
    let genesis = f.with_genesis();
    let (blocks, tds) = chain(1, 3, genesis, 100, f.state_root);

    let verifier = HeaderVerifier::new().with_static_rule(RefuseEverything);
    let mut session = f.session(verifier);
    let actions = session.on_status(&blocks, &tds, 5_000, 0);

    assert_eq!(session.phase(), Phase::Failed);
    assert!(actions.is_empty(), "asked for something after refusing the checkpoint");
    assert!(matches!(session.failure(), Some(SnapFailure::InvalidHeader { .. })));
}

/// And a header further down the walk is refused too, after the checkpoint
/// has been accepted.
#[test]
fn a_header_in_the_walk_that_fails_validation_ends_the_session() {
    let f = fixture(20);
    let genesis = f.with_genesis();
    let (blocks, tds) = chain(1, 3, genesis, 100, f.state_root);
    let checkpoint = blocks.last().unwrap().header.number;

    // Everything below the checkpoint fails; the checkpoint itself passes.
    let verifier = HeaderVerifier::new().with_static_rule(RefuseBelow(checkpoint));
    let mut session = f.session(verifier);
    session.on_status(&blocks, &tds, 5_000, 0);
    assert_eq!(session.phase(), Phase::VerifyingHeaders, "{:?}", session.failure());

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
    session.on_status(&blocks, &tds, 5_000, 0);

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
    session.on_status(&blocks, &tds, 5_000, 0);

    session.on_headers(&[blocks[0].header.clone()]);
    session.on_headers(&[foreign_genesis]);

    assert_eq!(session.phase(), Phase::Failed);
    // Told apart from merely running out of answers: this one is the peer's
    // doing, and it is charged for it.
    assert_eq!(session.failure(), Some(&SnapFailure::ForeignGenesis));
}

/// **The walk may not vouch for itself.** Headers are written to the store as
/// the walk verifies them, so the test for "a block we already had" must ask
/// something the walk cannot have written: the canonical index. Otherwise a
/// peer could walk the client back to an invented genesis and have the client
/// agree, on the strength of headers it had just been handed.
#[test]
fn the_header_walk_cannot_satisfy_its_own_anchor() {
    let f = fixture(20);
    let foreign_genesis = header(0, B256::ZERO, B256::repeat_byte(9), 100);
    let (blocks, tds) = chain(1, 3, foreign_genesis.hash(), 100, f.state_root);

    let mut session = f.session(HeaderVerifier::new());
    session.on_status(&blocks, &tds, 5_000, 0);

    // Feed the walk down to the foreign genesis, then offer it a second time:
    // by then its header is in the store, written by the walk itself.
    session.on_headers(&[blocks[1].header.clone()]);
    session.on_headers(&[blocks[0].header.clone()]);
    session.on_headers(&[foreign_genesis.clone()]);
    assert_eq!(session.phase(), Phase::Failed, "the walk vouched for itself");
    assert_eq!(session.failure(), Some(&SnapFailure::ForeignGenesis));

    // And the chain really is in the store by hash -- which is why the check
    // has to look elsewhere.
    assert!(
        f.store.has_block(foreign_genesis.hash()).unwrap(),
        "the test is not exercising what it claims"
    );
}

/// Blocks that do not form a chain are refused before anything expensive
/// happens.
#[test]
fn a_status_whose_blocks_do_not_link_is_refused() {
    let f = fixture(20);
    let (mut blocks, tds) = chain(1, 4, B256::ZERO, 100, f.state_root);
    blocks[2] = block(header(3, B256::repeat_byte(0x55), f.state_root, 100));

    let mut session = f.session(HeaderVerifier::new());
    session.on_status(&blocks, &tds, 5_000, 0);
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
    session.on_status(&blocks, &tds, 5_000, 0);
    assert_eq!(session.failure(), Some(&SnapFailure::BadDifficulty));
}

#[test]
fn an_empty_status_is_refused() {
    let f = fixture(10);
    let mut session = f.session(HeaderVerifier::new());
    session.on_status(&[], &[], 0, 0);
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

    // The peer keeps its own block store, as a different node would. The
    // client knows only its genesis, so the header walk has real work to do.
    let peer_dir = tempfile::tempdir().expect("tempdir");
    let peer_store = Arc::new(BlockStore::open(peer_dir.path()).expect("open"));
    for (b, td) in blocks.iter().zip(tds.iter()) {
        let hash = b.header.hash();
        peer_store.put_header_with_hash(hash, &b.header).expect("header");
        peer_store.put_canonical_hash(b.header.number, hash).expect("index");
        peer_store.put_total_difficulty(hash, *td).expect("td");
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
        peer_store.clone(),
        f.trie.clone() as Arc<dyn TrieStore>,
        config.clone(),
    );

    let mut actions = session.on_status(&blocks, &tds, 8_000, 0);
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
            Action::RequestStatus => session.on_status(&blocks, &tds, 8_000, 0),
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
                session.on_chunk(from, &response.payload, Refusal::None)
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

/// Snapshot sync is for a node that has nothing, not one that is behind. A
/// node that has executed blocks catches up the ordinary way rather than
/// throwing away the state it has.
#[test]
fn a_node_that_has_executed_blocks_does_not_snap_sync() {
    use crate::manager::SyncManager;
    use crate::SyncService;
    use rustock_networking::peers::PeerStore;

    let f = fixture(10);
    let number = 500_000u64;
    let h = header(number, B256::repeat_byte(1), f.state_root, 100);
    let hash = h.hash();
    f.store.put_header_with_hash(hash, &h).expect("header");
    f.store.set_exec_head(hash, f.state_root).expect("exec head");

    let manager = Arc::new(SyncManager::new(
        f.store.clone(),
        Arc::new(HeaderVerifier::new()),
        Arc::new(PeerStore::new()),
    ));
    let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let mut service = SyncService::new(manager, Arc::new(PeerStore::new()), rx)
        .with_trie_store_for_test(f.trie.clone() as Arc<dyn TrieStore>);

    service.start_snap_sync(SnapConfig::default(), Arc::new(HeaderVerifier::new()));
    assert!(service.snap_phase().is_none(), "snap sync started on a node with state");
}

/// **The blocks behind the checkpoint are never executed**, so nothing
/// downstream would ever notice a body that does not belong to its header.
/// The session checks it, or no one does.
#[test]
fn a_body_that_does_not_match_its_header_is_refused() {
    use rustock_core::Transaction;

    let f = fixture(20);
    let genesis = f.with_genesis();
    let (blocks, tds) = chain(1, 3, genesis, 100, f.state_root);

    let mut session = f.session(HeaderVerifier::new());
    session.on_status(&blocks, &tds, 5_000, 0);
    // Anchor the walk: the parent of block 1 is our genesis.
    session.on_headers(&[blocks[1].header.clone()]);
    // The chunk requests the walk's success produces: dropping them would
    // leave every slice marked in flight with nothing coming back.
    let started = session.on_headers(&[blocks[0].header.clone()]);
    assert_eq!(session.phase(), Phase::DownloadingState, "{:?}", session.failure());

    // Skip ahead to the block phase by finishing the (tiny) state.
    // Here it is enough to call on_blocks directly once the phase allows it:
    // drive the state download to completion first.
    let peer_dir = tempfile::tempdir().expect("tempdir");
    let peer_store = Arc::new(BlockStore::open(peer_dir.path()).expect("open"));
    for (b, td) in blocks.iter().zip(tds.iter()) {
        let hash = b.header.hash();
        peer_store.put_header_with_hash(hash, &b.header).expect("header");
        peer_store.put_canonical_hash(b.header.number, hash).expect("index");
        peer_store.put_total_difficulty(hash, *td).expect("td");
    }
    let server = SnapServer::new(
        peer_store,
        f.trie.clone() as Arc<dyn TrieStore>,
        SnapConfig { server_enabled: true, ..SnapConfig::default() },
    );

    let checkpoint = blocks.last().unwrap().header.number;
    let mut actions = started;
    let mut guard = 0;
    while session.phase() == Phase::DownloadingState {
        guard += 1;
        assert!(guard < 1000, "state download stalled");
        if actions.is_empty() {
            actions = session.poll();
            assert!(!actions.is_empty(), "no work and not finished");
        }
        let action = actions.remove(0);
        if let Action::RequestChunk { from, budget, .. } = action {
            let response = server
                .chunk(&SnapChunkRequest {
                    id: 1,
                    block_number: checkpoint,
                    from,
                    chunk_size: budget,
                    state_root: Some(f.state_root),
                })
                .expect("server answers");
            actions.extend(session.on_chunk(from, &response.payload, Refusal::None));
        }
    }
    assert_eq!(session.phase(), Phase::DownloadingBlocks, "{:?}", session.failure());

    // A genuine header with a transaction it never carried.
    let mut forged = blocks[0].clone();
    forged.transactions.push(Transaction::default());
    session.on_blocks(&[forged], &[tds[0]]);

    assert_eq!(session.phase(), Phase::Failed);
    assert!(
        matches!(session.failure(), Some(SnapFailure::BadBody { what: "transactions", .. })),
        "wrong failure: {:?}",
        session.failure()
    );
}

// ------------------------------------------------------- blaming the peer

use alloy_primitives::B512;
use crate::snap::driver::{Blame, SnapDriver};
use rustock_networking::protocol::{P2pMessage, RskSubMessage};
use rustock_networking::protocol::snap::Refusal;
use rustock_networking::scoring::EventType;

/// What a driver asked for, and under which id, so a test can answer it.
fn asked(out: &crate::snap::driver::Outbound) -> (u64, &'static str) {
    let P2pMessage::RskMessage(m) = &out.message else { panic!("not an rsk message") };
    match &m.sub_message {
        RskSubMessage::SnapStatusRequest(r) => (r.id, "status"),
        RskSubMessage::BlockHeadersRequest(r) => (r.id, "headers"),
        RskSubMessage::SnapChunkRequest(r) => (r.id, "chunk"),
        RskSubMessage::SnapBlocksRequest(r) => (r.id, "blocks"),
        other => panic!("unexpected request {other:?}"),
    }
}

/// A driver whose session has been driven to the state-download phase, with a
/// chunk request outstanding. Returns the driver, the peers, and the
/// (id, offset) of that outstanding request.
fn driver_awaiting_a_chunk(
    f: &Fixture,
    blocks: &[Block],
    tds: &[U256],
    peers: &[B512],
) -> (SnapDriver, u64, u64) {
    let mut driver = SnapDriver::new(f.session(HeaderVerifier::new()));

    let mut out = driver.poll(peers);
    let (status_id, kind) = asked(&out[0]);
    assert_eq!(kind, "status");
    out = driver.on_status(status_id, peers[0], blocks, tds, 8_000, 0, peers);

    // Walk the headers down to genesis, answering from the offered chain.
    let mut guard = 0;
    loop {
        guard += 1;
        assert!(guard < 100, "header walk did not finish");
        let (id, kind) = asked(&out[0]);
        if kind == "chunk" {
            break;
        }
        assert_eq!(kind, "headers");
        let P2pMessage::RskMessage(m) = &out[0].message else { unreachable!() };
        let RskSubMessage::BlockHeadersRequest(req) = m.sub_message.clone() else {
            unreachable!()
        };
        let answer: Vec<Header> = blocks
            .iter()
            .rev()
            .map(|b| b.header.clone())
            .filter(|h| h.hash() == req.query.hash)
            .collect();
        out = driver.on_headers(id, peers[0], &answer, peers);
        assert!(!out.is_empty(), "the walk stalled");
    }

    let P2pMessage::RskMessage(m) = &out[0].message else { unreachable!() };
    let RskSubMessage::SnapChunkRequest(req) = &m.sub_message else { unreachable!() };
    (driver, req.id, req.from)
}

/// **The point of the whole exercise.** A peer whose chunk fails its proof is
/// named and charged. Refusing bad data without charging anyone costs the
/// sender nothing, so it can do it again immediately, forever.
#[test]
fn a_peer_whose_chunk_fails_its_proof_is_charged() {
    let f = fixture(250);
    let genesis = f.with_genesis();
    let (blocks, tds) = chain(1, 3, genesis, 100, f.state_root);
    let peers = vec![B512::repeat_byte(1), B512::repeat_byte(2)];

    let (mut driver, id, from) = driver_awaiting_a_chunk(&f, &blocks, &tds, &peers);
    assert!(driver.take_blame().is_empty(), "nobody should owe anything yet");

    // A genuine chunk with one byte flipped, from a peer we can name.
    let liar = B512::repeat_byte(9);
    let mut payload = served_chunk(&f, from);
    tamper(&mut payload);
    driver.on_chunk(id, liar, &payload, Refusal::None, &peers);

    let blame = driver.take_blame();
    assert_eq!(blame.len(), 1, "expected exactly one charge, got {blame:?}");
    assert_eq!(blame[0].peer, liar, "charged the wrong peer");
    assert_eq!(blame[0].event, EventType::InvalidMessage);
}

/// And the charge lands on whoever *sent* it, not on whoever was asked. Taking
/// a chunk from any peer is deliberate -- a valid chunk is valid whatever its
/// route -- but that only works if blame follows the sender.
#[test]
fn the_charge_follows_the_sender_not_the_peer_that_was_asked() {
    let f = fixture(250);
    let genesis = f.with_genesis();
    let (blocks, tds) = chain(1, 3, genesis, 100, f.state_root);
    let asked_peer = B512::repeat_byte(1);
    let peers = vec![asked_peer, B512::repeat_byte(2)];

    let (mut driver, id, from) = driver_awaiting_a_chunk(&f, &blocks, &tds, &peers);
    driver.take_blame();

    let interloper = B512::repeat_byte(77);
    let mut payload = served_chunk(&f, from);
    tamper(&mut payload);
    driver.on_chunk(id, interloper, &payload, Refusal::None, &peers);

    let blame = driver.take_blame();
    assert_eq!(blame.len(), 1);
    assert_eq!(blame[0].peer, interloper, "blamed the peer we asked, not the one that answered");
    assert_ne!(blame[0].peer, asked_peer);
}

/// A peer that serves honestly owes nothing. An implementation that charged on
/// every answer would ban the network.
#[test]
fn an_honest_peer_is_never_charged() {
    let f = fixture(250);
    let genesis = f.with_genesis();
    let (blocks, tds) = chain(1, 3, genesis, 100, f.state_root);
    let peers = vec![B512::repeat_byte(1), B512::repeat_byte(2)];

    let (mut driver, id, from) = driver_awaiting_a_chunk(&f, &blocks, &tds, &peers);
    driver.take_blame();

    driver.on_chunk(id, peers[0], &served_chunk(&f, from), Refusal::None, &peers);
    assert!(driver.take_blame().is_empty(), "charged an honest peer");
}

/// A peer declining to serve is behaving correctly -- it may have pruned the
/// state, or be on another chain. It is dropped from the rotation, not
/// punished: punishing it would teach the network to stop offering.
#[test]
fn a_peer_that_declines_is_dropped_not_punished() {
    let f = fixture(250);
    let genesis = f.with_genesis();
    let (blocks, tds) = chain(1, 3, genesis, 100, f.state_root);
    let peers = vec![B512::repeat_byte(1), B512::repeat_byte(2)];

    let (mut driver, id, _from) = driver_awaiting_a_chunk(&f, &blocks, &tds, &peers);
    driver.take_blame();

    let pruned = B512::repeat_byte(5);
    let declined = ChunkPayload::Proved { entries: Vec::new(), witness: Vec::new() };
    driver.on_chunk(id, pruned, &declined, Refusal::None, &peers);

    assert!(driver.take_blame().is_empty(), "punished a peer for declining");
    assert!(driver.unhelpful().contains(&pruned), "kept asking a peer that cannot serve");
}

/// The same for an rskj peer speaking the older chunk format: not a fault, but
/// not worth asking again either.
#[test]
fn an_older_peer_is_dropped_not_punished() {
    let f = fixture(250);
    let genesis = f.with_genesis();
    let (blocks, tds) = chain(1, 3, genesis, 100, f.state_root);
    let peers = vec![B512::repeat_byte(1), B512::repeat_byte(2)];

    let (mut driver, id, _from) = driver_awaiting_a_chunk(&f, &blocks, &tds, &peers);
    driver.take_blame();

    let rskj = B512::repeat_byte(6);
    driver.on_chunk(id, rskj, &ChunkPayload::Legacy(vec![0xC1, 0x80].into()), Refusal::None, &peers);

    assert!(driver.take_blame().is_empty(), "punished an rskj peer for being rskj");
    assert!(driver.unhelpful().contains(&rskj));
}

/// A peer that offers a chain which does not link is charged for the status,
/// not merely ignored.
#[test]
fn a_peer_offering_a_broken_chain_is_charged() {
    let f = fixture(20);
    let (mut blocks, tds) = chain(1, 4, B256::ZERO, 100, f.state_root);
    blocks[2] = block(header(3, B256::repeat_byte(0x55), f.state_root, 100));

    let peers = vec![B512::repeat_byte(1)];
    let mut driver = SnapDriver::new(f.session(HeaderVerifier::new()));
    let out = driver.poll(&peers);
    let (id, _) = asked(&out[0]);

    let liar = B512::repeat_byte(3);
    driver.on_status(id, liar, &blocks, &tds, 5_000, 0, &peers);

    let blame = driver.take_blame();
    assert_eq!(blame.len(), 1, "got {blame:?}");
    assert_eq!(blame[0].peer, liar);
    assert_eq!(blame[0].event, EventType::InvalidMessage);
}

/// A header that fails proof of work is charged as a bad header, which scores
/// differently from a merely malformed message.
#[test]
fn a_peer_serving_an_unmined_header_is_charged_for_the_header() {
    let f = fixture(20);
    let genesis = f.with_genesis();
    let (blocks, tds) = chain(1, 3, genesis, 100, f.state_root);
    let checkpoint = blocks.last().unwrap().header.number;
    let peers = vec![B512::repeat_byte(1)];

    let mut driver =
        SnapDriver::new(f.session(HeaderVerifier::new().with_static_rule(RefuseBelow(checkpoint))));
    let out = driver.poll(&peers);
    let (status_id, _) = asked(&out[0]);
    let out = driver.on_status(status_id, peers[0], &blocks, &tds, 5_000, 0, &peers);
    driver.take_blame();

    let (headers_id, kind) = asked(&out[0]);
    assert_eq!(kind, "headers");
    let liar = B512::repeat_byte(4);
    driver.on_headers(headers_id, liar, &[blocks[blocks.len() - 2].header.clone()], &peers);

    let blame = driver.take_blame();
    assert_eq!(blame.len(), 1, "got {blame:?}");
    assert_eq!(blame[0].peer, liar);
    assert_eq!(blame[0].event, EventType::InvalidHeader);
}

/// A request nobody answered is the peer's fault too, or a slow peer would
/// hold work forever at no cost.
#[test]
fn a_timed_out_request_is_charged() {
    let f = fixture(20);
    let peers = vec![B512::repeat_byte(1)];
    let mut driver = SnapDriver::new(f.session(HeaderVerifier::new()));
    driver.poll(&peers);

    let long_after = std::time::Instant::now() + std::time::Duration::from_secs(600);
    assert_eq!(driver.expire(long_after), 1, "the request should have expired");

    let blame: Vec<Blame> = driver.take_blame();
    assert_eq!(blame.len(), 1);
    assert_eq!(blame[0].peer, peers[0]);
    assert_eq!(blame[0].event, EventType::TimeoutMessage);
}

/// Serves a real chunk for `from` out of the fixture's trie.
fn served_chunk(f: &Fixture, from: u64) -> ChunkPayload {
    let root_message = f.trie.get(f.state_root.as_slice()).expect("root");
    let root = TrieNode::from_message(&root_message, f.trie.as_ref());
    let proof = rustock_trie::snapshot_proof::prove_chunk(&root, from, 600, f.trie.as_ref());
    ChunkPayload::Proved {
        entries: proof
            .entries
            .iter()
            .map(|e| rustock_networking::protocol::snap::SnapEntry {
                message: e.message.clone().into(),
                long_values: e.long_values.iter().map(|v| v.clone().into()).collect(),
            })
            .collect(),
        witness: proof.witness.iter().map(|w| w.clone().into()).collect(),
    }
}

/// Flips a bit in the middle of a chunk, so every node is genuine but one.
fn tamper(payload: &mut ChunkPayload) {
    let ChunkPayload::Proved { entries, .. } = payload else { panic!("not a proved chunk") };
    assert!(!entries.is_empty(), "nothing to tamper with");
    let victim = entries.len() / 2;
    let mut bytes = entries[victim].message.to_vec();
    let last = bytes.len() - 1;
    bytes[last] ^= 0x01;
    entries[victim].message = bytes.into();
}

/// **One peer must not be able to end snapshot sync.** A malformed status used
/// to fail the session, and nothing ever started another: a single bad message
/// from a single peer turned the feature off for the life of the process, at
/// no cost to the sender.
#[tokio::test]
async fn a_bad_peer_does_not_end_snapshot_sync() {
    use crate::events::SyncEvent;
    use crate::manager::SyncManager;
    use crate::snap::session::Phase;
    use crate::SyncService;
    use rustock_networking::peers::PeerStore;

    let f = fixture(20);
    f.with_genesis();
    // A chain whose blocks do not link: refused on sight.
    let (mut blocks, tds) = chain(1, 4, B256::ZERO, 100, f.state_root);
    blocks[2] = block(header(3, B256::repeat_byte(0x55), f.state_root, 100));

    let peers = Arc::new(PeerStore::new());
    let liar = B512::repeat_byte(9);
    let (tx, _rx) = tokio::sync::mpsc::channel(8);
    peers.add_peer(liar, tx).await;

    let manager = Arc::new(SyncManager::new(
        f.store.clone(),
        Arc::new(HeaderVerifier::new()),
        peers.clone(),
    ));
    let (_event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut service = SyncService::new(manager, peers, event_rx)
        .with_trie_store_for_test(f.trie.clone() as Arc<dyn TrieStore>);

    service.start_snap_sync(SnapConfig::default(), Arc::new(HeaderVerifier::new()));
    assert_eq!(service.snap_phase(), Some(Phase::AwaitingStatus));

    // The driver has to have asked before an answer means anything.
    service.on_tick().await;

    // Answer with the broken chain, as the lying peer.
    service
        .handle_event(SyncEvent::SnapStatusResponse {
            peer: liar,
            id: service.snap_request_id_for_test().expect("a status request is outstanding"),
            blocks: blocks.clone(),
            difficulties: tds.clone(),
            trie_size: 5_000,
            chunk_grid: 0,
        })
        .await;

    // The session failed, and another one took its place rather than the
    // feature switching itself off.
    assert!(
        service.snap_phase().is_some(),
        "one bad status ended snapshot sync for good"
    );
    assert_eq!(service.snap_phase(), Some(Phase::AwaitingStatus));
    assert!(service.snap_attempts_for_test() >= 2, "no second attempt was made");
}

/// The charge must actually reach peer scoring. The driver naming a culprit is
/// only useful if the service hands it to the thing that punishes.
#[tokio::test]
async fn a_charge_reaches_peer_scoring() {
    use crate::events::SyncEvent;
    use crate::manager::SyncManager;
    use crate::SyncService;
    use rustock_networking::peers::PeerStore;
    use rustock_networking::scoring::ScoringService;

    let f = fixture(20);
    f.with_genesis();
    let (mut blocks, tds) = chain(1, 4, B256::ZERO, 100, f.state_root);
    blocks[2] = block(header(3, B256::repeat_byte(0x55), f.state_root, 100));

    let peers = Arc::new(PeerStore::new());
    let liar = B512::repeat_byte(9);
    let (tx, _rx) = tokio::sync::mpsc::channel(8);
    peers.add_peer(liar, tx).await;

    let scoring = Arc::new(ScoringService::in_memory());
    let before = scoring.with(|m| m.node_has_good_reputation(liar));
    assert!(before, "a peer starts in good standing");

    let manager = Arc::new(SyncManager::new(
        f.store.clone(),
        Arc::new(HeaderVerifier::new()),
        peers.clone(),
    ));
    let (_event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut service = SyncService::new(manager, peers, event_rx)
        .with_trie_store_for_test(f.trie.clone() as Arc<dyn TrieStore>)
        .with_scoring(Some(scoring.clone()));

    service.start_snap_sync(SnapConfig::default(), Arc::new(HeaderVerifier::new()));
    service.on_tick().await;

    let id = service.snap_request_id_for_test().expect("a status request is outstanding");
    service
        .handle_event(SyncEvent::SnapStatusResponse {
            peer: liar,
            id,
            blocks,
            difficulties: tds,
            trie_size: 5_000,
            chunk_grid: 0,
        })
        .await;

    // The peer now carries a recorded InvalidMessage and a negative score.
    let info = scoring.with(|m| m.information());
    let entry = info
        .iter()
        .find(|i| i.kind == "node" && i.counters.iter().any(|(_, n)| *n > 0))
        .unwrap_or_else(|| panic!("no peer was scored at all: {info:?}"));

    assert!(
        entry.counters.iter().any(|(name, n)| *name == "invalidMessages" && *n > 0),
        "the charge did not land as an invalid message: {:?}",
        entry.counters
    );
    assert!(entry.score < 0, "a charged peer should have lost points: {}", entry.score);
}
