# Where the RSKIPs and rskj disagree

Differences found between an RSKIP's written specification and what rskj
actually does. **rustock follows the code**, because the code is what the
network reached consensus with; a node that implemented the document would
fork. Each entry records what the spec says, what the code does, how it was
confirmed, and what rustock implements.

This is not a list of rskj bugs. Several of these are documents that were
written before the implementation settled and never revised. The point is that
anyone implementing from the RSKIPs alone would produce a node that cannot
follow the chain, and would have no way to know.

Checked against: RSKIPs repository at `rsksmart/RSKIPs` (cloned 2026-10-10),
rskj working tree at `/srv/rskj`.

---

## 1. RSKIP-535: the V2 extension field order is reversed

**Severity: consensus-fatal.** Two nodes disagreeing here compute different
block hashes for the same block and cannot follow each other.

**The spec says** (RSKIP535.md, "Specification"):

```
blockHeaderExtensionEncoded = RLP(logsBloom, txExecutionSublistsEdges, baseEvent)
blockHeaderExtensionHash = Keccak256(RLP(
  Keccak256(logsBloom), txExecutionSublistsEdges, baseEvent))
```

That is `baseEvent` **last**, after the edges.

**The code does the opposite.** `BlockHeaderExtensionV2.addElementsEncoded`
appends its own field and *then* delegates to V1, so `baseEvent` lands before
the edges:

```java
protected void addElementsEncoded(List<byte[]> fieldToEncodeList) {
    byte[] internalBridgeEvent = this.getBaseEvent();
    fieldToEncodeList.add(RLP.encodeElement(... internalBridgeEvent ...));
    super.addElementsEncoded(fieldToEncodeList);   // V1 appends the edges here
}
```

The decoder agrees with the encoder, which is what makes this the real
behaviour rather than a one-sided slip —
`BlockHeaderExtensionV2.fromEncoded`:

```java
byte[] logsBloom = rlpExtension.get(0).getRLPRawData();
byte[] baseEvent = rlpExtension.get(1).getRLPRawData();   // index 1, not 2
... rlpExtension.size() == 3 ? toEdges(rlpExtension.get(2).getRLPRawData()) : null
```

| | field 0 | field 1 | field 2 |
|---|---|---|---|
| RSKIP-535 | `logsBloom` | `txExecutionSublistsEdges` | `baseEvent` |
| **rskj** | `logsBloom` | **`baseEvent`** | **`edges`** |

**Confirmed against the live chain, not just read from the source.** Testnet
block #8,160,760 is a version-2 header. Rebuilding its commitment with the
rskj order -- `[bloom, base_event, edges]` -- reproduces the block hash the
network agreed on, `0xa3403ff9…`. The RSKIP's order does not. A node built
from the document cannot follow testnet.

**rustock implements the rskj order.** `BlockHeaderExtension::trailing_elements`
pushes `base_event` before `edges`, with the override order named in a comment
so it is not "tidied" later, and the test
`testnet_block_8160760_v2_wire_header_hashes_correctly` pins it to real wire
bytes.

---

## 2. RSKIP-351 / 535: the edges element is optional in code, mandatory in the spec

**Severity: consensus-fatal.** It changes the RLP list length, and therefore
the hash.

**The spec treats the field as always present.** RSKIP-351 writes the body as
`RLP(logsBloom, txExecutionSublistsEdges)` with no optionality, and RSKIP-535
is explicit about the inactive case:

> If parallel transaction is not activated, then the value
> `txExecutionSublistsEdges` is left empty.

"Left empty" reads as an empty element — a list of 2 (V1) or 3 (V2) items.

**The code omits the element entirely** when the edges are `null`.
`BlockHeaderExtensionV1.addElementsEncoded`:

```java
short[] internalExecutionSublistsEdges = this.getTxExecutionSublistsEdges();
if (internalExecutionSublistsEdges != null) {       // nothing added when null
    fieldToEncodeList.add(ByteUtil.shortsToRLP(internalExecutionSublistsEdges));
}
```

and both decoders branch on list length (`rlpExtension.size() == 2` for V1,
`== 3` for V2) rather than reading an empty element.

So `null` edges and empty (`short[0]`) edges are **different encodings** in
rskj: one omits the element, the other emits an empty one. The spec has no way
to express that distinction, and a reader who emitted an empty element for the
inactive case would produce a different hash from every rskj node.

**rustock implements the rskj behaviour:** `edges: Option<Vec<i16>>`, where
`None` omits the element and `Some(vec![])` emits an empty one. There is a test
(`empty_edges_and_absent_edges_are_different_commitments`) asserting the two
do not collide, because flattening them is the obvious simplification and it
would be silently wrong.

---

## 3. RSKIP-351: the byte order of the edges is unspecified; the code is little-endian

**Severity: consensus-fatal, and the easiest of these to get wrong by
following convention.**

**The spec says only** (RSKIP-351, "Extended header"):

> `txExecutionSublistsEdges` (at most 8 bytes now, 2 bytes for each short)

Two bytes per short — but not which way round.

**The code packs little-endian**, `ByteUtil.shortsToRLP`:

```java
ByteBuffer.wrap(edgesBytes).order(ByteOrder.LITTLE_ENDIAN).asShortBuffer().put(shorts);
return RLP.encodeElement(edgesBytes);
```

RLP is big-endian everywhere else in the protocol, so an implementer who did
not read this specific helper would reasonably choose big-endian, pass every
round-trip test they wrote themselves, and still compute the wrong block hash
for any block with more than one sublist.

**rustock implements little-endian**, with the byte order pinned by an
explicit test (`edges_are_packed_little_endian`) rather than left as a property
of the code.

---

## 4. RSKIP-144: "unsigned" edges are signed in the implementation

**Severity: cosmetic today, latent.**

**The spec says** (RSKIP144.md):

> This field consists of an array of short **unsigned** integers

**The code uses Java `short[]`**, which is signed 16-bit, throughout
`BlockHeaderExtensionV1`, `BlockHeaderExtensionV2` and
`ParallelizeTransactionHandler`.

Unreachable in practice — the values are transaction indices within one block,
and a block with 32,768 transactions is not a thing — so this has no effect on
any chain that exists. It is recorded because the types do not match the
document, and because an implementation that chose `u16` would differ from
rskj in exactly the range where rskj would produce a negative number.

**rustock uses `i16`**, mirroring Java, so the packed bytes are identical in
every range including the one that cannot occur.

---

## 5. RSKIP-351 says the version must be "exactly 1"

**Severity: none — stale text.**

RSKIP-351 states:

> Consensus must validate that the blockVersion is exactly 1 for blocks after
> the hard fork, until this number is incremented by another consensus change.

RSKIP-535 is that later consensus change and defines version 2. The clause is
self-limiting and was not contradicted, but a reader of 351 alone would
implement a validator that rejects every post-535 block. Noted so the
combination is not read as a conflict.

---

## How these were found

Reading the RSKIPs against the rskj source while implementing
`crates/core/src/types/header_extension.rs`. Items 1–3 would each have
produced a node that computes wrong block hashes, and none of them would have
been caught by rskj's own test suite ported naively, because those tests are
round-trip and differential properties — they check that encoding and decoding
agree with each other, which they would under any consistent field order or
byte order.

That is worth stating plainly: **porting rskj's tests is not sufficient to
establish compatibility with rskj.** The tests pin internal consistency. Only
reading the encoder, or checking against bytes produced by rskj itself, pins
the wire format.

---

## Solved: rustock misparses every V1 header on the wire

**Root cause, confirmed against live testnet.** A V1 header has *two*
serialisations, and rustock only knows one of them.

rskj `BlockHeaderV1.makeExtension`:

```java
return compressed
    ? new BlockHeaderExtensionV1(null, null)      // body fetched separately
    : new BlockHeaderExtensionV1(extensionData, txExecutionSublistsEdges);
                              // ^ field 6 IS the 256-byte bloom here
```

and `BlockHeaderV1.addExtraFieldsToEncodedHeader`:

```java
if (!usingCompressedEncoding) {
    fieldsToEncode.add(RLP.encodeByte(this.getVersion()));
    this.addTxExecutionSublistsEdgesIfAny(fieldsToEncode);
}
```

| | field 6 | after `ummRoot` |
|---|---|---|
| **uncompressed** (wire, storage) | the real 256-byte `logsBloom` | `version`, then `edges` if any |
| **compressed** (block hash only) | `RLP.encodeElement(extensionData)` | nothing |

`getEncodedForHash() = getEncoded(true, !rskip92, true)` — the hash uses the
compressed form; the network never sends it.

**What rustock does.** `Header::decode` dispatches on how many RLP items remain
after the base fields: 3 means the three merged-mining fields, 4 means
`ummRoot` plus those three. A testnet V1 header has **5 or 6** — `ummRoot`,
the version byte, optionally the edges, then the three mining fields. Every one
is therefore misparsed: the version byte is read as the merged-mining header,
and the real fields shift by one or two positions.

**How it was found.** Running a node against testnet (`--network-id 31`, 17
peers) produced, five times in a row:

```
WARN snapshot sync failed: the offered blocks do not form a chain
```

Not a peer problem. rustock cannot link the headers it is offered because it
cannot parse them, so no parent hash matches.

**Why the earlier reconstruction attempts failed.** They were assembling the
*compressed* form from RPC fields and comparing against the live hash. That is
the right shape for hashing, but the inputs were being taken from a JSON view
that cannot distinguish `null` edges from `short[0]` (§2), and the decoder gap
above meant no real V1 header was ever available to check against.

**What the fix requires.** Three things, none of them large but all
consensus-critical:

1. Decode the trailing `version` and optional `edges` when 5 or 6 items remain,
   and build the extension from them together with the wire bloom.
2. Hash using the compressed form — `encodeElement(extensionData)` in field 6
   and no trailing fields — while re-encoding for transmission with the bloom
   and the trailing fields.
3. Keep both forms on the decoded header, since it needs the bloom to serve
   logs and the commitment to hash.

**Why it matters.** This is the whole of "rustock cannot validate testnet". The
V0 path is correct and testnet #6,000,000 hashes exactly; everything above the
RSKIP-351 activation is unreadable. Mainnet is unaffected and still version 0
(`rskPteEdges: null` at #9,314,009), which is why nothing has surfaced it.

---

## Superseded: earlier notes on the V1 hash investigation

Not an RSKIP/rskj divergence — a rustock gap, recorded here because it was
found by the same exercise and it blocks validating testnet.

**What works.** Testnet block #6,000,000 (version 0) hashes correctly from
reconstructed fields, so the base header layout, `umm_root` handling, merged
mining fields and the RSKIP-92 hash rule are all right for testnet.

**What does not.** Testnet block #8,126,464 (version 1, `rskPteEdges = [1, 2]`)
does not. A matrix over the plausible unknowns — edges as `[1,2]`, `[]`,
absent, reversed, scaled; version 1 vs 2; `umm_root` present-and-empty vs
absent — produces fourteen different hashes and none is the one testnet agreed
on. So at least one more thing about the V1 header layout is still unknown.

**A real bug was found on the way, and is fixed.** `Header::encode_base_fields`
wrote `logs_bloom` unconditionally, so `extension_data` never reached the
encoder: every V1/V2 header re-encoded as a 256-byte zero bloom. This was
invisible because the decoder caches the hash from the peer's original bytes,
so nothing had ever asked rustock to *derive* a versioned header's hash. Before
the fix, all fourteen candidates above produced the *same* hash — which is how
it was noticed.

**Narrowed, with two more rskj findings along the way.**

1. **The commitment is carried as the payload of an RLP *string*, not spliced
   in as a nested list.** rskj `BlockHeader.getEncoded` line 365:

   ```java
   byte[] logsBloomField = RLP.encodeElement(compressed ? this.getExtensionData() : this.getLogsBloom());
   ```

   So field 6 of a V1 header is `0xA3 || RLP([version, hash])`, 36 bytes.
   rustock now encodes and parses that form.

2. **The block hash uses the *compressed* encoding**, and a V1 header adds
   nothing extra to it — `getEncodedForHash() = getEncoded(true, !rskip92, true)`
   and `BlockHeaderV1.addExtraFieldsToEncodedHeader` is a no-op when
   `usingCompressedEncoding`. The *uncompressed* form is different: it appends
   a version byte and then the edges, after `ummRoot` and before the merged
   mining fields. V2 appends `baseEvent` after those. That asymmetry is not in
   any of the three RSKIPs and matters for anyone decoding headers off the
   wire.

Structure verified: rustock computes `extension_data` as
`e201a0<32-byte hash>`, which is the shape rskj produces. Twelve further
combinations — edges `[1,2]`/`[]`/absent, wrapped vs spliced, with and without
the trailing fields — were tried against the live hash and none matched, so
the remaining disagreement is in the *inputs* to the extension hash (the bloom
or the edge values), not in the encoding of it.

**What would settle it.** The actual wire bytes. The public testnet RPC exposes
no raw-header method (`rsk_getRawBlockHeaderByNumber`, `debug_getRawHeader`,
`debug_getBlockRlp` and others are all absent), and the JSON view is lossy in
exactly the way that matters: `rskPteEdges: []` cannot distinguish rskj's
`null` from `short[0]`, which are different encodings and different hashes
(see §2). Pointing a rustock instance at testnet P2P and capturing a real V1
header would resolve it directly, and is now the only route left: the explorer
APIs return nothing, and **mainnet is still version 0**
(`rskPteEdges: null` at #9,314,009), so rustock's own database contains no V1
header to compare against. That also explains how the encoder bug survived --
no rustock node has ever been asked to encode a versioned header.

**Why this matters beyond one test.** Until a V1 hash can be derived rather
than cached, rustock cannot validate a testnet header it did not receive
verbatim — which includes any header it builds, and any it would re-serve.
The V0 path is unaffected.


---

## 6. The wire layout of a versioned header is in no RSKIP

Captured from a testnet peer, block #8,160,760, item by item:

```text
 0..5   parentHash … receiptTrieRoot
 6      logsBloom          256 bytes -- the REAL bloom, not the commitment
 7..15  difficulty … uncleCount
 16     ummRoot            empty
 17     version            0x02
 18     edges              0x0100   little-endian, = [1]
 19     baseEvent          empty
 20..22 btcHeader, merkleProof, coinbaseTx
```

So a versioned header has **two serialisations**, and only one is documented:

| | field 6 | after `ummRoot` |
|---|---|---|
| **wire / storage** (uncompressed) | the real 256-byte bloom | `version`, `edges?`, `baseEvent` |
| **block hash** (compressed) | `encodeElement(RLP([version, hash]))` | nothing |

`getEncodedForHash() = getEncoded(true, !rskip92, true)` — the compressed form
exists only to be hashed and is never transmitted. RSKIP-351 describes only the
compressed shape and calls it "the extended header"; nothing says the wire form
keeps the bloom and appends the fields separately.

An implementer reading the RSKIPs would decode every testnet header wrongly:
the version byte lands where the merged-mining header is expected and every
later field shifts.

## 7. The RPC reports an empty `baseEvent` as `null`

`BlockResultDTO`:

```java
this.baseEvent = baseEvent != null && baseEvent.length > 0 ? HexUtils.toUnformattedJsonHex(baseEvent) : null;
```

So `"baseEvent": null` means *either* "no such field" *or* "present and empty",
and the JSON cannot distinguish them. This cost real time here: testnet reports
`null` on every block, which reads as "RSKIP-535 is not active yet", while the
wire bytes show version 2 with an empty baseEvent — it **is** active.

Same class of loss as §2: `rskPteEdges: []` cannot distinguish `null` from
`short[0]`, which are different encodings and different hashes. **The JSON view
is not a sound basis for reconstructing a header.** Only the wire bytes are.

## 8. RSKIP-144 allows four parallel sublists; rskj allows two

**Severity: consensus-fatal in the accepts-too-much direction.** A node
following the document accepts blocks rskj rejects, and would follow a chain
rskj refuses.

**The spec says** (RSKIP144.md):

> A new constant `maxTransactionExecutionThreads` is specified. […] Initially,
> `maxTransactionExecutionThreads = 4`
>
> The maximum number of parallel sublists that the miner can specify is equal
> to `maxTransactionExecutionThreads`.

**The code says** `Constants.java`:

```java
public static final int TX_EXECUTION_THREADS = 2;
```

and `ValidTxExecutionSublistsEdgesRule` rejects any header with more edges than
that. So a block with three sublist edges is valid per RSKIP-144 and invalid
per rskj.

**rustock implements 2**, with the constant named `TX_EXECUTION_THREADS` to
match rskj rather than the RSKIP, and a test
(`more_edges_than_execution_threads_is_rejected`) asserting that three edges
are refused — the case that separates the two readings.

## 9. Testnet header versions, and what #48 assumed

Recorded because rustock's issue #48 states the version "is not checked to
equal 1", which is no longer the whole rule.

From `testnet.conf`:

```
reed810  = 7139600     # RSKIP144 + RSKIP351  -> header version 1
vetiver900 = 7604200
rskip535 = 7604200     # "To be activated in testnet only"  -> header version 2
```

| testnet range | header version |
|---|---|
| below #7,139,600 | 0 |
| #7,139,600 – #7,604,199 | 1 |
| from #7,604,200 | **2** |

So a validator that insists on version 1 rejects every testnet block after
#7,604,200. `ActivationConfig.getHeaderVersion` returns `0x2` when RSKIP-535 is
active and `0x1` otherwise; the check has to follow activation, not a constant.

Mainnet does not set `rskip535` and is still version 0 throughout
(`rskPteEdges: null` at #9,314,009), so none of this is reachable there yet.
