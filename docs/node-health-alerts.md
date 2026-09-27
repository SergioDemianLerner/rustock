# Node health alarms

Three conditions that mean the node is not doing its job, each mailed to an
operator once and then held quiet for a day.

| alarm | condition |
|---|---|
| **stalled** | the executed head has not moved for `for_secs` |
| **behind** | the best peer is more than `block_gap` blocks ahead, for `for_secs` |
| **ahead** | this node is more than `block_gap` blocks ahead of *every* peer, for `for_secs` |

Off by default. Configured under `[node_health]` in the same file as the
peg-out alerts, and mailed through the same `[pegout_alerts.email]` transport —
one configuration and one credential, because two of either is how one of them
goes stale.

## Why "ahead of every peer" is an alarm

It sounds like good news and is not. It means one of two things, and both look
like health from inside the node — blocks are arriving and executing:

- the node is following a chain the network rejected, so its state is diverging
  and every answer it gives is about a fork; or
- every peer it has is stale, so it is effectively blind and its own height
  says nothing about where the chain is.

Neither shows up in any other signal. A forked node reports a healthy tip, a
rising block number and a full peer list.

## Why a condition must persist

Each of these is momentarily true in normal operation. A reorg stalls the
executed head for seconds. A peer announces a block before this node has it, so
it is briefly behind. This node receives a block first, so it is briefly ahead.
Alerting on the instantaneous reading would mail an operator several times an
hour about a node that is fine.

So each condition carries a timer and must hold **continuously** for `for_secs`.
Any reading that clears it resets the timer — the requirement is *sustained*,
not *cumulative*. A node that is behind for nine minutes, catches up for one,
and falls behind again starts from zero.

## Why the cooldown survives recovery

After firing, an alarm goes quiet for `cooldown_secs` (a day by default), and
**recovery does not clear it**.

A node that flaps — stalling, recovering, stalling again — is precisely the
case where per-occurrence mail is useless: the first message already said what
is wrong and the next fifty add nothing except a filter rule. One a day per
alarm is a report; one per occurrence is noise.

The three alarms hold separate cooldowns. Silence on one says nothing about the
others, and a node can quite reasonably be stalled *and* behind.

## Which height each alarm reads

Two different heights, deliberately:

- **the executed head** for the stall, because a node downloading blocks it
  never executes is stalled in every way that matters, and watching the
  download head would miss exactly that failure;
- **the best block held** for the comparisons, because that is what a peer's
  advertised best is comparable to. Comparing a peer's download head against
  *our* executed head would read as permanently behind during any catch-up.

## What it will not tell you apart

**A deliberate resync trips the "behind" alarm.** The alarm cannot distinguish
a node catching up on purpose from one that is stuck, and it will fire once
during any long sync. The cooldown bounds that to one message; the body says so.

**A peerless node raises neither comparison.** With no peer reporting a height,
neither condition is decidable, and both timers stand still rather than reading
as satisfied. Reporting a node with no peers as "ahead of every peer" would be
true and useless. A peer that has connected but not yet announced a height is
not counted either, or every fresh connection would look stale.

## Configuration

```toml
[node_health]
enabled = false
poll_interval_secs = 30   # must not exceed for_secs
block_gap = 10            # strictly greater; a steady gap of 10 does not alert
for_secs = 600
cooldown_secs = 86400
```

`poll_interval_secs` greater than `for_secs` is **rejected at startup**: a
condition could hold for its entire window between two readings and never be
seen, and a monitor that silently cannot observe what it is for is worse than
no monitor.

## What it costs the node

Nothing it can notice. The poll reads two block numbers and the peer table and
holds no lock the node needs. Like the peg-out watcher, if it stalls on an
unreachable mail server the node does not care — there is no hook in the
execution or sync path at all.
