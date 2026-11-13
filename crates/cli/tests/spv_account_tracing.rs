//! An SPV client reconstructing an account's history, end to end.
//!
//! This is the test the RSKIP exists for. Everything else shows that a piece
//! works; this shows the pieces together do the thing that motivated them:
//!
//! > a wallet that holds no state learns every block in which its account's
//! > rBTC balance moved, from a node that holds no state either, over proofs
//! > neither of them has to trust.
//!
//! Native rBTC is the hard case. A value transfer emits no log, so it sets
//! no bloom bit, so a client watching the headers sees nothing at all. The
//! account modification witness is what makes it findable, and nothing
//! smaller than this test exercises the whole route.
//!
//! # The shape
//!
//! A regtest chain is built from genesis with the witness rule active. Most
//! of its blocks have nothing to do with account X — they move value between
//! two other addresses — and a handful pay X. The test then throws away all
//! knowledge of which blocks those were and recovers them, the way a wallet
//! would:
//!
//! 1. ask for a proof of X at the head;
//! 2. read `lastModifiedBlock` out of the verified account record;
//! 3. ask for a proof of X at the block *before* that one;
//! 4. repeat until the field reads zero.
//!
//! Each hop costs exactly one proof and lands on exactly one block in which X
//! actually changed. That is the whole point: a balance comparison would
//! have to visit every block, and a bloom filter cannot see any of them.
//!
//! The wallet talks only to the SPV node's RPC. The SPV node holds headers
//! and no trie, and gets every answer from the full node by way of the real
//! `SnapServer::trie_proof`, verifying each one against the state root in a
//! header it has itself.

use alloy_primitives::{Address, Bytes, B256, U256};
use k256::ecdsa::SigningKey;
use rustock_core::{Block, Header, Transaction};
use rustock_execution::hardfork::{ConsensusRule, RskHardforkConfig};
use rustock_execution::processor::BlockProcessor;
use rustock_rpc::server::{ProofFetcher, RpcState};
use rustock_storage::BlockStore;
use rustock_sync::snap::server::SnapServer;
use rustock_sync::snap::SnapConfig;
use rustock_trie::{MemoryTrieStore, TrieNode, TrieStore};
use serde_json::json;
use std::sync::Arc;

const CHAIN_ID: u64 = 33;
/// Blocks in the chain. Most of them have nothing to do with X.
const CHAIN_LENGTH: u64 = 24;
/// The blocks that actually pay X. Everything else is noise, and the point
/// of the exercise is that the wallet never visits the noise.
const PAYS_X: &[u64] = &[3, 7, 8, 16];
const PAYMENT: u64 = 1_000_000_000_000_000;

fn x_address() -> Address {
    Address::repeat_byte(0xAB)
}

// ---------------------------------------------------------------------------
// Building a regtest chain
// ---------------------------------------------------------------------------

fn sender_address(key: &SigningKey) -> Address {
    let pk = key.verifying_key().to_encoded_point(false);
    let hash = <sha3::Keccak256 as sha3::Digest>::digest(&pk.as_bytes()[1..]);
    Address::from_slice(&hash[12..])
}

fn sign_tx(tx: &mut Transaction, key: &SigningKey, chain_id: u64) {
    let hash = tx.signing_hash_eip155(chain_id);
    let (signature, recid): (k256::ecdsa::Signature, k256::ecdsa::RecoveryId) =
        key.sign_prehash_recoverable(hash.as_slice()).unwrap();
    let sig_bytes = signature.to_bytes();
    tx.r = U256::from_be_slice(&sig_bytes[..32]);
    tx.s = U256::from_be_slice(&sig_bytes[32..]);
    tx.v = chain_id * 2 + 35 + recid.to_byte() as u64;
}

fn put_account(
    root: &TrieNode,
    store: &dyn TrieStore,
    addr: &Address,
    nonce: u64,
    balance: U256,
) -> TrieNode {
    let acct = rustock_trie::AccountState::new(U256::from(nonce), balance);
    root.put(
        &rustock_trie::TrieKeySlice::from_key(&rustock_trie::account_key(addr)),
        &acct.encode(),
        store,
    )
}

fn header_at(number: u64, parent: B256) -> Header {
    Header {
        parent_hash: parent,
        ommers_hash: B256::ZERO,
        beneficiary: Address::repeat_byte(0x01),
        state_root: B256::ZERO,
        transactions_root: B256::ZERO,
        receipts_root: B256::ZERO,
        logs_bloom: alloy_primitives::Bloom::ZERO,
        extension_data: None,
        difficulty: U256::from(1_000_000u64),
        number,
        gas_limit: U256::from(6_800_000u64),
        gas_used: 0,
        timestamp: 1_700_000_000 + number,
        extra_data: Bytes::new(),
        paid_fees: U256::ZERO,
        minimum_gas_price: U256::ZERO,
        uncle_count: 0,
        umm_root: None,
        version_fields: None,
        bitcoin_merged_mining_header: None,
        bitcoin_merged_mining_merkle_proof: None,
        bitcoin_merged_mining_coinbase_transaction: None,
        cached_hash: None,
        cached_hash_for_merged_mining: None,
    }
}

/// Everything the chain left behind.
struct Chain {
    blocks: Arc<BlockStore>,
    trie: Arc<MemoryTrieStore>,
    /// Block number -> X's balance after it, for the blocks that paid X.
    expected: Vec<(u64, U256)>,
    _tmp: tempfile::TempDir,
}

fn build_chain() -> Chain {
    let tmp = tempfile::tempdir().expect("tempdir");
    let blocks = Arc::new(BlockStore::open(tmp.path()).expect("block store"));
    let trie = Arc::new(MemoryTrieStore::new());

    // The witness is scheduled on no network, so the chain turns it on by
    // hand from its first block. Below that height an account reads zero,
    // which is "not modified since activation" and is exactly what a real
    // activation leaves behind.
    let cfg = RskHardforkConfig::for_network(CHAIN_ID)
        .with_consensus_rule(ConsensusRule::AccountWitness, Some(0));
    let processor = BlockProcessor::new(cfg.clone(), blocks.clone());

    let payer = SigningKey::from_slice(&[7u8; 32]).unwrap();
    let payer_addr = sender_address(&payer);
    let bystander = Address::repeat_byte(0xCD);
    let funding = U256::from(10u64).pow(U256::from(20));

    let mut root = put_account(&TrieNode::empty(), trie.as_ref(), &payer_addr, 0, funding);
    let mut parent = B256::ZERO;
    let mut expected: Vec<(u64, U256)> = Vec::new();
    let mut x_balance = U256::ZERO;

    for number in 1..=CHAIN_LENGTH {
        let pays_x = PAYS_X.contains(&number);
        let (to, value) = if pays_x {
            (x_address(), U256::from(PAYMENT))
        } else {
            // Noise: value between two addresses the wallet never asks about.
            (bystander, U256::from(1u64))
        };

        let mut tx = Transaction {
            nonce: number - 1,
            gas_price: U256::ZERO,
            gas_limit: U256::from(21_000u64),
            to: Bytes::copy_from_slice(to.as_slice()),
            value,
            input: Bytes::default(),
            v: 0,
            r: U256::ZERO,
            s: U256::ZERO,
            cached_rlp: None,
        };
        sign_tx(&mut tx, &payer, CHAIN_ID);

        // Two passes, as the node does: a header has to carry the roots its
        // own execution produces, so run it once to learn them and then
        // build the header that commits to them.
        let draft = Block {
            header: header_at(number, parent),
            transactions: vec![tx.clone()],
            ommers: vec![],
        };
        let learned = processor
            .execute_block(&draft, &root, trie.clone())
            .unwrap_or_else(|e| panic!("#{number} must execute: {e:?}"));

        let mut header = header_at(number, parent);
        header.state_root = learned.state_root_hash;
        header.receipts_root = learned.receipts_root;
        header.transactions_root = rustock_core::ordered_tx_trie_root(
            std::slice::from_ref(&tx),
            cfg.has_unitrie_state_root(number),
        );
        header.ommers_hash = rustock_execution::processor::compute_ommers_hash(&[]);
        header.gas_used = learned.gas_used;
        header.paid_fees = learned.paid_fees;
        header.logs_bloom = learned.logs_bloom;

        let block = Block { header: header.clone(), transactions: vec![tx], ommers: vec![] };
        let hash = block.hash();

        let committed = processor
            .process_block(&block, &root, trie.clone())
            .unwrap_or_else(|e| panic!("#{number} must process: {e:?}"));

        blocks.put_header(&header).expect("put header");
        blocks.put_header_number(hash, number).expect("put number");
        blocks.put_canonical_hash(number, hash).expect("canonical");
        blocks.set_head(hash).expect("head");
        blocks.set_exec_head(hash, committed.state_root_hash).expect("exec head");

        root = committed.new_state_root;
        parent = hash;
        if pays_x {
            x_balance += U256::from(PAYMENT);
            expected.push((number, x_balance));
        }
    }

    Chain { blocks, trie, expected, _tmp: tmp }
}

// ---------------------------------------------------------------------------
// The two nodes
// ---------------------------------------------------------------------------

/// The SPV node's link to a peer: the real server code, reached directly
/// rather than over a socket.
///
/// The wire codec is covered by its own round-trip tests; what matters here
/// is that the proof the server actually builds is the one the verifier
/// actually accepts, and that nothing in between quietly substitutes local
/// state for a peer's answer.
struct FullNodePeer(SnapServer);

#[async_trait::async_trait]
impl ProofFetcher for FullNodePeer {
    async fn fetch_proof(
        &self,
        block_number: u64,
        address: Address,
        storage_keys: Vec<B256>,
    ) -> Result<(Vec<Bytes>, Vec<Vec<Bytes>>), String> {
        let req = rustock_networking::protocol::rsk::TrieProofRequest {
            id: 1,
            block_number,
            address,
            storage_keys,
        };
        let resp = self.0.trie_proof(&req).ok_or("the peer would not answer")?;
        Ok((resp.account_proof, resp.storage_proofs))
    }
}

fn spv_state(chain: &Chain) -> RpcState {
    let snap_config = SnapConfig { server_enabled: true, ..Default::default() };
    let peer = FullNodePeer(SnapServer::new(
        chain.blocks.clone(),
        chain.trie.clone() as Arc<dyn TrieStore>,
        snap_config,
    ));

    RpcState {
        store: chain.blocks.clone(),
        peer_store: Arc::new(rustock_networking::PeerStore::new()),
        config: Arc::new(rustock_core::config::ChainConfig::regtest()),
        tx_submitter: None,
        // No state of its own. This is what makes it an SPV node, and
        // leaving a trie here would let the answers come from local data and
        // prove nothing.
        trie_store: None,
        proof_fetcher: Some(Arc::new(peer)),
        hardfork_cfg: None,
        filter_store: Arc::new(rustock_rpc::logs::FilterStore::new()),
        tx_pool: None,
        wire_queue_depth: None,
        gas_price: None,
        epoch_store: None,
        snapshots: None,
        miner: None,
        auto_miner: None,
        wallet: None,
        admin_enabled: false,
        gc_burial: 0,
        gc_pins: None,
        prune_keep_depth: 0,
        prune_max_batch: 0,
        logs_max_blocks: 1000,
        gas_estimator_binary_search: false,
        scoring: None,
        events: None,
    }
}

// ---------------------------------------------------------------------------
// The wallet
// ---------------------------------------------------------------------------

/// What one `eth_getProof` told the wallet.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Reading {
    balance: U256,
    last_modified: u64,
}

async fn read_account(state: &RpcState, addr: Address, at: &str) -> Reading {
    let req = rustock_rpc::types::JsonRpcRequest {
        jsonrpc: Some("2.0".into()),
        method: "eth_getProof".into(),
        params: json!([format!("{addr:#x}"), [], at]),
        id: Some(json!(1)),
    };
    let resp = rustock_rpc::server::dispatch_rpc(state, req).await;
    let r = resp
        .result
        .unwrap_or_else(|| panic!("eth_getProof at {at} failed: {:?}", resp.error));

    let parse = |k: &str| -> u64 {
        let s = r[k].as_str().unwrap_or("0x0");
        u64::from_str_radix(s.trim_start_matches("0x"), 16).unwrap_or(0)
    };
    Reading {
        balance: U256::from_str_radix(
            r["balance"].as_str().unwrap().trim_start_matches("0x"),
            16,
        )
        .unwrap(),
        last_modified: parse("lastModifiedBlock"),
    }
}

#[tokio::test]
async fn a_wallet_recovers_an_accounts_whole_history_through_an_spv_node() {
    let chain = build_chain();
    let spv = spv_state(&chain);
    let x = x_address();

    // Sanity: the chain really did what the fixture claims.
    assert_eq!(chain.expected.len(), PAYS_X.len());
    let final_balance = chain.expected.last().unwrap().1;

    // --- the walk, exactly as a wallet would do it ---
    let head = read_account(&spv, x, "latest").await;
    assert_eq!(head.balance, final_balance, "the head balance must be proved, not trusted");

    let mut found: Vec<(u64, U256)> = Vec::new();
    let mut cursor = head.last_modified;
    let mut hops = 0usize;

    while cursor > 0 {
        hops += 1;
        assert!(hops <= CHAIN_LENGTH as usize, "the walk must terminate");

        let at = read_account(&spv, x, &format!("{cursor:#x}")).await;
        assert_eq!(
            at.last_modified, cursor,
            "#{cursor} was named as the last modification, so it must say so itself"
        );
        found.push((cursor, at.balance));

        // The hop that makes this O(k) rather than O(N): ask the block
        // *before* this one and it names the previous modification directly,
        // skipping everything in between.
        if cursor == 1 {
            break;
        }
        let before = read_account(&spv, x, &format!("{:#x}", cursor - 1)).await;
        cursor = before.last_modified;
    }

    found.reverse();
    assert_eq!(
        found, chain.expected,
        "the wallet must recover exactly the blocks that paid X, and the balance after each"
    );

    // One proof per modified block, plus one lookback each, plus the head.
    // What matters is that it is proportional to the four payments and not
    // to the twenty-four blocks.
    assert_eq!(hops, PAYS_X.len(), "one hop per block that actually changed X");
}

/// The negative half, and the reason the witness is needed at all: the
/// blocks the wallet skipped really do contain transactions, and X really is
/// untouched by them. A client comparing balances would have had to open
/// every one.
#[tokio::test]
async fn the_blocks_the_wallet_skipped_did_not_touch_x() {
    let chain = build_chain();
    let spv = spv_state(&chain);
    let x = x_address();

    for number in 1..=CHAIN_LENGTH {
        let r = read_account(&spv, x, &format!("{number:#x}")).await;
        if PAYS_X.contains(&number) {
            assert_eq!(r.last_modified, number, "#{number} paid X and must say so");
        } else {
            assert_ne!(
                r.last_modified, number,
                "#{number} did not touch X and must not claim to"
            );
        }
    }
}

/// Everything above would read the same if the SPV node were quietly
/// answering from a local trie. It has none -- this is what says so.
#[tokio::test]
async fn the_spv_node_has_no_state_of_its_own() {
    let chain = build_chain();
    let spv = spv_state(&chain);
    assert!(spv.trie_store.is_none(), "an SPV node must hold no trie");

    // With no peer either, the same question has no answer at all: the
    // balances above came over the link, not from local data.
    let mut stranded = spv_state(&chain);
    stranded.proof_fetcher = None;
    let req = rustock_rpc::types::JsonRpcRequest {
        jsonrpc: Some("2.0".into()),
        method: "eth_getProof".into(),
        params: json!([format!("{:#x}", x_address()), [], "latest"]),
        id: Some(json!(1)),
    };
    let resp = rustock_rpc::server::dispatch_rpc(&stranded, req).await;
    assert!(resp.result.is_none(), "with no trie and no peer there is nothing to answer from");
}
