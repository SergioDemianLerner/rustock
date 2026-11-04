# Audit re-triage (#46)

Re-checking the bulk RSKIP compliance review in archive `rustock-4796682`
(109 reports, 64 findings, 455 checklist assertions) against **deployed rskj**
rather than against the RSKIP text.

## Why the baseline matters

The audit states its own baseline:

> Baseline | RSKIP specification (spec is authoritative; rustock deviations are findings)

rustock's acceptance criterion is the opposite: **agreement with deployed rskj
mainnet**. Where the RSKIP and rskj disagree, rskj wins, and "fixing" rustock
to match the spec would *introduce* a fork.

Re-checking the 40 medium/low findings inverted the severity column in both
directions (see [audit-triage-medium-low.md](../audit-triage-medium-low.md)):
13 of 40 were stale spec text where acting on the recommendation would have
forked, while three findings rated LOW were real EVM divergences. The same
inversion is expected in the remainder — and the first pass found it again.

## Verdict vocabulary

| verdict | meaning |
|---|---|
| **Divergence from rskj** | rustock differs from deployed rskj. Real. |
| **Spec is stale** | rustock matches rskj; the RSKIP text is wrong or outdated. No action. |
| **No consensus effect** | local-only getter, RPC formatting, mempool policy. |
| **Premise does not hold** | the finding describes code that is not there, or is there and correct. |
| **Closed window, unexercised** | a genuine divergence in a permanently closed activation window, proven unexercised by the whole-chain replay. |

## Where the audit stands

| scope | status |
|---|---|
| 7 high findings (6 distinct defects) | done — #43, [audit-response-2026-09.md](../audit-response-2026-09.md) |
| 15 medium + 25 low | done — [audit-triage-medium-low.md](../audit-triage-medium-low.md), #43 and #44 |
| **17 info findings** | **done — this directory** |
| **348 checklist rows marked OK** | **not started** — the remaining bulk |
| 111 RSKIPs the audit never reviewed | split out to #149 |

## The 348 OK rows

The findings tables were triaged. Each report also carries a **compliance
checklist** whose rows are marked OK, and nobody has re-checked those. A row
marked OK was checked against the RSKIP text, which is not our acceptance
criterion — so an OK row against a stale RSKIP can hide a divergence from rskj,
which is the direction that matters.

Order of work is by **blast radius**, not by the audit's severity column:

1. Anything transaction-callable on the Bridge.
2. EVM opcode availability and gas, per activation.
3. Block/header validity rules.
4. Local-only getters last — wrong RPC answers cannot fork.

## Cluster files

| file | findings |
|---|---|
| [evm.md](evm.md) | FRCR-279, -281, -577 |
| [storage-state.md](storage-state.md) | FRCR-373, -375, -377, -379 |
| [rewards-fees.md](rewards-fees.md) | FRCR-876 |
| [bridge.md](bridge.md) | FRCR-73 |
| [addresses-accounts.md](addresses-accounts.md) | FRCR-774 |
| [misc.md](misc.md) | FRCR-173, -174, -175, -176, -177, -374, -576 |

## Result of the info pass

17 findings. **One was not info.**

| verdict | count |
|---|---:|
| Spec is stale / observation only | 9 |
| Premise does not hold | 3 |
| No consensus effect | 3 |
| Absent from rustock *and* from mainnet consensus | 1 |
| **Divergence from rskj** | **1 — FRCR-279** |

[FRCR-279](evm.md#frcr-279) was filed INFO as "no distinct RSKIP169 activation
gate". It is a real EVM divergence across a ~1.22M-block window, in a
permanently closed range that the whole-chain replay proves unexercised. The
severity column was wrong by three levels.

Archive: `/root/doc/rustock-4796682.zip`.
