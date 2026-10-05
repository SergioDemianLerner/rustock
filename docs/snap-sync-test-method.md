# How to run a snapshot sync measurement

Written after a run that produced nothing usable because the fixture was wrong.
If you are about to time a sync, read the first section before starting
anything: it costs two minutes and the run costs two hours.

## The rule

**The server must not change while the measurement runs.**

A node that is still following the chain is not the same fixture twice. Its
best block moves, its advertised total difficulty moves, and the second client
measured has more to do than the first. Worse, a live node is also serving
other peers and executing blocks, so the thing being measured is partly the
server's spare capacity on the day.

Three flags make a node a fixed fixture. All three, together:

```
--read-only          # open both databases read-only; nothing it does can write
--follow-up false    # serve what is held; do not sync, execute or follow
--simulate-height N  # behave as though the canonical chain ends at N
```

See issue #233 for what each one had to change.

## Starting the server

A read-only server opens the live data directory **while the ordinary node is
still running** -- verified -- so there is no need to stop production or copy a
194 GB directory.

```sh
rustock-cli \
  --data-dir /var/lib/rustock \
  --trie-backend epoch --trie-dir /var/lib/rustock/trie-epochs \
  --read-only --follow-up false \
  --simulate-height 9285000 \
  --port 30320 --rpc-port 4447 \
  --snap-server \
  --log-to-stdout
```

Check the log says both of these before going further:

```
Opening /var/lib/rustock READ-ONLY; nothing this node does can write to it
Snapshot server: currently offering the state at #<N> (0x...)
```

### Choosing `N`

Put it **on the chunk grid**: `N % 192 == 0`. The protocol serves headers in
runs of at most 192 descending from a hash, and skeleton identifiers land on
multiples of 192, so a target off the grid has a tail no identifier names. The
code handles that case now, but an aligned `N` removes a whole class of
question from the measurement.

Also keep `N` at least `checkpoint_distance` (10,000) below the real head, or
the server has no settled state to offer.

## Starting the client

A fresh data directory, its own ports, and pinned to the one server:

```sh
rustock-cli \
  --data-dir /mnt/import/<run-name>/data \
  --trie-backend epoch --trie-dir /mnt/import/<run-name>/trie \
  --port 30310 --rpc-port 4446 \
  --bootnodes 127.0.0.1:30321 \
  --closed-network --max-peers 1 \
  --snap-sync \
  --log-to-stdout > /mnt/import/<run-name>/run.log 2>&1
```

Four things that are easy to get wrong:

- **`--bootnodes` takes the discovery port, which is the listen port plus
  one.** A server on `--port 30320` is reached at `HOST:30321`. Pointing at the
  listen port produces a client that starts cleanly, logs nothing unusual, and
  never connects.
- **`--closed-network`** stops the client being introduced to public peers by
  the one it knows, which would make the measurement meaningless.
- **`--trie-backend epoch`** because a fresh data directory refuses to start on
  the default `single` backend -- it trips a guard meant for databases whose
  trie was detached.
- **Disk.** A header pass to genesis with uncles moves about 20 GB. `/` has no
  room for that; use `/mnt/import` or another volume with 30 GB free, and
  never a subdirectory of the production data directory.

Confirm the client actually connected before walking away:

```sh
curl -s -X POST -H 'Content-Type: application/json' \
  --data '{"jsonrpc":"2.0","id":1,"method":"net_peerCount","params":[]}' \
  http://127.0.0.1:4446
```

`0x1` is what you want. `0x0` after a minute means it is not talking to
anything, and the log will not say so.

## Reading progress

```sh
tail -f /mnt/import/<run-name>/run.log
```

**The ETA in the progress line is linear from the start of the phase and is not
trustworthy near the end.** A stalled sync prints `100.0%` and `~3s left`
indefinitely. Judge progress by whether the *height* moves:

```sh
grep -oE "at #[0-9]+ of #[0-9]+\), [0-9]+s elapsed" run.log | tail -5
```

If the height is the same across several lines, it is stuck, whatever the
percentage says.

## What to record

Per phase, from the log timestamps:

| | |
|---|---|
| status exchange | first contact to the offer being accepted |
| header phase | first header request to the chain being established |
| state download | bytes and seconds |
| blocks and execution | the `blocks_required` window below the checkpoint |
| total | process start to head reached |

And alongside them: bytes on the wire per category, peak RSS, and the server's
`--simulate-height`.

**Disk usage is not wire bytes.** Uncle headers are verified, counted and
dropped -- they are not blocks of this chain -- so a client that moved 20 GB
may hold 9 GB. Measure the wire if the wire is what you mean.

## Measuring how it scales with several servers

`tools/snap-scale-test/` runs the sweep. It exists because the setup has three
traps and all three produce a plausible-looking wrong number rather than an
error.

```sh
tools/snap-scale-test/run-all.sh 4 3     # 1..4 servers, 3 repeats, interleaved
```

Or by hand:

```sh
tools/snap-scale-test/server.sh 1 &      # port 30310, discovery 30311
tools/snap-scale-test/server.sh 2 &      # port 30320, discovery 30321
tools/snap-scale-test/measure.sh 2       # one 180s window against both
```

### Several servers may share one database

They must all be `--read-only`, which they are anyway to be a fixed fixture.
There is no need to copy anything: at these rates a server moves about 4 MB/s
and is bound by I/O *latency*, not bandwidth, so several on one disk overlap
their waits rather than competing for throughput. A full copy would be ~57 GB
and buys nothing.

### Each server needs `--secret-key`

The node identity lives in `<data-dir>/node.key`. Servers sharing a database
therefore present the **same node id**, and a client collapses them into one
peer: the peer count reads 1 when you asked for 2, and the measurement is of a
single server while appearing to be of several. `server.sh` derives a distinct
key per index, deterministically, so a rerun reproduces the same identities.

### The client must have settled before the window opens

A client that starts measuring before every server has connected measures
fewer servers than it thinks. That produced a 973 blocks/s reading in a group
whose other samples were 3,549 and 3,796 -- and nothing in the log said so;
it was only visible by sampling each server's CPU and finding one at 23% and
the rest at 1%.

`measure.sh` settles for 150s and checks the peer count against the number of
servers asked for, printing `** DISCARD **` when they disagree. Check that each
server is actually working if a number looks wrong:

```sh
top -b -n2 -d10 -p $(pgrep -d, -f simulate-height) | grep rustock
```

Healthy is every server at a similar percentage. One high and the rest idle
means the client is talking to one of them.

### Interleave the repeats

Run `1,2,3,4` then `1,2,3,4` again, not three of each in turn, so drift in
machine load falls on every group rather than on whichever ran last.

## Comparing against the recorded baseline

`docs/cross-client-snap-sync.md` holds the 2026-10-01/02 run: rustock client,
descending header walk, **6,971 s** for the walk to genesis and **7,545 s** end
to end, moving 21.42 GB at an effective 2.84 MB/s.

That run used a live server, so it carries the same caveat this document
exists to remove. Treat it as indicative until it is repeated against a frozen
one.

## Afterwards

Keep the client data directory and the log until the numbers are written up;
they are the only record of what happened. Delete them once they are, since
each run is tens of gigabytes.

Do not reuse a client data directory between runs. A partially synced directory
changes what the next run has to do, which is the same mistake as a server that
moves.
