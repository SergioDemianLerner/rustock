import co.rsk.trie.*;
import org.ethereum.datasource.HashMapDB;
import org.ethereum.util.RLP;
import java.util.*;
import java.util.stream.*;

/** Reproduces rskj's SnapshotProcessor.processStateChunkRequestInternal blob. */
public class Chunk {
    public static void main(String[] a) throws Exception {
        int keys = Integer.parseInt(a[0]);
        long from = Long.parseLong(a[1]);
        long to = Long.parseLong(a[2]);

        TrieStore store = new TrieStoreImpl(new HashMapDB());
        Trie t = new Trie(store);
        for (int i = 0; i < keys; i++) {
            byte[] key = new byte[]{(byte)(i >> 8), (byte)i, (byte)(i*7), (byte)(i*13)};
            byte[] val;
            if (i % 4 == 0) { val = new byte[64]; for (int j = 0; j < 64; j++) val[j] = (byte)(j + i); }
            else val = new byte[]{(byte)i, 2, 3};
            t = t.put(key, val);
        }
        store.save(t);
        byte[] root = t.getHash().getBytes();

        TrieDTOInOrderIterator it = new TrieDTOInOrderIterator(store, root, from, to);

        List<byte[]> preRootNodes = it.getPreRootNodes().stream()
            .map(x -> RLP.encodeList(RLP.encodeElement(x.getEncoded()),
                                     RLP.encodeElement(nz(x.getLeftHash())))).collect(Collectors.toList());
        byte[] preRootNodesBytes = !preRootNodes.isEmpty()
            ? RLP.encodeList(preRootNodes.toArray(new byte[0][0])) : RLP.encodedEmptyList();

        List<byte[]> trieEncoded = new ArrayList<>();
        TrieDTO first = it.peek();
        TrieDTO last = null;
        while (it.hasNext()) {
            TrieDTO e = it.next();
            if (it.hasNext() || it.isEmpty()) { last = e; trieEncoded.add(RLP.encodeElement(e.getEncoded())); }
        }
        byte[] firstNodeLeftHash = RLP.encodeElement(first.getLeftHash());
        byte[] nodesBytes = RLP.encodeList(trieEncoded.toArray(new byte[0][0]));
        byte[] lastNodeHashes = last != null
            ? RLP.encodeList(RLP.encodeElement(nz(last.getLeftHash())), RLP.encodeElement(nz(last.getRightHash())))
            : RLP.encodedEmptyList();
        List<byte[]> postRootNodes = it.getNodesLeftVisiting().stream()
            .map(x -> RLP.encodeList(RLP.encodeElement(x.getEncoded()),
                                     RLP.encodeElement(nz(x.getRightHash())))).collect(Collectors.toList());
        byte[] postRootNodesBytes = !postRootNodes.isEmpty()
            ? RLP.encodeList(postRootNodes.toArray(new byte[0][0])) : RLP.encodedEmptyList();

        byte[] chunkBytes = RLP.encodeList(preRootNodesBytes, nodesBytes, firstNodeLeftHash,
                                           lastNodeHashes, postRootNodesBytes);

        System.out.println("KEYS " + keys + " FROM " + from + " TO " + to);
        System.out.println("ROOT " + hex(root));
        System.out.println("TOTAL " + TrieDTO.decodeFromMessage(store.retrieveValue(root), store, true, root).getTotalSize());
        System.out.println("NODES " + trieEncoded.size());
        System.out.println("BLOB " + hex(chunkBytes));
    }
    static byte[] nz(byte[] b) { return b == null ? new byte[0] : b; }
    static String hex(byte[] b) {
        if (b == null) return "null";
        StringBuilder s = new StringBuilder();
        for (byte x : b) s.append(String.format("%02x", x));
        return s.toString();
    }
}
