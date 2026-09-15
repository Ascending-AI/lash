# Provider runtime feedback at conversation position

Read [../RULES.md](../RULES.md) first.

**Classification:** judged browser scenario, economy tier. Run one `typescript`
row in a fresh Workbench data directory. The subject is the Host
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

Expect **45 executed tests** covering Responses, Codex, Chat, both Anthropic
modes, Gemini, Code Assist, real RLM output-limit retries, and checkpoint directives. Pin the
count, not "nonzero": a filter that has drifted to select two of the forty-five still reports
a nonzero pass and reads as green.
The Anthropic witnesses must include native/fallback
wire equality after a partial answer and a request containing both a legal and
an illegal native slot. This companion uses scripted providers in tests only;
it does not substitute for the judged browser row.

## Phase 0: isolated Workbench

1. Set fresh `AGENT_WORKBENCH_RUN_DIR`, `AGENT_WORKBENCH_DATA_DIR`, and
   `AGENT_WORKBENCH_TRACE` paths, a free port, a fresh `RESTATE_AUTHORITY_ID`, and
   `OPENROUTER_MODEL=deepseek/deepseek-v4-flash`, and
   `AGENT_WORKBENCH_OUTPUT_TOKEN_CAP=256`. Start with
   `bash scripts/agent-workbench-dev.sh up --port <port>` (the `just agent-workbench <port>`
   recipe is the same command but does not export `CARGO_TARGET_DIR`, so source the fork's
   `env.sh` first); poll `/healthz` and the rendered compose form.
   Expect the judged host, and confirm the served model from the host's own
   `agent_workbench.startup` record. That record carries `addr`, `data_dir`,
   `dev_provider_scenario`, `lashlang_execution_path`, `model`, `restate_endpoint_addr`,
   `restate_ingress_url`, `store_backend` and `trace_path` — **no dialect field**, so do not
   look for one there. Read the `typescript` dialect from
   `composition_changed.rendered_system_prompt`, which does carry it. Save `00-ready.png` and
   `/api/state` as `00-state.json`.
2. Workbench enables `TraceLevel::Extended` at bootstrap, so its trace carries
   the outgoing provider request. **Gate it on what that record can actually
   show you.** Exact request JSON is retained only up to
   `MAX_PROVIDER_REQUEST_BODY_JSON_BYTES` (2 KiB, a compile-time constant in
   `crates/lash-core/src/runtime/turn_driver/streaming.rs` with no capture
   selector), and a judged Workbench request carrying the host prompt and its
   tool contracts is an order of magnitude larger, so every real row records
   `body_json_omitted_reason: "size_limit"` with `body_len` and `body_sha256`
   instead of the text. Save the first `provider_request` event as
   `01-initial-request.json` and require those three fields; a present
   `body_json` is usable evidence only when `body_len` is genuinely under
   2 KiB. Do not raise the cap to make a gate fire: it is paid on every JSONL
   record and OpenTelemetry attribute by every user of tracing.
   Take the composition and conversation evidence from the records that do
   carry it. Save the paired `composition_changed` event as
   `01-composition.json` — its `rendered_system_prompt` is the initial
   instruction slot verbatim and its `fingerprint` hashes that slot together
   with the tool contracts. Note that `tool_schemas` on the same record is `[]`: the RLM route
   renders the tool contracts *into the prompt text* rather than into that field, so the
   contracts the fingerprint covers are inside `rendered_system_prompt` (tens of kilobytes of
   them), not in the obvious slot. Save the paired `llm_call_started` request as
   `01-logical-request.json`. Expect an initial instruction slot and a
   conversation list that contains no duplicate of it. Abort as a
   capture/configuration gap only if the `provider_request` accounting fields
   or those two records are missing.

## Phase 1: cause and observe an output-limit retry

1. Confirm the 256-token output cap reached the wire from the record that
   reports it, not from the omitted body: the `llm_call_completed` response and
   each of its attempts carry `generation_disposition`, whose
   `output_token_cap` must be `applied` (or `clamped_to_capacity`, where the
   model's capacity is the smaller number and the caller's bound still holds).
   `not_requested` means the row was misconfigured, not that the runtime
   failed. Where `body_json` is genuinely present under the 2 KiB cap,
   `max_tokens` (or the selected Chat dialect's token-cap field) must agree
   with it. Submit an outcome request that requires more output than the cap, with a
   unique marker such as `feedback-position-2505-<run nonce>`. Do not request
   filesystem, shell, process, or other host-affecting tools.
2. Poll the trace for a provider response with output-limit termination followed
   by the runtime's retry request. Expect the original user marker and the
   runtime retry instruction in that order. **The partial answer is not a wire
   message on this route.** The truncated reply is retained by the RLM protocol in its bound
   `history` variable, witnessed by the paired `protocol_step` record whose
   `RlmDiagnostic.decision` is `retry_output_limit_cell`. Require that record for the same
   turn rather than an assistant message carrying the partial answer.

   Do **not** read this as "an RLM conversation carries no assistant message at all". It
   carries none until the model emits a cell that actually executes; from that iteration
   onward the protocol writes the cell and its result into the conversation, and `assistant`
   blocks appear on the wire in quantity. That is correct behaviour on a correct run. The
   finding would be *the truncated partial answer* appearing as a wire message — scope the
   check to that, and do not report assistant blocks after a successful cell as a defect. Save
   `02-truncated-response.json` (the `llm_call_completed` record with its
   attempts) and `03-retry-request.json` (the retry's `llm_call_started`
   request together with the `provider_request` accounting fields for the same
   `llm_call_id`). If the model finishes
   within the cap, the fault was not exercised: record that and use a fresh row
   with a smaller cap; never count a normal response as a retry witness.
3. Compare the initial-instruction slot across the two requests through the
   composition record rather than the wire body. `composition_changed` is
   emitted only when the fingerprint of the instruction slot plus the tool
   contracts changes, so one such event spanning both LLM calls, with no second
   event between them, is the byte-equality witness; its
   `rendered_system_prompt` must carry no runtime retry text — static tool
   contracts may legitimately discuss output limits, so read the surrounding
   text before calling a substring a violation. Expect the retry instruction to
   follow the user content of the turn that produced the truncated reply and to
   precede any later user content, in the `llm_call_started` messages of
   `03-retry-request.json`: the observed shape is
   `[user(marker), instruction-role(feedback), user(next iteration frame)]`, and
   further retries accumulate their feedback in place, each still ahead of the
   next user frame. On this Chat route its role is the host's
   instruction role. Other provider forms are proved by the deterministic
   companion; do not label this one live route as seven live provider runs.
4. Settle the turn and record the terminal. Do **not** expect a short completion to arrive:
   with the `AGENT_WORKBENCH_OUTPUT_TOKEN_CAP=256` this row prescribes and an essay prompt,
   the model does not escape the cap — nearly every response terminates `output_limit` and
   the turn's expected terminal is a **budget exhaustion** (`failed` / `max_turns`), not a
   reply to scroll to. Poll the UI and `/api/state` for settlement and save `04-settled.png`,
   `04-state.json`, and the matching turn/trace identities. Expect one submitted user turn,
   the same terminal outcome in UI and API, and provider-attempt evidence explaining the
   retry. If the row instead wants a settled answer on screen, raise the cap **after** the
   retry evidence of step 2 is captured; a run that never reaches the cap has not exercised
   the fault at all.

## Scorecard and teardown

| Gate | Expected | Observed / artifact | Pass |
| --- | --- | --- | --- |
| Host and dialect | Fresh judged Workbench; served model from `agent_workbench.startup`, dialect from `composition_changed.rendered_system_prompt` | | |
| Fault exercised | Output-limit response and a later retry request | | |
| Cap on the wire | `generation_disposition.output_token_cap` is `applied` or `clamped_to_capacity` on the response and its attempts | | |
| Request accounted for | `body_len` + `body_sha256` present; `body_json_omitted_reason: "size_limit"` whenever `body_len` exceeds the 2 KiB cap | | |
| Instructions | One `composition_changed` fingerprint spans both calls; retry text absent from `rendered_system_prompt` | | |
| Position | Retry instruction follows its turn's user content and precedes the next user frame; `protocol_step` `retry_output_limit_cell` witnesses the partial; the partial answer itself never appears as a wire message | | |
| Product agreement | Rendered outcome, API state, and trace identities agree | | |
| Ownership | Only this row's Workbench and containers stopped | | |

Use `just agent-workbench-down <port>` for this row. Never stop another host or
mutate its state. An unexecuted row remains unjudged; deterministic companion
passes alone do not fill the browser scorecard.
