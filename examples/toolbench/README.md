# Toolbench

Toolbench runs 16 easy tasks and 12 hard tasks against real OpenRouter models.
The easy pack covers weather, string extraction, KV, mail and world updates;
the hard pack covers chained retail and operations workflows. Every task owns an isolated
seeded world; the model is the source of variance. Set `OPENROUTER_API_KEY`
(or load the repository `.env`). Credentials are never printed.

## Cohorts and flags

- `--pack easy|hard|all` selects tasks (default: `all`). Task and attempt JSONL
  rows identify their concrete `pack`; summary and exclusion rows identify the
  selected pack. `--task` IDs must belong to that selection.

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

All channels, one repetition, both dialects: easy gives 80 rows/model, hard
gives 60, and the default combined pack gives 140. The two-model default
comparison uses medium reasoning and 280 task rows:

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
recorded in `submit_values`, while malformed arguments are recorded in
`malformed_submits`. Malformed submits are metrics only, so a malformed call
followed by a valid submit does not conflict; two differing well-formed values
fail with `conflicting submits`, and the first executed value is graded.

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

Tables show pass/rows, Attempts, total prompt tokens, cache reads and writes,
completion and reasoning tokens, provider cost, summed/median task wall time,
and per-task means. Every attempt, including retries and failures, contributes.
Missing metering makes the corresponding total `n/a`. Probe usage is separately
logged outside the task cohort totals. Deltas compare each RLM cohort against
standard using per-task means of the same quantity; zero or unknown baselines
produce `n/a`. Wall totals sum task durations, not concurrent elapsed time.
See the field mapping below for exact definitions.

## Tasks

Easy tasks retain their original prompts and expected answers. Hard tasks ask
for an outcome, with at most two task sentences, and require intermediate
inspection. The hard pack deliberately supersedes the easy pack's 1–3-call
limit: oracle solutions use 5–10 host calls. Counts exclude submit and are
metrics, not pass/fail limits. The harness deadline is 120 seconds per task;
run models concurrently with a one-hour external budget.

| Task (prefix `hard-`) | Domain | Source | Oracle calls | Inspection / branch |
|---|---|---|---:|---|
| retail-refund | Retail | [τ-bench][tau] | 9 | Delivery status and return age; refund and read back |
| retail-exchange | Retail | [τ-bench][tau] | 6 | Structured stock refusal; affordable stocked alternative; verify |
| retail-reschedule | Retail | [τ-bench][tau] | 7 | Payment policy, pending status and earliest day; verify |
| retail-reprice | Retail | [NESTFUL][nestful] | 7 | Return-window filter; quantity/price join and difference |
| retail-lamps | Retail | [NESTFUL][nestful] | 8 | SKU/category join and delivered filter; unordered IDs |
| retail-best-return | Retail | [τ-bench][tau] | 5 | Return-window filter; paid-per-unit ranking with tie rule |
| ops-deploy-recovery | Operations | [NESTFUL][nestful] | 8 | Capacity refusal; newest compatible passed release; verify |
| ops-resolve-chain | Operations | [NESTFUL][nestful] | 9 | Open prerequisites and required fixes; resolve and verify |
| ops-impact | Operations | [NESTFUL][nestful] | 5 | Open status and severity filter before aggregation |
| ops-oncall | Operations | [NESTFUL][nestful] | 10 | Cross-service ranking, owner join and contact availability |
| ops-ready | Operations | [NESTFUL][nestful] | 6 | Open-dependency exclusion and release compatibility |
| ops-blocked-impact | Operations | [NESTFUL][nestful] | 7 | Incident groups joined to failed release checks |

[tau]: https://github.com/sierra-research/tau-bench
[nestful]: https://github.com/IBM/NESTFUL

These are original tasks inspired by benchmark ideas, not imported benchmark
samples. τ-bench is [MIT licensed](https://github.com/sierra-research/tau-bench/blob/main/LICENSE);
NESTFUL is [Apache-2.0 licensed](https://github.com/IBM/NESTFUL/blob/main/LICENSE).
No upstream prompts, schemas, data or code are copied; operations translates
NESTFUL's nested-argument idea into an original deployment/incident world.

Retail has 16 records (3 customers, 7 orders, 6 products); operations has 18
(3 services, 6 incidents, 6 releases, 3 teams). Each hard task exposes its domain's eight tools with strict input/output
schemas and identical deterministic handlers in every cohort; easy tasks keep
their original seven-tool catalog. Unrelated domain tools are excluded from
the prompt to avoid spending the latency budget on irrelevant schemas. Lists of IDs require detail lookups; mutation receipts require readback
for current state. Domain refusals return `{"error":{"code":"..."}}` as JSON
in all channels and do not mutate state. Pending orders are unallocated; an
exchange reserves only the replacement stock. Refunds are idempotent.

`hard_tests.rs` supplies an independent Rust oracle closure for every task,
checks call counts and exact end state through the common grader, and rejects
unrequested writes. Unordered ID answers use structural set equality (duplicates
have set semantics); object answers use exact structural equality. Tests also
pin refusal atomicity, payment/return/dependency policies and strict arguments.

Calibration uses both requested models, standard plus native/TypeScript with
two repetitions (four observations/model/task). Acceptance targets are Sol
≥3/4 and GLM ≥1/4, followed by all five cohorts with one repetition. Do not
interpret correct multi-step executions as trivial solely because both models
succeed; inspect the observed dependent tool use.

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
Upstream transport failures at the recorder surface to Lash as HTTP 502 JSON
(`recorder_upstream_transport`), so retry classification follows the HTTP path.
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
Summary tables label provider invocations Attempts and include Retries. Probe
calls remain outside cohort cost totals.

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

The RLM host explicitly disables image/type-literal/decomposition prompt
features, label annotations, processes, sleep, process signals and triggers,
and disables continuation soft warnings. This removes what the current
renderer gates; the `control.continue_as` catalogue entry and the lashlang
label/process/sleep teaching are gated separately by FIG-2750 (#1172) and
still appear on renderers before that change.

One-task `kv-read` smoke on `z-ai/glm-5.3-flash`, medium reasoning, paired all
channels and both dialects, one repetition, concurrency 8 (2026-09-09):

| Cohort | First-call prompt before | First-call prompt after | Change |
|---|---:|---:|---:|
| standard/none | 1064 | 1064 | 0 |
| cell/lashlang | 5114 | 4467 | -647 |
| native/lashlang | 5143 | 4506 | -637 |
| cell/typescript | 4527 | 4527 | 0 |
| native/typescript | 4590 | 4590 | 0 |

These are whole-prompt usage counts, not isolated system-message tokens.
Both runs used main `367b35ceefeb2944995b55d74a2677f50d4ad151` as their base;
#1172 (additional host-capability prompt gating) was still open. TypeScript's
counts were unchanged with the prompt renderer on that base. All five tasks
passed in each run; the largest task wall time was 11.387 seconds.

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
captures are written once at provider completion or cancellation to retain
interrupted calls without repeatedly serializing accumulated chunks. Credential
values and sensitive object keys are redacted; request bodies are no longer
truncated to 4096 bytes. Dump errors are carried on attempt rows.

A task-local loopback HTTP recorder forwards the unmodified JSON request and
streams the OpenRouter response, capturing bodies before the facade's 2 KB
request projection and 4 KB failure excerpt limits. It uses the fixed OpenRouter
origin, forwards authentication only in headers, and shuts down with the task.
Every cohort uses this same recorder. Its local hop and synchronous capture I/O
are included in wall timings. `http-response.json` files preserve the received
HTTP body text (including SSE framing), written once at stream end; normalized
`response.json` files retain parsed chunks and usage. Interrupted streams are
marked `partial: true`, including outer task deadlines. Raw bytes accumulate
losslessly (including split UTF-8); per-chunk logging and one final dump keep
capture work linear in response size. A bounded in-memory bridge owns each
accepted connection; task teardown aborts these connection tasks and closes
their sockets. This avoids pretending a normalized request is wire evidence.

```sh
target/debug/toolbench --model z-ai/glm-5.3-flash --paired --channel-set all \
  --dialect both --reasoning-effort medium --concurrency 8 --allow-partial \
  --results-file results.jsonl --dump-requests requests
target/debug/toolbench --reconcile results.jsonl
```

Reconciliation writes `results.jsonl.reconcile.md` and
`results.jsonl.reconcile.jsonl` (`kind: "reconcile"`). It randomly samples at
least 30 attempts, balanced across the model/channel/dialect groups present
in the input (or all attempts when fewer than 30 exist), and queries the [OpenRouter generation endpoint](https://openrouter.ai/docs/api/api-reference/generations/get-generation).
Both `tokens_prompt`/`tokens_completion` and their `native_tokens_*` counterparts
are compared, without silently replacing differently tokenized counters.
Native token tolerance is exact; cost tolerance is 0.000001 USD. Interrupted
attempts, including cancelled generations with missing local usage, are
reported in an `interrupted` bucket with the generation cost and do not gate
exit; missing IDs, unavailable native/cost evidence, native
prompt/completion/cached/reasoning mismatches and cost mismatches beyond that
tolerance produce a nonzero exit for completed attempts.
Normalized `tokens_prompt`/`tokens_completion` differences are informational:
`normalized_mismatch` counts differing normalized fields per JSONL row, and
the Markdown report shows their per-field column and total count. They never
affect exit status; neither does a sample smaller than 30 attempts. The report
retains `cache_discount` as money; it cannot establish
cache-read token equality when `native_tokens_cached` is absent. See also
[OpenRouter prompt caching](https://openrouter.ai/docs/guides/best-practices/prompt-caching).

Normalized generation counters can use a different tokenizer from native
usage; they are never substituted for the native prompt counts. See
[OpenRouter's native-versus-normalized billing explanation](https://openrouter.zendesk.com/hc/en-us/articles/51691717731483-Why-was-I-charged-more-per-token-than-the-price-shown-on-the-model-page).
