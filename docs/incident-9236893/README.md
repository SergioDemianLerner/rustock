# Incident report: headers rejected with `DifficultyMismatch`

**Date:** 2026-09-14
**Reporter:** rustock node operator
**Affected software:** **rustock** (this repository)
**Affected network:** RSK Mainnet
**Severity:** High — valid canonical headers are rejected, which can wedge block execution

---

## 0. Verdict first: this is not an rskj bug

This report was opened on the suspicion that RSK Mainnet had produced a block
with an invalid difficulty. **It had not.** The block is valid, rskj is correct,
and the network is behaving as specified.

The defect is in **rustock**: it computed the expected difficulty of block
#9,236,893 from the wrong parent — the block one height too low. With the
correct parent, rustock's own formula reproduces the header's difficulty
exactly.

Nothing here needs to be sent to the RSK core team.

---

## 1. The observation

```
WARN Header #9236893 (0xde040bfb8602436a8cf5ec85332d4c19ef91adefaba74f8a49e5c4bf4ae2625e)
     failed verification: DifficultyMismatch {
         expected: 6644629785798474407042,
         got:      6694631041266795007058
     }
```

`got` is the difficulty in the block header as mined. `expected` is what rustock
calculated. The ratio `got / parent_difficulty` is exactly **1.0025**, i.e.
`+1 × parent/400`, which is a well-formed RSK difficulty step.

---

## 2. Proof that the wrong parent was used

Difficulties of the three relevant blocks:

| Block | Difficulty | Timestamp | Uncles |
|---|---:|---:|---:|
| #9,236,891 | 6,661,282,993,281,678,603,550 | 1789321796 | 0 |
| **#9,236,892** (true parent) | **6,677,936,200,764,882,800,058** | 1789321823 | 1 |
| #9,236,893 (subject) | 6,694,631,041,266,795,007,058 | 1789321830 | 0 |

Applying rustock's own formula (`crates/core/src/validation/difficulty.rs`):

**With the correct parent #9,236,892:**

```
delta     = 1789321830 - 1789321823 = 7 s
calc_dur  = (1 + uncle_count) * duration_limit = (1 + 0) * 14 = 14
sign      = +1                       (calc_dur 14 > delta 7)
divisor   = 400                      (RSKIP156, post-papyrus200)
expected  = p + p/400 = 6,694,631,041,266,795,007,058   ← matches the header exactly
```

**With the grandparent #9,236,891:**

```
delta     = 1789321830 - 1789321796 = 34 s
calc_dur  = 14
sign      = -1                       (calc_dur 14 < delta 34)
expected  = g - g/400 = 6,644,629,785,798,474,407,042   ← matches the logged "expected" exactly
```

The logged value is reproduced bit-for-bit by the grandparent and by nothing
else. Searching every block in #9,236,880–#9,236,899 for a difficulty `D` where
`D ± D/400` equals the logged `expected` returns exactly one match: #9,236,891.

The formula is correct. The **parent lookup** is not.

### Illustration

```
            #9,236,891            #9,236,892             #9,236,893
          ┌────────────┐        ┌────────────┐         ┌────────────┐
chain     │ 0x12ea9686 │◄───────│ 0xd1e738fa │◄────────│ 0xde040bfb │
          │ d=6.6612e21│ parent │ d=6.6779e21│ parent  │ d=6.6946e21│
          │ ts=…796    │        │ ts=…823    │         │ ts=…830    │
          │ uncles=0   │        │ uncles=1   │         │ uncles=0   │
          └────────────┘        └────────────┘         └────────────┘
                 ▲                     ▲
                 │                     │
                 │                     └── the header's parentHash points HERE,
                 │                         and this is the correct parent
                 │
                 └── rustock used THIS as the parent (off by one height),
                     so delta became 34 s instead of 7 s, the sign flipped
                     from +1 to -1, and the expected difficulty came out
                     0.75% low
```

The header's `parentHash` field is **correct** and does point at #9,236,892.
The wrong parent was chosen despite that, so the fault is in how rustock
resolves a parent, not in the data it received.

---

## 3. Header fields of the subject block

Block **#9,236,893** (`0x8cf19d`), hash
`0xde040bfb8602436a8cf5ec85332d4c19ef91adefaba74f8a49e5c4bf4ae2625e`

| Field | Value |
|---|---|
| parentHash | `0xd1e738faec826afc418243881b330803c1f233b3a3e830a02a95565c07934bc1` |
| sha3Uncles | `0x1dcc4de8dec75d7aab85b567b6ccd41ad312451b948a7413f0a142fd40d49347` (empty) |
| **miner** | `0x4e5dabc28e4a0f5e5b19fcb56b28c5a1989352c1` |
| stateRoot | `0x21594eea2c2646af8011706aa7ed9d61c936e4016a00085a43dcfd7f2e4e9c60` |
| transactionsRoot | `0x59be89c856370b2eb5b9ccf5e779c2bd797dd835c6fcacd1dcaa3b8bb77656a5` |
| receiptsRoot | `0x66cfdb731f620cd96e2c2cb0f7d3c3a2879c29b40014aa27efbbf3cf9cd3b0f6` |
| difficulty | `0x16aeaab465b52c5cc52` (6,694,631,041,266,795,007,058) |
| number | `0x8cf19d` (9,236,893) |
| gasLimit | `0x989680` (10,000,000) |
| gasUsed | `0x0` |
| timestamp | `0x6aa6e266` (1789321830) |
| minimumGasPrice | `0x1699280` (23,700,352) |
| totalDifficulty | `0x6f44c2bbdf02924ce6a7747f` |
| transactions | 1 (the REMASC system transaction) |
| uncles | 0 |
| extraData | `0xd30191564554495645522d643430323166636532` |

`extraData` decodes as an RLP list containing `1` and the ASCII string
**`VETIVER-d4021fce2`** — the miner is running an rskj *Vetiver* build. The
block is ordinary in every respect: no uncles, no transactions other than
REMASC, and a well-formed difficulty step.

---

## 4. Scope: how often this happens

Scanned the full journal and all rotated node logs.

| | |
|---|---|
| Log occurrences | 92 |
| Distinct (height, hash) pairs | 26 |
| **Distinct block heights affected** | **17** |
| Height range | #9,233,966 – #9,237,578 |
| Blocks observed in that range | ~3,612 |
| **Rate** | **~0.47% of blocks, roughly 1 in 210** |

This is **not** a one-off. It recurs steadily.

Heights where more than one competing hash was rejected:

| Height | Distinct hashes rejected |
|---|---:|
| #9,234,518 | 5 |
| #9,235,676 | 3 |
| #9,235,824 | 2 |
| #9,236,893 | 2 |
| #9,237,195 | 2 |

Clusters of competing hashes at one height indicate the failures concentrate
around **forks and uncles** — exactly where a positional or fallback parent
lookup is most likely to select the wrong block.

### Most rejected headers were valid canonical blocks

| Height | Canonical hash | Rejected hash | |
|---|---|---|---|
| #9,233,966 | `0x09f515cf…` | `0x09f515cf…` | **same block** |
| #9,234,517 | `0x0aa80514…` | `0x0aa80514…` | **same block** |
| #9,234,518 | `0x22c06255…` | `0xd1b9fcec…` | a fork |
| #9,234,691 | `0x581228b4…` | `0x581228b4…` | **same block** |
| #9,234,701 | `0xebd153a3…` | `0xebd153a3…` | **same block** |
| #9,234,819 | `0x3933aef4…` | `0x3933aef4…` | **same block** |

Five of six sampled rejections were of the block that is now canonical. rustock
rejected valid chain data.

---

## 5. Consequence: this is probably the cause of the 2026-09-12 wedge

The first affected height, **#9,233,966**, is the exact block that produced the
canonical-index hole documented in commit `0bc8c5c`. That incident had the node
halting 1,300+ times, unable to execute past a missing canonical entry.

The causal chain is consistent end to end:

```
difficulty computed from wrong parent
        │
        ▼
valid header rejected (DifficultyMismatch)
        │
        ▼
no canonical number → hash entry written for that height
        │
        ▼
hole in the canonical index at #9,233,966
        │
        ▼
execution reaches the hole, cannot cross it, halts permanently
```

The fixes in `0bc8c5c` addressed the *consequence* — they repair a hole and stop
the node wedging on one. This report identifies what *creates* the holes. Both
are needed; only one has been fixed.

---

## 6. Suspected root cause

Not yet confirmed. The strongest candidate is in
`crates/sync/src/manager.rs::handle_headers_response`, which resolves each
header's parent in a chunk of headers by **position in the batch** rather than
by hash:

```rust
// Track the previous header's TD by position for sequential propagation.
// Java's RLP encoding may differ from our canonical encoding, so
// header.hash() can produce a different value than what the next
// header's parent_hash field contains.  Hash-based parent lookup in ...
```

If a batch contains an out-of-order header, a duplicate, or a fork header at a
height already present, every subsequent header's positional parent is shifted
by one — which is precisely the off-by-one-height symptom observed. The
clustering around heights with several competing hashes supports this.

There is also a fallback to `canonical_hash(number - 1)`, which returns the
wrong block if the canonical index is itself mid-update.

---

## 7. Attached files

| File | Contents |
|---|---|
| `block-9236893-header.rlp` | Subject block header, 1,100 bytes, as stored |
| `block-9236893-body.rlp` | Subject block body, 36 bytes (REMASC only) |
| `block-9236892-header.rlp` | True parent header, 1,167 bytes |
| `block-9236892-body.rlp` | True parent body, 1,213 bytes |
| `block-9236891-header.rlp` | Grandparent — the block rustock wrongly used, 1,173 bytes |
| `block-9236891-body.rlp` | Grandparent body, 1,079 bytes |

Exported with `cargo run --release --example dump_block -- <datadir> <number> <outdir>`,
which reads the database read-only.

---

## 8. Reproduction

```bash
# Confirm the arithmetic against a node holding these blocks
cargo run --release --example dump_block -- /var/lib/rustock 9236893 /tmp/out

# expected(correct parent)   = 6677936200764882800058 + 6677936200764882800058/400
#                            = 6694631041266795007058   (the header's value)
# expected(grandparent)      = 6661282993281678603550 - 6661282993281678603550/400
#                            = 6644629785798474407042   (the value rustock logged)
```

A unit test should construct a three-block chain where the middle block carries
an uncle, feed the headers to `handle_headers_response` out of order and with a
competing fork header at the middle height, and assert that each header's
difficulty is validated against the block named by its own `parentHash`.

---

## 9. Recommended fix

Resolve a header's parent **by its `parentHash` field**, never by position in a
batch and never by canonical height lookup, for difficulty validation. Where the
Java/canonical RLP hash discrepancy referenced in the code comment genuinely
occurs, it should be handled explicitly and logged, not papered over with a
positional fallback that silently yields a different block.

Until fixed, a `DifficultyMismatch` on a header whose `parentHash` resolves to a
stored header should be treated as a node bug and logged at ERROR with both
candidate parents, rather than silently rejecting chain data at WARN.
