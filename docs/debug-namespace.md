# The `debug_*` namespace

rskj's `debug_*` is **not** go-ethereum's. The two share only
`debug_traceTransaction`'s name, and even there the output shape differs. This
records what rustock implements, what it deliberately does not, and the
differences a consumer will trip over.

Source of truth: `co/rsk/rpc/Web3DebugModule.java`,
`co/rsk/rpc/modules/debug/DebugModuleImpl.java` at the revision pinned in
[`rskj-reference.md`](./rskj-reference.md).

## What rskj has

| Method | rustock | Note |
|---|---|---|
| `debug_wireProtocolQueueSize` | **yes** | |
| `debug_accountTransactionQuota` | **yes** | |
| `debug_traceTransaction` | not yet | tracer built; needs block replay (issue #81) |
| `debug_traceBlockByHash` | not yet | ditto |
| `debug_traceBlockByNumber` | not yet | ditto |

go-ethereum's `debug_` namespace has dozens of other methods
(`debug_storageRangeAt`, `debug_getModifiedAccountsByNumber`, `debug_dumpBlock`
…). None of them exists in rskj, so none is a compatibility obligation here.

## `debug_wireProtocolQueueSize`

```java
public String wireProtocolQueueSize() {
    long n = messageHandler.getMessageQueueSize();
    return HexUtils.toQuantityJsonHex(n);
}
```

**Inbound**, not outbound: messages received from peers and not yet handled. It
answers "is the peer-handling loop falling behind", which is why the issue asks
for it alongside a slow-peer diagnosis.

rustock has no single `messageHandler`. The equivalent place inbound wire work
waits is the `SyncEvent` channel between the networking layer and the sync
loop, so its depth is what this reports. The sync loop refreshes the gauge
every time it takes an event, so a reader sees the live backlog rather than a
sample on some other schedule.

**It returns a hex quantity string**, e.g. `"0x1a"` — not a number. In the same
server `txpool_status` answers with JSON *numbers*, because rskj builds those
with `numberNode` and this one with `toQuantityJsonHex`. The inconsistency is
rskj's; both are reproduced rather than harmonised, because a consumer written
against rskj parses each the way rskj emits it.

## `debug_accountTransactionQuota`

```java
public TxQuota accountTransactionQuota(String address) {
    RskAddress rskAddress = new RskAddress(address);
    return txQuotaChecker.getTxQuota(rskAddress);
}
```

A plain map lookup, so **`null` is the normal answer**: an entry exists only
for an address whose transaction the limiter has admitted. Not an error, and
callers should not treat it as one.

`TxQuota` serialises exactly two fields:

| field | type | |
|---|---|---|
| `timestamp` | number | epoch **milliseconds**, when the quota was last refreshed |
| `availableVirtualGas` | **double** | remaining virtual gas |

The double matters. A transaction's virtual-gas cost is the product of six
fractional factors (`VirtualGasCalculator`), so a quota is rarely a whole
number, and the fractional part is exactly what separates one admitted
transaction from the next. Rounding it to an integer would merge quotas the
limiter distinguishes.

### One implementation difference, and why it is invisible

rskj stores the timestamp directly: `TxQuota.timestamp` is set from its
`TimeProvider` on every refresh. rustock's `TxQuota` holds `last_refresh:
Instant`, which is monotonic and deliberately carries no epoch — the right
choice for measuring elapsed time, and useless for reporting a wall-clock
moment.

So the reported timestamp is reconstructed at query time as *now minus the
quota's age*. It is accurate to whatever the system clock drifted over the
quota's lifetime, which is minutes at most, and it avoids carrying a second
timestamp that could disagree with the first. A consumer cannot tell the
difference; a reader of the code should know it is derived rather than stored.

## The tracer

`crates/execution/src/tracer.rs` implements rskj's structure, and
`RskExecutor::execute_tx_traced` runs a transaction through it. Three details
inside `structLogs` are rskj's and are the ones a geth-shaped implementation
gets wrong:

* **`gas` is measured before the opcode runs, `gasCost` after.** rskj calls
  `addOp` with the remaining gas and then `saveGasCost` on the entry it just
  added. Reporting post-execution gas shifts every row by one opcode.
* **Stack and memory are bare hex, not `0x`-prefixed**, and the stack is
  **bottom first** -- `stack[0]` is the deepest item. Memory is split into
  32-byte chunks, the last one short when the size is not a multiple of 32.
* **`storage` lags one opcode, deliberately.** rskj notes the key when it sees
  SSTORE or SLOAD (`storageKey = stack.peek()`) and reads its value on the
  *next* `addOp`, once that opcode has run. A step's `storage` therefore shows
  the effect of the step before it, and the map accumulates the keys touched
  so far rather than describing the contract's whole storage.

### Tracing does not change execution

The traced path is separate from block processing, and the storage read uses
`sload_skip_cold_load` so the tracer does not warm the slot.

That second point is **defensive, not load-bearing**, and the difference is
worth knowing. On Ethereum a tracer calling plain `sload` would warm the slot
and change EIP-2929 gas for everything after it. RSK has no EIP-2929 --
`make_cfg_env` zeroes `cold_storage_cost`, `warm_storage_read_cost` and the
rest, pinned by `test_cfg_env_no_eip2929_cold_access_cost` -- so here that
mistake would cost nothing. Verified by flipping the skip off: the equivalence
test still passes. The skip stays because not depending on that is free.

So `tracing_does_not_change_execution` does not prove side-effect freedom; it
proves the traced and untraced paths agree on gas, output, success and logs,
which is what catches them drifting apart. That is the real risk, because
`execute_tx_traced` duplicates the untraced setup rather than refactoring it.

### What is not built yet

`debug_traceTransaction` and the block variants need to replay a transaction
in its block's context: the state before transaction *i* of block *N* is the
state after block *N-1* plus transactions 0..*i*, and no intermediate root is
stored. That means re-executing the block with the tracer attached at one
index, and `RskExecutor::execute_block` is 484 lines of consensus path --
parameterising it over an inspector is its own change, with its own replay
validation, not a tail-end addition to this one.

Two constraints on those methods are already settled:

* **The output shape is rskj's `DetailedProgramTrace`** — `contractAddress`,
  `initStorage`, `structLogs`, `result`, `error`, `reverted`, `storageSize`,
  `currentStorage` — not go-ethereum's `{gas, failed, returnValue,
  structLogs}`. Consumers are written against rskj.
* **Only recent blocks can be traced.** Tracing re-executes a transaction
  against the state at its parent block, and this node keeps state for roughly
  the GC burial depth (4,000 blocks by default). Older transactions are not
  traceable without re-deriving state, and the method should say so rather than
  answer from the wrong state.
