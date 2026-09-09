# Toolbench

Toolbench runs 16 deterministic weather, string extraction, KV, mail and
world-update tasks against real OpenRouter models. Every task owns an isolated
seeded world; the model is the source of variance. Set `OPENROUTER_API_KEY`
(or load the repository `.env`). Credentials are never printed.

## Cohorts and flags

- `--channel cell|native|standard` selects one channel (default: cell).
- `--paired` pairs cell and native in randomized order for each task/dialect.
- `--channel-set all` includes both paired RLM channels plus standard, even
  without `--paired`. Standard runs once per task, with `dialect: "none"`.
- `--dialect both|lashlang|typescript` selects RLM dialects; standard ignores it.
- Repeat `--model` to compare models in the same run. Default:
  `z-ai/glm-5.3-flash`. Models share the concurrency budget and their work is
  interleaved, so cohorts run concurrently.
- `--reasoning-effort none|low|medium|high` applies to every model/cohort.
  `none` preserves provider-default behavior (it does not disable reasoning).
  Other values use the facade's `ModelSpec` variant/capability and the
  OpenAI-compatible provider's OpenRouter reasoning-effort encoding.
- `--repetitions N` and `--runs N` are aliases in both paired and single modes.
- `--concurrency N` bounds active tasks and preflight probes (default: 1).
- Repeat `--task ID` to select tasks; duplicate selections run once and unknown
  IDs are errors. `--allow-partial` permits failed tasks or excluded cohorts.

All channels, one repetition, both dialects: 16 × 5 = 80 task rows per model.
The two-model comparison uses medium reasoning and 160 task rows:

```sh
timeout 1h target/debug/toolbench --model z-ai/glm-5.3-flash --model openai/gpt-5.6-sol \
  --paired --repetitions 1 --dialect both --channel-set all \
  --reasoning-effort medium --concurrency 8 --allow-partial \
  --results-file /tmp/toolbench.jsonl
```

For a small standard-only run:

```sh
target/debug/toolbench --channel standard --model openai/gpt-5.6-sol \
  --reasoning-effort medium --runs 1 --task weather-temperature \
  --task kv-read --task mail-count --results-file /tmp/standard.jsonl
```

## Standard-mode grading

Standard uses `LashCore::standard_builder` with no RLM plugin. The same host
names, schemas and handlers are exposed as ordinary provider-native tools.
Task sentences are shared across cohorts; one prompt builder appends the
channel's constraints and renders standard prompts with the catalog's
ordinary underscore-separated tool names.

Standard adds `submit`, requiring `{"value": <any JSON>}`. Its handler records
the value and returns `ToolControl::Finish`, so no later model output is needed
or graded. Submits are counted before argument/ID validation; their values are
recorded in `submit_values`. Malformed arguments are tracked internally and
never count as identical valid submissions. Identical
repeated values count as one submission for grading. Differing values or malformed
duplicates fail with `conflicting submits`; the first executed value is graded.

The common grader checks the finish matchers (including Numeric), exact world
equality, and task completion with a finish value.
Standard must submit at least once with a consistent value. Execution counts, failures, repeated errors,
host call counts and provider round-trips are metrics only.

`--max-task-cost-usd` defaults to **0.10**. The grader compares this ceiling
against the sum of OpenRouter `usage.cost` across the task's provider attempts.
If any attempt lacks cost, the cost rule is `n/a`, the task row records
`cost_unknown: true`, and the summary counts unknown rows. Costs are never
silently treated as zero. Correctness checks still apply to these rows.

`--turn-wall-limit-secs` defaults to **120**. This outer harness deadline
produces a failed row with reason `wall_limit`; elapsed wall time itself is
not an efficiency grade. Turn and no-progress iteration budgets are unbounded.
The runtime retains its per-cell instruction, memory and execution sandbox bounds.

Native and standard are preflighted separately per model. Native must call
`execute_code` once and finish with 1; standard must call `submit` with 1;
both require zero host calls. A failed probe retries once (two total tries).
Only the failing model/channel is excluded, leaving other cohorts runnable.

## Evidence and summaries

Attempt and task rows stream to stdout and `--results-file`. Task rows include
finish value, executions, failed executions, actual and expected host tool counts,
rounds, grade, wall time, aggregate usage, cost availability and configured limits.
RLM attempt rows include `code` (null for prose-only/request-finish attempts)
and `observation`. Standard attempt rows use `tool_calls: [{name, arguments}]`
with `code: null`. RLM rows have an empty `tool_calls` array. Observations are capped at 2,000 Unicode characters and
`observation_truncated` reports whether anything was cut. Transport retries
have no execution source or observation; failed/timed-out tasks retain partial
evidence. A provider call interrupted before its ledger is sealed gets an
`interrupted` attempt row with observed usage (or null if unavailable) and attempt ordinal. Summary rows have `kind: "summary"` for each model/cohort. The Markdown tables print to
stdout and are saved at `<results-file>.summary.md`.

Tables show pass/rows, rounds, total prompt tokens, cache reads and writes,
completion and reasoning tokens, provider cost, summed/median task wall time,
and per-task means. Every attempt, including retries and failures, contributes.
Missing metering makes the corresponding total `n/a`. Probe usage is separately
logged outside the task cohort totals. Deltas compare each RLM cohort against
standard using per-task means of the same quantity; zero or unknown baselines
produce `n/a`. Wall totals sum task durations, not concurrent elapsed time.
See the field mapping below for exact definitions.

Keep new tasks small: one to three host calls and at most three prompt sentences.
Validate every channel/dialect against a 30-second target before adding a task. Cohort runs should run concurrently
across models with an approximately one-hour external wall budget.

## Provider logs and retries

`--provider-retries N` defaults to 3: at most four transport attempts per
provider round, with Lash's `ProviderRetryPolicy`. Backoff starts at 1 second,
doubles to a 10-second cap, and adds up to 500 ms jitter. Retry-After is honored
up to 60 seconds; longer delays are refused. Throttle courtesy attempts are
disabled so N remains a strict retry bound. The 120-second outer task deadline
still applies across requests and backoffs; the adapter's default request timeout
is 300 seconds. There is no whole-task retry and no change to world grading.

Lash decides whether a failure is retryable and charge-safe. Transient transport
errors and throttles can retry; 400/422 request-shape errors never retry.
The adapter can refuse malformed/empty responses or other errors, and Lash can
refuse retries after a response/output was observed without an idempotency
or resume guarantee. Toolbench preserves these verdicts instead of overriding
them. Each attempt includes `retry_decision` (scheduled, delay, reason,
charge_safety), adapter/classifier verdicts, protocol position and normalized
error. `retries` on an attempt is its retry ordinal; task and summary `retries`
count actual additional provider invocations, not scheduled sleeps.

`--trace-log PATH` defaults to `<results-file>.trace.log`. A file-only tracing
subscriber records Lash debug events and structured provider request/response
pairs, with model, task, repetition, channel and dialect span context. `RUST_LOG`
overrides `lash=debug,lash_core=debug,lash_provider_openai=debug,toolbench=debug`.
Requests in these pairs are facade semantic requests; response evidence also
includes the adapter's wire `request_body`. Stderr remains free of tracing output.

Every failed attempt has an error object. Transport evidence includes status,
message, raw body, headers, request id, response id, retry-after, partial response,
classification and adapter verdict. `body_excerpt` is capped at 2,000 characters;
`raw` preserves the facade's entire exposed body (the adapter already bounds
non-2xx bodies to 4,096 bytes). Request bodies are capped at 4,096 UTF-8 bytes.
Authorization, cookies, credential fields and the configured key are redacted.
Normalized diagnostics remain available as a fallback (upstream caps them at
1,024 characters). Non-completed turns carry their Debug `turn_outcome` when a
turn result exists, plus the last error; a harness exception/deadline has no
TurnOutcome and records null with its explicit error instead.

All attempts, including failures with partial responses, contribute their
reported costs, including `usage.cost` from a raw final response chunk when
the adapter omits the partial response. A run with no provider invocations costs zero; an actual call
with unavailable cost stays unknown. If cancellation interrupts a backoff, the
row retains observed calls and their partial costs even though Lash has not
sealed the call ledger. Such rows explicitly mark the retry decision unavailable.
Summary tables include Retries. Probe calls remain outside cohort cost totals.

## Accounting and forensic capture

Every `attempt` and `preflight` row uses the same OpenRouter Chat Completions
usage definition. `raw_usage` preserves the provider object; missing usage is
`null`, never zero. Cache/reasoning detail fields omitted from an otherwise
metered response are treated as zero, following the adapter's convention.

| Report field | OpenRouter usage field | Lash facade mapping |
|---|---|---|
| `prompt_tokens_total` | `prompt_tokens` | `input_tokens + cache_read_input_tokens + cache_write_input_tokens` |
| `prompt_uncached` | prompt total minus cache reads and writes | `input_tokens` |
| `cache_read` | `prompt_tokens_details.cached_tokens` | `cache_read_input_tokens` |
| `cache_write` | `prompt_tokens_details.cache_write_tokens` | `cache_write_input_tokens` |
| `completion_tokens` | `completion_tokens` | `output_tokens` |
| `reasoning_tokens` | `completion_tokens_details.reasoning_tokens` | `reasoning_output_tokens` |
| `cost_usd` | `cost` | `LlmResponse.provider_usage["cost"]` |

Source: `crates/lash-llm-transport/src/normalize.rs`,
`openai_usage_from_usage_value`; facade types `lash::direct::LlmUsage` and
`lash::provider::LlmResponse`. Reasoning is a **subset of completion**, so it is
never added again. Cache writes are part of total input, not cache reads. An
impossible cache sum leaves `prompt_uncached` unknown rather than clamping it.
The old `tokens.input` field is the uncached remainder, not prompt total; the
legacy `tokens` and `cost` objects remain attempt diagnostics only. Summary
columns and deltas use the explicit fields above.

`round` counts provider attempts from 1 within the task, including retries;
`protocol_round` counts logical model calls; `turn` is 1 because the runner
opens one turn per task. Task `rounds` and `provider_calls` count attempts;
`iterations` counts protocol iterations. Task `usage` sums every attempt,
including failed attempts. If any quantity is unknown its sum is unknown.
Preflight costs are logged separately and excluded from task cohort averages;
`excluded_route.usage` includes all failed probe attempts.

`system_prompt_tokens_first_call` is the first call's **whole prompt** count:
protocol instructions, task, message framing and tool definitions. It is a
baseline for protocol overhead, not a tokenizer measurement of the system
message alone. The cohort summary reports its mean. Later prompt totals can
be compared with that baseline to measure round-by-round growth.

`messages_chars`/`messages_bytes` measure the compact JSON message array;
`system_prompt_chars`/`system_prompt_bytes` sum compact JSON content values for
system/developer messages, and `tool_result_chars`/`tool_result_bytes` do the
same for tool messages. Tool definition count and compact JSON sizes are
separate. RLM cell observations may be carried in other message roles: use the
full message array to inspect those. Characters count Unicode scalars; bytes
count UTF-8. These measures explain relative input size, not tokenization.

Extended facade tracing captures wire request JSON and every response JSON
chunk, with contextual model/task/repetition/channel/dialect in `--trace-log`.
`--dump-requests DIR` additionally writes redacted request JSON and response
capture JSON (including ordered wire chunks, raw usage, normalized response
and errors) named by model/task/channel/dialect/repetition/turn/round. Response
captures are updated on each chunk to retain interrupted calls. Credential
values and sensitive object keys are redacted; request bodies are no longer
truncated to 4096 bytes. Dump errors are carried on attempt rows.

A task-local loopback HTTP recorder forwards the unmodified JSON request and
streams the OpenRouter response, capturing bodies before the facade's 2 KB
request projection and 4 KB failure excerpt limits. It uses the fixed OpenRouter
origin, forwards authentication only in headers, and shuts down with the task.
Every cohort uses this same recorder. Its local hop and synchronous capture I/O
are included in wall timings. `http-response.json` files preserve the received
HTTP body text (including SSE framing), updated on each chunk; normalized
`response.json` files retain parsed chunks and usage. Interrupted streams are
explicitly partial. This avoids pretending a normalized request is wire evidence.

```sh
target/debug/toolbench --model z-ai/glm-5.3-flash --paired --channel-set all \
  --dialect both --reasoning-effort medium --concurrency 8 --allow-partial \
  --results-file results.jsonl --dump-requests requests
target/debug/toolbench --reconcile results.jsonl
```

Reconciliation writes `results.jsonl.reconcile.md` and
`results.jsonl.reconcile.jsonl` (`kind: "reconcile"`). It randomly samples at
least 30 attempts, balanced across the model/channel/dialect groups present
in the input, and queries the [OpenRouter generation endpoint](https://openrouter.ai/docs/api/api-reference/generations/get-generation).
Both `tokens_prompt`/`tokens_completion` and their `native_tokens_*` counterparts
are compared, without silently replacing differently tokenized counters.
Token tolerance is exact; cost tolerance is 0.000001 USD. Missing IDs, unavailable
fields and mismatches are recorded per row with both values and produce a
nonzero exit. The report retains `cache_discount` as money; it cannot establish
cache-read token equality when `native_tokens_cached` is absent. See also
[OpenRouter prompt caching](https://openrouter.ai/docs/guides/best-practices/prompt-caching).

The validation sample from 2026-09-09 matched all native token/cache-read/reasoning
counts and costs across 60 calls. Normalized generation counters differed in
119 of 120 comparisons; this is a tokenizer distinction, not permission to
replace the native prompt counts. The reconcile reports deliberately retain
those mismatches and exit nonzero. OpenRouter documents the distinction in
[its native-versus-normalized billing explanation](https://openrouter.zendesk.com/hc/en-us/articles/51691717731483-Why-was-I-charged-more-per-token-than-the-price-shown-on-the-model-page).
