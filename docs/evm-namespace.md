# The `evm_*` namespace

Seven methods that let a test suite drive a development chain: snapshot before
a test, run it, revert; mine on demand instead of waiting; move the clock to
exercise time-dependent logic.

Ported from rskj's `EvmModuleImpl` and `SnapshotManager`, because a contract
test suite written against a regtest rskj node asks for these by name and fails
on the first call if they are absent.

## Off by default, unlike rskj

rskj ships the namespace **enabled** (`reference.conf`: `evm { enabled: "true" }`).
This node requires `--dev-rpc`, or `[rpc] dev = true`.

Every method here rewrites or extends the chain on request. A node that can be
told to discard its own history over RPC has no business being reachable from a
network, whatever the default elsewhere. With the flag off the methods are
reported as unknown rather than forbidden, so a node without them looks the
same as one that never had them.

## The methods

| method | | |
|---|---|---|
| `evm_snapshot()` | → id | record the current height |
| `evm_revert(id)` | → bool | put the head back to that height |
| `evm_reset()` | → bool | back to genesis, forget every snapshot and the clock |
| `evm_mine()` | → bool | mine one block now |
| `evm_increaseTime(s)` | → offset | move the clock future blocks will carry |
| `evm_startMining()` / `evm_stopMining()` | → bool | accepted; see below |

## A snapshot is a height, not a state copy

This is the thing worth knowing, and it is not what the name suggests.

rskj's `SnapshotManager.takeSnapshot` records
`blockchain.getBestBlock().getNumber()` and returns the size of the list. That
is the whole of it. Reverting sets the head back to that height and drops the
canonical index above it.

Two consequences:

- **Taking one is free**, and reverting is a head move rather than a state
  restoration. There is no copying and nothing to garbage-collect.
- **A snapshot is only as good as the state still being on disk at that
  height.** On a development chain nothing has been collected, so that always
  holds. It would not hold on a long-running node past the GC burial depth —
  another reason the namespace is not for one.

### Ids are not stable across a revert

Ids are 1-based indices into a list that is **truncated** on revert: reverting
to 2 discards snapshots 3 and above, so a later `evm_revert(3)` answers
`false`. That is rskj's behaviour and is reproduced rather than improved,
because a suite written against rskj relies on it.

An unknown id answers `false` rather than erroring, and reverting to a height
at or above the current head answers `true` having done nothing — both also
rskj's.

## Mining on demand

An RSK block needs merged-mining proof of work, so `evm_mine` cannot simply
append a block. It does what rskj's `MinerClientImpl` does: build the work,
construct a Bitcoin block committing to it, and brute-force the nonce until the
hash clears the target.

On a development chain the target is easy and a solution lands almost
immediately. The search is bounded (2^18 nonces across each of 16 extra-nonces)
and gives up with an error rather than spinning — which is also why this cannot
be turned against a real chain's difficulty.

`evm_mine` needs the node started with `--mine`, and says so if it was not.

`evm_startMining` and `evm_stopMining` are accepted and do nothing. rskj starts
and stops a background miner thread; this node mines only when asked, so there
is no loop to control. Suites call them for symmetry and do not inspect the
result.

## The clock

`evm_increaseTime(seconds)` adds to an offset that `timestamp_for_child` reads
when stamping a new block. It does not touch the system clock and does not
restamp blocks already mined. The offset is process-wide — the block template
is built in several places, and threading a clock through all of them to serve
a development-only feature would cost more than it is worth.

`evm_reset` clears it, so one test's clock cannot leak into the next.

## What this is not for

A node on mainnet or testnet. The flag exists to make that a deliberate act
rather than a default.
