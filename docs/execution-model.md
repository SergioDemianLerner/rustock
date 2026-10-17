# Execution model: rustock vs rskj

Where the two clients differ **structurally** in how a block is executed — not
in a rule, but in the shape of the machinery. Most of it is invisible to
consensus. The parts that are not are the interesting ones, and they are listed
here because each was found by a block that diverged rather than by reading the
spec.

Companion to the compatibility catalogue indexed in
[`java-compatibility.md`](./java-compatibility.md): that catalogue is about
*rules*, this is about *architecture*.

## 1. When the trie is written

**Both clients accumulate a whole block in memory and write the trie once.**
This is worth stating because the layering looks so different that it is easy to
assume otherwise.

| | rskj | rustock |
|---|---|---|
| per-call-frame isolation | nested `Repository` tracks | revm journal checkpoints |
| per-transaction boundary | `txSubTrack.commit()` → cache | none |
| block accumulation | `MutableTrieCache` (an in-memory map) | `EvmState` from `evm.finalize()` |
| trie materialised | `track.save()`, block end | `apply_state_changes`, block end |

rskj (`co.rsk.core.bc.BlockExecutor`):

```java
420:  Repository track = repositoryLocator.startTrackingAt(parent);   // one per block
752:  Repository txSubTrack = track.startTracking();                  // one per transaction
787:  txSubTrack.commit();                                            // after each transaction
916:  track.save();                                                   // once, at block end
```

`txSubTrack.commit()` merges into the block-level `track`, which is a
`MutableTrieCache` holding
`Map<ByteArrayWrapper, Map<ByteArrayWrapper, Optional<byte[]>>>` in memory
(`MutableTrieCache.java:44`). Nothing reaches the trie store until `save()`.

rustock: revm holds one journal for the whole block; each transaction commits
into it; `evm.finalize()` collapses it to an `EvmState`, and
`processor.rs` calls `apply_state_changes` exactly once.

**A `REVERT` therefore never touches the trie.** There is nothing to roll back
at the storage layer — the reverted writes are simply absent from the
`EvmState` that reaches `apply_state_changes`. Gas burned by a reverted call
does survive, because it moves balances, and balances are state.

## 2. The per-transaction boundary is consensus-visible

rustock has no per-transaction commit, and that is fine except for one
consequence, which it has to manufacture.

rskj's `TransactionExecutor.finalization` deletes self-destructed accounts at
the end of **each transaction** (`track.delete`). A later transaction in the
same block therefore sees the address as nonexistent and re-creates a fresh
`(0, 0)` account merely by calling it — frontier `transfer` / `addBalance(0)`
creates the record.

revm keeps one journal for the block and wipes a destroyed account lazily, on
the next cold load, leaving sticky `SelfDestructed` and `Touched` flags. At
block end that state cannot distinguish "destroyed" from "destroyed, then
re-created by a later transaction".

rustock closes the gap in `executor.rs` (the loop after each transaction
commits): wipe info and storage, clear `Touched`, clear the local
selfdestruct/created marks. A later transaction touching the address re-marks
`Touched`, which `apply_state_changes` reads as "alive again at block end".

Found by **mainnet #3,173,807**. Regression tests:

- `executor.rs::selfdestruct_then_recreate_in_same_block` — the exact shape:
  destroy, re-create by calling, then top up; asserts the old subtree (code,
  marker, storage) is gone and the fresh account has nonce 0 and only the later
  balance.
- `executor.rs::selfdestruct_in_last_tx_deletes_account_entirely`
- `executor.rs::selfdestruct_to_self_burns_balance`
- `executor.rs::selfdestruct_recreate_via_create2_then_selfdestruct_again`
- `state.rs::test_selfdestructed_then_recreated_account_rewritten_fresh` — the
  same case seen from `apply_state_changes`, where the account arrives both
  `SelfDestructed` and `Touched`.

## 3. Writes revm cannot express: the raw-storage overlay

Some rskj writes do not correspond to any EVM operation — they are direct
`addStorageBytes` calls from Java. revm has no way to represent them, so
rustock carries a second channel: `evm.ctx.chain_mut().raw_storage`, with its
own `begin_tx()` per transaction, drained into `markers.raw_storage` at block
end and applied alongside everything else.

The case that forced it is REMASC's `siblings` cell —
`RemascStorageProvider.saveSiblings()` runs on every REMASC `save()` with no
guard, writing a single `0xc0` byte. The cell is created at block #1, never
changes value, and is part of the state root from then on
([`quirks-frozen-bugs.md`](./quirks-frozen-bugs.md) §9c).

The overlay follows the same rule as everything else: accumulated in memory,
written once at block end.

## 4. Receipts carry no post-transaction state root

RSK is Byzantium from genesis (`hardfork.rs`: `genesis–orchid → BYZANTIUM`), so
`postTxState` in a receipt is the 1-byte status indicator — `0x01` for success,
empty for failure — and never a 32-byte root.

This is load-bearing for §1: a pre-Byzantium chain would need the state root
*after every transaction*, which would force a per-transaction trie
materialisation on both clients. RSK never does, which is why accumulating the
whole block is viable at all.

## 5. What this means for verification

`BlockProcessor::execute_block` validates nothing; `process_block` validates
transactions root, ommers hash, gas used, state root, receipts root and logs
bloom. A tool that calls the former and compares only the state root proves the
state transition and nothing else — see
[`trie-segments-design.md`](./trie-segments-design.md) §8 for a case where
exactly that distinction mattered.

## 6. Sublists run in sequence, and their independence is checked

RSKIP-144 splits a block's transactions into sublists that rskj executes **in
parallel**, one thread each, plus a sequential set that runs afterwards on the
main thread. The split is declared by the header's `txExecutionSublistsEdges`.
rustock executes every transaction in order, on one thread.

That is a performance difference and not a consensus one — but only because of
a rule that is easy to miss, and that rustock did not implement until
[#305](https://github.com/SergioDemianLerner/rustock/issues/305). The sublists
must be **independent**, and the validator has to establish it:

> - If a key belongs to a `writeMap` and a `readMap` of another parallel
>   sublist, then the block is considered invalid.
> - If a key belongs to a `writeMap` and a `writeMap` of another parallel
>   sublist, then the block is also invalid.

The tempting shortcut — "we execute sequentially, so we get a valid answer
either way" — is wrong twice over. A sequential executor computes *one* of the
interleavings; where sublists conflict, that interleaving is the parallel
result sometimes and not others, so the two clients agree by luck. And the
rule is a condition on the *block*, not a property a validator may assume the
miner established: a block whose sublists conflict is invalid, and a node that
skips the check accepts it, computes a state root nobody else reaches, and
forks.

Read against read is not a collision. Only a write makes an ordering
observable.

### How the sets are built without parallel execution

`collision.rs` holds a `CollisionTracker` keyed by sublist index — rskj keys
its `ReadWrittenKeysTracker` by thread id, which is the same thing said with
reference to how execution happens to be scheduled. `processor.rs` starts it
only for a block that actually splits, so a chain where RSKIP-144 is
unscheduled (mainnet) or active-but-unused pays nothing.

rskj records keys as they pass through its repository (`internalGet` a read,
`internalPut` a write). rustock has no such chokepoint: revm's journal caches
across the whole block, so a database-level hook would miss every access an
earlier transaction had already warmed. The sets are therefore read **out of
the journal after each transaction**, using the per-transaction stamp revm
keeps on every account and slot (`transaction_id`, updated on first access,
including a read — `JournalInner::load_account_mut_optional`). An entry
carrying this transaction's id was accessed by it; whether it was *written* is
`info` against `original_info`, the baseline revm resets at each transaction's
first touch.

The Bridge's byte-array cells bypass the journal entirely (§3), so they are
read separately out of `RawStorage::tx_writes`.

### What is deliberately not recorded

Two addresses are excluded, because rustock writes them from inside a
transaction and rskj does not:

- **REMASC.** rustock credits the fee beneficiary per transaction; rskj builds
  every sublist executor with `postponeFeePayment = true`
  (`TransactionListExecutor`), accumulating fees per sublist and settling them
  afterwards. Without the exclusion every sublist writes REMASC's account cell
  and *every* split block collides — which is what happened: testnet
  #7,690,880 onwards were all rejected until the exclusion was right.
- **Precompiles maintained at an activation block.** rskj's
  `maintainPrecompiledContractStorageRoots` runs on the block's repository
  before any sublist starts, so those writes belong to no sublist.

The sequential set is not recorded either, which is not an omission:
`BlockExecutor.executeParallel` calls `detectCollision()` *before* it runs,
and the sequential set is exactly where a miner puts the transactions that do
conflict.

### Known gaps, all in the safe direction

Our key sets are a **subset** of rskj's, so we can fail to reject a block rskj
rejects, but never reject one rskj accepts. That asymmetry is deliberate: the
first failure leaves the node where it was before the check existed, the
second forks it off a chain everyone else considers valid.

- **A reverted write reads as a read.** revm has undone it by the time the
  transaction ends. rskj's tracker is monotonic — `startTracking` hands the
  *same* tracker to every nested repository, so a rolled-back write stays in
  the set.
- **Reads of Bridge byte-array state are not recorded**, only writes: the
  raw-storage overlay's read path has no hook.
- **Code and storage-prefix cells** are recorded as writes only for a contract
  that deployed non-empty code, matching what `apply_state_changes` writes.

### Verification

Testnet is the only chain where this fires: reed810 is unscheduled on mainnet,
so no mainnet header has ever carried a non-empty edges field. Of 1,020,420
canonical testnet headers above the activation height, 203,419 split — the
first at #7,690,876, and the widest into two parallel sublists plus the
sequential set, so the check always compares exactly one pair.

Five ranges of 301 blocks, spread from the first splitting block to the
chain head, replay against rskj's own state with **zero divergences and zero
collisions**: 1,505 blocks, of which 685 split.

| range | blocks | of which split |
|---|---|---|
| #7,690,870–#7,691,170 | 301 | 165 |
| #7,800,000–#7,800,300 | 301 | 153 |
| #7,950,000–#7,950,300 | 301 | 137 |
| #8,050,000–#8,050,300 | 301 | 151 |
| #8,159,700–#8,160,000 | 301 | 79 |

This matters more than the unit tests do. The REMASC exclusion above was found
here and nowhere else: every unit test passed while every splitting block on
the chain was being rejected.
