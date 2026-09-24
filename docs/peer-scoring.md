# Peer scoring, punishment and banning

A port of rskj's `co.rsk.scoring` package. Before this, rustock had one
comment mentioning "node reputation" and nothing behind it: a peer that fed
bad headers, stalled chunks or flooded the mempool was sidelined for a few
minutes and then welcomed back, with no ban, no operator control and no
visibility.

## The shape

Every peer accumulates a counter per event type, keyed **twice** — once by
node id, once by IP address. A score and a `goodReputation` flag are derived
from those counters. Losing good reputation starts a *punishment* whose length
grows each time the same peer is punished again. When it expires the counters
reset and the peer is welcome back. Separately, an operator can ban an address
or a CIDR block outright; no amount of good behaviour lifts that.

### Why keyed twice

rskj's own comment on `recordEvent`:

> to collect events for the same node_id, but maybe with different address
> along the time. Or same address with different node id.

Scoring only by node id is defeated by regenerating a key, which costs
nothing. Scoring only by address punishes everyone behind a NAT. Both, and
either one can catch what the other misses.

This has a consequence for the code: most rustock callers hold only a node id,
and the address lives behind an `async` lock in `PeerStore`. `ScoringService`
therefore keeps a synchronous node-id → address map, updated when a session
starts and pruned when it ends, so `record_peer(id, event)` can reach both
keys. Without it the whole scheme quietly degrades to node ids.

## Three surprises in rskj's rules

### Only three of the fifteen counters decide reputation

```java
public boolean hasGoodScore(PeerScoring scoring) {
    //TODO(lsebrie): implement empty messages as responses so timeout can be handled as it should
    return  scoring.getEventCounter(EventType.INVALID_BLOCK) < 1 &&
            scoring.getEventCounter(EventType.INVALID_MESSAGE) < 1 &&
            scoring.getEventCounter(EventType.INVALID_HEADER) < 1;
}
```

A peer can fail every handshake, time out every request and disconnect
constantly without ever losing reputation. The exclusion of timeouts is an
explicit TODO, not a decision — but reproducing it is still right: a node that
punished on timeouts would exclude peers every rskj node still talks to, and
disagreeing about peers is how a node ends up partitioned.

`a_peer_that_is_only_unreliable_is_never_punished` pins this from the other
direction, because **the failure nobody notices is banning honest peers**. It
runs 1,400 adversarially-shaped events — disconnections, timeouts, failed
handshakes, repeated and unexpected messages — and asserts nothing happens.

### Five event types do not move the score at all

`UNEXPECTED_MESSAGE`, `FAILED_HANDSHAKE`, `SUCCESSFUL_HANDSHAKE`,
`REPEATED_MESSAGE` and `TIMEOUT_MESSAGE` fall through `updateScoring`'s switch
with an empty body. They increment their counter and nothing else. So the
score is not a summary of the report — reading a score of 0 next to 400 failed
handshakes is correct, not a bug.

### A positive score buys nothing

```java
case INVALID_BLOCK: ...
    if (score > 0) { score = 0; }
    score--;
```

A peer with a long good history is at −1 the instant it sends one invalid
block, exactly like a peer with no history. And in the other direction, the
increment branch is guarded by `if (score >= 0)`, so a punished peer cannot
climb back out by being useful — only the punishment expiring clears it.

## Punishment

| | initial | increment | maximum | rskj key |
|---|---|---|---|---|
| node id | 12 min | +10% per repeat | **none** | `scoring.nodes` |
| address | 12 min | +10% per repeat | 6,000 min (~4.2 days) | `scoring.addresses` |

`PunishmentCalculator.calculate` grows the duration by the increment per
previous punishment, then multiplies by `-score` when the score is negative.
Two details are kept literally:

- the growth loop **returns early** at the maximum, *before* the score
  multiplier, so a capped punishment ignores the score entirely;
- `endPunishment` clears every counter and the score, but **not** the
  punishment counter — which is what makes the next punishment longer.

The asymmetry between the two rows is deliberate and worth stating: a node id
is free to regenerate, so there is no cost to being wrong about one and no
ceiling. An address may carry many honest peers behind a NAT, so being wrong
about one is expensive and it is capped.

Punishment starts on the **transition** only. A peer already serving one is
not re-punished by further events, so the counter — and therefore the next
duration — grows once per episode rather than once per bad message.

### The first event after an expiry is swallowed

`recordEventAndStartPunishment` counts the event *first* and refreshes the
reputation *second*:

```java
peerScoring.updateScoring(event);
boolean hasBadReputationAlready = !peerScoring.refreshReputationAndPunishment();
if (hasBadReputationAlready) { return; }
```

If the punishment has just expired, the refresh runs `endPunishment`, which
zeroes every counter — including the one just incremented. So a peer that
misbehaves once per punishment period never accumulates anything, on rskj or
here. Reproduced deliberately, and pinned by
`the_first_event_after_an_expiry_is_swallowed_as_in_rskj`.

The refresh has to happen on this path and not only when something asks about
reputation. Otherwise a peer whose punishment expired while nobody was looking
keeps stale counters, is never re-punished, and its punishment counter stops
growing — so every later punishment is a first punishment.

## Banning

`sco_banAddress` takes an address or a CIDR block. rskj's mask arithmetic is
`(byte)(0xFF00 >> (cidr & 0x07))`, which is zero exactly when the prefix is a
whole number of bytes; the same expression is used here so the two agree on
every prefix length, including the awkward ones like `/20`.

Two refusals are rskj's and are kept:

- **loopback and wildcard addresses cannot be banned** (`getAddressForBan`
  throws on `isLoopbackAddress() || isAnyLocalAddress()`), which stops an
  operator locking out their own tooling;
- **`/0` is rejected** (`nbits <= 0`), so nobody bans the internet by typo.

### Bans persist, which rskj's do not

rskj's bans come from `peer.bannedPeerIPs` in the config file plus whatever
`sco_banAddress` added this process, and the second set is gone after a
restart. An operator who banned an abusive range at 3am did not mean "until
the next deploy".

Runtime bans are written to `banned-peers.txt` in the data directory and
reloaded at start-up. Plain text, one entry per line, `#` for comments, in
exactly the syntax `sco_banAddress` accepts — editable by hand and diffable.
The write goes to a temporary file and renames, so a crash mid-write leaves
the previous list rather than a truncated one, and a malformed line is
reported and skipped rather than discarding the rest of the file.

This is an addition to rskj's behaviour, not a divergence from it: a node with
this file absent behaves exactly as rskj does.

## Where it sits in the connection path

**Reputation is checked before the ECIES handshake.** rskj checks inside
`HandshakeHandler`, after decoding. Checking earlier matters: the handshake is
secp256k1 ECDH plus Keccak, and a banned host must not be able to make this
node do that work by reconnecting in a loop.

Then, once the handshake has produced a node id, the id is checked too — that
is the earliest it can be. A banned id is dropped before the session starts.

The outbound connector also skips banned and punished peers when choosing who
to dial. Without that it spends its attempt budget on hosts that will be
dropped on arrival, which is how a node ends up under its peer target while
the discovery table looks full.

## Sidelining feeds this; it is not replaced by it

The existing round-local machinery in `crates/sync/src/service.rs` stays:

| | question it answers | lifetime |
|---|---|---|
| `sidelined` / `body_peer_strikes` | "do not assign this peer work in this round" | the round |
| peer scoring | "this peer has a history" | the process, and bans outlive it |

A peer sidelined once is not banned. A peer sidelined every round accumulates
a record an operator can see. Both fire from the same place: the stall and
strike paths now also call `record_peer(..., TimeoutMessage)`.

## What feeds it today

| event | source |
|---|---|
| `SUCCESSFUL_HANDSHAKE` | session registration, with the address |
| `FAILED_HANDSHAKE` | inbound handshake error or timeout, address only (no node id exists yet) |
| `DISCONNECTION` | session end, clean or not |
| `TIMEOUT_MESSAGE` | a stalled header peer; a timed-out body request |
| `INVALID_HEADER` | a header chunk that fails to store — see below |
| `VALID_TRANSACTION` / `INVALID_TRANSACTION` | the transaction relay, per transaction |

### The header chunk had to learn who sent it

`INVALID_HEADER` is a punishing event, and chunks are processed in **skeleton
order**, not arrival order: a chunk that arrives early waits in
`PeerChunkTracker::buffered` until the ones before it land. So the peer whose
response happens to unblock the queue is routinely not the peer that sent the
chunk that then fails to validate.

The buffer therefore carries the supplier alongside the headers, and
`drain_ready` returns it. Without that the only punishing event this node
produces would ban an arbitrary peer — the exact failure the scoring tests are
built around. `drained_chunks_carry_the_peer_that_sent_them` pins it.

### What does not feed it yet, and why

`VALID_BLOCK` and `INVALID_BLOCK` are **not** recorded. Both need the peer
that supplied a block, and rustock does not carry that provenance to where
blocks are validated: `process_single_block` reads from `follow_buffer`, which
holds `(hash, header, transactions, ommers)` and no peer. Threading it through
is a change to the follow path — the path responsible for three separate
mainnet stalls on 2026-09-24 — and does not belong in the same change as the
scoring machinery. Tracked separately.

The practical effect is that `INVALID_HEADER` is currently the only *punishing*
event this node produces. That is narrower than rskj but not unsafe: a peer
serving a chain we cannot validate is caught at the header stage, before its
blocks are ever requested.

## The `sco_*` namespace

| method | |
|---|---|
| `sco_banAddress(addr)` | an address or CIDR block; persisted |
| `sco_unbanAddress(addr)` | likewise |
| `sco_bannedAddresses()` | addresses and blocks in one list |
| `sco_peerList()` | every scored entry, by node id and address |
| `sco_clearPeerScoring(id)` | by address if it parses as one, else by node id; returns the peer list |
| `sco_reputationSummary()` | totals per counter, plus the good/bad split |
| `sco_isWelcome(addr)` | **not rskj's** — see below |

Three differences from rskj:

**`sco_banAddress` returns `true`, not `null`.** rskj's is `void`, and a
JSON-RPC result of `null` is indistinguishable from "the method did nothing".
Callers checking only for an error see no difference.

**`sco_clearPeerScoring` does not resolve DNS.** rskj tries
`InetAddress.getByName(id)` first, which resolves hostnames — so
`sco_clearPeerScoring("example.com")` would clear whatever that resolves to
today. Here only a literal address parses, and anything else is taken as a node
id.

**`sco_isWelcome(address)` is an addition.** rskj has no way to ask "would
this address be let in", so an operator testing a CIDR ban has to wait for the
peer to reconnect and read the log. It reports `banned` and `welcome`
separately, because `welcome: false` without `banned: true` means a punishment
is running — temporary — and the distinction is the first thing you want.

Counters, `score`, `punishments` and `punishedUntil` are JSON **numbers**, as
they are in rskj (`PeerScoringInformation` is a bean of `int`s and a `long`
through Jackson). Same trap as the `trace_*` namespace for a client assuming
hex everywhere.

Without scoring enabled, every method reports itself unavailable rather than
answering from an empty table as though every peer were spotless.

## Configuration

| flag | config file | |
|---|---|---|
| `--banned-peers <ADDR_OR_CIDR>` | `peers.banned_peers` | repeatable; rskj's `peer.bannedPeerIPs` |
| `--no-peer-punishment` | `peers.no_peer_punishment` | rskj's `scoring.punishmentEnabled = false` |

`banned_peers` is the one flag where the config file and the command line are
**combined** rather than the command line winning. A standing ban list belongs
in the config file, and adding one more on the command line should not mean
retyping the rest.

With `--no-peer-punishment` the events are still counted and `sco_peerList`
still reports them — an operator can watch what *would* have been punished
before letting it punish.
