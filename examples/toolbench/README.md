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
instead of `code`. Observations are capped at 2,000 Unicode characters and
`observation_truncated` reports whether anything was cut. Transport retries
have no execution source or observation; failed/timed-out tasks retain partial
evidence. A provider call interrupted before its ledger is sealed gets an
`interrupted` attempt row with unknown tokens/cost and attempt ordinal. Summary rows have `kind: "summary"` for each model/cohort. The Markdown tables print to
stdout and are saved at `<results-file>.summary.md`.

Tables include pass/rows, uncached input tokens, output tokens, cache reads and
writes, provider cost USD, summed task wall time, median task wall time,
tokens/task and cost/task, executions, failed executions, host calls, expected N,
matched N percentage, model rounds and unknown-cost row counts. Lash normalizes
usage into disjoint buckets: total tokens add input, output, cache read and cache write. Every recorded attempt,
including failed retries, contributes usage. Cost is the provider usage `cost`
field, never estimated from pricing; any missing attempt cost makes the
cohort's cost `n/a`, avoiding incomplete totals. Failed task rows are included.
Probe usage is outside the task cohort totals.

Comparison lines report native versus cell per dialect and standard versus
each native dialect, using per-task mean token, cost and wall deltas. A zero
baseline or missing cost yields `n/a`. Summed wall time is task work, not the
elapsed duration of concurrent execution.

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
