# Parallel sublist execution: what is shared, and what had to change

RSKIP-144 splits a block's transactions into sublists that may execute in
parallel. rustock executed them in order on one thread; running them on
several means every piece of state the execution path touches has to be
accounted for.

This is that accounting. It is written as a list of *obligations* rather than
reassurances, because the dangerous entries are the ones that look fine: a
thread-local holding consensus configuration reads perfectly well on the
thread that set it.

Companion to [`execution-model.md`](./execution-model.md) §6, which covers the
collision rule itself.

## The rule that makes the rest work

> **Everything that configures an EVM must run on the thread that will use
> it.**

Each parallel sublist builds its own database handle, journal, instruction
table, precompile provider and overlay, on its own worker thread, from
immutable inputs it shares with the others. Nothing is built on one thread and
executed on another. Several entries below are safe *only* because of this,
and each says so.

## 1. Thread-locals carrying consensus configuration

**Severity: would have been a silent consensus divergence.**

`rsk_instructions` keeps four pieces of per-block configuration in
thread-locals, written by `install()`:

| thread-local | if unset on the executing thread |
|---|---|
| `REAL_COINBASE` | the `COINBASE` opcode pushes `0x0` instead of the miner |
| `EXTCODESIZE_MAX_PRECOMPILES` | RSKIP90 is off: `EXTCODESIZE` of a precompile reports its real size, not `2^256-1` |
| `EXTCODEHASH_PRECOMPILES` | `EXTCODEHASH` of a precompile reports the wrong hash |
| tracer dispatch flags | the all-ops tracer dispatches to the wrong opcode bodies |

The original comment said "Set by `install` for the block being executed
(single-threaded)", which was true and is exactly the kind of assumption that
stops being true without anything failing to compile. Had `install()` kept
running on the coordinating thread, every worker would have executed with
empty lists and a zero coinbase, and the first block containing a `COINBASE`
opcode in a parallel sublist would have produced a state root no other node
reaches.

**Resolution:** `install()` is called per sublist, on the worker thread, as
part of the rule above. A debug assertion in `install` records the thread it
ran on and the opcode bodies check it, so a future caller that configures on
one thread and executes on another trips a test rather than a chain.

## 2. Thread-locals carrying diagnostics

`silent::{SINK, POSITION}` and `collision::RECORDER` are also per-thread. They
are not consensus state, but a recorder that silently collects nothing is
worse than one that is off.

**Resolution:** both are started on the worker thread and their contents are
handed back with the sublist's result, then merged by the coordinator.

For the collision tracker this is not a workaround but the natural shape:
rskj keys its `ReadWrittenKeysTracker` by thread id for the same reason. Each
sublist accumulates its own key sets and the coordinator compares them, which
is what `detectCollision` does.

## 3. Shared mutable slots in the precompile provider

`RskPrecompileProvider` hands out three `Arc`-shared slots:

- `bridge_tx_context_slot` — `Mutex<BridgeTxContext>`, rewritten before every
  transaction;
- `invisible_exception_flag` — `AtomicBool`, set by the Bridge and read by the
  handler for the *current* transaction;
- `called_precompiles_slot` — `Mutex<Vec<Address>>`, drained after each
  transaction.

All three are `Sync`, so sharing them compiles. All three would also be
**logically** wrong to share: they carry per-transaction state, and two
sublists interleaving would read each other's. The `AtomicBool` is the worst
of them, because it is the one that looks most obviously thread-safe.

**Resolution:** one provider per sublist, built on its worker thread. The
slots are never shared across sublists.

## 4. The database handle

`RskDatabase` holds `code_cache: RefCell<HashMap<B256, Bytecode>>` and so is
not `Sync`. This is the one entry the compiler catches on its own.

**Resolution:** one `RskDatabase` per sublist. Construction is a root clone
and two `Arc` clones, so this costs nothing; the caches are per-sublist and
are discarded with them.

## 5. The trie

- `TrieStore` is declared `Send + Sync`, and the production implementations
  hold their mutable state behind a guard: `CachedTrieStore` is
  `Mutex<Inner>` around the batch, pending map and LRU; `EpochTrieStore` is
  `RwLock<Vec<Epoch>>` plus atomics, with a `collecting` flag that refuses a
  second concurrent cycle rather than queueing it.
- `TrieNode` is a persistent structure: `put` and `delete` return a new root
  and never mutate the one they were given, so sublists building their own
  roots from a shared parent cannot interfere. The nodes they write go into
  the shared store, which is guarded as above, and a node's key is its hash,
  so two sublists writing the same node write the same bytes.

`TrieNode: Send + Sync` is asserted at compile time in `node.rs`. It holds
today by construction (`Box`, `Vec`, `B256`), and the assertion is there so
that introducing an `Rc` anywhere beneath it fails the build instead of
failing to parallelise.

## 6. Global caches

- `federation::address_cache` — `RwLock<HashMap>` behind a `OnceLock`, and the
  value is a pure function of the key, so a concurrent derive-and-insert
  produces the same entry whoever wins. A poisoned lock falls through to the
  derivation rather than failing.
- `BtcBlockCache` — `Mutex<Lru>` with `AtomicU64` statistics. Shared across
  blocks by design.
- `BRIDGE_METHODS` / `BRIDGE_SELECTOR_MAP` — `LazyLock`, written once and read
  thereafter.

None of these change a result; they only change how long it takes to get it.

## 7. Flags read during execution

`supply::{CHECK_PER_TRANSACTION, CHECK_PER_BLOCK}` and
`rskj_sender_compat::ENABLED` are `AtomicBool`s set at startup and read during
execution. Read-only on the execution path, so no obligation beyond being
atomic, which they are.

## 8. Metrics

`Counter`, `Gauge` and `TimeCounter` are `AtomicU64`. Several sublists
incrementing the same counter is exactly what they are for.

Note that per-block timings measured around a parallel region become wall
clock rather than CPU time, which is the point, but it means
`EXECUTION_SECONDS` stops being comparable across a change to the thread
count.

## The shape, as built

`RskExecutor::run_sublist` executes one contiguous run of a block's
transactions against a state, in the block's own transaction indices. That is
the whole block when it declares no sublists -- every block on a chain where
RSKIP-144 is unscheduled -- and one call per sublist when it does.

A block runs as one unless **all** of these hold: more than one execution
thread, at least two parallel sublists declared, and RSKIP-144 active. The
last is not redundant with the second. Running the sublists separately is
sound *because the fees are postponed*: credited per transaction, every
sublist would add its own fees to REMASC's balance as read from the state
they all start from, and committing them in order would keep the last rather
than the sum. A unit test found this by setting edges on a configuration
where RSKIP-144 was off, and the block quietly lost a sublist's fees.

When a block does split:

1. The block-level precompile maintenance runs first, alone, and its result
   is the state every parallel sublist reads. rskj runs it on the block's own
   repository before any sublist starts; giving it to one sublist instead
   would hide it from the others.
2. Each parallel sublist runs on its own thread, against that state.
3. Their changes go into the trie as each sublist's own commit, in sublist
   order -- which is what rskj does, and what makes the sequential sublist's
   starting state well defined. **The trie is the merge**: there is no
   reconciliation of several journals into one, which is where this would
   otherwise have become delicate.
4. The sequential sublist runs on what they left, carrying their fees so that
   the settlement at the REMASC transaction covers the whole block.

`BlockExecutionResult::applied_root` carries the result, so the caller applies
nothing. It is `None` for a block that ran as one, where the caller applies
the state changes as it always has.

## What is deliberately still serial

- **The sequential sublist**, by RSKIP-144: it runs after the parallel ones,
  on the merged state.
- **Merging the sublists' results into the trie**, in sublist order. rskj does
  the same (`tle.getRepository().commit()` in order), and the order is part of
  consensus: it decides which sublist's value survives when the collision
  check has already established that no two sublists wrote the same cell, but
  the account cell of a shared participant may still differ.
- **Sender recovery**, which already happens before execution begins.

## How this is verified

Thread-safety is not something a passing test proves. What is checked:

- the compile-time assertions above;
- **3,812 block verifications on testnet**: six ranges spanning the chain
  from the reed810 activation to the head, each replayed at one thread and at
  two, every block compared against rskj's own state. Zero divergences. Both
  thread counts are compared against the reference rather than against each
  other, because a shared mistake would agree with itself;
- a block with two parallel sublists executed at one thread and at two,
  asserting the same state root, gas, fees, per-transaction results and
  per-account balances;
- a block whose sublists share a sender -- the race RSKIP-144 exists to
  forbid -- caught as a write-write collision on the account cell when the
  sublists really do run apart. Both transactions succeed, each seeing nonce
  0 against the state the block starts from, which is why the rule has to be
  a condition on the block rather than something execution would notice;
- a block with no sublists running as one whatever the thread count.

A sublist that fails still hands back what it recorded before the failure
propagates. A diagnostic dropped on the error path is one missing exactly
when it is wanted.
