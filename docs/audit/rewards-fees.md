# rewards-fees — info findings

## FRCR-876 — 1% federation cut paid but absent from RSKIP-15

**Verdict: spec is stale.** The cut is real and both implementations pay it.

rskj carries it as configuration rather than spec text —
`rskj-core/src/main/resources/remasc.json` sets `"federationDivisor": 100` for
every network. rustock's `remasc.rs` sets `federation_divisor: 100` in each of
its three network configurations, with the comment *"Federation receives
`1/federation_divisor` of the synthetic reward (default 100 = 1%)"*.

The RSKIP simply does not describe a payment the deployed protocol makes. That
is a gap in the document, and removing the cut to match it would change every
REMASC payout from genesis.
