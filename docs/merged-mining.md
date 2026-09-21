# Merged mining

Rustock builds RSK blocks, hands them to mining software through the `mnr_*`
JSON-RPC namespace, and completes them from Bitcoin solutions submitted back.
This describes what was built, what was verified against mainnet while building
it, and what is deliberately not there.

It supersedes `merged-mining-handover.md`, which surveyed the work before it
existed. Section numbers below refer to that document where a question it
raised is now answered.

---

## 1. Running it

```
rustock-cli --mine --mining-coinbase 0x<address> [--mining-extra-data <string>]
```

`--mine` serves the namespace; without it the four methods report as unknown,
so a node that is not mining is indistinguishable from one built without the
feature. `--mining-coinbase` is required: mining to the zero address would burn
the reward of every block found, and defaulting to it silently is worse than
refusing to start.

The template is rebuilt every `--mining-refresh-secs` (60 by default), matching
rskj. A template goes stale as the chain advances and as transactions arrive.

| Method | Sends |
|---|---|
| `mnr_getWork` | — |
| `mnr_submitBitcoinBlock` | the whole Bitcoin block |
| `mnr_submitBitcoinBlockTransactions` | header, coinbase, all transaction hashes |
| `mnr_submitBitcoinBlockPartialMerkle` | header, coinbase, the merkle branch |

The JSON shapes are rskj's, field for field, because the consumers are existing
pool daemons. Two are worth naming because the sensible-looking alternative
breaks them: `feesPaidToMiner` is a **decimal** string, and
`blockImportedResult` is the **hex encoding of an ASCII word** such as
`IMPORTED_BEST`. Both are `crates/rpc/src/mnr.rs`, both are pinned by tests.

---

## 2. How a block is mined

1. The node builds a candidate block and computes its merged-mining hash.
2. From wasabi100, only the first 20 bytes of that hash go in the coinbase; the
   last 12 are replaced with RSKIP110 fork-detection data (§4).
3. A miner puts `RSKBLOCK:` followed by those 32 bytes in a Bitcoin coinbase
   and searches for a Bitcoin block whose hash, read little-endian, is at most
   `U256::MAX / difficulty`. That is **RSK's** difficulty, not Bitcoin's, so a
   solution is usually not a valid Bitcoin block.
4. The solution comes back and fills in the three `bitcoin_merged_mining_*`
   header fields: the 80-byte Bitcoin header, the compressed coinbase, and the
   merkle proof linking the coinbase to the Bitcoin merkle root.

Producing all of this is the inverse of
`crates/core/src/validation/merged_mining.rs`, which already verified it. The
compressor lives in that same file, beside the hash it inverts, so the two
formats are read and changed together. Everything that exists only on the
producing side — fork-detection data, transaction selection, the work cache —
is in `crates/execution/src/mining/` with the rskj class named at each
definition.

---

## 3. What was verified against mainnet

Four things were checked against real chain data rather than reasoned about,
because each would have produced blocks that every peer rejects while looking
locally correct.

**`ummRoot` is present and empty.** rskj writes it from papyrus200 (#2,392,700)
on, and nothing in a block's JSON-RPC representation shows the field exists.
Omitting it changes both the block hash and the merged-mining hash. Pinned by
`mainnet_block_9257552_hashes_and_merged_mining_commitment`, which reconstructs
a real header and reproduces its hash and the 20-byte commitment its coinbase
carries.

**RSKIP351 extension data is not in use** (handover §5.3, left unverified).
`reed810 = -1` in rskj's `main.conf`: the fork never activated on mainnet.
`extension_data: None` is correct, and the fixture above confirms it from the
other direction — the header reproduces its real hash with a plain 256-byte
bloom.

**The fork-detection calculation is exact.** All 12 bytes of block #9,257,552's
fork-detection data were reproduced from its 449 mainchain ancestors: the
7-byte commit-to-parents vector, the uncle count, and the height.

**The REMASC transaction's encoding is rskj's.** Its zero fields are *not*
canonical RLP — gas price and gas limit are a literal `0x00`, the value is
`0x80` — and its nonce is the parent's height, not zero. Re-encoding it
canonically moves the transactions root, and the resulting failure names a root
rather than an encoding. Pinned against the real transaction of that same
block.

Two further handover questions are answered by tests rather than by mainnet:

- **`paid_fees` is not self-referential** (§5.4). REMASC reads the *matured*
  block's `paid_fees` from storage, not the block being built, so the zero
  placeholder during execution is harmless and no second pass is needed.
  `filling_paid_fees_does_not_change_what_the_block_executes_to`.
- **Filling the mining fields does not move the merged-mining hash** (§5.5).
  RSKIP92 keeps the proof and coinbase out of the hashed prefix precisely so a
  solution can attach to a block already committed to.

---

## 4. Fork-detection data

From wasabi100, the 12 bytes after the 20-byte hash prefix are:

| Bytes | Contents |
|---|---|
| 0..7 | commit-to-parents vector: the last byte of the Bitcoin block hash of the last block of each of the seven preceding 64-block windows |
| 7 | uncles included by the last 32 mainchain blocks, saturating at 255 |
| 8..12 | the height being mined, big-endian |

rskj validates these (`ForkDetectionDataRule`), so getting them wrong produces
blocks peers reject with nothing in the block to point at. They need 449
mainchain ancestors; below that rskj produces none and its rule rejects a
header that carries any, so a short chain correctly gets an empty field.

Rustock computes them for the blocks it mines but does **not** validate them on
incoming blocks — that rule was not in the node before this work and is not
added by it.

---

## 5. Deliberate omissions

**Uncles.** Templates are built with an empty ommer list. A block with no
uncles is always valid, so this costs only the extra reward REMASC pays for
including them -- but calling it a scope decision would be too kind, because
the node could not select uncles today even if it wanted to.

Uncle candidates are the sibling blocks at recent heights that nobody has
included yet. rskj finds them in `FamilyUtils.getFamily`, which walks back
through the ancestors and at each height calls
`BlockStore.getChainBlocksByNumber(n)` -- *every* block at that height,
canonical or not -- keeping those whose parent is the right ancestor. Rustock
has no such index: `CF_NUMBERS` maps a number to the one **canonical** hash
(`crates/storage/src/lib.rs`), so a fork block the node already holds cannot be
found by height at all. The blocks themselves are not thrown away --
`store_headers_batch` stores every header by hash and only moves the canonical
pointer for the winner -- they are simply unenumerable.

There is a second, quieter problem behind that one: sync downloads the best
chain through the skeleton, so sibling blocks largely never arrive. rskj learns
of them from gossip. Even with the index, a freshly synced node would often
have nothing to select from.

So uncle support is a storage change -- a `number -> [hashes]` column family,
written wherever a header is stored, plus a backfill for existing databases --
followed by a port of `getFamily`/`getUncles`/`getUsedUncles`. It carries
consensus weight, too: uncles set `uncle_count`, which feeds both the
difficulty calculation and the uncle byte of the fork-detection data.

**Pre-RSKIP92 merkle proofs.** Only the flat RSKIP92 format is produced. The
older partial-merkle-tree serialization is not, and the verifier in this node
does not accept it either (it requires a length that is a multiple of 32), so
nothing is lost: a miner only ever mines at the tip.

**Announcing mined blocks to peers.** The block is executed, stored and made
the head, but the node has no outbound `NewBlock`/`NewBlockHashes` path — that
is a gap in the wire protocol implementation, not in mining. Peers learn of the
block when they ask.

**Not writing speculative state.** Executing a template writes its trie nodes
through to the store, because that is what `execute_block` does and mining does
not get a second execution path. A template is rebuilt once a minute, so a
mining node accumulates trie nodes for blocks that were never found. The
garbage collector reclaims them (they are unreachable from any canonical
root), but on a node with collection disabled this grows without bound.

**Running a miner and a sync service at once.** The sync service owns the
executed state root and does not know the miner exists. The miner refuses a
submission whose parent is no longer the executed head, so a race is reported
rather than corrupting state — but a node that is actively syncing will refuse
most solutions. Mine on a node that has caught up.

---

## 6. What the tests do and do not cover

The round trip is real as far as it goes: a genuine Bitcoin block is built with
a tagged coinbase, a nonce is actually searched for until the block hash clears
RSK's target, the coinbase is really compressed, the proof really built, and
acceptance is decided by running `MergedMiningRule` -- the same validator that
runs on blocks arriving from peers. The search is not decorative: before it
existed the tests used a fixed nonce, cleared the target only by luck, and that
is how the `get_work` notify bug surfaced.

The difficulty is 2, so the target is `2^255 - 1` and a nonce is found in a few
tries. Two heights are used deliberately:

- Most tests mine #21, which is fast but **does not exercise REMASC**:
  `RskExecutor::new` hardcodes `RemascConfig::mainnet()` whatever the chain id,
  so at that height `process_miners_fees` returns immediately -- there is no
  matured block 4,000 back. The REMASC *transaction* is still in the block and
  still moves the transactions root, which is what §5.1 is about, but nothing
  is distributed.
- `a_solution_is_imported_on_a_chain_deep_enough_for_remasc_to_pay` seeds 4,010
  headers and mines #4,011, clearing both the maturity window and the synthetic
  span, so the matured-header fetch, sibling collection and payout all run.

Not covered, in rough order of how much it matters:

1. **No interop.** Nothing verifies that an rskj node accepts a block this
   miner produces. The evidence that it would is indirect: the header encoding,
   fork-detection data and REMASC transaction are pinned against real mainnet
   block #9,257,552, which says the formats are right but is not interop
   testing. Running a regtest rskj against this node is the test that would
   settle it.
2. **No real mining software.** The `mnr_*` JSON shapes match rskj field for
   field and are pinned by tests, but no pool daemon has parsed them.
3. **No real difficulty**, so nothing exercises a long search or template
   refresh under load.
4. **No uncles**, by design (§5).

## 7. Where the code is

| Path | What |
|---|---|
| `crates/core/src/validation/merged_mining.rs` | the format: verification, and `compress_coinbase` beside it |
| `crates/execution/src/mining/template.rs` | block template builder, transaction selection, the REMASC transaction |
| `crates/execution/src/mining/server.rs` | work cache, the three submit paths, import |
| `crates/execution/src/mining/fork_detection.rs` | RSKIP110 fork-detection data |
| `crates/execution/src/mining/merkle.rs` | RSKIP92 merkle proofs |
| `crates/execution/src/mining/coinbase.rs` | building the Bitcoin side, for regtest and tests |
| `crates/rpc/src/mnr.rs` | the four JSON-RPC methods |

The two tests worth reading first are
`a_template_validates_against_the_processing_path` — a freshly built template
put back through the validating path a peer would use — and
`a_solution_completes_a_block_and_is_imported`, the whole round trip.
