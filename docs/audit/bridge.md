# bridge — info findings

## FRCR-73 — deprecated `getBtcTxHashesAlreadyProcessed` getter absent

**Verdict: no consensus effect.** A deprecated local-only getter. It answers
RPC questions and cannot appear in a transaction's execution, so its absence
cannot fork a chain — it can only give a caller an error where rskj gives an
answer.

Worth having for API parity, at the bottom of the queue behind anything
transaction-callable. #46's priority order puts local-only getters last for
exactly this reason.
