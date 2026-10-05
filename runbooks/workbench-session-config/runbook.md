# Embedder runbook: Recorded Workbench session config

> Read [../RULES.md](../RULES.md) first. This deterministic operator rehearsal
> combines Workbench observations with named companion laws. It is inventoried
> under `deterministic_only`, emits no paid judged row, and makes no provider
> network call. Screenshots supplement the typed and provider-request gates.

Prove that model, reasoning and prompt-context commands change recorded config
at a run boundary. An earlier run retains its model and prompt; a stale write
publishes nothing; reopening and redriving use the record despite catalog
changes. A cell exceeding the recorded `max_tool_calls` receives a typed
program failure, and the same session remains usable.

The binding contracts are [ADR 0126](../../docs/adr/0126-session-config-changes-are-typed-owner-commands.md),
[ADR 0101](../../docs/adr/0101-one-session-ingress-carries-every-admitted-item.md),
[ADR 0109](../../docs/adr/0109-store-to-engine-delivery-is-an-outbox-of-obligations.md),
[ADR 0110](../../docs/adr/0110-the-engine-owns-process-recovery.md) and
[ADR 0111](../../docs/adr/0111-a-deployment-namespace-prefixes-every-restate-name.md).
Use `SetLlmProfile`, the current name of the model command. Every host example
below uses the `lash::` facade. Hosts submit through `send()` and observe the
engine's work; they never execute a run or read live config to reconstruct it.

## Working material and preflight

Use a fresh row directory with separate `data`, `run` and `evidence` directories,
a free port from the operator's allocation, and a stable private
`RESTATE_AUTHORITY_ID`. Retain these exports through every restart and teardown:

```sh
. ./env.sh
export AGENT_WORKBENCH_DATA_DIR=<row-dir>/data
export AGENT_WORKBENCH_RUN_DIR=<row-dir>/run
export RESTATE_AUTHORITY_ID=<row-authority>
export AGENT_WORKBENCH_OPEN=0
export AGENT_WORKBENCH_DEV_PROVIDER_SCENARIO=replay-route-change
export OPENROUTER_MODEL=dev/replay-route-a
export OPENROUTER_MODEL_VARIANT=high
export AGENT_WORKBENCH_CONTEXT_WINDOW_TOKENS=200000
kiln gate lash <fork> -- just agent-workbench <port>
```

Require `/healthz` to return 200, the startup warning to name
`replay-route-change`, and `/api/state?session_id=<S>` to identify the same
session as the page and `<data-dir>/session-id`. This public dev-provider
scenario serves executable TypeScript terminal cells without contacting a
model service. Record the Workbench PID and managed Restate container id.
Keep the namespace, authority, service addresses and data paths unchanged.

The Workbench creates roots from an explicit spec in
[`bootstrap.rs`](../../examples/agent-workbench/src/main_sections/bootstrap.rs):

```rust
let spec = lash::SessionSpec::new(
    lash::LlmProfileKey::new("dev/replay-route-a"),
    lash::TurnBudget::bounded(20),
    lash::MaxToolCalls::new(1024),
);
```

The workbench is a chat product. Deployment recovery lives in `lashctl`:
see [recovery commands](../../examples/agent-workbench/README.md#recovery-and-context-compaction).
Recorded settings and session commands are product APIs for embedders; they
have no workbench operator dialog or admin HTTP routes. Run config operations
below in the host integration that owns `core` and `session`, or use the named
facade companion laws for their typed evidence.

Read the catalog and its write revision with
`session.admin().config().commands().await?`. Read committed policy and plugin
values with `core.session(session_id).durable().await?.read().await?`.
For each transaction retain its `ConfigWrite` id, expected revision, typed
`ConfigTransaction` and receipt. Submit through `session.admin().config().submit(...)`.
`ConfigSettlement::Pending` is acceptance; pass the unchanged receipt to
`session.admin().config().settle(receipt).await?` to observe application.
Gate the typed outcome and its fields. A cancelled command or expired wait is
not an applied transaction. Equal-content retries reuse the same id; a new
edit uses a new id. The implementation is
[`config_transactions.rs`](../../crates/lash/src/admin/config_transactions.rs).

Retain `/api/state`, config reads, screenshot checkpoints, and the matching
`llm_call_started` and `turn_completed` trace records. Correlate requests by
session and turn id, and save a trace boundary before every send. Trace request
fields are `request.model`, `request.model_variant`, and
`request.messages[].blocks[]`. This is the runtime's pre-filter request,
not a serialized provider wire capture. The scripted provider's exact terminal
text is only a completion marker.

Run the named companions below once, one target per invocation, using their
full test paths. Save the output and the structured test report that Kiln
prints under a distinct evidence directory for each invocation. Require the
named cases and the stated executed count, not just a successful exit. These
companions use SQLite and the in-process Restate server double. No PostgreSQL,
live-Restate suite or paid provider leg is needed.

## Phase 1: A model and reasoning change is a recorded command

Send a baseline marker `FIG4690-before-<row-id>` through the composer. Wait
for its terminal and idle state. Save its request, requiring model
`dev/replay-route-a` and variant `high`, and its completed turn evidence.
Read config revision `R` after the baseline settles.

Type `dev/replay-route-b` into `#profileInput` and choose `low` in
`#variantSelect`. The selector is pending until send. Send
`FIG4690-after-<row-id>` through the composer and await its finished run.
[`start_user_turn`](../../examples/agent-workbench/src/restate/turn_follow.rs)
applies the selection transaction before `send()`. Retain that exact write's typed settlement through the host's
`session.admin().config()` API. Use the stable id
`model-selection:dev/replay-route-b:low:<R>` with the revision `R` read before
send and the same `SetLlmProfile` and `SetReasoning` commands. Equal-id,
equal-content submission reads the original command and publishes no second
change.
This is the typed operation `apply_llm_profile_selection_to_session` uses in
[`app_state.rs`](../../examples/agent-workbench/src/main_sections/app_state.rs):

```rust
let outcome = session.admin().config().apply(
    lash::config::ConfigWrite::new(id, revision),
    lash::config::ConfigTransaction::of(lash::config::SetLlmProfile {
        model: lash::LlmProfileKey::new("dev/replay-route-b"),
    }).then(lash::config::SetReasoning {
        reasoning: lash::provider::ReasoningSelection::Effort("low".into()),
    }),
).await?;
```

Require `ConfigTransactionOutcome::Applied`, with `base_revision == R` and
`revision == R + 1`. The committed read must carry B and `low`, still at
`R + 1` after reattachment. Require that run's first request to name B and
`low`. Re-read the baseline's
saved request and durable model-call evidence: they must still name A and
`high`. Do not relabel history from `/api/state.settings.model`.

Companion, **two executed laws**:

```sh
kiln test --test_output=all --no-test-cache //crates/lash-restate:lash-restate__unit_test \
  --test_arg=--exact \
  --test_arg=tests::shift_laws_on_the_double::an_input_sent_after_a_config_command_runs_on_the_new_profile \
  --test_arg=tests::shift_laws_on_the_double::a_reasoning_change_is_judged_against_the_final_recorded_llm_profile
```

The first law captures the ordered A/B requests. The second requires a typed
`CoreConfigRefusal::ReasoningRefused` for an unsupported effort, no publication,
then `Applied` when the final model supports that effort. Neither a string
error nor the mere existence of a model selector satisfies this phase.
Save `01-request-before.json`, `01-config-write.json`, `01-settlement.json`,
`01-config-after.json`, `01-request-after.json`, and `01-profile.png`.

## Phase 2: A stale revision is refused, typed

Submit a fresh id `FIG4690-stale-<row-id>` with the same old revision `R` and
commands attempting A and `high`. Do not reuse the Phase 1 id with changed
content, which tests `ConfigSubmitError::ChangedContent` instead of staleness.

Require `ConfigTransactionOutcome::Stale { expected: R, actual: R + 1 }` in
the settlement. The config read must equal `01-config-after.json`, including
its revision and both namespaces. Submit a new marker with B and `low` still
selected. Require its finished run's first provider request to name B and
`low`; the refused command must have generated no model request of its own.
This composer send records its own fresh selection command and advances the
revision once. Save that new revision for Phase 3; do not attribute its write
to the stale transaction.

Companion, **one executed test**, through the typed embedder config API:

```sh
kiln test --test_output=all --no-test-cache //crates/lash:lash__unit_test \
  --test_arg=--exact \
  --test_arg=tests::core_session_builder::recorded_plugin_config::a_stale_transaction_publishes_nothing
```

Save `02-stale-write.json`, `02-stale-settlement.json`, `02-config.json`,
`02-provider-request.json`, and a chat screenshot as `02-stale.png`; retain the typed settlement separately.

## Phase 3: Restart and redrive read the record despite catalog drift

Save the committed config and both earlier roots' model-call evidence. Change
only `AGENT_WORKBENCH_CONTEXT_WINDOW_TOKENS` to `240000`, which changes the
metadata the Workbench catalog mints for a new model binding. Then run:

```sh
kiln gate lash <fork> -- just agent-workbench-restart <port>
```

Require a changed web PID, the same Restate container, authority and session,
and unchanged recorded model metadata, reasoning and revision. Do not reset
the data or replace Restate. Earlier A evidence remains A.

Resubmit a new `SetLlmProfile(B).then(SetReasoning(low))` transaction at the
current revision. Even the same key is a new binding resolution. Require
`Applied` and a recorded context window of `240000`. Only a run admitted
after this command may use that replacement binding; the old record must
still contain `200000`. Send a marker with B/low selected. Its composer
selection command is also a new recorded resolution, not a live override.
Require a finished run's provider request naming B/low and carrying the
earlier user markers. Save the config read immediately before that send and
after its own command, keeping the earlier snapshots unchanged.
The compact Workbench request trace omits model metadata, so the exact
metadata-and-redrive provider witness is the companion below, not an inferred
field in `trace.jsonl`.

Run the Phase 3 law in the shared facade invocation below. It changes the
registered metadata for the same key from a default cap of 3333 to 7777 and
changes every request-default field. It replaces the deployment, reopens,
crashes under a model call and lets the engine redrive. Require finished
outcomes and captured requests with the original complete metadata for each
old session on reopen and redrive; a later-created session carries the new
metadata. A cold open or another ordinary send alone does not prove redrive.

Also run the committed-run replay companion, **one executed law**:

```sh
kiln test --test_output=all --no-test-cache //crates/lash-restate:lash-restate__unit_test \
  --test_arg=--exact \
  --test_arg=tests::shift_laws_on_the_double::a_committed_run_redriven_after_a_profile_change_answers_from_its_receipt
```

It kills the execution after root A commits, applies a profile change at the
next boundary, then redrives A through its recorded journal. Require the
finished original answer under A, no new provider call or head write during
that redrive, and the following root's captured B request. This supplies the
pre-change root witness without manufacturing a host-driven replay.

Save `03-before.json`, `03-restarted.json`, `03-provider-request.json`,
`03-rebind-settlement.json`, `03-rebound.json`, `03-redrive-law.log`, and
`03-restarted.png`. Keep the pre-command and post-command run identities
distinct from the companion's later-created session identity.

## Phase 4: Host context changes through the RLM owner's command

Use the Accounts tab to add a uniquely named account. Retain its returned
authority and the `api.accounts.add` record. The host calls
`record_accounts_context_for_session` in
[`plugins.rs`](../../examples/agent-workbench/src/main_sections/plugins.rs),
which applies `ConfigTransaction::of(lash::rlm::SetRlmPromptContext { context })`.
Read the newly recorded context array and config revision. Re-submit that
exact array under a fresh id and the current revision using the typed
`lash::rlm::SetRlmPromptContext` command through `session.admin().config()`, so this phase retains an explicit typed
`Applied` outcome as well as the host-data change.

Send `FIG4690-context-<row-id>` and require a finished run. In its first
provider request, every whole recorded context string must occur exactly
once. The string beginning `Connected inbox authorities right now:` must
contain the returned authority and replace the earlier no-account context.
Count the complete context string, not an authority name also present in
tool schemas or worked examples. Compare against `recorded.plugins`, never
against a live callback's latest value.

The two Phase 4 laws in the shared invocation require the RLM command to
settle `Applied`, increment revision once and survive cold reopen, and a raw
run-options payload carrying `prompt` to fail before any provider request:
`EmbedError::Runtime` with code `RunShapeRefused`,
`RunShapeRefusal::Owner`, the RLM owner id and
`ConfigRefusalReason::Unreadable { role: ConfigValueRole::RunOptions, .. }`.
An HTTP JSON decode error or prose saying "prompt refused" is insufficient.
The negative law captures zero requests. Do not invent a `prompt` field on
the Workbench's turn endpoint to substitute for this typed facade witness.

Save `04-account.json`, `04-recorded-context.json`, `04-settlement.json`,
`04-provider-request.json`, `04-prompt-refusal-law.log`, and `04-context.png`.

## Phase 5: A recorded tool-call limit produces a program failure

Create a dedicated session through the host's explicit `SessionSpec`, with
`MaxToolCalls::new(4)` as its third argument. The RLM companion below creates
its fixture session under that recorded limit. A facade host states it as:

```rust
core.session(limit_session_id).create(lash::SessionCreation::root(
    lash::SessionSpec::new(
        profile_key,
        lash::TurnBudget::bounded(20),
        lash::MaxToolCalls::new(4),
    ),
)).await?;
```

The Workbench session-create HTTP
form accepts only `name`; do not send it a fictitious limit field. Its own
root spec explicitly records 1024, so lower the Workbench session's bound
using `lash::config::SetMaxToolCalls` and require typed `Applied` plus a
committed read of 4 before testing its readback.

The dev `replay-route-change` cells only finish and cannot exceed the bound.
Run the actual RLM overflow fixture, **three executed laws**:

```sh
kiln test --test_output=all --no-test-cache //crates/lash-protocol-rlm:tool_batch_parallelism__test \
  --test_arg=--exact \
  --test_arg=restate_double::tool_call_limit_admits_the_limit_and_refuses_the_group_past_it \
  --test_arg=restate_double::tool_call_limit_staged_calls \
  --test_arg=restate_double::tool_call_limit_refuses_the_same_call_across_a_crash
```

Require the staged cell's first four calls to execute and its fifth call to
be refused without starting. The failure is `lash::ToolCallLimitExceeded`
with `ToolCallLimitScope::Cell`, limit 4, counted 4 and requested 1. Its
tool failure carries class `ResourceLimit`, code `max_tool_calls_exceeded`
and the structured `raw.tool_call_limit` cause; retain these typed fields
when inspecting the failure, not just its rendered sentence. Its
sentence naming `max_tool_calls = 4` must reach the next captured provider
request as program feedback. The crash law must reproduce the same refusal
after engine redrive. A provider 429, infrastructure retry or permanent
session failure does not satisfy this gate. A group of five is refused
whole; that case alone does not prove the sequential fifth-call contract.

The shared facade invocation also proves that a commanded limit is `Applied`,
reaches the next run and survives the engine's reopen on the same session.
Its three answering requests carry refusal counts `[0, 2, 4]`: the earlier
1024-limit run has none; both later 1-limit runs expose their refused group
of two in feedback and finish. This is the same-session continuation witness.
The other limit laws require applied/stale revision behavior and typed
`EmbedError::MissingMaxToolCalls` with no created session for an unstated
root limit. Do not confuse a successful root with absence of cell failure.

Restart the Workbench with the same paths and authority, and require its
recorded limit still equals 4. Send another terminal fixture marker on the
same session and require a finished run's request with B/low and the complete
recorded Phase 4 context once. Save `05-limit-settlement.json`,
`05-before-restart.json`, `05-after-restart.json`, `05-provider-request.json`,
the RLM and facade reports, and `05-same-session.png`.

## Shared facade companions and executed counts

Run this invocation once for Phases 3, 4 and 5, **six executed tests**:

```sh
kiln test --test_output=all --no-test-cache //crates/lash:lash__unit_test \
  --test_arg=--exact \
  --test_arg=tests::recorded_protocol_prompt::a_session_created_under_defaults_a_reopens_and_redrives_under_a_on_sqlite_memory \
  --test_arg=tests::core_session_builder::rlm_session_facts::the_prompt_is_recorded_config_its_owners_commands_change \
  --test_arg=tests::recorded_protocol_prompt::an_rlm_run_options_prompt_is_refused \
  --test_arg=tests::recorded_execution_controls::a_commanded_max_tool_calls_binds_the_next_run \
  --test_arg=tests::recorded_execution_controls::set_max_tool_calls_is_applied_at_the_next_revision_and_a_stale_one_publishes_nothing \
  --test_arg=tests::recorded_execution_controls::a_creation_without_max_tool_calls_is_refused
```

The five companion invocations execute **thirteen tests** total, with no ignored
or unmatched case credited. Inspect the assertion bodies as well as the
reports before scoring: a passing count alone cannot prove a request fact.
Sources are [the facade prompt laws](../../crates/lash/src/tests/recorded_protocol_prompt.rs),
[the RLM prompt command law](../../crates/lash/src/tests/core_session_builder/rlm_session_facts.rs),
[the recorded control laws](../../crates/lash/src/tests/recorded_execution_controls.rs),
[the model laws](../../crates/lash-conformance/src/conformance/turn_config.rs),
[the RLM double registrations](../../crates/lash-protocol-rlm/tests/tool_batch_parallelism.rs)
and [the limit request assertions](../../crates/lash-conformance/src/conformance/tool_batch_parallelism/limit.rs).
These symbols were checked on `origin/main` at `c748204b43`; refresh them
against the execution revision before running. The FIG-4636 smoke report at
`/workspace/notes/lash/tasks/lanes/fig-4636.report.md` covers separate end-phase
checks and is not evidence that this rehearsal executed.

## Teardown and score

Run `kiln gate lash <fork> -- just agent-workbench-down <port>` with the same
run/data paths and authority. Confirm that only this rehearsal's Workbench
process and managed Restate container are gone. Preserve all evidence.

| Phase | Typed outcome | Provider-request fact | Verdict and evidence |
| --- | --- | --- | --- |
| 1 | `Applied`, one revision step; unsupported effort is owner-refused | A/high before, B/low after; earlier evidence stays A | `01-*`, two model laws |
| 2 | `Stale`, exact expected/actual; config unchanged | no command call; following request remains B/low | `02-*`, embedder config law |
| 3 | `Applied` rebind; finished reopen and engine redrive | old metadata on old runs; new binding only after resolution | `03-*`, restart/redrive law |
| 4 | `Applied`; forbidden run options yield typed `RunShapeRefused` | new complete context once; refused run makes zero requests | `04-*`, two RLM prompt laws |
| 5 | typed cell-limit failure; `Applied`/`Stale`; missing limit refused | fifth call's refusal in feedback; same session answers after reopen | `05-*`, three RLM and three control laws |

Abort/RCA on any missing typed cause, changed old-run evidence, request-count
mismatch or config readback mismatch. Report a missing artifact as unproven;
never infer a pass from assistant prose or from FIG-4636's unrelated smoke run.
