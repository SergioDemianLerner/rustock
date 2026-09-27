# What geth and reth have that rustock does not

A gap list against the two main Ethereum execution clients, excluding
everything specific to proof-of-stake.

## How to read this

**The two sides are not equally verified.** Everything claimed about rustock
was read out of this repository: the RPC list is the dispatcher in
`crates/rpc/src/server.rs`, not documentation. Everything claimed about geth
and reth is from knowledge — there is no local checkout of either to grep —
so treat those rows as "believed true of recent releases" and check any one of
them before acting on it. Rows I am less sure of are marked ⚠.

**rustock's target is rskj, not geth.** A gap against geth is only a defect if
rskj has the feature too; otherwise it is a difference between the RSK and
Ethereum ecosystems. Each table carries an **applicable** column saying which:

| | meaning |
|---|---|
| ✅ | a real gap — meaningful for an RSK node |
| ➖ | not applicable: depends on Ethereum machinery RSK does not have |
| 🔸 | applicable but low value, or deliberately declined |

## What was excluded

Proof-of-stake and everything hanging off it: the Engine API
(`engine_forkchoiceUpdated`, `engine_newPayload`, `engine_getPayload`), JWT
authentication for it, beacon-chain sync, withdrawals
(`eth_getBlockByNumber` withdrawal fields), and validator/builder plumbing.

One consequence is worth stating because it removes a lot of surface: RSK has
**no typed transactions** (EIP-2718) and **no EIP-1559 fee market** — no
`baseFeePerGas`, no priority fees, no access lists, no blobs. Confirmed on
both sides: neither `crates/core/src/types/` nor rskj's `Transaction.java`
carries a transaction type. So `eth_feeHistory`, `eth_maxPriorityFeePerGas`,
`eth_createAccessList`, `eth_blobBaseFee` and the blob pool are not gaps; they
describe a protocol RSK does not implement.

## Where rustock stands today

77 dispatched JSON-RPC methods across ten namespaces — `eth` (37), `rsk` (10),
`sco` (7), `debug` (5), `trace` (4), `net` (4), `mnr` (4), `txpool` (3),
`web3` (2), `rpc` (1) — plus `eth_subscribe`/`eth_unsubscribe` over WebSocket,
and five wallet methods that exist only to return a clear error.

---

## 1. `eth` namespace

| method | geth | reth | applicable | note |
|---|:-:|:-:|:-:|---|
| `eth_getProof` | ✔ | ✔ | ✅ | **The biggest single gap.** EIP-1186 Merkle proofs of account and storage. Needs a unitrie-shaped answer rather than a copy of Ethereum's, since the trie differs — but light clients, bridges and cross-chain verifiers all want it. |
| `eth_getBlockReceipts` | ✔ | ✔ | ✅ | All receipts for a block in one call. Indexers otherwise make N round trips; cheap to add, the data is already stored together. |
| `eth_simulateV1` | ✔ | ⚠ | ✅ | Simulate a bundle of calls across block overrides. Newer, increasingly used by wallets. |
| `eth_getHeaderByNumber` / `…ByHash` | ✔ | ⚠ | 🔸 | `rsk_getRawBlockHeaderByNumber`/`…ByHash` already answer this in RLP form. |
| `eth_feeHistory`, `eth_maxPriorityFeePerGas` | ✔ | ✔ | ➖ | EIP-1559. No base fee on RSK. |
| `eth_createAccessList` | ✔ | ✔ | ➖ | EIP-2930. No typed transactions on RSK. |
| `eth_blobBaseFee`, blob accessors | ✔ | ✔ | ➖ | EIP-4844. |
| `eth_fillTransaction`, `eth_resend` | ✔ | ✖ | 🔸 | Wallet-adjacent; rustock has no key management by design. |

## 2. `debug` namespace

rustock has five `debug_` methods; geth has roughly forty. Setting aside the
Go-runtime ones (`debug_memStats`, `debug_gcStats`, `debug_stacks`,
`debug_startCPUProfile`, …), which are language-specific rather than protocol
features:

| method | geth | reth | applicable | note |
|---|:-:|:-:|:-:|---|
| `debug_traceCall` | ✔ | ✔ | ✅ | **The most commonly missed one.** Trace a call that was never mined. Wallets and simulators use it constantly, and rustock already has the tracer and the call machinery — this is mostly plumbing. |
| `debug_storageRangeAt` | ✔ | ✔ | ✅ | Paginate a contract's storage. Nothing else answers "what is in this contract" without knowing the keys. |
| `debug_getRawBlock`, `…RawHeader`, `…RawReceipts`, `…RawTransaction` | ✔ | ✔ | 🔸 | `rsk_getRawBlockHeaderBy*` and `rsk_getRawTransactionReceiptByHash` cover part of this under RSK names. |
| `debug_getModifiedAccountsByNumber` / `…ByHash` | ✔ | ⚠ | ✅ | Which accounts a block touched. Indexers and state-diff tools want it; rustock's `diff_state`/`diff_roots` examples do this offline but not over RPC. |
| `debug_accountRange` | ✔ | ⚠ | 🔸 | Iterate the state trie. The trie tooling covers this offline. |
| `debug_setHead` | ✔ | ⚠ | 🔸 | Force a reorg. `rewind_to` and `set_exec_head` do it as offline tools, which is arguably safer. |
| `debug_dumpBlock`, `debug_intermediateRoots` | ✔ | ⚠ | 🔸 | Debugging aids. |
| `debug_chaindbProperty`, `debug_chaindbCompact` | ✔ | ⚠ | 🔸 | `lsm_stats` exists as an offline example. |

### Tracer plugins — a structural gap, not a missing method

geth and reth both accept a **named tracer** on `debug_trace*`:
`callTracer`, `prestateTracer`, `4byteTracer`, `muxTracer`, `flatCallTracer`,
`noopTracer`, and geth additionally runs **JavaScript tracers** supplied in the
request.

rustock supports none of these. It produces rskj's opcode-level `structLogs`
from `debug_trace*`, and Parity-style call trees from `trace_*`. That covers
what `callTracer` and `flatCallTracer` are usually wanted *for*, in a different
shape — but tooling written against geth asks by tracer name and will get
nothing it recognises.

`prestateTracer` has no rustock equivalent in any shape, and it is the one
that matters most: it is how simulators obtain the exact state a transaction
read. ✅

## 3. `trace` namespace (Parity/Erigon-style)

geth has no `trace_` namespace at all. **reth** implements the full set;
rustock implements four of nine.

| method | reth | applicable | note |
|---|:-:|:-:|---|
| `trace_call` | ✔ | ✅ | Call tree for an unmined call — the `trace_` counterpart of `debug_traceCall`. |
| `trace_callMany` | ✔ | ✅ | Sequential calls sharing state. |
| `trace_rawTransaction` | ✔ | 🔸 | Trace a signed-but-unmined transaction. |
| `trace_replayTransaction` | ✔ | ✅ | Trace **plus** state diff and VM trace in one response. The state-diff output has no rustock equivalent. |
| `trace_replayBlockTransactions` | ✔ | ✅ | Same for a whole block. |

rustock's four (`trace_block`, `trace_filter`, `trace_get`,
`trace_transaction`) match rskj's `TraceModuleImpl` exactly, which is the
declared target. The five above are reth-and-Erigon extensions.

## 4. `txpool`, `admin`, `ots`

| | geth | reth | applicable | note |
|---|:-:|:-:|:-:|---|
| `txpool_contentFrom` | ✔ | ✔ | ✅ | One sender's queued and pending transactions. Small, and rustock's pool already indexes by sender. |
| `admin_nodeInfo` | ✔ | ✔ | ✅ | Own enode, protocols, ports. Standard in every ops runbook; rustock has no `admin_` namespace at all. |
| `admin_peers` | ✔ | ✔ | 🔸 | `net_peerList` and `sco_peerList` answer this under other names. |
| `admin_addPeer`, `admin_removePeer`, `admin_addTrustedPeer` | ✔ | ✔ | ✅ | Manual peering. rustock can ban (`sco_banAddress`) but cannot *dial* on request — recovery from a bad peer set needs a restart with different flags. |
| `admin_startHTTP` / `stopHTTP` / `startWS` / `stopWS` | ✔ | ⚠ | 🔸 | Toggle transports at runtime. |
| `ots_*` (Otterscan, 12 methods) | ✖ | ✔ | ✅ | `ots_searchTransactionsBefore`/`After`, `ots_getContractCreator`, `ots_getTransactionBySenderAndNonce`, `ots_getInternalOperations`. This is the block-explorer API: it turns a node into something a UI can browse without an external indexer. The single largest coherent chunk of RPC rustock lacks. |
| `reth_getBalanceChangesInBlock` | ✖ | ✔ | 🔸 | reth-specific. |

---

## 5. Node components and services

| component | geth | reth | applicable | note |
|---|:-:|:-:|:-:|---|
| **Archive mode** | ✔ | ✔ | ✅ | Both can retain all historical state. rustock's epoch GC keeps a bounded window, so `eth_call`, `debug_trace*` and `trace_*` answer only within the burial depth (4,000 blocks) and error past it. This is the deepest architectural difference in the list, and it is a deliberate trade — but it does mean rustock cannot serve the historical-query workloads either client can. |
| **Ancient store / static files** | ✔ freezer | ✔ static files | ✅ | Both move old blocks, headers and receipts out of the mutable database into append-only flat files: far smaller, faster to read, trivially backed up. rustock keeps everything in RocksDB column families and prunes instead. |
| **Flat state snapshot layer** | ✔ | ✔ | ✅ | geth's snapshot layer answers account and storage reads without walking the trie. rustock reads through the unitrie every time — measured at 2.1 disk reads per node during snapshot serving. A flat layer is the standard fix. |
| **Staged sync pipeline** | ✖ | ✔ | 🔸 | reth's headers → bodies → execution → merkle → indexing stages, each independently resumable and unwindable, with `reth stage run/unwind`. rustock's sync is a state machine with its own recovery invariants; different design, comparable intent. |
| **Execution Extensions (ExEx)** | ✖ | ✔ | ✅ | In-process post-execution hooks, reorg-aware, for building indexers without an external ETL. Nothing equivalent exists in rustock; the closest is `eth_subscribe`. |
| **Prometheus metrics** | ✔ | ✔ | ✅ | **rustock exposes no metrics endpoint at all** — no Prometheus, no expvar, no counters over RPC. Operational state is visible only through logs and a handful of RPC calls. For a node meant to run unattended this is the most practically felt gap in this table. |
| **pprof / runtime profiling** | ✔ | ⚠ | 🔸 | Partly language-specific. |
| **IPC transport** | ✔ | ✔ | 🔸 | Unix-socket JSON-RPC. rustock serves HTTP and WebSocket only. Common in local tooling and avoids exposing a TCP port. |
| **GraphQL endpoint** | ✔ | ✖ | 🔸 | geth only, and lightly used. |
| **discv5** | ✔ | ✔ | 🔸 | rustock and rskj both use discv4-style discovery. ENR-based discv5 buys little unless the RSK network adopts it. |
| **Key management / keystore / Clef** | ✔ | ⚠ | 🔸 | Deliberately absent: rustock has no wallet, and `eth_sendTransaction`/`eth_sign`/`personal_*` return errors on purpose. |
| **Database tooling subcommands** | ✔ `geth db`, `snapshot` | ✔ `reth db` | 🔸 | rustock ships 42 read-only example binaries that cover this ground, 21 of which open the store without a lock. Different packaging, comparable capability — arguably better, since they cannot write. |
| **Per-table pruning configuration** | ⚠ | ✔ | 🔸 | reth prunes receipts, history and tx-lookup independently. rustock has block pruning and epoch GC, both coarser. |

---

## 6. What rustock has that neither does

For balance, since a one-way list overstates the distance:

- **The two-way peg** — the Bridge precompile, ~22,700 lines, with a BTC SPV
  chain, federation handling and peg-out queues.
- **Merged mining** — `mnr_*`, the Bitcoin-header validation path, and a
  mining template service.
- **Peer scoring over RPC** — `sco_*`, seven methods including persisted bans.
- **Epoch garbage collection** — wholesale reclamation of the oldest epoch,
  O(1) in dead entries, where geth and reth both walk what they delete.
- **Snapshot sync with completeness proofs** — a chunk is verified by replaying
  the server's traversal over only the bytes the peer supplied, proving no node
  between those sent was skipped. geth's snap sync proves inclusion and heals
  the difference afterwards.
- **`rsk_*` consensus introspection** — raw headers, receipt trie nodes,
  storage bytes.

---

## 7. If any of this were to be done

Ordered by value per unit of work, not by size:

1. **Prometheus metrics.** Nothing else on this list is felt daily by whoever
   operates the node. There is no observability surface at all right now.
2. **`eth_getBlockReceipts`** and **`txpool_contentFrom`.** Both are small,
   both are asked for constantly by indexers, and both read data already laid
   out the right way.
3. **`debug_traceCall`** and **`trace_call`.** The tracer, the call path and
   the state-override machinery all exist; what is missing is the entry point.
4. **`admin_nodeInfo`** plus manual peering. Small, and it removes a restart
   from the bad-peer recovery path.
5. **`eth_getProof`.** Larger, and it needs a unitrie-shaped design rather than
   a port — RSK's trie is not Ethereum's, so the proof format is a decision, not
   a translation. Worth doing for bridges and light clients.
6. **The Otterscan `ots_*` set.** The biggest coherent block, and the one that
   changes what the node is *for*: a block explorer with no external indexer.
7. **A flat state snapshot layer**, if historical query performance ever
   becomes a goal. This is architecture, not a feature.

Deliberately not recommended: anything EIP-1559 or typed-transaction shaped,
anything PoS shaped, and key management.
