# evm — info findings

## FRCR-279 — no distinct RSKIP169 activation gate on EXTCODEHASH

**Verdict: divergence from rskj — closed window, unexercised.** Filed INFO;
it is not info.

### What rskj does

`VM.doEXTCODEHASH` calls `program.getCodeHashAt(address, activations.isActive(RSKIP169))`,
and `RSKIP169` activates at **iris300** (`reference.conf:43`). The two branches
are `MutableRepository.getCodeHashStandard` and `getCodeHashNonStandard`, and
they differ in exactly one case:

| account state | pre-RSKIP169 (`NonStandard`) | post-RSKIP169 (`Standard`) |
|---|---|---|
| does not exist | `ZERO_HASH` | `ZERO_HASH` |
| exists, not a contract | `keccak256("")` | `keccak256("")` |
| **exists, is a contract, code key has no value** | **`ZERO_HASH`** | **`keccak256("")`** |

rskj comments the old branch itself: *"Returning ZERO_HASH is the non standard
implementation we had pre RSKIP169 implementation and thus me must honor it."*

### What rustock does

`rsk_extcodehash` (`crates/execution/src/rsk_instructions.rs`) has **no gate**:

```rust
let result = if load.is_empty {
    B256::ZERO
} else if load.account.is_empty_code_hash() || load.account.code_hash.is_zero() {
    KECCAK_EMPTY
} else {
    load.account.code_hash
};
```

The third row is always `KECCAK_EMPTY` — the *Standard*, post-iris300 answer.
Correct from iris300 onward; wrong before it.

### The window

EXTCODEHASH exists from **papyrus200 (#2,392,700)** (RSKIP140) and the
behaviour is only correct from **iris300 (#3,614,800)**. So the divergence
spans **#2,392,700 – #3,614,799, about 1.22M blocks**, and that window is
permanently closed.

### Why it is not being fixed as a defect

The whole-chain replay executes every mainnet block from genesis and checks
`gas_used`, `paid_fees` and the receipts root. A different EXTCODEHASH result
feeds a contract's computation and would move a state root, so an exercised
occurrence would have failed the replay. It did not — which places this in the
same family as the `DUPN`/`SWAPN`/`TXINDEX` carve-out: a real divergence in a
closed, proven-unexercised window.

It still matters for **testnet replay**, where the activation ladder differs,
and it should be implemented if that is ever in scope. The fix is a hardfork
gate plus the `ZERO_HASH` branch, and it needs the "is a contract but has no
code value" condition to match rskj's `isContract` exactly — a wrong fix here
is worse than the documented gap, because it would move results inside the
window that currently agree.

### Why the audit rated it INFO

The audit checked whether rustock *had* a gate named for RSKIP169 and found
none. It did not compare the two branches' return values, so the finding reads
as a naming/structure observation rather than a behavioural difference. Reading
rskj rather than the RSKIP is what surfaces it.

---

## FRCR-281 — RSKIP552 gate defined but unused

**Verdict: premise does not hold.** `RskHardforkConfig::has_rskip552` exists
(`hardfork.rs:579`) and is covered by the activation-table tests. A gate that
is defined and tested but whose call site has not landed yet is not a
divergence; it is the ordinary shape of staged work.

## FRCR-577 — EIP-2028 calldata reduction correctly gated at arrowhead600

**Verdict: observation, no action.** The audit is confirming the
implementation is right. Recorded so the row is not re-examined later.
