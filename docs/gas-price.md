# Gas price: what to pay to be mined

Two different numbers get called "the gas price" and the node used to confuse
them.

**`minimumGasPrice`** is a header field: the floor a transaction must clear to
be *valid* in that block. Below it the transaction is rejected, full stop.

**The suggested gas price** is what a transaction should pay to be *mined* in
reasonable time. On a congested chain it is well above the floor, and on an
empty one it is the floor.

`eth_gasPrice` answered with the first. During congestion a wallet trusting it
underpays and its transaction sits. rskj answers with the second, computed by
`org.ethereum.listener.GasPriceTracker`, which this ports.

## The algorithm

| constant | value | rskj name |
|---|---|---|
| transactions sampled | 512 | `TX_WINDOW_SIZE` |
| blocks sampled for fullness | 50 | `BLOCK_WINDOW_SIZE` |
| "fee market working" threshold | 90% average fullness | `BLOCK_COMPLETION_PERCENT_FOR_FEE_MARKET_WORKING` |
| floor multiplier | 1.1 | `DEFAULT_GAS_PRICE_MULTIPLIER` |

```
gasPrice = max( percentile(last 512 transaction gas prices),
                bestBlock.minimumGasPrice × 1.1 )
```

falling back to the last block's `minimumGasPrice` while the transaction
window is not yet full.

## Four details that are rskj's, not obvious

**It is the 25th percentile, not the median.** `values[values.length / 4]` of
the sorted window — the lower quartile. A deliberately modest suggestion,
which is why the `× 1.1` floor matters as much as the percentile does.

**It is recalculated only when the window wraps.** rskj caches the sorted
result in `lastVal` and clears it only when the ring index passes zero, with
the comment `// recalculate only 'sometimes'`. So the suggestion lags the
market by up to 512 transactions. Recomputing per call would be more accurate
and would not match rskj, and a consumer comparing two nodes would see the
difference.

**The transaction ring fills backwards**, index 511 down to 0, and "is it
primed?" is `txWindow[0] == null`. The calculator therefore returns *nothing*
until it has seen a full 512 transactions, however many blocks that takes —
there is no partial answer from a half-full window.

**REMASC is excluded from both windows.** It is the synthetic transaction
appended to every RSK block, carries no gas price, and would drag the
percentile toward zero. rskj filters it with `instanceof RemascTransaction`;
here it is `BlockProcessor::is_remasc_tx`, the same predicate the executor
uses.

## A start-up quirk, reproduced deliberately

rskj primes both windows from the block store so a restarted node is not blind
for 50 blocks (`initializeWindowsFromDB`). It walks back from the head, then:

```java
Collections.reverse(blocks);        // oldest first
...
onBestBlock(blocks.get(0), emptyList());   // <- the OLDEST block
blocks.forEach(b -> onBlock(b, emptyList()));
```

`blocks.get(0)` after the reverse is the **oldest** block of the window, so
the "best block price" recorded at start-up is taken from a block up to 50
heights stale. The `× 1.1` floor is computed from that until the next real
block arrives — about half a minute on mainnet.

rustock reproduces it. `eth_gasPrice` is observably different in that window,
and matching rskj is the point of the exercise; a node that quietly "fixed"
this would disagree with every rskj node for the first thirty seconds after
every restart.

## The other consumer: the rate limiter

`isFeeMarketWorking()` — the 50-block window full **and** average fullness at
or above 90% — gates the sixth cost factor in `VirtualGasCalculator`:

```
lowGasPriceFactor = 1 + 3 × (avgGasPrice − txGasPrice) / (avgGasPrice − blockMinGasPrice)
```

ranging 1 (priced at or above the average) to 4 (priced at the floor). Without
a tracker the node took rskj's own `createSkippingGasPriceFactor` branch,
pinning it to 1 — so a transaction priced at the floor cost **four times less
virtual gas** than it should, and an attacker got roughly four times more
replacement-flood attempts from the same quota.

That matters more than the multiplier suggests. A cheap transaction is one the
sender is not in a hurry to have mined, so it occupies the pool longer; and
someone flooding by broadcast-then-replace does not want their transactions
mined *at all*, so pricing at the floor is their optimal play. It is the
factor aimed most directly at the abuse the limiter exists for.

Note that on a chain with spare capacity rskj *also* skips this factor much of
the time, because `isFeeMarketWorking()` is false whenever blocks are under
90% full on average. That is a real rskj path, not a degraded one. The
difference this closes is that rskj recovers the factor when blocks fill up,
and rustock previously never did.

## Where it lives

`crates/sync/src/gas_price.rs`, fed by both execution paths in the sync
service — the follow path and the batch path — so every block this node
executes reaches it. rskj feeds its tracker from an `EthereumListener` on the
same events; calling it from the execution paths keeps "a block was executed"
and "the tracker saw it" in one place rather than two that can drift.

The pool and the RPC layer share **one** tracker instance, because the
limiter's view of the market and `eth_gasPrice`'s must not disagree.
