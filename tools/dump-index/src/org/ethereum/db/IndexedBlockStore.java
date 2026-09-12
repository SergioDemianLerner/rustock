package org.ethereum.db;

import java.io.Serializable;
import java.math.BigInteger;

/**
 * Stand-in for rskj's class of the same name, declaring only the nested
 * BlockInfo that its MapDB index stores.
 *
 * Java deserialisation resolves classes by binary name and checks
 * serialVersionUID plus field compatibility -- it does not need the original
 * jar. So matching the package, the nested-class name
 * (org.ethereum.db.IndexedBlockStore$BlockInfo), the UID and the three field
 * names/types is sufficient to read what rskj wrote, without building rskj.
 */
public class IndexedBlockStore {

    public static class BlockInfo implements Serializable {
        private static final long serialVersionUID = 5906746360128478753L;

        private byte[] hash;
        private BigInteger cummDifficulty;
        private boolean mainChain;

        public byte[] getHash() { return hash; }
        public BigInteger getCummDifficulty() { return cummDifficulty; }
        public boolean isMainChain() { return mainChain; }
    }
}
