# Trace records, spans and operational metrics

The [instrumentation contract](../../../crates/lash/docs/instrumentation-contract.md)
is the registry of spans, attributes and metrics, and
[durable tracing](../../architecture/tracing.md#integration-status) names the
production producer of each logical record. An entry with no producer is
deleted (FIG-5658). The emission laws run a real core over SQLite memory
stores with a JSONL `TraceSink` and in-memory OpenTelemetry SDK exporters:

```sh
kiln test //crates/lash:lash__unit_test --test_arg=--exact \
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
