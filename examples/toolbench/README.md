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
or graded. Multiple submits in a response batch are counted before argument/ID validation
and fail with `repeated submit`; the first submitted value remains the graded
value.

The common grader checks the finish matchers (including Numeric), exact world
equality, and exact host call count, excluding
submit. Standard must submit exactly once and use at most three provider
round-trips. RLM retains at most two code executions, at most two failed
executions, and no repeated identical execution error. Every row records
`rounds`: the number of provider attempts, including transport retries, for
all channels. Summary rows sum rounds. Every task retains the 120-second
outer timeout; exceeding it fails grading.

Native and standard are preflighted separately per model. Native must call
`execute_code` once and finish with 1; standard must call `submit` with 1;
both require zero host calls. A failed probe retries once (two total tries).
Only the failing model/channel is excluded, leaving other cohorts runnable.

## Evidence and summaries

Attempt and task rows stream to stdout and `--results-file`. Task rows include
finish value, tool count, rounds, grade, wall time and aggregate usage. Summary
rows have `kind: "summary"` for each model/cohort. The Markdown tables print to
stdout and are saved at `<results-file>.summary.md`.

Tables include pass/rows, uncached input tokens, output tokens, cache reads and
writes, provider cost USD, summed task wall time, median task wall time,
tokens/task and cost/task. Lash normalizes usage into disjoint buckets: total
tokens add input, output, cache read and cache write. Every recorded attempt,
including failed retries, contributes usage. Cost is the provider usage `cost`
field, never estimated from pricing; any missing attempt cost makes the
cohort's cost `n/a`, avoiding incomplete totals. Failed task rows are included.
Probe usage is outside the task cohort totals.

Comparison lines report native versus cell per dialect and standard versus
each native dialect, using per-task mean token, cost and wall deltas. A zero
baseline or missing cost yields `n/a`. Summed wall time is task work, not the
elapsed duration of concurrent execution.

Keep new tasks small: one to three host calls, at most two code executions,
and at most three prompt sentences. Validate every channel/dialect against a
30-second target before adding a task. Cohort runs should run concurrently
across models with an approximately one-hour external wall budget. This
round ships tooling and GLM/three-task Sol smokes; the full comparison waits
for the FIG-2523 TypeScript prompt correction.
