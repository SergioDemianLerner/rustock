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
| `debug_traceTransaction` | **yes** | only within the state-retention window |
| `debug_traceBlockByHash` | **yes** | ditto |
| `debug_traceBlockByNumber` | **yes** | ditto |

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

### How a historical transaction is traced

A trace needs the state **before** the transaction, and no such thing is
stored: the state before transaction *i* of block *N* is the state after *N-1*
plus transactions 0..*i*, and only per-block roots are kept. So the whole block
is re-executed from its parent's state with the tracer switched on at one
index.

`RskExecutor::execute_block_with` takes the inspector as a parameter and
`execute_block` passes a tracer with a zero cap and `trace_index: None`.
**That is not a behavioural change**, and the reason is worth stating rather
than asserting. In revm-handler 17's `mainnet_builder.rs`, `build_mainnet` and
`build_mainnet_with_inspector` construct the same `Evm` from the same spec --
identical `instruction`, `precompiles` and `frame_stack` -- differing only in
the `inspector` field, which the first sets to `()`. And
`impl<CTX, INSP, I, P> EvmTr for Evm<…>` is generic over that field, so
`Handler::run` on an inspector-carrying EVM runs the same code as on one
without. The inspector is consulted only by `inspect_run`, which is called for
exactly one transaction index and never on the consensus path.

Sharing one body rather than copying it was deliberate. The alternative was a
second 484-line block executor, and two copies of the consensus block loop
would drift -- the failure this codebase has already had, in the two
orphan-recovery paths that were meant to stay identical and did not.
`tracing_a_block_records_the_transaction_and_changes_nothing` asserts the
traced and untraced runs agree on gas, fees and every per-transaction result.

### Only recent blocks can be traced

The parent's state has to still be in the trie, which for this node means
roughly the GC burial depth (4,000 blocks by default). Past that,
`debug_traceTransaction` returns an error naming the block it could not reach,
rather than an answer computed from a state that is not the right one.

One further note:

* **The output shape is rskj's `DetailedProgramTrace`** — `contractAddress`,
  `initStorage`, `structLogs`, `result`, `error`, `reverted`, `storageSize`,
  `currentStorage` — not go-ethereum's `{gas, failed, returnValue,
  structLogs}`. Consumers are written against rskj.
* `truncated` is **not an rskj field**. rskj has no step cap and so never has
  to say it hit one; a consumer here must be able to tell a truncated trace
  from a short one, so the flag is added rather than the cap hidden.

### Three places the output is knowingly not rskj's

These are differences, not oversights, and a consumer comparing two nodes will
see them.

**`error` is not a Java class name.** rskj builds it as
`format("%s: %s", error.getClass(), error.getMessage())`, so a real trace says
`class org.ethereum.vm.program.Program$OutOfGasException: ...`. There is no
such class here and printing one would be a lie about what ran; the revm halt
reason is reported instead. A consumer that *parses* this string will not work
against both nodes -- but one that only shows it to a human reads the same
fact.

**`reverted` and `error` stay distinct.** rskj sets `reverted` for an actual
REVERT and leaves `error` empty; a halt (out of gas, invalid opcode, stack
underflow) goes in `error` with `reverted` false. That distinction is kept
here, which is why the executor fills these fields from the transaction's own
`ExecutionResult` rather than from `tx_results[i].success` -- `success` is a
single boolean and cannot tell the two apart.
`a_trace_reports_revert_and_halt_differently` pins both directions.

**`initStorage` is not produced, and `storageSize` counts what the trace
touched.** rskj reads the contract's *entire* storage before the program runs
-- `getStorageKeysCount`, then every key up to `vmTraceInitStorageLimit` --
and reports the count as `storageSize` and the contents as `initStorage`. That
needs an enumeration of the account's storage in the unitrie at the moment the
transaction starts, which is mid-block state the inspector has no route to:
revm's journal holds the dirty overlay, the trie holds the parent block, and
neither alone is the pre-transaction storage. `storageSize` here is therefore
the size of `currentStorage` -- the keys the trace observed -- and
`initStorage` is absent. Filed as a follow-up rather than approximated
silently.
