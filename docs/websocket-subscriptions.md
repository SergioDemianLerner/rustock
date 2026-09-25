# WebSocket JSON-RPC and `eth_subscribe`

Everything the HTTP endpoint answers, the WebSocket endpoint answers too —
the same dispatcher handles both — plus `eth_subscribe` and
`eth_unsubscribe`, which only mean anything on a connection that stays open.

Source of truth: rskj's `co.rsk.rpc.netty.Web3WebSocketServer`,
`RskWebSocketJsonRpcHandler`, and the `co.rsk.rpc.modules.eth.subscribe`
package.

## Turning it on

Off by default, as rskj's `rpc.providers.web.ws.enabled` is, and on its own
port, as rskj's is:

```
--ws --ws-port 4445          # rskj's default port
```

or in the config file:

```toml
[rpc]
ws = true
ws_port = 4445
```

A subscription socket is a resource an operator should opt into: it is the
one transport that lets a client make the node hold state on its behalf.

## What can be subscribed to

| | what arrives |
|---|---|
| `newHeads` | a block header each time a block becomes the executed head |
| `logs` | each matching log, with `removed` saying whether it joined or left |
| `newPendingTransactions` | the hash of each transaction the pool accepts |
| `syncing` | accepted; see the note below |

`logs` takes the same filter object `eth_getLogs` does — `address` as a
string or array, `topics` as a positional array where `null` means "anything
here" and a nested array means "any of these".

## Reorgs, which is the whole design

A subscription is only useful if it tells you when it was wrong.

rskj's `LogsNotificationEmitter` computes the fork between the last block it
emitted and the new best block
(`BlockchainBranchComparator.calculateFork`), then sends the abandoned
branch's logs with `removed: true` before the new branch's with
`removed: false`. rustock does the same, driven by the rollbacks the sync
service already performs: `SyncService::publish_removals` walks the abandoned
branch's own ancestry from the old executed head down to the resume point and
retracts each block, before execution moves.

**This is not a corner case on this chain.** The node observes a tip fork
several times an hour — the I5 repair in `crates/sync/src/invariant.rs` fires
that often — so a subscriber that never heard about a removal would steadily
accumulate logs from blocks that are no longer on the chain.

`a_removed_block_retracts_its_logs` pins the pair: the same log arrives once
with `removed: false` and again with `removed: true`.

## Three deliberate differences from rskj

### `newHeads` does not announce blocks that never became the head

rskj's `BlockHeaderNotificationEmitter` listens on `onBlock`, not
`onBestBlock`. In `BlockChainImpl.tryToConnect` those are different events:

```java
// IMPORTED_BEST
onBestBlock(block, result);
onBlock(block, result);
...
// IMPORTED_NOT_BEST
extendAlternativeBlockChain(block, totalDifficulty);
saveReceipts(block, result);
onBlock(block, result);          // <- fires here too
```

So **rskj sends a `newHeads` notification for blocks that never became the
head**, including the losing side of a fork. Its logs emitter uses
`onBestBlock` and does not have this.

rustock emits `newHeads` only for blocks that became the executed head. That
is geth's behaviour and what the subscription's name promises, and it is also
what rustock can honestly produce: it executes the canonical chain, so a
losing sibling is frequently stored and never executed, and there is no point
at which the node could describe it as a head.

A client written against rskj sees **strictly fewer** notifications here,
never spurious ones. A client that deduplicates by hash, or that treats a head
as authoritative, is unaffected; one that counted on hearing about siblings is
relying on a quirk.

### Subscriptions are fed from the tip, not from bulk sync

`newHeads` and `logs` are published from the follow path only. During
skeleton sync a subscriber would receive thousands of heads it did not ask to
be caught up on, and would trip the lag disconnect below before the node
reached the tip. Subscriptions are a tip feature.

### `syncing` is accepted but never fires

rskj's `SyncNotificationEmitter` reports entering and leaving sync. rustock
accepts the subscription — so a client that opens one is not broken by an
error — and does not yet publish the transitions. Poll `eth_syncing` for now.

## Backpressure

A subscriber that stops reading must not be able to grow the node's memory.
The channel gives each connection a backlog of `SUBSCRIBER_LAG` (512) events;
a connection that falls further behind is **disconnected**, with a warning
naming how many events it missed.

Disconnecting is the honest outcome. A subscriber that silently skipped `n`
events would carry a wrong view of the chain and never know it; one that is
disconnected reconnects and resynchronises from the RPC.

`crates/networking/src/peers.rs` learned this lesson once already, with an
unbounded peer channel that let a single stalled peer drive the node's memory
arbitrarily high. The same rule applies to anyone holding a socket open.

## Where it lives

| | |
|---|---|
| transport, per-connection loop | `crates/rpc/src/ws.rs` |
| subscription kinds, filters, rendering | `crates/rpc/src/subscribe.rs` |
| the event type | `crates/core/src/events.rs` |
| publishing heads and removals | `crates/sync/src/service.rs` (`publish_block`, `publish_removals`) |
| publishing pending transactions | `crates/sync/src/tx_relay.rs` |

The event type is in `core` because the sync service publishes and the RPC
layer consumes, and neither crate depends on the other. The *rendering* stays
in `rustock-rpc`, which is the only place that should know what a subscriber's
wire format looks like.

rskj's equivalent plumbing is the `EthereumListener` fan-out, which rustock
deliberately does not have — one typed channel rather than an interface with
a dozen methods and a dozen implementors.
