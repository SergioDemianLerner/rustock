use crate::server::{dispatch_for_test, RpcState};
use crate::types::*;
use alloy_primitives::{Address, Bloom, B256, U256, Bytes};
use rustock_core::config::ChainConfig;
use rustock_core::types::header::Header;
use rustock_networking::peers::PeerStore;
use rustock_storage::BlockStore;
use serde_json::{json, Value};
use std::sync::Arc;

fn test_header(number: u64) -> Header {
    Header {
        parent_hash: B256::ZERO,
        ommers_hash: B256::ZERO,
        beneficiary: Address::ZERO,
        state_root: B256::ZERO,
        transactions_root: B256::ZERO,
        receipts_root: B256::ZERO,
        logs_bloom: Bloom::ZERO,
        extension_data: None,
        difficulty: U256::from(1000),
        number,
        gas_limit: U256::from(8_000_000),
        gas_used: 21000,
        timestamp: 1_600_000_000 + number,
        extra_data: Bytes::new(),
        paid_fees: U256::ZERO,
        minimum_gas_price: U256::from(59_240_000),
        uncle_count: 0,
        umm_root: None,
        bitcoin_merged_mining_header: None,
        bitcoin_merged_mining_merkle_proof: None,
        bitcoin_merged_mining_coinbase_transaction: None,
        cached_hash: None,
        cached_hash_for_merged_mining: None,
    }
}

fn setup_state() -> (RpcState, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    let store = Arc::new(BlockStore::open(tmp.path()).unwrap());

    let header = test_header(42);
    store.update_head(&header, U256::from(42_000)).unwrap();

    let state = RpcState {
        store,
        peer_store: Arc::new(PeerStore::new()),
        config: Arc::new(ChainConfig::mainnet()),
        tx_submitter: None,
        trie_store: None,
        hardfork_cfg: None,
        filter_store: Arc::new(crate::logs::FilterStore::new()),
        tx_pool: None,
        epoch_store: None,
        miner: None,
        admin_enabled: false,
        gc_burial: 4000,
        prune_keep_depth: 100_000,
        prune_max_batch: 50_000,
    };
    (state, tmp)
}

fn make_request(method: &str, params: Value) -> JsonRpcRequest {
    JsonRpcRequest {
        jsonrpc: Some("2.0".to_string()),
        method: method.to_string(),
        params,
        id: Some(json!(1)),
    }
}

// ========== web3 ==========

#[tokio::test]
async fn test_web3_client_version() {
    let (state, _tmp) = setup_state();
    let req = make_request("web3_clientVersion", json!([]));
    let resp = dispatch_for_test(&state, req).await;
    assert_eq!(resp.result.unwrap(), json!("Rustock/0.1.0"));
}

#[tokio::test]
async fn test_web3_sha3() {
    let (state, _tmp) = setup_state();
    let req = make_request("web3_sha3", json!(["0x68656c6c6f"]));
    let resp = dispatch_for_test(&state, req).await;
    let result = resp.result.unwrap();
    let hash = result.as_str().unwrap();
    assert!(hash.starts_with("0x"));
    assert_eq!(hash.len(), 66);
}

#[tokio::test]
async fn test_web3_sha3_invalid_hex() {
    let (state, _tmp) = setup_state();
    let req = make_request("web3_sha3", json!(["not_hex"]));
    let resp = dispatch_for_test(&state, req).await;
    assert!(resp.error.is_some());
    assert_eq!(resp.error.unwrap().code, INVALID_PARAMS);
}

// ========== net ==========

#[tokio::test]
async fn test_net_version() {
    let (state, _tmp) = setup_state();
    let req = make_request("net_version", json!([]));
    let resp = dispatch_for_test(&state, req).await;
    let version = resp.result.unwrap();
    assert_eq!(version, json!(ChainConfig::mainnet().network_id.to_string()));
}

#[tokio::test]
async fn test_net_listening() {
    let (state, _tmp) = setup_state();
    let req = make_request("net_listening", json!([]));
    let resp = dispatch_for_test(&state, req).await;
    assert_eq!(resp.result.unwrap(), json!(true));
}

#[tokio::test]
async fn test_net_peer_count() {
    let (state, _tmp) = setup_state();
    let req = make_request("net_peerCount", json!([]));
    let resp = dispatch_for_test(&state, req).await;
    assert_eq!(resp.result.unwrap(), json!("0x0"));
}

#[tokio::test]
async fn test_net_peer_list_empty() {
    let (state, _tmp) = setup_state();
    let req = make_request("net_peerList", json!([]));
    let resp = dispatch_for_test(&state, req).await;
    assert_eq!(resp.result.unwrap(), json!([]));
}

// ========== eth ==========

#[tokio::test]
async fn test_eth_protocol_version() {
    let (state, _tmp) = setup_state();
    let req = make_request("eth_protocolVersion", json!([]));
    let resp = dispatch_for_test(&state, req).await;
    assert_eq!(resp.result.unwrap(), json!("0x3e"));
}

#[tokio::test]
async fn test_eth_chain_id() {
    let (state, _tmp) = setup_state();
    let req = make_request("eth_chainId", json!([]));
    let resp = dispatch_for_test(&state, req).await;
    assert_eq!(resp.result.unwrap(), json!("0x1e"));
}

#[tokio::test]
async fn test_eth_block_number() {
    let (state, _tmp) = setup_state();
    let req = make_request("eth_blockNumber", json!([]));
    let resp = dispatch_for_test(&state, req).await;
    assert_eq!(resp.result.unwrap(), json!("0x2a"));
}

#[tokio::test]
async fn test_eth_syncing_not_syncing() {
    let (state, _tmp) = setup_state();
    let req = make_request("eth_syncing", json!([]));
    let resp = dispatch_for_test(&state, req).await;
    assert_eq!(resp.result.unwrap(), json!(false));
}

#[tokio::test]
async fn test_eth_syncing_reports_execution_backlog() {
    // The failure this method used to hide. Header download and execution are
    // separate pipelines: when execution wedges, the downloaded head keeps
    // climbing with the network while the executed head stands still. Taking
    // `currentBlock` from the downloaded head made such a node answer `false`
    // -- fully synced -- while serving state 100 blocks stale.
    let (state, _tmp) = setup_state();

    let executed = test_header(42);
    state.store.put_header(&executed).unwrap();
    state
        .store
        .set_exec_head(executed.hash(), executed.state_root)
        .unwrap();

    // Headers ran ahead; nothing executed them.
    let downloaded = test_header(142);
    state
        .store
        .update_head(&downloaded, U256::from(142_000))
        .unwrap();

    let req = make_request("eth_syncing", json!([]));
    let resp = dispatch_for_test(&state, req).await;
    let result = resp.result.unwrap();

    assert_ne!(result, json!(false), "a 100-block execution backlog is not 'synced'");
    assert_eq!(
        result["currentBlock"],
        json!("0x2a"),
        "currentBlock must be the executed head, not the downloaded one"
    );
    assert_eq!(
        result["highestBlock"],
        json!("0x8e"),
        "downloaded-but-unexecuted blocks count towards the target even with no peers"
    );
}

#[tokio::test]
async fn test_eth_syncing_false_once_execution_catches_up() {
    // The complement: execution level with the downloaded head is synced, and
    // must not report a backlog just because there are no peers to compare to.
    let (state, _tmp) = setup_state();

    let head = test_header(42);
    state.store.put_header(&head).unwrap();
    state
        .store
        .set_exec_head(head.hash(), head.state_root)
        .unwrap();

    let req = make_request("eth_syncing", json!([]));
    let resp = dispatch_for_test(&state, req).await;
    assert_eq!(resp.result.unwrap(), json!(false));
}

#[tokio::test]
async fn test_eth_gas_price() {
    let (state, _tmp) = setup_state();
    let req = make_request("eth_gasPrice", json!([]));
    let resp = dispatch_for_test(&state, req).await;
    let result = resp.result.unwrap();
    assert!(result.as_str().unwrap().starts_with("0x"));
}

#[tokio::test]
async fn test_eth_mining() {
    let (state, _tmp) = setup_state();
    let req = make_request("eth_mining", json!([]));
    let resp = dispatch_for_test(&state, req).await;
    assert_eq!(resp.result.unwrap(), json!(false));
}

#[tokio::test]
async fn test_eth_accounts() {
    let (state, _tmp) = setup_state();
    let req = make_request("eth_accounts", json!([]));
    let resp = dispatch_for_test(&state, req).await;
    assert_eq!(resp.result.unwrap(), json!([]));
}

#[tokio::test]
async fn test_eth_get_block_by_number() {
    let (state, _tmp) = setup_state();
    let req = make_request("eth_getBlockByNumber", json!(["0x2a", false]));
    let resp = dispatch_for_test(&state, req).await;
    let result = resp.result.unwrap();
    assert_eq!(result["number"], json!("0x2a"));
    assert_eq!(result["gasUsed"], json!("0x5208"));
    assert!(result["hash"].as_str().unwrap().starts_with("0x"));
}

#[tokio::test]
async fn test_eth_get_block_by_number_latest() {
    let (state, _tmp) = setup_state();
    let req = make_request("eth_getBlockByNumber", json!(["latest", false]));
    let resp = dispatch_for_test(&state, req).await;
    let result = resp.result.unwrap();
    assert_eq!(result["number"], json!("0x2a"));
}

#[tokio::test]
async fn test_eth_get_block_by_number_with_importer_written_td() {
    // Every existing test writes total difficulty through
    // `put_total_difficulty`, which RLP-encodes it. The rskj import wrote the
    // same column family as raw 32 big-endian bytes, and nothing here ever
    // exercised that -- so `eth_getBlockByNumber` returned null for all 9.2M
    // imported blocks while the whole suite stayed green.
    let (state, _tmp) = setup_state();
    let header = test_header(43);
    let hash = header.hash();
    state.store.put_header(&header).unwrap();
    state.store.put_canonical_hash(43, hash).unwrap();
    state
        .store
        .put_total_difficulty_raw(hash, &U256::from(99_u64).to_be_bytes::<32>())
        .unwrap();

    let req = make_request("eth_getBlockByNumber", json!(["0x2b", false]));
    let resp = dispatch_for_test(&state, req).await;
    let result = resp.result.unwrap();
    assert_ne!(result, json!(null), "an imported block must not read as missing");
    assert_eq!(result["number"], json!("0x2b"));
    assert_eq!(result["totalDifficulty"], json!("0x63"));
}

#[tokio::test]
async fn test_eth_get_block_by_number_survives_an_unreadable_td() {
    // Defence in depth for the same failure: whatever the encoding, a block
    // that exists should still be returned. Losing the total difficulty is not
    // a reason to claim the block does not exist.
    let (state, _tmp) = setup_state();
    let header = test_header(44);
    let hash = header.hash();
    state.store.put_header(&header).unwrap();
    state.store.put_canonical_hash(44, hash).unwrap();
    state
        .store
        .put_total_difficulty_raw(hash, &[0xFF, 0xFF, 0xFF])
        .unwrap();

    let req = make_request("eth_getBlockByNumber", json!(["0x2c", false]));
    let resp = dispatch_for_test(&state, req).await;
    let result = resp.result.unwrap();
    assert_ne!(result, json!(null), "block must survive a corrupt total difficulty");
    assert_eq!(result["number"], json!("0x2c"));
    assert_eq!(result["totalDifficulty"], json!("0x0"), "unknown is reported as zero");
}

#[tokio::test]
async fn test_eth_get_block_by_number_not_found() {
    let (state, _tmp) = setup_state();
    let req = make_request("eth_getBlockByNumber", json!(["0xfffff", false]));
    let resp = dispatch_for_test(&state, req).await;
    assert_eq!(resp.result.unwrap(), Value::Null);
}

#[tokio::test]
async fn test_eth_get_block_by_hash() {
    let (state, _tmp) = setup_state();

    let head_hash = state.store.head().unwrap().unwrap();
    let hash_str = format!("{:#x}", head_hash);

    let req = make_request("eth_getBlockByHash", json!([hash_str, false]));
    let resp = dispatch_for_test(&state, req).await;
    let result = resp.result.unwrap();
    assert_eq!(result["number"], json!("0x2a"));
}

#[tokio::test]
async fn test_eth_get_block_by_hash_not_found() {
    let (state, _tmp) = setup_state();
    let hash = format!("{:#x}", B256::repeat_byte(0xff));
    let req = make_request("eth_getBlockByHash", json!([hash, false]));
    let resp = dispatch_for_test(&state, req).await;
    assert_eq!(resp.result.unwrap(), Value::Null);
}

#[tokio::test]
async fn test_eth_get_block_transaction_count_by_number() {
    let (state, _tmp) = setup_state();
    let req = make_request("eth_getBlockTransactionCountByNumber", json!(["0x2a"]));
    let resp = dispatch_for_test(&state, req).await;
    assert_eq!(resp.result.unwrap(), json!("0x0"));
}

#[tokio::test]
async fn test_eth_get_uncle_count_by_block_number() {
    let (state, _tmp) = setup_state();
    let req = make_request("eth_getUncleCountByBlockNumber", json!(["0x2a"]));
    let resp = dispatch_for_test(&state, req).await;
    assert_eq!(resp.result.unwrap(), json!("0x0"));
}

// ========== rpc ==========

#[tokio::test]
async fn test_rpc_modules() {
    let (state, _tmp) = setup_state();
    let req = make_request("rpc_modules", json!([]));
    let resp = dispatch_for_test(&state, req).await;
    let modules = resp.result.unwrap();
    assert_eq!(modules["eth"], json!("1.0"));
    assert_eq!(modules["net"], json!("1.0"));
    assert_eq!(modules["web3"], json!("1.0"));
    assert_eq!(modules["rsk"], json!("1.0"));
}

// ========== rsk ==========

#[tokio::test]
async fn test_rsk_protocol_version() {
    let (state, _tmp) = setup_state();
    let req = make_request("rsk_protocolVersion", json!([]));
    let resp = dispatch_for_test(&state, req).await;
    assert_eq!(resp.result.unwrap(), json!("0x1"));
}

#[tokio::test]
async fn test_rsk_get_raw_block_header_by_number() {
    let (state, _tmp) = setup_state();
    let req = make_request("rsk_getRawBlockHeaderByNumber", json!(["0x2a"]));
    let resp = dispatch_for_test(&state, req).await;
    let result = resp.result.unwrap();
    let rlp_hex = result.as_str().unwrap();
    assert!(rlp_hex.starts_with("0x"));
    assert!(rlp_hex.len() > 10);
}

#[tokio::test]
async fn test_rsk_get_raw_block_header_by_hash() {
    let (state, _tmp) = setup_state();
    let head_hash = state.store.head().unwrap().unwrap();
    let hash_str = format!("{:#x}", head_hash);
    let req = make_request("rsk_getRawBlockHeaderByHash", json!([hash_str]));
    let resp = dispatch_for_test(&state, req).await;
    let result = resp.result.unwrap();
    assert!(result.as_str().unwrap().starts_with("0x"));
}

// ========== dispatch / error handling ==========

#[tokio::test]
async fn test_method_not_found() {
    let (state, _tmp) = setup_state();
    let req = make_request("nonexistent_method", json!([]));
    let resp = dispatch_for_test(&state, req).await;
    assert!(resp.error.is_some());
    assert_eq!(resp.error.unwrap().code, METHOD_NOT_FOUND);
}

#[tokio::test]
async fn test_unsupported_eth_method() {
    let (state, _tmp) = setup_state();
    let req = make_request("eth_sendTransaction", json!(["0x1234", "latest"]));
    let resp = dispatch_for_test(&state, req).await;
    assert!(resp.error.is_some());
    let err = resp.error.unwrap();
    assert_eq!(err.code, METHOD_NOT_FOUND);
    assert!(err.message.contains("execution engine"));
}

#[tokio::test]
async fn test_unsupported_debug_method() {
    let (state, _tmp) = setup_state();
    let req = make_request("debug_traceTransaction", json!(["0x1234"]));
    let resp = dispatch_for_test(&state, req).await;
    assert!(resp.error.is_some());
    let err = resp.error.unwrap();
    assert!(err.message.contains("execution engine"));
}

#[tokio::test]
async fn test_unsupported_personal_method() {
    let (state, _tmp) = setup_state();
    let req = make_request("personal_unlockAccount", json!([]));
    let resp = dispatch_for_test(&state, req).await;
    assert!(resp.error.is_some());
}

#[tokio::test]
async fn test_unsupported_txpool_method() {
    let (state, _tmp) = setup_state();
    let req = make_request("txpool_status", json!([]));
    let resp = dispatch_for_test(&state, req).await;
    assert!(resp.error.is_some());
}

// ========== BlockResultDTO format ==========

#[tokio::test]
async fn test_block_dto_has_all_fields() {
    let (state, _tmp) = setup_state();
    let req = make_request("eth_getBlockByNumber", json!(["0x2a", false]));
    let resp = dispatch_for_test(&state, req).await;
    let block = resp.result.unwrap();

    let required_fields = [
        "number", "hash", "parentHash", "sha3Uncles", "miner",
        "stateRoot", "transactionsRoot", "receiptsRoot", "logsBloom",
        "difficulty", "totalDifficulty", "gasLimit", "gasUsed",
        "timestamp", "extraData", "minimumGasPrice",
        "transactions", "uncles", "size",
    ];
    for field in &required_fields {
        assert!(block.get(field).is_some(), "Missing field: {}", field);
    }
}

#[tokio::test]
async fn test_block_dto_hex_encoding() {
    let (state, _tmp) = setup_state();
    let req = make_request("eth_getBlockByNumber", json!(["0x2a", false]));
    let resp = dispatch_for_test(&state, req).await;
    let block = resp.result.unwrap();

    let hex_fields = [
        "number", "hash", "parentHash", "sha3Uncles", "miner",
        "stateRoot", "transactionsRoot", "receiptsRoot", "logsBloom",
        "difficulty", "totalDifficulty", "gasLimit", "gasUsed",
        "timestamp", "extraData", "minimumGasPrice", "size",
    ];
    for field in &hex_fields {
        let val = block[field].as_str().unwrap_or("");
        assert!(val.starts_with("0x"), "Field {} should be hex: {}", field, val);
    }
}

// ========== batch requests (dispatch-level) ==========

#[tokio::test]
async fn test_batch_dispatch() {
    let (state, _tmp) = setup_state();

    let requests = vec![
        make_request("eth_blockNumber", json!([])),
        make_request("web3_clientVersion", json!([])),
        make_request("nonexistent_method", json!([])),
    ];

    let mut responses = Vec::new();
    for req in requests {
        responses.push(dispatch_for_test(&state, req).await);
    }

    assert_eq!(responses.len(), 3);
    assert_eq!(responses[0].result.as_ref().unwrap(), &json!("0x2a"));
    assert_eq!(responses[1].result.as_ref().unwrap(), &json!("Rustock/0.1.0"));
    assert!(responses[2].error.is_some());
}

// ========== eth_sendRawTransaction ==========

#[tokio::test]
async fn test_eth_send_raw_transaction_no_submitter() {
    let (state, _tmp) = setup_state();
    let req = make_request("eth_sendRawTransaction", json!(["0xdeadbeef"]));
    let resp = dispatch_for_test(&state, req).await;
    assert!(resp.error.is_some(), "Should error when tx_submitter is None");
}

#[tokio::test]
async fn test_eth_send_raw_transaction_with_submitter() {
    use crate::server::TxSubmitter;
    use alloy_primitives::B256;
    use sha3::Digest;

    struct MockSubmitter;

    #[async_trait::async_trait]
    impl TxSubmitter for MockSubmitter {
        async fn submit_transaction(&self, raw_tx: alloy_primitives::Bytes) -> Result<B256, String> {
            Ok(B256::from_slice(&sha3::Keccak256::digest(&raw_tx)))
        }
    }

    let tmp = tempfile::tempdir().unwrap();
    let store = Arc::new(BlockStore::open(tmp.path()).unwrap());
    let header = test_header(1);
    store.update_head(&header, U256::from(1000)).unwrap();

    let state = RpcState {
        store,
        peer_store: Arc::new(PeerStore::new()),
        config: Arc::new(ChainConfig::mainnet()),
        tx_submitter: Some(Arc::new(MockSubmitter)),
        trie_store: None,
        hardfork_cfg: None,
        filter_store: Arc::new(crate::logs::FilterStore::new()),
        tx_pool: None,
        epoch_store: None,
        miner: None,
        admin_enabled: false,
        gc_burial: 4000,
        prune_keep_depth: 100_000,
        prune_max_batch: 50_000,
    };

    let req = make_request("eth_sendRawTransaction", json!(["0xdeadbeef"]));
    let resp = dispatch_for_test(&state, req).await;
    assert!(resp.error.is_none(), "Should succeed with a submitter");
    let result = resp.result.unwrap();
    let hash_str = result.as_str().unwrap();
    assert!(hash_str.starts_with("0x"));
    assert_eq!(hash_str.len(), 66);
}

#[tokio::test]
async fn test_eth_send_raw_transaction_invalid_hex() {
    use crate::server::TxSubmitter;
    use alloy_primitives::B256;

    struct MockSubmitter;

    #[async_trait::async_trait]
    impl TxSubmitter for MockSubmitter {
        async fn submit_transaction(&self, _raw_tx: alloy_primitives::Bytes) -> Result<B256, String> {
            Ok(B256::ZERO)
        }
    }

    let tmp = tempfile::tempdir().unwrap();
    let store = Arc::new(BlockStore::open(tmp.path()).unwrap());
    let header = test_header(1);
    store.update_head(&header, U256::from(1000)).unwrap();

    let state = RpcState {
        store,
        peer_store: Arc::new(PeerStore::new()),
        config: Arc::new(ChainConfig::mainnet()),
        tx_submitter: Some(Arc::new(MockSubmitter)),
        trie_store: None,
        hardfork_cfg: None,
        filter_store: Arc::new(crate::logs::FilterStore::new()),
        tx_pool: None,
        epoch_store: None,
        miner: None,
        admin_enabled: false,
        gc_burial: 4000,
        prune_keep_depth: 100_000,
        prune_max_batch: 50_000,
    };

    let req = make_request("eth_sendRawTransaction", json!(["0xZZZZ"]));
    let resp = dispatch_for_test(&state, req).await;
    assert!(resp.error.is_some(), "Should error on invalid hex");
}

// ========== Phase 6: State queries with trie ==========

fn setup_state_with_trie() -> (RpcState, tempfile::TempDir) {
    use rustock_trie::{MemoryTrieStore, TrieKeySlice, TrieNode, account_key, code_key, storage_key};

    let tmp = tempfile::tempdir().unwrap();
    let store = Arc::new(BlockStore::open(tmp.path()).unwrap());

    let trie_store = Arc::new(MemoryTrieStore::new());

    // Create accounts in the trie
    let mut root = TrieNode::empty();

    // Account with 10000 balance (0x2710) and nonce 1
    let addr = "0xcd2a3d9f938e13cd947ec05abc7fe734df8dd826".parse::<alloy_primitives::Address>().unwrap();
    let acct = rustock_trie::AccountState::new(U256::from(1), U256::from(10000));
    let key = account_key(&addr);
    root = root.put(&TrieKeySlice::from_key(&key), &acct.encode(), &*trie_store);

    // Code for the account: [0x01, 0x02, 0x03]
    let ckey = code_key(&addr);
    root = root.put(&TrieKeySlice::from_key(&ckey), &[0x01, 0x02, 0x03], &*trie_store);

    // Storage slot 0x01 with value 42
    let mut slot_bytes = [0u8; 32];
    slot_bytes[31] = 1;
    let slot = B256::from(slot_bytes);
    let skey = storage_key(&addr, &slot);
    root = root.put(&TrieKeySlice::from_key(&skey), &[42], &*trie_store);

    root.save(&*trie_store, true);
    let state_root = root.compute_hash(&*trie_store);

    // Store the root node by its hash
    let root_data = root.to_message(&*trie_store);
    rustock_trie::TrieStore::put(&*trie_store, state_root.as_slice(), &root_data);

    let header = Header {
        state_root,
        ..test_header(42)
    };
    let hash = header.hash();
    store.put_header(&header).unwrap();
    store.put_canonical_hash(42, hash).unwrap();
    store.set_head(hash).unwrap();
    store.put_total_difficulty(hash, U256::from(42_000)).unwrap();

    let state = RpcState {
        store,
        peer_store: Arc::new(PeerStore::new()),
        config: Arc::new(ChainConfig::mainnet()),
        tx_submitter: None,
        trie_store: Some(trie_store),
        hardfork_cfg: Some(rustock_execution::RskHardforkConfig::mainnet()),
        filter_store: Arc::new(crate::logs::FilterStore::new()),
        tx_pool: None,
        epoch_store: None,
        miner: None,
        admin_enabled: false,
        gc_burial: 4000,
        prune_keep_depth: 100_000,
        prune_max_batch: 50_000,
    };
    (state, tmp)
}

#[tokio::test]
async fn test_eth_get_balance_with_account() {
    let (state, _tmp) = setup_state_with_trie();
    let addr = "0xcd2a3d9f938e13cd947ec05abc7fe734df8dd826";
    let req = make_request("eth_getBalance", json!([addr, "latest"]));
    let resp = dispatch_for_test(&state, req).await;
    assert_eq!(resp.result.unwrap(), json!("0x2710"));
}

#[tokio::test]
async fn test_eth_get_balance_missing_account() {
    let (state, _tmp) = setup_state_with_trie();
    let addr = "0x0000000000000000000000000000000000000001";
    let req = make_request("eth_getBalance", json!([addr, "latest"]));
    let resp = dispatch_for_test(&state, req).await;
    assert_eq!(resp.result.unwrap(), json!("0x0"));
}

#[tokio::test]
async fn test_eth_get_transaction_count_with_account() {
    let (state, _tmp) = setup_state_with_trie();
    let addr = "0xcd2a3d9f938e13cd947ec05abc7fe734df8dd826";
    let req = make_request("eth_getTransactionCount", json!([addr, "latest"]));
    let resp = dispatch_for_test(&state, req).await;
    assert_eq!(resp.result.unwrap(), json!("0x1"));
}

#[tokio::test]
async fn test_eth_get_transaction_count_missing() {
    let (state, _tmp) = setup_state_with_trie();
    let addr = "0x0000000000000000000000000000000000000001";
    let req = make_request("eth_getTransactionCount", json!([addr, "latest"]));
    let resp = dispatch_for_test(&state, req).await;
    assert_eq!(resp.result.unwrap(), json!("0x0"));
}

#[tokio::test]
async fn test_eth_get_code_existing() {
    let (state, _tmp) = setup_state_with_trie();
    let addr = "0xcd2a3d9f938e13cd947ec05abc7fe734df8dd826";
    let req = make_request("eth_getCode", json!([addr, "latest"]));
    let resp = dispatch_for_test(&state, req).await;
    assert_eq!(resp.result.unwrap(), json!("0x010203"));
}

#[tokio::test]
async fn test_eth_get_code_missing() {
    let (state, _tmp) = setup_state_with_trie();
    let addr = "0x0000000000000000000000000000000000000001";
    let req = make_request("eth_getCode", json!([addr, "latest"]));
    let resp = dispatch_for_test(&state, req).await;
    assert_eq!(resp.result.unwrap(), json!("0x"));
}

#[tokio::test]
async fn test_eth_get_storage_at_nonexistent_slot() {
    let (state, _tmp) = setup_state_with_trie();
    let addr = "0xcd2a3d9f938e13cd947ec05abc7fe734df8dd826";
    let slot = format!("{:#066x}", B256::ZERO);
    let req = make_request("eth_getStorageAt", json!([addr, slot, "latest"]));
    let resp = dispatch_for_test(&state, req).await;
    let expected = "0x0000000000000000000000000000000000000000000000000000000000000000";
    assert_eq!(resp.result.unwrap(), json!(expected));
}

// ========== Phase 6: Transaction and Receipt Lookup ==========

fn setup_state_with_tx() -> (RpcState, tempfile::TempDir, B256) {
    use alloy_rlp::Encodable;
    use sha3::{Digest, Keccak256};

    let tmp = tempfile::tempdir().unwrap();
    let store = Arc::new(BlockStore::open(tmp.path()).unwrap());

    let tx = rustock_core::Transaction {
        nonce: 0,
        gas_price: U256::from(20_000_000_000u64),
        gas_limit: U256::from(21_000),
        to: alloy_primitives::Bytes::from(vec![0x12; 20]),
        value: U256::from(1_000_000),
        input: alloy_primitives::Bytes::default(),
        v: 27,
        r: U256::from(100),
        s: U256::from(200),
        cached_rlp: None,
    };

    let mut tx_buf = Vec::new();
    tx.encode(&mut tx_buf);
    let tx_hash = B256::from_slice(&Keccak256::digest(&tx_buf));

    let header = test_header(1);
    let block_hash = header.hash();
    store.put_header(&header).unwrap();
    store.put_canonical_hash(1, block_hash).unwrap();
    store.set_head(block_hash).unwrap();
    store.put_total_difficulty(block_hash, U256::from(1000)).unwrap();

    store.put_body(block_hash, std::slice::from_ref(&tx), &[]).unwrap();
    store.put_tx_index(tx_hash, block_hash, 0).unwrap();

    let receipt = rustock_core::Receipt {
        post_tx_state: vec![0x01],
        cumulative_gas_used: 21_000,
        gas_used: 21_000,
        logs_bloom: alloy_primitives::Bloom::ZERO,
        logs: vec![],
        status: true,
    };
    store.put_receipts(block_hash, &[receipt]).unwrap();

    let state = RpcState {
        store,
        peer_store: Arc::new(PeerStore::new()),
        config: Arc::new(ChainConfig::mainnet()),
        tx_submitter: None,
        trie_store: None,
        hardfork_cfg: None,
        filter_store: Arc::new(crate::logs::FilterStore::new()),
        tx_pool: None,
        epoch_store: None,
        miner: None,
        admin_enabled: false,
        gc_burial: 4000,
        prune_keep_depth: 100_000,
        prune_max_batch: 50_000,
    };
    (state, tmp, tx_hash)
}

/// The hash `eth_getBlockBy*` reports must be the hash `eth_getTransactionByHash`
/// can find -- a round trip through the production code, with nothing computed
/// by hand in the test.
///
/// This broke because three different hash implementations existed: the
/// canonical `Transaction::tx_hash()` (which hashes the ORIGINAL bytes when
/// cached at decode time) and two RPC-local helpers that re-encoded instead.
/// The index is keyed by the canonical hash, so for every transaction whose RLP
/// does not round-trip byte-for-byte -- about 42% of mainnet transactions when
/// measured on the production node -- the RPC handed out a hash that it could
/// not then resolve.
///
/// The fixture deliberately uses a transaction whose cached RLP differs from a
/// re-encoding, and indexes it with `index_block_transactions`, the function the
/// commit path uses, rather than calling `put_tx_index` by hand.
#[tokio::test]
async fn reported_tx_hash_is_the_one_that_can_be_looked_up() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Arc::new(BlockStore::open(tmp.path()).unwrap());

    let mut tx = rustock_core::Transaction {
        nonce: 0,
        gas_price: U256::from(1),
        gas_limit: U256::from(21_000),
        to: alloy_primitives::Bytes::from(vec![0x12; 20]),
        value: U256::from(1_000_000),
        input: alloy_primitives::Bytes::default(),
        v: 27,
        r: U256::from(100),
        s: U256::from(200),
        cached_rlp: None,
    };
    // Non-canonical but perfectly decodable bytes: `r` carries a leading zero,
    // which is what Java's BigInteger.toByteArray() produces for a positive
    // number whose top bit is set. rustock decodes it leniently (see
    // rlp_compat) to the same value, but re-encoding drops the zero -- so the
    // stored bytes and a re-encoding hash differently. This is the real shape
    // of the divergence, not a synthetic one.
    let mut payload: Vec<u8> = Vec::new();
    payload.push(0x80);                                   // nonce 0
    payload.push(0x01);                                   // gas_price 1
    payload.extend_from_slice(&[0x82, 0x52, 0x08]);       // gas_limit 21000
    payload.push(0x94);                                   // to: 20-byte string
    payload.extend_from_slice(&[0x12; 20]);
    payload.extend_from_slice(&[0x83, 0x0f, 0x42, 0x40]); // value 1_000_000
    payload.push(0x80);                                   // input: empty
    payload.push(0x1b);                                   // v 27
    payload.extend_from_slice(&[0x82, 0x00, 0x80]);       // r 128, WITH a leading zero
    payload.extend_from_slice(&[0x81, 0xc8]);             // s 200
    let mut original = vec![0xc0 + payload.len() as u8];
    original.extend_from_slice(&payload);

    tx.r = U256::from(128);
    tx.cached_rlp = Some(original);

    let mut reencoded = Vec::new();
    alloy_rlp::Encodable::encode(&tx, &mut reencoded);
    assert_ne!(
        tx.tx_hash(),
        {
            use sha3::Digest;
            B256::from_slice(&sha3::Keccak256::digest(&reencoded))
        },
        "fixture is pointless unless the two hashes differ"
    );

    let header = test_header(1);
    let block_hash = header.hash();
    store.put_header(&header).unwrap();
    store.put_canonical_hash(1, block_hash).unwrap();
    store.set_head(block_hash).unwrap();
    store.put_total_difficulty(block_hash, U256::from(1000)).unwrap();
    store.put_body(block_hash, std::slice::from_ref(&tx), &[]).unwrap();
    // The production indexer, not a hand-written index entry.
    store.index_block_transactions(block_hash, std::slice::from_ref(&tx)).unwrap();

    let state = RpcState {
        store,
        peer_store: Arc::new(PeerStore::new()),
        config: Arc::new(ChainConfig::mainnet()),
        tx_submitter: None,
        trie_store: None,
        hardfork_cfg: None,
        filter_store: Arc::new(crate::logs::FilterStore::new()),
        tx_pool: None,
        epoch_store: None,
        miner: None,
        admin_enabled: false,
        gc_burial: 4000,
        prune_keep_depth: 100_000,
        prune_max_batch: 50_000,
    };

    // Ask the node for the block, take the hash it reports...
    let req = make_request("eth_getBlockByNumber", json!(["0x1", false]));
    let resp = dispatch_for_test(&state, req).await;
    let block = resp.result.clone();
    let reported = match resp.result.as_ref().and_then(|r| r["transactions"].get(0)).and_then(|v| v.as_str()) {
        Some(h) => h.to_string(),
        None => panic!("block response had no transactions: {block:?} err={:?}", resp.error),
    };

    // ...and require that the same hash resolves.
    let req = make_request("eth_getTransactionByHash", json!([reported.clone()]));
    let resp = dispatch_for_test(&state, req).await;
    let found = resp.result.unwrap();
    assert!(
        !found.is_null(),
        "the hash reported by eth_getBlockByNumber ({reported}) must be resolvable"
    );
    assert_eq!(found["hash"], json!(reported));
    assert_eq!(found["blockNumber"], json!("0x1"));

    // Every RPC that reports a transaction hash must report the SAME one.
    // eth_getTransactionByBlockNumberAndIndex had its own third copy of the
    // hash computation and disagreed with eth_getBlockByNumber about the very
    // same transaction, handing out a hash that resolved to nothing.
    for method in ["eth_getTransactionByBlockNumberAndIndex", "eth_getTransactionByBlockHashAndIndex"] {
        let first = if method.ends_with("NumberAndIndex") {
            json!(["0x1", "0x0"])
        } else {
            json!([format!("{:#x}", block_hash), "0x0"])
        };
        let req = make_request(method, first);
        let resp = dispatch_for_test(&state, req).await;
        let got = resp.result.unwrap();
        assert_eq!(
            got["hash"], json!(reported),
            "{method} must agree with eth_getBlockByNumber on the transaction hash"
        );
    }
}

#[tokio::test]
async fn test_eth_get_transaction_by_hash() {
    let (state, _tmp, tx_hash) = setup_state_with_tx();
    let hash_str = format!("{:#x}", tx_hash);
    let req = make_request("eth_getTransactionByHash", json!([hash_str]));
    let resp = dispatch_for_test(&state, req).await;
    let result = resp.result.unwrap();
    assert_eq!(result["hash"], json!(hash_str));
    assert_eq!(result["nonce"], json!("0x0"));
    assert!(result["blockHash"].is_string());
    assert_eq!(result["blockNumber"], json!("0x1"));
    assert_eq!(result["transactionIndex"], json!("0x0"));
    assert_eq!(result["input"], json!("0x"));
    assert_eq!(result["type"], json!("0x0"));
}

#[tokio::test]
async fn test_eth_get_transaction_by_hash_not_found() {
    let (state, _tmp, _) = setup_state_with_tx();
    let hash = format!("{:#x}", B256::repeat_byte(0xff));
    let req = make_request("eth_getTransactionByHash", json!([hash]));
    let resp = dispatch_for_test(&state, req).await;
    assert_eq!(resp.result.unwrap(), Value::Null);
}

#[tokio::test]
async fn test_eth_get_transaction_by_block_number_and_index() {
    let (state, _tmp, tx_hash) = setup_state_with_tx();
    let req = make_request("eth_getTransactionByBlockNumberAndIndex", json!(["0x1", "0x0"]));
    let resp = dispatch_for_test(&state, req).await;
    let result = resp.result.unwrap();
    let hash_str = format!("{:#x}", tx_hash);
    assert_eq!(result["hash"], json!(hash_str));
    assert_eq!(result["transactionIndex"], json!("0x0"));
}

#[tokio::test]
async fn test_eth_get_transaction_by_block_hash_and_index() {
    let (state, _tmp, tx_hash) = setup_state_with_tx();
    let block_hash = state.store.head().unwrap().unwrap();
    let block_hash_str = format!("{:#x}", block_hash);
    let req = make_request("eth_getTransactionByBlockHashAndIndex", json!([block_hash_str, "0x0"]));
    let resp = dispatch_for_test(&state, req).await;
    let result = resp.result.unwrap();
    let hash_str = format!("{:#x}", tx_hash);
    assert_eq!(result["hash"], json!(hash_str));
}

#[tokio::test]
async fn test_eth_get_transaction_receipt() {
    let (state, _tmp, tx_hash) = setup_state_with_tx();
    let hash_str = format!("{:#x}", tx_hash);
    let req = make_request("eth_getTransactionReceipt", json!([hash_str]));
    let resp = dispatch_for_test(&state, req).await;
    let result = resp.result.unwrap();
    assert_eq!(result["transactionHash"], json!(hash_str));
    assert_eq!(result["transactionIndex"], json!("0x0"));
    assert_eq!(result["blockNumber"], json!("0x1"));
    assert_eq!(result["status"], json!("0x1"));
    assert_eq!(result["gasUsed"], json!("0x5208"));
    assert_eq!(result["cumulativeGasUsed"], json!("0x5208"));
    assert_eq!(result["type"], json!("0x0"));
    assert!(result["contractAddress"].is_null());
}

#[tokio::test]
async fn test_eth_get_transaction_receipt_not_found() {
    let (state, _tmp, _) = setup_state_with_tx();
    let hash = format!("{:#x}", B256::repeat_byte(0xff));
    let req = make_request("eth_getTransactionReceipt", json!([hash]));
    let resp = dispatch_for_test(&state, req).await;
    assert_eq!(resp.result.unwrap(), Value::Null);
}

// ========== Phase 6: Log filtering ==========

fn setup_state_with_logs() -> (RpcState, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    let store = Arc::new(BlockStore::open(tmp.path()).unwrap());

    let log_addr = "0x1111111111111111111111111111111111111111".parse::<alloy_primitives::Address>().unwrap();
    let topic1 = B256::repeat_byte(0xAA);

    for block_num in 1u64..=3 {
        let header = test_header(block_num);
        let hash = header.hash();
        store.put_header(&header).unwrap();
        store.put_canonical_hash(block_num, hash).unwrap();
        store.put_total_difficulty(hash, U256::from(block_num * 1000)).unwrap();

        store.put_body(hash, &[], &[]).unwrap();

        let receipt = rustock_core::Receipt {
            post_tx_state: vec![0x01],
            cumulative_gas_used: 21_000,
            gas_used: 21_000,
            logs_bloom: alloy_primitives::Bloom::ZERO,
            logs: vec![
                rustock_core::Log {
                    address: log_addr,
                    topics: vec![topic1],
                    data: vec![block_num as u8].into(),
                }
            ],
            status: true,
        };
        store.put_receipts(hash, &[receipt]).unwrap();

        if block_num == 3 {
            store.set_head(hash).unwrap();
        }
    }

    let state = RpcState {
        store,
        peer_store: Arc::new(PeerStore::new()),
        config: Arc::new(ChainConfig::mainnet()),
        tx_submitter: None,
        trie_store: None,
        hardfork_cfg: None,
        filter_store: Arc::new(crate::logs::FilterStore::new()),
        tx_pool: None,
        epoch_store: None,
        miner: None,
        admin_enabled: false,
        gc_burial: 4000,
        prune_keep_depth: 100_000,
        prune_max_batch: 50_000,
    };
    (state, tmp)
}

#[tokio::test]
async fn test_eth_get_logs_by_range() {
    let (state, _tmp) = setup_state_with_logs();
    let req = make_request("eth_getLogs", json!([{
        "fromBlock": "0x1",
        "toBlock": "0x3"
    }]));
    let resp = dispatch_for_test(&state, req).await;
    let logs = resp.result.unwrap();
    let logs = logs.as_array().unwrap();
    assert_eq!(logs.len(), 3);
}

#[tokio::test]
async fn test_eth_get_logs_by_address() {
    let (state, _tmp) = setup_state_with_logs();
    let req = make_request("eth_getLogs", json!([{
        "fromBlock": "0x1",
        "toBlock": "0x3",
        "address": "0x1111111111111111111111111111111111111111"
    }]));
    let resp = dispatch_for_test(&state, req).await;
    let logs = resp.result.unwrap().as_array().unwrap().len();
    assert_eq!(logs, 3);

    // Different address should return nothing
    let req2 = make_request("eth_getLogs", json!([{
        "fromBlock": "0x1",
        "toBlock": "0x3",
        "address": "0x2222222222222222222222222222222222222222"
    }]));
    let resp2 = dispatch_for_test(&state, req2).await;
    assert_eq!(resp2.result.unwrap().as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn test_eth_get_logs_by_topic() {
    let (state, _tmp) = setup_state_with_logs();
    let topic = format!("{:#x}", B256::repeat_byte(0xAA));
    let req = make_request("eth_getLogs", json!([{
        "fromBlock": "0x1",
        "toBlock": "0x3",
        "topics": [topic]
    }]));
    let resp = dispatch_for_test(&state, req).await;
    assert_eq!(resp.result.unwrap().as_array().unwrap().len(), 3);

    // Non-matching topic
    let wrong_topic = format!("{:#x}", B256::repeat_byte(0xBB));
    let req2 = make_request("eth_getLogs", json!([{
        "fromBlock": "0x1",
        "toBlock": "0x3",
        "topics": [wrong_topic]
    }]));
    let resp2 = dispatch_for_test(&state, req2).await;
    assert_eq!(resp2.result.unwrap().as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn test_eth_new_filter_and_get_changes() {
    let (state, _tmp) = setup_state_with_logs();
    // Create a filter
    let req = make_request("eth_newFilter", json!([{
        "fromBlock": "0x1",
        "toBlock": "0x3"
    }]));
    let resp = dispatch_for_test(&state, req).await;
    let filter_id = resp.result.unwrap();
    assert!(filter_id.is_string());

    // Get changes — since we just created it with last_polled = head (3), no new blocks
    let req2 = make_request("eth_getFilterChanges", json!([filter_id]));
    let resp2 = dispatch_for_test(&state, req2).await;
    let changes = resp2.result.unwrap().as_array().unwrap().len();
    assert_eq!(changes, 0);
}

#[tokio::test]
async fn test_eth_new_block_filter() {
    let (state, _tmp) = setup_state_with_logs();
    let req = make_request("eth_newBlockFilter", json!([]));
    let resp = dispatch_for_test(&state, req).await;
    assert!(resp.result.unwrap().is_string());
}

#[tokio::test]
async fn test_eth_uninstall_filter() {
    let (state, _tmp) = setup_state_with_logs();
    let req = make_request("eth_newFilter", json!([{"fromBlock": "0x1"}]));
    let resp = dispatch_for_test(&state, req).await;
    let filter_id = resp.result.unwrap();

    let req2 = make_request("eth_uninstallFilter", json!([filter_id]));
    let resp2 = dispatch_for_test(&state, req2).await;
    assert_eq!(resp2.result.unwrap(), json!(true));

    // Second uninstall should return false
    let req3 = make_request("eth_uninstallFilter", json!([filter_id]));
    let resp3 = dispatch_for_test(&state, req3).await;
    assert_eq!(resp3.result.unwrap(), json!(false));
}

#[tokio::test]
async fn test_eth_new_pending_transaction_filter() {
    let (state, _tmp) = setup_state();
    let req = make_request("eth_newPendingTransactionFilter", json!([]));
    let resp = dispatch_for_test(&state, req).await;
    assert_eq!(resp.result.unwrap(), json!("0x0"));
}

// ========== Phase 6: DTO field name compatibility with rskj ==========

#[tokio::test]
async fn test_transaction_dto_fields_match_rskj() {
    let (state, _tmp, tx_hash) = setup_state_with_tx();
    let hash_str = format!("{:#x}", tx_hash);
    let req = make_request("eth_getTransactionByHash", json!([hash_str]));
    let resp = dispatch_for_test(&state, req).await;
    let result = resp.result.unwrap();

    let required_fields = [
        "hash", "nonce", "blockHash", "blockNumber", "transactionIndex",
        "from", "to", "gas", "gasPrice", "value", "input", "v", "r", "s", "type",
    ];
    for field in &required_fields {
        assert!(result.get(field).is_some(), "Missing TransactionDTO field: {}", field);
    }

    // Verify hex formatting matches rskj conventions
    assert!(result["nonce"].as_str().unwrap().starts_with("0x"));
    assert!(result["gas"].as_str().unwrap().starts_with("0x"));
    assert!(result["gasPrice"].as_str().unwrap().starts_with("0x"));
    assert!(result["value"].as_str().unwrap().starts_with("0x"));
    assert_eq!(result["type"], json!("0x0"));
    // v should be formatted as 0x%02x
    let v_str = result["v"].as_str().unwrap();
    assert!(v_str.starts_with("0x"));
    assert_eq!(v_str, "0x1b"); // v=27 -> 0x1b
}

#[tokio::test]
async fn test_receipt_dto_fields_match_rskj() {
    let (state, _tmp, tx_hash) = setup_state_with_tx();
    let hash_str = format!("{:#x}", tx_hash);
    let req = make_request("eth_getTransactionReceipt", json!([hash_str]));
    let resp = dispatch_for_test(&state, req).await;
    let result = resp.result.unwrap();

    let required_fields = [
        "transactionHash", "transactionIndex", "blockHash", "blockNumber",
        "cumulativeGasUsed", "gasUsed", "contractAddress", "logs",
        "from", "to", "status", "logsBloom", "type",
    ];
    for field in &required_fields {
        assert!(result.get(field).is_some(), "Missing ReceiptDTO field: {}", field);
    }

    assert_eq!(result["type"], json!("0x0"));
    assert_eq!(result["status"], json!("0x1"));
}

#[tokio::test]
async fn test_receipt_dto_failed_status() {
    use alloy_rlp::Encodable;
    use sha3::{Digest, Keccak256};

    let tmp = tempfile::tempdir().unwrap();
    let store = Arc::new(BlockStore::open(tmp.path()).unwrap());

    let tx = rustock_core::Transaction {
        nonce: 0,
        gas_price: U256::from(20_000_000_000u64),
        gas_limit: U256::from(21_000),
        to: alloy_primitives::Bytes::from(vec![0x12; 20]),
        value: U256::from(1_000_000),
        input: alloy_primitives::Bytes::default(),
        v: 27,
        r: U256::from(100),
        s: U256::from(200),
        cached_rlp: None,
    };

    let mut tx_buf = Vec::new();
    tx.encode(&mut tx_buf);
    let tx_hash = B256::from_slice(&Keccak256::digest(&tx_buf));

    let header = test_header(1);
    let block_hash = header.hash();
    store.put_header(&header).unwrap();
    store.put_canonical_hash(1, block_hash).unwrap();
    store.set_head(block_hash).unwrap();
    store.put_total_difficulty(block_hash, U256::from(1000)).unwrap();
    store.put_body(block_hash, &[tx], &[]).unwrap();
    store.put_tx_index(tx_hash, block_hash, 0).unwrap();

    let receipt = rustock_core::Receipt {
        post_tx_state: vec![],
        cumulative_gas_used: 21_000,
        gas_used: 21_000,
        logs_bloom: alloy_primitives::Bloom::ZERO,
        logs: vec![],
        status: false,
    };
    store.put_receipts(block_hash, &[receipt]).unwrap();

    let state = RpcState {
        store,
        peer_store: Arc::new(PeerStore::new()),
        config: Arc::new(ChainConfig::mainnet()),
        tx_submitter: None,
        trie_store: None,
        hardfork_cfg: None,
        filter_store: Arc::new(crate::logs::FilterStore::new()),
        tx_pool: None,
        epoch_store: None,
        miner: None,
        admin_enabled: false,
        gc_burial: 4000,
        prune_keep_depth: 100_000,
        prune_max_batch: 50_000,
    };

    let hash_str = format!("{:#x}", tx_hash);
    let req = make_request("eth_getTransactionReceipt", json!([hash_str]));
    let resp = dispatch_for_test(&state, req).await;
    let result = resp.result.unwrap();
    assert_eq!(result["status"], json!("0x0"));
}

// ========== Phase 6: Log DTO field names ==========

#[tokio::test]
async fn test_log_dto_fields_match_rskj() {
    let (state, _tmp) = setup_state_with_logs();
    let req = make_request("eth_getLogs", json!([{
        "fromBlock": "0x1",
        "toBlock": "0x1"
    }]));
    let resp = dispatch_for_test(&state, req).await;
    let logs = resp.result.unwrap();
    let log = &logs.as_array().unwrap()[0];

    let required_fields = [
        "address", "topics", "data", "blockNumber", "blockHash",
        "transactionHash", "transactionIndex", "logIndex", "removed",
    ];
    for field in &required_fields {
        assert!(log.get(field).is_some(), "Missing LogDTO field: {}", field);
    }

    assert_eq!(log["removed"], json!(false));
    assert!(log["address"].as_str().unwrap().starts_with("0x"));
    assert!(log["blockNumber"].as_str().unwrap().starts_with("0x"));
    assert_eq!(log["logIndex"], json!("0x0"));
}

// ── mnr: merged mining ────────────────────────────────────────────────
//
// These check the wire contract rather than the mining itself: the JSON shapes
// are rskj's and the consumers are existing pool daemons, so a field that
// looks wrong here is a field that breaks a miner.


// --- block-count getters, and the header-only distinction --------------------
//
// rskj covers these in Web3ImplTest (getBlockTransactionCountByHash and
// friends). Both were named in no rustock test. The interesting case is not
// the happy path but the three-way answer: a count, zero for a block whose
// body is absent, and null for a block that is not here at all -- a caller
// cannot tell "no transactions" from "no block" unless those differ.

#[tokio::test]
async fn test_block_transaction_count_by_hash_distinguishes_absent_from_empty() {
    let (state, _tmp) = setup_state();
    let known = test_header(42).hash();

    let resp = dispatch_for_test(
        &state,
        make_request("eth_getBlockTransactionCountByHash", json!([format!("{known:?}")])),
    )
    .await;
    assert_eq!(
        resp.result.unwrap(),
        json!("0x0"),
        "a stored header with no body has zero transactions, not null"
    );

    let unknown = "0x".to_string() + &"11".repeat(32);
    let resp = dispatch_for_test(
        &state,
        make_request("eth_getBlockTransactionCountByHash", json!([unknown])),
    )
    .await;
    assert_eq!(
        resp.result.unwrap(),
        serde_json::Value::Null,
        "a block that is not stored must answer null, not zero"
    );
}

#[tokio::test]
async fn test_block_count_getters_reject_a_malformed_hash() {
    let (state, _tmp) = setup_state();
    for method in [
        "eth_getBlockTransactionCountByHash",
        "eth_getUncleCountByBlockHash",
    ] {
        let resp = dispatch_for_test(&state, make_request(method, json!(["not-a-hash"]))).await;
        assert!(resp.result.is_none(), "{method} must not answer a bad hash");
        assert_eq!(
            resp.error.expect("an error is expected").code,
            -32602,
            "{method} must report INVALID_PARAMS"
        );

        let resp = dispatch_for_test(&state, make_request(method, json!([]))).await;
        assert_eq!(
            resp.error.expect("an error is expected").code,
            -32602,
            "{method} must report INVALID_PARAMS when the hash is missing"
        );
    }
}

/// eth_hashrate is a fixed zero: rustock does not hash, it hands work to
/// merged-mining software that does. Pinned so it is not "improved" into
/// reporting a number the node cannot know.
#[tokio::test]
async fn test_eth_hashrate_is_zero() {
    let (state, _tmp) = setup_state();
    let resp = dispatch_for_test(&state, make_request("eth_hashrate", json!([]))).await;
    assert_eq!(resp.result.unwrap(), json!("0x0"));
}

mod mnr_tests {
    use super::*;
    use crate::mnr::{MiningService, SUBMIT_BLOCK_ERROR};
    use rustock_execution::mining::{ImportResult, MinerWork, SubmitError, SubmittedBlockInfo};
    use std::sync::Mutex;

    /// A miner that answers from a script, so the RPC layer can be checked
    /// without a block processor, a trie and a chain behind it.
    #[derive(Default)]
    struct FakeMiner {
        work: Option<MinerWork>,
        /// The arguments the last submit call arrived with.
        last_call: Mutex<Option<(Vec<u8>, Vec<u8>, String, u32)>>,
        fail_with: Option<String>,
    }

    impl FakeMiner {
        fn ok(&self) -> Result<SubmittedBlockInfo, SubmitError> {
            if let Some(message) = &self.fail_with {
                return Err(SubmitError::Storage(message.clone()));
            }
            Ok(SubmittedBlockInfo {
                block_imported_result: ImportResult::ImportedBest,
                block_hash: B256::repeat_byte(0xAB),
                block_included_height: 0x2a,
            })
        }
    }

    impl MiningService for FakeMiner {
        fn coinbase(&self) -> [u8; 20] {
            [0x5Au8; 20]
        }


        fn get_work(&self) -> Result<MinerWork, SubmitError> {
            self.work
                .clone()
                .ok_or_else(|| SubmitError::Storage("no work".into()))
        }

        fn submit_bitcoin_block(&self, raw: &[u8]) -> Result<SubmittedBlockInfo, SubmitError> {
            *self.last_call.lock().unwrap() = Some((raw.to_vec(), Vec::new(), String::new(), 0));
            self.ok()
        }

        fn submit_bitcoin_block_transactions(
            &self,
            header: &[u8],
            coinbase: &[u8],
            hashes: &str,
        ) -> Result<SubmittedBlockInfo, SubmitError> {
            *self.last_call.lock().unwrap() =
                Some((header.to_vec(), coinbase.to_vec(), hashes.to_string(), 0));
            self.ok()
        }

        fn submit_bitcoin_block_partial_merkle(
            &self,
            header: &[u8],
            coinbase: &[u8],
            hashes: &str,
            count: u32,
        ) -> Result<SubmittedBlockInfo, SubmitError> {
            *self.last_call.lock().unwrap() =
                Some((header.to_vec(), coinbase.to_vec(), hashes.to_string(), count));
            self.ok()
        }
    }

    fn sample_work() -> MinerWork {
        MinerWork {
            block_hash_for_merged_mining: B256::repeat_byte(0x11),
            // A target well under 32 bytes, to prove it is zero-padded rather
            // than sent as a quantity.
            target: U256::from(0x1234u64),
            fees_paid_to_miner: U256::from(123_456_789_u64),
            notify: true,
            parent_block_hash: B256::repeat_byte(0x22),
        }
    }

    fn state_with(miner: FakeMiner) -> (RpcState, tempfile::TempDir, Arc<FakeMiner>) {
        let (mut state, tmp) = setup_state();
        let miner = Arc::new(miner);
        state.miner = Some(miner.clone() as Arc<dyn MiningService>);
        (state, tmp, miner)
    }

    #[tokio::test]
    async fn get_work_returns_rskj_field_shapes() {
        let (state, _tmp, _miner) =
            state_with(FakeMiner { work: Some(sample_work()), ..Default::default() });

        let resp = dispatch_for_test(&state, make_request("mnr_getWork", json!([]))).await;
        let result = resp.result.expect("getWork must answer");

        assert_eq!(
            result["blockHashForMergedMining"],
            json!("0x1111111111111111111111111111111111111111111111111111111111111111")
        );
        // Padded to 32 bytes: this is a threshold a hash is compared against,
        // not a quantity, and trimming it would change its meaning.
        assert_eq!(
            result["target"],
            json!("0x0000000000000000000000000000000000000000000000000000000000001234")
        );
        // Decimal, not hex: rskj sends `String.valueOf(Coin)` here and pools
        // parse it as a number.
        assert_eq!(result["feesPaidToMiner"], json!("123456789"));
        assert_eq!(result["notify"], json!(true));
        assert_eq!(
            result["parentBlockHash"],
            json!("0x2222222222222222222222222222222222222222222222222222222222222222")
        );
    }

    /// `blockImportedResult` is the hex encoding of an ASCII status word --
    /// `HexUtils.toJsonHex(stringToByteArray(importResult.toString()))` in
    /// rskj. Sending the word itself would be more sensible and would break
    /// every client.
    #[tokio::test]
    async fn submit_returns_an_rskj_submitted_block_info() {
        let (state, _tmp, _miner) = state_with(FakeMiner::default());

        let resp = dispatch_for_test(
            &state,
            make_request("mnr_submitBitcoinBlock", json!(["0xdeadbeef"])),
        )
        .await;
        let result = resp.result.expect("submit must answer");

        assert_eq!(
            result["blockImportedResult"],
            json!(format!("0x{}", hex::encode("IMPORTED_BEST")))
        );
        assert_eq!(
            result["blockHash"],
            json!("0xabababababababababababababababababababababababababababababababab")
        );
        assert_eq!(result["blockIncludedHeight"], json!("0x2a"));
    }

    #[tokio::test]
    async fn submit_passes_the_raw_block_through() {
        let (state, _tmp, miner) = state_with(FakeMiner::default());

        dispatch_for_test(
            &state,
            make_request("mnr_submitBitcoinBlock", json!(["0x0102ff"])),
        )
        .await;

        let call = miner.last_call.lock().unwrap().clone().expect("submit reached the miner");
        assert_eq!(call.0, vec![0x01, 0x02, 0xff]);
    }

    /// rskj takes a block hash as the first argument of the two-part submits
    /// and ignores it -- the hash is recomputed from the header. The
    /// positional shape still has to match, or every argument lands one place
    /// out.
    #[tokio::test]
    async fn submit_transactions_reads_arguments_by_rskj_position() {
        let (state, _tmp, miner) = state_with(FakeMiner::default());

        dispatch_for_test(
            &state,
            make_request(
                "mnr_submitBitcoinBlockTransactions",
                json!(["0xdead", "0xaabb", "0xccdd", "hash1 hash2"]),
            ),
        )
        .await;

        let call = miner.last_call.lock().unwrap().clone().expect("submit reached the miner");
        assert_eq!(call.0, vec![0xaa, 0xbb], "header");
        assert_eq!(call.1, vec![0xcc, 0xdd], "coinbase");
        assert_eq!(call.2, "hash1 hash2");
    }

    #[tokio::test]
    async fn submit_partial_merkle_reads_arguments_by_rskj_position() {
        let (state, _tmp, miner) = state_with(FakeMiner::default());

        dispatch_for_test(
            &state,
            make_request(
                "mnr_submitBitcoinBlockPartialMerkle",
                json!(["0xdead", "0xaabb", "0xccdd", "hash1 hash2", "10"]),
            ),
        )
        .await;

        let call = miner.last_call.lock().unwrap().clone().expect("submit reached the miner");
        assert_eq!(call.0, vec![0xaa, 0xbb], "header");
        assert_eq!(call.1, vec![0xcc, 0xdd], "coinbase");
        assert_eq!(call.2, "hash1 hash2");
        // The transaction count arrives as hex without a prefix.
        assert_eq!(call.3, 16);
    }

    #[tokio::test]
    async fn submit_partial_merkle_rejects_an_empty_hash_list() {
        let (state, _tmp, _miner) = state_with(FakeMiner::default());

        let resp = dispatch_for_test(
            &state,
            make_request(
                "mnr_submitBitcoinBlockPartialMerkle",
                json!(["0xdead", "0xaabb", "0xccdd", "", "10"]),
            ),
        )
        .await;

        let error = resp.error.expect("an empty branch cannot be a proof");
        assert_eq!(error.code, SUBMIT_BLOCK_ERROR);
    }

    /// A refused submission carries rskj's application-defined code, so a
    /// miner can tell a rejected block from a malformed request.
    #[tokio::test]
    async fn a_refused_submission_uses_rskj_error_code() {
        let (state, _tmp, _miner) = state_with(FakeMiner {
            fail_with: Some("the chain moved on".into()),
            ..Default::default()
        });

        let resp = dispatch_for_test(
            &state,
            make_request("mnr_submitBitcoinBlock", json!(["0xdeadbeef"])),
        )
        .await;

        let error = resp.error.expect("the submission was refused");
        assert_eq!(error.code, SUBMIT_BLOCK_ERROR);
        assert!(error.message.contains("the chain moved on"), "{}", error.message);
    }

    #[tokio::test]
    async fn malformed_parameters_are_rejected_before_the_miner_is_reached() {
        let (state, _tmp, miner) = state_with(FakeMiner::default());

        for params in [json!([]), json!(["not hex"]), json!([42])] {
            let resp =
                dispatch_for_test(&state, make_request("mnr_submitBitcoinBlock", params.clone()))
                    .await;
            assert_eq!(
                resp.error.expect("bad params must be refused").code,
                INVALID_PARAMS,
                "{params}"
            );
        }
        assert!(miner.last_call.lock().unwrap().is_none());
    }

    /// Without a miner the namespace must look absent rather than refused: a
    /// node that is not mining should be indistinguishable from one built
    /// without the feature.
    #[tokio::test]
    async fn the_namespace_is_absent_when_mining_is_off() {
        let (state, _tmp) = setup_state();

        for method in [
            "mnr_getWork",
            "mnr_submitBitcoinBlock",
            "mnr_submitBitcoinBlockTransactions",
            "mnr_submitBitcoinBlockPartialMerkle",
        ] {
            let resp = dispatch_for_test(&state, make_request(method, json!([]))).await;
            assert_eq!(
                resp.error.unwrap_or_else(|| panic!("{method} must not answer")).code,
                METHOD_NOT_FOUND,
                "{method}"
            );
        }

        let resp = dispatch_for_test(&state, make_request("rpc_modules", json!([]))).await;
        assert!(resp.result.unwrap().get("mnr").is_none());
    }

    #[tokio::test]
    async fn rpc_modules_advertises_mnr_when_mining_is_on() {
        let (state, _tmp, _miner) =
            state_with(FakeMiner { work: Some(sample_work()), ..Default::default() });

        let resp = dispatch_for_test(&state, make_request("rpc_modules", json!([]))).await;
        assert_eq!(resp.result.unwrap()["mnr"], json!("1.0"));
    }

    /// eth_coinbase must report the miner's address once mining is on. With
    /// mining off the zero address is honest; with mining on it is a lie that
    /// mining software acts on, so both halves are pinned here.
    ///
    /// rskj answers this from the miner configuration in EthModule. rustock
    /// had it hard-coded to zero, which was correct only for a node that
    /// never mines -- and stopped being correct when mining landed.
    #[tokio::test]
    async fn test_eth_coinbase_reports_the_miner_address() {
        let (state, _tmp) = setup_state();
        assert!(state.miner.is_none(), "the default node does not mine");
        let resp = dispatch_for_test(&state, make_request("eth_coinbase", json!([]))).await;
        assert_eq!(
            resp.result.unwrap(),
            json!("0x0000000000000000000000000000000000000000"),
            "with no miner the zero address is the honest answer"
        );

        let (mut state, _tmp2) = setup_state();
        state.miner = Some(std::sync::Arc::new(FakeMiner::default()));
        let resp = dispatch_for_test(&state, make_request("eth_coinbase", json!([]))).await;
        assert_eq!(
            resp.result.unwrap(),
            json!("0x5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a"),
            "with a miner the configured coinbase must be reported"
        );
    }

}

// --- administrative methods -------------------------------------------------
//
// rskj gates its equivalents behind the `rsk` module being enabled in config
// and tests that in Web3ImplTest / ModuleDescriptionTest. rustock gates on
// --rpc-admin. These two methods were named in no test at all, which the
// coverage document calls the most uncomfortable gap in the RPC surface:
// rsk_collectTrie DELETES trie epochs.

/// Without --rpc-admin both admin methods must report as unknown, not as
/// forbidden. The distinction is deliberate: a node that never had them and a
/// node that has them switched off should be indistinguishable from outside,
/// so that probing cannot enumerate which nodes are worth attacking.
#[tokio::test]
async fn test_admin_methods_are_unknown_without_the_flag() {
    let (state, _tmp) = setup_state();
    assert!(!state.admin_enabled, "the default must be off");

    for method in ["rsk_collectTrie", "rsk_collectTrieStatus"] {
        let resp = dispatch_for_test(&state, make_request(method, json!([]))).await;
        assert!(resp.result.is_none(), "{method} must not answer when admin is off");
        let err = resp.error.expect("an error is expected");
        assert_eq!(err.code, -32601, "{method} must report METHOD_NOT_FOUND");
        assert_eq!(
            err.message, "Method not found",
            "{method} must not disclose that it exists but is disabled"
        );
    }
}

/// The gate is on the method name, not on the arguments: a request carrying
/// plausible parameters must be refused exactly the same way. Worth pinning
/// because rsk_collectTrie with a block number is the shape that deletes
/// something, and a gate that only covered the no-argument form would look
/// correct in every other test.
#[tokio::test]
async fn test_admin_gate_ignores_the_parameters() {
    let (state, _tmp) = setup_state();
    let resp = dispatch_for_test(
        &state,
        make_request("rsk_collectTrie", json!(["0x2a"])),
    )
    .await;
    assert!(resp.result.is_none(), "parameters must not open the gate");
    assert_eq!(resp.error.unwrap().code, -32601);
}

/// With admin on, the methods must actually dispatch -- otherwise the gate
/// test above would pass against a node where they were removed entirely, and
/// prove nothing. This node has no epoch store, so the call reaches the
/// handler and reports that rather than being refused by the gate.
#[tokio::test]
async fn test_admin_methods_dispatch_when_enabled() {
    let (mut state, _tmp) = setup_state();
    state.admin_enabled = true;

    let resp = dispatch_for_test(&state, make_request("rsk_collectTrieStatus", json!([]))).await;
    let refused_by_gate = resp
        .error
        .as_ref()
        .is_some_and(|e| e.code == -32601 && e.message == "Method not found");
    assert!(
        !refused_by_gate,
        "with admin enabled the method must reach its handler, not the gate"
    );
}

// --- key custody ------------------------------------------------------------
//
// rskj covers the signing surface in Web3ImplTest and the personal module
// tests, where the node DOES hold keys and unlocking is part of the contract.
// rustock holds none, and these tests pin that posture rather than the
// behaviour of a wallet it does not have. The property is worth a test because
// it is the kind that regresses quietly: someone implements eth_sign for a
// local tool, and a node that never had custody suddenly has it.

/// No key-holding method may answer. A node with no wallet must refuse to
/// sign rather than sign with something it happens to have -- the node
/// identity key, a miner key -- which would be a signature the operator never
/// authorised.
#[tokio::test]
async fn test_the_node_holds_no_keys_and_refuses_to_sign() {
    let (state, _tmp) = setup_state();

    for (method, params) in [
        ("eth_sign", json!(["0x0000000000000000000000000000000000000001", "0xdeadbeef"])),
        ("eth_signTransaction", json!([{"from": "0x0000000000000000000000000000000000000001"}])),
        ("eth_sendTransaction", json!([{"from": "0x0000000000000000000000000000000000000001"}])),
    ] {
        let resp = dispatch_for_test(&state, make_request(method, params)).await;
        assert!(resp.result.is_none(), "{method} must not return a result");
        assert_eq!(
            resp.error.expect("an error is expected").code,
            -32601,
            "{method} must be refused"
        );
    }
}

/// eth_accounts must be empty, and consistently so: a caller that trusts a
/// non-empty list would go on to call eth_sign, which cannot work. rskj
/// returns the wallet's accounts here; rustock has no wallet.
#[tokio::test]
async fn test_eth_accounts_is_empty_because_there_is_no_wallet() {
    let (state, _tmp) = setup_state();
    let resp = dispatch_for_test(&state, make_request("eth_accounts", json!([]))).await;
    assert_eq!(
        resp.result.unwrap(),
        json!([]),
        "a node with no wallet must report no accounts"
    );
}

/// The Solidity compiler methods are refused rather than answered with an
/// empty list. rskj removed these upstream; answering "no compilers" would
/// invite a caller to treat compilation as supported-but-unavailable.
#[tokio::test]
async fn test_compiler_methods_are_refused() {
    let (state, _tmp) = setup_state();
    for method in ["eth_getCompilers", "eth_compileSolidity"] {
        let resp = dispatch_for_test(&state, make_request(method, json!([]))).await;
        assert!(resp.result.is_none(), "{method} must not answer");
        assert_eq!(resp.error.unwrap().code, -32601);
    }
}
