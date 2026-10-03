# Concurrent SDK acceptance

H05 accepts the dependency consumed by H01 and the laws landed by H02, H03
and H04. Run its selected developer cases from a Kiln fork:

```sh
. ./env.sh
python3 scripts/restate-sdk-acceptance.py
```

The recipe checks the full workspace git revision against the locked SDK and
macros sources, requires one locked SDK/macros/shared-core closure, and checks
that `lash-internal-restate` is the sole direct SDK dependency owner. It then
runs 34 existing cases on six owning targets, with full test names and no test
verdict cache. It verifies that the JUnit reports contain exactly those
successful executions, including the union of the adapter's shards. Reports
and an aggregate receipt go under `.tmp/restate-sdk-acceptance`. A partial
receipt has `complete: false`.

Use `--output-dir <path>` to retain evidence at a chosen path. `--group <name>`
selects one owning target when an invocation needs correction. Do not repeat
passed groups for confidence.

| Group | Cases | Contract |
| --- | ---: | --- |
| `handlers` | 19 | Existing state, calls, sends, workflow attach, promises, awakeables, durable timers, retry exhaustion, workflow identity, cancellation and deployment pinning; H02's eight started-run laws. |
| `wake` | 2 | H03's V6 and V7 open-stream wake laws, including retry, cancel and termination. |
| `adapter` | 9 | Shared Endpoint, stored-shape readers, route and workflow-key identity, typed serde refusal and limits, legal and oversized HTTP/2 input, and long legal replay. |
| `driver` | 1 | The host submits and the engine alone drives the turn. |
| `host` | 2 | Replayed acceptance submits once; exclusive object acceptance frees its lock while a shared handler waits. |
| `transition` | 1 | The facade's historical transition journal refuses the wrong generation before decoding or executing, and the predecessor retains its result and drain lane. |

The double discovers real SDK endpoints through their `/discover` manifest.
The deployment and host cases bind the real `RestateEngine` over SQLite stores
alongside host handlers. `RestateTestBackend` remains the owner of that engine
and a chosen `StoreSet`; it is not a replacement effect journal.

## Accepted dependency and predecessor evidence

H01 landed as `476a273fd8`. The accepted SDK and macros are 0.12.1 at
`c25608305340e431dff8808a3303bc3106762d6c`, from
`https://github.com/SamGalanakis/sdk-rust`, with registry shared-core 7.0.3.
The dependency disables default features and enables `http_server`. H01's
intake receipt confirms the published cleanup branch revision. SDK-owner
tests supplement the Lash evidence below; they do not replace it.

| Predecessor | Landed commit | Lash evidence accepted at intake |
| --- | --- | --- |
| H02, FIG-4871 | `59c8b3d51e` | Eight laws, 78 scenarios on double V6/V7, streaming and always-replay. Old SDK 0.11.1 fails the partial-replay law once. |
| H03, FIG-4872 | `ba9be7b60b` | Double: 2 cases and 10 scenarios. Live Restate 1.7.13, negotiated V7 asserted: 1 case and 5 scenarios. Live replay: 1 case and 3 scenarios. Old SDK fails double V6/V7 and live V7 with all receipts durable but the handler still parked. |
| H04, FIG-4873 | `3ab13e278d` | 64 compiler targets and four passing target actions for the host import cutover. The old report gives target counts, so it establishes no additional executed-case total. |

The predecessor reports are
`/workspace/notes/lash/tasks/lanes/fig-4870.report.md` through
`fig-4873.report.md`. H03's live scenarios cover joined wake and started wake,
retryable failure, cancellation and kill. Its replay leg covers started wake,
retry and kill. This developer recipe starts no live service and produces no
new live receipt. The H05 lane receipt must keep fresh and inherited counts
separate, as its task's minimal proof contract requires.

## Rules retained for the Run coordinator

L01 proves that all three bodies enter before completion and retain separate
receipts in forced completion order 2/0/1. L02's double law cuts at seven
execution, proposal and output boundaries in four modes, reuses durable
results, and reruns only unfinished bodies under the same call IDs. L03's
double cancellation law covers six journal-order cases in those four modes.
L21 retains real host/engine bindings, identity, typed decoding, generation
refusal and pinned old routes.

Concurrent work uses consuming `.start()` before awaiting any result. The
borrowed joined shape remains a wake witness; it cannot replay partial
results. Every issued handle must be awaited before successful return:
dropping a future does not settle it, and successful return drops unfinished
bodies. The SDK's bounded retry budget counts failures since the last
recorded invocation entry, so a sibling receipt restarts that budget.
C01/C02 own handle draining and the recorded per-call reported-retry schedule.
On closed replay streams cancellation reaches the SDK after executing bodies
finish. These constraints are preserved, not treated as unproved spike gaps.

The 78 H02 scenarios and 10 H03 double scenarios are scenario counts inside
ten libtest cases, not 88 additional cases. They prove SDK concurrency and
replay mechanics; the later Run final/rank, publication and handover tickets
retain their own semantic laws and cost acceptance.
