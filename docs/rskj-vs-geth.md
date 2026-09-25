# Where rskj differs from Ethereum convention

A running record of places where rskj's behaviour is **not** what the method
name, the Ethereum JSON-RPC spec, or go-ethereum would lead you to expect.

Written because three issues in a row in this repository described
go-ethereum's semantics rather than rskj's, and only reading the Java caught
it. Each entry gives the rskj source, what the difference is, and what it costs
to get wrong.

> **The rule this document exists to enforce:** read rskj before implementing.
> A method name is not a specification, and neither is the Ethereum one.

---

## `eth_pendingTransactions` is the wallet's, not the mempool's

| | |
|---|---|
| **go-ethereum** | all pending transactions in the node's pool |
| **rskj** | only transactions whose sender is an account the node's *own wallet* manages |
| **source** | `co/rsk/rpc/modules/eth/EthModuleWalletEnabled.java`, `EthModuleWalletDisabled.java` |

```java
// EthModuleWalletEnabled
List<Transaction> pendingTxs = transactionPool.getPendingTransactions();
List<String> managedAccounts = Arrays.asList(accounts());
return pendingTxs.stream()
        .filter(tx -> managedAccounts.contains(tx.getSender(...).toJsonString()))
        .collect(Collectors.toList());

// EthModuleWalletDisabled
return Collections.emptyList();
```

**Cost of getting it wrong.** rustock has no wallet, so the faithful answer is
`[]`. Returning the pool instead would present every transaction in the public
mempool as the *caller's own* — wrong in a way the caller cannot detect. A
wallet UI would show strangers' transactions as the user's.

**What to use instead:** `txpool_content`, which reports the pool without
claiming the transactions belong to anyone.

---

## `rsk_getStorageBytesAt` has three answers, not two

| | |
|---|---|
| **Ethereum convention** | a value, or null/zero |
| **rskj** | `0x` + bytes (present), `0x` (present but empty), `0x0` (absent) |
| **source** | `org/ethereum/rpc/Web3Impl.java`, `co/rsk/util/HexUtils.toUnformattedJsonHex` |

```java
return Optional.ofNullable(accountInformationProvider.getStorageBytes(address, key))
        .map(HexUtils::toUnformattedJsonHex)   // "0x" + hex(bytes); "0x" when empty
        .orElse("0x0");                        // absent
```

`0x` and `0x0` look alike and are different facts: a key holding a zero-length
value against a key that is not there. For Bridge state that is "the queue is
empty" against "there is no queue".

**Why the method exists at all** is itself a difference: `eth_getStorageAt`
returns one 32-byte word, and rskj stores precompile state as a single
*variable-length* value at one unitrie key (`Repository.addStorageBytes`). Most
of the Bridge's state — BTC chain head, federation, release request queue, UTXO
set — is unreachable through `eth_getStorageAt`, which silently returns the
first word.

---

## Receipt-proof nodes come back leaf-first, root-last

| | |
|---|---|
| **Intuition** | a proof is ordered root → leaf, as you would walk it |
| **rskj** | the reverse: deepest node first, root last |
| **source** | `co/rsk/trie/Trie.java`, `findNodes` |

```java
List<Trie> subnodes = node.findNodes(key.slice(commonPathLength + 1, key.length()));
if (subnodes == null) return null;
subnodes.add(this);      // <- each level appends ITSELF after recursing
return subnodes;
```

**Cost of getting it wrong.** A consumer that assumes root-first takes the
wrong element as the root, and the proof silently fails to verify against
`receiptsRoot` — with nothing to indicate the ordering was the problem.

### And the nodes alone do not prove anything

A receipt is a *long value*: the leaf carries the value's hash, not its bytes.
`rsk_getTransactionReceiptNodesByHash` therefore cannot yield the receipt on its
own — it must be paired with `rsk_getRawTransactionReceiptByHash`. That is why
the two methods exist together, and a verifier needs both.

A related trap when verifying: a node's hash is `keccak(toMessage(store))`, and
`toMessage` consults the store. **Re-serialising a node whose siblings are
absent — which is exactly what a proof is — produces different bytes and a
different hash.** Hash the message *as received*; do not rebuild it.

---

## `eth_bridgeState` returns two fields, not the Bridge

| | |
|---|---|
| **What the name implies** | the Bridge's state: federation, UTXOs, queues, pegouts |
| **rskj** | exactly two fields — `rskTxsWaitingForSignatures` and `btcBlockchainBestChainHeight` |
| **source** | `co/rsk/peg/BridgeState.java`, `stateToMap()`; `co/rsk/rpc/modules/eth/EthModule.java`, `bridgeState()` |

```java
public Map<String, Object> stateToMap() {
    Map<String, Object> result = new HashMap<>();
    result.put("rskTxsWaitingForSignatures", this.toStringList(rskTxsWaitingForSignatures.keySet()));
    result.put("btcBlockchainBestChainHeight", this.btcBlockchainBestChainHeight);
    return result;
}
```

`BridgeState` *holds* the UTXO set, the federation, the release request queue
and the pegouts waiting for confirmations. `stateToMap()` exposes none of them;
they appear only in `getEncoded()`, which the RPC never calls. The class name is
the trap.

Two further details:

* **No block parameter.** `EthModule.bridgeState()` reads
  `blockchain.getBestBlock()` unconditionally. There is no way to ask about a
  historical block, and an implementation that accepts one would be inventing a
  capability rskj does not have.
* **The hashes carry no `0x` prefix.** `Keccak256.toHexString()` is
  `Hex.toHexString(bytes)` — bare hex, where almost every other hash in
  JSON-RPC is prefixed. A consumer finds this by failing to parse.

---

## `eth_getUncleBy*AndIndex` may return the uncle's transactions

| | |
|---|---|
| **go-ethereum** | always `types.NewBlockWithHeader(uncle)` — empty `transactions`, empty `uncles`, every time |
| **rskj** | the uncle's *stored block* when the node has one, with its real transaction hashes; an empty block synthesised from the header only as a fallback |
| **source** | `org/ethereum/rpc/Web3Impl.java`, `getUncleResultDTO` |

```java
BlockHeader uncleHeader = block.getUncleList().get(uncleIdx);
Block uncle = blockchain.getBlockByHash(uncleHeader.getHash().getBytes());

if (uncle == null) {                     // <- only then is it header-only
    boolean isRskip126Enabled = config.getActivationConfig()
            .isActive(ConsensusRule.RSKIP126, uncleHeader.getNumber());
    uncle = Block.createBlockFromHeader(uncleHeader, isRskip126Enabled);
}

return getBlockResult(uncle, false);
```

An uncle is a real block that lost a race on a competing branch. A node that
downloaded that branch has its body, and rskj hands it back.

**The consequence is that this method is not deterministic across nodes.** Two
honest, fully synced rskj nodes can answer the same call with different
`transactions`, different `size` and a different `totalDifficulty` — zero on
the node that lacks the block, the real cumulative difficulty on the node that
has it — purely because of what each happened to store. Nothing in the response
says which branch produced it.

**Nor is it deterministic on one node over time.** The same node answers
differently at two moments if it acquires the uncle's block in between — which
is what a reorg at the tip does — or prunes it afterwards. So the answer cannot
be cached, and two calls a minute apart are not guaranteed to agree with each
other any more than two nodes are.

**Cost of getting it wrong.** Hard-coding `transactions: []` looks correct and
passes against any node that never saw the competing branch — which, measured
on this node, is *every* uncle: 3,818 uncles across 1,971 blocks below
#5,000,000, none of them stored as a block. The divergence would then appear
only against a node that had reorged, i.e. exactly when someone is
investigating a reorg.

Two smaller notes on the same method:

* **An out-of-range index is `null`, not an error** (`if (uncleIdx >=
  block.getUncleList().size()) return null;`), and so is an unknown block. Only
  a *malformed* index is an error, rejected earlier by `HexIndexParam`.
* **`totalDifficulty` is `0x0` for an unstored uncle** rather than absent:
  `IndexedBlockStore.getTotalDifficultyForHash` returns `ZERO` for a hash it
  does not have.

---

## The `txpool` namespace is rskj's, field for field

`txpool_content`, `txpool_inspect` and `txpool_status` share a grouping
function, and it differs from go-ethereum's in four ways at once. A client
written against geth misreads all three methods.

| | go-ethereum | rskj |
|---|---|---|
| sender key | `0x` + EIP-55 mixed case | **bare hex, no prefix, lowercase** |
| value at a nonce | the transaction object | **an array of transactions** |
| `txpool_status` counts | `hexutil.Uint` — `"0x2"` | **JSON numbers** — `2` |
| `blockHash` of an unmined tx | `null` | **the 32-byte zero hash** |

**source:** `co/rsk/rpc/modules/txpool/TxPoolModuleImpl.java`

```java
// the sender key
senderProps.put(entrySender.getKey().toString(), ...);   // RskAddress.toString() == ByteUtil.toHexString(bytes)

// the value at a nonce
ArrayNode txsNodes = jsonNodeFactory.arrayNode();
for (Transaction tx : entryNonce.getValue()) { txsNodes.add(txSerializer.apply(tx)); }
nonceProps.put(entryNonce.getKey().toString(), txsNodes);

// status
txProps.put(PENDING, jsonNodeFactory.numberNode(transactionPool.getPendingTransactions().size()));
```

**Cost of getting it wrong.** The sender key is the worst of the four, because
it fails *silently and completely*: a consumer looking up `"0xab…"` in a map
keyed `"ab…"` finds nothing for every sender and concludes the pool is empty.
This is the same trap as `eth_bridgeState`'s unprefixed hashes — rskj
distinguishes `toString()` (bare, documented as "a DEBUG representation")
from `toJsonString()` (prefixed), and the txpool module uses the debug one.

### `value` and `gasPrice` are byte dumps, not quantities

In the same object, `gas` and `nonce` go through `toQuantityJsonHex` and are
canonical, while `value` and `gasPrice` go through
`HexUtils.toJsonHex(Coin.getBytes())` — and `Coin.getBytes()` is
`BigInteger.toByteArray()`, which is **two's-complement**:

```java
txNode.put("gas", HexUtils.toQuantityJsonHex(tx.getGasLimitAsInteger()));  // 0x5208
txNode.put("value", HexUtils.toJsonHex(tx.getValue().getBytes()));         // 0x03e8, and 0x00ff for 255
```

So a value of 255 renders as `0x00ff`, a value of 1000 as `0x03e8`, and zero
as `0x00` — the same amounts that `eth_getTransactionByHash` reports as
`0xff`, `0x3e8` and `0x0`. A strict hex-quantity parser rejects the leading
zero; a lenient one is fine. And because `toJsonHex` maps an empty array to
`0x00`, **a contract creation's `to` is the string `0x00`**, not null.

### `txpool_inspect`'s summary string

```java
String.format("%s: %s wei + %d x %s gas",
        tx.getReceiveAddress().toString(), tx.getValue().toString(),
        tx.getGasLimitAsInteger(), tx.getGasPrice().toString());
```

Against geth's `"%s: %v wei + %v gas × %v wei"`: a plain ASCII `x` rather than
`×`, the gas limit and gas price in the opposite order, and the word `gas`
last. The address is bare hex, so a contract creation's summary begins with
`": "`. Consumers parse this string, so the difference is not cosmetic.

### `trace_get` takes an index, not a path

Parity's `trace_get(txHash, positions)` walks `positions` down the call tree.
rskj rejects more than one:

```java
if (tracePositions.size() > 1) {
    throw invalidParamError("'positions' accepts only one index");
}
...
List<TransactionTrace> traces = buildBlockTraces(block);   // every tx in the block
TransactionTrace transactionTrace = traces.get(positions.get(0));
```

and then indexes the **whole block's** flattened trace list. The transaction
hash selects the *block*, not the traces. `trace_get(tx, ["0x0"])` on the
second transaction of a block therefore returns a trace belonging to the
first — silently, with a plausible-looking result. Full detail in
`docs/trace-namespace.md`.

### `trace_filter` addresses match transactions, not traces

```java
txStream = txStream.filter(tx -> addresses.contains(tx.getSender(signatureCache)));
```

Parity matches each individual trace. rskj filters whole transactions by their
own sender and receive address, then emits every trace of the survivors. So an
internal call *to* the address you asked about is invisible unless the
top-level transaction also matched — usually the opposite of the question.

### Precompile calls are absent from `trace_*`

`Program.callToPrecompiledAddress` never calls `addSubTrace`. On RSK that
means **the Bridge does not appear in any call tree**: peg-ins, peg-out
requests and federation calls are invisible to an indexer reading internal
transactions. Bridge events are the only route.

### A failed CREATE is absent too

```java
if (programResult.getException() == null && !programResult.isRevert()) {
    getTrace().addSubTrace(ProgramSubtrace.newCreateSubtrace(...));
}
```

A reverted or halted inner CREATE leaves nothing — no entry, no error, and
none of the frames it opened. Parity shows it with its error. A deployment
that reverted looks exactly like one that never happened. A failed **CALL**
is reported normally; only CREATE is dropped.

### `trace_*` mixes JSON numbers with hex strings

`blockNumber`, `transactionPosition` and `subtraces` come out as JSON numbers
(`42`), because they are Java `long`/`int` fields. Everything else in the
object — `gas`, `value`, `gasUsed`, `input`, `output` — is a `0x` string. A
client that assumes hex everywhere reads `blockNumber` as zero.

### `RskAddress.toJsonString()` returns `null` for the zero address

```java
public String toJsonString() {
    if (NULL_ADDRESS.equals(this)) { return null; }
    return HexUtils.toUnformattedJsonHex(this.getBytes());
}
```

`TraceAction` is `@JsonInclude(NON_NULL)`, so the key vanishes rather than
carrying `"0x0000…0000"`. A consumer indexing by `action.to` must treat an
absent key as the zero address, not as malformed input.

### The `sco_*` namespace has no go-ethereum counterpart

geth exposes peer management through `admin_addPeer` / `admin_removePeer` and
nothing about reputation; its scoring is internal and unqueryable. rskj's six
`sco_*` methods have no equivalent to compare against, so the only reference
is rskj itself. `docs/peer-scoring.md` has the detail; two shapes worth
repeating here:

- `sco_peerList` returns counters, `score`, `punishments` and `punishedUntil`
  as JSON **numbers**, not hex strings — `PeerScoringInformation` is a plain
  bean of `int`s and a `long`. The same trap as `trace_*`.
- `sco_clearPeerScoring` resolves its argument with
  `InetAddress.getByName(id)` **first**, which resolves DNS names, and falls
  back to treating it as a node id. So a hostname argument clears whatever it
  resolves to at that moment. rustock parses only literal addresses and treats
  anything else as a node id.

### `eth_getLogs` skips on the block bloom; rskj skips on a range bloom

Both nodes answer the same logs; they reject non-matching blocks differently.

rskj keeps `co.rsk.logfilter.BlocksBloomStore` — the ORed bloom of a *group*
of blocks, written once the range is confirmed — so a wide query can discard
a whole group with one test. rustock tests each block's own `logs_bloom` from
its header, which is one read per block rather than one per group.

Same answers, different cost curve: rustock is closer to linear in the range,
rskj closer to linear in the number of groups. Not a compatibility difference,
recorded here because someone comparing query latency between the two nodes
will see it and should know why. Issue #122 tracks the grouped index.

### `newHeads` announces blocks that never became the head

`BlockHeaderNotificationEmitter` listens on `onBlock`, and in
`BlockChainImpl.tryToConnect` that fires for `IMPORTED_NOT_BEST` as well as
`IMPORTED_BEST`:

```java
// IMPORTED_NOT_BEST
extendAlternativeBlockChain(block, totalDifficulty);
saveReceipts(block, result);
onBlock(block, result);          // <- newHeads fires here too
```

So an rskj `newHeads` subscriber is told about the losing side of a fork,
which geth never does. rskj's *logs* emitter uses `onBestBlock` and does not
have this, so the two subscriptions on the same node disagree about what a
head is.

rustock follows geth here rather than rskj -- see
`docs/websocket-subscriptions.md` for why, and note the direction: a client
written against rskj sees strictly fewer notifications here, never spurious
ones.

---

## How to add an entry

When implementing anything against rskj, if the Java does not match what the
name or the Ethereum spec implies, add a row here with:

1. what each side does,
2. the rskj file and method, with the lines that matter quoted,
3. **what it costs to get wrong** — the failure a consumer would see.

The third is the part worth writing. A difference nobody can be hurt by is
trivia; a difference that silently mislabels data is the reason this file
exists.
