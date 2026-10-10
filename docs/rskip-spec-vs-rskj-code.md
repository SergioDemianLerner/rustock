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

**rustock implements the rskj order.** `BlockHeaderExtension::trailing_elements`
pushes `base_event` before `edges`, with the override order named in a comment
so it is not "tidied" later.

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

## Open: rustock cannot yet reproduce a testnet V1 header hash

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

**What would settle it.** The actual wire bytes. The public testnet RPC exposes
no raw-header method (`rsk_getRawBlockHeaderByNumber`, `debug_getRawHeader`,
`debug_getBlockRlp` and others are all absent), and the JSON view is lossy in
exactly the way that matters: `rskPteEdges: []` cannot distinguish rskj's
`null` from `short[0]`, which are different encodings and different hashes
(see §2). Pointing a rustock instance at testnet P2P and capturing a real V1
header would resolve it directly.

**Why this matters beyond one test.** Until a V1 hash can be derived rather
than cached, rustock cannot validate a testnet header it did not receive
verbatim — which includes any header it builds, and any it would re-serve.
The V0 path is unaffected.
