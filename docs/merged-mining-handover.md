# Merged mining: implementation handover

**Status:** not implemented. This document is a survey of what rustock already
has, what has to be built, and the traps found while establishing that. Written
for whoever picks the work up.

Branch `feat/mining-rpc` holds one preparatory commit (§3). Nothing else exists.

> **Re-verified against `main` on 2026-09-20**, 64 commits after this was
> written. Everything still holds: `mnr_*` is still rejected at
> `crates/rpc/src/server.rs:228`, `merged_mining.rs` is still 328 lines, the
> README still states the limitation. One correction: `compute_ommers_hash` has
> moved to `crates/execution/src/processor.rs:407` and is **still private** on
> `main` — making it public is part of §3's unmerged commit, not something
> already done.

---

## 1. What rustock does and does not have

**Does not have.** No block production of any kind. `mnr_*` is rejected
outright in `crates/rpc/src/server.rs:228`, there is no `eth_getWork` /
`eth_submitWork`, no block-template builder, and no code that constructs a
`Header` for a block that does not yet exist. `README.md` §"Limitations" states
this, and it is accurate.

**Does have, and this is the point:** merged-mining *verification* is complete
and tested — `crates/core/src/validation/merged_mining.rs` (328 lines). Mining
is largely the inverse of code that already exists, so the formats do not have
to be reverse-engineered from rskj; they can be read off the validator.

---

## 2. How RSK merged mining works

An RSK block is mined by embedding a commitment to it inside a Bitcoin block's
coinbase transaction, so one SHA-256 search secures both chains.

1. The node builds a candidate RSK block and computes its
   **`hash_for_merged_mining`** — the header hash over the base fields only,
   excluding the three `bitcoin_merged_mining_*` fields. (`Header::hash_for_merged_mining`,
   `crates/core/src/types/header.rs`.)
2. The miner builds a Bitcoin coinbase transaction containing the tag
   `RSKBLOCK:` followed by that hash, and mines a Bitcoin block normally.
3. When the Bitcoin block's hash meets **RSK's** difficulty (not Bitcoin's), the
   miner submits it back.
4. The node fills the RSK header's three merged-mining fields from the submitted
   Bitcoin block and imports it.

Difficulty check: `target = U256::MAX / difficulty`, and the Bitcoin block hash
read **little-endian** must be `<= target` (`merged_mining.rs:32-47`). Note the
Bitcoin block itself need not meet Bitcoin's own difficulty.

### The three header fields

| Field | Contents |
|---|---|
| `bitcoin_merged_mining_header` | The 80-byte Bitcoin block header, consensus-encoded |
| `bitcoin_merged_mining_coinbase_transaction` | The **compressed** coinbase (§2.1) |
| `bitcoin_merged_mining_merkle_proof` | Concatenated 32-byte siblings, coinbase → Bitcoin merkle root |

### 2.1 The compressed coinbase format

Read off `compute_coinbase_hash` (`merged_mining.rs:139-196`). The coinbase is
not stored whole; it is stored as a SHA-256 midstate plus the trailing bytes, so
that the full transaction (which can be large) need not go into every RSK
header.

```
[ trimmed midstate: 40 bytes ][ tail: variable ]

trimmed[0..8]   byteCount, big-endian u64 — bytes already absorbed
trimmed[8..40]  H1..H8, eight big-endian u32 — the SHA-256 state
```

This is BouncyCastle's `SHA256Digest` encoded state with `xBuf`/`xBufOff`
(bytes 0..8) stripped. The hash is completed by absorbing `tail` and finishing
normally, then SHA-256 again (Bitcoin double hash).

**To produce it** you must take the coinbase transaction, absorb whole 64-byte
blocks up to a boundary at or before the RSK tag, export the midstate, and keep
the remainder as the tail. Constraints the validator enforces, all of which the
builder must satisfy:

- the tag must begin at offset **< 64** within the tail (`merged_mining.rs:72`)
- it must be the **last** occurrence of `RSKBLOCK:` in the tail (`:77-80`)
- at most **128 bytes** may follow the tag + hash (`MAX_BYTES_AFTER_MERGED_MINING_HASH`)

### 2.2 The tag, and the fork-detection prefix

From `merged_mining.rs:62-67`:

- **At or after wasabi100 (#1,591,000):** tag is `RSKBLOCK:` + the **first 20
  bytes** of `hash_for_merged_mining`. The remaining 12 bytes carry RSKIP110
  fork-detection data.
- **Before wasabi100:** tag is `RSKBLOCK:` + the full 32-byte hash.

A miner on current mainnet only needs the 20-byte form, but the builder must not
hardcode it — regtest and historical replay use the other.

### 2.3 The merkle proof

`rskip92_merkle_root` (`merged_mining.rs:197-232`) folds the coinbase hash with
each 32-byte sibling in order. The proof is the plain concatenation, length a
multiple of 32. The validator accepts the computed root in **either byte order**
against `btc_header.merkle_root` (`:103-110`) — endianness there is forgiving,
which is worth knowing when a proof "almost" verifies.

---

## 3. What is already on the branch

One commit, `71cbc33`, exposing two things a builder needs and nothing else:

- `DifficultyRule::difficulty_for_child(parent, number, timestamp, uncle_count)`
  — validation already computes the difficulty a header must carry; a miner
  needs the same answer before the header exists. Shared deliberately: two
  copies of a consensus calculation will drift.
- `compute_ommers_hash` made public (`crates/execution/src/processor.rs:407`
  as of 2026-09-20; it was line 360 when this was written).

Take it or start from `main`; nothing is lost either way.

---

## 4. What has to be built

### 4.1 Block template builder

Suggested home: `crates/execution/src/miner.rs`.

**The key hook:** `BlockProcessor::execute_block` executes *without* validating
roots — only `process_block` validates (`processor.rs:88` vs `:177`). So the
builder can assemble a header with placeholder roots, execute, and fill in the
real ones from `ProcessedBlock`, which returns `gas_used`, `paid_fees`,
`state_root_hash`, `receipts_root`, `logs_bloom`.

Sketch:

1. `number = parent.number + 1`; `timestamp = max(now, parent.timestamp + 1)`
2. `difficulty = difficulty_for_child(parent, number, timestamp, ommers.len())`
3. `gas_limit` and `minimum_gas_price`: **inherit the parent's** (§5.2)
4. select transactions from the pool, accumulating `tx.gas_limit` below the
   block gas limit, skipping any below `minimum_gas_price`
5. **append the REMASC transaction** (§5.1)
6. build the header with zero roots, then `execute_block`
7. fill `state_root`, `receipts_root`, `logs_bloom`, `gas_used`, `paid_fees`;
   compute `transactions_root` (`ordered_tx_trie_root`) and `ommers_hash`
8. return the block plus `hash_for_merged_mining()` and
   `target = U256::MAX / difficulty`

Uncle selection can be left out of v1 — an empty ommer list is valid.

### 4.2 `mnr_getWork`

Needs a template cache keyed by `hash_for_merged_mining`, because a submission
arrives minutes later and references work handed out earlier. Keep the last N
templates, not just the newest, or a miner that was slightly slow loses its
block.

### 4.3 Submission

`mnr_submitBitcoinBlock` is the one to do first (full Bitcoin block, everything
derivable from it). For each submission: look up the template by the hash in the
coinbase tag, compress the coinbase, build the merkle proof, populate the three
header fields, run the existing `MergedMiningRule` as a self-check, then import
through the normal path so the block is stored, executed and announced.

Returning rskj's `SubmittedBlockInfo` shape matters if real mining software is
to be pointed at it.

---

## 5. Traps

### 5.1 REMASC is an explicit transaction

It is **not** appended by the executor. It is a real transaction in the block's
list, detected by pattern (`processor.rs:347`): `to == REMASC_ADDR`, `v = r = s = 0`,
`gas_limit == 0`, empty input. A constructor exists in the tests at
`processor.rs:941`. A template without it will produce the wrong state root, and
the error will look like an execution bug rather than a missing transaction.

### 5.2 Inherit gas limit and minimum gas price

The gas-limit rule is a min/max bound only (`header_rules.rs:99-105`), so
equality with the parent is always valid. Same for `minimum_gas_price`. Porting
rskj's gradual-adjustment rules is not needed for a working miner and is a good
way to produce blocks that fail validation for reasons unrelated to mining.

### 5.3 RSKIP351 `extension_data` — unverified, check this first

`Header` carries `extension_data: Option<Bytes>`; when present the bloom moves
there and `logs_bloom` is defaulted (`header.rs:145-170`). rustock always writes
`None` (`processor.rs:416`). **I did not verify whether mainnet blocks at
current heights (~#9,240,000) carry extension data.** If they do, a template
built with `None` will be rejected by peers. Fetch a recent header and look
before building anything else — this is cheap and could otherwise cost a day.

### 5.4 `paid_fees` may look self-referential

`remasc.rs:533` reads `processing_header.paid_fees`, but for the *matured* block
retrieved from storage, not the block being built — so a zero placeholder during
execution should be harmless. **Not verified.** Cheap test: re-execute with the
filled-in `paid_fees` and assert the roots are unchanged. If they are not, the
builder needs a two-pass fixed point.

### 5.5 The block hash excludes the mining fields, the wire encoding does not

`Header::encode_payload(with_merkle_proof_and_coinbase)` — RSKIP92 excludes the
merkle proof and coinbase from the hash while keeping them on the wire. Filling
the mining fields after the template is built must not change
`hash_for_merged_mining`, or the submitted proof will not match. Assert this in
a test.

---

## 6. Suggested tests

- **Round trip, regtest difficulty.** Build a template, construct a Bitcoin
  block whose coinbase carries the tag, submit it, assert acceptance and that
  the stored block's `MergedMiningRule` passes. At regtest difficulty the PoW
  search is trivial, so this runs in a unit test.
- **Compression inverts verification.** Feed a known coinbase through the
  compressor, then through `compute_coinbase_hash`, and compare with a direct
  double-SHA-256. This is the single most error-prone piece.
- **Tag placement bounds.** Tag at offset 63 and 64 of the tail; 128 and 129
  trailing bytes; two `RSKBLOCK:` occurrences. Each must behave as
  `merged_mining.rs:72-88` says.
- **Pre- and post-wasabi100 tag forms** (32-byte vs 20-byte).
- **Template executes to the same roots on replay** — build, then run the result
  through `process_block`, which validates everything the builder filled in.
  This is the strongest single test available and should exist first.

---

## 7. Open questions

1. **RSKIP351 extension data** (§5.3) — verify against a live mainnet header.
2. **`paid_fees` self-reference** (§5.4) — verify with the re-execution test.
3. **Uncle selection.** v1 can mine without uncles; a real miner wants them,
   since REMASC rewards depend on them. `remasc.rs` has the sibling logic to
   read.
4. **rskj's exact JSON shapes.** `mnr_getWork` is understood to return
   `{blockHashForMergedMining, target, feesPaidToMiner, notify, parentBlockHash}`
   and the submit methods a `SubmittedBlockInfo`, but this is from recollection
   of rskj, **not** verified against its source. Check `MinerWork` and
   `SubmittedBlockInfo` in rskj before fixing the wire format.
5. **Where the miner lives.** The transaction pool is in `crates/sync`, the
   processor in `crates/execution`, the RPC in `crates/rpc`. The template
   builder needs all three; deciding whether it is owned by the sync service or
   by a new component is the first design call.
