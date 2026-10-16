# Metrics

The node exposes Prometheus metrics when asked to, and not otherwise:

```
rustock-cli --metrics-port 9898 [--metrics-host 127.0.0.1]
```

or in the config file:

```toml
[metrics]
port = 9898
host = "127.0.0.1"
```

Unset means no listener is opened at all.

## Why its own listener

A scrape is not an RPC call. The RPC port is usually bound to localhost or
put behind something, while metrics are usually scraped from elsewhere on a
private network, and the two should not have to share a reachability
decision. Keeping them apart also means a scrape cannot contend with real
RPC traffic, and that a node serving no RPC can still be monitored.

`GET /` answers with a line naming the real path, because that is how a
mistyped scrape target is usually diagnosed.

## What is exported

| metric | type | what it says |
|---|---|---|
| `rustock_blocks_executed_total` | counter | blocks executed |
| `rustock_gas_used_total` | counter | gas executed — the work rate |
| `rustock_transactions_executed_total` | counter | transactions executed |
| `rustock_execution_seconds_total` | counter | time inside block execution |
| `rustock_bodies_requested_total` | counter | bodies asked of a peer |
| `rustock_bodies_derived_total` | counter | bodies rebuilt from their header |
| `rustock_bodies_received_total` | counter | bodies delivered by a peer |
| `rustock_headers_received_total` | counter | headers delivered by a peer |
| `rustock_peers` | gauge | connected peers |
| `rustock_sync_height` | gauge | height this node holds |
| `rustock_sync_target_height` | gauge | best height a peer has announced |
| `rustock_body_requests_in_flight` | gauge | requests sent and unanswered |

**Quote gas, not blocks, when comparing rates.** Blocks get much heavier with
height, so blocks per second overstates early progress and understates late
progress; two runs over different ranges are not comparable in those units.

`rustock_bodies_derived_total` against `rustock_bodies_requested_total` is
the share of bodies that cost no round trip. Around a fifth of mainnet
qualifies.

## Design

Counters and gauges are `static`s rather than entries in a name-keyed
registry, because they are updated from the sync loop and the block executor:
a registry would put a lock or a hash lookup on every update, and the set of
metrics is known at compile time anyway. Updates are `Relaxed` atomics —
these are read by a scrape, not used to synchronise threads.

Gauges are sampled on the sync tick rather than written wherever they change.
A gauge is a level, not an event: a scrape wants its value now, and the tick
is the one place that reliably has all of them without adding a write to a
hot path.

The exposition is hand-written. What is needed here is counters and gauges;
histograms and label cardinality are most of what a metrics facade and an
exporter would carry, and they would bring a dependency tree into a workspace
that checks its dependencies, to produce a text format of four line shapes.

## What is deliberately absent

There is no storage-time metric. There is no point in the node where
committing to the store is cleanly separable from executing — the trie writes
happen inside `execute_block` and so are already counted in
`rustock_execution_seconds_total`. A declared metric that is always zero is
worse than an absent one, because a dashboard shows it as a real measurement
of nothing.

That is also the main thing still missing. These metrics can say how long
execution took and how many bodies arrived; they cannot yet say how long the
node spent *waiting* for them, which is what issue #299 asks. Splitting
execution from storage, and accounting for time spent idle in the sync loop,
is the next step.
