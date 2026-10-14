# lash

A durable, performant and customizable agent harness in Rust, built for scalable backend deployments rather than single-user coding agents.

Key features:

1. **Durable execution** ([crash contract](#what-survives-a-crash))
    1. A turn's terminal commits atomically; recovery on a compatible node loads its last committed checkpoint, and uncommitted work may repeat
    2. Many nodes share one PostgreSQL store, and survive being killed, partitioned and restarted
    3. Durable processes, waits and cancellation, for approvals, callbacks and triggers
    4. Sessions fork, park and resume
    5. Clients follow a turn or process with durable cursors, so a late follower catches up
2. **Performance**
    1. About 10 ms of engine overhead per tool round on PostgreSQL
3. **Deep customization** through the plugin system
4. **Swappable interfaces**
    1. Store: SQLite and PostgreSQL provided
    2. Attachments: S3-compatible storage provided
    3. Tracing: a `TraceSink` seam for host sinks, and an OpenTelemetry adapter provided
    4. Models: Anthropic, OpenAI, Gemini and any OpenAI-compatible endpoint
    5. Tools: your own tool providers, plus MCP servers
    6. Deferred tools: a host resolver supplies tools on demand, so a large catalog need not be loaded up front
5. **Two main execution modes, as plugins**
    1. Standard: native provider tool calling
    2. RLM: the model works in a persistent REPL
6. **RLM, a persistent REPL over code mode**
    1. The model writes code cells in a persistent REPL: state carries from cell to cell, and printed values come back as observations
    2. Tools are typed functions the code calls, so one cell can loop over and combine many calls, and run them concurrently as tasks
    3. Runs on the Lash VM, a small kernel language built for it; a cell saves its state whenever it waits, and a crashed cell resumes from there without re-executing committed effect outcomes; unfinished calls follow their execution policy
    4. Dialects lower to the kernel and none is built into it; TypeScript provided
    5. Model code runs in resettable worker processes, and every effect crosses the host
    6. Deferred tools resolve mid-session, and a granted tool stays callable after a restart
7. **Workflows**
    1. Durable workflows built on the same kernel
    2. The agent writes them in TypeScript, or in any dialect that lowers to the kernel
    3. Every dialect lowers to one representation, the kernel document
    4. A host reads and edits that document through typed transactions, with no dialect code, which is what a workflow-editing UI needs


> **Alpha:** works today, API still moving fast — pin to an exact `=0.1.0-alpha.N` version when you embed.

## Embedding

The SDK package is named `lash-runtime`; alias it as `lash` in your dependencies
and enable the `mcp` feature for MCP support.

Lash owns its MCP SDK as the publishable `lash-rmcp` dependency, based on
upstream rmcp 3.5.1. Every embedder gets the nested `x-mcp-header` fix through
that dependency. See the [ownership and upstream merge note](third-party/vendored/lash-rmcp/OWNERSHIP.md)
for the maintained diff and release procedure.

## What survives a crash

Lash recovers from committed state. These guarantees require acknowledged
commits to survive the failure: SQLite file stores need `SqliteSynchronous::Full`
on storage that honors sync requests; PostgreSQL needs durable WAL flushes and
a failover policy that preserves acknowledged commits. An in-memory SQLite store
does not survive loss of its process. See [the storage assumption](docs/adr/0132-durability-is-state-first-over-the-lash-store.md#13-the-storage-durability-assumption)
and [SQLite's configuration](crates/lash-sqlite-store/src/conn.rs).

- **Committed effect outcomes are never re-executed.** Recovery loads the
  outcome instead of starting its body again. This says nothing about how many
  times an external action ran *before* its outcome committed. Store-local
  effects committed in the same transaction as their outcome are atomic with it.
  Enforcement: [round recovery](crates/lash-core-execution/src/runtime/actor/round/fold.rs)
  (`RoundMember::recovery`) and [store-local settlement](crates/lash-core-execution/src/runtime/actor/round/store_local.rs).
  Law: [the mixed-round crash matrix](crates/lash-durable-test/tests/round_crash_matrix.rs)
  (`a_mixed_round_holds_once_and_repeatable_rules_across_every_cut`).
- **Computation since the last committed checkpoint may run again.** This
  includes clock, random, projection reads, declared `Read` tools, printing and
  logging; read answers may differ, and diagnostics may repeat or be lost.
  Planned cell reads have no individual outcome record: their results survive
  only in the next saved cell state. A `Read` routed through a recorded call
  can also run again when interrupted, but never retries a reported failure.
  Enforcement: [kernel evaluation](crates/lash-kernel-vm/src/machine/eval.rs),
  [printing](crates/lash-kernel-vm/src/machine/exec.rs),
  [read execution](crates/lash-kernel-vm/src/machine/actions.rs) (`perform_read`),
  and [read planning](crates/lash-protocol-rlm/src/executor/reads.rs).
  Laws: [cell reads](crates/lash-durable-test/tests/cell_read_laws.rs)
  (`a_read_lost_with_its_unsaved_stretch_is_made_again_on_sqlite_memory` and
  `a_saved_read_is_never_made_again_on_sqlite_memory`).
- **`ExecutionPolicy::Once` effects interrupted mid-body settle as
  `Interrupted` and are never re-run.** The started row commits before the
  body starts. Recovery cannot tell whether an external action happened before
  the crash, so `Interrupted` is uncertainty, not proof that nothing happened.
  `Once` also forbids retry after a reported failure; it does not promise
  successful completion. Enforcement: [policy](crates/lash-sansio/src/tool_contract.rs)
  (`ExecutionPolicy`) and [round recovery](crates/lash-core-execution/src/runtime/actor/round/fold.rs).
  Law: [the mixed-round crash matrix](crates/lash-durable-test/tests/round_crash_matrix.rs).
- **`Repeatable` effects may run again under their recorded retry policy and
  deadlines, subject to the current declaration's veto.** Recovery of a started
  attempt without an outcome re-enters the same ordinal; it does not consume a
  new retry ordinal. Thus `max_attempts` bounds admitted ordinals, not total
  body entries across repeated crashes. External systems must deduplicate on
  `call_id` to make these repeats safe. Enforcement: [policy](crates/lash-sansio/src/tool_contract.rs)
  (`permits_repeat`, `reruns_interrupted`), [round recovery](crates/lash-core-execution/src/runtime/actor/round/fold.rs)
  and [retry admission](crates/lash-core-execution/src/runtime/actor/round/rounds.rs).
  Laws: [the mixed-round crash matrix and attempt cap](crates/lash-durable-test/tests/round_crash_matrix.rs)
  (`the_attempt_cap_ends_a_repeatable_call`).
- **A durably recorded response of a durable turn-model call is never
  purchased again.** The turn commits the provider's raw response, with what
  its stream hooks saw, as the call's receipt (`model.answer`) before any
  stream-finished or response hook runs. A crash after the receipt commits
  derives the response again from the receipt on any node: hooks run again,
  the provider is not asked again. Recovery re-sends the pinned request only
  for a call with no receipt, while its recorded deadline allows; a crash
  after the provider answered but before the receipt commits can therefore
  still cost a second provider call, whose answer may differ
  ([ADR 0132 §4](docs/adr/0132-durability-is-state-first-over-the-lash-store.md#4-turns-restore-from-their-checkpoint),
  [ADR 0142](docs/adr/0142-a-cells-first-save-carries-its-turns-model-response.md)).
  Model calls a tool owns (direct calls) and the unowned client are outside
  this guarantee. Enforcement: [model send, receipt and restore](crates/lash-core/src/runtime/durable/model_call.rs)
  and [phases](crates/lash-core/src/runtime/durable/phases.rs).
  Laws: [model answer receipts](crates/lash-durable-test/tests/model_answer_receipt_laws.rs).
- **Exactly-once external behaviour is the host's job.** Lash supplies a stable
  `ToolCallId`, exposed as `AttemptContext::call_id()`, across crash recovery and
  reported-failure retries of the same logical tool call. The host and external
  system must use it as their deduplication key, with durable reconciliation for
  uncertain outcomes. A local ledger alone does not atomically commit a remote
  action. A fresh logical call has a fresh id; business deduplication across
  distinct calls needs a domain key. This tool id does not promise idempotent
  model billing. Enforcement: [identity derivation](crates/lash-sansio/src/tool_call_id.rs)
  and [attempt context](crates/lash-core-execution/src/tool_provider.rs).
  Laws: [tool call identity](crates/lash-durable-test/tests/tool_call_identity.rs).
  See [the host's external-effect contract](docs/operations/durable-hosting.md#external-effects)
  and [ADR 0117](docs/adr/0117-lash-names-every-tool-call.md).

Observation replay is delivery of retained events, not re-execution of their
producers. Live streams and logs have their own retention and delivery contracts;
see [observing turns](docs/observing-turns.md) and [observing processes](docs/observing-processes.md).

## Examples

Runnable apps under `examples/

The agent workbench is the product E2E host: it owns its UI and product state,
opens a Lash session per chat, and resumes browser observation with durable
cursors. It carries Standard and RLM turns and durable processes. Hosts own
events, routing and scheduling. The upgrade node and external consumer remain
structural harness adapters for upgrade and public API proofs.

```bash
OPENROUTER_API_KEY=sk-or-... just agent-workbench 3000
# then open http://127.0.0.1:3000
```


## License

MIT
