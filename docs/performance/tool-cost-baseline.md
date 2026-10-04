# Controlled tool cost receipts

FIG-4868 preserves the predecessor of the tool execution cutover. Its archived
receipts below are immutable. The current fixture captures the native Run route
through A/X/D/V; it records the runtime source revision supplied by the caller.
Instrumentation is opt-in; the fixture calls `SessionHandle::send()` and lets the engine drive the Run.

The boundary starts at send, after an admitted session has settled, and ends
when the consuming model has seen every result and the invocation tree has
settled through scope close. External Deferred resolution belongs to that
boundary. Each sample has a private Restate server double and SQLite memory
stores. Session creation is excluded; Run admission, model work, incorporation,
retirement and terminal work are included.

The raw receipt retains every journal payload as base64, parent IDs, journal
indices, endpoint frame byte counts, HTTP body byte counts and timings, and SQL templates
with expanded byte lengths. Bound SQL values are not retained. ACKs and replay
traffic belong to endpoint transport bytes, not the raw journal census.

Source commands count SDK run/call/send/timer/awakeable issuance separately
from raw engine entries, which also contain state operations, input/output and
notifications. The historical `14 + 19N` estimate and final `1 + 3N` budget
apply to the tool route under stated assumptions, not to a whole Run or raw
engine records. A source run has its own command and completion notification.

The Done matrix uses widths 1, 2 and 16 with 32 B, 8 KiB, 256 KiB, 64 KiB and
1 MiB request and output markers. Other branch probes use the small payload.
A receipt is one structural sample, not a latency distribution. Double timings
include tracing; archive work follows the timed boundary. They establish no production latency claim.
The later paired live comparison and quiet-host release run remain external.

The native census law is
`tool_cost::tests::l15_census_tracks_native_run_records_and_all_descendants`.
It checks widths 1/2/16, logical call identity at every A/X/D/V and incorporation
boundary, zero child/group invocations, and reconciliation of all raw journals
and payload bytes. The predecessor bucket tests were retired with that route;
they are represented by the archive, not regenerated against a newer runtime.

Capture new receipts with `kiln run //crates/lash-perf:tool_batch_baseline__bin`
and explicit `--source-sha` and `--out` arguments. To reconcile a previously
captured predecessor receipt log without executing its fixture again:

```sh
. ./env.sh
scripts/tool-batch-baseline.sh --archive-root <directory> --receipt-log <log>
```

The archive includes compressed raw JSONL, an independently reconciled summary
and a manifest. `tool_cost_census` refuses incomplete journals, missing
ancestors, payload byte/copy mismatches and SQL/transport count mismatches.
Explicit isolated declarations and operation Run transfer have no predecessor
API: their eventual routes are owned by A01/D04 and O01/O02 respectively, and
are reported as unavailable on this baseline rather than assigned a zero cost.

The 1 MiB predecessor refusal is retained as evidence. Its JSON byte-array
output exceeds the pinned decoder's 1,000,000-node guard. Payload storage and
child settlement return code 400, leaving a rank unseated. These three receipts
end at that refusal prefix: consumption and scope-close latency are null,
unfinished waits are marked censored, and the raw failures are retained. They
are not completed Done samples. The runtime decoder is unchanged.

The independent summary decodes call-target notifications and retained Run
completions, checks all descendants, and reconstructs the tool route from
request binding through incorporation and close. It exposes Ready/read-rank
responses, physical presentation records, bytes by service, SQL transactions
with overlapping table roles, and opener input-starvation intervals. SQL
worker grouping follows connection execution order; table roles do not assert
which concurrent invocation issued a statement. Application storage and engine
storage remain separate: the double has no SQL engine store.

Tool-admission-to-incorporation time is derived from the opener's observed
request-binding command and next model command after incorporation, when the
opener has one endpoint attempt. Replayed and incomplete boundaries have no
such interval. Full Run latency is measured from send. Neither timing is a
production acceptance result.

Reported retry repeats every logical call once. External Deferred resolves
retained completion keys. Declared start returns a native child session's final
value. Cancellation returns typed Run cancellation. The RLM race observes its
winner through a later Deferred tool before resolving its surviving losers
and that gate. The race returns the winner; scope close drains the settled
losers. The predecessor refuses a second await of raced tool handles. Process
transfer executes real compiled processes, with two tool calls across the
process segment budget, then awaits their handles individually; turn transfer cuts
the native turn. Successor invocations must
appear in the raw population. All other fixture branches use the standard
protocol and the small marker payload.

Process transfer keeps the enclosing turn at its normal 10,000-effect budget
and cuts process segments after one effect. Its canonical material counts two
logical tool calls per process; the width counts processes. The other small
branches count one logical tool call per member, plus the explicit race gate.

Pinned receipts are in
[`crates/lash-perf/testdata/tool-cost/e4735b406286c266e6ef2965242634afcd35d6a5`](../../crates/lash-perf/testdata/tool-cost/e4735b406286c266e6ef2965242634afcd35d6a5).
The 32 B Done samples report:

| Width | Whole Run source | Whole Run raw | Tool-route source | Tool-route raw | Application SQL transactions | Opener SDK waits |
|---:|---:|---:|---:|---:|---:|---:|
| 1 | 74 | 528 | 39 | 296 | 27 | 57 |
| 2 | 94 | 671 | 59 | 443 | 30 | 58 |
| 16 | 362 | 2597 | 327 | 2369 | 59 | 63 |

At every width, the recorded Ready subscription and first consuming read
violate the historical branch assumptions. The singleton tool route contains
26 calls, ten recorded runs, one send and two awakeable completions. It has no
new awakeable. The historical 33 remains a conditional source estimate;
these actual receipts fail the four-record singleton and `1 + 3N` controls.
The whole Run includes another 35 source commands for common work.

The process-transfer fixture reuses one definition. The default VM queue's
two-item bound refused definition resolution at the ninth admission in the
16-process probe. This fixture admits `2 * width + 4` queued items, recorded in
its environment, while retaining the default worker count and deadlines.
The compiler/executor budgets are unchanged. Final proof includes thirteen
executed Rust laws, one independent archive law and the failed root-only census
witness. The raw archive contains 33 completed samples and three explicit
codec-refusal prefixes.
