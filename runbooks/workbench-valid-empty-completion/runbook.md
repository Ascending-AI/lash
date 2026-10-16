# E2E Scenario: Workbench Valid Empty Completion

> **Read [../RULES.md](../RULES.md) first.** Its browser, polling, screenshot,
> Abort/RCA, and teardown rules apply. This row uses a development-only,
> token-free Provider Wire Script and the normal chat composer.

**Purpose.** Prove that an OpenAI-compatible stream with explicit normal `stop`,
usage, and no assistant content completes a Standard turn successfully through
Workbench's ordinary ingress. The page, committed transcript and sealed provider
attempt must agree: one call, one completed attempt, terminal evidence retained,
and zero assistant bytes.

**Tier and fixture.** The matrix row is `standard`, tier `deterministic`, model
`scripted-provider`; execution costs $0. Boot with both
`AGENT_WORKBENCH_PROTOCOL=standard` and
`AGENT_WORKBENCH_DEV_PROVIDER_SCENARIO=valid-empty-completion`. The launcher
resolves the `provider-wire-fixtures` Buck2 feature target and builds with
`--config=judged`. The fixture
`crates/lash-provider-fixtures/provider-scripts/runtime/openai-compatible.chat-valid-empty-stop.json`
backs `OpenAiCompatibleProvider` with `ScriptedLlmHttpTransport`. It supplies
native finish reason `stop`, model `openai/gpt-5.4`, and 7 input / 0 output tokens.
The model name identifies fixture evidence, not a paid model request. An
OpenRouter request or absent development-provider warning invalidates the run.

**Runtime honesty.** The host composes `LashCore::standard_builder`; a composer
send uses `POST /api/turn`, `send()` and the durable engine. Judge the committed
`/api/state.transcript` and `/api/state.product_events.events`, plus the
`llm_call_completed` trace. The empty reply is retained as an `assistant_reply`
transcript row. Its rendered body must contain no prose; an agent label or
timestamp alone is consistent with an empty reply. Returning to idle without a
failure is the visible completion gate; a new success banner is not required.

The companion laws also cover post-tool completion without repeated tool
execution and checkpoint-delivered pending input.

## Phase 0 — Deterministic contract gate

From the repository root, source `./env.sh` and run each owning target once:

```sh
kiln test --test_output=all \
  //crates/lash-provider-openai:lash-provider-openai__unit_test \
  --test_arg=tests::empty_completion_tests::valid_empty_terminal_completions_succeed_across_chat_and_responses \
  --test_arg=tests::empty_completion_tests::eof_tolerance_does_not_turn_empty_unterminated_streams_into_success

kiln test --test_output=all \
  //crates/lash-protocol-standard:protocol_scenarios__test \
  --test_arg=standard_protocol_scenario_empty_model_response_finishes_after_checkpoint \
  --test_arg=post_tool_empty_model_response_finishes_without_repeating_the_tool \
  --test_arg=empty_model_response_checkpoint_delivers_pending_input_before_completion
```

Require **five executed tests and five passes**: two on the provider target and
three on the Standard target, as printed in Kiln's receipt lines. The provider
law covers buffered and streamed Chat and Responses. Its negative companion
proves EOF tolerance cannot make an unterminated empty stream succeed.

## Phase 1 — Boot the Standard fixture host

Choose an unused `<port>` and fresh `<data-dir>`, `<run-dir>` and `<artifact-dir>`.
From the fork, run:

```sh
. ./env.sh
AGENT_WORKBENCH_PROTOCOL=standard \
AGENT_WORKBENCH_DEV_PROVIDER_SCENARIO=valid-empty-completion \
AGENT_WORKBENCH_DATA_DIR=<data-dir> \
AGENT_WORKBENCH_RUN_DIR=<run-dir> \
AGENT_WORKBENCH_TRACE=<artifact-dir>/trace.jsonl \
AGENT_WORKBENCH_OPEN=0 \
bash scripts/agent-workbench-dev.sh up --port <port>
```

Poll `GET /healthz` for 200. Require the host log in `<run-dir>` to contain
`development provider scenario enabled: valid-empty-completion`; require the
`agent_workbench.startup` trace to name that scenario and SQLite. Open the normal
page at `http://127.0.0.1:<port>/`. Require a ready composer and an empty
`/api/state.transcript`. Save `01-ready-state.json` and `01-valid-empty-ready.png`.

**Fail if:** startup asks for credentials, the warning is absent, the host log
contains `panicked at`, or the browser cannot reach the normal chat page.

## Phase 2 — Send and judge one composer turn

Select **provider default** for reasoning (the fixture route cannot encode the
host's default `high` effort). Start capture of the live `/api/events` stream
before sending; save it as `02-events.ndjson`. Type **Run valid empty completion.**
into the composer and send it once. Capture
its `POST /api/turn` response as `02-turn-accepted.json`: require `accepted=true`,
`queued=false`, and a `turn_id`. Require one `done` product event for that turn on the captured live stream and
poll `/api/state` until `active_turns` is empty. The event's omitted `outcome`
means `completed`; an explicit `failed` outcome fails the run. Settled `done`
events retire from snapshots, so do not require one in a later `/api/state` read.

Require the rendered user prompt once, the composer ready to send again, no
assistant body text and no failure/retry row. An empty reply may retain its
agent label and timestamp. Save `02-state.json` and `02-valid-empty-finished.png`.

For the accepted turn require one committed user row and one `assistant_reply`
row, with `content.text == ""` and matching `provenance.turn_id`. Require one
`model_call_recorded` event whose `event_id` names that turn. Save its `record`
as `02-model-call.json` and check:

| Evidence | Required value |
|---|---|
| Assistant bytes | `0` |
| Model calls / attempts | `1 / 1` |
| Attempt outcome | `completed` |
| Protocol position | `terminal_observed` |
| Provider finish reason | `evidence.provider_finish_reason == "stop"` |
| Served fixture model | `evidence.served_model == "openai/gpt-5.4"` |
| Usage | `usage.input_tokens == 7`, `usage.output_tokens == 0` |
| Failure / retry | no attempt `error` or `retry_decision` |

Require exactly one `llm_call_completed` trace for the turn, with empty response
text, terminal reason `stop`, matching attempt evidence and 7/0 usage. Require its
`turn_completed` outcome to be `status=completed`, `done_reason=assistant_message`.
No `llm_call_failed`, second call, code block, or tool execution may occur.
These are the Standard completion and charge gates; no special probe response
or nested core is involved.

Reload the page. Require the user prompt once, the same committed empty reply
row and model-call record in `/api/state`, idle controls, and no visible error or
assistant prose. Save `03-reloaded-state.json` and `03-valid-empty-reloaded.png`.

**Judgment — FAIL if:** the UI and snapshot disagree, empty content becomes a
failure, terminal/usage evidence is missing, or the turn makes more than one
model call or attempt.

## Phase 3 — Teardown and score

Run `bash scripts/agent-workbench-dev.sh down --port <port>` with the same
`AGENT_WORKBENCH_DATA_DIR` and `AGENT_WORKBENCH_RUN_DIR`. Confirm the port is closed.

| Item | Objective gate | Verdict | Evidence |
|---|---|---|---|
| Contract coverage | five focused tests execute and pass | | Kiln receipts |
| Dev-only fixture | exact warning and scenario, SQLite, no provider network request | | host log, startup trace, ready screenshot |
| Normal completion | composer returns to idle without error; zero assistant bytes | | finished screenshot, accepted turn and state JSON |
| Transcript | one user row and one empty reply for the accepted turn, retained on reload | | state JSON, reloaded screenshot |
| Provider evidence | completed terminal-observed attempt, native `stop`, 7/0 usage | | model-call JSON, `llm_call_completed` trace |
| Charge safety | one model call and one attempt | | state and trace |
| Teardown | port closed | | teardown log |

Record the matrix label `standard`, deterministic tier, cost $0 and artifact
paths with the filled scorecard. Did ordinary Standard ingress accept explicit
normal terminal syntax independently of content while retaining the evidence
that proves the empty completion was valid?
