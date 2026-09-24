use super::*;
use alloy_primitives::{Address, B256, U256, Bytes, B512};
use tempfile::tempdir;

fn dummy_header(number: u64, parent: B256, difficulty: U256) -> Header {
    Header {
        number,
        parent_hash: parent,
        ommers_hash: B256::ZERO,
        beneficiary: Address::ZERO,
        state_root: B256::ZERO,
        transactions_root: B256::ZERO,
        receipts_root: B256::ZERO,
        logs_bloom: Default::default(),
        extension_data: None,
        difficulty,
        gas_limit: U256::from(8_000_000),
        gas_used: 0,
        timestamp: number * 15,
        extra_data: Bytes::default(),
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

// -- SyncManager tests (validation logic) --------------------------------

#[tokio::test]
async fn test_sync_manager_processing() {
    let dir = tempdir().unwrap();
    let store = Arc::new(BlockStore::open(dir.path()).unwrap());

    let genesis = dummy_header(0, B256::ZERO, U256::from(1));
    let genesis_hash = genesis.hash();
    store.update_head(&genesis, U256::from(1)).unwrap();

    let verifier = Arc::new(HeaderVerifier::new()
        .with_parent_rule(rustock_core::validation::BlockNumberRule)
        .with_parent_rule(rustock_core::validation::ParentHashRule));
    let peer_store = Arc::new(rustock_networking::peers::PeerStore::new());
    let manager = SyncManager::new(store.clone(), verifier, peer_store);

    // 1. Valid sequential block
    let b1 = dummy_header(1, genesis_hash, U256::from(10));
    manager.handle_headers_response(vec![b1.clone()]).unwrap();
    assert_eq!(store.head().unwrap(), Some(b1.hash()));
    assert_eq!(store.total_difficulty(b1.hash()).unwrap(), Some(U256::from(11)));

    // 2. Duplicate block (should be ignored)
    manager.handle_headers_response(vec![b1.clone()]).unwrap();
    assert_eq!(store.head().unwrap(), Some(b1.hash()));

    // 3. Extension block
    let b2 = dummy_header(2, b1.hash(), U256::from(5));
    manager.handle_headers_response(vec![b2.clone()]).unwrap();
    assert_eq!(store.head().unwrap(), Some(b2.hash()));

    // 4. Gap block (parent unknown) — stored with TD = difficulty only
    let b4 = dummy_header(4, B256::repeat_byte(0xee), U256::from(1));
    let b4_hash = b4.hash();
    manager.handle_headers_response(vec![b4]).unwrap();
    assert_eq!(store.head().unwrap(), Some(b2.hash()), "Head should not change");
    assert!(store.header(b4_hash).unwrap().is_some(), "Gap block should be stored");
    assert_eq!(store.total_difficulty(b4_hash).unwrap(), Some(U256::from(1)));
}

#[tokio::test]
async fn test_invalid_header_rejected_when_parent_known() {
    let dir = tempdir().unwrap();
    let store = Arc::new(BlockStore::open(dir.path()).unwrap());

    let genesis = dummy_header(0, B256::ZERO, U256::from(1));
    let genesis_hash = genesis.hash();
    store.update_head(&genesis, U256::from(1)).unwrap();

    let verifier = Arc::new(HeaderVerifier::new()
        .with_parent_rule(rustock_core::validation::BlockNumberRule)
        .with_parent_rule(rustock_core::validation::ParentHashRule));
    let peer_store = Arc::new(rustock_networking::peers::PeerStore::new());
    let manager = SyncManager::new(store.clone(), verifier, peer_store);

    // Header claims parent is genesis but has wrong block number
    let bad = dummy_header(5, genesis_hash, U256::from(50));
    let bad_hash = bad.hash();
    manager.handle_headers_response(vec![bad]).unwrap();

    assert!(store.header(bad_hash).unwrap().is_none(), "Invalid header should be rejected");
    assert_eq!(store.head().unwrap(), Some(genesis_hash));
}

#[tokio::test]
async fn test_parent_is_taken_from_parent_hash_not_from_batch_position() {
    // The mainnet defect. A chunk can contain a fork header at a height it
    // already covers; if the parent is chosen because it is the previous entry
    // at `number - 1`, the next header is verified against the wrong block.
    // Observed on RSK mainnet: #9,236,893 checked against #9,236,891.
    let dir = tempdir().unwrap();
    let store = Arc::new(BlockStore::open(dir.path()).unwrap());

    let genesis = dummy_header(0, B256::ZERO, U256::from(1));
    let genesis_hash = genesis.hash();
    store.update_head(&genesis, U256::from(1)).unwrap();

    let verifier = Arc::new(HeaderVerifier::new()
        .with_parent_rule(rustock_core::validation::BlockNumberRule)
        .with_parent_rule(rustock_core::validation::ParentHashRule));
    let peer_store = Arc::new(rustock_networking::peers::PeerStore::new());
    let manager = SyncManager::new(store.clone(), verifier, peer_store);

    // The real chain: genesis <- a1 <- a2.
    let a1 = dummy_header(1, genesis_hash, U256::from(10));
    manager.handle_headers_response(vec![a1.clone()]).unwrap();

    // A competing block at height 1, also building on genesis.
    let mut fork1 = dummy_header(1, genesis_hash, U256::from(11));
    fork1.timestamp = 999;
    assert_ne!(fork1.hash(), a1.hash(), "fork must be a different block");

    // A batch carrying the fork immediately before a2. a2's parent is a1.
    let a2 = dummy_header(2, a1.hash(), U256::from(5));
    let a2_hash = a2.hash();
    manager.handle_headers_response(vec![fork1.clone(), a2.clone()]).unwrap();

    assert!(
        store.header(a2_hash).unwrap().is_some(),
        "a2 must be accepted: its parent_hash names a1, which is stored. \
         Verifying it against the fork at the same height rejects valid chain data."
    );
    assert_eq!(
        store.total_difficulty(a2_hash).unwrap(),
        Some(U256::from(16)),
        "TD must accumulate through a1 (1+10+5), not through the fork"
    );
}

#[tokio::test]
async fn test_competing_headers_at_one_height_are_both_kept() {
    // Deduplicating a chunk by block number discards whichever block at a
    // contested height arrives second -- which may be the canonical one.
    let dir = tempdir().unwrap();
    let store = Arc::new(BlockStore::open(dir.path()).unwrap());

    let genesis = dummy_header(0, B256::ZERO, U256::from(1));
    let genesis_hash = genesis.hash();
    store.update_head(&genesis, U256::from(1)).unwrap();

    let verifier = Arc::new(HeaderVerifier::new()
        .with_parent_rule(rustock_core::validation::BlockNumberRule)
        .with_parent_rule(rustock_core::validation::ParentHashRule));
    let peer_store = Arc::new(rustock_networking::peers::PeerStore::new());
    let manager = SyncManager::new(store.clone(), verifier, peer_store);

    let a1 = dummy_header(1, genesis_hash, U256::from(10));
    let mut b1 = dummy_header(1, genesis_hash, U256::from(20));
    b1.timestamp = 777;
    assert_ne!(a1.hash(), b1.hash());

    manager.handle_headers_response(vec![a1.clone(), b1.clone()]).unwrap();

    assert!(store.header(a1.hash()).unwrap().is_some(), "first block at height 1 kept");
    assert!(
        store.header(b1.hash()).unwrap().is_some(),
        "the competing block at height 1 must also be kept -- that is what a fork is"
    );
    // Fork choice still picks the heavier one.
    assert_eq!(store.head().unwrap(), Some(b1.hash()), "heavier fork wins");
}

#[tokio::test]
async fn test_difficulty_is_checked_against_the_true_parent() {
    // Reproduces the shape of RSK mainnet #9,236,891/892/893, where the middle
    // block carries an uncle and sits 7s before its child while the grandparent
    // sits 34s before it. Verifying against the grandparent flips the
    // adjustment sign and rejects a valid header.
    use rustock_core::config::ChainConfig;
    use rustock_core::validation::DifficultyRule;

    let dir = tempdir().unwrap();
    let store = Arc::new(BlockStore::open(dir.path()).unwrap());
    let config = Arc::new(ChainConfig::mainnet());

    // Heights above papyrus200 so the divisor is 400, matching mainnet.
    let base = config.activation_heights.papyrus200 + 1_000_000;
    let step = |d: U256| d / U256::from(400);

    let mut g = dummy_header(base, B256::ZERO, U256::from(6_661_282_993_281_678_603_550u128));
    g.timestamp = 1_789_321_796;
    let g_hash = g.hash();
    store.update_head(&g, g.difficulty).unwrap();

    // Parent: 27s after the grandparent, one uncle, difficulty stepped down.
    let mut parent = dummy_header(base + 1, g_hash, U256::ZERO);
    parent.timestamp = 1_789_321_823;
    parent.uncle_count = 1;
    // (1 + 1 uncle) * 14 = 28 > 27 elapsed  ->  sign +1
    parent.difficulty = g.difficulty + step(g.difficulty);
    let parent_hash = parent.hash();

    // Child: 7s after the parent, no uncles.  (1 + 0) * 14 = 14 > 7  ->  sign +1
    let mut child = dummy_header(base + 2, parent_hash, U256::ZERO);
    child.timestamp = 1_789_321_830;
    child.difficulty = parent.difficulty + step(parent.difficulty);
    let child_hash = child.hash();

    // Against the grandparent the elapsed time would be 34s, so the sign would
    // be -1 and the expected difficulty would come out low. Assert the two
    // really do differ, or the test proves nothing.
    let against_grandparent = g.difficulty - step(g.difficulty);
    assert_ne!(child.difficulty, against_grandparent);

    let verifier = Arc::new(HeaderVerifier::new()
        .with_parent_rule(rustock_core::validation::BlockNumberRule)
        .with_parent_rule(rustock_core::validation::ParentHashRule)
        .with_parent_rule(DifficultyRule { config: config.clone() }));
    let peer_store = Arc::new(rustock_networking::peers::PeerStore::new());
    let manager = SyncManager::new(store.clone(), verifier, peer_store);

    // Store the true parent first, as the node would have from an earlier chunk.
    manager.handle_headers_response(vec![parent.clone()]).unwrap();
    assert!(store.header(parent_hash).unwrap().is_some(), "parent accepted");

    // A competing block at the parent's height, building on the grandparent
    // 34s later -- so its own difficulty is a valid -1 step. This is the shape
    // that produced the mainnet rejection: its difficulty is exactly the
    // "expected" value that was logged for #9,236,893.
    let mut fork = dummy_header(base + 1, g_hash, against_grandparent);
    fork.timestamp = 1_789_321_830;
    assert_ne!(fork.hash(), parent_hash, "fork must differ from the true parent");

    // The fork arrives immediately before the child. Positionally it looks like
    // the parent -- same height, right order -- but it is a different block.
    manager.handle_headers_response(vec![fork.clone(), child.clone()]).unwrap();

    assert!(
        store.header(child_hash).unwrap().is_some(),
        "child must be accepted: its difficulty is a correct +1 step from the \
         block its parent_hash names. Checking it against the fork at the same \
         height yields a -1 step and rejects valid chain data -- and the missing \
         canonical entry at its height is what wedges execution."
    );
}

#[tokio::test]
async fn test_follow_buffer_gap_triggers_refetch_instead_of_stalling() {
    // The silent stall. In follow mode the lowest buffered block is executed
    // only if it builds on the executed head; if it does not, the old code
    // returned and nothing ever fetched the block in between. Later blocks piled
    // up behind the hole and execution stopped permanently -- observed on
    // mainnet as a node that executed nothing for twenty hours while downloading
    // tips the whole time, with no error logged.
    //
    // The resync trigger cannot catch it: that compares the *downloaded* head
    // against peers, and the downloaded head is at the tip.
    let dir = tempdir().unwrap();
    let store = Arc::new(BlockStore::open(dir.path()).unwrap());

    let genesis = dummy_header(0, B256::ZERO, U256::from(1));
    store.update_head(&genesis, U256::from(1)).unwrap();

    // Executed head at #10.
    let mut parent = genesis.clone();
    for n in 1..=10u64 {
        let h = dummy_header(n, parent.hash(), U256::from(1));
        store.put_header(&h).unwrap();
        store.put_canonical_hash(n, h.hash()).unwrap();
        parent = h;
    }
    let head10 = parent.clone();
    store.set_exec_head(head10.hash(), head10.state_root).unwrap();

    let verifier = Arc::new(HeaderVerifier::new());
    let peer_store = Arc::new(rustock_networking::peers::PeerStore::new());
    let manager = Arc::new(SyncManager::new(store.clone(), verifier, peer_store.clone()));
    let (_tx, event_rx) = mpsc::unbounded_channel();
    let mut service = SyncService::new(manager, peer_store, event_rx);
    service.state = SyncState::Following;
    service.last_body_height = 99;

    // Buffer #12 and #13 -- #11 never arrived, so nothing links to #10.
    let b11 = dummy_header(11, head10.hash(), U256::from(1));
    let b12 = dummy_header(12, b11.hash(), U256::from(1));
    let b13 = dummy_header(13, b12.hash(), U256::from(1));
    service.follow_buffer.insert(12, (b12.hash(), b12.clone(), vec![], vec![]));
    service.follow_buffer.insert(13, (b13.hash(), b13.clone(), vec![], vec![]));

    // First observation only starts the clock: an out-of-order response may
    // still be on its way.
    service.drain_follow_buffer().await;
    assert_eq!(service.follow_buffer.len(), 2, "a fresh gap must be given time");
    assert!(matches!(service.state, SyncState::Following));

    // Once it has outlived any plausible in-flight response, re-fetch.
    service.follow_gap_since = Some(std::time::Instant::now() - std::time::Duration::from_secs(120));
    service.drain_follow_buffer().await;

    assert!(
        service.follow_buffer.is_empty(),
        "the stale buffer must be dropped so the missing range can be re-fetched"
    );
    assert_eq!(
        service.last_body_height, 10,
        "the body cursor must rewind to the executed head so the sync path \
         re-fetches #11 onward"
    );
    assert!(
        matches!(service.state, SyncState::Idle),
        "must leave follow mode so a sync round can start; staying in Following \
         is what made this stall permanent"
    );
}

#[tokio::test]
async fn test_follow_buffer_keeps_waiting_on_a_same_height_reorg() {
    // The other reason the lowest buffered block may not link: it is the very
    // next block but builds on a different block at our head's height. That is a
    // reorg, handled by the reconcile path, and must NOT trigger a re-fetch --
    // otherwise every tip reorg would throw away the follow buffer.
    let dir = tempdir().unwrap();
    let store = Arc::new(BlockStore::open(dir.path()).unwrap());

    let genesis = dummy_header(0, B256::ZERO, U256::from(1));
    store.update_head(&genesis, U256::from(1)).unwrap();
    let a1 = dummy_header(1, genesis.hash(), U256::from(1));
    store.put_header(&a1).unwrap();
    store.set_exec_head(a1.hash(), a1.state_root).unwrap();

    let verifier = Arc::new(HeaderVerifier::new());
    let peer_store = Arc::new(rustock_networking::peers::PeerStore::new());
    let manager = Arc::new(SyncManager::new(store.clone(), verifier, peer_store.clone()));
    let (_tx, event_rx) = mpsc::unbounded_channel();
    let mut service = SyncService::new(manager, peer_store, event_rx);
    service.state = SyncState::Following;
    service.last_body_height = 1;

    // #2 building on a *different* #1.
    let mut fork1 = dummy_header(1, genesis.hash(), U256::from(2));
    fork1.timestamp = 4242;
    let b2 = dummy_header(2, fork1.hash(), U256::from(1));
    service.follow_buffer.insert(2, (b2.hash(), b2.clone(), vec![], vec![]));

    service.drain_follow_buffer().await;

    assert_eq!(service.follow_buffer.len(), 1, "buffer kept: this is a reorg, not a gap");
    assert_eq!(service.last_body_height, 1, "cursor untouched");
    assert!(matches!(service.state, SyncState::Following), "stays in follow mode");
}

#[tokio::test]
async fn test_execution_watchdog_restarts_a_stalled_pipeline() {
    // Nothing in this service measured execution itself. Header download, the
    // "fell behind" check and the resync trigger all compare the *downloaded*
    // head against peers, and that head keeps climbing while execution is
    // stopped -- so every trigger reported health through twenty hours of a node
    // executing nothing.
    let dir = tempdir().unwrap();
    let store = Arc::new(BlockStore::open(dir.path()).unwrap());

    let genesis = dummy_header(0, B256::ZERO, U256::from(1));
    store.update_head(&genesis, U256::from(1)).unwrap();
    let mut parent = genesis.clone();
    for n in 1..=20u64 {
        let h = dummy_header(n, parent.hash(), U256::from(1));
        store.put_header(&h).unwrap();
        store.put_canonical_hash(n, h.hash()).unwrap();
        store.update_head(&h, U256::from(n + 1)).unwrap();
        parent = h;
    }
    // Downloaded to #20, executed only #5.
    let exec = store.canonical_hash(5).unwrap().unwrap();
    let exec_hdr = store.header(exec).unwrap().unwrap();
    store.set_exec_head(exec, exec_hdr.state_root).unwrap();

    let verifier = Arc::new(HeaderVerifier::new());
    let peer_store = Arc::new(rustock_networking::peers::PeerStore::new());
    let manager = Arc::new(SyncManager::new(store.clone(), verifier, peer_store.clone()));
    let (_tx, event_rx) = mpsc::unbounded_channel();
    let mut service = SyncService::new(manager, peer_store, event_rx);
    service.state = SyncState::Following;
    service.last_body_height = 20;

    // A fresh observation only starts the clock.
    service.on_tick().await;
    assert!(matches!(service.state, SyncState::Following), "not yet a fault");
    assert_eq!(service.last_body_height, 20);

    // Once it has stood still long enough with blocks waiting, restart.
    service.last_exec_progress = std::time::Instant::now() - std::time::Duration::from_secs(300);
    service.on_tick().await;

    assert_eq!(
        service.last_body_height, 5,
        "the body cursor must rewind to the executed head so the range re-queues"
    );
    assert!(
        !matches!(service.state, SyncState::Following),
        "must leave follow mode so a sync round can start"
    );
}

#[tokio::test]
async fn test_execution_watchdog_is_quiet_when_there_is_nothing_to_execute() {
    // A node at the tip executes nothing because there is nothing to execute.
    // Restarting the pipeline for that would be a permanent loop.
    let dir = tempdir().unwrap();
    let store = Arc::new(BlockStore::open(dir.path()).unwrap());

    let genesis = dummy_header(0, B256::ZERO, U256::from(1));
    store.update_head(&genesis, U256::from(1)).unwrap();
    let h1 = dummy_header(1, genesis.hash(), U256::from(1));
    store.put_header(&h1).unwrap();
    store.put_canonical_hash(1, h1.hash()).unwrap();
    store.update_head(&h1, U256::from(2)).unwrap();
    store.set_exec_head(h1.hash(), h1.state_root).unwrap();

    let verifier = Arc::new(HeaderVerifier::new());
    let peer_store = Arc::new(rustock_networking::peers::PeerStore::new());
    let manager = Arc::new(SyncManager::new(store.clone(), verifier, peer_store.clone()));
    let (_tx, event_rx) = mpsc::unbounded_channel();
    let mut service = SyncService::new(manager, peer_store, event_rx);
    service.state = SyncState::Following;
    service.last_body_height = 1;
    service.last_exec_progress = std::time::Instant::now() - std::time::Duration::from_secs(600);

    service.on_tick().await;

    assert!(matches!(service.state, SyncState::Following), "caught up is not stalled");
    assert_eq!(service.last_body_height, 1, "cursor untouched");
}

#[tokio::test]
async fn test_unanswered_follow_body_requests_expire() {
    // The root cause of the stall. A follow-mode body request a peer never
    // answered stayed in the map forever: the block was never re-requested, and
    // both recovery paths stand down while the map is non-empty, so one stuck
    // entry disabled every mechanism that could have noticed.
    let dir = tempdir().unwrap();
    let store = Arc::new(BlockStore::open(dir.path()).unwrap());
    let genesis = dummy_header(0, B256::ZERO, U256::from(1));
    store.update_head(&genesis, U256::from(1)).unwrap();

    let verifier = Arc::new(HeaderVerifier::new());
    let peer_store = Arc::new(rustock_networking::peers::PeerStore::new());
    let manager = Arc::new(SyncManager::new(store.clone(), verifier, peer_store.clone()));
    let (_tx, event_rx) = mpsc::unbounded_channel();
    let mut service = SyncService::new(manager, peer_store, event_rx);
    service.state = SyncState::Following;

    let h = dummy_header(1, genesis.hash(), U256::from(1));
    // A fresh request is left alone.
    service.pending_follow_bodies.insert(7, (h.hash(), h.clone(), std::time::Instant::now()));
    service.on_tick().await;
    assert_eq!(service.pending_follow_bodies.len(), 1, "a fresh request must not be dropped");

    // One that has gone unanswered is dropped, so the block can be re-requested
    // and the recovery paths can see an idle pipeline.
    service.pending_follow_bodies.insert(
        7,
        (h.hash(), h, std::time::Instant::now() - std::time::Duration::from_secs(120)),
    );
    service.on_tick().await;
    assert!(
        service.pending_follow_bodies.is_empty(),
        "an unanswered request must not block the pipeline forever"
    );
}

#[test]
fn test_backlog_decision() {
    // Follow mode cannot execute a backlog already in the store, so a node that
    // executes must clear one before following. This is the decision that made
    // the mainnet stall self-sustaining: both recovery paths rewound the cursor
    // and dropped to Idle, and the next tick saw a small gap and returned to
    // follow mode having executed nothing.
    use super::service::backlog_needs_executing;

    // Blocks downloaded past the executed head: must be executed first.
    assert!(backlog_needs_executing(true, 10, 4));
    assert!(backlog_needs_executing(true, 5, 4), "even one block counts");

    // Caught up: nothing to do.
    assert!(!backlog_needs_executing(true, 4, 4));

    // A node that does not execute cannot fall behind on execution.
    assert!(!backlog_needs_executing(false, 10, 4));
}

// -- SyncHandler tests (event forwarding) --------------------------------

#[tokio::test]
async fn test_sync_handler_forwards_headers() {
    use rustock_networking::protocol::{P2pMessage, RskMessage, RskSubMessage};
    use rustock_networking::protocol::P2pHandler;

    let dir = tempdir().unwrap();
    let store = Arc::new(BlockStore::open(dir.path()).unwrap());
    let verifier = Arc::new(HeaderVerifier::new());
    let peer_store = Arc::new(rustock_networking::peers::PeerStore::new());
    let manager = Arc::new(SyncManager::new(store, verifier, peer_store));

    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let handler = SyncHandler::new(manager, event_tx);

    let h0 = dummy_header(0, B256::ZERO, U256::from(10));
    let resp = rustock_networking::protocol::rsk::BlockHeadersResponse {
        id: 1,
        headers: vec![h0.clone()],
    };
    let msg = P2pMessage::RskMessage(RskMessage::new(RskSubMessage::BlockHeadersResponse(resp)));

    let handler_resp = handler.handle_message(B512::ZERO, &msg);
    assert!(handler_resp.is_none());

    // Event should be forwarded to the channel
    let event = event_rx.try_recv().unwrap();
    match event {
        SyncEvent::HeadersResponse { headers, .. } => {
            assert_eq!(headers.len(), 1);
            assert_eq!(headers[0].number, 0);
        }
        _ => panic!("Expected HeadersResponse event"),
    }
}

#[tokio::test]
async fn test_sync_handler_forwards_block_hash() {
    use rustock_networking::protocol::{P2pMessage, RskMessage, RskSubMessage};
    use rustock_networking::protocol::P2pHandler;
    use rustock_networking::protocol::rsk::BlockHashResponse;

    let dir = tempdir().unwrap();
    let store = Arc::new(BlockStore::open(dir.path()).unwrap());
    let verifier = Arc::new(HeaderVerifier::new());
    let peer_store = Arc::new(rustock_networking::peers::PeerStore::new());
    let manager = Arc::new(SyncManager::new(store, verifier, peer_store));

    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let handler = SyncHandler::new(manager, event_tx);

    let resp = BlockHashResponse { id: 5, hash: B256::repeat_byte(0xab) };
    let msg = P2pMessage::RskMessage(RskMessage::new(RskSubMessage::BlockHashResponse(resp)));

    handler.handle_message(B512::ZERO, &msg);

    match event_rx.try_recv().unwrap() {
        SyncEvent::BlockHashResponse { hash, .. } => {
            assert_eq!(hash, B256::repeat_byte(0xab));
        }
        _ => panic!("Expected BlockHashResponse event"),
    }
}

#[tokio::test]
async fn test_sync_handler_forwards_skeleton() {
    use rustock_networking::protocol::{P2pMessage, RskMessage, RskSubMessage};
    use rustock_networking::protocol::P2pHandler;
    use rustock_networking::protocol::rsk::SkeletonResponse;

    let dir = tempdir().unwrap();
    let store = Arc::new(BlockStore::open(dir.path()).unwrap());
    let verifier = Arc::new(HeaderVerifier::new());
    let peer_store = Arc::new(rustock_networking::peers::PeerStore::new());
    let manager = Arc::new(SyncManager::new(store, verifier, peer_store));

    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let handler = SyncHandler::new(manager, event_tx);

    let resp = SkeletonResponse {
        id: 1,
        block_identifiers: vec![
            BlockIdentifier { hash: B256::repeat_byte(0x01), number: 0 },
            BlockIdentifier { hash: B256::repeat_byte(0x02), number: 192 },
        ],
    };
    let msg = P2pMessage::RskMessage(RskMessage::new(RskSubMessage::SkeletonResponse(resp)));

    handler.handle_message(B512::ZERO, &msg);

    match event_rx.try_recv().unwrap() {
        SyncEvent::SkeletonResponse { identifiers, .. } => {
            assert_eq!(identifiers.len(), 2);
            assert_eq!(identifiers[1].number, 192);
        }
        _ => panic!("Expected SkeletonResponse event"),
    }
}

// -- State machine tests -------------------------------------------------

#[tokio::test]
async fn test_connection_point_binary_search() {
    let dir = tempdir().unwrap();
    let store = Arc::new(BlockStore::open(dir.path()).unwrap());

    // Store genesis only
    let genesis = dummy_header(0, B256::ZERO, U256::from(1));
    store.update_head(&genesis, U256::from(1)).unwrap();

    let verifier = Arc::new(HeaderVerifier::new());
    let peer_store = Arc::new(rustock_networking::peers::PeerStore::new());
    let manager = Arc::new(SyncManager::new(store, verifier, peer_store.clone()));

    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let mut service = SyncService::new(manager, peer_store, event_rx);

    // Simulate: peer is at block 1000, we only have genesis
    let peer = B512::repeat_byte(0x01);
    service.state = SyncState::FindingConnectionPoint {
        peer,
        peer_best: 1000,
        start: 0,
        end: 1000,
    };

    // Probe at midpoint 500: we don't have this block hash
    service.on_block_hash_response(B256::repeat_byte(0xff)).await;
    // Range should narrow: start=0, end=500
    if let SyncState::FindingConnectionPoint { start, end, .. } = &service.state {
        assert_eq!(*start, 0);
        assert_eq!(*end, 500);
    } else {
        panic!("Expected FindingConnectionPoint, got {:?}", service.state);
    }

    // Probe at 250: don't have it
    service.on_block_hash_response(B256::repeat_byte(0xfe)).await;
    if let SyncState::FindingConnectionPoint { start, end, .. } = &service.state {
        assert_eq!(*start, 0);
        assert_eq!(*end, 250);
    } else {
        panic!("Expected FindingConnectionPoint");
    }

    // Probe at 125: don't have it
    service.on_block_hash_response(B256::repeat_byte(0xfd)).await;
    if let SyncState::FindingConnectionPoint { start, end, .. } = &service.state {
        assert_eq!(*start, 0);
        assert_eq!(*end, 125);
    } else {
        panic!("Expected FindingConnectionPoint");
    }

    // Continue narrowing... eventually probe at 1
    // Simulate finding genesis hash — we DO have block 0
    let genesis_hash = dummy_header(0, B256::ZERO, U256::from(1)).hash();

    // Set state to final narrowing: range [0, 1]
    service.state = SyncState::FindingConnectionPoint {
        peer,
        peer_best: 1000,
        start: 0,
        end: 1,
    };
    // Probe at 0: we have genesis
    service.on_block_hash_response(genesis_hash).await;
    // Connection point = 0, should transition to DownloadingSkeleton
    match &service.state {
        SyncState::DownloadingSkeleton { connection_point, .. } => {
            assert_eq!(*connection_point, 0);
        }
        _ => panic!("Expected DownloadingSkeleton, got {:?}", service.state),
    }

    drop(event_tx); // cleanup
}

#[tokio::test]
async fn test_skeleton_to_headers_transition() {
    let dir = tempdir().unwrap();
    let store = Arc::new(BlockStore::open(dir.path()).unwrap());

    let genesis = dummy_header(0, B256::ZERO, U256::from(1));
    store.update_head(&genesis, U256::from(1)).unwrap();

    let verifier = Arc::new(HeaderVerifier::new());
    let peer_store = Arc::new(rustock_networking::peers::PeerStore::new());
    let manager = Arc::new(SyncManager::new(store, verifier, peer_store.clone()));

    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let mut service = SyncService::new(manager, peer_store, event_rx);

    let peer = B512::repeat_byte(0x01);
    service.state = SyncState::DownloadingSkeleton {
        peer,
        peer_best: 384,
        connection_point: 0,
    };

    // Receive skeleton: [0, 192, 384]
    let skeleton = vec![
        BlockIdentifier { hash: B256::repeat_byte(0x01), number: 0 },
        BlockIdentifier { hash: B256::repeat_byte(0x02), number: 192 },
        BlockIdentifier { hash: B256::repeat_byte(0x03), number: 384 },
    ];
    service.on_skeleton_response(skeleton).await;

    match &service.state {
        SyncState::DownloadingHeaders { tracker, skeleton, .. } => {
            assert_eq!(tracker.next_to_process, 1);
            assert_eq!(skeleton.len(), 3);
        }
        _ => panic!("Expected DownloadingHeaders, got {:?}", service.state),
    }

    drop(event_tx);
}

#[tokio::test]
async fn test_empty_skeleton_returns_to_idle() {
    let dir = tempdir().unwrap();
    let store = Arc::new(BlockStore::open(dir.path()).unwrap());

    let verifier = Arc::new(HeaderVerifier::new());
    let peer_store = Arc::new(rustock_networking::peers::PeerStore::new());
    let manager = Arc::new(SyncManager::new(store, verifier, peer_store.clone()));

    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let mut service = SyncService::new(manager, peer_store, event_rx);

    let peer = B512::repeat_byte(0x01);
    service.state = SyncState::DownloadingSkeleton {
        peer,
        peer_best: 100,
        connection_point: 0,
    };

    // Skeleton with only 1 entry → too small → Following
    service.on_skeleton_response(vec![
        BlockIdentifier { hash: B256::ZERO, number: 0 },
    ]).await;

    assert!(matches!(service.state, SyncState::Following));

    drop(event_tx);
}

#[tokio::test]
async fn test_headers_response_advances_chunks() {
    let dir = tempdir().unwrap();
    let store = Arc::new(BlockStore::open(dir.path()).unwrap());

    // Build a small chain: genesis + 4 blocks
    let genesis = dummy_header(0, B256::ZERO, U256::from(1));
    let genesis_hash = genesis.hash();
    store.update_head(&genesis, U256::from(1)).unwrap();

    let verifier = Arc::new(HeaderVerifier::new());
    let peer_store = Arc::new(rustock_networking::peers::PeerStore::new());
    let manager = Arc::new(SyncManager::new(store.clone(), verifier, peer_store.clone()));

    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let mut service = SyncService::new(manager, peer_store.clone(), event_rx);

    // Build headers
    let b1 = dummy_header(1, genesis_hash, U256::from(1));
    let b2 = dummy_header(2, b1.hash(), U256::from(1));
    let b3 = dummy_header(3, b2.hash(), U256::from(1));
    let b4 = dummy_header(4, b3.hash(), U256::from(1));

    // Skeleton: [0, 2, 4]
    let skeleton = vec![
        BlockIdentifier { hash: genesis_hash, number: 0 },
        BlockIdentifier { hash: b2.hash(), number: 2 },
        BlockIdentifier { hash: b4.hash(), number: 4 },
    ];

    let peer = B512::repeat_byte(0x01);
    // Register the peer so fill_pipeline can find it
    let (tx, _rx) = mpsc::channel(rustock_networking::peers::PEER_CHANNEL_CAPACITY);
    peer_store.add_peer(peer, tx).await;

    let mut tracker = PeerChunkTracker::new(skeleton.len());
    // Simulate: chunk 1 assigned to peer, chunk 2 assigned to peer
    let c1 = tracker.next_assignment().unwrap();
    tracker.record_sent(peer, c1);
    let c2 = tracker.next_assignment().unwrap();
    tracker.record_sent(peer, c2);

    service.state = SyncState::DownloadingHeaders {
        peer_best: 4,
        skeleton: skeleton.clone(),
        connection_point: 0,
        tracker,
        pending_next_skeleton: None,
    };

    // Chunk 1: headers for blocks 1-2 (descending from b2)
    service.on_headers_response(peer, vec![b2.clone(), b1.clone()]).await;

    // Should still be in DownloadingHeaders (chunk 2 pending)
    match &service.state {
        SyncState::DownloadingHeaders { tracker, .. } => {
            assert_eq!(tracker.next_to_process, 2);
        }
        _ => panic!("Expected DownloadingHeaders with next_to_process=2, got {:?}", service.state),
    }

    // Chunk 2: headers for blocks 3-4 (descending from b4)
    service.on_headers_response(peer, vec![b4.clone(), b3.clone()]).await;

    // All chunks done → transitions to DownloadingBodies (bodies not yet stored)
    assert!(matches!(service.state, SyncState::DownloadingBodies { .. }),
        "Expected DownloadingBodies after final chunk, got {:?}", service.state);

    // Verify all headers are stored
    assert!(store.header(b1.hash()).unwrap().is_some());
    assert!(store.header(b4.hash()).unwrap().is_some());

    drop(event_tx);
}

#[tokio::test]
async fn test_try_start_sync_when_behind_peer() {
    let dir = tempdir().unwrap();
    let store = Arc::new(BlockStore::open(dir.path()).unwrap());

    let genesis = dummy_header(0, B256::ZERO, U256::from(1));
    store.update_head(&genesis, U256::from(1)).unwrap();

    let verifier = Arc::new(HeaderVerifier::new());
    let peer_store = Arc::new(rustock_networking::peers::PeerStore::new());

    let peer_id = B512::repeat_byte(0x01);
    let (tx, _rx) = mpsc::channel(rustock_networking::peers::PEER_CHANNEL_CAPACITY);
    peer_store.add_peer(peer_id, tx).await;
    peer_store.update_metadata(&peer_id, rustock_networking::peers::PeerMetadata {
        best_number: 1000,
        total_difficulty: U256::from(1000),
        ..Default::default()
    }).await;

    let manager = Arc::new(SyncManager::new(store, verifier, peer_store.clone()));
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let mut service = SyncService::new(manager, peer_store, event_rx);

    service.try_start_sync().await;

    match &service.state {
        SyncState::DownloadingSkeleton { peer_best, connection_point, .. } => {
            assert_eq!(*peer_best, 1000);
            assert_eq!(*connection_point, 0);
        }
        _ => panic!("Expected DownloadingSkeleton, got {:?}", service.state),
    }

    drop(event_tx);
}

#[tokio::test]
async fn test_try_start_sync_already_synced() {
    let dir = tempdir().unwrap();
    let store = Arc::new(BlockStore::open(dir.path()).unwrap());

    let genesis = dummy_header(0, B256::ZERO, U256::from(1));
    store.update_head(&genesis, U256::from(1)).unwrap();

    let verifier = Arc::new(HeaderVerifier::new());
    let peer_store = Arc::new(rustock_networking::peers::PeerStore::new());

    let peer_id = B512::repeat_byte(0x01);
    let (tx, _rx) = mpsc::channel(rustock_networking::peers::PEER_CHANNEL_CAPACITY);
    peer_store.add_peer(peer_id, tx).await;
    peer_store.update_metadata(&peer_id, rustock_networking::peers::PeerMetadata {
        best_number: 0,
        total_difficulty: U256::from(1),
        ..Default::default()
    }).await;

    let manager = Arc::new(SyncManager::new(store, verifier, peer_store.clone()));
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let mut service = SyncService::new(manager, peer_store, event_rx);

    service.try_start_sync().await;

    assert!(matches!(service.state, SyncState::Following),
        "Expected Following when already synced, got {:?}", service.state);

    drop(event_tx);
}

#[tokio::test]
async fn test_timeout_resets_to_idle() {
    let dir = tempdir().unwrap();
    let store = Arc::new(BlockStore::open(dir.path()).unwrap());
    let verifier = Arc::new(HeaderVerifier::new());
    let peer_store = Arc::new(rustock_networking::peers::PeerStore::new());
    let manager = Arc::new(SyncManager::new(store, verifier, peer_store.clone()));

    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let mut service = SyncService::new(manager, peer_store, event_rx);

    let peer = B512::repeat_byte(0x01);
    service.state = SyncState::FindingConnectionPoint {
        peer,
        peer_best: 1000,
        start: 0,
        end: 1000,
    };
    service.last_progress = Instant::now() - Duration::from_secs(60);

    service.on_tick().await;

    assert!(matches!(service.state, SyncState::Idle),
        "Expected Idle after timeout, got {:?}", service.state);

    drop(event_tx);
}

#[tokio::test]
async fn test_descending_headers_reversed() {
    let dir = tempdir().unwrap();
    let store = Arc::new(BlockStore::open(dir.path()).unwrap());

    let genesis = dummy_header(0, B256::ZERO, U256::from(1));
    let genesis_hash = genesis.hash();
    store.update_head(&genesis, U256::from(1)).unwrap();

    let verifier = Arc::new(HeaderVerifier::new());
    let peer_store = Arc::new(rustock_networking::peers::PeerStore::new());
    let manager = SyncManager::new(store.clone(), verifier, peer_store);

    let b1 = dummy_header(1, genesis_hash, U256::from(1));
    let b2 = dummy_header(2, b1.hash(), U256::from(1));
    let b3 = dummy_header(3, b2.hash(), U256::from(1));

    manager.handle_headers_response(vec![b3.clone(), b2.clone(), b1.clone()]).unwrap();

    assert!(store.header(b1.hash()).unwrap().is_some());
    assert!(store.header(b2.hash()).unwrap().is_some());
    assert!(store.header(b3.hash()).unwrap().is_some());
    assert_eq!(store.head().unwrap(), Some(b3.hash()));
}

#[tokio::test]
async fn test_skeleton_round_transitions_to_next_skeleton() {
    let dir = tempdir().unwrap();
    let store = Arc::new(BlockStore::open(dir.path()).unwrap());

    let genesis = dummy_header(0, B256::ZERO, U256::from(1));
    let genesis_hash = genesis.hash();
    store.update_head(&genesis, U256::from(1)).unwrap();

    let b1 = dummy_header(1, genesis_hash, U256::from(1));
    let b2 = dummy_header(2, b1.hash(), U256::from(1));
    let b3 = dummy_header(3, b2.hash(), U256::from(1));
    let b4 = dummy_header(4, b3.hash(), U256::from(1));

    store.put_header(&b1).unwrap();
    store.put_header(&b2).unwrap();
    store.put_total_difficulty(b1.hash(), U256::from(2)).unwrap();
    store.put_total_difficulty(b2.hash(), U256::from(3)).unwrap();
    store.update_head(&b2, U256::from(3)).unwrap();

    let verifier = Arc::new(HeaderVerifier::new());
    let peer_store = Arc::new(rustock_networking::peers::PeerStore::new());
    let manager = Arc::new(SyncManager::new(store.clone(), verifier, peer_store.clone()));

    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let mut service = SyncService::new(manager, peer_store.clone(), event_rx);

    let skeleton = vec![
        BlockIdentifier { hash: genesis_hash, number: 0 },
        BlockIdentifier { hash: b2.hash(), number: 2 },
        BlockIdentifier { hash: b4.hash(), number: 4 },
    ];
    let peer = B512::repeat_byte(0x01);
    // Register the peer so the service can find it for the next skeleton
    let (tx, _rx) = mpsc::channel(rustock_networking::peers::PEER_CHANNEL_CAPACITY);
    peer_store.add_peer(peer, tx).await;
    peer_store.update_metadata(&peer, rustock_networking::peers::PeerMetadata {
        best_number: 10000,
        total_difficulty: U256::from(10000),
        ..Default::default()
    }).await;

    // Set up tracker: chunks 1 already processed, chunk 2 in flight
    let mut tracker = PeerChunkTracker::new(skeleton.len());
    tracker.next_to_assign = 3; // all assigned
    tracker.next_to_process = 2; // chunk 1 already done
    tracker.record_sent(peer, 2); // chunk 2 in flight from peer

    service.state = SyncState::DownloadingHeaders {
        peer_best: 10000,
        skeleton,
        connection_point: 0,
        tracker,
        pending_next_skeleton: None,
    };

    service.on_headers_response(peer, vec![b4.clone(), b3.clone()]).await;

    // Headers complete → transitions to DownloadingBodies since bodies are missing
    match &service.state {
        SyncState::DownloadingBodies { peer_best, .. } => {
            assert_eq!(*peer_best, 10000);
        }
        _ => panic!("Expected DownloadingBodies after completing last chunk, got {:?}", service.state),
    }

    // Simulate body responses to complete the phase
    if let SyncState::DownloadingBodies { id_index, .. } = &service.state {
        let req_ids: Vec<u64> = id_index.keys().copied().collect();
        for req_id in req_ids {
            service.on_body_response(req_id, vec![], vec![]).await;
        }
    }

    // After all bodies downloaded, should continue to DownloadingSkeleton
    match &service.state {
        SyncState::DownloadingSkeleton { connection_point, .. } => {
            assert_eq!(*connection_point, 4, "Should request next skeleton from our head");
        }
        _ => panic!("Expected DownloadingSkeleton after body download, got {:?}", service.state),
    }

    drop(event_tx);
}

#[tokio::test]
async fn test_failed_body_send_stays_tracked_for_retry() {
    let dir = tempdir().unwrap();
    let store = Arc::new(BlockStore::open(dir.path()).unwrap());

    let genesis = dummy_header(0, B256::ZERO, U256::from(1));
    store.update_head(&genesis, U256::from(1)).unwrap();
    let b1 = dummy_header(1, genesis.hash(), U256::from(1));
    let b2 = dummy_header(2, b1.hash(), U256::from(1));
    store.put_header(&b1).unwrap();
    store.put_header(&b2).unwrap();

    let verifier = Arc::new(HeaderVerifier::new());
    let peer_store = Arc::new(rustock_networking::peers::PeerStore::new());
    let manager = Arc::new(SyncManager::new(store.clone(), verifier, peer_store.clone()));
    let (_event_tx, event_rx) = mpsc::unbounded_channel();
    let mut service = SyncService::new(manager, peer_store.clone(), event_rx);

    // Peer whose session channel is closed: every send to it fails.
    let peer = B512::repeat_byte(0x01);
    let (tx, rx) = mpsc::channel(rustock_networking::peers::PEER_CHANNEL_CAPACITY);
    peer_store.add_peer(peer, tx).await;
    drop(rx);

    let mut in_flight = std::collections::HashMap::new();
    in_flight.insert(0usize, InFlightBody { req_id: 7, sent: Instant::now(), peer }); // b1 in flight
    let mut id_index = std::collections::HashMap::new();
    id_index.insert(7u64, 0usize);
    service.state = SyncState::DownloadingBodies {
        peer_best: 10,
        pending_headers: vec![(b1.hash(), b1.clone()), (b2.hash(), b2.clone())],
        next_request: 1,
        in_flight,
        id_index,
    };

    // b1's body arrives; the service tops up with a request for b2, whose
    // send fails (dead peer). The request must stay tracked so the stalled
    // retry re-attempts it — dropping it would orphan b2 forever and the
    // batch could never complete.
    service.on_body_response(7, vec![], vec![]).await;

    match &service.state {
        SyncState::DownloadingBodies { in_flight, .. } => {
            assert_eq!(
                in_flight.len(),
                1,
                "request with failed send must remain in flight for retry"
            );
        }
        other => panic!("Expected DownloadingBodies, got {:?}", other),
    }
}

#[tokio::test]
async fn test_stalled_body_retry_rotates_across_peers() {
    // A lone stalled body must not be re-sent to the same peer every round:
    // if peers[0] is unresponsive, pinning to it wedges the whole batch. Each
    // retry round should target a different peer.
    let dir = tempdir().unwrap();
    let store = Arc::new(BlockStore::open(dir.path()).unwrap());

    let genesis = dummy_header(0, B256::ZERO, U256::from(1));
    store.update_head(&genesis, U256::from(1)).unwrap();
    let b1 = dummy_header(1, genesis.hash(), U256::from(1));
    store.put_header(&b1).unwrap();

    let verifier = Arc::new(HeaderVerifier::new());
    let peer_store = Arc::new(rustock_networking::peers::PeerStore::new());
    let manager = Arc::new(SyncManager::new(store.clone(), verifier, peer_store.clone()));
    let (_event_tx, event_rx) = mpsc::unbounded_channel();
    let mut service = SyncService::new(manager, peer_store.clone(), event_rx);

    // Three live peers; keep their receivers so we can see who got the request.
    let mut rxs = std::collections::HashMap::new();
    for i in 0u8..3 {
        let id = B512::repeat_byte(i + 1);
        let (tx, rx) = mpsc::channel(rustock_networking::peers::PEER_CHANNEL_CAPACITY);
        peer_store.add_peer(id, tx).await;
        rxs.insert(id, rx);
    }

    // One straggler in flight (b1's body), the batch can't finish without it.
    let mut in_flight = std::collections::HashMap::new();
    in_flight.insert(
        0usize,
        InFlightBody { req_id: 7, sent: Instant::now(), peer: B512::repeat_byte(1) },
    );
    let mut id_index = std::collections::HashMap::new();
    id_index.insert(7u64, 0usize);
    service.state = SyncState::DownloadingBodies {
        peer_best: 10,
        pending_headers: vec![(b1.hash(), b1.clone())],
        next_request: 1,
        in_flight,
        id_index,
    };

    // Backdate the lone request so the per-request timeout considers it stale.
    let force_stale = |service: &mut SyncService| {
        if let SyncState::DownloadingBodies { in_flight, .. } = &mut service.state {
            for req in in_flight.values_mut() {
                req.sent = Instant::now()
                    .checked_sub(Duration::from_secs(120))
                    .expect("monotonic clock far enough from origin");
            }
        }
    };

    // Identify which peer received the single retried request this round.
    let receiver_of_round = |rxs: &mut std::collections::HashMap<B512, mpsc::Receiver<_>>| {
        let mut hit = None;
        for (id, rx) in rxs.iter_mut() {
            if rx.try_recv().is_ok() {
                assert!(hit.is_none(), "exactly one peer should get the lone straggler");
                hit = Some(*id);
            }
        }
        hit.expect("some peer must receive the retried request")
    };

    // Each reclaim must move the straggler off the peer it just timed out on,
    // so consecutive rounds always hit a different peer (never pinning).
    force_stale(&mut service);
    service.retry_stale_body_requests().await;
    let first = receiver_of_round(&mut rxs);
    force_stale(&mut service);
    service.retry_stale_body_requests().await;
    let second = receiver_of_round(&mut rxs);
    force_stale(&mut service);
    service.retry_stale_body_requests().await;
    let third = receiver_of_round(&mut rxs);

    assert_ne!(first, second, "retry must move off the timed-out peer");
    assert_ne!(second, third, "retry must keep moving off the timed-out peer");
}

#[tokio::test]
async fn test_body_requests_skip_struck_peers_and_balance() {
    // New body requests must avoid peers over the strike limit (dead/slow) and
    // spread evenly across the responsive ones (least-outstanding assignment).
    let dir = tempdir().unwrap();
    let store = Arc::new(BlockStore::open(dir.path()).unwrap());

    let genesis = dummy_header(0, B256::ZERO, U256::from(1));
    store.update_head(&genesis, U256::from(1)).unwrap();
    let mut headers = Vec::new();
    let mut parent = genesis.hash();
    for n in 1..=6u64 {
        let h = dummy_header(n, parent, U256::from(1));
        store.put_header(&h).unwrap();
        parent = h.hash();
        headers.push((h.hash(), h));
    }

    let verifier = Arc::new(HeaderVerifier::new());
    let peer_store = Arc::new(rustock_networking::peers::PeerStore::new());
    let manager = Arc::new(SyncManager::new(store.clone(), verifier, peer_store.clone()));
    let (_event_tx, event_rx) = mpsc::unbounded_channel();
    let mut service = SyncService::new(manager, peer_store.clone(), event_rx);

    // Two good peers and one "dead" peer marked over the strike limit.
    let good_a = B512::repeat_byte(0x0a);
    let good_b = B512::repeat_byte(0x0b);
    let dead = B512::repeat_byte(0x0d);
    let mut rxs = std::collections::HashMap::new();
    for id in [good_a, good_b, dead] {
        let (tx, rx) = mpsc::channel(rustock_networking::peers::PEER_CHANNEL_CAPACITY);
        peer_store.add_peer(id, tx).await;
        rxs.insert(id, rx);
    }
    service.body_peer_strikes.insert(dead, 100);

    service.state = SyncState::DownloadingBodies {
        peer_best: 10,
        pending_headers: headers,
        next_request: 0,
        in_flight: std::collections::HashMap::new(),
        id_index: std::collections::HashMap::new(),
    };

    service.send_body_requests().await;

    let count = |id: &B512, rxs: &mut std::collections::HashMap<B512, mpsc::Receiver<_>>| {
        let rx = rxs.get_mut(id).unwrap();
        let mut n = 0;
        while rx.try_recv().is_ok() {
            n += 1;
        }
        n
    };
    assert_eq!(count(&dead, &mut rxs), 0, "struck peer must receive no requests");
    let a = count(&good_a, &mut rxs);
    let b = count(&good_b, &mut rxs);
    assert_eq!(a + b, 6, "all six bodies requested from the responsive peers");
    assert_eq!(a, 3, "load balanced evenly across responsive peers");
    assert_eq!(b, 3, "load balanced evenly across responsive peers");
}

#[tokio::test]
async fn test_late_body_response_on_superseded_id_still_applies() {
    // After a request is retried (new id), a late response to the *original*
    // id must still store the body instead of being discarded as "unknown" —
    // and a second (duplicate) response must be ignored without re-fetching.
    let dir = tempdir().unwrap();
    let store = Arc::new(BlockStore::open(dir.path()).unwrap());

    let genesis = dummy_header(0, B256::ZERO, U256::from(1));
    store.update_head(&genesis, U256::from(1)).unwrap();
    let b1 = dummy_header(1, genesis.hash(), U256::from(1));
    let b2 = dummy_header(2, b1.hash(), U256::from(1));
    store.put_header(&b1).unwrap();
    store.put_header(&b2).unwrap();

    let verifier = Arc::new(HeaderVerifier::new());
    let peer_store = Arc::new(rustock_networking::peers::PeerStore::new());
    let manager = Arc::new(SyncManager::new(store.clone(), verifier, peer_store.clone()));
    let (_event_tx, event_rx) = mpsc::unbounded_channel();
    let mut service = SyncService::new(manager, peer_store.clone(), event_rx);

    let peer = B512::repeat_byte(0x01);
    let (tx, _rx) = mpsc::channel(rustock_networking::peers::PEER_CHANNEL_CAPACITY);
    peer_store.add_peer(peer, tx).await;

    // b1 (idx 0) outstanding on id 7 and already stale; b2 (idx 1) on id 8, fresh.
    let stale = Instant::now().checked_sub(Duration::from_secs(120)).unwrap();
    let mut in_flight = std::collections::HashMap::new();
    in_flight.insert(0usize, InFlightBody { req_id: 7, sent: stale, peer });
    in_flight.insert(1usize, InFlightBody { req_id: 8, sent: Instant::now(), peer });
    let mut id_index = std::collections::HashMap::new();
    id_index.insert(7u64, 0usize);
    id_index.insert(8u64, 1usize);
    service.state = SyncState::DownloadingBodies {
        peer_best: 10,
        pending_headers: vec![(b1.hash(), b1.clone()), (b2.hash(), b2.clone())],
        next_request: 2,
        in_flight,
        id_index,
    };

    // Retry: only b1 is stale, so it gets a fresh id; id 7 stays mapped.
    service.retry_stale_body_requests().await;
    let new_id = match &service.state {
        SyncState::DownloadingBodies { in_flight, .. } => in_flight[&0].req_id,
        other => panic!("Expected DownloadingBodies, got {:?}", other),
    };
    assert_ne!(new_id, 7, "retry must issue a fresh request id");

    // Late response on the SUPERSEDED id 7 must still store b1.
    service.on_body_response(7, vec![], vec![]).await;
    assert!(
        store.body(b1.hash()).unwrap().is_some(),
        "late response on a superseded id must still apply its body"
    );
    match &service.state {
        SyncState::DownloadingBodies { in_flight, .. } => {
            assert!(!in_flight.contains_key(&0), "b1 no longer outstanding");
            assert!(in_flight.contains_key(&1), "b2 still outstanding");
        }
        other => panic!("Expected DownloadingBodies, got {:?}", other),
    }

    // Duplicate: the retried id now arrives too — must be ignored, no panic,
    // batch state unchanged (b2 still the only thing outstanding).
    service.on_body_response(new_id, vec![], vec![]).await;
    match &service.state {
        SyncState::DownloadingBodies { in_flight, .. } => {
            assert!(!in_flight.contains_key(&0));
            assert!(in_flight.contains_key(&1), "duplicate must not disturb b2");
        }
        other => panic!("Expected DownloadingBodies, got {:?}", other),
    }
}

// -- PeerChunkTracker tests -----------------------------------------------

#[test]
fn test_tracker_new_starts_at_chunk_1() {
    let tracker = PeerChunkTracker::new(5);
    assert_eq!(tracker.next_to_assign, 1);
    assert_eq!(tracker.next_to_process, 1);
    assert_eq!(tracker.total_chunks, 5);
    assert!(!tracker.is_complete());
}

#[test]
fn test_tracker_assignment_sequence() {
    let mut tracker = PeerChunkTracker::new(4); // chunks 1, 2, 3

    assert_eq!(tracker.next_assignment(), Some(1));
    assert_eq!(tracker.next_assignment(), Some(2));
    assert_eq!(tracker.next_assignment(), Some(3));
    assert_eq!(tracker.next_assignment(), None); // all assigned
    assert_eq!(tracker.next_assignment(), None); // still None
}

#[test]
fn test_tracker_record_and_identify_response() {
    let mut tracker = PeerChunkTracker::new(5);
    let peer_a = B512::repeat_byte(0x0A);
    let peer_b = B512::repeat_byte(0x0B);

    tracker.record_sent(peer_a, 1);
    tracker.record_sent(peer_a, 2);
    tracker.record_sent(peer_b, 3);

    // Responses come back in FIFO order per peer
    assert_eq!(tracker.identify_response(&peer_a), Some(1));
    assert_eq!(tracker.identify_response(&peer_b), Some(3));
    assert_eq!(tracker.identify_response(&peer_a), Some(2));

    // No more in flight
    assert_eq!(tracker.identify_response(&peer_a), None);
    assert_eq!(tracker.identify_response(&peer_b), None);
}

#[test]
fn test_tracker_identify_unknown_peer() {
    let mut tracker = PeerChunkTracker::new(3);
    let unknown = B512::repeat_byte(0xFF);
    assert_eq!(tracker.identify_response(&unknown), None);
}

#[test]
fn test_tracker_drain_ready_in_order() {
    let mut tracker = PeerChunkTracker::new(5);
    // Simulate: chunks 1, 2, 3, 4 all buffered
    tracker.buffer_response(1, vec![]);
    tracker.buffer_response(2, vec![]);
    tracker.buffer_response(3, vec![]);
    tracker.buffer_response(4, vec![]);

    let ready = tracker.drain_ready();
    assert_eq!(ready.len(), 4);
    assert_eq!(ready[0].0, 1);
    assert_eq!(ready[1].0, 2);
    assert_eq!(ready[2].0, 3);
    assert_eq!(ready[3].0, 4);
    assert!(tracker.is_complete());
}

#[test]
fn test_tracker_drain_ready_out_of_order() {
    let mut tracker = PeerChunkTracker::new(5);

    // Chunk 3 arrives first — can't process yet
    tracker.buffer_response(3, vec![]);
    let ready = tracker.drain_ready();
    assert!(ready.is_empty());
    assert_eq!(tracker.next_to_process, 1);

    // Chunk 2 arrives — still can't process (waiting for 1)
    tracker.buffer_response(2, vec![]);
    let ready = tracker.drain_ready();
    assert!(ready.is_empty());

    // Chunk 1 arrives — now process 1, 2, 3 consecutively
    tracker.buffer_response(1, vec![]);
    let ready = tracker.drain_ready();
    assert_eq!(ready.len(), 3);
    assert_eq!(ready[0].0, 1);
    assert_eq!(ready[1].0, 2);
    assert_eq!(ready[2].0, 3);
    assert_eq!(tracker.next_to_process, 4);

    // Chunk 4 arrives — immediately ready
    tracker.buffer_response(4, vec![]);
    let ready = tracker.drain_ready();
    assert_eq!(ready.len(), 1);
    assert_eq!(ready[0].0, 4);
    assert!(tracker.is_complete());
}

#[test]
fn test_tracker_peer_capacity() {
    let mut tracker = PeerChunkTracker::new(20);
    let peer = B512::repeat_byte(0x01);

    // Fresh peer has full capacity
    assert_eq!(tracker.peer_capacity(&peer), 4); // PIPELINE_DEPTH

    // Fill up the pipeline
    tracker.record_sent(peer, 1);
    assert_eq!(tracker.peer_capacity(&peer), 3);
    tracker.record_sent(peer, 2);
    assert_eq!(tracker.peer_capacity(&peer), 2);
    tracker.record_sent(peer, 3);
    assert_eq!(tracker.peer_capacity(&peer), 1);
    tracker.record_sent(peer, 4);
    assert_eq!(tracker.peer_capacity(&peer), 0);

    // Completing a response frees capacity
    tracker.identify_response(&peer);
    assert_eq!(tracker.peer_capacity(&peer), 1);
}

#[test]
fn test_tracker_handle_peer_disconnect() {
    let mut tracker = PeerChunkTracker::new(10);
    let peer_a = B512::repeat_byte(0x0A);
    let peer_b = B512::repeat_byte(0x0B);

    // Assign chunks: peer_a gets 1,2,3 — peer_b gets 4,5,6
    for i in 1..=3 {
        tracker.record_sent(peer_a, i);
    }
    for i in 4..=6 {
        tracker.record_sent(peer_b, i);
    }
    tracker.next_to_assign = 7;

    // Chunk 1 already processed
    tracker.next_to_process = 2;

    // Peer A disconnects — chunks 2, 3 should be reassigned
    tracker.handle_peer_disconnect(&peer_a);

    // next_to_assign should be reset to min(2, 3) = 2
    assert_eq!(tracker.next_to_assign, 2);

    // peer_a should have no in-flight
    assert!(!tracker.in_flight.contains_key(&peer_a));

    // peer_b should be unaffected
    assert_eq!(tracker.in_flight.get(&peer_b).unwrap().len(), 3);
}

#[test]
fn test_tracker_disconnect_with_buffered_chunk() {
    let mut tracker = PeerChunkTracker::new(6);
    let peer = B512::repeat_byte(0x01);

    tracker.record_sent(peer, 1);
    tracker.record_sent(peer, 2);
    tracker.record_sent(peer, 3);
    tracker.next_to_assign = 4;

    // Chunk 2 already buffered (response received but not processed)
    tracker.buffer_response(2, vec![]);

    // Peer disconnects — only chunks 1 and 3 need reassignment (2 is buffered)
    tracker.handle_peer_disconnect(&peer);

    // next_to_assign should be 1 (the minimum un-buffered, un-processed chunk)
    assert_eq!(tracker.next_to_assign, 1);
}

#[test]
fn test_tracker_stalled_peers() {
    let mut tracker = PeerChunkTracker::new(10);
    let peer_a = B512::repeat_byte(0x0A);
    let peer_b = B512::repeat_byte(0x0B);

    tracker.record_sent(peer_a, 1);
    tracker.record_sent(peer_b, 2);
    tracker.record_sent(peer_b, 3);

    // With a zero timeout, both peers count as stalled.
    let mut stalled = tracker.stalled_peers(std::time::Duration::ZERO);
    stalled.sort();
    assert_eq!(stalled, vec![peer_a, peer_b]);

    // A response drains peer_a's queue — it is no longer waiting.
    assert_eq!(tracker.identify_response(&peer_a), Some(1));
    assert_eq!(tracker.stalled_peers(std::time::Duration::ZERO), vec![peer_b]);

    // A response from peer_b refreshes its clock but it still has chunk 3
    // outstanding, so it remains subject to the stall timeout.
    assert_eq!(tracker.identify_response(&peer_b), Some(2));
    assert_eq!(tracker.stalled_peers(std::time::Duration::ZERO), vec![peer_b]);
    assert!(tracker.stalled_peers(std::time::Duration::from_secs(60)).is_empty());
}

#[test]
fn test_tracker_disconnect_clears_stall_tracking() {
    let mut tracker = PeerChunkTracker::new(10);
    let peer = B512::repeat_byte(0x0A);

    tracker.record_sent(peer, 1);
    // Backdate the wait so the stall check is deterministic: stalled_peers uses
    // `elapsed() > timeout`, and elapsed() right after record_sent can be 0ns on
    // a fast machine, making the ZERO-timeout boundary race under load.
    tracker.waiting_since.insert(
        peer,
        std::time::Instant::now()
            .checked_sub(std::time::Duration::from_secs(1))
            .expect("system uptime > 1s"),
    );
    assert_eq!(tracker.stalled_peers(std::time::Duration::ZERO), vec![peer]);

    tracker.handle_peer_disconnect(&peer);
    assert!(tracker.stalled_peers(std::time::Duration::ZERO).is_empty());
}

#[tokio::test]
async fn test_stalled_peer_sidelined_and_chunks_reassigned() {
    let dir = tempdir().unwrap();
    let store = Arc::new(BlockStore::open(dir.path()).unwrap());

    let genesis = dummy_header(0, B256::ZERO, U256::from(1));
    store.update_head(&genesis, U256::from(1)).unwrap();

    let verifier = Arc::new(HeaderVerifier::new());
    let peer_store = Arc::new(rustock_networking::peers::PeerStore::new());
    let manager = Arc::new(SyncManager::new(store, verifier, peer_store.clone()));

    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let mut service = SyncService::new(manager, peer_store.clone(), event_rx);

    let peer_a = B512::repeat_byte(0x0A);
    let peer_b = B512::repeat_byte(0x0B);
    let (tx_a, _rx_a) = mpsc::channel(rustock_networking::peers::PEER_CHANNEL_CAPACITY);
    let (tx_b, mut rx_b) = mpsc::channel(rustock_networking::peers::PEER_CHANNEL_CAPACITY);
    peer_store.add_peer(peer_a, tx_a).await;
    peer_store.add_peer(peer_b, tx_b).await;

    // Round in flight: peer_a holds chunk 1 (stalled 11s), peer_b holds chunk 2.
    let mut tracker = PeerChunkTracker::new(3);
    tracker.record_sent(peer_a, 1);
    tracker.record_sent(peer_b, 2);
    tracker.next_to_assign = 3;
    tracker.waiting_since.insert(
        peer_a,
        std::time::Instant::now()
            .checked_sub(std::time::Duration::from_secs(11))
            .expect("system uptime > 11s"),
    );

    service.state = SyncState::DownloadingHeaders {
        peer_best: 384,
        skeleton: vec![
            BlockIdentifier { hash: B256::repeat_byte(0x01), number: 0 },
            BlockIdentifier { hash: B256::repeat_byte(0x02), number: 192 },
            BlockIdentifier { hash: B256::repeat_byte(0x03), number: 384 },
        ],
        connection_point: 0,
        tracker,
        pending_next_skeleton: None,
    };
    service.last_progress = std::time::Instant::now();

    service.on_tick().await;

    // peer_a is sidelined and its chunk handed to peer_b.
    assert!(service.sidelined.contains_key(&peer_a));
    match &service.state {
        SyncState::DownloadingHeaders { tracker, .. } => {
            assert!(!tracker.in_flight.contains_key(&peer_a));
            assert!(tracker.in_flight.get(&peer_b).unwrap().contains(&1));
        }
        other => panic!("Expected DownloadingHeaders, got {:?}", other),
    }
    assert!(rx_b.try_recv().is_ok(), "reassigned chunk request sent to peer_b");

    drop(event_tx);
}

#[test]
fn test_tracker_is_complete() {
    let mut tracker = PeerChunkTracker::new(3); // chunks 1, 2
    assert!(!tracker.is_complete());

    tracker.next_to_process = 2;
    assert!(!tracker.is_complete());

    tracker.next_to_process = 3;
    assert!(tracker.is_complete());
}

// -- Following mode / NewBlockHashes tests --------------------------------

#[tokio::test]
async fn test_small_gap_enters_following_mode() {
    let dir = tempdir().unwrap();
    let store = Arc::new(BlockStore::open(dir.path()).unwrap());

    let genesis = dummy_header(0, B256::ZERO, U256::from(1));
    store.update_head(&genesis, U256::from(1)).unwrap();

    let verifier = Arc::new(HeaderVerifier::new());
    let peer_store = Arc::new(rustock_networking::peers::PeerStore::new());

    let peer_id = B512::repeat_byte(0x01);
    let (tx, _rx) = mpsc::channel(rustock_networking::peers::PEER_CHANNEL_CAPACITY);
    peer_store.add_peer(peer_id, tx).await;
    peer_store.update_metadata(&peer_id, rustock_networking::peers::PeerMetadata {
        best_number: 10, // only 10 blocks behind (< 24)
        total_difficulty: U256::from(10),
        ..Default::default()
    }).await;

    let manager = Arc::new(SyncManager::new(store, verifier, peer_store.clone()));
    let (_event_tx, event_rx) = mpsc::unbounded_channel();
    let mut service = SyncService::new(manager, peer_store, event_rx);

    service.try_start_sync().await;

    assert!(matches!(service.state, SyncState::Following),
        "Expected Following for small gap, got {:?}", service.state);
}

#[tokio::test]
async fn test_large_gap_enters_skeleton_sync() {
    let dir = tempdir().unwrap();
    let store = Arc::new(BlockStore::open(dir.path()).unwrap());

    let genesis = dummy_header(0, B256::ZERO, U256::from(1));
    store.update_head(&genesis, U256::from(1)).unwrap();

    let verifier = Arc::new(HeaderVerifier::new());
    let peer_store = Arc::new(rustock_networking::peers::PeerStore::new());

    let peer_id = B512::repeat_byte(0x01);
    let (tx, _rx) = mpsc::channel(rustock_networking::peers::PEER_CHANNEL_CAPACITY);
    peer_store.add_peer(peer_id, tx).await;
    peer_store.update_metadata(&peer_id, rustock_networking::peers::PeerMetadata {
        best_number: 100, // 100 blocks behind (> 24)
        total_difficulty: U256::from(100),
        ..Default::default()
    }).await;

    let manager = Arc::new(SyncManager::new(store, verifier, peer_store.clone()));
    let (_event_tx, event_rx) = mpsc::unbounded_channel();
    let mut service = SyncService::new(manager, peer_store, event_rx);

    service.try_start_sync().await;

    assert!(matches!(service.state, SyncState::DownloadingSkeleton { .. }),
        "Expected DownloadingSkeleton for large gap, got {:?}", service.state);
}

#[tokio::test]
async fn test_new_block_hashes_ignored_during_sync() {
    let dir = tempdir().unwrap();
    let store = Arc::new(BlockStore::open(dir.path()).unwrap());
    let verifier = Arc::new(HeaderVerifier::new());
    let peer_store = Arc::new(rustock_networking::peers::PeerStore::new());
    let manager = Arc::new(SyncManager::new(store, verifier, peer_store.clone()));

    let (_event_tx, event_rx) = mpsc::unbounded_channel();
    let mut service = SyncService::new(manager, peer_store, event_rx);

    let peer = B512::repeat_byte(0x01);
    service.state = SyncState::DownloadingSkeleton {
        peer,
        peer_best: 1000,
        connection_point: 0,
    };

    // NewBlockHashes should be silently dropped
    service.on_new_block_hashes(peer, vec![
        BlockIdentifier { hash: B256::repeat_byte(0xaa), number: 1001 },
    ]).await;

    // State should be unchanged
    assert!(matches!(service.state, SyncState::DownloadingSkeleton { .. }),
        "State should remain DownloadingSkeleton, got {:?}", service.state);
}

#[tokio::test]
async fn test_new_block_hashes_processed_in_following_mode() {
    let dir = tempdir().unwrap();
    let store = Arc::new(BlockStore::open(dir.path()).unwrap());

    let genesis = dummy_header(0, B256::ZERO, U256::from(1));
    store.update_head(&genesis, U256::from(1)).unwrap();

    let verifier = Arc::new(HeaderVerifier::new());
    let peer_store = Arc::new(rustock_networking::peers::PeerStore::new());

    let peer = B512::repeat_byte(0x01);
    let (tx, mut rx) = mpsc::channel(rustock_networking::peers::PEER_CHANNEL_CAPACITY);
    peer_store.add_peer(peer, tx).await;

    let manager = Arc::new(SyncManager::new(store, verifier, peer_store.clone()));
    let (_event_tx, event_rx) = mpsc::unbounded_channel();
    let mut service = SyncService::new(manager, peer_store.clone(), event_rx);
    service.state = SyncState::Following;

    // Announce a new block
    service.on_new_block_hashes(peer, vec![
        BlockIdentifier { hash: B256::repeat_byte(0xbb), number: 1 },
    ]).await;

    // Should have sent a headers request to the peer
    let msg = rx.try_recv();
    assert!(msg.is_ok(), "Expected a headers request message to be sent");

    // Peer metadata should be updated
    let best = peer_store.best_peer().await;
    assert!(best.is_some());
    let (_, meta) = best.unwrap();
    assert_eq!(meta.best_number, 1);
}

#[tokio::test]
async fn test_following_switches_to_sync_on_large_gap() {
    let dir = tempdir().unwrap();
    let store = Arc::new(BlockStore::open(dir.path()).unwrap());

    // Build a small chain so our_height > 0 (check_follow_gap returns early at 0)
    let genesis = dummy_header(0, B256::ZERO, U256::from(1));
    let genesis_hash = genesis.hash();
    store.update_head(&genesis, U256::from(1)).unwrap();

    let b1 = dummy_header(1, genesis_hash, U256::from(1));
    let b2 = dummy_header(2, b1.hash(), U256::from(1));
    store.put_header(&b1).unwrap();
    store.put_header(&b2).unwrap();
    store.put_total_difficulty(b1.hash(), U256::from(2)).unwrap();
    store.put_total_difficulty(b2.hash(), U256::from(3)).unwrap();
    store.update_head(&b2, U256::from(3)).unwrap();

    let verifier = Arc::new(HeaderVerifier::new());
    let peer_store = Arc::new(rustock_networking::peers::PeerStore::new());

    let peer_id = B512::repeat_byte(0x01);
    let (tx, _rx) = mpsc::channel(rustock_networking::peers::PEER_CHANNEL_CAPACITY);
    peer_store.add_peer(peer_id, tx).await;
    peer_store.update_metadata(&peer_id, rustock_networking::peers::PeerMetadata {
        best_number: 200, // 198 blocks ahead (> 24)
        total_difficulty: U256::from(200),
        ..Default::default()
    }).await;

    let manager = Arc::new(SyncManager::new(store, verifier, peer_store.clone()));
    let (_event_tx, event_rx) = mpsc::unbounded_channel();
    let mut service = SyncService::new(manager, peer_store, event_rx);
    service.state = SyncState::Following;

    service.check_follow_gap().await;

    assert!(
        matches!(service.state, SyncState::DownloadingSkeleton { .. } | SyncState::FindingConnectionPoint { .. }),
        "Expected DownloadingSkeleton or FindingConnectionPoint after large gap in Following, got {:?}", service.state
    );
}

#[tokio::test]
async fn test_following_stays_when_gap_small() {
    let dir = tempdir().unwrap();
    let store = Arc::new(BlockStore::open(dir.path()).unwrap());

    // Build a small chain so our_height > 0
    let genesis = dummy_header(0, B256::ZERO, U256::from(1));
    let genesis_hash = genesis.hash();
    store.update_head(&genesis, U256::from(1)).unwrap();

    let b1 = dummy_header(1, genesis_hash, U256::from(1));
    store.put_header(&b1).unwrap();
    store.put_total_difficulty(b1.hash(), U256::from(2)).unwrap();
    store.update_head(&b1, U256::from(2)).unwrap();

    let verifier = Arc::new(HeaderVerifier::new());
    let peer_store = Arc::new(rustock_networking::peers::PeerStore::new());

    let peer_id = B512::repeat_byte(0x01);
    let (tx, _rx) = mpsc::channel(rustock_networking::peers::PEER_CHANNEL_CAPACITY);
    peer_store.add_peer(peer_id, tx).await;
    peer_store.update_metadata(&peer_id, rustock_networking::peers::PeerMetadata {
        best_number: 5, // only 4 blocks ahead (< 24)
        total_difficulty: U256::from(5),
        ..Default::default()
    }).await;

    let manager = Arc::new(SyncManager::new(store, verifier, peer_store.clone()));
    let (_event_tx, event_rx) = mpsc::unbounded_channel();
    let mut service = SyncService::new(manager, peer_store, event_rx);
    service.state = SyncState::Following;

    service.check_follow_gap().await;

    assert!(matches!(service.state, SyncState::Following),
        "Expected Following to persist with small gap, got {:?}", service.state);
}

#[tokio::test]
async fn test_following_small_gap_requests_missing_headers() {
    use rustock_networking::protocol::{P2pMessage, RskMessage, RskSubMessage};

    let dir = tempdir().unwrap();
    let store = Arc::new(BlockStore::open(dir.path()).unwrap());

    // Our head is #1; peer is at #5 (gap of 4, < LONG_SYNC_LIMIT).
    let genesis = dummy_header(0, B256::ZERO, U256::from(1));
    let genesis_hash = genesis.hash();
    store.update_head(&genesis, U256::from(1)).unwrap();
    let b1 = dummy_header(1, genesis_hash, U256::from(1));
    store.put_header(&b1).unwrap();
    store.put_total_difficulty(b1.hash(), U256::from(2)).unwrap();
    store.update_head(&b1, U256::from(2)).unwrap();

    let verifier = Arc::new(HeaderVerifier::new());
    let peer_store = Arc::new(rustock_networking::peers::PeerStore::new());

    let peer_id = B512::repeat_byte(0x01);
    let peer_best_hash = B256::repeat_byte(0x05);
    let (tx, mut rx) = mpsc::channel(rustock_networking::peers::PEER_CHANNEL_CAPACITY);
    peer_store.add_peer(peer_id, tx).await;
    peer_store.update_metadata(&peer_id, rustock_networking::peers::PeerMetadata {
        best_number: 5,
        best_hash: peer_best_hash,
        total_difficulty: U256::from(5),
        ..Default::default()
    }).await;

    let manager = Arc::new(SyncManager::new(store, verifier, peer_store.clone()));
    let (_event_tx, event_rx) = mpsc::unbounded_channel();
    let mut service = SyncService::new(manager, peer_store, event_rx);
    service.state = SyncState::Following;

    service.check_follow_gap().await;

    // Stays in Following, but must proactively request the 4 missing headers
    // (ending at the peer's best hash) rather than stalling.
    assert!(matches!(service.state, SyncState::Following),
        "Expected Following to persist, got {:?}", service.state);
    match rx.try_recv().expect("expected a headers request to be sent on a small gap") {
        P2pMessage::RskMessage(RskMessage { sub_message: RskSubMessage::BlockHeadersRequest(req), .. }) => {
            assert_eq!(req.query.hash, peer_best_hash);
            assert_eq!(req.query.count, 4, "should request exactly the gap (5 - 1)");
        }
        other => panic!("Expected BlockHeadersRequest, got {:?}", other),
    }
}

#[tokio::test]
async fn test_handler_forwards_new_block_hashes() {
    use rustock_networking::protocol::{P2pMessage, RskMessage, RskSubMessage};
    use rustock_networking::protocol::P2pHandler;

    let dir = tempdir().unwrap();
    let store = Arc::new(BlockStore::open(dir.path()).unwrap());
    let verifier = Arc::new(HeaderVerifier::new());
    let peer_store = Arc::new(rustock_networking::peers::PeerStore::new());
    let manager = Arc::new(SyncManager::new(store, verifier, peer_store));

    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let handler = SyncHandler::new(manager, event_tx);

    let blocks = vec![
        BlockIdentifier { hash: B256::repeat_byte(0x01), number: 100 },
        BlockIdentifier { hash: B256::repeat_byte(0x02), number: 101 },
    ];
    let msg = P2pMessage::RskMessage(RskMessage::new(
        RskSubMessage::NewBlockHashes(blocks),
    ));

    let resp = handler.handle_message(B512::repeat_byte(0xaa), &msg);
    assert!(resp.is_none());

    match event_rx.try_recv().unwrap() {
        SyncEvent::NewBlockHashes { peer, identifiers } => {
            assert_eq!(peer, B512::repeat_byte(0xaa));
            assert_eq!(identifiers.len(), 2);
            assert_eq!(identifiers[0].number, 100);
            assert_eq!(identifiers[1].number, 101);
        }
        other => panic!("Expected NewBlockHashes event, got {:?}", other),
    }
}

#[tokio::test]
async fn test_headers_response_in_following_mode() {
    let dir = tempdir().unwrap();
    let store = Arc::new(BlockStore::open(dir.path()).unwrap());

    let genesis = dummy_header(0, B256::ZERO, U256::from(1));
    let genesis_hash = genesis.hash();
    store.update_head(&genesis, U256::from(1)).unwrap();

    let verifier = Arc::new(HeaderVerifier::new());
    let peer_store = Arc::new(rustock_networking::peers::PeerStore::new());
    let manager = Arc::new(SyncManager::new(store.clone(), verifier, peer_store.clone()));

    let (_event_tx, event_rx) = mpsc::unbounded_channel();
    let mut service = SyncService::new(manager, peer_store, event_rx);
    service.state = SyncState::Following;

    let b1 = dummy_header(1, genesis_hash, U256::from(1));
    let b1_hash = b1.hash();
    let peer = B512::repeat_byte(0x01);

    // Simulate receiving a headers response while in Following mode
    service.on_headers_response(peer, vec![b1]).await;

    // State should remain Following
    assert!(matches!(service.state, SyncState::Following),
        "Expected Following after headers response, got {:?}", service.state);

    // Header should be stored
    assert!(store.header(b1_hash).unwrap().is_some());
    assert_eq!(store.head().unwrap(), Some(b1_hash));
}

#[tokio::test]
async fn test_follow_mode_buffers_out_of_order_body() {
    // A follow-mode gap-pull fires several body requests at once; responses can
    // land out of order. A block whose parent is NOT our executed head must be
    // buffered, never executed against the wrong parent state.
    let dir = tempdir().unwrap();
    let store = Arc::new(BlockStore::open(dir.path()).unwrap());

    // Executed head is #100; #101 is missing, #102 arrives first.
    let h100 = dummy_header(100, B256::repeat_byte(0xaa), U256::from(100));
    let h100_hash = h100.hash();
    store.put_header(&h100).unwrap();
    store.set_exec_head(h100_hash, B256::ZERO).unwrap();

    let h101 = dummy_header(101, h100_hash, U256::from(101));
    let h102 = dummy_header(102, h101.hash(), U256::from(102));
    let h102_hash = h102.hash();

    let verifier = Arc::new(HeaderVerifier::new());
    let peer_store = Arc::new(rustock_networking::peers::PeerStore::new());
    let manager = Arc::new(SyncManager::new(store.clone(), verifier, peer_store.clone()));
    let (_event_tx, event_rx) = mpsc::unbounded_channel();
    let mut service = SyncService::new(manager, peer_store, event_rx);
    service.state = SyncState::Following;

    // Body for #102 arrives while #101 is still missing.
    let req_id = 42u64;
    service.pending_follow_bodies.insert(req_id, (h102_hash, h102, std::time::Instant::now()));
    service.on_body_response(req_id, vec![], vec![]).await;

    // #102 must remain buffered (parent #101 is not our head), not executed.
    assert!(service.follow_buffer.contains_key(&102),
        "out-of-order block #102 should be buffered, not executed against #100");
    assert_eq!(service.follow_buffer.len(), 1);
    // exec head must not have advanced past #100.
    assert_eq!(store.exec_head().unwrap().map(|(h, _)| h), Some(h100_hash));
    let _ = h101; // #101 intentionally never delivered in this test
}

#[tokio::test]
async fn test_reconcile_recovers_when_the_fork_has_no_canonical_pointer() {
    // The mainnet wedge of 2026-09-18, reduced. The node executed a2(#2) on a
    // branch the network left. It HAS the winning branch's headers -- b2(#2),
    // b3(#3) -- but never adopted them, so canonical_hash(3) is None and the
    // head never rose above #2. Both of reconcile's signals read the canonical
    // index, so both are blind here, and the node re-asks peers for headers
    // after a block they abandoned. It stalled for three days.
    //
    // The height index sees what the canonical pointer cannot: something is
    // stored at #3 and nothing stored at #3 builds on a2.
    let dir = tempdir().unwrap();
    let store = Arc::new(BlockStore::open(dir.path()).unwrap());

    let genesis = dummy_header(0, B256::ZERO, U256::from(1));
    let genesis_hash = genesis.hash();
    store.update_head(&genesis, U256::from(1)).unwrap();
    let b1 = dummy_header(1, genesis_hash, U256::from(1));
    let b1_hash = b1.hash();
    store.update_head(&b1, U256::from(2)).unwrap();

    // Our branch: a2 on b1, canonical and executed.
    let mut a2 = dummy_header(2, b1_hash, U256::from(1));
    a2.extra_data = vec![0xAA].into();
    let a2_hash = a2.hash();
    store.update_head(&a2, U256::from(3)).unwrap();

    // The network's branch: b2(#2) <- b3(#3). Headers only, never adopted --
    // put_header indexes by height without touching the canonical pointers.
    let mut b2 = dummy_header(2, b1_hash, U256::from(5));
    b2.extra_data = vec![0xBB].into();
    let b2_hash = b2.hash();
    store.put_header(&b2).unwrap();
    let b3 = dummy_header(3, b2_hash, U256::from(5));
    let b3_hash = b3.hash();
    store.put_header(&b3).unwrap();

    // The exact conditions that made the node blind.
    assert_eq!(store.canonical_hash(3).unwrap(), None, "no pointer at #3");
    assert_eq!(store.canonical_hash(2).unwrap(), Some(a2_hash), "we hold #2");
    assert!(store.hashes_at_height(3).unwrap().contains(&b3_hash),
        "but the height index knows about b3");

    let trie = Arc::new(rustock_trie::MemoryTrieStore::new()) as Arc<dyn rustock_trie::TrieStore>;
    let empty = rustock_trie::TrieNode::empty();
    trie.put(b1.state_root.as_slice(), &empty.to_message(trie.as_ref()));

    let verifier = Arc::new(HeaderVerifier::new());
    let peer_store = Arc::new(rustock_networking::peers::PeerStore::new());
    let manager = Arc::new(SyncManager::new(store.clone(), verifier, peer_store.clone()));
    let (_event_tx, event_rx) = mpsc::unbounded_channel();
    let processor = rustock_execution::BlockProcessor::new(
        rustock_execution::RskHardforkConfig::mainnet(),
        store.clone(),
    );
    let mut service = SyncService::new(manager, peer_store, event_rx)
        .with_block_processor(processor, trie, empty);
    store.set_exec_head(a2_hash, a2.state_root).unwrap();

    // First round adopts the stored lineage, giving #3 a canonical pointer.
    service.reconcile_exec_head_with_canonical().await;
    assert_eq!(store.canonical_hash(3).unwrap(), Some(b3_hash),
        "the winning branch should now be canonical at #3");
    assert_eq!(store.canonical_hash(2).unwrap(), Some(b2_hash),
        "and at #2, down to the fork point");

    // Second round sees an ordinary orphan and rolls execution back.
    service.reconcile_exec_head_with_canonical().await;
    let (rolled, _) = store.exec_head().unwrap().unwrap();
    assert_eq!(rolled, b1_hash,
        "exec head should roll back to the fork point b1, got {rolled:?}");
}

#[tokio::test]
async fn test_reconcile_when_the_forks_parent_was_never_downloaded() {
    // The mainnet stall of 2026-09-21, which is NOT the shape of the test
    // above. There the winning sibling was in our store. Here the network's
    // block at our own height was never downloaded: we hold only its child.
    //
    //   #1  b1              common
    //   #2  a2 canonical, executed      x2 (the network's) NOT IN STORE
    //   #3  c3 stored, parent = x2      -- so the lineage walk cannot reach
    //                                      the fork point
    //
    // Adopting c3's lineage sets a pointer at #3 whose parent is not the
    // canonical #2, which is an inconsistent chain. What must happen instead
    // is that execution retreats to a block the network can extend.
    let dir = tempdir().unwrap();
    let store = Arc::new(BlockStore::open(dir.path()).unwrap());

    let genesis = dummy_header(0, B256::ZERO, U256::from(1));
    let genesis_hash = genesis.hash();
    store.update_head(&genesis, U256::from(1)).unwrap();
    let b1 = dummy_header(1, genesis_hash, U256::from(1));
    let b1_hash = b1.hash();
    store.update_head(&b1, U256::from(2)).unwrap();

    let mut a2 = dummy_header(2, b1_hash, U256::from(1));
    a2.extra_data = vec![0xAA].into();
    let a2_hash = a2.hash();
    store.update_head(&a2, U256::from(3)).unwrap();

    // x2 is the network's #2. Deliberately NOT stored -- only its hash exists,
    // as the parent field of a child we did download.
    let mut x2 = dummy_header(2, b1_hash, U256::from(9));
    x2.extra_data = vec![0xCC].into();
    let x2_hash = x2.hash();

    let c3 = dummy_header(3, x2_hash, U256::from(9));
    store.put_header(&c3).unwrap();

    assert!(store.header(x2_hash).unwrap().is_none(), "the fork's #2 is absent");
    assert_eq!(store.canonical_hash(3).unwrap(), None);

    let trie = Arc::new(rustock_trie::MemoryTrieStore::new()) as Arc<dyn rustock_trie::TrieStore>;
    let empty = rustock_trie::TrieNode::empty();
    trie.put(b1.state_root.as_slice(), &empty.to_message(trie.as_ref()));
    let verifier = Arc::new(HeaderVerifier::new());
    let peer_store = Arc::new(rustock_networking::peers::PeerStore::new());
    let manager = Arc::new(SyncManager::new(store.clone(), verifier, peer_store.clone()));
    let (_event_tx, event_rx) = mpsc::unbounded_channel();
    let processor = rustock_execution::BlockProcessor::new(
        rustock_execution::RskHardforkConfig::mainnet(),
        store.clone(),
    );
    let mut service = SyncService::new(manager, peer_store, event_rx)
        .with_block_processor(processor, trie, empty);
    store.set_exec_head(a2_hash, a2.state_root).unwrap();

    // Drive several rounds, as the live node would.
    for _ in 0..4 {
        service.reconcile_exec_head_with_canonical().await;
    }

    // At or below b1 -- the network's chain builds on b1 and on everything
    // under it, so any of them lets sync re-request #2 and receive x2.
    //
    // This used to assert exactly b1, i.e. a one-block retreat, and that
    // assertion was the 2026-09-24 stall written down as a requirement. The
    // node cannot know the fork is only one block deep: x2 is missing, which
    // is precisely why `fork_point` cannot answer here. It now retreats
    // `BLIND_RETREAT_DEPTH` blocks instead, which on this three-block fixture
    // reaches genesis.
    let (head, _) = store.exec_head().unwrap().unwrap();
    let exec_number = store.header(head).unwrap().unwrap().number;
    assert!(exec_number <= 1,
        "execution must retreat to at least b1 (#1), the deepest block the \
         network's chain also builds on, so sync can re-request #2 and receive \
         x2; stopped at #{exec_number} ({head:?})");

    // Rolling execution back is only half of it, and asserting only that is why
    // this wedge survived a test written for its own shape. `our_head_number()`
    // -- which chooses where the next skeleton is requested from -- reads
    // `store.head()`, not the exec head. While that still names a2, every round
    // asks peers for headers after a block their chain abandoned, stores the
    // dangling children again, and completes without advancing. Mainnet
    // #9,258,222 looped there for 15 minutes with peers answering in 1s.
    let head_number = store
        .head()
        .unwrap()
        .and_then(|h| store.header(h).unwrap())
        .map(|h| h.number);
    assert_eq!(head_number, Some(exec_number),
        "the head pointer must retreat with execution, or the next skeleton is \
         requested from the orphan and the node re-downloads the same dangling \
         headers");
    assert_ne!(store.canonical_hash(2).unwrap(), Some(a2_hash),
        "the canonical pointer must stop naming the orphan at #2, so the height \
         is downloaded again");
}

/// The mainnet stall of 2026-09-24, at the service level.
///
/// A **three-deep** reorg whose replacement branch was never downloaded:
///
/// ```text
///   #30  f30                      the fork point (canonical, executed)
///   #31  a31 ours       x31 the network's -- NOT IN STORE
///   #32  a32 ours       x32                -- NOT IN STORE
///   #33  a33 ours, executed head  x33      -- NOT IN STORE
///   #34                           c34 stored, parent = x33
/// ```
///
/// Retreating to the orphaned head's parent lands on a32 (#32), which is
/// itself on the abandoned branch. The live node then sat there: a32 is
/// stored and builds on a31, so the orphan check saw a successor extending
/// the head and reported everything fine, while no peer would ever extend it.
/// Twelve minutes, until the network moved 25 blocks ahead and skeleton sync
/// took over.
///
/// The retreat must end up at or below the fork.
#[tokio::test]
async fn test_reconcile_retreats_below_a_three_deep_fork() {
    let dir = tempdir().unwrap();
    let store = Arc::new(BlockStore::open(dir.path()).unwrap());

    // A common chain up to the fork point.
    let genesis = dummy_header(0, B256::ZERO, U256::from(1));
    store.update_head(&genesis, U256::from(1)).unwrap();
    let mut parent = genesis.hash();
    let mut fork_hash = parent;
    for n in 1..=30u64 {
        let h = dummy_header(n, parent, U256::from(1));
        store.update_head(&h, U256::from(n + 1)).unwrap();
        parent = h.hash();
        fork_hash = parent;
    }

    // Our branch: three blocks above the fork, all executed.
    let mut ours = fork_hash;
    let mut our_tip = None;
    for n in 31..=33u64 {
        let mut h = dummy_header(n, ours, U256::from(1));
        h.extra_data = vec![0xAA].into();
        store.update_head(&h, U256::from(n + 1)).unwrap();
        ours = h.hash();
        our_tip = Some(h);
    }
    let our_tip = our_tip.unwrap();
    let our_tip_hash = our_tip.hash();

    // The network's branch: three blocks the node never downloaded, and one
    // child of the last of them that it did.
    let mut theirs = fork_hash;
    for n in 31..=33u64 {
        let mut h = dummy_header(n, theirs, U256::from(9));
        h.extra_data = vec![0xCC].into();
        theirs = h.hash(); // deliberately NOT stored
    }
    let c34 = dummy_header(34, theirs, U256::from(9));
    store.put_header(&c34).unwrap();
    assert!(store.header(theirs).unwrap().is_none(), "their #33 is absent");

    let trie = Arc::new(rustock_trie::MemoryTrieStore::new()) as Arc<dyn rustock_trie::TrieStore>;
    let empty = rustock_trie::TrieNode::empty();
    trie.put(our_tip.state_root.as_slice(), &empty.to_message(trie.as_ref()));
    let verifier = Arc::new(HeaderVerifier::new());
    let peer_store = Arc::new(rustock_networking::peers::PeerStore::new());
    let manager = Arc::new(SyncManager::new(store.clone(), verifier, peer_store.clone()));
    let (_event_tx, event_rx) = mpsc::unbounded_channel();
    let processor = rustock_execution::BlockProcessor::new(
        rustock_execution::RskHardforkConfig::mainnet(),
        store.clone(),
    );
    let mut service = SyncService::new(manager, peer_store, event_rx)
        .with_block_processor(processor, trie, empty);
    store.set_exec_head(our_tip_hash, our_tip.state_root).unwrap();

    service.reconcile_exec_head_with_canonical().await;

    let (exec, _) = store.exec_head().unwrap().unwrap();
    let exec_number = store.header(exec).unwrap().unwrap().number;
    assert!(
        exec_number <= 30,
        "the retreat must end at or below the fork (#30); it stopped at #{exec_number}, \
         which is still on the branch the network abandoned"
    );
    assert!(
        exec_number < 32,
        "#32 is the orphaned head's own parent -- retreating there is the 2026-09-24 bug"
    );

    let head_number = store
        .head()
        .unwrap()
        .and_then(|h| store.header(h).unwrap())
        .map(|h| h.number);
    assert_eq!(head_number, Some(exec_number),
        "the head must retreat with execution, or the next skeleton is requested from \
         a block the network abandoned");
    assert_ne!(store.canonical_hash(33).unwrap(), Some(our_tip_hash),
        "the canonical index must stop naming the orphan");
}

#[tokio::test]
async fn test_reconcile_leaves_a_genuine_tip_alone() {
    // The same code path must not fire at the tip. Nothing is stored above our
    // head, so there is no evidence of a fork and nothing to reconcile --
    // rolling back here would undo good work every time the node caught up.
    let dir = tempdir().unwrap();
    let store = Arc::new(BlockStore::open(dir.path()).unwrap());
    let genesis = dummy_header(0, B256::ZERO, U256::from(1));
    let genesis_hash = genesis.hash();
    store.update_head(&genesis, U256::from(1)).unwrap();
    let b1 = dummy_header(1, genesis_hash, U256::from(1));
    let b1_hash = b1.hash();
    store.update_head(&b1, U256::from(2)).unwrap();

    let trie = Arc::new(rustock_trie::MemoryTrieStore::new()) as Arc<dyn rustock_trie::TrieStore>;
    let empty = rustock_trie::TrieNode::empty();
    trie.put(genesis.state_root.as_slice(), &empty.to_message(trie.as_ref()));
    let verifier = Arc::new(HeaderVerifier::new());
    let peer_store = Arc::new(rustock_networking::peers::PeerStore::new());
    let manager = Arc::new(SyncManager::new(store.clone(), verifier, peer_store.clone()));
    let (_event_tx, event_rx) = mpsc::unbounded_channel();
    let processor = rustock_execution::BlockProcessor::new(
        rustock_execution::RskHardforkConfig::mainnet(),
        store.clone(),
    );
    let mut service = SyncService::new(manager, peer_store, event_rx)
        .with_block_processor(processor, trie, empty);
    store.set_exec_head(b1_hash, b1.state_root).unwrap();

    service.reconcile_exec_head_with_canonical().await;

    let (head, _) = store.exec_head().unwrap().unwrap();
    assert_eq!(head, b1_hash, "a node at the tip must not roll itself back");
}

#[tokio::test]
async fn test_reconcile_rolls_back_orphaned_exec_head() {
    // A 1-block tip reorg. We executed #2 = a2 (now orphaned). The canonical
    // chain is b2(#2) <- b3(#3). The reorg can't be seen via canonical_hash(2)
    // (sync never re-fetches the block at our own head); it's seen via the
    // canonical child #3, whose parent is b2, not a2. Exec head must roll back
    // one block to the common ancestor #1.
    let dir = tempdir().unwrap();
    let store = Arc::new(BlockStore::open(dir.path()).unwrap());

    // Common chain: genesis(#0) <- b1(#1).
    let genesis = dummy_header(0, B256::ZERO, U256::from(1));
    let genesis_hash = genesis.hash();
    store.update_head(&genesis, U256::from(1)).unwrap();
    let b1 = dummy_header(1, genesis_hash, U256::from(1));
    let b1_hash = b1.hash();
    store.update_head(&b1, U256::from(2)).unwrap();

    // Orphan fork: a2 on top of b1 (what we executed).
    let mut a2 = dummy_header(2, b1_hash, U256::from(1));
    a2.extra_data = vec![0xAA].into();
    let a2_hash = a2.hash();
    store.put_header(&a2).unwrap();
    store.put_total_difficulty(a2_hash, U256::from(3)).unwrap();

    // Canonical fork: b2(#2) <- b3(#3), the heavier chain.
    let mut b2 = dummy_header(2, b1_hash, U256::from(5));
    b2.extra_data = vec![0xBB].into();
    let b2_hash = b2.hash();
    store.put_header(&b2).unwrap();
    store.put_total_difficulty(b2_hash, U256::from(7)).unwrap();
    let b3 = dummy_header(3, b2_hash, U256::from(5));
    store.update_head(&b3, U256::from(12)).unwrap(); // canonical chain head

    assert_eq!(store.canonical_hash(3).unwrap(), Some(b3.hash()),
        "b3 should be the canonical child");

    // The rollback reloads the ancestor's state root from the trie store; make
    // b1's state_root resolve to a node there.
    let trie = Arc::new(rustock_trie::MemoryTrieStore::new()) as Arc<dyn rustock_trie::TrieStore>;
    let empty = rustock_trie::TrieNode::empty();
    trie.put(b1.state_root.as_slice(), &empty.to_message(trie.as_ref()));

    let verifier = Arc::new(HeaderVerifier::new());
    let peer_store = Arc::new(rustock_networking::peers::PeerStore::new());
    let manager = Arc::new(SyncManager::new(store.clone(), verifier, peer_store.clone()));
    let (_event_tx, event_rx) = mpsc::unbounded_channel();
    let processor = rustock_execution::BlockProcessor::new(
        rustock_execution::RskHardforkConfig::mainnet(),
        store.clone(),
    );
    let mut service = SyncService::new(manager, peer_store, event_rx)
        .with_block_processor(processor, trie, empty);
    // We (wrongly) executed the orphan a2; point the exec head there.
    store.set_exec_head(a2_hash, a2.state_root).unwrap();

    service.reconcile_exec_head_with_canonical().await;

    // Exec head must have rolled back to the common ancestor #1.
    let (rolled_hash, _) = store.exec_head().unwrap().unwrap();
    assert_eq!(rolled_hash, b1_hash,
        "exec head should roll back to common ancestor b1, got {:?}", rolled_hash);
    assert_eq!(service.last_body_height_for_test(), 1,
        "body cursor should follow the rolled-back head");
}

#[tokio::test]
async fn test_follow_mode_tick_reconciles_orphaned_exec_head() {
    // Same 1-block tip reorg as above, but the node is in follow mode -- the
    // state it is in whenever a reorg orphans the tip it just executed.
    //
    // Follow mode used to have no way to notice: the reconcile that rolls the
    // executed head back was reachable only from `try_start_sync`, which runs
    // in `Idle`, so the node stood still until the backlog check or the
    // execution watchdog forced it out. A tick in `Following` must reconcile
    // and hand the chain back to the body pipeline, which is the only path
    // that can re-fetch the canonical blocks.
    let dir = tempdir().unwrap();
    let store = Arc::new(BlockStore::open(dir.path()).unwrap());

    let genesis = dummy_header(0, B256::ZERO, U256::from(1));
    let genesis_hash = genesis.hash();
    store.update_head(&genesis, U256::from(1)).unwrap();
    let b1 = dummy_header(1, genesis_hash, U256::from(1));
    let b1_hash = b1.hash();
    store.update_head(&b1, U256::from(2)).unwrap();

    // Orphan fork: a2 on top of b1 -- the block we executed.
    let mut a2 = dummy_header(2, b1_hash, U256::from(1));
    a2.extra_data = vec![0xAA].into();
    let a2_hash = a2.hash();
    store.put_header(&a2).unwrap();
    store.put_total_difficulty(a2_hash, U256::from(3)).unwrap();

    // Canonical fork: b2(#2) <- b3(#3), heavier.
    let mut b2 = dummy_header(2, b1_hash, U256::from(5));
    b2.extra_data = vec![0xBB].into();
    let b2_hash = b2.hash();
    store.put_header(&b2).unwrap();
    store.put_total_difficulty(b2_hash, U256::from(7)).unwrap();
    let b3 = dummy_header(3, b2_hash, U256::from(5));
    store.update_head(&b3, U256::from(12)).unwrap();

    let trie = Arc::new(rustock_trie::MemoryTrieStore::new()) as Arc<dyn rustock_trie::TrieStore>;
    let empty = rustock_trie::TrieNode::empty();
    trie.put(b1.state_root.as_slice(), &empty.to_message(trie.as_ref()));

    let verifier = Arc::new(HeaderVerifier::new());
    let peer_store = Arc::new(rustock_networking::peers::PeerStore::new());
    let manager = Arc::new(SyncManager::new(store.clone(), verifier, peer_store.clone()));
    let (_event_tx, event_rx) = mpsc::unbounded_channel();
    let processor = rustock_execution::BlockProcessor::new(
        rustock_execution::RskHardforkConfig::mainnet(),
        store.clone(),
    );
    let mut service = SyncService::new(manager, peer_store, event_rx)
        .with_block_processor(processor, trie, empty);
    store.set_exec_head(a2_hash, a2.state_root).unwrap();
    service.state = SyncState::Following;

    service.on_tick().await;

    let (rolled_hash, _) = store.exec_head().unwrap().unwrap();
    assert_eq!(rolled_hash, b1_hash,
        "a tick in follow mode should roll the orphaned exec head back to b1, got {:?}",
        rolled_hash);
    assert!(matches!(service.state, SyncState::Idle),
        "after rolling back, follow mode must hand over to the body pipeline, state is {:?}",
        std::mem::discriminant(&service.state));
    assert_eq!(service.last_body_height_for_test(), 1,
        "body cursor should follow the rolled-back head");
}

#[tokio::test]
async fn test_follow_mode_tick_keeps_following_when_head_is_canonical() {
    // The reconcile now runs on every follow-mode tick, so it must be inert
    // when the executed head is on the canonical chain: no rollback, and no
    // drop out of follow mode.
    let dir = tempdir().unwrap();
    let store = Arc::new(BlockStore::open(dir.path()).unwrap());

    let genesis = dummy_header(0, B256::ZERO, U256::from(1));
    let genesis_hash = genesis.hash();
    store.update_head(&genesis, U256::from(1)).unwrap();
    let b1 = dummy_header(1, genesis_hash, U256::from(1));
    let b1_hash = b1.hash();
    store.update_head(&b1, U256::from(2)).unwrap();
    let b2 = dummy_header(2, b1_hash, U256::from(1));
    let b2_hash = b2.hash();
    store.update_head(&b2, U256::from(3)).unwrap();

    let trie = Arc::new(rustock_trie::MemoryTrieStore::new()) as Arc<dyn rustock_trie::TrieStore>;
    let empty = rustock_trie::TrieNode::empty();
    trie.put(b1.state_root.as_slice(), &empty.to_message(trie.as_ref()));

    let verifier = Arc::new(HeaderVerifier::new());
    let peer_store = Arc::new(rustock_networking::peers::PeerStore::new());
    let manager = Arc::new(SyncManager::new(store.clone(), verifier, peer_store.clone()));
    let (_event_tx, event_rx) = mpsc::unbounded_channel();
    let processor = rustock_execution::BlockProcessor::new(
        rustock_execution::RskHardforkConfig::mainnet(),
        store.clone(),
    );
    let mut service = SyncService::new(manager, peer_store, event_rx)
        .with_block_processor(processor, trie, empty);
    // Executed head is b1, and the canonical child b2 builds on it.
    store.set_exec_head(b1_hash, b1.state_root).unwrap();
    service.state = SyncState::Following;

    service.on_tick().await;

    assert_eq!(store.exec_head().unwrap().map(|(h, _)| h), Some(b1_hash),
        "a canonical exec head must not be rolled back");
    assert!(matches!(service.state, SyncState::Following),
        "the node should still be following");
    let _ = b2_hash;
}

#[tokio::test]
async fn test_follow_drain_waits_for_background_execution() {
    // A batch executing in the background commits each block as it goes, so
    // `exec_head` advances in the store while `current_state_root` -- the
    // in-memory root follow mode executes against -- still holds the pre-batch
    // state until `poll_execution` reaps the job on a later tick.
    //
    // In that window a follow-mode block whose parent IS the freshly committed
    // head would pass the link check and execute against an older state,
    // computing a wrong state root for a perfectly valid block. Seen on mainnet
    // at #9238768, which re-executed cleanly from the pipeline two minutes
    // later.
    //
    // The drain must stand down while a batch is executing or parked. Staying
    // buffered is the correct behaviour: `follow_buffer` still holds the block.
    let dir = tempdir().unwrap();
    let store = Arc::new(BlockStore::open(dir.path()).unwrap());

    let genesis = dummy_header(0, B256::ZERO, U256::from(1));
    let genesis_hash = genesis.hash();
    store.update_head(&genesis, U256::from(1)).unwrap();
    let b1 = dummy_header(1, genesis_hash, U256::from(1));
    let b1_hash = b1.hash();
    store.update_head(&b1, U256::from(2)).unwrap();
    // b2 builds directly on the executed head, so only the guard can hold it.
    let b2 = dummy_header(2, b1_hash, U256::from(1));
    let b2_hash = b2.hash();
    store.update_head(&b2, U256::from(3)).unwrap();

    let trie = Arc::new(rustock_trie::MemoryTrieStore::new()) as Arc<dyn rustock_trie::TrieStore>;
    let empty = rustock_trie::TrieNode::empty();
    trie.put(b1.state_root.as_slice(), &empty.to_message(trie.as_ref()));

    let verifier = Arc::new(HeaderVerifier::new());
    let peer_store = Arc::new(rustock_networking::peers::PeerStore::new());
    let manager = Arc::new(SyncManager::new(store.clone(), verifier, peer_store.clone()));
    let (_event_tx, event_rx) = mpsc::unbounded_channel();
    let processor = rustock_execution::BlockProcessor::new(
        rustock_execution::RskHardforkConfig::mainnet(),
        store.clone(),
    );
    let mut service = SyncService::new(manager, peer_store, event_rx)
        .with_block_processor(processor, trie, empty);
    store.set_exec_head(b1_hash, b1.state_root).unwrap();
    service.state = SyncState::Following;
    service.follow_buffer.insert(2, (b2_hash, b2, vec![], vec![]));

    // A batch is parked waiting to run: the trie state is not ours to use.
    service.park_empty_batch_for_test();
    service.drain_follow_buffer().await;

    assert!(service.follow_buffer.contains_key(&2),
        "the drain must leave the block buffered while a batch is parked; \
         removing it means execution was attempted against a stale root");
    assert_eq!(store.exec_head().unwrap().map(|(h, _)| h), Some(b1_hash),
        "the executed head must not move while a batch owns the trie state");
}

// -- Peer serving tests (BlockHeadersRequest, BlockHashRequest, SkeletonRequest) --

#[tokio::test]
async fn test_serve_block_hash_request() {
    use rustock_networking::protocol::{P2pMessage, RskMessage, RskSubMessage};
    use rustock_networking::protocol::P2pHandler;
    use rustock_networking::protocol::rsk::BlockHashRequest;

    let dir = tempdir().unwrap();
    let store = Arc::new(BlockStore::open(dir.path()).unwrap());

    let genesis = dummy_header(0, B256::ZERO, U256::from(1));
    let genesis_hash = genesis.hash();
    store.update_head(&genesis, U256::from(1)).unwrap();

    let b1 = dummy_header(1, genesis_hash, U256::from(1));
    let b1_hash = b1.hash();
    store.put_header(&b1).unwrap();
    store.put_canonical_hash(1, b1_hash).unwrap();

    let verifier = Arc::new(HeaderVerifier::new());
    let peer_store = Arc::new(rustock_networking::peers::PeerStore::new());
    let manager = Arc::new(SyncManager::new(store, verifier, peer_store));

    let (event_tx, _event_rx) = mpsc::unbounded_channel();
    let handler = SyncHandler::new(manager, event_tx);

    let req = BlockHashRequest { id: 42, height: 1 };
    let msg = P2pMessage::RskMessage(RskMessage::new(RskSubMessage::BlockHashRequest(req)));

    let resp = handler.handle_message(B512::repeat_byte(0x01), &msg);
    assert!(resp.is_some(), "Should respond to BlockHashRequest");

    if let Some(P2pMessage::RskMessage(rsk_msg)) = resp {
        if let RskSubMessage::BlockHashResponse(r) = rsk_msg.sub_message {
            assert_eq!(r.id, 42);
            assert_eq!(r.hash, b1_hash);
        } else {
            panic!("Expected BlockHashResponse");
        }
    } else {
        panic!("Expected RskMessage response");
    }
}

#[tokio::test]
async fn test_serve_block_hash_request_unknown_height() {
    use rustock_networking::protocol::{P2pMessage, RskMessage, RskSubMessage};
    use rustock_networking::protocol::P2pHandler;
    use rustock_networking::protocol::rsk::BlockHashRequest;

    let dir = tempdir().unwrap();
    let store = Arc::new(BlockStore::open(dir.path()).unwrap());
    let verifier = Arc::new(HeaderVerifier::new());
    let peer_store = Arc::new(rustock_networking::peers::PeerStore::new());
    let manager = Arc::new(SyncManager::new(store, verifier, peer_store));

    let (event_tx, _event_rx) = mpsc::unbounded_channel();
    let handler = SyncHandler::new(manager, event_tx);

    let req = BlockHashRequest { id: 99, height: 9999 };
    let msg = P2pMessage::RskMessage(RskMessage::new(RskSubMessage::BlockHashRequest(req)));

    let resp = handler.handle_message(B512::ZERO, &msg);
    assert!(resp.is_none(), "Should not respond for unknown height");
}

#[tokio::test]
async fn test_serve_block_hash_request_height_zero() {
    use rustock_networking::protocol::{P2pMessage, RskMessage, RskSubMessage};
    use rustock_networking::protocol::P2pHandler;
    use rustock_networking::protocol::rsk::BlockHashRequest;

    let dir = tempdir().unwrap();
    let store = Arc::new(BlockStore::open(dir.path()).unwrap());
    let genesis = dummy_header(0, B256::ZERO, U256::from(1));
    store.update_head(&genesis, U256::from(1)).unwrap();

    let verifier = Arc::new(HeaderVerifier::new());
    let peer_store = Arc::new(rustock_networking::peers::PeerStore::new());
    let manager = Arc::new(SyncManager::new(store, verifier, peer_store));

    let (event_tx, _event_rx) = mpsc::unbounded_channel();
    let handler = SyncHandler::new(manager, event_tx);

    let req = BlockHashRequest { id: 1, height: 0 };
    let msg = P2pMessage::RskMessage(RskMessage::new(RskSubMessage::BlockHashRequest(req)));

    let resp = handler.handle_message(B512::ZERO, &msg);
    assert!(resp.is_none(), "Should not respond for height 0 (matches rskj)");
}

#[tokio::test]
async fn test_serve_headers_request_single() {
    use rustock_networking::protocol::{P2pMessage, RskMessage, RskSubMessage};
    use rustock_networking::protocol::P2pHandler;
    use rustock_networking::protocol::rsk::{BlockHeadersRequest, BlockHeadersQuery};

    let dir = tempdir().unwrap();
    let store = Arc::new(BlockStore::open(dir.path()).unwrap());

    let genesis = dummy_header(0, B256::ZERO, U256::from(1));
    let genesis_hash = genesis.hash();
    store.update_head(&genesis, U256::from(1)).unwrap();

    let b1 = dummy_header(1, genesis_hash, U256::from(1));
    let b1_hash = b1.hash();
    store.put_header(&b1).unwrap();
    store.put_canonical_hash(1, b1_hash).unwrap();

    let verifier = Arc::new(HeaderVerifier::new());
    let peer_store = Arc::new(rustock_networking::peers::PeerStore::new());
    let manager = Arc::new(SyncManager::new(store, verifier, peer_store));

    let (event_tx, _event_rx) = mpsc::unbounded_channel();
    let handler = SyncHandler::new(manager, event_tx);

    let req = BlockHeadersRequest {
        id: 7,
        query: BlockHeadersQuery { hash: b1_hash, count: 1 },
    };
    let msg = P2pMessage::RskMessage(RskMessage::new(RskSubMessage::BlockHeadersRequest(req)));

    let resp = handler.handle_message(B512::repeat_byte(0x01), &msg);
    assert!(resp.is_some());

    if let Some(P2pMessage::RskMessage(rsk_msg)) = resp {
        if let RskSubMessage::BlockHeadersResponse(r) = rsk_msg.sub_message {
            assert_eq!(r.id, 7);
            assert_eq!(r.headers.len(), 1);
            assert_eq!(r.headers[0].number, 1);
        } else {
            panic!("Expected BlockHeadersResponse");
        }
    }
}

#[tokio::test]
async fn test_serve_headers_request_chain_walk() {
    use rustock_networking::protocol::{P2pMessage, RskMessage, RskSubMessage};
    use rustock_networking::protocol::P2pHandler;
    use rustock_networking::protocol::rsk::{BlockHeadersRequest, BlockHeadersQuery};

    let dir = tempdir().unwrap();
    let store = Arc::new(BlockStore::open(dir.path()).unwrap());

    let genesis = dummy_header(0, B256::ZERO, U256::from(1));
    let genesis_hash = genesis.hash();
    store.update_head(&genesis, U256::from(1)).unwrap();

    let b1 = dummy_header(1, genesis_hash, U256::from(1));
    let b2 = dummy_header(2, b1.hash(), U256::from(1));
    let b3 = dummy_header(3, b2.hash(), U256::from(1));
    store.put_header(&b1).unwrap();
    store.put_header(&b2).unwrap();
    store.put_header(&b3).unwrap();

    let verifier = Arc::new(HeaderVerifier::new());
    let peer_store = Arc::new(rustock_networking::peers::PeerStore::new());
    let manager = Arc::new(SyncManager::new(store, verifier, peer_store));

    let (event_tx, _event_rx) = mpsc::unbounded_channel();
    let handler = SyncHandler::new(manager, event_tx);

    // Request 3 headers starting from b3 (walking backwards)
    let req = BlockHeadersRequest {
        id: 10,
        query: BlockHeadersQuery { hash: b3.hash(), count: 3 },
    };
    let msg = P2pMessage::RskMessage(RskMessage::new(RskSubMessage::BlockHeadersRequest(req)));

    let resp = handler.handle_message(B512::ZERO, &msg);
    assert!(resp.is_some());

    if let Some(P2pMessage::RskMessage(rsk_msg)) = resp {
        if let RskSubMessage::BlockHeadersResponse(r) = rsk_msg.sub_message {
            assert_eq!(r.id, 10);
            assert_eq!(r.headers.len(), 3);
            // Headers are returned in descending order (b3, b2, b1)
            assert_eq!(r.headers[0].number, 3);
            assert_eq!(r.headers[1].number, 2);
            assert_eq!(r.headers[2].number, 1);
        } else {
            panic!("Expected BlockHeadersResponse");
        }
    }
}

#[tokio::test]
async fn test_serve_headers_request_unknown_hash() {
    use rustock_networking::protocol::{P2pMessage, RskMessage, RskSubMessage};
    use rustock_networking::protocol::P2pHandler;
    use rustock_networking::protocol::rsk::{BlockHeadersRequest, BlockHeadersQuery};

    let dir = tempdir().unwrap();
    let store = Arc::new(BlockStore::open(dir.path()).unwrap());
    let verifier = Arc::new(HeaderVerifier::new());
    let peer_store = Arc::new(rustock_networking::peers::PeerStore::new());
    let manager = Arc::new(SyncManager::new(store, verifier, peer_store));

    let (event_tx, _event_rx) = mpsc::unbounded_channel();
    let handler = SyncHandler::new(manager, event_tx);

    let req = BlockHeadersRequest {
        id: 1,
        query: BlockHeadersQuery {
            hash: B256::repeat_byte(0xFF),
            count: 10,
        },
    };
    let msg = P2pMessage::RskMessage(RskMessage::new(RskSubMessage::BlockHeadersRequest(req)));

    let resp = handler.handle_message(B512::ZERO, &msg);
    assert!(resp.is_none(), "Should not respond for unknown hash");
}

#[tokio::test]
async fn test_serve_headers_request_capped_count() {
    use rustock_networking::protocol::{P2pMessage, RskMessage, RskSubMessage};
    use rustock_networking::protocol::P2pHandler;
    use rustock_networking::protocol::rsk::{BlockHeadersRequest, BlockHeadersQuery};

    let dir = tempdir().unwrap();
    let store = Arc::new(BlockStore::open(dir.path()).unwrap());

    let genesis = dummy_header(0, B256::ZERO, U256::from(1));
    let genesis_hash = genesis.hash();
    store.update_head(&genesis, U256::from(1)).unwrap();

    let b1 = dummy_header(1, genesis_hash, U256::from(1));
    store.put_header(&b1).unwrap();

    let verifier = Arc::new(HeaderVerifier::new());
    let peer_store = Arc::new(rustock_networking::peers::PeerStore::new());
    let manager = Arc::new(SyncManager::new(store, verifier, peer_store));

    let (event_tx, _event_rx) = mpsc::unbounded_channel();
    let handler = SyncHandler::new(manager, event_tx);

    // Request 1000 headers but we only have 2 (b1 + genesis)
    let req = BlockHeadersRequest {
        id: 1,
        query: BlockHeadersQuery { hash: b1.hash(), count: 1000 },
    };
    let msg = P2pMessage::RskMessage(RskMessage::new(RskSubMessage::BlockHeadersRequest(req)));

    let resp = handler.handle_message(B512::ZERO, &msg);
    assert!(resp.is_some());

    if let Some(P2pMessage::RskMessage(rsk_msg)) = resp {
        if let RskSubMessage::BlockHeadersResponse(r) = rsk_msg.sub_message {
            // Should only return what's available (capped at MAX_HEADERS_SERVE=192
            // and chain length = 2)
            assert_eq!(r.headers.len(), 2);
        } else {
            panic!("Expected BlockHeadersResponse");
        }
    }
}

#[tokio::test]
async fn test_serve_skeleton_request() {
    use rustock_networking::protocol::{P2pMessage, RskMessage, RskSubMessage};
    use rustock_networking::protocol::P2pHandler;
    use rustock_networking::protocol::rsk::SkeletonRequest;

    let dir = tempdir().unwrap();
    let store = Arc::new(BlockStore::open(dir.path()).unwrap());

    // Build a chain of 500 blocks
    let mut prev_hash = B256::ZERO;
    let mut td = U256::ZERO;
    for i in 0..=500u64 {
        let h = dummy_header(i, prev_hash, U256::from(1));
        let hash = h.hash();
        td += h.difficulty;
        store.put_header(&h).unwrap();
        store.put_canonical_hash(i, hash).unwrap();
        store.put_total_difficulty(hash, td).unwrap();
        if i == 500 {
            store.set_head(hash).unwrap();
        }
        prev_hash = hash;
    }

    let verifier = Arc::new(HeaderVerifier::new());
    let peer_store = Arc::new(rustock_networking::peers::PeerStore::new());
    let manager = Arc::new(SyncManager::new(store, verifier, peer_store));

    let (event_tx, _event_rx) = mpsc::unbounded_channel();
    let handler = SyncHandler::new(manager, event_tx);

    let req = SkeletonRequest { id: 55, start_number: 0 };
    let msg = P2pMessage::RskMessage(RskMessage::new(RskSubMessage::SkeletonRequest(req)));

    let resp = handler.handle_message(B512::ZERO, &msg);
    assert!(resp.is_some(), "Should respond to SkeletonRequest");

    if let Some(P2pMessage::RskMessage(rsk_msg)) = resp {
        if let RskSubMessage::SkeletonResponse(r) = rsk_msg.sub_message {
            assert_eq!(r.id, 55);
            // Should have entries at 0, 192, 384, and best=500
            assert!(r.block_identifiers.len() >= 3);
            assert_eq!(r.block_identifiers[0].number, 0);
            assert_eq!(r.block_identifiers[1].number, 192);
            assert_eq!(r.block_identifiers[2].number, 384);
            // Last entry should be the best block (500)
            let last = r.block_identifiers.last().unwrap();
            assert_eq!(last.number, 500);
        } else {
            panic!("Expected SkeletonResponse");
        }
    }
}

#[tokio::test]
async fn test_serve_skeleton_request_unknown_start() {
    use rustock_networking::protocol::{P2pMessage, RskMessage, RskSubMessage};
    use rustock_networking::protocol::P2pHandler;
    use rustock_networking::protocol::rsk::SkeletonRequest;

    let dir = tempdir().unwrap();
    let store = Arc::new(BlockStore::open(dir.path()).unwrap());
    let verifier = Arc::new(HeaderVerifier::new());
    let peer_store = Arc::new(rustock_networking::peers::PeerStore::new());
    let manager = Arc::new(SyncManager::new(store, verifier, peer_store));

    let (event_tx, _event_rx) = mpsc::unbounded_channel();
    let handler = SyncHandler::new(manager, event_tx);

    let req = SkeletonRequest { id: 1, start_number: 99999 };
    let msg = P2pMessage::RskMessage(RskMessage::new(RskSubMessage::SkeletonRequest(req)));

    let resp = handler.handle_message(B512::ZERO, &msg);
    assert!(resp.is_none(), "Should not respond for unknown start number");
}

#[tokio::test]
async fn test_serve_skeleton_request_at_boundary() {
    use rustock_networking::protocol::{P2pMessage, RskMessage, RskSubMessage};
    use rustock_networking::protocol::P2pHandler;
    use rustock_networking::protocol::rsk::SkeletonRequest;

    let dir = tempdir().unwrap();
    let store = Arc::new(BlockStore::open(dir.path()).unwrap());

    // Build a chain of exactly 192 blocks (one skeleton step)
    let mut prev_hash = B256::ZERO;
    let mut td = U256::ZERO;
    for i in 0..=192u64 {
        let h = dummy_header(i, prev_hash, U256::from(1));
        let hash = h.hash();
        td += h.difficulty;
        store.put_header(&h).unwrap();
        store.put_canonical_hash(i, hash).unwrap();
        store.put_total_difficulty(hash, td).unwrap();
        if i == 192 {
            store.set_head(hash).unwrap();
        }
        prev_hash = hash;
    }

    let verifier = Arc::new(HeaderVerifier::new());
    let peer_store = Arc::new(rustock_networking::peers::PeerStore::new());
    let manager = Arc::new(SyncManager::new(store, verifier, peer_store));

    let (event_tx, _event_rx) = mpsc::unbounded_channel();
    let handler = SyncHandler::new(manager, event_tx);

    let req = SkeletonRequest { id: 1, start_number: 0 };
    let msg = P2pMessage::RskMessage(RskMessage::new(RskSubMessage::SkeletonRequest(req)));

    let resp = handler.handle_message(B512::ZERO, &msg);
    assert!(resp.is_some());

    if let Some(P2pMessage::RskMessage(rsk_msg)) = resp {
        if let RskSubMessage::SkeletonResponse(r) = rsk_msg.sub_message {
            // Should have entries at 0 and 192
            assert_eq!(r.block_identifiers.len(), 2);
            assert_eq!(r.block_identifiers[0].number, 0);
            assert_eq!(r.block_identifiers[1].number, 192);
        } else {
            panic!("Expected SkeletonResponse");
        }
    }
}

#[tokio::test]
async fn test_serve_requests_dont_interfere_with_event_forwarding() {
    use rustock_networking::protocol::{P2pMessage, RskMessage, RskSubMessage};
    use rustock_networking::protocol::P2pHandler;
    use rustock_networking::protocol::rsk::{BlockHashRequest, BlockHeadersResponse as BHResp};

    let dir = tempdir().unwrap();
    let store = Arc::new(BlockStore::open(dir.path()).unwrap());

    let genesis = dummy_header(0, B256::ZERO, U256::from(1));
    store.update_head(&genesis, U256::from(1)).unwrap();

    let verifier = Arc::new(HeaderVerifier::new());
    let peer_store = Arc::new(rustock_networking::peers::PeerStore::new());
    let manager = Arc::new(SyncManager::new(store, verifier, peer_store));

    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let handler = SyncHandler::new(manager, event_tx);
    let peer = B512::repeat_byte(0x01);

    // 1. Serve a block hash request (returns response, no event)
    let req = BlockHashRequest { id: 1, height: 0 };
    let msg = P2pMessage::RskMessage(RskMessage::new(RskSubMessage::BlockHashRequest(req)));
    let _resp = handler.handle_message(peer, &msg);
    assert!(event_rx.try_recv().is_err(), "Serving should not forward events");

    // 2. Forward a headers response (no response, forwards event)
    let headers_resp = BHResp { id: 2, headers: vec![genesis.clone()] };
    let msg = P2pMessage::RskMessage(RskMessage::new(RskSubMessage::BlockHeadersResponse(headers_resp)));
    let resp = handler.handle_message(peer, &msg);
    assert!(resp.is_none(), "Forwarding should not return a response");
    assert!(event_rx.try_recv().is_ok(), "Should forward the event");
}

// -- Reorg tests ----------------------------------------------------------

/// Like dummy_header but with extra_data to differentiate fork branches.
fn fork_header(number: u64, parent: B256, difficulty: U256, branch: u8) -> Header {
    let mut h = dummy_header(number, parent, difficulty);
    h.extra_data = Bytes::from(vec![branch]);
    h
}

#[tokio::test]
async fn test_new_block_hashes_reorg_candidate_triggers_request() {
    let dir = tempdir().unwrap();
    let store = Arc::new(BlockStore::open(dir.path()).unwrap());

    let genesis = dummy_header(0, B256::ZERO, U256::from(1));
    let genesis_hash = genesis.hash();
    store.update_head(&genesis, U256::from(1)).unwrap();

    let b1 = dummy_header(1, genesis_hash, U256::from(1));
    let b1_hash = b1.hash();
    store.put_header(&b1).unwrap();
    store.put_canonical_hash(1, b1_hash).unwrap();
    store.put_total_difficulty(b1_hash, U256::from(2)).unwrap();
    store.update_head(&b1, U256::from(2)).unwrap();

    let verifier = Arc::new(HeaderVerifier::new());
    let peer_store = Arc::new(rustock_networking::peers::PeerStore::new());

    let peer = B512::repeat_byte(0x01);
    let (tx, mut rx) = mpsc::channel(rustock_networking::peers::PEER_CHANNEL_CAPACITY);
    peer_store.add_peer(peer, tx).await;

    let manager = Arc::new(SyncManager::new(store, verifier, peer_store.clone()));
    let (_event_tx, event_rx) = mpsc::unbounded_channel();
    let mut service = SyncService::new(manager, peer_store.clone(), event_rx);
    service.state = SyncState::Following;

    // Announce a competing block at height 1 with a different hash
    let competing_hash = B256::repeat_byte(0xCC);
    service.on_new_block_hashes(peer, vec![
        BlockIdentifier { hash: competing_hash, number: 1 },
    ]).await;

    // Should have sent a headers request for the competing chain
    let msg = rx.try_recv();
    assert!(msg.is_ok(), "Should request headers for reorg candidate");
}

#[tokio::test]
async fn test_new_block_hashes_same_hash_ignored() {
    let dir = tempdir().unwrap();
    let store = Arc::new(BlockStore::open(dir.path()).unwrap());

    let genesis = dummy_header(0, B256::ZERO, U256::from(1));
    let genesis_hash = genesis.hash();
    store.update_head(&genesis, U256::from(1)).unwrap();

    let b1 = dummy_header(1, genesis_hash, U256::from(1));
    let b1_hash = b1.hash();
    store.put_header(&b1).unwrap();
    store.put_canonical_hash(1, b1_hash).unwrap();
    store.put_total_difficulty(b1_hash, U256::from(2)).unwrap();
    store.update_head(&b1, U256::from(2)).unwrap();

    let verifier = Arc::new(HeaderVerifier::new());
    let peer_store = Arc::new(rustock_networking::peers::PeerStore::new());

    let peer = B512::repeat_byte(0x01);
    let (tx, mut rx) = mpsc::channel(rustock_networking::peers::PEER_CHANNEL_CAPACITY);
    peer_store.add_peer(peer, tx).await;

    let manager = Arc::new(SyncManager::new(store, verifier, peer_store.clone()));
    let (_event_tx, event_rx) = mpsc::unbounded_channel();
    let mut service = SyncService::new(manager, peer_store.clone(), event_rx);
    service.state = SyncState::Following;

    // Announce the SAME hash at height 1 — should be ignored
    service.on_new_block_hashes(peer, vec![
        BlockIdentifier { hash: b1_hash, number: 1 },
    ]).await;

    assert!(rx.try_recv().is_err(), "Should not request headers for same hash");
}

#[tokio::test]
async fn test_reorg_via_headers_response_higher_td() {
    let dir = tempdir().unwrap();
    let store = Arc::new(BlockStore::open(dir.path()).unwrap());

    // Build canonical chain: genesis -> 1A -> 2A
    let genesis = dummy_header(0, B256::ZERO, U256::from(10));
    let genesis_hash = genesis.hash();
    store.update_head(&genesis, U256::from(10)).unwrap();

    let a1 = dummy_header(1, genesis_hash, U256::from(10));
    let a2 = dummy_header(2, a1.hash(), U256::from(10));
    store.put_header(&a1).unwrap();
    store.put_canonical_hash(1, a1.hash()).unwrap();
    store.put_total_difficulty(a1.hash(), U256::from(20)).unwrap();
    store.put_header(&a2).unwrap();
    store.put_canonical_hash(2, a2.hash()).unwrap();
    store.put_total_difficulty(a2.hash(), U256::from(30)).unwrap();
    store.update_head(&a2, U256::from(30)).unwrap();

    let verifier = Arc::new(HeaderVerifier::new());
    let peer_store = Arc::new(rustock_networking::peers::PeerStore::new());
    let manager = Arc::new(SyncManager::new(store.clone(), verifier, peer_store.clone()));

    let (_event_tx, event_rx) = mpsc::unbounded_channel();
    let mut service = SyncService::new(manager, peer_store, event_rx);
    service.state = SyncState::Following;

    // Fork from genesis: 1B -> 2B with higher difficulty
    let b1 = fork_header(1, genesis_hash, U256::from(20), 0xBB);
    let b2 = fork_header(2, b1.hash(), U256::from(20), 0xBB);

    // Receive fork headers (descending order, like real peers send)
    let peer = B512::repeat_byte(0x01);
    service.on_headers_response(peer, vec![b2.clone(), b1.clone()]).await;

    // Fork B has TD = 10 + 20 + 20 = 50 vs chain A's TD = 30
    // Head should switch to fork B
    assert_eq!(store.head().unwrap(), Some(b2.hash()));
    assert_eq!(store.canonical_hash(1).unwrap(), Some(b1.hash()));
    assert_eq!(store.canonical_hash(2).unwrap(), Some(b2.hash()));
}

#[tokio::test]
async fn test_no_reorg_via_headers_response_lower_td() {
    let dir = tempdir().unwrap();
    let store = Arc::new(BlockStore::open(dir.path()).unwrap());

    // Build canonical chain: genesis -> 1A -> 2A (high difficulty)
    let genesis = dummy_header(0, B256::ZERO, U256::from(10));
    let genesis_hash = genesis.hash();
    store.update_head(&genesis, U256::from(10)).unwrap();

    let a1 = dummy_header(1, genesis_hash, U256::from(20));
    let a2 = dummy_header(2, a1.hash(), U256::from(20));
    store.put_header(&a1).unwrap();
    store.put_canonical_hash(1, a1.hash()).unwrap();
    store.put_total_difficulty(a1.hash(), U256::from(30)).unwrap();
    store.put_header(&a2).unwrap();
    store.put_canonical_hash(2, a2.hash()).unwrap();
    store.put_total_difficulty(a2.hash(), U256::from(50)).unwrap();
    store.update_head(&a2, U256::from(50)).unwrap();

    let verifier = Arc::new(HeaderVerifier::new());
    let peer_store = Arc::new(rustock_networking::peers::PeerStore::new());
    let manager = Arc::new(SyncManager::new(store.clone(), verifier, peer_store.clone()));

    let (_event_tx, event_rx) = mpsc::unbounded_channel();
    let mut service = SyncService::new(manager, peer_store, event_rx);
    service.state = SyncState::Following;

    // Fork from genesis: 1B -> 2B with lower difficulty
    let b1 = fork_header(1, genesis_hash, U256::from(5), 0xBB);
    let b2 = fork_header(2, b1.hash(), U256::from(5), 0xBB);

    let peer = B512::repeat_byte(0x01);
    service.on_headers_response(peer, vec![b2.clone(), b1.clone()]).await;

    // Fork B has TD = 10 + 5 + 5 = 20 vs chain A's TD = 50
    // Head should NOT change
    assert_eq!(store.head().unwrap(), Some(a2.hash()));
    assert_eq!(store.canonical_hash(1).unwrap(), Some(a1.hash()));
    assert_eq!(store.canonical_hash(2).unwrap(), Some(a2.hash()));

    // Fork B headers should still be stored
    assert!(store.header(b1.hash()).unwrap().is_some());
    assert!(store.header(b2.hash()).unwrap().is_some());
}

// ========== Body download tests ==========

#[tokio::test]
async fn test_sync_handler_serves_body_request() {
    use rustock_networking::protocol::{P2pMessage, RskMessage, RskSubMessage};
    use rustock_networking::protocol::P2pHandler;
    use rustock_networking::protocol::rsk::BodyRequest;
    use rustock_core::Transaction;

    let dir = tempdir().unwrap();
    let store = Arc::new(BlockStore::open(dir.path()).unwrap());

    let genesis = dummy_header(0, B256::ZERO, U256::from(1));
    let genesis_hash = genesis.hash();
    store.update_head(&genesis, U256::from(1)).unwrap();

    // Store a body for genesis
    let tx = Transaction {
        nonce: 42,
        gas_price: U256::from(10),
        gas_limit: U256::from(21000),
        to: Bytes::from(vec![0x12; 20]),
        value: U256::from(100),
        input: Bytes::default(),
        v: 27,
        r: U256::from(88),
        s: U256::from(99),
        cached_rlp: None,
    };
    store.put_body(genesis_hash, &[tx], &[]).unwrap();

    let verifier = Arc::new(HeaderVerifier::new());
    let peer_store = Arc::new(rustock_networking::peers::PeerStore::new());
    let manager = Arc::new(SyncManager::new(store, verifier, peer_store));

    let (event_tx, _event_rx) = mpsc::unbounded_channel();
    let handler = SyncHandler::new(manager, event_tx);

    let req = BodyRequest { id: 33, hash: genesis_hash };
    let msg = P2pMessage::RskMessage(RskMessage::new(RskSubMessage::BodyRequest(req)));

    let resp = handler.handle_message(B512::repeat_byte(0x01), &msg);
    assert!(resp.is_some(), "Should respond to BodyRequest");

    if let Some(P2pMessage::RskMessage(rsk_msg)) = resp {
        if let RskSubMessage::BodyResponse(r) = rsk_msg.sub_message {
            assert_eq!(r.id, 33);
            assert_eq!(r.transactions.len(), 1);
            assert_eq!(r.transactions[0].nonce, 42);
            assert!(r.uncles.is_empty());
        } else {
            panic!("Expected BodyResponse");
        }
    } else {
        panic!("Expected RskMessage response");
    }
}

#[tokio::test]
async fn test_sync_handler_serves_body_request_not_found() {
    use rustock_networking::protocol::{P2pMessage, RskMessage, RskSubMessage};
    use rustock_networking::protocol::P2pHandler;
    use rustock_networking::protocol::rsk::BodyRequest;

    let dir = tempdir().unwrap();
    let store = Arc::new(BlockStore::open(dir.path()).unwrap());
    let verifier = Arc::new(HeaderVerifier::new());
    let peer_store = Arc::new(rustock_networking::peers::PeerStore::new());
    let manager = Arc::new(SyncManager::new(store, verifier, peer_store));

    let (event_tx, _event_rx) = mpsc::unbounded_channel();
    let handler = SyncHandler::new(manager, event_tx);

    let req = BodyRequest { id: 1, hash: B256::repeat_byte(0xFF) };
    let msg = P2pMessage::RskMessage(RskMessage::new(RskSubMessage::BodyRequest(req)));

    let resp = handler.handle_message(B512::ZERO, &msg);
    assert!(resp.is_none(), "Should not respond when body not found");
}

#[tokio::test]
async fn test_sync_handler_forwards_body_response() {
    use rustock_networking::protocol::{P2pMessage, RskMessage, RskSubMessage};
    use rustock_networking::protocol::P2pHandler;
    use rustock_networking::protocol::rsk::BodyResponse;
    use crate::events::SyncEvent;

    let dir = tempdir().unwrap();
    let store = Arc::new(BlockStore::open(dir.path()).unwrap());
    let verifier = Arc::new(HeaderVerifier::new());
    let peer_store = Arc::new(rustock_networking::peers::PeerStore::new());
    let manager = Arc::new(SyncManager::new(store, verifier, peer_store));

    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let handler = SyncHandler::new(manager, event_tx);

    let resp = BodyResponse {
        id: 42,
        transactions: vec![],
        uncles: vec![],
    };
    let msg = P2pMessage::RskMessage(RskMessage::new(RskSubMessage::BodyResponse(resp)));

    let handler_resp = handler.handle_message(B512::repeat_byte(0x01), &msg);
    assert!(handler_resp.is_none());

    match event_rx.try_recv().unwrap() {
        SyncEvent::BodyResponse { id, transactions, uncles, .. } => {
            assert_eq!(id, 42);
            assert!(transactions.is_empty());
            assert!(uncles.is_empty());
        }
        other => panic!("Expected BodyResponse event, got {:?}", other),
    }
}

#[tokio::test]
async fn test_body_download_state_machine() {
    let dir = tempdir().unwrap();
    let store = Arc::new(BlockStore::open(dir.path()).unwrap());

    let genesis = dummy_header(0, B256::ZERO, U256::from(1));
    let genesis_hash = genesis.hash();
    store.update_head(&genesis, U256::from(1)).unwrap();

    let b1 = dummy_header(1, genesis_hash, U256::from(1));
    let b1_hash = b1.hash();
    store.put_header(&b1).unwrap();
    store.put_canonical_hash(1, b1_hash).unwrap();
    store.put_total_difficulty(b1_hash, U256::from(2)).unwrap();
    store.update_head(&b1, U256::from(2)).unwrap();

    let verifier = Arc::new(HeaderVerifier::new());
    let peer_store = Arc::new(rustock_networking::peers::PeerStore::new());

    let peer = B512::repeat_byte(0x01);
    let (tx, _rx) = mpsc::channel(rustock_networking::peers::PEER_CHANNEL_CAPACITY);
    peer_store.add_peer(peer, tx).await;

    let manager = Arc::new(SyncManager::new(store.clone(), verifier, peer_store.clone()));
    let (_event_tx, event_rx) = mpsc::unbounded_channel();
    let mut service = SyncService::new(manager, peer_store, event_rx);

    // Trigger body download; peer_best=1 means we're at tip after downloading
    service.start_body_downloads(1).await;

    match &service.state {
        SyncState::DownloadingBodies { pending_headers, .. } => {
            assert_eq!(pending_headers.len(), 1);
            assert_eq!(pending_headers[0].0, b1_hash);
        }
        _ => panic!("Expected DownloadingBodies, got {:?}", service.state),
    }

    // Simulate receiving the body response
    if let SyncState::DownloadingBodies { id_index, .. } = &service.state {
        let req_id = *id_index.keys().next().unwrap();
        service.on_body_response(req_id, vec![], vec![]).await;
    }

    assert!(matches!(service.state, SyncState::Following),
        "Expected Following after all bodies downloaded, got {:?}", service.state);

    // Body should be stored
    let (txs, ommers) = store.body(b1_hash).unwrap().unwrap();
    assert!(txs.is_empty());
    assert!(ommers.is_empty());
}

#[tokio::test]
async fn test_body_download_all_bodies_present_skips() {
    let dir = tempdir().unwrap();
    let store = Arc::new(BlockStore::open(dir.path()).unwrap());

    let genesis = dummy_header(0, B256::ZERO, U256::from(1));
    let genesis_hash = genesis.hash();
    store.update_head(&genesis, U256::from(1)).unwrap();

    let b1 = dummy_header(1, genesis_hash, U256::from(1));
    let b1_hash = b1.hash();
    store.put_header(&b1).unwrap();
    store.put_canonical_hash(1, b1_hash).unwrap();
    store.put_total_difficulty(b1_hash, U256::from(2)).unwrap();
    store.update_head(&b1, U256::from(2)).unwrap();

    // Pre-store the body
    store.put_body(b1_hash, &[], &[]).unwrap();

    let verifier = Arc::new(HeaderVerifier::new());
    let peer_store = Arc::new(rustock_networking::peers::PeerStore::new());
    let manager = Arc::new(SyncManager::new(store, verifier, peer_store.clone()));
    let (_event_tx, event_rx) = mpsc::unbounded_channel();
    let mut service = SyncService::new(manager, peer_store, event_rx);

    service.start_body_downloads(1).await;

    // Should skip directly to Following since all bodies are present
    assert!(matches!(service.state, SyncState::Following),
        "Expected Following when all bodies present, got {:?}", service.state);
}

// ========== TxRelay tests ==========

#[tokio::test]
async fn test_tx_relay_filters_duplicates() {
    use crate::tx_relay::TxRelay;
    use rustock_networking::protocol::rsk::{RskMessage, RskSubMessage};
    use rustock_networking::protocol::{P2pHandler, P2pMessage};
    use alloy_primitives::Bytes;
    use tokio::sync::mpsc;

    let peer_store = Arc::new(rustock_networking::peers::PeerStore::new());

    let peer_a = alloy_primitives::B512::repeat_byte(0x0a);
    let peer_b = alloy_primitives::B512::repeat_byte(0x0b);
    let (_tx_a, _rx_a) = mpsc::channel(rustock_networking::peers::PEER_CHANNEL_CAPACITY);
    let (tx_b, mut rx_b) = mpsc::channel(rustock_networking::peers::PEER_CHANNEL_CAPACITY);
    peer_store.add_peer(peer_a, _tx_a).await;
    peer_store.add_peer(peer_b, tx_b).await;

    let relay = TxRelay::new(peer_store);
    let tx_data = Bytes::from(vec![0xde, 0xad, 0xbe, 0xef]);

    let msg = P2pMessage::RskMessage(RskMessage::new(
        RskSubMessage::Transactions(vec![tx_data.clone()]),
    ));
    relay.handle_message(peer_a, &msg);
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    assert!(rx_b.try_recv().is_ok(), "peer B should receive the tx");

    relay.handle_message(peer_a, &msg);
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert!(rx_b.try_recv().is_err(), "duplicate should be suppressed");
}

#[tokio::test]
async fn test_tx_relay_submit_transaction() {
    use crate::tx_relay::TxRelay;
    use alloy_primitives::Bytes;
    use tokio::sync::mpsc;

    let peer_store = Arc::new(rustock_networking::peers::PeerStore::new());
    let peer_a = alloy_primitives::B512::repeat_byte(0x0a);
    let (tx_a, mut rx_a) = mpsc::channel(rustock_networking::peers::PEER_CHANNEL_CAPACITY);
    peer_store.add_peer(peer_a, tx_a).await;

    let relay = TxRelay::new(peer_store);
    let raw = Bytes::from(vec![0xca, 0xfe]);
    let hash = relay.submit_transaction(raw).await.unwrap();

    assert_ne!(hash, alloy_primitives::B256::ZERO);
    assert!(rx_a.try_recv().is_ok(), "peer should receive broadcast");
}

// ========== One-batch-ahead execution pipelining ==========
//
// These tests drive the scheduling logic with a controllable execution step
// (set_test_exec) so they exercise finish_batch / poll_execution without a
// real EVM/trie. The mock blocks each invocation until the test releases it,
// letting the test assert state mid-execution (e.g. that the next batch is
// already downloading).

use std::collections::VecDeque;
use std::sync::{Condvar, Mutex};

/// Controllable stand-in for block execution. Each invocation records the
/// batch's last block number, then blocks until the test releases it with an
/// outcome (all_ok). Reports completion so the test can deterministically wait
/// before reaping the job.
#[derive(Default)]
struct ExecControllerInner {
    /// Last block number of each batch as it *started* executing, in order.
    started: Vec<u64>,
    /// Number of batches that have finished executing.
    completed: usize,
    /// Pending release tokens: each `Some(all_ok)` releases one waiting batch.
    releases: VecDeque<bool>,
}

struct ExecController {
    inner: Mutex<ExecControllerInner>,
    cv: Condvar,
}

impl ExecController {
    fn new() -> Arc<Self> {
        Arc::new(Self { inner: Mutex::new(ExecControllerInner::default()), cv: Condvar::new() })
    }

    /// Build an ExecFn bound to this controller.
    fn exec_fn(self: &Arc<Self>) -> crate::service::ExecFn {
        let this = self.clone();
        Arc::new(move |state_root, blocks_since_flush, pending: Vec<(B256, Header)>| {
            let last = pending.last().map(|(_, h)| h.number).unwrap_or(0);
            let mut guard = this.inner.lock().unwrap();
            guard.started.push(last);
            this.cv.notify_all();
            // Wait for a release token.
            while guard.releases.is_empty() {
                guard = this.cv.wait(guard).unwrap();
            }
            let all_ok = guard.releases.pop_front().unwrap();
            guard.completed += 1;
            this.cv.notify_all();
            crate::service::ExecOutcome {
                all_ok,
                new_state_root: state_root,
                blocks_since_flush,
            }
        })
    }

    /// Block until at least `n` batches have *started* executing.
    fn wait_started(&self, n: usize) {
        let mut guard = self.inner.lock().unwrap();
        while guard.started.len() < n {
            guard = self.cv.wait(guard).unwrap();
        }
    }

    /// Block until at least `n` batches have *completed*.
    fn wait_completed(&self, n: usize) {
        let mut guard = self.inner.lock().unwrap();
        while guard.completed < n {
            guard = self.cv.wait(guard).unwrap();
        }
    }

    /// Release one waiting batch with the given outcome.
    fn release(&self, all_ok: bool) {
        let mut guard = self.inner.lock().unwrap();
        guard.releases.push_back(all_ok);
        self.cv.notify_all();
    }

    fn started(&self) -> Vec<u64> {
        self.inner.lock().unwrap().started.clone()
    }
}

/// Store genesis + a linear chain of `count` blocks (headers, canonical map,
/// bodies, TD, head) and return the non-genesis headers in ascending order.
fn store_chain(store: &BlockStore, count: u64) -> Vec<Header> {
    let genesis = dummy_header(0, B256::ZERO, U256::from(1));
    store.update_head(&genesis, U256::from(1)).unwrap();
    store.put_body(genesis.hash(), &[], &[]).unwrap();
    store.set_exec_head(genesis.hash(), B256::ZERO).unwrap();

    let mut prev = genesis.hash();
    let mut td = U256::from(1);
    let mut headers = Vec::new();
    for n in 1..=count {
        let h = dummy_header(n, prev, U256::from(1));
        let hash = h.hash();
        td += U256::from(1);
        store.put_header(&h).unwrap();
        store.put_canonical_hash(n, hash).unwrap();
        store.put_total_difficulty(hash, td).unwrap();
        store.put_body(hash, &[], &[]).unwrap();
        store.update_head(&h, td).unwrap();
        prev = hash;
        headers.push(h);
    }
    headers
}

fn pending_of(headers: &[Header]) -> Vec<(B256, Header)> {
    headers.iter().map(|h| (h.hash(), h.clone())).collect()
}

fn make_service(store: Arc<BlockStore>) -> SyncService {
    let verifier = Arc::new(HeaderVerifier::new());
    let peer_store = Arc::new(rustock_networking::peers::PeerStore::new());
    let manager = Arc::new(SyncManager::new(store, verifier, peer_store.clone()));
    // SyncEvent channel, not a peer channel -- stays unbounded.
    let (_tx, rx) = mpsc::unbounded_channel();
    SyncService::new(manager, peer_store, rx)
}

/// (a) Execution of batch N runs off the event loop, so batch N+1's download
/// begins (next skeleton requested) while N is still executing.
#[tokio::test]
async fn test_pipeline_next_batch_downloads_while_executing() {
    let dir = tempdir().unwrap();
    let store = Arc::new(BlockStore::open(dir.path()).unwrap());
    let headers = store_chain(&store, 6);

    let ctrl = ExecController::new();
    let mut service = make_service(store.clone());
    service.set_test_exec(ctrl.exec_fn());

    // Register a peer so continue_after_bodies can request the next skeleton.
    let peer = B512::repeat_byte(0x01);
    let (tx, mut rx) = mpsc::channel(rustock_networking::peers::PEER_CHANNEL_CAPACITY);
    service.peer_store_for_test().add_peer(peer, tx).await;
    service.peer_store_for_test()
        .update_metadata(&peer, rustock_networking::peers::PeerMetadata {
            best_number: 1000,
            total_difficulty: U256::from(1000),
            ..Default::default()
        })
        .await;

    // Batch N = blocks 1..=3, peer_best ahead so we keep syncing.
    let batch_n = pending_of(&headers[0..3]);
    service.finish_batch(&batch_n, 1000).await;

    // N is executing on the blocking thread.
    ctrl.wait_started(1);
    assert_eq!(ctrl.started(), vec![3]);
    assert!(service.is_executing_for_test(), "batch N should be executing");

    // Meanwhile the next skeleton round was requested (download of N+1 started).
    assert!(matches!(service.state, SyncState::DownloadingSkeleton { .. }),
        "next batch download should start concurrently, got {:?}", service.state);
    assert!(rx.try_recv().is_ok(), "a skeleton request should have been sent");

    ctrl.release(true);
    ctrl.wait_completed(1);
}

/// (b) Depth cap: while N executes, completing N+1's download must NOT start
/// N+2's download. N+1 is parked; only after N finishes does N+1 execute.
#[tokio::test]
async fn test_pipeline_depth_capped_at_one_ahead() {
    let dir = tempdir().unwrap();
    let store = Arc::new(BlockStore::open(dir.path()).unwrap());
    let headers = store_chain(&store, 9);

    let ctrl = ExecController::new();
    let mut service = make_service(store.clone());
    service.set_test_exec(ctrl.exec_fn());

    let peer = B512::repeat_byte(0x01);
    let (tx, _rx) = mpsc::channel(rustock_networking::peers::PEER_CHANNEL_CAPACITY);
    service.peer_store_for_test().add_peer(peer, tx).await;
    service.peer_store_for_test()
        .update_metadata(&peer, rustock_networking::peers::PeerMetadata {
            best_number: 1000,
            total_difficulty: U256::from(1000),
            ..Default::default()
        })
        .await;

    // N = 1..=3 starts executing.
    service.finish_batch(&pending_of(&headers[0..3]), 1000).await;
    ctrl.wait_started(1);
    assert!(service.is_executing_for_test());

    // N+1 = 4..=6 finishes downloading while N still executes → parked.
    service.finish_batch(&pending_of(&headers[3..6]), 1000).await;

    // The cap: N+1 must NOT have started executing, and no N+2 download begins.
    assert_eq!(ctrl.started(), vec![3], "N+1 must not execute while N runs");
    assert!(service.has_parked_batch_for_test(), "N+1 should be parked");
    assert!(matches!(service.state, SyncState::Idle),
        "downloading must pause at the cap (no N+2), got {:?}", service.state);

    // on_tick while N still executes must not start N+2 either.
    service.on_tick().await;
    assert!(service.has_parked_batch_for_test());
    assert_eq!(ctrl.started(), vec![3]);

    // Now let N finish: poll_execution should start N+1 and resume downloading.
    ctrl.release(true);   // releases N
    ctrl.wait_completed(1);
    service.await_reapable_execution_for_test().await;
    service.on_tick().await; // reaps N, spawns N+1
    ctrl.wait_started(2);
    assert_eq!(ctrl.started(), vec![3, 6], "N+1 executes only after N completes");
    assert!(!service.has_parked_batch_for_test(), "parked batch consumed");

    ctrl.release(true);   // releases N+1
    ctrl.wait_completed(2);
}

/// (c) An execution failure on batch N halts the sync, does not advance the
/// download cursor past the executed head, and drops any parked batch.
#[tokio::test]
async fn test_pipeline_exec_failure_halts() {
    let dir = tempdir().unwrap();
    let store = Arc::new(BlockStore::open(dir.path()).unwrap());
    let headers = store_chain(&store, 6);

    let ctrl = ExecController::new();
    let mut service = make_service(store.clone());
    service.set_test_exec(ctrl.exec_fn());

    let peer = B512::repeat_byte(0x01);
    let (tx, _rx) = mpsc::channel(rustock_networking::peers::PEER_CHANNEL_CAPACITY);
    service.peer_store_for_test().add_peer(peer, tx).await;
    service.peer_store_for_test()
        .update_metadata(&peer, rustock_networking::peers::PeerMetadata {
            best_number: 1000,
            total_difficulty: U256::from(1000),
            ..Default::default()
        })
        .await;

    // N = 1..=3 begins executing; N+1 = 4..=6 gets parked.
    service.finish_batch(&pending_of(&headers[0..3]), 1000).await;
    ctrl.wait_started(1);
    service.finish_batch(&pending_of(&headers[3..6]), 1000).await;
    assert!(service.has_parked_batch_for_test());

    // N fails. poll_execution must halt: the parked batch is dropped (it must
    // not execute on the wrong parent state), nothing is left executing, and
    // the download cursor is reset to the executed head (genesis, #0 — nothing
    // was committed by the mock) so the retry re-downloads/re-executes from
    // there rather than skipping past the failed range. (A halt leaves the
    // service Idle; the very next tick may legitimately restart a fresh round,
    // so we assert the durable halt effects, not the transient Idle state.)
    ctrl.release(false);
    ctrl.wait_completed(1);
    service.await_reapable_execution_for_test().await;
    service.on_tick().await;

    assert!(!service.has_parked_batch_for_test(), "parked batch must be dropped on halt");
    assert!(!service.is_executing_for_test(), "no batch should be executing after halt");
    assert_eq!(service.last_body_height_for_test(), 0,
        "download cursor must reset to executed head, not advance past the failure");
}

/// (d) Across a normal two-batch sequence, both batches execute in order and
/// the execution cursor (exec_head) advances to the last block.
#[tokio::test]
async fn test_pipeline_two_batches_advance_exec_head() {
    let dir = tempdir().unwrap();
    let store = Arc::new(BlockStore::open(dir.path()).unwrap());
    let headers = store_chain(&store, 6);

    // Real-ish exec that advances exec_head to the batch's last block so we
    // can assert cursor progress (the scheduling, not EVM, is under test).
    let store_for_exec = store.clone();
    let exec_fn: crate::service::ExecFn = Arc::new(move |state_root, bsf, pending: Vec<(B256, Header)>| {
        if let Some((hash, h)) = pending.last() {
            let _ = store_for_exec.set_exec_head(*hash, B256::from_slice(&[h.number as u8; 32]));
        }
        crate::service::ExecOutcome { all_ok: true, new_state_root: state_root, blocks_since_flush: bsf }
    });

    let mut service = make_service(store.clone());
    service.set_test_exec(exec_fn);

    let peer = B512::repeat_byte(0x01);
    let (tx, _rx) = mpsc::channel(rustock_networking::peers::PEER_CHANNEL_CAPACITY);
    service.peer_store_for_test().add_peer(peer, tx).await;
    service.peer_store_for_test()
        .update_metadata(&peer, rustock_networking::peers::PeerMetadata {
            best_number: 1000,
            total_difficulty: U256::from(1000),
            ..Default::default()
        })
        .await;

    // Batch 1 = 1..=3. Executes (synchronously-ish; mock returns immediately).
    service.finish_batch(&pending_of(&headers[0..3]), 1000).await;
    // Reap batch 1.
    drain_executions(&mut service).await;
    assert_eq!(exec_head_number(&store), 3, "exec head should reach #3 after batch 1");

    // Batch 2 = 4..=6.
    service.finish_batch(&pending_of(&headers[3..6]), 1000).await;
    drain_executions(&mut service).await;
    assert_eq!(exec_head_number(&store), 6, "exec head should reach #6 after batch 2");

    assert_eq!(service.last_body_height_for_test(), 6, "download cursor at #6");
}

/// Poll on_tick until no background execution remains in flight.
async fn drain_executions(service: &mut SyncService) {
    // Bounded by wall-clock, not by iteration count. Execution runs on a
    // background task, so a fixed number of polls is really a bet on how much
    // CPU this process gets -- and on a loaded machine that bet loses, failing
    // the test for reasons that have nothing to do with the code under test.
    // Sleeping rather than only yielding also lets the executor actually run.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while std::time::Instant::now() < deadline {
        if !service.is_executing_for_test() && !service.has_parked_batch_for_test() {
            return;
        }
        service.on_tick().await;
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }
    panic!("executions did not drain within 30s");
}

fn exec_head_number(store: &BlockStore) -> u64 {
    store
        .exec_head()
        .ok()
        .flatten()
        .and_then(|(hash, _)| store.header(hash).ok().flatten())
        .map(|h| h.number)
        .unwrap_or(0)
}

// -- Stage 1/2: the coherence invariant and the Φ watchdog ----------------

/// Build a store holding a linked chain 0..=`len`, with `KEY_HEAD` and the
/// executed head both at the top.
fn linked_chain(len: u64) -> (Arc<BlockStore>, tempfile::TempDir, Vec<Header>) {
    let dir = tempdir().unwrap();
    let store = Arc::new(BlockStore::open(dir.path()).unwrap());
    let mut headers = Vec::new();
    let mut parent = B256::ZERO;
    for n in 0..=len {
        let h = dummy_header(n, parent, U256::from(1));
        store.update_head(&h, U256::from(n + 1)).unwrap();
        parent = h.hash();
        headers.push(h);
    }
    let top = headers.last().unwrap();
    store.set_exec_head(top.hash(), top.state_root).unwrap();
    (store, dir, headers)
}

fn service_over(store: Arc<BlockStore>) -> SyncService {
    let verifier = Arc::new(HeaderVerifier::new());
    let peer_store = Arc::new(rustock_networking::peers::PeerStore::new());
    let manager = Arc::new(SyncManager::new(store, verifier, peer_store.clone()));
    let (_tx, event_rx) = mpsc::unbounded_channel();
    SyncService::new(manager, peer_store, event_rx)
}

/// Stall 6, end to end.
///
/// `ensure_canonical_lineage` writes a canonical entry above `KEY_HEAD`. Every
/// path that looks for work reads `KEY_HEAD`, so the block is held, linked,
/// valid and unreachable — on mainnet, for three hours with no error logged.
/// Head re-verification walks the index up and makes it reachable again.
#[tokio::test]
async fn head_reverification_recovers_a_block_stranded_above_the_head() {
    let (store, _dir, headers) = linked_chain(10);
    let top = headers.last().unwrap();

    // The stranded block: stored, canonical, linked — but KEY_HEAD stays at #10.
    let stranded = dummy_header(11, top.hash(), U256::from(1));
    store.put_header_with_hash(stranded.hash(), &stranded).unwrap();
    store.put_canonical_hash(11, stranded.hash()).unwrap();
    assert_eq!(store.head().unwrap(), Some(top.hash()), "precondition: head is behind");

    // The invariant sees it immediately.
    let violation = crate::invariant::check(&store, None, crate::Scope::Full).unwrap_err();
    assert_eq!(violation.relation(), "I6");

    let mut service = service_over(store.clone());
    service.last_body_height = 10;
    service.reverify_head_lineage(1);

    assert_eq!(store.head().unwrap(), Some(stranded.hash()), "head did not advance");
    assert_eq!(
        crate::invariant::check(&store, None, crate::Scope::Full),
        Ok(()),
        "the store is still incoherent after re-verification"
    );
    assert!(
        service.last_body_height <= 10,
        "the body cursor must not sit above the block we just made reachable"
    );
}

/// The walk up must stop at the first link that does not hold, or it would
/// adopt a chain the node cannot actually follow.
#[tokio::test]
async fn head_reverification_stops_at_a_broken_link() {
    let (store, _dir, headers) = linked_chain(10);
    let top = headers.last().unwrap();

    let good = dummy_header(11, top.hash(), U256::from(1));
    store.put_header_with_hash(good.hash(), &good).unwrap();
    store.put_canonical_hash(11, good.hash()).unwrap();

    // #12 is canonical and held, but builds on something else entirely.
    let detached = dummy_header(12, B256::repeat_byte(0x77), U256::from(1));
    store.put_header_with_hash(detached.hash(), &detached).unwrap();
    store.put_canonical_hash(12, detached.hash()).unwrap();

    let mut service = service_over(store.clone());
    service.reverify_head_lineage(0);

    assert_eq!(store.head().unwrap(), Some(good.hash()), "walked past a broken link");
}

/// A canonical pointer naming a block we never downloaded (stall 5) must not
/// be walked onto.
#[tokio::test]
async fn head_reverification_does_not_adopt_a_block_we_do_not_hold() {
    let (store, _dir, headers) = linked_chain(10);
    let top = headers.last().unwrap();
    store.put_canonical_hash(11, B256::repeat_byte(0xaa)).unwrap();

    let mut service = service_over(store.clone());
    service.reverify_head_lineage(0);

    assert_eq!(store.head().unwrap(), Some(top.hash()), "adopted a block we do not hold");
}

/// With nothing above the head, the escalation retreats — but only into ground
/// execution has not already covered, so the range is downloaded again without
/// leaving execution stranded above the head.
#[tokio::test]
async fn head_reverification_retreats_when_there_is_nothing_above() {
    let (store, _dir, headers) = linked_chain(10);
    // Execution is back at #5, so there is room to retreat into.
    store.set_exec_head(headers[5].hash(), headers[5].state_root).unwrap();

    let mut service = service_over(store.clone());
    service.last_body_height = 10;
    service.reverify_head_lineage(3);

    assert_eq!(store.head().unwrap(), Some(headers[7].hash()), "did not retreat three blocks");
    assert!(service.last_body_height <= 7);
    assert_eq!(
        crate::invariant::check(&store, None, crate::Scope::Full),
        Ok(()),
        "the retreat left the store incoherent"
    );
}

/// A coherent, caught-up node must not be disturbed by either mechanism.
#[tokio::test]
async fn a_healthy_node_is_left_alone() {
    let (store, _dir, headers) = linked_chain(10);
    let top = headers.last().unwrap();

    assert_eq!(crate::invariant::check(&store, None, crate::Scope::Full), Ok(()));

    let mut service = service_over(store.clone());
    service.verify_coherence();
    service.reverify_head_lineage(0);
    // No peers, so the watchdog has no target and must not accumulate a stall.
    service.check_phi_progress().await;

    assert_eq!(store.head().unwrap(), Some(top.hash()));
    assert_eq!(service.watchdog.level(), 0);
}

/// The second rung of the ladder has to be a different action from the first,
/// or the escalation is decorative.
#[tokio::test]
async fn the_second_escalation_forces_a_connection_point_search() {
    let (store, _dir, _headers) = linked_chain(10);
    let mut service = service_over(store);
    assert!(!service.force_connection_search);

    // Drive the watchdog to its second rung with a gap that never improves.
    let t0 = std::time::Instant::now();
    let window = std::time::Duration::from_secs(1);
    service.watchdog = ProgressWatchdog::new(window);
    let phi = Phi::new(500, 10, 0, None);
    service.watchdog.observe(phi, t0);
    let first = service.watchdog.observe(phi, t0 + std::time::Duration::from_secs(2));
    let second = service.watchdog.observe(phi, t0 + std::time::Duration::from_secs(4));

    assert!(matches!(first, Some(Escalation::RetreatAndReverify { .. })));
    assert!(matches!(second, Some(Escalation::ResearchConnectionPoint)));
}

/// The upward walk must be bounded, so a damaged index cannot turn every
/// escalation into a full-chain sweep.
#[tokio::test]
async fn head_reverification_bounds_the_upward_walk() {
    let (store, _dir, headers) = linked_chain(10);
    let mut parent = headers.last().unwrap().clone();

    // A linked, canonical, held run far longer than the walk limit.
    for n in 11..=(11 + 5_000u64) {
        let h = dummy_header(n, parent.hash(), U256::from(1));
        store.put_header_with_hash(h.hash(), &h).unwrap();
        store.put_canonical_hash(n, h.hash()).unwrap();
        parent = h;
    }

    let mut service = service_over(store.clone());
    service.reverify_head_lineage(0);

    let head = store.header(store.head().unwrap().unwrap()).unwrap().unwrap();
    assert!(head.number > 10, "the walk made no progress at all");
    assert!(
        head.number <= 10 + 4_096,
        "the walk was not bounded: reached #{}",
        head.number
    );
}

/// The live failure from the first deployment, 2026-09-22 20:32.
///
/// `KEY_HEAD` named a non-canonical block at #9,262,401 while the canonical
/// index held #9,262,401 and #9,262,402, linked. The walk up compared the
/// stranded block's parent against `KEY_HEAD`'s hash, saw a mismatch, and
/// retreated — when everything needed to walk forward was present.
#[tokio::test]
async fn head_reverification_reseats_a_head_that_is_not_canonical_then_walks_up() {
    let (store, _dir, headers) = linked_chain(10);
    let canonical_top = headers.last().unwrap();

    // A sibling at #10, held but not canonical, wrongly named as the head.
    let mut sibling = dummy_header(10, headers[9].parent_hash, U256::from(1));
    sibling.timestamp += 7;
    store.put_header_with_hash(sibling.hash(), &sibling).unwrap();
    store.set_head(sibling.hash()).unwrap();

    // And a block above the canonical head: held, canonical, linked.
    let above = dummy_header(11, canonical_top.hash(), U256::from(1));
    store.put_header_with_hash(above.hash(), &above).unwrap();
    store.put_canonical_hash(11, above.hash()).unwrap();

    let mut service = service_over(store.clone());
    service.reverify_head_lineage(1);

    assert_eq!(
        store.head().unwrap(),
        Some(above.hash()),
        "should have re-seated onto the canonical chain and then walked up"
    );
    assert_eq!(crate::invariant::check(&store, None, crate::Scope::Full), Ok(()));
}

/// Retreating below the executed head breaks I4 by construction — which the
/// first deployment did within fifteen seconds of its first escalation.
#[tokio::test]
async fn head_reverification_never_retreats_below_the_executed_head() {
    let (store, _dir, headers) = linked_chain(10);
    let top = headers.last().unwrap();
    // Executed and head both at #10: there is nowhere to retreat to.
    store.set_exec_head(top.hash(), top.state_root).unwrap();

    let mut service = service_over(store.clone());
    service.reverify_head_lineage(5);

    assert_eq!(store.head().unwrap(), Some(top.hash()), "retreated below execution");
    assert_eq!(crate::invariant::check(&store, None, crate::Scope::Full), Ok(()));
}

/// With execution further back, a retreat is allowed but stops at it.
#[tokio::test]
async fn head_reverification_retreats_only_as_far_as_the_executed_head() {
    let (store, _dir, headers) = linked_chain(10);
    store.set_exec_head(headers[8].hash(), headers[8].state_root).unwrap();

    let mut service = service_over(store.clone());
    service.reverify_head_lineage(5);

    let head = store.header(store.head().unwrap().unwrap()).unwrap().unwrap();
    assert_eq!(head.number, 8, "should have stopped at the executed head");
    assert_eq!(crate::invariant::check(&store, None, crate::Scope::Full), Ok(()));
}

/// I6 is briefly true on essentially every new tip: a header is canonicalised
/// as soon as it links, and the head only advances once the body is executed.
/// Reporting that would put a warning in the log every few seconds and teach
/// whoever reads it to skip the line — which is the failure this subsystem
/// already had, where a stall printed 450 times and was treated as nothing.
#[tokio::test]
async fn a_transient_violation_is_not_reported() {
    let (store, _dir, headers) = linked_chain(10);
    let top = headers.last().unwrap();

    let above = dummy_header(11, top.hash(), U256::from(1));
    store.put_header_with_hash(above.hash(), &above).unwrap();
    store.put_canonical_hash(11, above.hash()).unwrap();

    let mut service = service_over(store.clone());

    // Seen, clock started, nothing said.
    service.verify_coherence();
    assert!(service.violation_since.is_some(), "the violation was not noticed");
    assert!(service.last_violation_log.is_none(), "a transient violation was reported");

    // Still inside the grace period.
    service.verify_coherence();
    assert!(service.last_violation_log.is_none(), "reported before the grace period elapsed");

    // It clears, as an ordinary tip does.
    store.set_head(above.hash()).unwrap();
    service.verify_coherence();
    assert!(service.violation_since.is_none(), "a cleared violation was not forgotten");
}

/// A violation that outlives the grace period is exactly what the node was
/// blind to for three hours, and must be reported.
///
/// Uses I1 rather than I6: I6 now repairs itself on the tick that finds it, so
/// it can no longer persist — which is the point of the fix, and makes it the
/// wrong relation to test persistence with.
#[tokio::test]
async fn a_persistent_violation_is_reported() {
    let (store, _dir, _headers) = linked_chain(10);
    // A hole at the head's own height: inside the delta window, and not one of
    // the relations with an automatic repair.
    store.delete_canonical_hash(10).unwrap();

    let mut service = service_over(store.clone());
    service.verify_coherence();

    // Backdate the first sighting past the grace period.
    let (id, _) = service.violation_since.clone().unwrap();
    service.violation_since =
        Some((id, std::time::Instant::now() - std::time::Duration::from_secs(120)));

    service.verify_coherence();
    assert!(service.last_violation_log.is_some(), "a three-hour-class violation went unreported");
}

/// A different relation breaking restarts the clock rather than inheriting the
/// previous one's age — otherwise one long-standing violation would make every
/// later one look instantly urgent.
#[tokio::test]
async fn a_different_violation_starts_its_own_clock() {
    let (store, _dir, headers) = linked_chain(10);
    let top = headers.last().unwrap();
    let above = dummy_header(11, top.hash(), U256::from(1));
    store.put_header_with_hash(above.hash(), &above).unwrap();
    store.put_canonical_hash(11, above.hash()).unwrap();

    let mut service = service_over(store.clone());
    service.verify_coherence();
    let (first_id, _) = service.violation_since.clone().unwrap();

    // Resolve I6 and break something else inside the checked window. (Height
    // 5 would be invisible here: the delta scope spans the executed head to
    // the validated head, which is the whole point of it being cheap.)
    store.set_head(above.hash()).unwrap();
    store.delete_canonical_hash(11).unwrap();
    service.verify_coherence();

    let (second_id, _) = service.violation_since.clone().unwrap();
    assert_ne!(first_id, second_id, "the new violation inherited the old identity");
    assert!(service.last_violation_log.is_none(), "reported a freshly-seen violation at once");
}

/// The root cause of stall 6, fixed at its source.
///
/// The orphan-adopt path declares a lineage canonical and, before this, left
/// KEY_HEAD behind it — so the blocks it had just adopted were held, linked,
/// valid and unreachable, because every path that looks for work reads
/// KEY_HEAD.
#[tokio::test]
async fn adopting_a_canonical_lineage_moves_the_head_onto_it() {
    let (store, _dir, headers) = linked_chain(10);
    let top = headers.last().unwrap();

    let a = dummy_header(11, top.hash(), U256::from(1));
    let b = dummy_header(12, a.hash(), U256::from(1));
    for h in [&a, &b] {
        store.put_header_with_hash(h.hash(), h).unwrap();
    }
    // There is no longer an API that writes the lineage without the head:
    // `ensure_canonical_lineage` is crate-private to the storage crate, and
    // `Transition::Adopt` is the only public way in. That is the fix — the
    // state stall 6 lived in is not reachable from here any more.
    let proved = rustock_storage::Validated::prove(&store, b.hash()).unwrap();
    assert_eq!(proved.lineage().len(), 2, "both fork blocks should need canonical entries");
    store.apply(&rustock_storage::Transition::Adopt { head: proved }).unwrap();

    assert_eq!(store.head().unwrap(), Some(b.hash()), "the head did not follow the lineage");
    assert_eq!(crate::invariant::check(&store, None, crate::Scope::Full), Ok(()));
    let _ = top;
}

/// I6 must be repaired the moment it is seen, not three minutes later when the
/// watchdog gets to it. Waiting made the node sawtooth between the tip and
/// three minutes behind it, because adopting an orphaned tip is routine.
#[tokio::test]
async fn an_i6_violation_is_repaired_on_the_tick_that_finds_it() {
    let (store, _dir, headers) = linked_chain(10);
    let top = headers.last().unwrap();

    let above = dummy_header(11, top.hash(), U256::from(1));
    store.put_header_with_hash(above.hash(), &above).unwrap();
    store.put_canonical_hash(11, above.hash()).unwrap();

    let mut service = service_over(store.clone());
    service.verify_coherence();

    assert_eq!(store.head().unwrap(), Some(above.hash()), "I6 was seen but not repaired");
    assert_eq!(crate::invariant::check(&store, None, crate::Scope::Full), Ok(()));
}

/// The repair must never invent a chain: a canonical pointer to a block we do
/// not hold stays unrepaired and reported rather than adopted.
#[tokio::test]
async fn the_immediate_repair_never_adopts_a_block_we_do_not_hold() {
    let (store, _dir, headers) = linked_chain(10);
    let top = headers.last().unwrap();
    store.put_canonical_hash(11, B256::repeat_byte(0xaa)).unwrap();

    let mut service = service_over(store.clone());
    service.verify_coherence();

    assert_eq!(store.head().unwrap(), Some(top.hash()), "adopted a block we do not hold");
}

/// Mainnet, 2026-09-23 03:34.
///
/// A reorg orphaned the executed head. The rollback targeted the orphan's own
/// **parent**, which is on the branch being abandoned and therefore not
/// canonical either — so `Transition::Executed` refused the write (correctly),
/// the caller gave up, and the executed head stayed on the orphan. I5 broke
/// within a minute, I7 half an hour later once that branch's state root was no
/// longer in the trie, and the node could not execute another block.
///
/// The guard was right. The target was wrong. Rollback must land on the
/// canonical chain.
#[tokio::test]
async fn rolling_execution_back_lands_on_the_canonical_chain_not_the_orphans_parent() {
    let dir = tempdir().unwrap();
    let store = Arc::new(BlockStore::open(dir.path()).unwrap());

    let genesis = dummy_header(0, B256::ZERO, U256::from(1));
    store.update_head(&genesis, U256::from(1)).unwrap();

    // Canonical branch: a1, a2 — each with its own state root.
    let mut a1 = dummy_header(1, genesis.hash(), U256::from(2));
    a1.state_root = B256::repeat_byte(0xA1);
    let mut a2 = dummy_header(2, a1.hash(), U256::from(3));
    a2.state_root = B256::repeat_byte(0xA2);

    // Abandoned branch: b1, b2 — b1 is NOT canonical, which is the whole point.
    let mut b1 = dummy_header(1, genesis.hash(), U256::from(2));
    b1.extra_data = vec![0xBB].into();
    b1.state_root = B256::repeat_byte(0xB1);
    let mut b2 = dummy_header(2, b1.hash(), U256::from(3));
    b2.extra_data = vec![0xBB].into();
    b2.state_root = B256::repeat_byte(0xB2);

    for h in [&a1, &a2, &b1, &b2] {
        store.put_header(h).unwrap();
    }
    store.put_canonical_hash(1, a1.hash()).unwrap();
    store.put_canonical_hash(2, a2.hash()).unwrap();
    store.set_head(a2.hash()).unwrap();

    // Execution stranded on the abandoned branch.
    store.set_exec_head(b2.hash(), b2.state_root).unwrap();

    // The trie holds the canonical state, and deliberately NOT b1's — so a
    // rollback onto the orphan's parent could not succeed even if it were
    // allowed.
    let trie = Arc::new(rustock_trie::MemoryTrieStore::new()) as Arc<dyn rustock_trie::TrieStore>;
    let empty = rustock_trie::TrieNode::empty();
    trie.put(a1.state_root.as_slice(), &empty.to_message(trie.as_ref()));
    trie.put(genesis.state_root.as_slice(), &empty.to_message(trie.as_ref()));

    let verifier = Arc::new(HeaderVerifier::new());
    let peer_store = Arc::new(rustock_networking::peers::PeerStore::new());
    let manager = Arc::new(SyncManager::new(store.clone(), verifier, peer_store.clone()));
    let (_tx, event_rx) = mpsc::unbounded_channel();
    let processor = rustock_execution::BlockProcessor::new(
        rustock_execution::RskHardforkConfig::mainnet(),
        store.clone(),
    );
    let mut service = SyncService::new(manager, peer_store, event_rx)
        .with_block_processor(processor, trie, empty);

    assert!(
        service.roll_execution_back(rustock_storage::BlockRef::new(2, b2.hash())),
        "rollback gave up; execution is left stranded on the abandoned branch"
    );

    // Genesis, not a1.
    //
    // a1 is canonical at #1 and its state is present, so a search by HEIGHT
    // would choose it -- and that is precisely the 2026-09-23 bug. a1 is a
    // *sibling* of b1, not an ancestor of b2: this node never executed it, so
    // claiming it as the executed head is a lie the next block discovers as a
    // nonce mismatch. The only block on b2's ancestry that is canonical with
    // state is genesis, the fork point.
    let (exec_hash, exec_root) = store.exec_head().unwrap().unwrap();
    assert_eq!(exec_hash, genesis.hash(), "resumed at a block this node never executed");
    assert_eq!(exec_root, genesis.state_root);
    let _ = (&a1, &a2);
    assert_eq!(
        crate::invariant::check(&store, None, crate::Scope::Full),
        Ok(()),
        "the store is incoherent after the rollback"
    );
}

/// A rollback must not land on a canonical block whose state we no longer
/// hold: that is I7, and the node would fail on its next execution instead of
/// at the moment the decision was made.
#[tokio::test]
async fn rolling_execution_back_skips_heights_whose_state_is_gone() {
    let dir = tempdir().unwrap();
    let store = Arc::new(BlockStore::open(dir.path()).unwrap());

    let genesis = dummy_header(0, B256::ZERO, U256::from(1));
    store.update_head(&genesis, U256::from(1)).unwrap();

    let mut chain = vec![genesis.clone()];
    for n in 1..=5u64 {
        let mut h = dummy_header(n, chain[(n - 1) as usize].hash(), U256::from(n + 1));
        h.state_root = B256::repeat_byte(n as u8);
        store.update_head(&h, U256::from(n + 1)).unwrap();
        chain.push(h);
    }
    store.set_exec_head(chain[5].hash(), chain[5].state_root).unwrap();

    // Only #2's state survives; #3, #4 and #5 have been collected.
    let trie = Arc::new(rustock_trie::MemoryTrieStore::new()) as Arc<dyn rustock_trie::TrieStore>;
    let empty = rustock_trie::TrieNode::empty();
    trie.put(chain[2].state_root.as_slice(), &empty.to_message(trie.as_ref()));

    let verifier = Arc::new(HeaderVerifier::new());
    let peer_store = Arc::new(rustock_networking::peers::PeerStore::new());
    let manager = Arc::new(SyncManager::new(store.clone(), verifier, peer_store.clone()));
    let (_tx, event_rx) = mpsc::unbounded_channel();
    let processor = rustock_execution::BlockProcessor::new(
        rustock_execution::RskHardforkConfig::mainnet(),
        store.clone(),
    );
    let mut service = SyncService::new(manager, peer_store, event_rx)
        .with_block_processor(processor, trie, empty);

    assert!(
        service.roll_execution_back(rustock_storage::BlockRef::new(5, chain[5].hash())),
        "rollback gave up with a usable state at #2"
    );

    let (exec_hash, _) = store.exec_head().unwrap().unwrap();
    assert_eq!(exec_hash, chain[2].hash(), "landed on a height whose state is missing");
}

/// With no usable state anywhere in range, the rollback must say so rather
/// than leave execution somewhere it cannot continue from.
#[tokio::test]
async fn rolling_execution_back_reports_when_no_state_survives() {
    let dir = tempdir().unwrap();
    let store = Arc::new(BlockStore::open(dir.path()).unwrap());
    let genesis = dummy_header(0, B256::ZERO, U256::from(1));
    store.update_head(&genesis, U256::from(1)).unwrap();

    let mut h = dummy_header(1, genesis.hash(), U256::from(2));
    h.state_root = B256::repeat_byte(0x11);
    store.update_head(&h, U256::from(2)).unwrap();
    store.set_exec_head(h.hash(), h.state_root).unwrap();

    // An empty trie: nothing is resumable.
    let trie = Arc::new(rustock_trie::MemoryTrieStore::new()) as Arc<dyn rustock_trie::TrieStore>;
    let empty = rustock_trie::TrieNode::empty();

    let verifier = Arc::new(HeaderVerifier::new());
    let peer_store = Arc::new(rustock_networking::peers::PeerStore::new());
    let manager = Arc::new(SyncManager::new(store.clone(), verifier, peer_store.clone()));
    let (_tx, event_rx) = mpsc::unbounded_channel();
    let processor = rustock_execution::BlockProcessor::new(
        rustock_execution::RskHardforkConfig::mainnet(),
        store.clone(),
    );
    let mut service = SyncService::new(manager, peer_store, event_rx)
        .with_block_processor(processor, trie, empty);

    assert!(
        !service.roll_execution_back(rustock_storage::BlockRef::new(1, h.hash())),
        "claimed success with no state to resume from"
    );
}

/// Mainnet, 2026-09-23 14:10. The executed head was perfectly canonical — so
/// nothing considered it orphaned — but its state root was no longer in the
/// trie store. I7 reported it every 30 seconds and no recovery path responded,
/// and the node sat in a retry loop failing every block with
/// `NonceTooHigh { state: 0 }`: an account read back empty, which is what a
/// missing trie node looks like.
///
/// A violation nothing acts on is a slower version of no violation at all.
#[tokio::test]
async fn a_missing_state_root_at_the_executed_head_is_repaired_not_just_reported() {
    let dir = tempdir().unwrap();
    let store = Arc::new(BlockStore::open(dir.path()).unwrap());

    let genesis = dummy_header(0, B256::ZERO, U256::from(1));
    store.update_head(&genesis, U256::from(1)).unwrap();

    let mut chain = vec![genesis.clone()];
    for n in 1..=4u64 {
        let mut h = dummy_header(n, chain[(n - 1) as usize].hash(), U256::from(n + 1));
        h.state_root = B256::repeat_byte(0x40 + n as u8);
        store.update_head(&h, U256::from(n + 1)).unwrap();
        chain.push(h);
    }

    // The executed head IS canonical — I5 is satisfied — but its state is gone.
    store.set_exec_head(chain[4].hash(), chain[4].state_root).unwrap();

    let trie = Arc::new(rustock_trie::MemoryTrieStore::new()) as Arc<dyn rustock_trie::TrieStore>;
    let empty = rustock_trie::TrieNode::empty();
    trie.put(chain[3].state_root.as_slice(), &empty.to_message(trie.as_ref()));

    let verifier = Arc::new(HeaderVerifier::new());
    let peer_store = Arc::new(rustock_networking::peers::PeerStore::new());
    let manager = Arc::new(SyncManager::new(store.clone(), verifier, peer_store.clone()));
    let (_tx, event_rx) = mpsc::unbounded_channel();
    let processor = rustock_execution::BlockProcessor::new(
        rustock_execution::RskHardforkConfig::mainnet(),
        store.clone(),
    );
    let mut service = SyncService::new(manager, peer_store, event_rx)
        .with_block_processor(processor, trie.clone(), empty);

    // Precondition: the invariant sees it, and I5 does not.
    let violation = crate::invariant::check(&store, Some(trie.as_ref()), crate::Scope::Full)
        .unwrap_err();
    assert_eq!(violation.relation(), "I7");

    service.verify_coherence();

    let (exec_hash, exec_root) = store.exec_head().unwrap().unwrap();
    assert_eq!(exec_hash, chain[3].hash(), "execution was not rolled back to usable state");
    assert_eq!(exec_root, chain[3].state_root);
    assert_eq!(
        crate::invariant::check(&store, Some(trie.as_ref()), crate::Scope::Full),
        Ok(()),
        "still incoherent after the repair"
    );
}

/// Mainnet, 2026-09-23 17:21. `Transition::Retreat` rolled the *committed*
/// executed head back on its own — that is what it is for — and the service's
/// *in-memory* state root was never told. Follow mode refused to execute in
/// that state, correctly, but refusing is not recovering, and the body
/// pipeline had no such guard: it executed against the stale root and produced
/// `NonceTooLow { tx: 240848, state: 240849 }`.
///
/// Two representations of one fact, and a code path reading the wrong one.
#[tokio::test]
async fn an_in_memory_state_root_ahead_of_the_committed_one_is_reloaded() {
    let dir = tempdir().unwrap();
    let store = Arc::new(BlockStore::open(dir.path()).unwrap());

    let genesis = dummy_header(0, B256::ZERO, U256::from(1));
    store.update_head(&genesis, U256::from(1)).unwrap();

    let mut chain = vec![genesis.clone()];
    for n in 1..=3u64 {
        let mut h = dummy_header(n, chain[(n - 1) as usize].hash(), U256::from(n + 1));
        h.state_root = B256::repeat_byte(0x20 + n as u8);
        store.update_head(&h, U256::from(n + 1)).unwrap();
        chain.push(h);
    }

    let trie = Arc::new(rustock_trie::MemoryTrieStore::new()) as Arc<dyn rustock_trie::TrieStore>;
    // Two distinct, resolvable states: #2's and #3's.
    let committed_node = rustock_trie::TrieNode::empty();
    let committed_root = committed_node.compute_hash(trie.as_ref());
    trie.put(committed_root.as_slice(), &committed_node.to_message(trie.as_ref()));

    // The executed head commits #2 with that root.
    store.set_exec_head(chain[2].hash(), committed_root).unwrap();

    let verifier = Arc::new(HeaderVerifier::new());
    let peer_store = Arc::new(rustock_networking::peers::PeerStore::new());
    let manager = Arc::new(SyncManager::new(store.clone(), verifier, peer_store.clone()));
    let (_tx, event_rx) = mpsc::unbounded_channel();
    let processor = rustock_execution::BlockProcessor::new(
        rustock_execution::RskHardforkConfig::mainnet(),
        store.clone(),
    );

    // The service starts with a DIFFERENT in-memory root — the shape the
    // retreat leaves behind.
    let mut stale = rustock_trie::TrieNode::empty();
    stale = stale.put(
        &rustock_trie::TrieKeySlice::from_key(&[1u8, 2, 3]),
        &[9u8; 8],
        trie.as_ref(),
    );
    stale.save(trie.as_ref(), true);
    let stale_root = stale.compute_hash(trie.as_ref());
    assert_ne!(stale_root, committed_root, "fixture did not create a divergence");

    let mut service = SyncService::new(manager, peer_store, event_rx)
        .with_block_processor(processor, trie.clone(), stale);

    service.resync_state_root_with_exec_head();

    let now = service
        .current_state_root
        .as_ref()
        .map(|r| r.compute_hash(trie.as_ref()));
    assert_eq!(
        now,
        Some(committed_root),
        "the in-memory root was not reloaded from the committed one"
    );
}

/// And it must not churn when they already agree.
#[tokio::test]
async fn a_matching_state_root_is_left_alone() {
    let dir = tempdir().unwrap();
    let store = Arc::new(BlockStore::open(dir.path()).unwrap());
    let genesis = dummy_header(0, B256::ZERO, U256::from(1));
    store.update_head(&genesis, U256::from(1)).unwrap();

    let trie = Arc::new(rustock_trie::MemoryTrieStore::new()) as Arc<dyn rustock_trie::TrieStore>;
    let node = rustock_trie::TrieNode::empty();
    let root = node.compute_hash(trie.as_ref());
    trie.put(root.as_slice(), &node.to_message(trie.as_ref()));
    store.set_exec_head(genesis.hash(), root).unwrap();

    let verifier = Arc::new(HeaderVerifier::new());
    let peer_store = Arc::new(rustock_networking::peers::PeerStore::new());
    let manager = Arc::new(SyncManager::new(store.clone(), verifier, peer_store.clone()));
    let (_tx, event_rx) = mpsc::unbounded_channel();
    let processor = rustock_execution::BlockProcessor::new(
        rustock_execution::RskHardforkConfig::mainnet(),
        store.clone(),
    );
    let mut service = SyncService::new(manager, peer_store, event_rx)
        .with_block_processor(processor, trie.clone(), node);

    service.follow_buffer.insert(
        7,
        (B256::repeat_byte(7), dummy_header(7, B256::ZERO, U256::from(1)), vec![], vec![]),
    );
    service.resync_state_root_with_exec_head();

    assert_eq!(service.follow_buffer.len(), 1, "cleared the buffer when nothing was wrong");
}
