# addresses-accounts — info findings

## FRCR-774 — no node-level checksum encoding; RPC serializes lowercase addresses

**Verdict: no consensus effect.** EIP-55 checksumming is a display convention.
Addresses are 20 bytes on the wire and in the trie, and case never reaches
consensus. rskj's RPC behaviour here is worth matching for client
compatibility, but nothing about it can fork a chain.
