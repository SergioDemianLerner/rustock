# A block the node has only part of

rustock can hold a block it has not fully received. rskj cannot. That single
difference in the storage model decides what several RPC methods are allowed to
answer, and getting it wrong produces replies that look complete and are wrong.

## The two storage models

**rskj stores a block whole.** `IndexedBlockStore.saveBlock` writes

```java
blocks.put(block.getHash().getBytes(), block.getEncoded());
```

— one key, one value, header and body together. `getBlockByHash` decodes that
value or returns `null`. There is no third state.

**rustock stores a block in pieces**, across column families: the header in
`headers`, the body in `block_bodies`, the canonical entry in `block_numbers`,
the total difficulty in `total_difficulty`. Nothing writes all of them at once,
and that is deliberate: sync is **header-first**. Headers arrive in batches and
are stored immediately so the chain can be validated and the download planned;
bodies are requested afterwards and filled in as peers answer
(`crates/sync/src/service.rs`, the `put_body` call sites).

So a block with a header and no body is not a race or a bug in rustock. It is a
normal, expected state that lasts as long as the body takes to arrive — moments
in follow mode, longer in a skeleton round.

## What that state must not be rendered as

An empty block.

`eth_getBlockByHash` used to treat a missing body as an absent one:

```rust
let body = store.body(hash).ok().flatten();   // None -> transactions: []
```

A caller asking for a block mid-download was told it had no transactions. That
is not an incomplete answer, it is a false one, and nothing in the response
said so — although the response did contain the contradiction, since
`transactionsRoot` was a real root while `transactions` was `[]`.

rskj never produces this because it cannot: at the same moment it simply does
not have the block, and answers `null`. **`null` is therefore both the truthful
answer and the rskj-compatible one**, which is what rustock now returns.

## A missing body is not an empty body

The rule cannot be "no body row, no block", because some blocks legitimately
have no body row at all. Genesis is one: `setup_genesis` writes the header,
total difficulty, canonical entry and head, and stops. A blanket rule would
have made `eth_getBlockByNumber("0x0")` answer `null` on every node.

The header settles it. A header whose transaction-trie root is the empty root
and whose ommers hash is the empty-list hash claims a body with nothing in it,
so there is nothing missing and the block is served. Anything else claims
content the node does not hold.

Both eras of the empty transaction root count: RSKIP126 changed it from the
Ethereum keccak-of-empty-RLP to the unitrie's, and one node serves blocks from
both sides of that fork.

`held_or_implied_body` in `crates/rpc/src/eth.rs` is the one place this is
decided.

## The uncle exception

`eth_getUncleByBlockHashAndIndex` and `eth_getUncleByBlockNumberAndIndex` do
**not** follow this rule, and must not.

rskj deliberately synthesises an empty block from an uncle's header when it
does not hold that block:

```java
Block uncle = blockchain.getBlockByHash(uncleHeader.getHash().getBytes());
if (uncle == null) {
    uncle = Block.createBlockFromHeader(uncleHeader, isRskip126Enabled);
}
```

So for an uncle, empty `transactions` is the correct answer rather than a
missing one — the method's contract is "describe this uncle", not "give me a
block you hold". The uncle getters keep synthesising, and
`test_an_uncle_is_still_synthesised_from_its_header` pins the distinction so a
later tidy-up does not make the two paths "consistent" by breaking one.

See [`rskj-vs-geth.md`](./rskj-vs-geth.md) for the rest of the uncle getters'
behaviour, including why their answer is not deterministic across nodes or over
time.

## Everything else that reads a body was already right

The transaction and receipt getters (`crates/rpc/src/tx.rs`) return `null` when
the body is absent, because without it they cannot find the transaction —
which is the same answer this rule gives. Log filtering reads receipts, and a
block with no body was never executed and so has none. The block getters were
the only place that invented a value.

## What is still not atomic

Each RPC response is assembled from several independent reads — for a block:
header, then total difficulty, then body — with no RocksDB snapshot, while the
sync thread writes. Fields in one response can therefore come from different
instants.

This is much narrower than the header-only window above, because the pieces of
a *received* block are written close together, and it is not what produced the
wrong answers this document is about. It is recorded here so that the next
person to see two inconsistent fields in one response knows the mechanism
exists. Fixing it means either a read snapshot per response or writing a
block's pieces in one batch; neither is needed for correctness of the rule
above, which depends only on what is present when the body is read.
