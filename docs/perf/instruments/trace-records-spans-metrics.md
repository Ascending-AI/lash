# Trace records, spans and operational metrics

The [instrumentation contract](../../../crates/lash/docs/instrumentation-contract.md)
is the registry of spans, attributes and metrics, and
[durable tracing](../../architecture/tracing.md#integration-status) names the
production producer of each logical record. An entry with no producer is
deleted (FIG-5658). The emission laws run a real core over SQLite memory
stores with a recording `TraceSink` and in-memory OpenTelemetry SDK exporters:

```sh
kiln test //crates/lash-facade-tests:facade_tracing__test --test_arg=--exact \
  --test_arg=tests::trace_emission::a_served_turn_exports_one_invoke_agent_for_its_committed_terminal
```

| Instrument | Kind | Producer |
|---|---|---|
| `turn_started` | record | the session actor after `turn.admit` |
| `turn_completed`, `invoke_agent` | record, span | the session actor after the commit that writes the turn's terminal |
| `execute_tool` | live span | the execution that ran the call |
| `lash.tool_intent`, `lash.tool_intent.admitted` | spans | the host ingress, at the ledger's admission and retained settlement |
| `lash.process` | span | the process actor after `process.terminal` |
| `lash.parked_work.parks` | counter (parks, by `kind` and `reason`) | the session or process actor after the commit that parks it |

These laws prove emission, not cost: they carry no timing and measure no
trace sink.

The `perf-witness` runtime-work collector counts checkpoint-body hash passes
and hashed bytes at the store's hashing sites. It has no body-copy counter:
checkpoint bodies are `Arc<[u8]>` shared from encoding to the store's bind,
so no production site copies one, and a counter that nothing increments
could not fail its zero budget.

## Physical turn breakdown

`just profile-turn <replay-workload> --turn N --store sqlite` prints a
send-through-outcome phase table and SQL shape/store-origin rows. Its sidecar
retains the physical profile used for the table. `--store postgres` is accepted
only inside `kiln gate`, against the gate's private server. Each PostgreSQL
statement is reported as an attempted statement round trip; pool validation,
protocol control and wire bytes are excluded.

The terminal owner's `TurnCompleted.profile` also drives production child
spans and the `lash.turn.*` work attributes. It is content-free and bounded,
with explicit omissions. The [architecture contract](../../architecture/tracing.md#turn-physical-profiles)
defines its population, limits, callback coverage and recovery semantics.
Phase walls overlap; the CLI partitions by the deepest active interval so its
exclusive rows sum to the observed turn wall. SQL rows count once at the
physical execution sites even when operation windows nest. Semantic graph
write rows name inserted node kinds and canonical bytes, not provider callback
entries or disk traffic. Failed/rolled-back attempts can have physical work.

Connection queue/gate waits, PostgreSQL pool checkout, and worker checkout/IPC
are measured at their actual boundaries. Lease wait and fsync clocks are
unavailable in the warm owner window. They are not estimated by subtracting
known spans from the total.

`just profile-recovery` runs the SQLite owner-kill/re-exec control and prints
its raw phase milestones. Replay receipts retain individual successful
session-turn `send()` through `outcome()` latencies as `session_wall_ms` and
print their nearest-rank p95/max separately from round-average summaries.
Setup rounds are excluded from both tail populations. These are closed-loop
replay tails, not offered-load capacity measurements.

Cold owners carry the successful physical claim into their first owner pass.
The `claim` child measures that store operation, rather than inventing lease
wait from a turn's unexplained wall time. A batch claim's SQL is assigned once,
to its first actor; every member can retain the shared claim wall. Warm resident
turns have no claim. A cold profile can begin before facade send acceptance;
the replay reader joins by the durable turn id and reports that full wall.
