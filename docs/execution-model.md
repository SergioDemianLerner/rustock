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
