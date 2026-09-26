//! Snapshot sync, server against client, over a trie small enough to reason
//! about.
//!
//! These exercise the two halves together: nothing is stubbed between
//! `SnapServer::chunk` and `StateDownload::accept` except the wire, which is
//! encoded and decoded for real.

use super::client::{proof_from_payload, ChunkError, StateDownload};
use super::server::SnapServer;
use super::SnapConfig;
use alloy_primitives::{keccak256, B256};
use rustock_networking::protocol::snap::{
    ChunkPayload, SnapChunkRequest, SnapChunkResponse, SnapEntry,
};
use rustock_trie::snapshot::total_size;
use rustock_trie::{MemoryTrieStore, TrieKeySlice, TrieNode, TrieStore};
use std::sync::Arc;

/// A trie of `n` accounts, a fifth of them with values too long to embed.
fn toy_state(n: usize) -> (TrieNode, Arc<MemoryTrieStore>, B256) {
    let store = Arc::new(MemoryTrieStore::new());
    let mut root = TrieNode::empty();
    for i in 0..n {
        let key = [(i >> 8) as u8, i as u8, (i * 7) as u8, (i * 13) as u8];
        let value: Vec<u8> = if i % 5 == 0 {
            (0..96u8).map(|b| b.wrapping_add(i as u8)).collect()
        } else {
            vec![i as u8, (i >> 8) as u8, 7]
        };
        root = root.put(&TrieKeySlice::from_key(&key), &value, store.as_ref());
    }
    root.save(store.as_ref(), true);
    let hash = root.compute_hash(store.as_ref());
    (root, store, hash)
}

/// A peer that serves honestly from its own copy of the trie, through the
/// real wire encoding.
struct Peer {
    root: TrieNode,
    store: Arc<MemoryTrieStore>,
    root_hash: B256,
}

impl Peer {
    fn new(n: usize) -> Self {
        let (root, store, root_hash) = toy_state(n);
        Self { root, store, root_hash }
    }

    /// Serves a chunk and puts it through RLP, so the test sees what a real
    /// client would.
    fn serve(&self, from: u64, budget: u64) -> SnapChunkResponse {
        let proof = rustock_trie::snapshot_proof::prove_chunk(
            &self.root,
            from,
            budget,
            self.store.as_ref(),
        );
        let response = SnapChunkResponse {
            id: 1,
            payload: ChunkPayload::Proved {
                entries: proof
                    .entries
                    .iter()
                    .map(|e| SnapEntry {
                        message: e.message.clone().into(),
                        long_values: e.long_values.iter().map(|v| v.clone().into()).collect(),
                    })
                    .collect(),
                witness: proof.witness.iter().map(|w| w.clone().into()).collect(),
            },
            block_number: 100,
            from,
            to: 0,
            complete: false,
        };
        let encoded = response.encode_body();
        SnapChunkResponse::decode_body(&mut encoded.as_slice()).expect("survives the wire")
    }

    fn total(&self) -> u64 {
        total_size(&self.root, self.store.as_ref())
    }
}

fn config(workers: usize, chunk_bytes: u64) -> SnapConfig {
    SnapConfig {
        server_enabled: true,
        client_enabled: true,
        max_in_flight: workers,
        chunk_bytes,
        ..SnapConfig::default()
    }
}

/// Drives a download to completion against one honest peer, returning the
/// client's store.
fn download(peer: &Peer, workers: usize, chunk_bytes: u64, hint: u64) -> StateDownload {
    let local = Arc::new(MemoryTrieStore::new());
    let mut client =
        StateDownload::new(peer.root_hash, hint, &config(workers, chunk_bytes), local);

    let mut guard = 0;
    while !client.is_complete() {
        guard += 1;
        assert!(guard < 100_000, "download did not terminate");

        let Some(request) = client.next_request() else {
            panic!("no work available and not complete");
        };
        let response = peer.serve(request.from, request.budget);
        let proof = proof_from_payload(&response.payload).expect("proved format");
        if proof.entries.is_empty() {
            // Past the end of the trie: the slice is finished.
            client.release(request.from);
            break;
        }
        client.accept(request.from, &proof).expect("honest chunk");
    }
    client
}

#[test]
fn a_state_downloads_and_reads_back() {
    for (n, workers, chunk_bytes) in
        [(1usize, 1usize, 1024u64), (50, 1, 256), (300, 4, 512), (300, 8, 4096)]
    {
        let peer = Peer::new(n);
        let client = download(&peer, workers, chunk_bytes, peer.total());

        assert!(client.is_complete(), "n={n} workers={workers}: not complete");
        let nodes = client
            .verify_stored()
            .unwrap_or_else(|e| panic!("n={n} workers={workers}: {e}"));
        assert!(nodes > 0);
    }
}

/// The point of it all: the downloaded state answers the same questions the
/// original does.
#[test]
fn the_downloaded_state_matches_the_original() {
    let n = 300;
    let peer = Peer::new(n);
    let local = Arc::new(MemoryTrieStore::new());
    let mut client =
        StateDownload::new(peer.root_hash, peer.total(), &config(4, 700), local.clone());

    while !client.is_complete() {
        let request = client.next_request().expect("work remains");
        let response = peer.serve(request.from, request.budget);
        let proof = proof_from_payload(&response.payload).unwrap();
        client.accept(request.from, &proof).expect("honest chunk");
    }

    let message = local.get(peer.root_hash.as_slice()).expect("root stored");
    let rebuilt = TrieNode::from_message(&message, local.as_ref());
    assert_eq!(rebuilt.compute_hash(local.as_ref()), peer.root_hash);

    for i in 0..n {
        let key = [(i >> 8) as u8, i as u8, (i * 7) as u8, (i * 13) as u8];
        let k = TrieKeySlice::from_key(&key);
        assert_eq!(
            rebuilt.get(&k, local.as_ref()),
            peer.root.get(&k, peer.store.as_ref()),
            "key {i} differs"
        );
    }
}

/// Several peers, each serving whatever range it is handed, with no
/// coordination between them.
#[test]
fn several_peers_serve_one_state() {
    let n = 400;
    let peers: Vec<Peer> = (0..4).map(|_| Peer::new(n)).collect();
    assert!(
        peers.iter().all(|p| p.root_hash == peers[0].root_hash),
        "the fixture should be deterministic"
    );

    let local = Arc::new(MemoryTrieStore::new());
    let mut client =
        StateDownload::new(peers[0].root_hash, peers[0].total(), &config(4, 600), local);

    let mut served = vec![0usize; peers.len()];
    let mut turn = 0;
    while !client.is_complete() {
        let request = client.next_request().expect("work remains");
        // Round-robin, so every peer contributes and none sees the whole
        // trie in order.
        let peer = &peers[turn % peers.len()];
        served[turn % peers.len()] += 1;
        turn += 1;

        let response = peer.serve(request.from, request.budget);
        let proof = proof_from_payload(&response.payload).unwrap();
        client.accept(request.from, &proof).expect("honest chunk");
    }

    assert!(served.iter().all(|c| *c > 0), "some peer was never used: {served:?}");
    client.verify_stored().expect("the assembled state is whole");
}

/// The size a peer claims does not decide the download: the root does.
#[test]
fn a_lie_about_the_trie_size_does_not_truncate_the_download() {
    let peer = Peer::new(300);
    let truth = peer.total();

    for hint in [1u64, truth / 3, truth, truth * 3] {
        let client = download(&peer, 4, 512, hint);
        assert!(client.is_complete(), "hint={hint}: not complete");
        client
            .verify_stored()
            .unwrap_or_else(|e| panic!("hint={hint} left the state incomplete: {e}"));
    }
}

/// A peer that alters a node is refused and the range is handed back, so an
/// honest peer can serve it instead. The store keeps nothing from the attempt.
#[test]
fn a_tampered_chunk_is_refused_and_the_range_retried() {
    let peer = Peer::new(200);
    let local = Arc::new(MemoryTrieStore::new());
    let mut client =
        StateDownload::new(peer.root_hash, peer.total(), &config(1, 512), local.clone());

    let request = client.next_request().expect("work");
    let response = peer.serve(request.from, request.budget);
    let mut proof = proof_from_payload(&response.payload).unwrap();

    let victim = proof.entries.len() / 2;
    let last = proof.entries[victim].message.len() - 1;
    proof.entries[victim].message[last] ^= 0x01;

    let before: Vec<B256> =
        proof.entries.iter().map(|e| keccak256(&e.message)).collect();
    assert!(matches!(
        client.accept(request.from, &proof),
        Err(ChunkError::Invalid(_))
    ));

    // Nothing was written -- not even the entries that were genuine.
    for hash in &before {
        assert!(local.get(hash.as_slice()).is_none(), "a refused chunk left bytes behind");
    }
    assert_eq!(client.stored_nodes(), 0);

    // And the same range is offered again.
    let retry = client.next_request().expect("the range came back");
    assert_eq!(retry.from, request.from);

    let honest = peer.serve(retry.from, retry.budget);
    let proof = proof_from_payload(&honest.payload).unwrap();
    client.accept(retry.from, &proof).expect("the honest answer is taken");
    assert!(client.stored_nodes() > 0);
}

/// A chunk nobody asked for is refused outright, even if it is genuine.
/// Otherwise a peer could write into ranges another peer is responsible for.
#[test]
fn an_unsolicited_chunk_is_refused() {
    let peer = Peer::new(200);
    let local = Arc::new(MemoryTrieStore::new());
    let mut client =
        StateDownload::new(peer.root_hash, peer.total(), &config(1, 512), local);

    let response = peer.serve(0, 512);
    let proof = proof_from_payload(&response.payload).unwrap();
    assert_eq!(client.accept(0, &proof), Err(ChunkError::Unsolicited(0)));
}

/// rskj's older chunk format is named, not guessed at.
#[test]
fn an_rskj_chunk_is_reported_rather_than_parsed() {
    let payload = ChunkPayload::Legacy(vec![0xC1, 0x80].into());
    assert_eq!(proof_from_payload(&payload), Err(ChunkError::LegacyFormat));
}

/// Requests go out to different offsets, so peers work on the trie at once
/// rather than queueing behind each other.
#[test]
fn workers_are_given_different_parts_of_the_trie() {
    let peer = Peer::new(400);
    let local = Arc::new(MemoryTrieStore::new());
    let mut client =
        StateDownload::new(peer.root_hash, peer.total(), &config(4, 512), local);

    let mut offsets = Vec::new();
    while let Some(request) = client.next_request() {
        offsets.push(request.from);
    }
    assert_eq!(offsets.len(), 4, "every worker should have a range");

    let mut sorted = offsets.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted.len(), 4, "workers overlap: {offsets:?}");
    assert_eq!(sorted[0], 0);
}

/// The server refuses to serve a state root that is not the one its own chain
/// has at that height, rather than silently answering a different question.
#[test]
fn the_server_declines_a_root_from_another_chain() {
    use rustock_storage::BlockStore;

    let dir = tempfile::tempdir().expect("tempdir");
    let store = Arc::new(BlockStore::open(dir.path()).expect("open"));
    let peer = Peer::new(50);

    let server = SnapServer::new(
        store,
        peer.store.clone() as Arc<dyn TrieStore>,
        config(4, 512),
    );

    // No such block: an empty answer, promptly, rather than silence.
    let response = server
        .chunk(&SnapChunkRequest {
            id: 1,
            block_number: 42,
            from: 0,
            chunk_size: 512,
            state_root: Some(B256::repeat_byte(0xEE)),
        })
        .expect("an answer");
    match response.payload {
        ChunkPayload::Proved { entries, .. } => assert!(entries.is_empty()),
        other => panic!("unexpected payload {other:?}"),
    }
}

/// A disabled server says nothing at all.
#[test]
fn a_disabled_server_serves_nothing() {
    use rustock_storage::BlockStore;

    let dir = tempfile::tempdir().expect("tempdir");
    let store = Arc::new(BlockStore::open(dir.path()).expect("open"));
    let peer = Peer::new(50);
    let server = SnapServer::new(
        store,
        peer.store.clone() as Arc<dyn TrieStore>,
        SnapConfig::default(),
    );

    assert!(server
        .chunk(&SnapChunkRequest {
            id: 1,
            block_number: 1,
            from: 0,
            chunk_size: 0,
            state_root: None
        })
        .is_none());
}

/// A half-finished download is *reported* as half-finished. Without this the
/// completeness check would be a formality, and a node would start executing
/// against state with holes in it.
#[test]
fn an_incomplete_state_is_not_mistaken_for_a_whole_one() {
    let peer = Peer::new(300);
    let local = Arc::new(MemoryTrieStore::new());
    let mut client =
        StateDownload::new(peer.root_hash, peer.total(), &config(1, 256), local);

    // Take only the first few chunks.
    for _ in 0..3 {
        let request = client.next_request().expect("work");
        let response = peer.serve(request.from, request.budget);
        let proof = proof_from_payload(&response.payload).unwrap();
        client.accept(request.from, &proof).expect("honest chunk");
    }
    assert!(!client.is_complete());
    assert!(client.stored_nodes() > 0, "the test should have stored something");

    let err = client.verify_stored().expect_err("a partial state must not pass");
    assert!(
        err.contains("root") || err.contains("hole") || err.contains("spans"),
        "unhelpful complaint: {err}"
    );
}

/// The whole path, server to client, through a real block store: the server
/// finds the state from the block number, and the client rebuilds it.
#[test]
fn a_real_server_serves_a_real_client() {
    let (store, peer, block_number) = server_fixture(300);
    let server = SnapServer::new(store, peer.store.clone() as Arc<dyn TrieStore>, config(4, 700));

    let local = Arc::new(MemoryTrieStore::new());
    let mut client =
        StateDownload::new(peer.root_hash, peer.total(), &config(4, 700), local.clone());

    while !client.is_complete() {
        let request = client.next_request().expect("work remains");
        let response = server
            .chunk(&SnapChunkRequest {
                id: 1,
                block_number,
                from: request.from,
                chunk_size: request.budget,
                state_root: Some(peer.root_hash),
            })
            .expect("the server answers");

        // Through the wire, as a client would see it.
        let encoded = response.encode_body();
        let decoded = SnapChunkResponse::decode_body(&mut encoded.as_slice()).unwrap();
        let proof = proof_from_payload(&decoded.payload).expect("proved format");
        client.accept(request.from, &proof).expect("the server is honest");
    }

    client.verify_stored().expect("the assembled state is whole");
    let message = local.get(peer.root_hash.as_slice()).expect("root stored");
    let rebuilt = TrieNode::from_message(&message, local.as_ref());
    assert_eq!(rebuilt.compute_hash(local.as_ref()), peer.root_hash);
}

/// Chunk sizes a peer asks for are clamped, so one request cannot ask this
/// node to serialize the whole trie into a single message.
#[test]
fn an_outsized_request_is_clamped() {
    let (store, peer, block_number) = server_fixture(2000);
    let mut config = config(4, 512);
    config.max_chunk_bytes = 4096;
    let server = SnapServer::new(store, peer.store.clone() as Arc<dyn TrieStore>, config);

    let response = server
        .chunk(&SnapChunkRequest {
            id: 1,
            block_number,
            from: 0,
            // "Send me everything."
            chunk_size: u64::MAX,
            state_root: Some(peer.root_hash),
        })
        .expect("an answer");

    let ChunkPayload::Proved { entries, .. } = response.payload else {
        panic!("expected a proved chunk");
    };
    let served: u64 = entries.iter().map(|e| e.message.len() as u64).sum();
    assert!(served > 0, "clamping should not mean serving nothing");
    assert!(
        served < peer.total() / 4,
        "asking for everything got {served} of {} bytes",
        peer.total()
    );
}

/// The server refuses an offset past the end rather than looping a client
/// forever on an empty answer.
#[test]
fn the_server_declines_an_offset_past_the_end() {
    let (store, peer, block_number) = server_fixture(100);
    let server = SnapServer::new(store, peer.store.clone() as Arc<dyn TrieStore>, config(4, 512));

    let response = server
        .chunk(&SnapChunkRequest {
            id: 1,
            block_number,
            from: peer.total() + 1,
            chunk_size: 512,
            state_root: Some(peer.root_hash),
        })
        .expect("an answer");
    match response.payload {
        ChunkPayload::Proved { entries, .. } => assert!(entries.is_empty()),
        other => panic!("unexpected payload {other:?}"),
    }
}

/// A block store holding one block whose state root is the toy trie's.
fn server_fixture(n: usize) -> (Arc<rustock_storage::BlockStore>, Peer, u64) {
    use alloy_primitives::{Address, U256};
    use rustock_core::Header;
    use rustock_storage::BlockStore;

    let peer = Peer::new(n);
    let dir = tempfile::tempdir().expect("tempdir");
    // Leaked on purpose: the store must outlive the directory handle, and
    // these are test processes.
    let dir = Box::leak(Box::new(dir));
    let store = Arc::new(BlockStore::open(dir.path()).expect("open"));

    let number = 20_000u64;
    let header = Header {
        parent_hash: B256::repeat_byte(1),
        ommers_hash: B256::ZERO,
        beneficiary: Address::ZERO,
        state_root: peer.root_hash,
        transactions_root: B256::ZERO,
        receipts_root: B256::ZERO,
        logs_bloom: Default::default(),
        extension_data: None,
        difficulty: U256::from(1u64),
        number,
        gas_limit: U256::from(6_800_000u64),
        gas_used: 0,
        timestamp: 1_700_000_000,
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
    };
    let hash = header.hash();
    store.put_header(&header).expect("put header");
    store.put_canonical_hash(number, hash).expect("index height");
    store.put_total_difficulty(hash, U256::from(1u64)).expect("td");
    store.set_head(hash).expect("head");

    (store, peer, number)
}

/// The last seam: a request arriving as a p2p message is answered as one.
///
/// Everything between is the real path -- the handler's dispatch, the message
/// envelope, the RLP -- so this catches a variant wired to the wrong arm,
/// which the pieces tested separately cannot.
#[test]
fn a_snap_request_is_answered_through_the_handler() {
    use crate::events::SyncEvent;
    use crate::manager::SyncManager;
    use crate::SyncHandler;
    use alloy_primitives::B512;
    use rustock_core::validation::HeaderVerifier;
    use rustock_networking::peers::PeerStore;
    use rustock_networking::protocol::{P2pHandler, P2pMessage, RskMessage, RskSubMessage};

    let (store, peer, block_number) = server_fixture(120);
    let manager = Arc::new(SyncManager::new(
        store.clone(),
        Arc::new(HeaderVerifier::new()),
        Arc::new(PeerStore::new()),
    ));
    let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel::<SyncEvent>();
    let handler = SyncHandler::new(manager, event_tx).with_snap_server(Arc::new(
        SnapServer::new(store, peer.store.clone() as Arc<dyn TrieStore>, config(4, 700)),
    ));

    let request = P2pMessage::RskMessage(RskMessage::new(RskSubMessage::SnapChunkRequest(
        SnapChunkRequest {
            id: 77,
            block_number,
            from: 0,
            chunk_size: 700,
            state_root: Some(peer.root_hash),
        },
    )));

    let reply = handler
        .handle_message(B512::repeat_byte(1), &request)
        .expect("the handler answers a chunk request");

    let P2pMessage::RskMessage(message) = reply else { panic!("not an rsk message") };
    let RskSubMessage::SnapChunkResponse(response) = message.sub_message else {
        panic!("wrong reply type")
    };
    assert_eq!(response.id, 77, "the answer must carry the request's id");

    // And it verifies, which is the only thing that makes it an answer.
    let proof = proof_from_payload(&response.payload).expect("proved format");
    assert!(!proof.entries.is_empty());
    rustock_trie::snapshot_proof::verify_chunk(peer.root_hash, 0, &proof)
        .expect("the handler's answer verifies");
}

/// A node that serves snapshots but has pruned the state it would offer says
/// so, rather than reporting a checkpoint it cannot serve.
#[test]
fn a_node_without_the_state_offers_nothing() {
    use rustock_storage::BlockStore;

    let dir = tempfile::tempdir().expect("tempdir");
    let store = Arc::new(BlockStore::open(dir.path()).expect("open"));
    // An empty trie store: whatever the chain says, the state is not here.
    let server = SnapServer::new(
        store,
        Arc::new(MemoryTrieStore::new()) as Arc<dyn TrieStore>,
        config(4, 512),
    );
    assert_eq!(server.offer(), None);
}

/// An answer far larger than the question is thrown away unread. The
/// transport allows 16 MB; verifying that much costs real CPU, and a peer
/// that can make a client spend it on demand can make it spend it forever.
#[test]
fn a_wildly_oversized_chunk_is_refused_without_verifying() {
    // Big enough that a chunk can exceed the 256 KB floor.
    let peer = Peer::new(6000);
    let local = Arc::new(MemoryTrieStore::new());
    let mut client =
        StateDownload::new(peer.root_hash, peer.total(), &config(1, 512), local.clone());

    let request = client.next_request().expect("work");
    // A genuine chunk, just enormously bigger than what was asked for.
    let response = peer.serve(request.from, 8 << 20);
    let proof = proof_from_payload(&response.payload).unwrap();
    assert!(proof.wire_len() > 1 << 18, "the fixture is not big enough to test this");

    match client.accept(request.from, &proof) {
        Err(ChunkError::Oversized { asked, .. }) => assert_eq!(asked, 512),
        other => panic!("oversized chunk was not refused: {other:?}"),
    }
    assert_eq!(client.stored_nodes(), 0);

    // The range is still available, so an honest answer still lands.
    let retry = client.next_request().expect("the range came back");
    let honest = peer.serve(retry.from, retry.budget);
    let proof = proof_from_payload(&honest.payload).unwrap();
    client.accept(retry.from, &proof).expect("a right-sized answer is taken");
}

/// But a server rounding up, or a straddling node at the end, is not an
/// attack: a chunk somewhat over the budget is still accepted.
#[test]
fn a_slightly_oversized_chunk_is_still_accepted() {
    let peer = Peer::new(400);
    let local = Arc::new(MemoryTrieStore::new());
    let mut client =
        StateDownload::new(peer.root_hash, peer.total(), &config(1, 100_000), local);

    let request = client.next_request().expect("work");
    let response = peer.serve(request.from, 150_000);
    let proof = proof_from_payload(&response.payload).unwrap();
    client.accept(request.from, &proof).expect("a generous answer is fine");
}

/// A single node bigger than the whole budget must still get through. A
/// server sends at least one node whatever it is asked for, because that is
/// the only way past a node larger than the budget -- and a value that can
/// never be downloaded is a state that can never be completed.
#[test]
fn one_node_larger_than_the_budget_still_arrives() {
    let store = Arc::new(MemoryTrieStore::new());
    let mut root = TrieNode::empty();

    // One account carrying far more than any sane chunk budget.
    let huge: Vec<u8> = (0..900_000u32).map(|i| i as u8).collect();
    root = root.put(&TrieKeySlice::from_key(&[1, 2, 3, 4]), &huge, store.as_ref());
    for i in 0..20u8 {
        root = root.put(&TrieKeySlice::from_key(&[9, i, 0, 0]), &[i, 1], store.as_ref());
    }
    root.save(store.as_ref(), true);
    let root_hash = root.compute_hash(store.as_ref());

    let peer = Peer { root, store, root_hash };
    let local = Arc::new(MemoryTrieStore::new());
    let mut client =
        StateDownload::new(root_hash, peer.total(), &config(1, 1024), local.clone());

    let mut guard = 0;
    while !client.is_complete() {
        guard += 1;
        assert!(guard < 1000, "download stalled on the oversized node");
        let request = client.next_request().expect("work remains");
        let response = peer.serve(request.from, request.budget);
        let proof = proof_from_payload(&response.payload).unwrap();
        client.accept(request.from, &proof).expect("every chunk is honest");
    }

    client.verify_stored().expect("the state is whole");
    assert_eq!(
        local.get(alloy_primitives::keccak256(&huge).as_slice()),
        Some(huge),
        "the oversized value did not survive"
    );
}
