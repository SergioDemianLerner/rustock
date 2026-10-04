# The difficulty gate

How this node decides whether a peer's claimed cumulative difficulty is
possible, before spending anything on that peer.

A peer announces a total difficulty in its status message. Nothing makes that
figure true. Sync decisions are made from it — who to follow, whose checkpoint
to trust — so a peer that inflates it steers work toward itself, and a peer
that inflates it enormously can hold a node's attention indefinitely.

The gate answers one question: **could any valid chain carry this much work?**
It does not answer "is this peer honest" or "is this the best chain". It
filters who is worth talking to.

## Two halves, with very different costs

| | bounds a claim | cost | runs today |
|---|---|---|---|
| **free half** | at or below the checkpoint height | arithmetic, no requests | yes, always |
| **sampled half** | above the checkpoint height | ~340 header requests | only when a checkpoint is configured |

The split exists because the two cases are not alike. Below the checkpoint the
work is *known*, so refuting a claim is subtraction. Above it there is nothing
to check against — and that window is where a lie lives.

### The free half

`ChainSampler::refuted_by_checkpoint_alone`, reached from
`handler::refuted_by_checkpoint`. A shipped `DifficultyCheckpoint`
(`MAINNET_CHECKPOINT`) records a height, its cumulative difficulty and its
difficulty. A peer claiming more work than that at a height at or below it is
claiming something arithmetically impossible, and its metadata is never
recorded.

This costs nothing and is always on.

### The sampled half

Above the checkpoint, the node asks the peer for **one header every 768 blocks**
(`SAMPLE_INTERVAL`), verifies each one's proof of work, and feeds the
difficulties to `ceiling_for`.

The samples must come **from the peer being judged**. If its chain is
fabricated, nobody else holds those blocks — so a peer that cannot produce them
has failed by another route. `MISSING_SAMPLES_ALLOWED = 4` gives up on a peer
that will not answer: a peer that will not substantiate its claim is as useless
as one that cannot.

## Why sampling bounds anything

Difficulty cannot move arbitrarily between blocks — the retarget rule limits
each step. So given two sampled blocks, the work in the gap between them is
bounded even though those blocks were never seen.

`max_work_between` computes that bound for one gap; `ceiling_for` tiles the
gaps across the chain and adds the exact work below the checkpoint.

### Two ceilings per block, take the lower

Every block between two samples faces two separate limits:

- **the retarget rule** — it cannot exceed the previous block by more than
  `1/divisor`;
- **the landing constraint** — it cannot be so high that even falling as fast
  as the rule allows, it would still overshoot the next sample's difficulty by
  the end of the gap.

The lower of the two is the most that block could have been. `cap[]` holds the
second limit, built backwards from the next sample.

```rust
for step in 1..=span {
    let risen = d + d / divisor;        // the retarget ceiling
    d = risen.min(cap[step]);           // the landing ceiling
    if d < min_difficulty { d = min_difficulty; }
    total += d;
}
```

A worked example, with `divisor = 4` (±25% a block, rather than the real
0.25%, so the numbers stay legible), `from = to = 100`, `span = 3`:

```
cap, built backwards from `to`:   cap[3]=100   cap[2]=133   cap[1]=177

step 1:   risen = 125    cap = 177   ->   min = 125
step 2:   risen = 156    cap = 133   ->   min = 133
step 3:   risen = 166    cap = 100   ->   min = 100
                                          total = 358
```

A chain that stayed flat at 100 across that gap really carries 300. The bound
says 358 — above the truth, which is what an upper bound has to be.

### Why not a rise-then-fall shape

The intuitive model is a triangle: rise as fast as possible, turn, fall onto
the next sample. It is wrong, and the reason is sharper than "it is less
accurate" — **for an important case it has no valid path at all.**

A rise and a fall do not cancel:

```
rise x fall  =  (1 + 1/divisor) x (1 - 1/divisor)  =  1 - 1/divisor²
```

At `divisor = 4` that is 15/16: a rise followed by a fall lands a sixteenth
*below* where it started. So when two consecutive samples carry the **same**
difficulty, no sequence of max-rate moves can return to it. Every such path
ends somewhere other than `to`.

The one path that does land is the flat one — difficulty unchanged the whole
way, which consensus permits. A triangle cannot express it, because a triangle
is always rising or falling.

Worked, with `from = to = 100`, `span = 2`, `divisor = 4`:

| path | ends at | sum | lands on `to`? |
|---|---|---|---|
| rise, rise | 156.25 | 281.25 | no |
| rise, fall | 93.75 | 218.75 | no |
| fall, rise | 93.75 | 168.75 | no |
| fall, fall | 56.25 | 131.25 | no |
| **flat** | **100** | **200** | **yes** |

Note the rise-then-fall path sums *above* the flat chain, not below. The
triangle's problem is not that it undercounts — it is that it cannot land, and
an implementation forced to produce something anyway can easily return a figure
below the flat chain's real 200. A bound below reality rejects honest peers,
which is the one failure an upper bound must never have.

Taking the lower of two ceilings avoids all of this without needing a concept
for "unchanged": once the rise limit exceeds the cap, `d` simply tracks the
cap, which may sit level. That *is* the flat chain, arrived at by arithmetic
rather than by special-casing.

### Why the `min_difficulty` clamp is safe

Consensus will not let difficulty fall below a floor, but `cap[]` is built
assuming maximal falls, so it can compute values below that floor — positions
no real chain could occupy.

The clamp raises those back to the floor. The **direction** is the point: it
only ever raises a term, and raising terms in an upper bound keeps it an upper
bound. It can make the ceiling more generous, never too tight.

Past the newest sample there is no later difficulty to aim for, so the bound is
unconstrained growth. **This is why the newest sample should sit close to the
head** — the further the head is beyond the last sample, the looser the
ceiling.

### How tight is it

The bound loosens roughly as `(1 + 1/divisor)^(N/2)` across a gap of `N`
blocks. At `SAMPLE_INTERVAL = 768` and the post-Papyrus divisor of 400, the
ceiling is about **1.68× the true work** — loose enough to admit any honest
chain, tight enough that an inflated claim has nowhere to hide.

That ratio is the knob. A shorter interval tightens the bound and costs more
requests; a longer one is cheaper and admits more.

## What it costs

The samples come from one peer, so the budget is the **number of requests**,
not bytes — rskj's inbound limit is 1,000 messages per minute per peer.

With a checkpoint refreshed each release, the window above it is small: three
months of chain at 768-block intervals is about **340 samples**, roughly twenty
seconds against that limit. Sampling the whole chain instead would take about
34 minutes, which is the reason the checkpoint exists.

## The three verdicts

`CheckpointVerdict`:

- **`Plausible { ceiling }`** — the claim is within what the chain could carry.
  Not proof of honesty; only that it is not arithmetically impossible.
- **`Impossible { claimed, ceiling }`** — no valid chain through the sampled
  headers carries this much work. The peer is lying.
- **`SelfExceeds`** — *this node's own chain* carries more work at the
  checkpoint height than the checkpoint allows. The checkpoint is wrong, or
  this build is not what it claims to be. **This is not a peer's fault and must
  not be charged to one**: the right response is to stop and tell the operator,
  not to pick a side.

That third verdict is the one most likely to be mishandled by a later change.
It looks like a failure and is not one.

## Where it sits in snapshot sync

`Phase::SamplingClaim` runs between `AwaitingStatus` and `VerifyingHeaders`:
bound the claim from a few hundred headers before committing to a walk that
costs hours and tens of gigabytes. A checkpoint that cannot be supported is
abandoned in seconds rather than at the end.

The walk remains the authority. The gate only decides whether the walk is worth
starting.

## Current state

**The sampled half does not run by default.** `SamplingGate` is constructed
only when `SnapConfig.checkpoint` is `Some`, and `SnapConfig::default()` sets
it to `None`, so no caller supplies one. The free half is unaffected and always
runs.

The consequence is not "no early rejection" but "early rejection only below the
checkpoint height". Above it, an impossible claim costs a full header walk to
reject instead of about twenty seconds.

Tracked as issue #230. `MAINNET_CHECKPOINT` exists and is verified against this
project's own synced node; `ChainSampler` and `SamplingGate` are implemented
and tested against simulated honest, inflated and silent peers. What is missing
is a caller.

## Tests

`crates/sync/src/sampler.rs`:

| Test | Pins |
|---|---|
| `an_honest_claim_passes` | the bound does not reject a real chain |
| `an_inflated_claim_is_refused` | the ceiling rejects more work than any chain could carry |
| `a_peer_that_will_not_answer_is_given_up_on` | `MISSING_SAMPLES_ALLOWED` |
| `an_honest_peer_passes_the_gate` | the same, through the gate rather than the sampler |
| `an_inflated_claim_is_rejected_by_the_gate` | the same, end to end |
| `a_claim_below_the_checkpoint_never_reaches_the_network` | the free half costs no requests |
| `a_silent_peer_is_rejected_rather_than_waited_on` | not answering is a verdict, not a stall |
| `no_verdict_before_the_window_is_covered` | the gate does not judge on partial evidence |
