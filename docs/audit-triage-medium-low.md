# Audit triage: medium and low severity findings

**Archive:** `rustock-4796682` · **Date:** 2026-09-22
**Companion to:** `docs/audit-response-2026-09.md` (the five high-severity rows)

## The filter applied

The audit states its baseline explicitly:

> Baseline | RSKIP specification (spec is authoritative; rustock deviations are findings)

rustock's acceptance criterion is the opposite: **agreement with deployed rskj
mainnet**. Where the RSKIP text and rskj disagree, rskj wins and the finding is
not actionable for us — changing rustock to match the spec would *introduce* a
fork.

So every finding below was re-checked against the rskj source tree (rskj
**9.1.0**, `/srv/rskj`), not against the RSKIP. Verdicts:

| Verdict | Count | Meaning |
|---|---:|---|
| **Divergence from rskj** | 13 | rustock differs from deployed rskj. Real. |
| **Spec is stale** | 13 | rustock matches rskj; the RSKIP text is wrong or outdated. No action. |
| **Prototype gap, no consensus effect** | 6 | Local-only getters / mempool policy. Affects RPC answers, not consensus. |
| **Matches rskj already** | 8 | The finding's premise does not hold against rskj. |

15 medium + 25 low = 40 findings.

The 13 real ones fall into three groups, and the grouping matters more than the
individual rows — two of the three are single structural gaps that the audit
saw as several unrelated findings.

---

## Group 1 — Missing block-validity rules (8 findings, one root cause)

**Findings:** FRCR-873 (RSKIP-9), FRCR-573 + FRCR-677 (RSKIP-252),
FRCR-480 (RSKIP-110), FRCR-479 (RSKIP-351), FRCR-481 (RSKIP-180),
FRCR-574 (RSKIP-252 mempool).

These are not six separate omissions. rskj composes a block validator from
~15 rules (`RskContext.getBlockValidationRule`); rustock's `HeaderVerifier`
has 8 header rules plus the root/gas checks in `processor.rs`. Lining the two
up:

| rskj rule | what it rejects | rustock |
|---|---|---|
| `TxsMinGasPriceRule` | tx `gasPrice` < block `minGasPrice` | **missing** (FRCR-873) |
| `BlockTxsMaxGasPriceRule` | tx `gasPrice` > 100 × `minGasPrice` (RSKIP252, fingerroot500) | **missing** (FRCR-573/677) |
| `PrevMinGasPriceRule` | `minGasPrice` outside the ±1/100 vote bound vs parent | **missing** (FRCR-873) |
| `ForkDetectionDataRule` | bad CPV / NU / BN in the merged-mining tag (RSKIP110) | **missing** (FRCR-480) |
| `Rskip92MerkleProofValidator` 960-byte cap | oversized merged-mining merkle proof (RSKIP180) | **missing** (FRCR-481) |
| `ExtraDataRule` | `extraData` over the maximum size | **missing** |
| `BlockUnclesValidationRule` | >10 uncles, uncles older than 7 generations, invalid uncle headers | **missing** |
| `RemascValidationRule` | last transaction is not the REMASC transaction | **missing** |
| `ValidTxExecutionSublistsEdgesRule` | malformed RSKIP144 parallel-execution edges | **missing** |
| RSKIP-351 header version / extension hash | malformed compressed header | **missing** (FRCR-479) |
| `BlockRootValidationRule` | wrong txs/receipts root | present (`processor.rs`) |
| `ProofOfWorkRule` | bad merged-mining PoW | present |
| `BlockDifficultyRule`, `BlockParentGasLimitRule`, `GasLimitRule`, `BlockTimeStampValidationRule`, `BlockParentNumberRule` | — | present |

**Why this is a different kind of divergence from the high-severity four.**
Every one of these is an *accepts-too-much* gap. rustock computes the same
state root as rskj for every block rskj considers valid, so it cannot produce a
wrong answer on the canonical chain. What it can do is **follow a chain rskj
rejects**. That is a consensus failure in the chain-selection sense, not the
state-transition sense, and it needs an attacker who can mine a block — but it
is exactly the scenario a validating node exists to prevent.

It also means replay can never detect any of them: mainnet contains no invalid
block, so the missing rules are never given anything to reject.

**Recommendation.** Build these as one piece of work, not six. rustock already
has the `HeaderValidator` / `ParentHeaderValidator` traits and a composition
point; the rules themselves are small. Suggested order by exposure:
`TxsMinGasPriceRule`, `BlockTxsMaxGasPriceRule`, `PrevMinGasPriceRule` (cheap,
purely arithmetic), then uncles and REMASC, then the RSKIP-110/180/351
merged-mining and header rules.

FRCR-574 (the 80× *propagation* cap) is mempool policy, not consensus — rskj
applies `TxGasPriceCap.FOR_TRANSACTION` in `TxValidatorMaximumGasPriceValidator`
only when accepting into the pool. Worth having; cannot fork.

---

## Group 2 — Opcodes rskj shipped and rustock never implemented (3 findings)

**Findings:** FRCR-274 (TXINDEX), FRCR-275 (DUPN/SWAPN), FRCR-674 (STATICCALL).

The audit rated these LOW because the RSKIPs were never adopted and the opcodes
are "not implemented" — true of the spec, false of rskj. rskj **shipped all
three**, and they were live on mainnet for millions of blocks:

| opcode | rskj | live on mainnet | rustock |
|---|---|---|---|
| `DUPN` (0xa8) | `OpCode.DUPN(0xa8, 2, 2, VERY_LOW_TIER)`, `VM.doDUPN` | genesis → iris300 (#3,614,800), disabled by RSKIP191 | **absent** |
| `SWAPN` (0xa9) | `OpCode.SWAPN(0xa9, 3, 2, VERY_LOW_TIER)`, `VM.doSWAPN` | genesis → iris300 | **absent** |
| `TXINDEX` (0xaa) | `OpCode.TXINDEX(0xaa, 0, 1, BASE_TIER)`, `VM.doTXINDEX` | genesis → iris300 | **absent** |
| `STATICCALL` (0xfa) | gated on RSKIP91 = orchid | from #729,000 only | **was available from block 0** |

Two distinct defects:

1. **rustock was missing three opcodes rskj had.** Executing `0xa8`/`0xa9`/`0xaa`
   in a pre-iris block halts rustock with an undefined-opcode error consuming
   the whole frame, where rskj executes it.
2. **rustock had STATICCALL 729,000 blocks too early** — the mirror image.
   rskj throws `invalidOpCode`; rustock executed it.

There is also a third, sharper variant the audit did not report, found while
checking FRCR-674: **rskj's STATICCALL pops a value word before RSKIP103**
(orchid060, mainnet #1,052,700). `VM.calculateCallValue` special-cases only
DELEGATECALL until RSKIP103, so for mainnet blocks #729,000–#1,052,699 a
STATICCALL consumes *three* stack items, and adds `VT_CALL` (9,000 gas) when the
popped value is non-zero. rustock pops two, always. See *Not fixed here*.

**Are these live on mainnet?** No — and the evidence is good. rustock's
`processor.rs` validates `gas_used`, `paid_fees`, the transactions root, the
receipts root and the logs bloom on **every** block from genesis (the state
root only from #1,591,000, but any of these divergences would move gas or a
receipt). The 2026-09-16 replay of all 9,230,008 blocks passed, so no mainnet
block ever executed any of them. Both windows are also permanently closed —
iris300 and orchid060 are long past — so they can never be exercised on mainnet
again.

They remain reachable on **testnet**, where iris300 is at #2,060,500, and in
any future replay from a different data source.

---

## Group 3 — Wrong testnet activation heights (4 findings)

**Findings:** FRCR-673 (orchid), FRCR-675 (papyrus), FRCR-680 (lovell),
FRCR-681 (reed).

rustock's **mainnet** ladder matches rskj's `config/main.conf` exactly. Its
**testnet** ladder had drifted badly — and by more than the audit reported:

| upgrade | rskj `testnet.conf` | rustock (before) |
|---|---:|---:|
| wasabi100 | 0 | 863,000 |
| twoToThree | 504,000 | 863,000 |
| papyrus200 | 863,000 | 1,580,000 |
| lovell700 | 6,110,487 | 5,735,824 |
| reed800 | 6,835,700 | 6,420,700 |

The shape of the error is telling: rskj's `papyrus200 = 863,000` had been
written into rustock's `wasabi100` slot, and everything below it shifted. rskj
testnet starts *already on wasabi100* (bahamas, orchid, orchid060 and wasabi100
are all height 0), which rustock's ladder did not reflect.

Mainnet is unaffected. Anyone running rustock against testnet would diverge
from block 0.

---

## Group 4 — Other real divergences (2 findings)

### FRCR-78 / FRCR-678 — `getEstimatedFeesForNextPegOutEvent` returns a hardcoded 0

rustock:

```rust
pub fn get_estimated_fees_for_next_pegout<CTX: crate::RskContextTr>(
    _ctx: &mut CTX,
    gas_cost: u64,
) -> Result<PrecompileOutput, PrecompileError> {
    // Return 0 for now
    let output = [0u8; 32];
    Ok(PrecompileOutput::new(gas_cost, output.to_vec().into()))
}
```

rskj `BridgeSupport.getEstimatedFeesForNextPegOutEvent` returns zero only when
`shouldReturnZeroEstimatedFees()`, and otherwise computes a real fee — from
input/output counts before RSKIP305, and from a full peg-out transaction
simulation after it.

This is **not** a local-only getter: `BridgeMethods.java` gives it
`fixedPermission(false)`, so it is transaction-callable, and rustock registers
it as `TransactionCallable` too. A contract that calls it gets `0` from rustock
and a real fee from rskj — divergent execution, divergent state root.

This is the only medium/low finding that can fork the canonical chain today,
and it is the one that is genuinely expensive to fix (it needs the peg-out
transaction-size simulation, the active federation redeem script, and
`feePerKb`). Not fixed here — see below.

### FRCR-674 (part two) — STATICCALL value semantics before RSKIP103

Described in Group 2. Not fixed here — see below.

---

## Fixed in this branch

| Finding | Fix |
|---|---|
| FRCR-674 (part one) | STATICCALL gated on RSKIP91 (orchid, #729,000); before it, the invalid-opcode handler, matching `VM.java` |
| FRCR-673, -675, -680, -681 | testnet activation ladder corrected against rskj `config/testnet.conf`, plus a test transcribing **both** networks' tables as a block |

## Not fixed here, with reasons

| Finding | Why not |
|---|---|
| Group 1 (8 rules) | One coherent piece of work on the validator pipeline, not eight patches. Sized and ordered above. No effect on the canonical chain. |
| FRCR-274, -275 (DUPN/SWAPN/TXINDEX) | Implementable — the semantics are recorded below — but proven unexercised on mainnet and permanently unreachable there. Matters for testnet replay. `doDUPN`/`doSWAPN` also call `program.step()` **twice**, so the PC advances by 2 and the byte after the opcode is skipped; that quirk has to be reproduced, and it is worth doing deliberately rather than in passing. |
| FRCR-674 (part two) | Needs care: pre-RSKIP103 STATICCALL pops a third stack word and may charge `VT_CALL`, and the downstream static-call write-protection interacts with it. Window is #729,000–#1,052,699, permanently closed, proven unexercised. A wrong "fix" is worse than the documented gap. |
| FRCR-78 / -678 | Requires the peg-out transaction simulation. Real and current; the largest single item on this list. |
| FRCR-574 | Mempool policy, not consensus. |

## No action — the RSKIP text is stale, rustock matches rskj

Each verified against the rskj source:

| Finding | RSKIP says | rskj says | rustock |
|---|---|---|---|
| FRCR-277 | HDWalletUtils gas 8,000 / 55,000 / 6,800 / 13,500+500 | `ToBase58Check` 13,000, `DeriveExtendedPublicKey` 107,000, `ExtractPublicKey…` 11,300, `GetMultisigScriptHash` 20,000 + 700/extra key | matches rskj exactly |
| FRCR-278 | `toBase58Check(bytes,uint8)`, `getMultisigScriptHash(uint8,bytes[])` | `…(bytes,int256)`, `…(int256,bytes[])` | matches rskj |
| FRCR-473 | `getRSKDifficulty`, `uint256` params | `getDifficulty(int256)`, `getMinGasPrice(int256)`, `getCoinbaseAddress(int256)` | matches rskj |
| FRCR-474 | fixed 1,000 gas | `BlockHeaderContractMethod.getGas` = `4000 + super` = `4000 + 2·len` | matches rskj |
| FRCR-475 | fixed 4,000 gas | same `4000 + 2·len` | matches rskj |
| FRCR-874 | simpler `isBrokenSelectionRule` pseudocode | `SelectionRule.isBrokenSelectionRule` — 2× paid-fees criterion both ways, hash tiebreak, `maxUncleCount >` | matches rskj line for line |
| FRCR-875 | uncle window 10 | **both** exist: `getUncleGenerationLimit() = 7`, `getUncleListLimit() = 10` | has both, 7 and 10 |
| FRCR-877 | flush when **pool balance** ≥ MPFG × BMGP | `Remasc.payToFederation` compares `payToFederator`, the **per-federator share** | matches rskj |
| FRCR-477 | HEADER (0xfc) pseudo-opcode | declared but falls through to `invalidOpCode` — never implemented | matches rskj (absent) |
| FRCR-273 | ROL/ROR (0x1e/0x1f) | absent from `OpCodes.java` entirely | matches rskj (absent) |
| FRCR-276 | CODEREPLACE | absent from `OpCodes.java` entirely | matches rskj (absent) |
| FRCR-575 | RSKIP-10 header fields | never adopted | matches rskj |
| FRCR-773 | switch condition `>` | rskj uses `>=` — the audit says so itself | matches rskj |

Plus FRCR-376, FRCR-378 (RSKIP-107/108 trie format wording), FRCR-80
(RSKIP-176 UPI concatenation order) and FRCR-478 (RSKIP-177 empty-`ummRoot`
ambiguity), which the audit itself files as `spec`-actionable and which the
whole-chain replay settles empirically: the deployed format is what rustock
reproduces for 9,230,008 blocks.

## No consensus effect — local-only getters and prototype gaps

FRCR-74 (RSKIP-89 getters), FRCR-878 (`getLockWhitelistEntryByAddress`),
FRCR-81 (peg-in rejection sub-reasons), FRCR-682 (RSKIP-535 `baseEvent` header
extension), FRCR-482 / FRCR-676 / FRCR-79 (testnet-only difficulty, Hop
federation members, timestamp relaxation).

The getters are registered in the method table with the correct selector, gas
and `LocalOnly` permission, but have no dispatch arm, so they fall through to
`_ => Ok(PrecompileOutput::new(gas_cost, Bytes::new()))` and answer with empty
bytes. `LocalOnly` means they are reachable only through `eth_call`, never from
a transaction, so they cannot affect consensus — they give wrong RPC answers.
Worth finishing; not urgent.

---

## What the exercise says about the audit

The audit's severity ranking is calibrated to its own baseline, and that
baseline inverts the ranking for us in both directions:

- Three findings rated **LOW** (FRCR-274, -275, -674) are real rskj
  divergences, in the EVM, that rustock had for millions of blocks of history.
  They were rated low because the *RSKIPs* were never adopted — but rskj
  shipped them anyway.
- Eight findings rated **MEDIUM** or **LOW** across six RSKIPs are one
  structural gap in the block validator.
- Nine findings rated **MEDIUM** are stale spec text, where acting on the
  recommendation would have introduced a fork.

Net: of the 40 medium/low rows, 13 are actionable, and the two with real teeth
(the missing validator rules, and `getEstimatedFeesForNextPegOutEvent`) are not
the ones the severity column points at. Reading the reports against rskj rather
than against the RSKIP is what separates them — which is the same conclusion the
high-severity pass reached with FRCR-77.
