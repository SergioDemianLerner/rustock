# dump-index

Dumps rskj's MapDB block index to a flat text file.

## Do you need this?

**Almost certainly not.** `rustock --metadata-in-memory` derives the same
information from headers rustock already holds, in 2m47s against this tool's
~2.5 hours, with no JVM involved. This exists for two narrower reasons:

- as an **independent cross-check**: it reads the values rskj itself computed,
  so diffing them against rustock's derived ones verifies the derivation;
- as a **fallback** if headers are unavailable but the rskj index is.

## What it extracts

rskj keeps its canonical-chain data in `blocks/index`, a MapDB file whose
values are Java-serialised `ArrayList<IndexedBlockStore.BlockInfo>`. Each
`BlockInfo` holds three fields:

    byte[]     hash              this block's hash
    BigInteger cummDifficulty    total difficulty at this block
    boolean    mainChain         whether it is canonical

A height may hold several entries — the canonical block plus orphans and losing
forks. Only the `mainChain` entry is emitted:

    <number> <hash-hex> <cumulative-difficulty-decimal>

Note this does **not** include a parent pointer. Parenthood lives in the block
headers, in the separate `blocks` RocksDB, not in the index.

## Why it needs no rskj build

Java deserialisation resolves classes by binary name and checks
`serialVersionUID` plus field compatibility. `src/org/ethereum/db/IndexedBlockStore.java`
declares a stand-in with the matching package, nested-class name, UID
(`5906746360128478753`) and three fields — enough to read what rskj wrote
without compiling rskj.

## Build and run

    curl -sSLo mapdb.jar https://repo1.maven.org/maven2/org/mapdb/mapdb/2.0-beta13/mapdb-2.0-beta13.jar
    javac -cp mapdb.jar -d classes src/org/ethereum/db/IndexedBlockStore.java src/DumpIndex.java
    java -Xmx4g -cp mapdb.jar:classes DumpIndex \
        /path/to/database/mainnet/blocks/index \
        index-dump.txt

MapDB version must match rskj's (`rskj-core/build.gradle`, `mapdbVer`), which is
`2.0-beta13` at the time of writing.

Sort before feeding it to rustock, so writes land in key order and RocksDB can
compact by trivial move:

    sort -n index-dump.txt -o index-dump-sorted.txt
    rustock --import-index-dump index-dump-sorted.txt --data-dir /var/lib/rustock

## Performance

~1,100 records/s, roughly 2.5 hours for RSK mainnet's 9.2M blocks. The cost is
not MapDB: it is constructing an `ObjectInputStream` and running
reflection-driven deserialisation once per record. That is rskj's serialiser, so
a reader has to pay it.
