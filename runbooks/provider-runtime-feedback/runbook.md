# Provider runtime feedback at conversation position

Read [../RULES.md](../RULES.md) first.

**Classification:** judged browser scenario, economy tier. Run both `lashlang`
and `typescript` in fresh Workbench data directories. The subject is the Host
Application's rendered retry outcome and captured outgoing request, not exact
model prose. Use `deepseek/deepseek-v4-flash` through the Workbench's OpenRouter
route and record the served model from evidence.

**Contract:** [ADR 0084](../../docs/adr/0084-runtime-feedback-position.md).
Initial instructions stay unchanged while output-limit feedback follows the
partial answer that caused it. The tag is fallback and has no native authority.

## Deterministic companion

Source the fork's `env.sh`, then run:

```sh
cargo nextest run --workspace --locked -E 'test(runtime_feedback_)'
```

Expect nonzero passed counts covering Responses, Codex, Chat, both Anthropic
modes, Gemini, Code Assist, real RLM output-limit retries, checkpoint directives,
and Standard turn limits. The Anthropic witnesses must include native/fallback
wire equality after a partial answer and a request containing both a legal and
an illegal native slot. This companion uses scripted providers in tests only;
it does not substitute for the judged browser row.

## Phase 0: isolated Workbench

1. Set fresh `AGENT_WORKBENCH_RUN_DIR`, `AGENT_WORKBENCH_DATA_DIR`, and
   `AGENT_WORKBENCH_TRACE` paths, a free port, `LASH_RUNBOOK_DIALECT`, and
   `OPENROUTER_MODEL=deepseek/deepseek-v4-flash`, and
   `AGENT_WORKBENCH_OUTPUT_TOKEN_CAP=256`. Start with
   `just agent-workbench <port>`; poll `/healthz` and the rendered compose form.
   Expect the judged host and the selected dialect. Save `00-ready.png` and
   `/api/state` as `00-state.json`.
2. Workbench enables `TraceLevel::Extended` at bootstrap, including serialized
   provider-request bodies in its trace. Inspect a first short request and save the complete
   `provider_request` event's `body_json` as `01-initial-request.json`. Require
   `body_json_omitted_reason` to be absent. Expect an initial
   instruction slot and a conversation list that contains no duplicate of it.
   If that body is unavailable, abort as a capture/configuration gap; a
   `llm_call_started` logical-request trace alone cannot prove the wire shape.

## Phase 1: cause and observe an output-limit retry

1. Confirm `max_tokens` (or the selected Chat dialect token-cap field) is 256
   in the captured body. Submit an outcome request that requires more output than the cap, with a
   unique marker such as `feedback-position-2505-<run nonce>`. Do not request
   filesystem, shell, process, or other host-affecting tools.
2. Poll the trace for a provider response with output-limit termination followed
   by the runtime's retry request. Expect the original user marker, assistant
   partial, and runtime retry instruction in that order. Save
   `02-truncated-response.json` and `03-retry-request.json`. If the model finishes
   within the cap, the fault was not exercised: record that and use a fresh row
   with a smaller cap; never count a normal response as a retry witness.
3. Compare the initial-instruction slot byte for byte across the two requests.
   Expect equality and no retry text there. Expect the retry after the partial
   and before any later user content. On this Chat route its role is the host's
   instruction role. Other provider forms are proved by the deterministic
   companion; do not label this one live route as seven live provider runs.
4. Allow a short completion. Poll the UI and `/api/state` for settlement, scroll
   to the latest reply, and save `04-settled.png`, `04-state.json`, and the matching
   turn/trace identities. Expect one submitted user turn, the same terminal
   outcome in UI and API, and provider-attempt evidence explaining the retry.

## Scorecard and teardown

| Gate | Expected | Observed / artifact | Pass |
| --- | --- | --- | --- |
| Host and dialect | Fresh judged Workbench, served model and dialect evidenced | | |
| Fault exercised | Output-limit response and a later retry request | | |
| Instructions | Byte-identical initial slot; retry absent there | | |
| Position | Partial precedes retry; later user content follows it | | |
| Product agreement | Rendered outcome, API state, and trace identities agree | | |
| Ownership | Only this row's Workbench and containers stopped | | |

Use `just agent-workbench-down <port>` for this row. Never stop another host or
mutate its state. An unexecuted row remains unjudged; deterministic companion
passes alone do not fill the browser scorecard.
