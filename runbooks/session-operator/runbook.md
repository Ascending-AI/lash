# Session operator verbs on live Restate and PostgreSQL

Run from an isolated Kiln fork after sourcing `env.sh`:

```sh
kiln gate lash <fork> -- just session-operator-e2e
```

The recipe builds the operator host and VM worker through the shared Buck2
pool. It executes six cases twenty times, each execution on a fresh local
PostgreSQL 16 container and the repo's pinned native Restate server. The gate
identity selects the service names and locked port block. Cleanup removes the
container and stops Restate even when a case fails. It never uses a configured
production database or shared Restate server. `LASH_OPERATOR_RUNS=1` selects
one diagnostic execution; acceptance uses the default twenty.

The host submits through `LashSession::send`. The engine endpoint runs the
production session driver and process worker. The scripted model returns a
TypeScript cell that starts a child waiting for a signal, with the process
plugin's `lifetime::starter` policy. PostgreSQL must record `Until(Turn)`.
The second model call returns a cell that does nothing. A fixture response hook
raises typed `StoreCommitFailed` while it derives that call's response, once
the child is live: the completion is journaled and its derivation retries, where
a checkpoint hook's failure is the checkpoint's recorded outcome and fails the
root. Restate pauses after
the turn handler's existing eight-attempt bound, with 50 ms retry intervals;
the production recovery tick records the engine park.
Repair removes the fault. Redrive serves the recorded first cell and both
recorded model effects without buying the second completion again, then a
distinct third model call returns the answer. The harness never
constructs admitted roots or drives a host turn
inline. Attachment bytes use a private SQLite byte store; session, input,
root, process, scope and control-intent records all use PostgreSQL.

Each execution emits these case rows in `cases.jsonl`:

- `withdrawal`: send before registering the endpoint, withdraw by input ID,
  observe a terminal cancelled input with no admitted root and zero model calls.
- `running_cancel`: cancel by root while its child is live, observe one
  Cancelled terminal and one scope close, and await the child's cancellation.
- `parked_redrive`: observe no terminal or close while parked, repair and
  redrive by park ID, then require the original admission and exact journal
  command bytes, three distinct model calls,
  a real successful output and one child cancellation.
- `parked_cancel`: discard the first reply, recover the retained decision
  through `ParkedWork::intents`, then repeat the exact park address.
  Require typed `NotParked` refusals and unchanged intent and terminal receipts;
  the separately parked
  fork root and child must remain open.
- `parked_fork`: discard the first reply, recover its retained intent
  and distinct addressed successor. Observe that successor parked with its
  own live child. Only the original scope closes.

- `lost_reply_repeat`: report three stale repeats per parked verb and require
  unchanged receipts, two model calls per original root plus the successor's
  two distinct model calls, and no duplicate
  terminal write, scope close or child-cancel request.

Parked cancel and fork release the held root invocation. Restate can cascade
that kill to its child. Under [ADR 0110](../../docs/adr/0110-the-engine-owns-process-recovery.md),
recovery then ends the child `Abandoned` with `ResumeRefused { SubstrateLost }`.
The scenario accepts only that exact outcome or cooperative `Cancelled`,
requires one `ParentEnded` cancel request naming the original turn scope,
and waits for both the turn and child scope plans to settle.

Park verbs compare the caller's park ID; a stale repeat is refused, rather
than accepted as a new operation. The retained intent is the recovery receipt.

The runbook proves the lost operator reply alternative of the recovery
condition. Worker-kill and full cluster-restart campaigns remain separate
recipes. Existing root-control laws remain unchanged. The contracts are
ADRs 0039, 0101, 0104, 0105 and 0108.

Artifacts are under `target/session-operator/$KILN_GATE_ID/campaign.*/run-<n>/`, or
`LASH_OPERATOR_ARTIFACT_ROOT`. Each directory holds `services.json` with source
SHA, versions and local addresses; `cases.jsonl`; `verdict.json` with executed
counts; before/after redrive journals; the worker trace and log; the PostgreSQL
row census and service logs. The root also retains the source patch and records
its digest and the executable digest beside the source SHA. The root
`summary.json` totals executions. The evidence checker rejects a missing or
duplicated case, a zero-case run, or incorrect counted evidence.

This is a correctness recipe. A consolidated quiet-host rerun belongs to
FIG-4172; it introduces no latency or load budget.
