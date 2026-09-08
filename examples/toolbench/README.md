# Toolbench

Toolbench is a small development gate for Lash's real-model RLM tool-calling
path. It runs a seeded, closed-world task pack through both the `lashlang` and
`typescript` dialects. The tools, world state, expected finish values, and
graders are deterministic; the model is the only source of variance.

The default run needs a real OpenRouter model and `OPENROUTER_API_KEY`. The
binary loads the repository `.env` when present and never prints credentials.
It is intentionally not part of CI.

```sh
just toolbench
just toolbench z-ai/glm-5.3-flash --runs 3
just toolbench openai/gpt-5.2 --task kv-read --dialect typescript
```

JSON is written to stdout and a compact human table to stderr. The process
exits nonzero when any task fails; pass `--allow-partial` when collecting a
result set where model failures are expected.

The grader requires a completed turn, bounds failed execution iterations,
rejects repeated identical execution errors, checks the exact final mock-world
state (including untouched records), checks the deterministic finish value,
and enforces each task's expected tool-call count and a maximum of two code
executions. A 120-second per-turn outer limit converts a provider stall into a failed row so the rest of the pack can
still be graded.

For a channel comparison, pair each task and dialect in randomized channel
order, repeat three times, and write the attempt and task rows to JSONL:

```sh
target/debug/toolbench --model z-ai/glm-5.3-flash --dialect both \
  --paired --repetitions 3 --results-file /tmp/toolbench-glm.jsonl --allow-partial
```

Run each cohort model in its own concurrent process with a distinct results
file. Sum every attempt's usage, including retries, when comparing tokens.
The small-task pack admits only tasks that pass a default-model validation on
both channels and both dialects in under 30 seconds per task. Prompts contain
at most three sentences, require one to three host calls, and finish with a
scalar or an object with at most three keys. Validation is an admission sample,
not a guarantee that later model runs will pass.

The pack contains five admitted tasks: weather temperature, weather condition,
KV read, mail count, and weather-to-KV. Three paired repetitions across both
dialects produce 60 task rows per model.
