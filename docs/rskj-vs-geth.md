# Where rskj differs from Ethereum convention

A running record of places where rskj's behaviour is **not** what the method
name, the Ethereum JSON-RPC spec, or go-ethereum would lead you to expect.

Written because three issues in a row in this repository described
go-ethereum's semantics rather than rskj's, and only reading the Java caught
it. Each entry gives the rskj source, what the difference is, and what it costs
to get wrong.

> **The rule this document exists to enforce:** read rskj before implementing.
> A method name is not a specification, and neither is the Ethereum one.

---

## `eth_pendingTransactions` is the wallet's, not the mempool's

| | |
|---|---|
| **go-ethereum** | all pending transactions in the node's pool |
| **rskj** | only transactions whose sender is an account the node's *own wallet* manages |
| **source** | `co/rsk/rpc/modules/eth/EthModuleWalletEnabled.java`, `EthModuleWalletDisabled.java` |

```java
// EthModuleWalletEnabled
List<Transaction> pendingTxs = transactionPool.getPendingTransactions();
List<String> managedAccounts = Arrays.asList(accounts());
return pendingTxs.stream()
        .filter(tx -> managedAccounts.contains(tx.getSender(...).toJsonString()))
        .collect(Collectors.toList());

// EthModuleWalletDisabled
return Collections.emptyList();
```

**Cost of getting it wrong.** rustock has no wallet, so the faithful answer is
`[]`. Returning the pool instead would present every transaction in the public
mempool as the *caller's own* — wrong in a way the caller cannot detect. A
wallet UI would show strangers' transactions as the user's.

**What to use instead:** `txpool_content`, which reports the pool without
claiming the transactions belong to anyone.

---

## `rsk_getStorageBytesAt` has three answers, not two

| | |
|---|---|
| **Ethereum convention** | a value, or null/zero |
| **rskj** | `0x` + bytes (present), `0x` (present but empty), `0x0` (absent) |
| **source** | `org/ethereum/rpc/Web3Impl.java`, `co/rsk/util/HexUtils.toUnformattedJsonHex` |

```java
return Optional.ofNullable(accountInformationProvider.getStorageBytes(address, key))
        .map(HexUtils::toUnformattedJsonHex)   // "0x" + hex(bytes); "0x" when empty
        .orElse("0x0");                        // absent
```

`0x` and `0x0` look alike and are different facts: a key holding a zero-length
value against a key that is not there. For Bridge state that is "the queue is
empty" against "there is no queue".

**Why the method exists at all** is itself a difference: `eth_getStorageAt`
returns one 32-byte word, and rskj stores precompile state as a single
*variable-length* value at one unitrie key (`Repository.addStorageBytes`). Most
of the Bridge's state — BTC chain head, federation, release request queue, UTXO
set — is unreachable through `eth_getStorageAt`, which silently returns the
first word.

---

## Receipt-proof nodes come back leaf-first, root-last

| | |
|---|---|
| **Intuition** | a proof is ordered root → leaf, as you would walk it |
| **rskj** | the reverse: deepest node first, root last |
| **source** | `co/rsk/trie/Trie.java`, `findNodes` |

```java
List<Trie> subnodes = node.findNodes(key.slice(commonPathLength + 1, key.length()));
if (subnodes == null) return null;
subnodes.add(this);      // <- each level appends ITSELF after recursing
return subnodes;
```

**Cost of getting it wrong.** A consumer that assumes root-first takes the
wrong element as the root, and the proof silently fails to verify against
`receiptsRoot` — with nothing to indicate the ordering was the problem.

### And the nodes alone do not prove anything

A receipt is a *long value*: the leaf carries the value's hash, not its bytes.
`rsk_getTransactionReceiptNodesByHash` therefore cannot yield the receipt on its
own — it must be paired with `rsk_getRawTransactionReceiptByHash`. That is why
the two methods exist together, and a verifier needs both.

A related trap when verifying: a node's hash is `keccak(toMessage(store))`, and
`toMessage` consults the store. **Re-serialising a node whose siblings are
absent — which is exactly what a proof is — produces different bytes and a
different hash.** Hash the message *as received*; do not rebuild it.

---

## How to add an entry

When implementing anything against rskj, if the Java does not match what the
name or the Ethereum spec implies, add a row here with:

1. what each side does,
2. the rskj file and method, with the lines that matter quoted,
3. **what it costs to get wrong** — the failure a consumer would see.

The third is the part worth writing. A difference nobody can be hurt by is
trivia; a difference that silently mislabels data is the reason this file
exists.
