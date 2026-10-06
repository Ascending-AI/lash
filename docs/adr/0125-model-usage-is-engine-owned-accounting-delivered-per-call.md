# 0125: Model usage is engine-owned accounting delivered per call

## Status

Replaced by [ADR 0127](0127-usage-is-result-data-hosts-meter-spend.md). A
store test still names this file; it is deleted with that test.

## Note

This decision kept an engine-owned usage ledger: a meter row admitted before
each provider dispatch and a journaled settlement delivered per call. ADR 0127
makes usage data on the model call's recorded result and leaves spend metering
to the host at the `Provider` seam.
