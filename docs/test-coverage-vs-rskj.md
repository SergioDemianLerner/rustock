# Test coverage: rustock against rskj

**Generated 2026-09-17** by scanning both repositories mechanically
(`/root/rskj` at the checked-out revision, `/srv/rustock` at `3b68d15`).
Regenerate with the scripts described under *Method*.

## Why this document exists

While implementing the Bridge's federation-address getters it turned out that
`getFederationAddress` was a stub returning raw serialized bytes, and that no
caller could reach it anyway because rustock had rskj's local-call *gate*
(`Bridge.validateLocalCall`) without rskj's local-call *flag*. Both defects
survived because **`eth_call` had no test at all**.

That prompted the obvious question: what else is untested, and therefore
possibly unimplemented or wrong? This document answers it by comparing the two
test suites.

## Headline

| | rskj | rustock | ratio |
|---|---:|---:|---:|
| test classes / files | 642 | 66 | 0.10 |
| test functions | 6,810 | 1,124 | **0.17** |

A ratio below one is expected and not by itself alarming: rskj is a decade old,
carries its own bitcoinj fork, a miner, a solidity compiler bridge and an RPC
surface rustock does not implement. What matters is *where* the ratio is worst,
and whether a low ratio lines up with code that has never been executed by a
test. It does, in several places.

## Method

- rskj: every `*.java` under `src/test/java`, counting `@Test` annotations.
  642 classes contain at least one; 6,810 test methods total.
- rustock: every `*.rs` under `crates/`, counting `#[test]` and
  `#[tokio::test]`. 66 files, 1,124 test functions.
- Each rskj test class is assigned a functional area from its package; each
  rustock test file is assigned the same area from its path.

**Caveat, and it matters when reading the table.** rskj's package tree is finer
than rustock's module tree, so an area showing zero rustock tests is sometimes
a mapping artifact rather than a real hole — rskj's `co/rsk/net/sync` maps to a
package of its own, while rustock's equivalent tests live in
`crates/sync/src/tests.rs` which the mapping files under a different area.
Every zero in the table below was therefore checked by hand against whether the
*feature* exists in rustock at all. The findings section reports only what
survived that check; do not read the raw table as a defect list.

A second caveat: counting test functions rewards granularity, not thoroughness.
One rskj test method with twenty assertions counts once, and so does a rustock
test that checks a single boolean. Treat the ratios as a way to rank where to
look, not as a measurement of quality.

## By area

| Area | rskj classes | rskj tests | rustock tests | Ratio | Where rustock tests it |
|---|---:|---:|---:|---:|---|
| Bridge: core two-way peg | 51 | 1606 | 113 | 0.07 | `execution/bridge/peg.rs`, `execution/bridge/release_tx.rs`, `execution/bridge/mod.rs` |
| RPC | 72 | 720 | 68 | 0.09 | `rpc/tests.rs`, `rpc/helpers.rs` |
| VM / EVM | 40 | 643 | 143 | 0.22 | `execution/executor.rs`, `execution/state.rs`, `execution/database.rs` |
| Core types | 44 | 459 | 27 | 0.06 | `core/types/receipt.rs`, `core/types/header.rs`, `core/types/transaction.rs` |
| Utils | 25 | 338 | 8 | 0.02 | `core/rlp_compat.rs` |
| Bridge: federation | 13 | 315 | 7 | 0.02 | `execution/bridge/federation.rs` |
| Block execution / chain | 25 | 274 | 21 | 0.08 | `execution/processor.rs` |
| Storage/DB | 28 | 253 | 73 | 0.29 | `storage/lib.rs`, `storage/cached_trie_store.rs`, `storage/epoch_store.rs` |
| P2P: node/sync | 24 | 222 | 84 | 0.38 | `sync/tests.rs`, `sync/progress.rs` |
| Trie | 21 | 171 | 122 | 0.71 | `trie/tests.rs`, `trie/node.rs`, `trie/path.rs` |
| Precompiles (non-Bridge) | 21 | 152 | 110 | 0.72 | `execution/precompiles.rs` |
| P2P: messages | 25 | 119 | 58 | 0.49 | `networking/protocol/rsk.rs`, `networking/rlpx/frame.rs`, `networking/peers.rs` |
| Mining / merged mining | 19 | 116 | 0 | **none** | — |
| Peer scoring | 10 | 113 | 0 | **none** | — |
| Block validation | 18 | 88 | 20 | 0.23 | `core/validation/tests.rs`, `core/validation/merged_mining.rs` |
| Bridge: bitcoin primitives | 9 | 88 | 53 | 0.60 | `execution/bridge/pmt.rs`, `execution/bridge/btc_store.rs`, `execution/bridge/btc_chain.rs` |
| Crypto | 10 | 79 | 0 | **none** | — |
| Other: org/ethereum/net | 22 | 77 | 0 | **none** | — |
| Other: co/rsk/test | 8 | 77 | 0 | **none** | — |
| P2P: sync | 13 | 76 | 0 | **none** | — |
| Config | 8 | 67 | 45 | 0.67 | `core/config.rs`, `execution/hardfork.rs`, `execution/bridge/constants.rs` |
| P2P: discovery | 16 | 66 | 5 | 0.08 | `networking/discovery/table.rs`, `networking/discovery/message.rs`, `networking/discovery/tests.rs` |
| Bridge: performance | 27 | 62 | 0 | **none** | — |
| VM / EVM (json suite) | 6 | 62 | 0 | **none** | — |
| Other: co/rsk/jsontestsuite | 6 | 60 | 0 | **none** | — |
| Bridge: whitelist | 3 | 54 | 0 | **none** | — |
| Bridge: utils | 7 | 49 | 40 | 0.82 | `execution/bridge/serialization.rs`, `execution/bridge/events.rs` |
| REMASC | 9 | 46 | 20 | 0.43 | `execution/remasc.rs` |
| P2P: tx handling | 13 | 39 | 38 | 0.97 | `sync/txpool.rs` |
| Log filters | 5 | 38 | 0 | **none** | — |
| CLI / tools | 6 | 37 | 0 | **none** | — |
| Bridge: locking cap | 3 | 36 | 0 | **none** | — |
| Bridge: fee per KB | 3 | 35 | 0 | **none** | — |
| Other: co/rsk | 4 | 35 | 0 | **none** | — |
| Other: org/ethereum/validator | 5 | 31 | 0 | **none** | — |
| Bridge: governance vote | 3 | 27 | 13 | 0.48 | `execution/bridge/governance.rs`, `execution/bridge/vote.rs` |
| Events/listeners | 3 | 21 | 0 | **none** | — |
| Other: co/rsk/metrics | 3 | 15 | 0 | **none** | — |
| Chain bootstrap | 2 | 12 | 0 | **none** | — |
| Other: co/rsk/jsonrpc | 6 | 9 | 0 | **none** | — |
| P2P: eth wire | 1 | 8 | 0 | **none** | — |
| Other: org/ethereum/solidity | 2 | 7 | 0 | **none** | — |
| Other: co/rsk/datasource | 1 | 4 | 0 | **none** | — |
| Other: co/rsk/lll | 1 | 2 | 0 | **none** | — |
| Other: org/ethereum/facade | 1 | 2 | 0 | **none** | — |
| Bridge: storage | 0 | 0 | 28 | — | `execution/bridge/storage.rs` |
| Peg-out alerting (rustock only) | 0 | 0 | 28 | — | `pegout-alerts/config.rs`, `pegout-alerts/watch.rs`, `pegout-alerts/alert.rs` |

## Findings

Ranked by how likely a defect is to reach production unnoticed.

### 1. `eth_call` and `eth_estimateGas` had no test — confirmed defect

13 of the 49 JSON-RPC methods the server dispatches are named in no test:

```
eth_call            eth_estimateGas     eth_sign          eth_signTransaction
eth_getFilterLogs   eth_coinbase        eth_hashrate      eth_getCompilers
eth_compileSolidity eth_getBlockTransactionCountByHash    rsk_collectTrie
eth_getUncleCountByBlockHash                              rsk_collectTrieStatus
```

rskj tests the same surface with 720 methods across 72 classes, of which
`Web3ImplTest` alone carries 146 and `EthModuleTest` 29, plus
`EthModuleGasEstimationDSLTest` (14) devoted entirely to gas estimation.

This is not hypothetical: it is where the two defects fixed on 2026-09-17 were
hiding. `rsk_collectTrie` being untested is the most uncomfortable of the
remainder — it is admin-gated and it *deletes trie epochs*.

### 2. No Ethereum/RSK JSON state-test suite — the largest single gap

rskj runs the standard JSON test vectors through
`org/ethereum/jsontestsuite` and `co/rsk/jsontestsuite` — 122 test methods
across 12 classes, each driving many vectors from committed JSON files.

rustock has **no equivalent**: no fixture runner, no vendored vectors. Its EVM
confidence comes from replaying mainnet blocks instead. That is strong evidence
for paths mainnet exercises and no evidence at all for paths it does not —
precisely the opcodes and edge cases the vectors exist to cover.

### 3. Peer scoring exists and is never tested

`crates/sync/src/service.rs` and `crates/networking/src/outbound.rs` contain
peer scoring and banning logic. No test mentions it. rskj devotes 113 test
methods across 10 classes (`co/rsk/scoring`) to this, because the failure mode
is a node that banishes honest peers or fails to banish hostile ones — and
neither shows up as a crash.

### 4. Bridge: 1,606 rskj tests against 113 in rustock

The worst ratio of any consensus-critical area (0.07). rustock's Bridge is
mostly validated by whole-chain replay, which is genuinely strong evidence: the
2026-09-16 run reproduced 9,230,008 blocks and 7,639,009 state roots. But
replay only covers what mainnet actually did. Both Bridge consensus bugs found
this month — the DER signature check and the `registerBtcTransaction`
throw/swallow split — were found by replay, not by unit tests, which means they
were live for as long as it took to get to that block.

Untested-by-unit-test Bridge features, all of which exist in the code:
`getFederationAddress` / `getRetiringFederationAddress` (until today), SVP and
the RSKIP419 proposed federation (9 mentions), locking cap (12), fee-per-KB (14).

### 5. Core types and utils: 0.06 and 0.02

459 rskj tests for core types against 27; 338 for utils against 8. rskj tests
RLP, hex, byte-array and numeric helpers exhaustively because every one of them
sits under consensus encoding. rustock leans on `alloy` for much of this, which
legitimately removes some of the need — but not for `rlp_compat.rs`, which
exists *because* rustock must reproduce rskj's non-standard encodings, and
carries 8 tests.

## Differences that are scope, not gaps

These rskj areas have no rustock counterpart because rustock does not implement
the feature. They are not defects and should not be counted as missing tests:

| rskj area | tests | why it does not apply |
|---|---:|---|
| Mining / block production | 116 | rustock is a full node; it has no miner and no `--mine` flag. Merged-mining *validation* is implemented and well covered (234 test mentions). |
| Solidity compiler, LLL | 9 | `eth_compileSolidity` is a stub; rskj shells out to a compiler. |
| CLI / tools | 37 | different tool set. |
| bitcoinj fork | — | rustock uses the `bitcoin` crate rather than maintaining a fork. |
| Bridge performance suite | 62 | rskj's JMH-style timing harness, not correctness. |

## What I would do first

1. **Test `eth_call` and `eth_estimateGas`.** One test today pins local-call
   behaviour; there is nothing for the ordinary paths — a contract call
   returning data, reverting, running out of gas, or being made against a
   historical block. This is the cheapest gap to close and the one that has
   already cost two defects.
2. **Import the JSON state-test vectors.** Largest one-off increase in EVM
   confidence available, and the vectors are maintained upstream.
3. **Test peer scoring**, whose failures are silent and whose surface is small.
4. **Unit-test the Bridge getters** now that they return real values, so a
   future refactor cannot quietly re-stub one.
5. **Test `rsk_collectTrie`** before anything else admin-gated grows, given
   what it does.

## Appendix: every rskj test class

642 classes, grouped by area, with the rustock files covering that area. This
is the raw material for the table above; a class with no rustock counterpart
listed is a candidate for review, subject to the scope caveats.

### Bridge: core two-way peg

rskj: **1606 tests** in 51 classes — rustock: **113 tests** in `crates/execution/src/bridge/peg.rs`, `crates/execution/src/bridge/release_tx.rs`, `crates/execution/src/bridge/mod.rs`, `crates/execution/src/bridge/pegin_instructions.rs`

| rskj test class | tests | package |
|---|---:|---|
| `BridgeSupportTest` | 177 | `co/rsk/peg` |
| `BridgeStorageProviderTest` | 136 | `co/rsk/peg` |
| `BridgeIT` | 135 | `co/rsk/peg` |
| `ReleaseTransactionBuilderTest` | 101 | `co/rsk/peg` |
| `BridgeUtilsTest` | 88 | `co/rsk/peg` |
| `BridgeTest` | 81 | `co/rsk/peg` |
| `BridgeSupportIT` | 76 | `co/rsk/peg` |
| `BridgeSerializationUtilsTest` | 70 | `co/rsk/peg` |
| `UnionBridgeStorageProviderImplTest` | 61 | `co/rsk/peg/union` |
| `BridgeSupportFlyoverTest` | 58 | `co/rsk/peg` |
| `BridgeSupportReleaseBtcTest` | 50 | `co/rsk/peg` |
| `PegUtilsLegacyTest` | 49 | `co/rsk/peg` |
| `UnionBridgeSupportImplTest` | 45 | `co/rsk/peg/union` |
| `BridgeSupportProcessFundsMigrationTest` | 42 | `co/rsk/peg` |
| `BridgeSupportSvpTest` | 39 | `co/rsk/peg` |
| `BridgeSupportGetEstimatedFeesTest` | 36 | `co/rsk/peg` |
| `UnionBridgeIT` | 32 | `co/rsk/peg/union` |
| `BridgeSupportAddSignatureTest` | 26 | `co/rsk/peg` |
| `BridgeSupportRegisterBtcTransactionTest` | 26 | `co/rsk/peg` |
| `BridgeUtilsLegacyTest` | 24 | `co/rsk/peg` |
| `PegUtilsTest` | 23 | `co/rsk/peg` |
| `PegUtilsAllUTXOsToFedAreAboveMinimumPeginValueTest` | 22 | `co/rsk/peg` |
| `BridgeCostsTest` | 16 | `co/rsk/peg` |
| `RepositoryBtcBlockStoreWithCacheTest` | 16 | `co/rsk/peg` |
| `PeginInstructionsProviderTest` | 15 | `co/rsk/peg/pegininstructions` |
| `ReleaseRequestQueueTest` | 14 | `co/rsk/peg` |
| `PegoutsWaitingForConfirmationsTest` | 11 | `co/rsk/peg` |
| `PegUtilsGetTransactionTypeTest` | 10 | `co/rsk/peg` |
| `ErpRedeemScriptBuilderUtilsTest` | 10 | `co/rsk/peg` |
| `PeginInformationTest` | 9 | `co/rsk/peg` |
| `P2PkhBtcLockSenderTest` | 9 | `co/rsk/peg/btcLockSender` |
| `P2shP2wpkhBtcLockSenderTest` | 9 | `co/rsk/peg/btcLockSender` |
| `PeginInstructionsVersion1Test` | 8 | `co/rsk/peg/pegininstructions` |
| `P2shP2wshBtcLockSenderTest` | 8 | `co/rsk/peg/btcLockSender` |
| `PegUtilsLegacyGetTransactionTypeTest` | 7 | `co/rsk/peg` |
| `P2shMultisigBtcLockSenderTest` | 7 | `co/rsk/peg/btcLockSender` |
| `FlyoverCompatibleBtcWalletWithStorageTest` | 6 | `co/rsk/peg` |
| `PegUtilsEvaluatePeginTest` | 6 | `co/rsk/peg` |
| `BridgeSupportRSKIP220NewMethodsTest` | 6 | `co/rsk/peg` |
| `UnionResponseCodeTest` | 6 | `co/rsk/peg/union` |
| `BridgeRSKIP220NewMethodsTest` | 5 | `co/rsk/peg` |
| `RskForksBridgeTest` | 5 | `co/rsk/peg` |
| `BridgeStorageProviderPegoutTxIndexTests` | 4 | `co/rsk/peg` |
| `PeginInstructionsBaseTest` | 4 | `co/rsk/peg/pegininstructions` |
| `BtcLockSenderProviderTest` | 4 | `co/rsk/peg/btcLockSender` |
| `StateForFederatorTest` | 3 | `co/rsk/peg` |
| `FlyoverCompatibleBtcWallextWithSingleScriptTest` | 3 | `co/rsk/peg` |
| `PocSighashTest` | 3 | `co/rsk/peg` |
| `NonStandardErpRedeemScriptBuilderFactoryTest` | 2 | `co/rsk/peg` |
| `StateForProposedFederatorTest` | 2 | `co/rsk/peg` |
| `BridgeStateTest` | 1 | `co/rsk/peg` |

### RPC

rskj: **720 tests** in 72 classes — rustock: **68 tests** in `crates/rpc/src/tests.rs`, `crates/rpc/src/helpers.rs`

| rskj test class | tests | package |
|---|---:|---|
| `Web3ImplTest` | 146 | `org/ethereum/rpc` |
| `Web3ImplLogsTest` | 48 | `org/ethereum/rpc` |
| `EthModuleTest` | 29 | `co/rsk/rpc/modules/eth` |
| `TraceModuleImplTest` | 26 | `co/rsk/rpc/modules/trace` |
| `Web3HttpServerTest` | 22 | `co/rsk/rpc/netty` |
| `Web3ImplScoringTest` | 19 | `org/ethereum/rpc` |
| `Web3ImplUnitTest` | 19 | `org/ethereum/rpc` |
| `Web3InformationRetrieverTest` | 18 | `co/rsk/rpc` |
| `DebugModuleImplTest` | 15 | `co/rsk/rpc/modules/debug` |
| `TxPoolModuleImplTest` | 15 | `co/rsk/rpc/modules/txpool` |
| `CallArgumentsParamTest` | 15 | `org/ethereum/rpc/parameters` |
| `ExecutionFoundBlockRetrieverTest` | 14 | `co/rsk/rpc` |
| `EthModuleGasEstimationDSLTest` | 14 | `co/rsk/rpc/modules/eth` |
| `OriginValidatorTest` | 13 | `co/rsk/rpc` |
| `TraceOptionsTest` | 13 | `co/rsk/rpc/modules/debug` |
| `RskErrorResolverTest` | 13 | `org/ethereum/rpc/exception` |
| `DebugTracerParamTest` | 11 | `org/ethereum/rpc/parameters` |
| `CallArgumentsToByteArrayTest` | 11 | `org/ethereum/rpc/converters` |
| `EthSubscriptionNotificationEmitterTest` | 10 | `co/rsk/rpc` |
| `EthSubscribeRequestTest` | 10 | `co/rsk/rpc/modules/eth/subscribe` |
| `DefaultStateOverrideApplierTest` | 9 | `co/rsk/rpc/modules/eth` |
| `JsonRpcCustomServerTest` | 9 | `co/rsk/rpc/netty` |
| `JsonRPCParamValidationTest` | 9 | `co/rsk/rpc/netty` |
| `TopicTest` | 9 | `org/ethereum/rpc` |
| `JsonRpcDocCoverageTest` | 8 | `co/rsk/rpc` |
| `ModuleDescriptionTest` | 8 | `co/rsk/rpc` |
| `SyncNotificationEmitterTest` | 8 | `co/rsk/rpc/modules/eth/subscribe` |
| `LogsNotificationTest` | 8 | `co/rsk/rpc/modules/eth/subscribe` |
| `LogFilterTest` | 8 | `org/ethereum/rpc` |
| `FilterRequestParamTest` | 8 | `org/ethereum/rpc/parameters` |
| `BlockRefParamTest` | 8 | `org/ethereum/rpc/parameters` |
| `BlockResultDTOTest` | 8 | `org/ethereum/rpc/dto` |
| `Web3WebSocketServerTest` | 7 | `co/rsk/rpc/netty` |
| `TransactionResultDTOTest` | 7 | `org/ethereum/rpc/dto` |
| `AccountOverrideTest` | 6 | `co/rsk/rpc/modules/eth/subscribe` |
| `PendingTransactionsNotificationEmitterTest` | 6 | `co/rsk/rpc/modules/eth/subscribe` |
| `LogsNotificationEmitterTest` | 6 | `co/rsk/rpc/modules/eth/subscribe` |
| `AddressesTopicsFilterTest` | 6 | `org/ethereum/rpc` |
| `Web3ImplSnapshotTest` | 6 | `org/ethereum/rpc` |
| `CorsConfigurationTest` | 5 | `co/rsk/rpc` |
| `Web3RskImplTest` | 5 | `co/rsk/rpc` |
| `EthModuleDSLTest` | 5 | `co/rsk/rpc/modules/eth` |
| `Web3MethodResolutionTest` | 5 | `co/rsk/rpc/netty` |
| `TransactionReceiptDTOTest` | 5 | `org/ethereum/rpc/dto` |
| `EthSubscriptionNotificationTest` | 4 | `co/rsk/rpc/modules/eth/subscribe` |
| `RskWebSocketJsonRpcHandlerTest` | 4 | `co/rsk/rpc/netty` |
| `FilterTest` | 4 | `org/ethereum/rpc` |
| `BlockIdentifierParamTest` | 4 | `org/ethereum/rpc/parameters` |
| `TraceAddressTest` | 3 | `co/rsk/rpc/modules/trace` |
| `TraceTransformerTest` | 3 | `co/rsk/rpc/modules/trace` |
| `CallTracerTest` | 3 | `co/rsk/rpc/modules/debug/trace/call` |
| `BlockHeaderNotificationEmitterTest` | 3 | `co/rsk/rpc/modules/eth/subscribe` |
| `PersonalModuleWalletEnabledTest` | 3 | `co/rsk/rpc/modules/personal` |
| `FilterManagerTest` | 3 | `org/ethereum/rpc` |
| `NewBlockFilterTest` | 3 | `org/ethereum/rpc` |
| `PendingTransactionFilterTest` | 3 | `org/ethereum/rpc` |
| `HexNumberParamTest` | 3 | `org/ethereum/rpc/parameters` |
| `Web3EthModuleTest` | 2 | `co/rsk/rpc` |
| `EthUnsubscribeRequestTest` | 2 | `co/rsk/rpc/modules/eth/subscribe` |
| `PersonalModuleTest` | 2 | `co/rsk/rpc/modules/personal` |
| `Web3HttpStatusCodeProviderTest` | 2 | `co/rsk/rpc/netty` |
| `RskWebSocketJsonParameterValidatorTest` | 2 | `co/rsk/rpc/netty` |
| `JsonResponseSizeLimiterTest` | 2 | `co/rsk/rpc/json` |
| `HashParamTest` | 2 | `org/ethereum/rpc/parameters` |
| `HexDurationParamTest` | 2 | `org/ethereum/rpc/parameters` |
| `HexDataParamTest` | 2 | `org/ethereum/rpc/parameters` |
| `HexKeyParamTest` | 2 | `org/ethereum/rpc/parameters` |
| `HexIndexParamTest` | 2 | `org/ethereum/rpc/parameters` |
| `HexAddressParamTest` | 2 | `org/ethereum/rpc/parameters` |
| `JsonRpcMethodFilterTest` | 1 | `co/rsk/rpc` |
| `Web3ImplRpcTest` | 1 | `co/rsk/rpc` |
| `HttpUtilsTest` | 1 | `org/ethereum/rpc` |

### VM / EVM

rskj: **643 tests** in 40 classes — rustock: **143 tests** in `crates/execution/src/executor.rs`, `crates/execution/src/state.rs`, `crates/execution/src/database.rs`, `crates/execution/src/raw_storage.rs`

| rskj test class | tests | package |
|---|---:|---|
| `VMTest` | 220 | `org/ethereum/vm` |
| `VMExecutionTest` | 69 | `co/rsk/vm` |
| `GasCostTest` | 32 | `org/ethereum/vm` |
| `VMCustomTest` | 31 | `org/ethereum/vm` |
| `ProgramMemoryTest` | 30 | `org/ethereum/vm` |
| `TransientStorageDslTest` | 28 | `co/rsk/vm/opcode` |
| `DataWordTest` | 27 | `org/ethereum/vm` |
| `MemoryTest` | 24 | `org/ethereum/vm` |
| `PrecompiledContractTest` | 17 | `org/ethereum/vm` |
| `ProgramTest` | 16 | `org/ethereum/vm/program` |
| `Create2Test` | 15 | `co/rsk/vm` |
| `ProgramTest` | 13 | `org/ethereum/vm` |
| `VmDslTest` | 11 | `co/rsk/vm` |
| `PrecompiledContractsCallErrorHandlingTests` | 11 | `co/rsk/vm/precompiles` |
| `VMComplexTest` | 10 | `org/ethereum/vm` |
| `BitSetTest` | 9 | `co/rsk/vm` |
| `BytecodeCompilerTest` | 8 | `co/rsk/vm` |
| `OverrideablePrecompiledContractsTest` | 8 | `org/ethereum/vm` |
| `ExtCodeHashTest` | 6 | `co/rsk/vm` |
| `PrecompiledContractTest` | 6 | `co/rsk/vm/precompiles` |
| `ContractCodePrefixDslTest` | 5 | `co/rsk/vm` |
| `TransientStorageTest` | 5 | `co/rsk/vm/opcode` |
| `ExtCodeHashDslTest` | 4 | `co/rsk/vm` |
| `DetailedProgramTraceProcessorTest` | 4 | `org/ethereum/vm/trace` |
| `VMPerformanceTest` | 3 | `co/rsk/vm` |
| `MCopyDslTest` | 3 | `co/rsk/vm/opcode` |
| `ProgramResultTest` | 3 | `org/ethereum/vm/program` |
| `StackTest` | 3 | `org/ethereum/vm/program` |
| `ProgramBeforeRSKIP197Test` | 3 | `org/ethereum/vm/program` |
| `BlockchainVMTest` | 2 | `co/rsk/vm` |
| `VMSpecificOpcodesPerformanceTest` | 2 | `co/rsk/vm` |
| `BasefeeDslTest` | 2 | `co/rsk/vm/opcode` |
| `Blake2fNullDataTest` | 2 | `co/rsk/vm/precompiles/blake2b` |
| `VMUtilsTest` | 2 | `org/ethereum/vm` |
| `RevertOpCodeTest` | 2 | `org/ethereum/vm/opcodes` |
| `ProgramInvokeImplTest` | 2 | `org/ethereum/vm/program` |
| `NestedContractsTest` | 2 | `org/ethereum/vm/program` |
| `RSKIP544Test` | 1 | `co/rsk/vm` |
| `PrecompiledContractAddressTests` | 1 | `co/rsk/vm/precompiles` |
| `Blake2bEipExampleTest` | 1 | `co/rsk/vm/precompiles/blake2b` |

### Core types

rskj: **459 tests** in 44 classes — rustock: **27 tests** in `crates/core/src/types/receipt.rs`, `crates/core/src/types/header.rs`, `crates/core/src/types/transaction.rs`, `crates/core/src/types/block.rs`

| rskj test class | tests | package |
|---|---:|---|
| `BlockFactoryTest` | 46 | `co/rsk/core` |
| `ParallelExecutionStateTest` | 46 | `co/rsk/core/parallel` |
| `BlockHeaderBuilderTest` | 43 | `org/ethereum/core` |
| `BlockHeaderTest` | 40 | `co/rsk/core` |
| `BlockHeaderExtensionV2Test` | 22 | `org/ethereum/core` |
| `BlockTest` | 20 | `co/rsk/core` |
| `BlockHeaderV2Test` | 20 | `co/rsk/core` |
| `TransactionTest` | 17 | `co/rsk/core` |
| `BytesTest` | 17 | `co/rsk/core/types/bytes` |
| `TransactionTest` | 16 | `org/ethereum/core` |
| `WalletTest` | 13 | `co/rsk/core` |
| `RskAddressTest` | 13 | `co/rsk/core` |
| `BytesSliceTest` | 12 | `co/rsk/core/types/bytes` |
| `CallTransactionTest` | 10 | `co/rsk/core` |
| `SnapshotManagerTest` | 9 | `co/rsk/core` |
| `BlockDifficultyTest` | 9 | `co/rsk/core` |
| `TransactionIsRemascTest` | 9 | `co/rsk/core` |
| `ABITest` | 9 | `org/ethereum/core` |
| `TransactionSetTest` | 9 | `org/ethereum/core` |
| `Uint8Test` | 8 | `co/rsk/core/types/ints` |
| `Uint24Test` | 7 | `co/rsk/core/types/ints` |
| `BlockHeaderExtensionV1Test` | 6 | `co/rsk/core` |
| `TransactionExecutorInitCodeSizeDslTest` | 6 | `org/ethereum/core` |
| `BlockHeaderV1Test` | 5 | `co/rsk/core` |
| `ReversibleTransactionExecutorTest` | 4 | `co/rsk/core` |
| `NetworkStateExporterTest` | 4 | `co/rsk/core` |
| `AccountStateTest` | 4 | `org/ethereum/core` |
| `ContractCreatingDslRollbackTest` | 4 | `org/ethereum/core` |
| `GenesisLoaderImplTest` | 4 | `org/ethereum/core/genesis` |
| `BlockHeaderV0Test` | 3 | `co/rsk/core` |
| `NestedCallsStackDepthTest` | 3 | `org/ethereum/core` |
| `LogInfoTest` | 3 | `org/ethereum/core` |
| `BlockHeaderExtensionTest` | 2 | `co/rsk/core` |
| `CoinTest` | 2 | `co/rsk/core` |
| `TransactionReceiptTest` | 2 | `org/ethereum/core` |
| `StateTest` | 2 | `org/ethereum/core` |
| `BlockchainLoaderTest` | 2 | `org/ethereum/core/genesis` |
| `GenesisHashesTest` | 2 | `org/ethereum/core/genesis` |
| `ImmutableTransactionTest` | 1 | `co/rsk/core` |
| `BlockEncodingTest` | 1 | `co/rsk/core` |
| `CallContractTest` | 1 | `co/rsk/core` |
| `BloomTest` | 1 | `org/ethereum/core` |
| `BlockTest` | 1 | `org/ethereum/core` |
| `GenesisJsonTest` | 1 | `org/ethereum/core/genesis` |

### Utils

rskj: **338 tests** in 25 classes — rustock: **8 tests** in `crates/core/src/rlp_compat.rs`

| rskj test class | tests | package |
|---|---:|---|
| `RLPTest` | 65 | `co/rsk/util` |
| `RLPTest` | 51 | `org/ethereum/util` |
| `HexUtilsTest` | 41 | `co/rsk/util` |
| `ByteUtilTest` | 37 | `org/ethereum/util` |
| `ListArrayUtilTest` | 22 | `co/rsk/util` |
| `HashUtilTest` | 21 | `org/ethereum/util` |
| `UtilsTest` | 17 | `org/ethereum/util` |
| `MapSnapshotTest` | 13 | `org/ethereum/util` |
| `JacksonParserUtilTest` | 12 | `co/rsk/util` |
| `CompactEncoderTest` | 11 | `org/ethereum/util` |
| `ValueTest` | 11 | `org/ethereum/util` |
| `IpUtilsTest` | 9 | `co/rsk/util` |
| `PreflightChecksUtilsTest` | 5 | `co/rsk/util` |
| `RskCustomCacheTest` | 4 | `co/rsk/util` |
| `TraceUtilsTest` | 4 | `co/rsk/util` |
| `StringUtilsTest` | 3 | `co/rsk/util` |
| `SystemUtilsTest` | 3 | `co/rsk/util` |
| `BIUtilTest` | 2 | `org/ethereum/util` |
| `AssemblerTest` | 1 | `co/rsk/util` |
| `CacheElementTest` | 1 | `co/rsk/util` |
| `FormatUtilsTest` | 1 | `co/rsk/util` |
| `MaxSizeHashMapTest` | 1 | `co/rsk/util` |
| `SimpleFileWriterTest` | 1 | `co/rsk/util` |
| `TransactionArgumentsUtilTest` | 1 | `org/ethereum/util` |
| `RLPDump` | 1 | `org/ethereum/util` |

### Bridge: federation

rskj: **315 tests** in 13 classes — rustock: **7 tests** in `crates/execution/src/bridge/federation.rs`

| rskj test class | tests | package |
|---|---:|---|
| `FederationSupportImplTest` | 92 | `co/rsk/peg/federation` |
| `NonStandardErpFederationsTest` | 46 | `co/rsk/peg/federation` |
| `FederationStorageProviderImplTests` | 39 | `co/rsk/peg/federation` |
| `PendingFederationTest` | 24 | `co/rsk/peg/federation` |
| `P2shErpFederationTest` | 23 | `co/rsk/peg/federation` |
| `StandardMultisigFederationTest` | 20 | `co/rsk/peg/federation` |
| `VoteFederationChangeTest` | 20 | `co/rsk/peg/federation` |
| `P2shP2wshErpFederationTest` | 19 | `co/rsk/peg/federation` |
| `FederationMemberTest` | 10 | `co/rsk/peg/federation` |
| `FederationFactoryTest` | 7 | `co/rsk/peg/federation` |
| `FederationContextTest` | 7 | `co/rsk/peg/federation` |
| `FederationRegTestConstantsTest` | 7 | `co/rsk/peg/federation/constants` |
| `FederationChangeIT` | 1 | `co/rsk/peg/federation` |

### Block execution / chain

rskj: **274 tests** in 25 classes — rustock: **21 tests** in `crates/execution/src/processor.rs`

| rskj test class | tests | package |
|---|---:|---|
| `TransactionPoolImplTest` | 46 | `co/rsk/core/bc` |
| `ParallelizeTransactionHandlerTest` | 44 | `co/rsk/core/bc` |
| `BlockChainImplTest` | 32 | `co/rsk/core/bc` |
| `ReadWrittenKeysTrackerTest` | 28 | `co/rsk/core/bc` |
| `BlockValidatorTest` | 24 | `co/rsk/core/bc` |
| `MiningMainchainViewImplTest` | 14 | `co/rsk/core/bc` |
| `PrecompiledContractHasBeenCalledTest` | 12 | `co/rsk/core/bc/transactionexecutor` |
| `TransactionExecutorTest` | 9 | `co/rsk/core/bc/transactionexecutor` |
| `BlockUtilsTest` | 7 | `co/rsk/core/bc` |
| `FamilyUtilsTest` | 6 | `co/rsk/core/bc` |
| `ConsensusValidationMainchainViewImplTest` | 6 | `co/rsk/core/bc` |
| `PendingStateSortTest` | 6 | `co/rsk/core/bc` |
| `ReadWrittenKeysTrackerConcurrencyTest` | 4 | `co/rsk/core/bc` |
| `BlockRelayValidatorTest` | 4 | `co/rsk/core/bc` |
| `BlockResultTest` | 4 | `co/rsk/core/bc` |
| `BlockExecutorInvalidTxTest` | 4 | `co/rsk/core/bc` |
| `BlockchainBranchComparatorTest` | 4 | `co/rsk/core/bc` |
| `BlockChainImplInvalidTest` | 3 | `co/rsk/core/bc` |
| `BlockHeaderValidatorTest` | 3 | `co/rsk/core/bc` |
| `BlockHashesHelperTest` | 3 | `co/rsk/core/bc` |
| `BlockChainFlusherTest` | 3 | `co/rsk/core/bc` |
| `GarbageCollectorTest` | 2 | `co/rsk/core/bc` |
| `SelectionRuleTest` | 2 | `co/rsk/core/bc` |
| `InvalidTxMiningEvictionTest` | 2 | `co/rsk/core/bc` |
| `BlockExecutorTest` | 2 | `co/rsk/core/bc` |

### Storage/DB

rskj: **253 tests** in 28 classes — rustock: **73 tests** in `crates/storage/src/lib.rs`, `crates/storage/src/cached_trie_store.rs`, `crates/storage/src/epoch_store.rs`, `crates/storage/src/trie_inspect.rs`

| rskj test class | tests | package |
|---|---:|---|
| `RepositoryImplTest` | 29 | `co/rsk/db` |
| `RepositoryImplOriginalTest` | 26 | `co/rsk/db` |
| `BootstrapImporterV2Test` | 22 | `co/rsk/db/importer` |
| `DataSourceWithCacheTest` | 20 | `org/ethereum/datasource` |
| `MapDBBlocksIndexTest` | 19 | `co/rsk/db` |
| `RepositoryTest` | 16 | `co/rsk/db` |
| `RepositoryTrackingTest` | 14 | `co/rsk/db` |
| `MutableTrieCacheTest` | 13 | `co/rsk/db` |
| `NotParameterizedKeyValueDataSourceTest` | 13 | `org/ethereum/datasource` |
| `BootstrapFileHandlerTest` | 10 | `co/rsk/db/importer/provider` |
| `IndexedBlockStoreTest` | 10 | `org/ethereum/db` |
| `LevelDbDataSourceTest` | 8 | `org/ethereum/datasource` |
| `RocksDbDataSourceTest` | 7 | `org/ethereum/datasource` |
| `BootstrapIndexCandidateSelectorTest` | 6 | `co/rsk/db/importer/provider/index` |
| `StateRootsStoreImplTests` | 5 | `co/rsk/db` |
| `CacheSnapshotHandlerTest` | 5 | `org/ethereum/datasource` |
| `RepositoryUpdateTest` | 4 | `co/rsk/db` |
| `RepositoryLocatorTest` | 3 | `co/rsk/db` |
| `BootstrapDataVerifierTest` | 3 | `co/rsk/db/importer/provider` |
| `BootstrapDataProviderTest` | 3 | `co/rsk/db/importer/provider` |
| `TrieKeyMapperTest` | 3 | `org/ethereum/db` |
| `ByteArrayWrapperTest` | 3 | `org/ethereum/db` |
| `ReceiptStoreImplFallbackTest` | 3 | `org/ethereum/db` |
| `MapDbBlockIndexIntTest` | 2 | `co/rsk/db` |
| `BootstrapURLProviderTest` | 2 | `co/rsk/db/importer` |
| `BootstrapIndexRetrieverTest` | 2 | `co/rsk/db/importer/provider/index` |
| `RepositoryMigrationTest` | 1 | `co/rsk/db` |
| `BootstrapImporterTest` | 1 | `co/rsk/db/importer` |

### P2P: node/sync

rskj: **222 tests** in 24 classes — rustock: **84 tests** in `crates/sync/src/tests.rs`, `crates/sync/src/progress.rs`

| rskj test class | tests | package |
|---|---:|---|
| `NodeBlockProcessorTest` | 40 | `co/rsk/net` |
| `AsyncNodeBlockProcessorTest` | 34 | `co/rsk/net` |
| `NodeMessageHandlerTest` | 34 | `co/rsk/net` |
| `SyncProcessorTest` | 26 | `co/rsk/net` |
| `SnapshotProcessorTest` | 14 | `co/rsk/net` |
| `NetBlockStoreTest` | 12 | `co/rsk/net` |
| `TwoAsyncNodeUsingSyncProcessorTest` | 7 | `co/rsk/net` |
| `BlockCacheTest` | 6 | `co/rsk/net` |
| `SimpleHttpClientIntTest` | 5 | `co/rsk/net/http` |
| `BlockNodeInformationTest` | 4 | `co/rsk/net` |
| `NodeBlockProcessorUnclesTest` | 4 | `co/rsk/net` |
| `AsyncNodeBlockProcessorUnclesTest` | 4 | `co/rsk/net` |
| `TransactionGatewayTest` | 4 | `co/rsk/net` |
| `BlockProcessResultTest` | 4 | `co/rsk/net` |
| `BlockSyncServiceTest` | 3 | `co/rsk/net` |
| `MessageCounterTest` | 3 | `co/rsk/net` |
| `SyncPeerStatusTest` | 3 | `co/rsk/net` |
| `TwoAsyncNodeTest` | 3 | `co/rsk/net` |
| `OneAsyncNodeTest` | 2 | `co/rsk/net` |
| `ThreeAsyncNodeUsingSyncProcessorTest` | 2 | `co/rsk/net` |
| `StatusTest` | 2 | `co/rsk/net` |
| `StatusResolverTest` | 2 | `co/rsk/net` |
| `TwoNodeTest` | 2 | `co/rsk/net` |
| `OneNodeTest` | 2 | `co/rsk/net` |

### Trie

rskj: **171 tests** in 21 classes — rustock: **122 tests** in `crates/trie/src/tests.rs`, `crates/trie/src/node.rs`, `crates/trie/src/path.rs`, `crates/trie/src/account.rs`

| rskj test class | tests | package |
|---|---:|---|
| `TrieKeyValueTest` | 19 | `co/rsk/trie` |
| `TrieStoreImplTest` | 18 | `co/rsk/trie` |
| `TrieGetNodesTest` | 14 | `co/rsk/trie` |
| `TrieDeleteTest` | 14 | `co/rsk/trie` |
| `TrieHashTest` | 14 | `co/rsk/trie` |
| `TrieSaveRetrieveTest` | 12 | `co/rsk/trie` |
| `PathEncoderTest` | 10 | `co/rsk/trie` |
| `MultiTrieStoreTest` | 10 | `co/rsk/trie` |
| `TrieOrchidMessageTest` | 10 | `co/rsk/trie` |
| `TrieDTOTest` | 8 | `co/rsk/trie` |
| `TrieHashTest` | 8 | `co/rsk/trie/delete` |
| `SecureTrieKeyValueTest` | 6 | `co/rsk/trie/delete` |
| `TrieDTOInOrderIteratorTest` | 5 | `co/rsk/trie` |
| `TrieImplKeyValueTest` | 5 | `co/rsk/trie/delete` |
| `NodeReferenceTest` | 4 | `co/rsk/trie` |
| `TrieValueTest` | 4 | `co/rsk/trie` |
| `TrieTreeSizeTest` | 3 | `co/rsk/trie` |
| `InOrderIteratorCachingTest` | 3 | `co/rsk/trie` |
| `TrieKeySliceTest` | 2 | `co/rsk/trie` |
| `TrieIteratorTest` | 1 | `co/rsk/trie` |
| `SecureTrieHashTest` | 1 | `co/rsk/trie/delete` |

### Precompiles (non-Bridge)

rskj: **152 tests** in 21 classes — rustock: **110 tests** in `crates/execution/src/precompiles.rs`

| rskj test class | tests | package |
|---|---:|---|
| `AltBN128Test` | 32 | `co/rsk/pcc` |
| `NativeContractTest` | 22 | `co/rsk/pcc` |
| `DeriveExtendedPublicKeyTest` | 17 | `co/rsk/pcc/bto` |
| `GetMultisigScriptHashTest` | 13 | `co/rsk/pcc/bto` |
| `ExtractPublicKeyFromExtendedPublicKeyTest` | 8 | `co/rsk/pcc/bto` |
| `ToBase58CheckTest` | 8 | `co/rsk/pcc/bto` |
| `NativeMethodTest` | 7 | `co/rsk/pcc` |
| `BlockHeaderContractTest` | 6 | `co/rsk/pcc/blockheader` |
| `HDWalletUtilsTest` | 6 | `co/rsk/pcc/bto` |
| `GetCallStackDepthTest` | 6 | `co/rsk/pcc/environment` |
| `BlockAccessorTest` | 4 | `co/rsk/pcc/blockheader` |
| `HDWalletUtilsHelperTest` | 4 | `co/rsk/pcc/bto` |
| `EnvironmentTest` | 4 | `co/rsk/pcc/environment` |
| `Secp256k1MultiplicationTest` | 3 | `co/rsk/pcc/secp256k1` |
| `Secp256k1AdditionTest` | 3 | `co/rsk/pcc/secp256k1` |
| `AbstractAltBN128Test` | 3 | `co/rsk/pcc/altBN128/impls` |
| `GetMultisigScriptHashPerformanceTestCase` | 2 | `co/rsk/pcc/bto` |
| `GetCoinbasePerformanceTestCase` | 1 | `co/rsk/pcc/blockheader` |
| `ExtractPublicKeyFromExtendedPublicKeyPerformanceTestCase` | 1 | `co/rsk/pcc/bto` |
| `DeriveExtendedPublicKeyPerformanceTestCase` | 1 | `co/rsk/pcc/bto` |
| `ToBase58CheckPerformanceTestCase` | 1 | `co/rsk/pcc/bto` |

### P2P: messages

rskj: **119 tests** in 25 classes — rustock: **58 tests** in `crates/networking/src/protocol/rsk.rs`, `crates/networking/src/rlpx/frame.rs`, `crates/networking/src/peers.rs`, `crates/networking/src/peer_exchange.rs`

| rskj test class | tests | package |
|---|---:|---|
| `MessageVisitorTest` | 25 | `co/rsk/net/messages` |
| `MessageTest` | 23 | `co/rsk/net/messages` |
| `SnapStatusResponseMessageTest` | 7 | `co/rsk/net/messages` |
| `SnapBlocksResponseMessageTest` | 6 | `co/rsk/net/messages` |
| `SnapStateChunkRequestMessageTest` | 6 | `co/rsk/net/messages` |
| `SnapStateChunkResponseMessageTest` | 6 | `co/rsk/net/messages` |
| `SnapBlocksRequestMessageTest` | 5 | `co/rsk/net/messages` |
| `StatusMessageTest` | 4 | `co/rsk/net/messages` |
| `SnapStatusRequestMessageTest` | 4 | `co/rsk/net/messages` |
| `BlockMessageTest` | 3 | `co/rsk/net/messages` |
| `TransactionsMessageTest` | 3 | `co/rsk/net/messages` |
| `NewBlockHashesTest` | 3 | `co/rsk/net/messages` |
| `BodyResponseMessageTest` | 3 | `co/rsk/net/messages` |
| `BlockHeadersRequestMessageTest` | 2 | `co/rsk/net/messages` |
| `GetBlockMessageTest` | 2 | `co/rsk/net/messages` |
| `SkeletonResponseMessageTest` | 2 | `co/rsk/net/messages` |
| `BlockHashResponseMessageTest` | 2 | `co/rsk/net/messages` |
| `BlockHashRequestMessageTest` | 2 | `co/rsk/net/messages` |
| `BodyRequestMessageTest` | 2 | `co/rsk/net/messages` |
| `SkeletonRequestMessageTest` | 2 | `co/rsk/net/messages` |
| `BlockRequestMessageTest` | 2 | `co/rsk/net/messages` |
| `BlockResponseMessageTest` | 2 | `co/rsk/net/messages` |
| `BlockHeadersResponseMessageTest` | 1 | `co/rsk/net/messages` |
| `BlockHeadersByHashMessageTest` | 1 | `co/rsk/net/messages` |
| `NewBlockHashTest` | 1 | `co/rsk/net/messages` |

### Mining / merged mining

rskj: **116 tests** in 19 classes — rustock: **0 tests** in _no rustock tests_

| rskj test class | tests | package |
|---|---:|---|
| `MinerServerTest` | 19 | `co/rsk/mine` |
| `GasLimitCalculatorTest` | 11 | `co/rsk/mine` |
| `MinerUtilsTest` | 11 | `co/rsk/mine` |
| `MinerManagerTest` | 8 | `co/rsk/mine` |
| `MinimumGasPriceCalculatorTest` | 7 | `co/rsk/mine` |
| `MinerClockTest` | 6 | `co/rsk/mine` |
| `TransactionModuleTest` | 6 | `co/rsk/mine` |
| `SubmissionRateLimitHandlerTest` | 6 | `co/rsk/mine` |
| `BlockToMineBuilderTest` | 6 | `co/rsk/mine` |
| `StableMinGasPriceProviderTest` | 6 | `co/rsk/mine/gas/provider` |
| `ForkDataDetectionCalculatorTest` | 5 | `co/rsk/mine` |
| `HttpGetMinGasPriceProviderTest` | 5 | `co/rsk/mine/gas/provider` |
| `EthCallMinGasPriceProviderTest` | 4 | `co/rsk/mine/gas/provider` |
| `MinGasPriceProviderFactoryTest` | 4 | `co/rsk/mine/gas/provider` |
| `AutoMinerClientTest` | 3 | `co/rsk/mine` |
| `FixedMinGasPriceProviderTest` | 3 | `co/rsk/mine/gas` |
| `BlockToMineBuilderEvictionEndToEndTest` | 2 | `co/rsk/mine` |
| `BlockToMineBuilderInvalidTxEvictionTest` | 2 | `co/rsk/mine` |
| `MainNetMinerTest` | 2 | `co/rsk/mine` |

### Peer scoring

rskj: **113 tests** in 10 classes — rustock: **0 tests** in _no rustock tests_

| rskj test class | tests | package |
|---|---:|---|
| `PeerScoringManagerTest` | 30 | `co/rsk/scoring` |
| `PeerScoringTest` | 21 | `co/rsk/scoring` |
| `InetAddressCidrBlockTest` | 15 | `co/rsk/scoring` |
| `InetAddressUtilsTest` | 14 | `co/rsk/scoring` |
| `InetAddressTableTest` | 12 | `co/rsk/scoring` |
| `PunishmentCalculatorTest` | 7 | `co/rsk/scoring` |
| `ScoringCalculatorTest` | 5 | `co/rsk/scoring` |
| `PeerScoringReputationSummaryTest` | 4 | `co/rsk/scoring` |
| `PeerScoringReporterServiceTest` | 3 | `co/rsk/scoring` |
| `PeerScoringReporterUtilTest` | 2 | `co/rsk/scoring` |

### Bridge: bitcoin primitives

rskj: **88 tests** in 9 classes — rustock: **53 tests** in `crates/execution/src/bridge/pmt.rs`, `crates/execution/src/bridge/btc_store.rs`, `crates/execution/src/bridge/btc_chain.rs`

| rskj test class | tests | package |
|---|---:|---|
| `BitcoinUtilsTest` | 58 | `co/rsk/peg/bitcoin` |
| `MerkleBranchTest` | 9 | `co/rsk/peg/bitcoin` |
| `BitcoinUtilsLegacyTest` | 7 | `co/rsk/peg/bitcoin` |
| `UtxoUtilsTest` | 6 | `co/rsk/peg/bitcoin` |
| `ScriptValidationsTest` | 4 | `co/rsk/peg/bitcoin` |
| `CoinbaseInformationTest` | 1 | `co/rsk/peg/bitcoin` |
| `P2shErpRedeemScriptBuilderTest` | 1 | `co/rsk/peg/bitcoin` |
| `NonStandardErpRedeemScriptBuilderTest` | 1 | `co/rsk/peg/bitcoin` |
| `NonStandardErpRedeemScriptBuilderHardcodedTest` | 1 | `co/rsk/peg/bitcoin` |

### Block validation

rskj: **88 tests** in 18 classes — rustock: **20 tests** in `crates/core/src/validation/tests.rs`, `crates/core/src/validation/merged_mining.rs`

| rskj test class | tests | package |
|---|---:|---|
| `BlockTxsFieldsValidationRuleTest` | 12 | `co/rsk/validators` |
| `ValidTxExecutionSublistsEdgesTest` | 10 | `co/rsk/validators` |
| `BlockTimeStampValidationRuleTest` | 10 | `co/rsk/validators` |
| `ForkDetectionDataRuleTest` | 7 | `co/rsk/validators` |
| `BlockTxsValidationRuleTest` | 7 | `co/rsk/validators` |
| `TxGasPriceCapTest` | 5 | `co/rsk/validators` |
| `TxsMinGasPriceValidatorTest` | 5 | `co/rsk/validators` |
| `PrevMinGasPriceValidatorTest` | 5 | `co/rsk/validators` |
| `BlockParentCompositeRuleTest` | 5 | `co/rsk/validators` |
| `RemascValidationRuleTest` | 4 | `co/rsk/validators` |
| `ValidGasUsedValidatorTest` | 3 | `co/rsk/validators` |
| `GasLimitRuleTests` | 3 | `co/rsk/validators` |
| `ExtraDataRuleTests` | 3 | `co/rsk/validators` |
| `BlockTxsMaxGasPriceRuleTest` | 2 | `co/rsk/validators` |
| `BlockParentGasLimitRuleTest` | 2 | `co/rsk/validators` |
| `BlockUnclesValidationRuleTest` | 2 | `co/rsk/validators` |
| `BlockDifficultyRuleTest` | 2 | `co/rsk/validators` |
| `BlockDifficultyValidationRuleTest` | 1 | `co/rsk/validators` |

### Crypto

rskj: **79 tests** in 10 classes — rustock: **0 tests** in _no rustock tests_

| rskj test class | tests | package |
|---|---:|---|
| `Secp256k1ServiceTest` | 30 | `org/ethereum/crypto/signature` |
| `ECKeyTest` | 17 | `org/ethereum/crypto` |
| `CryptoTest` | 14 | `org/ethereum/crypto` |
| `Secp256k1Test` | 6 | `org/ethereum/crypto/signature` |
| `ECDSASignatureTest` | 4 | `org/ethereum/crypto/signature` |
| `ECIESTest` | 3 | `org/ethereum/crypto` |
| `ECIESCoderTest` | 2 | `org/ethereum/crypto` |
| `EncryptedDataTest` | 1 | `co/rsk/crypto` |
| `Blake2bTest` | 1 | `org/ethereum/crypto/cryptohash` |
| `Secp256k1ServiceNativeTest` | 1 | `org/ethereum/crypto/signature` |

### Other: co/rsk/test

rskj: **77 tests** in 8 classes — rustock: **0 tests** in _no rustock tests_

| rskj test class | tests | package |
|---|---:|---|
| `DslFilesTest` | 30 | `co/rsk/test` |
| `WorldDslProcessorTest` | 25 | `co/rsk/test/dsltest` |
| `DslParserTest` | 8 | `co/rsk/test/dsltest` |
| `WorldTest` | 6 | `co/rsk/test` |
| `DslCommandTest` | 2 | `co/rsk/test/dsltest` |
| `TransactionBuilderTest` | 2 | `co/rsk/test/builderstest` |
| `AccountBuilderTest` | 2 | `co/rsk/test/builderstest` |
| `BlockBuilderTest` | 2 | `co/rsk/test/builderstest` |

### Other: org/ethereum/net

rskj: **77 tests** in 22 classes — rustock: **0 tests** in _no rustock tests_

| rskj test class | tests | package |
|---|---:|---|
| `ChannelManagerImplTest` | 9 | `org/ethereum/net/server` |
| `NodeTest` | 8 | `org/ethereum/net/rlpx` |
| `StatsTest` | 7 | `org/ethereum/net/server` |
| `DisconnectMessageTest` | 6 | `org/ethereum/net` |
| `HandshakeHandlerTest` | 5 | `org/ethereum/net/rlpx` |
| `PeersMessageTest` | 5 | `org/ethereum/net/p2p` |
| `HelloMessageTest` | 4 | `org/ethereum/net` |
| `NodeManagerTest` | 4 | `org/ethereum/net` |
| `EIP8HandshakeTest` | 4 | `org/ethereum/net/rlpx` |
| `AdaptiveMessageIdsTest` | 4 | `org/ethereum/net/wire` |
| `RlpxConnectionTest` | 3 | `org/ethereum/net/rlpx` |
| `ChannelTest` | 3 | `org/ethereum/net/server` |
| `PingPongMessageTest` | 2 | `org/ethereum/net` |
| `EncryptionHandshakeTest` | 2 | `org/ethereum/net/rlpx` |
| `EthereumChannelInitializerTest` | 2 | `org/ethereum/net/server` |
| `CapabilityTest` | 2 | `org/ethereum/net/client` |
| `P2pHandlerTest` | 2 | `org/ethereum/net/p2p` |
| `NodeStatisticsTest` | 1 | `org/ethereum/net` |
| `StatusMessageTest` | 1 | `org/ethereum/net` |
| `PeerConnectionDataTest` | 1 | `org/ethereum/net` |
| `NettyTest` | 1 | `org/ethereum/net` |
| `EIP8P2pTest` | 1 | `org/ethereum/net/p2p` |

### P2P: sync

rskj: **76 tests** in 13 classes — rustock: **0 tests** in _no rustock tests_

| rskj test class | tests | package |
|---|---:|---|
| `SnapSyncStateTest` | 17 | `co/rsk/net/sync` |
| `PeersInformationTest` | 10 | `co/rsk/net/sync` |
| `PeerAndModeDecidingSyncStateTest` | 10 | `co/rsk/net/sync` |
| `DownloadingBackwardsBodiesSyncStateTest` | 9 | `co/rsk/net/sync` |
| `DownloadingHeadersSyncStateTest` | 6 | `co/rsk/net/sync` |
| `ConnectionPointFinderTest` | 5 | `co/rsk/net/sync` |
| `CheckingBestHeaderSyncStateTest` | 5 | `co/rsk/net/sync` |
| `DownloadingBodiesSyncStateTest` | 3 | `co/rsk/net/sync` |
| `DownloadingBackwardsHeadersSyncStateTest` | 3 | `co/rsk/net/sync` |
| `SyncMessageHandlerTest` | 3 | `co/rsk/net/sync` |
| `FindingConnectionPointSyncStateTest` | 2 | `co/rsk/net/sync` |
| `BlockConnectorHelperTest` | 2 | `co/rsk/net/sync` |
| `DownloadingSkeletonSyncStateTest` | 1 | `co/rsk/net/sync` |

### Config

rskj: **67 tests** in 8 classes — rustock: **45 tests** in `crates/core/src/config.rs`, `crates/execution/src/hardfork.rs`, `crates/execution/src/bridge/constants.rs`

| rskj test class | tests | package |
|---|---:|---|
| `RskSystemPropertiesTest` | 20 | `co/rsk/config` |
| `ConfigLoaderTest` | 15 | `co/rsk/config` |
| `ActivationConfigTest` | 12 | `org/ethereum/config/blockchain/upgrades` |
| `StableMinGasPriceSystemConfigTest` | 7 | `co/rsk/config/mining` |
| `ConstantsTest` | 4 | `org/ethereum/config` |
| `RemascConfigFactoryTest` | 3 | `co/rsk/config` |
| `EthCallMinGasPriceSystemConfigTest` | 3 | `co/rsk/config/mining` |
| `HttpGetStableMinGasSystemConfigTest` | 3 | `co/rsk/config/mining` |

### P2P: discovery

rskj: **66 tests** in 16 classes — rustock: **5 tests** in `crates/networking/src/discovery/table.rs`, `crates/networking/src/discovery/message.rs`, `crates/networking/src/discovery/tests.rs`

| rskj test class | tests | package |
|---|---:|---|
| `PeerExplorerTest` | 17 | `co/rsk/net/discovery` |
| `MessageDecoderTest` | 9 | `co/rsk/net/discovery` |
| `PeerMessagesTest` | 6 | `co/rsk/net/discovery` |
| `PeerExplorerCleanerTest` | 5 | `co/rsk/net/discovery` |
| `UDPChannelTest` | 4 | `co/rsk/net/discovery` |
| `NodeDistanceTableTest` | 4 | `co/rsk/net/discovery/table` |
| `UDPServerTest` | 3 | `co/rsk/net/discovery` |
| `FindNodePeerMessageTest` | 3 | `co/rsk/net/discovery/message` |
| `PingPeerMessageTest` | 3 | `co/rsk/net/discovery/message` |
| `PongPeerMessageTest` | 3 | `co/rsk/net/discovery/message` |
| `NeighborsPeerMessageTest` | 3 | `co/rsk/net/discovery/message` |
| `KnownPeersHandlerTest` | 2 | `co/rsk/net/discovery` |
| `PacketDecoderTest` | 1 | `co/rsk/net/discovery` |
| `NodeChallengeManagerTest` | 1 | `co/rsk/net/discovery` |
| `PeerDiscoveryRequestTest` | 1 | `co/rsk/net/discovery` |
| `DistanceCalculatorTest` | 1 | `co/rsk/net/discovery/table` |

### Bridge: performance

rskj: **62 tests** in 27 classes — rustock: **0 tests** in _no rustock tests_

| rskj test class | tests | package |
|---|---:|---|
| `RetiringFederationTest` | 6 | `co/rsk/peg/performance` |
| `GetBtcTransactionConfirmationsTest` | 6 | `co/rsk/peg/performance` |
| `ActiveFederationTest` | 6 | `co/rsk/peg/performance` |
| `LockWhitelistTest` | 5 | `co/rsk/peg/performance` |
| `ReceiveHeadersTest` | 4 | `co/rsk/peg/performance` |
| `FederationChangeTest` | 4 | `co/rsk/peg/performance` |
| `PendingFederationTest` | 3 | `co/rsk/peg/performance` |
| `PegoutBatchingBridgeMethodsTest` | 3 | `co/rsk/peg/performance` |
| `LockingCapTest` | 3 | `co/rsk/peg/performance` |
| `LockTest` | 3 | `co/rsk/peg/performance` |
| `UpdateCollectionsTest` | 2 | `co/rsk/peg/performance` |
| `VoteFeePerKbChangeTest` | 2 | `co/rsk/peg/performance` |
| `StateForBtcReleaseClientTest` | 1 | `co/rsk/peg/performance` |
| `GetFeePerKbTest` | 1 | `co/rsk/peg/performance` |
| `AddSignatureTest` | 1 | `co/rsk/peg/performance` |
| `GetBtcBlockchainParentBlockHeaderByHashTest` | 1 | `co/rsk/peg/performance` |
| `PowpegRedeemScriptTest` | 1 | `co/rsk/peg/performance` |
| `RegisterBtcTransactionTest` | 1 | `co/rsk/peg/performance` |
| `GetBtcBlockchainBestBlockHeaderTest` | 1 | `co/rsk/peg/performance` |
| `GetBtcBlockchainBlockHeaderByHeightTest` | 1 | `co/rsk/peg/performance` |
| `IdentityPerformanceTestCase` | 1 | `co/rsk/peg/performance` |
| `ReceiveHeaderTest` | 1 | `co/rsk/peg/performance` |
| `ReleaseBtcTest` | 1 | `co/rsk/peg/performance` |
| `RegisterFlyoverBtcTransactionTest` | 1 | `co/rsk/peg/performance` |
| `RegisterBtcCoinbaseTransactionTest` | 1 | `co/rsk/peg/performance` |
| `HasBtcBlockCoinbaseTransactionInformationTest` | 1 | `co/rsk/peg/performance` |
| `GetBtcBlockchainBlockHeaderByHashTest` | 1 | `co/rsk/peg/performance` |

### VM / EVM (json suite)

rskj: **62 tests** in 6 classes — rustock: **0 tests** in _no rustock tests_

| rskj test class | tests | package |
|---|---:|---|
| `GitHubStateTest` | 24 | `org/ethereum/jsontestsuite` |
| `GitHubVMTest` | 16 | `org/ethereum/jsontestsuite` |
| `GitHubBlockTest` | 16 | `org/ethereum/jsontestsuite` |
| `GitHubBasicTest` | 3 | `org/ethereum/jsontestsuite` |
| `GitHubRLPTest` | 2 | `org/ethereum/jsontestsuite` |
| `GitHubCryptoTest` | 1 | `org/ethereum/jsontestsuite` |

### Other: co/rsk/jsontestsuite

rskj: **60 tests** in 6 classes — rustock: **0 tests** in _no rustock tests_

| rskj test class | tests | package |
|---|---:|---|
| `LocalStateTest` | 22 | `co/rsk/jsontestsuite` |
| `LocalBlockTest` | 16 | `co/rsk/jsontestsuite` |
| `LocalVMTest` | 15 | `co/rsk/jsontestsuite` |
| `LocalBasicTest` | 4 | `co/rsk/jsontestsuite` |
| `LocalRLPTest` | 2 | `co/rsk/jsontestsuite` |
| `LocalCryptoTest` | 1 | `co/rsk/jsontestsuite` |

### Bridge: whitelist

rskj: **54 tests** in 3 classes — rustock: **0 tests** in _no rustock tests_

| rskj test class | tests | package |
|---|---:|---|
| `WhitelistSupportImplTest` | 31 | `co/rsk/peg/whitelist` |
| `LockWhitelistTest` | 17 | `co/rsk/peg/whitelist` |
| `WhitelistStorageProviderImplTest` | 6 | `co/rsk/peg/whitelist` |

### Bridge: utils

rskj: **49 tests** in 7 classes — rustock: **40 tests** in `crates/execution/src/bridge/serialization.rs`, `crates/execution/src/bridge/events.rs`

| rskj test class | tests | package |
|---|---:|---|
| `BridgeEventLoggerImplTest` | 18 | `co/rsk/peg/utils` |
| `BridgeEventLoggerTest` | 10 | `co/rsk/peg/utils` |
| `BridgeEventLoggerLegacyImplTest` | 9 | `co/rsk/peg/utils` |
| `PartialMerkleTreeFormatUtilsTest` | 5 | `co/rsk/peg/utils` |
| `BtcTransactionFormatUtilsTest` | 4 | `co/rsk/peg/utils` |
| `EcKeyUtilsTest` | 2 | `co/rsk/peg/utils` |
| `MerkleTreeUtilsTest` | 1 | `co/rsk/peg/utils` |

### REMASC

rskj: **46 tests** in 9 classes — rustock: **20 tests** in `crates/execution/src/remasc.rs`

| rskj test class | tests | package |
|---|---:|---|
| `RemascStorageProviderTest` | 18 | `co/rsk/remasc` |
| `RemascProcessMinerFeesTest` | 15 | `co/rsk/remasc` |
| `RemascFederationProviderTest` | 3 | `co/rsk/remasc` |
| `RemascContractExecuteTest` | 3 | `co/rsk/remasc` |
| `RemascStateTest` | 2 | `co/rsk/remasc` |
| `SiblingTest` | 2 | `co/rsk/remasc` |
| `RemascTransactionTest` | 1 | `co/rsk/remasc` |
| `RemascRskAddressActivationTest` | 1 | `co/rsk/remasc` |
| `RemascFeesPayerTest` | 1 | `co/rsk/remasc` |

### P2P: tx handling

rskj: **39 tests** in 13 classes — rustock: **38 tests** in `crates/sync/src/txpool.rs`

| rskj test class | tests | package |
|---|---:|---|
| `TxsPerAccountTest` | 7 | `co/rsk/net/handler` |
| `TxPendingValidatorTest` | 6 | `co/rsk/net/handler` |
| `TxValidatorNonceRangeValidatorTest` | 4 | `co/rsk/net/handler/txvalidator` |
| `TxValidatorAccountBalanceValidatorTest` | 3 | `co/rsk/net/handler/txvalidator` |
| `TxQuotaTest` | 3 | `co/rsk/net/handler/quota` |
| `TxValidatorGasLimitValidatorTest` | 2 | `co/rsk/net/handler/txvalidator` |
| `TxValidatorAccountStateValidatorTest` | 2 | `co/rsk/net/handler/txvalidator` |
| `TxValidatorMinimumGasPriceValidatorTest` | 2 | `co/rsk/net/handler/txvalidator` |
| `TxValidatorNotNullValidatorTest` | 2 | `co/rsk/net/handler/txvalidator` |
| `TxValidatorNotRemascTxValidatorTest` | 2 | `co/rsk/net/handler/txvalidator` |
| `TxValidatorMaximumGasPriceValidatorTest` | 2 | `co/rsk/net/handler/txvalidator` |
| `TxValidatorIntrinsicGasLimitValidatorTest` | 2 | `co/rsk/net/handler/txvalidator` |
| `TxQuotaCheckerIntegrationTest` | 2 | `co/rsk/net/handler/quota` |

### Log filters

rskj: **38 tests** in 5 classes — rustock: **0 tests** in _no rustock tests_

| rskj test class | tests | package |
|---|---:|---|
| `BlocksBloomTest` | 12 | `co/rsk/logfilter` |
| `BlocksBloomProcessorTest` | 10 | `co/rsk/logfilter` |
| `BlocksBloomStoreTest` | 9 | `co/rsk/logfilter` |
| `BlocksBloomServiceTest` | 4 | `co/rsk/logfilter` |
| `BlocksBloomEncoderTest` | 3 | `co/rsk/logfilter` |

### CLI / tools

rskj: **37 tests** in 6 classes — rustock: **0 tests** in _no rustock tests_

| rskj test class | tests | package |
|---|---:|---|
| `CliToolsTest` | 17 | `co/rsk/cli/tools` |
| `ValidateStateTest` | 13 | `co/rsk/cli/tools` |
| `RskCliTest` | 3 | `co/rsk/cli` |
| `CliArgsTest` | 2 | `co/rsk/cli` |
| `MigratorTest` | 1 | `co/rsk/cli/config` |
| `ValidateBtcHeadersTest` | 1 | `co/rsk/cli/tools` |

### Bridge: locking cap

rskj: **36 tests** in 3 classes — rustock: **0 tests** in _no rustock tests_

| rskj test class | tests | package |
|---|---:|---|
| `LockingCapSupportImplTest` | 16 | `co/rsk/peg/lockingcap` |
| `LockingCapIT` | 13 | `co/rsk/peg/lockingcap` |
| `LockingCapStorageProviderImplTest` | 7 | `co/rsk/peg/lockingcap` |

### Other: co/rsk

rskj: **35 tests** in 4 classes — rustock: **0 tests** in _no rustock tests_

| rskj test class | tests | package |
|---|---:|---|
| `RskContextTest` | 18 | `co/rsk` |
| `NodeRunnerImplTest` | 9 | `co/rsk` |
| `NodeRunnerSmokeTest` | 5 | `co/rsk` |
| `StartTest` | 3 | `co/rsk` |

### Bridge: fee per KB

rskj: **35 tests** in 3 classes — rustock: **0 tests** in _no rustock tests_

| rskj test class | tests | package |
|---|---:|---|
| `FeePerKbIT` | 17 | `co/rsk/peg/feeperkb` |
| `FeePerKbSupportImplTest` | 13 | `co/rsk/peg/feeperkb` |
| `FeePerKbStorageProviderImplTest` | 5 | `co/rsk/peg/feeperkb` |

### Other: org/ethereum/validator

rskj: **31 tests** in 5 classes — rustock: **0 tests** in _no rustock tests_

| rskj test class | tests | package |
|---|---:|---|
| `ProofOfWorkRuleTest` | 11 | `org/ethereum/validator` |
| `Rskip92MerkleProofValidatorTests` | 10 | `org/ethereum/validator` |
| `ParentGasLimitRuleTest` | 5 | `org/ethereum/validator` |
| `ParentNumberRuleTest` | 3 | `org/ethereum/validator` |
| `DifficultyRuleTest` | 2 | `org/ethereum/validator` |

### Bridge: governance vote

rskj: **27 tests** in 3 classes — rustock: **13 tests** in `crates/execution/src/bridge/governance.rs`, `crates/execution/src/bridge/vote.rs`

| rskj test class | tests | package |
|---|---:|---|
| `AddressBasedAuthorizerFactoryTest` | 12 | `co/rsk/peg/vote` |
| `ABICallElectionTest` | 11 | `co/rsk/peg/vote` |
| `ABICallSpecTest` | 4 | `co/rsk/peg/vote` |

### Events/listeners

rskj: **21 tests** in 3 classes — rustock: **0 tests** in _no rustock tests_

| rskj test class | tests | package |
|---|---:|---|
| `GasPriceTrackerTest` | 13 | `org/ethereum/listener` |
| `WeightedPercentileGasPriceCalculatorTest` | 7 | `org/ethereum/listener` |
| `WeightedPercentileCalcTest` | 1 | `org/ethereum/listener` |

### Other: co/rsk/metrics

rskj: **15 tests** in 3 classes — rustock: **0 tests** in _no rustock tests_

| rskj test class | tests | package |
|---|---:|---|
| `HashRateCalculatorTest` | 5 | `co/rsk/metrics` |
| `JmxMetricTest` | 5 | `co/rsk/metrics/jmx` |
| `JmxProfilerTest` | 5 | `co/rsk/metrics/profilers/impl` |

### Chain bootstrap

rskj: **12 tests** in 2 classes — rustock: **0 tests** in _no rustock tests_

| rskj test class | tests | package |
|---|---:|---|
| `BlockchainTest` | 10 | `co/rsk/blockchain` |
| `BlockGeneratorTest` | 2 | `co/rsk/blockchain/utils` |

### Other: co/rsk/jsonrpc

rskj: **9 tests** in 6 classes — rustock: **0 tests** in _no rustock tests_

| rskj test class | tests | package |
|---|---:|---|
| `JacksonBasedRpcSerializerTest` | 3 | `co/rsk/jsonrpc` |
| `JsonRpcBooleanResultTest` | 2 | `co/rsk/jsonrpc` |
| `JsonRpcInternalErrorTest` | 1 | `co/rsk/jsonrpc` |
| `JsonRpcResultResponseTest` | 1 | `co/rsk/jsonrpc` |
| `JsonRpcErrorTest` | 1 | `co/rsk/jsonrpc` |
| `JsonRpcErrorResponseTest` | 1 | `co/rsk/jsonrpc` |

### P2P: eth wire

rskj: **8 tests** in 1 classes — rustock: **0 tests** in _no rustock tests_

| rskj test class | tests | package |
|---|---:|---|
| `RskWireProtocolTest` | 8 | `co/rsk/net/eth` |

### Other: org/ethereum/solidity

rskj: **7 tests** in 2 classes — rustock: **0 tests** in _no rustock tests_

| rskj test class | tests | package |
|---|---:|---|
| `SolidityTypeTest` | 6 | `org/ethereum/solidity` |
| `CompilerTest` | 1 | `org/ethereum/solidity` |

### Other: co/rsk/datasource

rskj: **4 tests** in 1 classes — rustock: **0 tests** in _no rustock tests_

| rskj test class | tests | package |
|---|---:|---|
| `HashMapDBTest` | 4 | `co/rsk/datasource` |

### Other: co/rsk/lll

rskj: **2 tests** in 1 classes — rustock: **0 tests** in _no rustock tests_

| rskj test class | tests | package |
|---|---:|---|
| `LLLTest` | 2 | `co/rsk/lll` |

### Other: org/ethereum/facade

rskj: **2 tests** in 1 classes — rustock: **0 tests** in _no rustock tests_

| rskj test class | tests | package |
|---|---:|---|
| `EthereumImplTest` | 2 | `org/ethereum/facade` |