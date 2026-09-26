import co.rsk.trie.*;
import co.rsk.core.RskAddress;
import org.ethereum.datasource.HashMapDB;
import java.lang.reflect.*;

public class Probe {
    public static void main(String[] a) throws Exception {
        // Build a small trie with rskj's own Trie.
        TrieStore store = new TrieStoreImpl(new HashMapDB());
        Trie t = new Trie(store);
        int keys = Integer.parseInt(a[0]);
        for (int i = 0; i < keys; i++) {
            byte[] key = new byte[]{(byte)(i >> 8), (byte)i, (byte)(i*7), (byte)(i*13)};
            byte[] val = (i % 4 == 0)
                ? new byte[64]
                : new byte[]{(byte)i, 2, 3};
            if (i % 4 == 0) for (int j = 0; j < 64; j++) val[j] = (byte)(j + i);
            t = t.put(key, val);
        }
        store.save(t);
        byte[] root = t.getHash().getBytes();
        System.out.println("ROOT " + bytesToHex(root));

        // Walk it the way the snap server does.
        TrieDTOInOrderIterator it = new TrieDTOInOrderIterator(store, root, 0, Long.MAX_VALUE);
        int n = 0;
        while (it.hasNext()) {
            TrieDTO d = it.next();
            System.out.println("NODE " + n + " encoded=" + bytesToHex(d.getEncoded())
                + " source=" + bytesToHex(d.getSource())
                + " left=" + bytesToHex(d.getLeftHash())
                + " right=" + bytesToHex(d.getRightHash())
                + " childrenSize=" + d.getChildrenSize().value
                + " totalSize=" + d.getTotalSize());
            n++;
        }
        System.out.println("COUNT " + n);
    }
    static String bytesToHex(byte[] b) {
        if (b == null) return "null";
        StringBuilder s = new StringBuilder();
        for (byte x : b) s.append(String.format("%02x", x));
        return s.toString();
    }
}
