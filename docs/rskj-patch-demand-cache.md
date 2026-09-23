# Patch: make rskj's BTC block cache demand-populated

**Target:** `rskj` at the revision pinned in `docs/rskj-reference.md` —
`3dc9d977228d938385f0ee7e025162da5c220f1b`.

**Audience:** an agent or engineer applying this to rskj. Everything needed is
here; no familiarity with rustock is assumed, and the rustock implementation is
cited only as the worked reference.

**Scope:** one production file, one interface comment, and new tests. No
consensus rule changes. No storage format changes.

---

## 1. What the cache does today

`co/rsk/peg/RepositoryBtcBlockStoreWithCache.java` keeps a bounded map of
`Sha256Hash -> StoredBlock` so that walking the BTC main chain does not read
the repository at every hop.

| Thing | Where | Value |
|---|---|---|
| Cache map | `Factory` ctor, l.373 | `MaxSizeHashMap<>(maxSizeBlockCache, true)` — access-ordered LRU |
| `DEFAULT_MAX_DEPTH_BLOCK_CACHE` | l.56 | 5,000 |
| `DEFAULT_MAX_SIZE_BLOCK_CACHE` | l.57 | 10,000 |
| Config keys | `reference.conf` l.635 | `cache.btcBlockStore.depth`, `cache.btcBlockStore.size` |
| Wiring | `RskContext.getBtcBlockStoreFactory()`, l.543 | reads both keys |

Three code paths touch it:

**`put(StoredBlock)` — l.112.** Writes to the repository, then caches the block
if it is within `maxDepthBlockCache` of the chain head. *No read is involved:
the block is already in hand.*

**`setChainHead(StoredBlock)` — l.145.** Writes the head, then calls
**`populateCache(newChainHead)`** at l.150.

**`populateCache(StoredBlock)` — l.246.** Walks back from the new head up to
`maxDepthBlockCache - 1` blocks, calling `get(blockHash)` on each — i.e.
**reading the repository for up to 5,000 blocks that nothing asked for.** It
stops early on a cache hit or a missing block.

**`get(Sha256Hash)` — l.125.** Reads the repository. Note it does **not**
consult the cache; only `getFromCache` does, and only
`getStoredBlockAtMainChainDepth` (l.271) calls that.

---

## 2. Why change it

Be clear about what this does and does not buy, because overselling it is the
easiest way to get the patch rejected.

**It is roughly performance-neutral for rskj itself.** `populateCache` stops at
the first already-cached block, so the full 5,000-block walk happens once —
effectively at startup — and after that each `setChainHead` adds one entry. The
saving is one startup cost, not a steady-state one.

**What it actually buys is a deterministic read set.** The repository reads a
node performs while executing a block become that block's read set, and several
things downstream depend on the read set being exactly what consensus required:

- state proofs and snapshot-style sync, where a chunk must contain the nodes a
  consumer will read;
- differential testing between implementations, where two clients that read
  different node sets cannot be compared directly;
- any attempt to bound or reason about I/O per block.

A speculative walk of 5,000 entries on `setChainHead` makes the read set depend
on **cache warmth and process age** rather than on the block being executed.
Two nodes executing the same block read different things.

**And there is a correctness trap that only appears once snapshots exist.** A
repository read that cannot find a node and a genuinely absent value are
indistinguishable — both come back `null`. A speculative read against a
partial state therefore reads "absent", and a cache that *recorded* that would
hand it to a later consensus-required lookup. rskj does not cache negatives
today, and the patch below must keep it that way. This is called out because it
is the obvious "improvement" someone will propose later.

---

## 3. The design

Two rules. Both are about what the cache is allowed to *learn*, not how fast it
is.

**R1 — populate only from reads that happened anyway.** Nothing walks ahead.
The cache fills from `get()` calls the node made because something asked, and
from `put()` (which involves no read at all).

**R2 — never record an absence.** There is no "this block is not here" entry.
A lookup that finds nothing leaves the cache unchanged and the next lookup asks
the repository again. This makes the trap in §2 unreachable rather than
guarded against: the cache can answer or say nothing, and it can never say no.

The mutable keys stay uncached, exactly as today: the chain head
(`BLOCK_STORE_CHAIN_HEAD_KEY`) and the RSKIP199 height→hash index
(`getBtcBestBlockHashByHeight`). Only `hash -> StoredBlock` is cached, and that
mapping is immutable — a `StoredBlock` is stored under its own header hash, and
a header's height and chain work are fixed by its ancestry, so an entry cannot
go stale and needs no invalidation.

---

## 4. The patch

### 4.1 `RepositoryBtcBlockStoreWithCache.get` — consult and populate

Currently (l.124–133):

```java
    @Override
    public synchronized StoredBlock get(Sha256Hash hash) {
        logger.trace("[get] Looking in storage for block with hash {}", hash);
        byte[] ba = repository.getStorageBytes(contractAddress, DataWord.valueFromHex(hash.toString()));
        if (ba == null) {
            logger.trace("[get] Block with hash {} not found in storage", hash);
            return null;
        }
        return byteArrayToStoredBlock(ba);
    }
```

Replace with:

```java
    @Override
    public synchronized StoredBlock get(Sha256Hash hash) {
        // The cache is keyed by the block's own header hash, and a StoredBlock's
        // height and chain work are fixed by its ancestry, so an entry can never
        // go stale and needs no invalidation.
        if (cacheBlocks != null) {
            StoredBlock cached = cacheBlocks.get(hash);
            if (cached != null) {
                return cached;
            }
        }

        logger.trace("[get] Looking in storage for block with hash {}", hash);
        byte[] ba = repository.getStorageBytes(contractAddress, DataWord.valueFromHex(hash.toString()));
        if (ba == null) {
            logger.trace("[get] Block with hash {} not found in storage", hash);
            // Deliberately NOT cached. A repository read that cannot find a node
            // and a genuinely absent value both return null, and nothing at this
            // layer can tell them apart. Recording an absence would let a partial
            // state turn into a wrong answer for a later consensus-required
            // lookup. See the class javadoc.
            return null;
        }

        StoredBlock block = byteArrayToStoredBlock(ba);
        if (cacheBlocks != null) {
            cacheBlocks.put(hash, block);
        }
        return block;
    }
```

### 4.2 `setChainHead` — stop pre-loading

At l.145–152, delete the `populateCache` call:

```diff
     @Override
     public synchronized void setChainHead(StoredBlock newChainHead) {
         logger.trace("Set new chain head with height: {}.", newChainHead.getHeight());
         byte[] ba = storedBlockToByteArray(newChainHead);
         repository.addStorageBytes(contractAddress, DataWord.fromString(BLOCK_STORE_CHAIN_HEAD_KEY), ba);
-        if (cacheBlocks != null) {
-            populateCache(newChainHead);
-        }
         setMainChainBlock(newChainHead.getHeight(), newChainHead.getHeader().getHash());
     }
```

### 4.3 Delete `populateCache` entirely

Remove l.246–267 (the whole `private synchronized void populateCache(StoredBlock chainHead)` method). It has no other caller — confirm with:

```sh
grep -rn "populateCache" rskj-core/src/
```

which must return nothing after the patch.

### 4.4 `put` — leave the depth gate, but say why it is now different

`put` (l.112) keeps caching, because it involves no read. Its
`maxDepthBlockCache` gate now means only *"a block we are writing far below the
head is not worth a cache slot"* — it no longer bounds a pre-load, because
there is no pre-load. Add a comment saying so, or the next reader will assume
the constant still does what its name suggests:

```java
        if (cacheBlocks != null) {
            StoredBlock chainHead = getChainHead();
            // Caching a block we are writing costs no read. The depth gate is a
            // slot-budget heuristic only; since populateCache was removed it no
            // longer bounds any speculative walk. Blocks below this depth still
            // reach the cache when a lookup actually reads them (see get()).
            if (chainHead == null || chainHead.getHeight() - storedBlock.getHeight() < this.maxDepthBlockCache) {
                cacheBlocks.put(storedBlock.getHeader().getHash(), storedBlock);
            }
        }
```

### 4.5 Class javadoc — record the rule

Append to the class javadoc at l.46–50, because R2 is the kind of rule that
gets "optimised" away by someone who does not know why it is there:

```java
/**
 * Implementation of a bitcoinj blockstore that persists to RSK's Repository
 *
 * The cache is DEMAND-POPULATED and holds NO NEGATIVE ENTRIES.
 *
 *  - Nothing is read speculatively. Entries come from reads the node performed
 *    because something asked for them, and from put(), which involves no read.
 *    This keeps a block's repository read set equal to what consensus required,
 *    which state proofs and snapshot-style sync depend on.
 *
 *  - An absent block is never recorded. A repository read that cannot find a
 *    node and a genuinely absent value are the same null, so caching an absence
 *    would let a partial state produce a wrong answer for a later
 *    consensus-required lookup. The cache may answer or say nothing; it must
 *    never say "no".
 *
 * Only hash -> StoredBlock is cached. The chain head and the RSKIP199
 * height->hash index are mutable and are deliberately left uncached.
 *
 * @author Oscar Guindzberg
 */
```

### 4.6 `BtcBlockStoreWithCache.getFromCache` — unchanged, now redundant

`getStoredBlockAtMainChainDepth` (l.271) calls `getFromCache` then falls back to
`get`. With §4.1 applied, `get` already consults the cache, so the explicit
check is redundant — but harmless, and removing it changes the interface.
**Leave it.** A follow-up may simplify it; this patch should not.

### 4.7 Configuration

`cache.btcBlockStore.depth` keeps its meaning for `put` (§4.4) and can stay.
If you prefer to retire it, that is a separate change: it is read in
`RskSystemProperties.getBtcBlockStoreCacheDepth()` (l.496) and passed from
`RskContext` (l.545), and dropping it is a config-compatibility decision, not a
behaviour one.

---

## 5. Tests

### 5.1 Existing tests: expect all to pass unmodified

`rskj-core/src/test/java/co/rsk/peg/RepositoryBtcBlockStoreWithCacheTest.java`.

One looks like it depends on pre-loading and does not:

```java
    void cacheLivesAcrossInstances() {
        ...
        btcBlockStore.put(firstStoredBlock);
        //Cache should have the genesis block and the one we just added
        assertNotNull(btcBlockStore.getFromCache(genesis.getHash()));
```

Genesis reaches the cache through `checkIfInitialized()` (l.345), which calls
`put(storedGenesis)` — and `populateCache` returns immediately when the head
*is* genesis (l.248), so it never contributed. Verify by running the suite
before and after.

`put_oldBlockShouldNotGoToCache` still passes: it tests the `put` gate (§4.4),
which is unchanged.

### 5.2 New tests to add

```java
    @Test
    void get_populatesTheCacheFromARealRead() throws BlockStoreException {
        BtcBlockStoreWithCache btcBlockStore = createBlockStore();
        BtcBlock genesis = networkParameters.getGenesisBlock();

        // Write a block far below the head so put()'s depth gate does NOT cache it.
        StoredBlock head = createStoredBlock(genesis, 6000, 0);
        btcBlockStore.put(head);
        btcBlockStore.setChainHead(head);

        StoredBlock deep = createStoredBlock(genesis, 1, 1);
        Sha256Hash deepHash = deep.getHeader().getHash();
        btcBlockStore.put(deep);
        assertNull(btcBlockStore.getFromCache(deepHash), "precondition: not cached by put");

        // A real read caches it.
        assertEquals(deep, btcBlockStore.get(deepHash));
        assertEquals(deep, btcBlockStore.getFromCache(deepHash));
    }

    @Test
    void get_doesNotCacheAnAbsentBlock() throws BlockStoreException {
        BtcBlockStoreWithCache btcBlockStore = createBlockStore();
        Sha256Hash absent = Sha256Hash.of(new byte[]{1, 2, 3});

        for (int i = 0; i < 5; i++) {
            assertNull(btcBlockStore.get(absent));
        }
        assertNull(btcBlockStore.getFromCache(absent),
            "an absence was recorded; a partial state could now produce a wrong answer");
    }

    @Test
    void setChainHead_doesNotPreloadAncestors() throws BlockStoreException {
        Repository repository = createRepository();
        RepositoryBtcBlockStoreWithCache.Factory factory = createBlockStoreFactory();
        BtcBlockStoreWithCache store = createBlockStoreWithTrack(factory, repository.startTracking());

        BtcBlock genesis = networkParameters.getGenesisBlock();
        StoredBlock prev = createStoredBlock(genesis, 1, 0);
        store.put(prev);
        for (int h = 2; h <= 20; h++) {
            StoredBlock b = createStoredBlock(prev.getHeader(), h, 0);
            store.put(b);
            prev = b;
        }

        // A fresh instance shares the trie but starts with a cold view: only
        // setChainHead runs, and it must read no ancestor.
        BtcBlockStoreWithCache fresh = createBlockStoreWithTrack(factory, repository.startTracking());
        // (Assert via a counting Repository; see 5.3.)
        fresh.setChainHead(prev);
    }
```

### 5.3 The test that matters most

The three above check behaviour. The one that protects the property needs a
**counting `Repository`** — a decorator that increments on `getStorageBytes` —
so the assertion can be about *reads performed*, not about what ended up
cached:

```java
    @Test
    void theCacheReadsNothingTheUncachedPathWouldNot() throws BlockStoreException {
        // Build the same chain twice, behind counting repositories: one store
        // with a cache, one without (pass null for cacheBlocks).
        // Issue the same sequence of getStoredBlockAtMainChainHeight calls to both.
        // Assert: cachedReads <= uncachedReads.
        //
        // This is what keeps the read set equal to what consensus required. If
        // anyone reintroduces a speculative walk, this is the test that fails.
    }
```

**Write this one first, and make sure it can fail.** In the rustock
implementation the equivalent test initially passed while measuring **zero
reads on both sides**, because the fixture served everything from memory and
never touched the store. It was asserting `0 <= 0`. Add an explicit
`assertTrue(uncachedReads > 0)` guard so a fixture that goes inert reports it
instead of passing.

---

## 6. Validation

```sh
# The direct suite
./gradlew :rskj-core:test --tests "co.rsk.peg.RepositoryBtcBlockStoreWithCacheTest"

# Everything that exercises the Bridge's BTC chain
./gradlew :rskj-core:test --tests "co.rsk.peg.*"

# No stragglers
grep -rn "populateCache" rskj-core/src/
```

Beyond the unit tests, the change is only credible with a replay: run a node
from a snapshot across a range containing `getBtcTransactionConfirmations`
calls and peg-ins, with and without the patch, and compare state roots
block-for-block. They must be identical — the patch changes *when* a read
happens, never *what* it returns.

---

## 7. Risk

**Consensus:** none. Every path returns the same value from the same
repository; only the order and count of reads change. `getStoredBlockAtMainChainDepth`
and `getStoredBlockAtMainChainHeight` keep their `null`-vs-`BlockStoreException`
distinction untouched, which matters because `getBtcTransactionConfirmations`
maps them to distinct error codes.

**Performance:** the first deep query after startup is slower, because the
walk now reads from the repository instead of a pre-warmed map. Subsequent
queries at the same or nearby depths are faster, because the walk's own reads
populate the cache. Startup loses a 5,000-entry speculative walk.

For scale, measured in rustock against its mainnet database (same algorithm,
different storage engine): a depth-5,000 main-chain query costs ~209,000 trie
reads and ~28 s cold, and ~0.9 ms once the walk's blocks are cached. Absolute
numbers will differ in rskj; the shape will not.

**Rollback:** restore `populateCache` and its call site. Nothing persists
differently, so a downgrade needs no migration.

---

## 8. What not to do

Three changes that look like improvements and are not:

1. **Do not cache negatives.** §3 R2. The whole point.
2. **Do not pre-load "just a little"** — e.g. the head's parent. A speculative
   read of depth 1 has the same property as depth 5,000: the read set stops
   being a function of the block being executed.
3. **Do not cache the chain head or the height→hash index.** Both are mutable;
   a reorg rewrites them, and the cache has no invalidation because the
   immutable-key argument in §3 does not cover them.

If pre-loading is ever genuinely wanted, it needs a different contract first:
the speculative read must be able to return **present / absent / not-available**
rather than collapsing the last two into `null`, the cache must be told which
it got, and a consensus-required lookup that finds only a speculative negative
must fall through to the real read. That is a larger change than this patch and
should not be smuggled into it.

---

## 9. Reference implementation

rustock implements exactly this design, and the files are readable as a
worked example:

| Concern | File |
|---|---|
| The cache, the argument for it, and 13 unit tests | `crates/execution/src/bridge/btc_block_cache.rs` |
| The three-layer read path (own writes → cache → storage) | `crates/execution/src/bridge/btc_store.rs`, `get_stored_block` |
| 7 read-path tests incl. the read-set property | `crates/execution/src/bridge/btc_block_cache_readpath_tests.rs` |
| The benchmark producing the numbers in §7 | `crates/cli/examples/btc_cache_bench.rs` |

The differences from rskj worth knowing: rustock's overlay of uncommitted
in-block writes is consulted *ahead* of the cache (rskj's `Repository` track
handles that for it), and rustock does not cache on write at all, where this
patch keeps rskj's `put` caching because it costs no read.
