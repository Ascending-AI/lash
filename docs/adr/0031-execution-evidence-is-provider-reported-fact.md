# Execution evidence is provider-reported fact, never echoed intent

Providers report what actually ran — the served model id, a response identity, a reasoning
output token breakdown, a finish reason — and lash parsed those fields and dropped them,
leaving hosts to label pre-run resolution as "actual" in their audit records: an
unfalsifiable echo that cannot detect routing substitution or a dropped reasoning setting.
We decided evidence is a first-class, typed, optional contract: **`ExecutionEvidence`
carries only what the provider explicitly reported, is never populated from requested or
resolved intent, and absence always means unreported — an explicit zero is information**
(`Some(0) != None`; usage normalization that collapses missing breakdowns to zero is
accounting, not evidence).

Each LLM response carries its own evidence. The turn result additionally carries a
per-attempt ledger: every attempt — successful, failed, retried, or interrupted — becomes an
entry with its outcome, request identity, protocol position, and whatever evidence was
actually observed before the attempt ended. A successful response's evidence stays pure;
failed-attempt facts live in the ledger, because served-model drift, billed failures, and
abort states matter most precisely when the attempt did not complete. Ledger entries carry
an open label describing why the call ran; the label vocabulary is host- and
plugin-supplied, not a core enumeration — internal machinery that makes model calls labels
its own work through the same interface.

(Superseded in part by ADR 0033: the "tagged final-output provenance" idea below was
over-reach — a turn has no single producing model to attribute, so lash does not compute
one. Per-call evidence and the per-call ledger stand; higher-level attribution is host
policy.) ~~"What produced the final output" is a tagged provenance value, never an index inference:
final output can come from a model call, from a terminal value or tool result without any
assistant text (an RLM Final Value ends a turn this way), from host synthesis, or from
nothing (incomplete).~~ Turn activity and trace records project the same evidence types
rather than defining parallel shapes, and the remote mirrors carry them behind a protocol
version bump. Providers implement extraction independently; a provider that cannot report a
field leaves it absent rather than guessing.

## Amendment (FIG-2765): unreported usage is a typed fact, not a zero

Absence of usage on an attempt was being read as "free". It is not: an attempt the
runtime aborted at the cell boundary, or that failed before its usage frame, consumed
provider capacity that nobody reported. Each `AttemptRecord` therefore carries an
`AttemptUsageDisposition` (`Reported`, `UnreportedByProvider`, `UnreportedAfterAbort`,
`UnreportedAfterFailure`) derived from the attempt's outcome and observed usage, never
from intent. The turn's usage ledger writes a `TokenLedgerEntry` for every interrupted
unreported attempt even at zero usage, marked `LedgerUsageDisposition::Unreported`, and
the session report exposes `unreported_attempts` next to the summed counters. A host may
later ask the runtime to reconcile those holes; recovered usage is appended as
`LedgerUsageDisposition::Reconciled` correction rows attributed to the call and attempt
(never rewriting the original row), and the totals sum corrections while
`reconciled_attempts` settles the outstanding count. A lookup only settles a hole when the
provider actually accounts for the call: a generation record whose token counts are absent
or null was found before its accounting landed, so it leaves the attempt registered for the
next sweep, while an explicitly reported zero is a real observation and closes the hole.
Legacy rows and attempt records decode
as `Reported`; remote mirrors and trace projections carry the disposition verbatim.
