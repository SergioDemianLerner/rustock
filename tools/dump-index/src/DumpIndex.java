import org.ethereum.db.IndexedBlockStore.BlockInfo;
import org.mapdb.DB;
import org.mapdb.DBMaker;
import org.mapdb.DataIO;
import org.mapdb.Serializer;

import java.io.*;
import java.math.BigInteger;
import java.util.ArrayList;
import java.util.List;
import java.util.Map;

/**
 * Dumps rskj's MapDB block index to a flat file rustock can read sequentially.
 *
 * Why this exists: the canonical-chain flag and cumulative difficulty live only
 * in this MapDB file, encoded with Java object serialisation. Deriving them
 * instead -- walking parentHash back from the tip and re-summing difficulty --
 * takes about five hours on RSK mainnet, because the walk is 9.2M dependent
 * random lookups that cannot be batched or prefetched. The data is already here;
 * it just needed a reader that speaks Java.
 *
 * Output format, one record per canonical block, ASCII, newline-separated:
 *     <number> <hash-hex> <cumulative-difficulty-decimal>
 *
 * Only mainChain entries are emitted: a height can hold several BlockInfos
 * (orphans and losing forks) and rustock's block_numbers wants the canonical one.
 */
public final class DumpIndex {

    /** Mirrors rskj's IndexedBlockStore.BLOCK_INFO_SERIALIZER. */
    static final Serializer<List<BlockInfo>> BLOCK_INFO_SERIALIZER = new Serializer<List<BlockInfo>>() {
        @Override
        public void serialize(DataOutput out, List<BlockInfo> value) throws IOException {
            throw new UnsupportedOperationException("read-only");
        }

        @Override
        @SuppressWarnings("unchecked")
        public List<BlockInfo> deserialize(DataInput in, int available) throws IOException {
            int size = DataIO.unpackInt(in);
            byte[] data = new byte[size];
            in.readFully(data);
            try (ObjectInputStream ois = new ObjectInputStream(new ByteArrayInputStream(data))) {
                return (List<BlockInfo>) ois.readObject();
            } catch (ClassNotFoundException e) {
                throw new IOException(e);
            }
        }
    };

    public static void main(String[] args) throws Exception {
        if (args.length != 2) {
            System.err.println("usage: DumpIndex <path-to-blocks/index> <output-file>");
            System.exit(2);
        }
        File indexFile = new File(args[0]);
        if (!indexFile.isFile()) {
            System.err.println("not a file: " + indexFile);
            System.exit(2);
        }

        long started = System.currentTimeMillis();
        // readOnly so the source is never mutated: the import can be re-run.
        DB db = DBMaker.fileDB(indexFile).readOnly().make();
        Map<Long, List<BlockInfo>> index = db.hashMapCreate("index")
                .keySerializer(Serializer.LONG)
                .valueSerializer(BLOCK_INFO_SERIALIZER)
                .makeOrGet();

        long emitted = 0, skipped = 0, multi = 0, maxNumber = -1;
        StringBuilder sb = new StringBuilder(1 << 16);

        try (BufferedWriter out = new BufferedWriter(new FileWriter(args[1]), 1 << 20)) {
            for (Map.Entry<Long, List<BlockInfo>> e : index.entrySet()) {
                List<BlockInfo> infos = e.getValue();
                if (infos == null || infos.isEmpty()) { skipped++; continue; }
                if (infos.size() > 1) multi++;

                BlockInfo canonical = null;
                for (BlockInfo bi : infos) {
                    if (bi.isMainChain()) { canonical = bi; break; }
                }
                // A height with no mainChain entry is a pure side branch.
                if (canonical == null) { skipped++; continue; }

                long number = e.getKey();
                if (number > maxNumber) maxNumber = number;

                sb.setLength(0);
                sb.append(number).append(' ');
                for (byte b : canonical.getHash()) sb.append(String.format("%02x", b));
                BigInteger td = canonical.getCummDifficulty();
                sb.append(' ').append(td == null ? "0" : td.toString()).append('\n');
                out.write(sb.toString());

                if (++emitted % 100_000 == 0) {
                    double secs = (System.currentTimeMillis() - started) / 1000.0;
                    System.out.printf("emitted %,d records (%.0f/s), at #%d%n",
                            emitted, emitted / Math.max(secs, 0.001), number);
                }
            }
        }
        db.close();

        double secs = (System.currentTimeMillis() - started) / 1000.0;
        System.out.printf("DONE: %,d canonical records, %,d heights skipped, %,d heights with forks, tip #%d, %.1fs%n",
                emitted, skipped, multi, maxNumber, secs);
    }
}
