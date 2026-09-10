# E2E Scenario: Workbench Valid Empty Completion

> **Read [../RULES.md](../RULES.md) first.** Its browser, polling, screenshot,
> Abort/RCA, and teardown rules apply. This runbook uses a development-only,
> token-free Provider Wire Script.

**Purpose.** Prove that a syntactically valid OpenAI-compatible stream containing an
explicit normal `stop`, usage, and no assistant content completes one Standard turn
successfully. The rendered page must agree with the runtime's sealed provider-attempt
record: one call, one exchange, terminal evidence retained, and zero assistant bytes.

**No real tokens.** Set `AGENT_WORKBENCH_DEV_PROVIDER_SCENARIO` to exactly
`valid-empty-completion`. The launcher then enables the opt-in
`provider-wire-fixtures` feature. The checked-in
`openai-compatible.chat-valid-empty-stop` Provider Wire Script is the only transport;
an OpenRouter request or a missing development-provider startup warning invalidates the
run.

**Fixture honesty.** The probe creates a real `LashCore::standard_builder` session and
runs `session.turn().run()` through `OpenAiCompatibleProvider` backed by
`ScriptedLlmHttpTransport`. Its JSON result is derived from `TurnOutput`, the sealed
`LlmCallRecord`, and the transport exchange ledger. It does not construct a successful
outcome or terminal evidence in the handler.

This browser row covers the initial empty completion. The focused Standard scenario tests
separately cover post-tool completion, no repeated tool execution, and checkpoint-delivered
pending input.

## Phase 0 — Deterministic contract gate

From the repository root, source `./env.sh` and run:

```sh
cargo nextest run --workspace --all-targets --locked \
  -E 'test(valid_empty_terminal_completions_succeed_across_chat_and_responses) | test(eof_tolerance_does_not_turn_empty_unterminated_streams_into_success) | test(standard_protocol_scenario_empty_model_response_finishes_after_checkpoint) | test(post_tool_empty_model_response_finishes_without_repeating_the_tool) | test(empty_model_response_checkpoint_delivers_pending_input_before_completion)'
```

Require five executed tests and five passes. The adapter suite covers buffered and streamed
Chat and Responses paths inside one test, and its negative companion proves EOF tolerance
does not turn an empty unterminated stream into success.

## Phase 1 — Boot the dev-only Workbench surface

Choose an unused `<port>` and fresh `<data-dir>` and run:

```sh
AGENT_WORKBENCH_DEV_PROVIDER_SCENARIO=valid-empty-completion \
AGENT_WORKBENCH_DATA_DIR=<data-dir> \
AGENT_WORKBENCH_OPEN=0 \
just agent-workbench <port>
```

Poll `GET /healthz` for 200. Require the startup log to contain
`development provider scenario enabled: valid-empty-completion`. Open
`http://127.0.0.1:<port>/dev/valid-empty-completion`, require the heading **Valid empty
completion**, the deterministic-provider label, and initial status **Ready**. Capture
`01-valid-empty-ready.png`.

**Fail if:** startup asks for an API key, the dev-provider warning is absent, the page is
404, or the host log contains `panicked at`.

## Phase 2 — Run and judge the Standard turn

Start browser response capture for the page's same-path POST, click **Run valid empty
completion**, and poll the rendered status until it equals
`valid empty completion finished successfully`. Save the POST response as
`02-valid-empty-report.json` and capture `02-valid-empty-finished.png` with the evidence
grid visible.

Require the rendered page and captured JSON to agree on every field:

| Evidence | Required value |
|---|---|
| Protocol | `standard` |
| Assistant bytes | `0` |
| LLM calls | `1` |
| Provider exchanges | `1` |
| Attempt | `Completed` |
| Protocol position | `TerminalObserved` |
| Finish reason | `stop` |
| Usage | `7 input / 0 output` |

Require no rendered failure, retry, provider-error stop, assistant prose, or second
exchange. In the host log require no `panicked at` and no outbound provider URL.

**Judgment — FAIL if:** the browser reports success while the POST response disagrees,
the response lacks provider terminal evidence or usage, an empty assistant is classified
as an error, or more than one model call/attempt/exchange occurs.

## Phase 3 — Teardown and score

Run `just agent-workbench-down <port>`. Confirm the Workbench port is closed and its
port-derived Restate container is gone.

| Item | Objective gate | Verdict | Evidence |
|---|---|---|---|
| Contract coverage | five focused tests execute and pass | | focused nextest log |
| Dev-only fixture | exact startup warning, no credentials or network provider | | Workbench log, `01-valid-empty-ready.png` |
| Normal completion | rendered successful Standard result with zero assistant bytes | | `02-valid-empty-finished.png` |
| Provider evidence | one completed terminal-observed attempt, native `stop`, 7/0 usage | | `02-valid-empty-report.json` |
| Charge safety | one LLM call and one wire exchange | | rendered grid and report JSON |
| Teardown | port and owned Restate container gone | | teardown log |

**Aggregate:** did the real Standard runtime accept explicit normal terminal syntax
independently of content while preserving the provider and usage evidence that proves why
the empty completion was valid?

---

_Stop triggers and the Abort/RCA + reporting protocol are in
[../RULES.md](../RULES.md)._
