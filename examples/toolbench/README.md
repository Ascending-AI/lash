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
just toolbench openai/gpt-5.2 --task kv-read --task weather-condition --dialect typescript
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

Machine-graded sessions explicitly request raw final values for both channels
and dialects, so the root-session Markdown preference cannot alter finish types.

For a channel comparison, pair each task and dialect in randomized channel
order, repeat five times, and write the attempt and task rows to JSONL:

```sh
target/debug/toolbench --model z-ai/glm-5.3-flash --dialect both \
  --paired --repetitions 5 --concurrency 8 --results-file /tmp/toolbench-glm.jsonl --allow-partial
```

The comparison cohort uses `z-ai/glm-5.3-flash`. Sum every attempt's usage,
including retries and cache buckets, when comparing tokens. Prompts contain
at most three sentences, request one to three host calls, and finish with a
scalar or an object with at most three keys. Validate all four channel/dialect
combinations against a 30-second target. Keep tasks that still fail after one
prompt correction: those failures are benchmark evidence.

The default pack contains all 16 weather, string extraction, KV, mail,
missing-field and targeted-update tasks. Five paired repetitions across both
dialects produce 320 task rows. Repeat `--task <id>` to select a subset;
duplicate selections run once, and unknown IDs are errors.

Use `--concurrency N` to bound simultaneous task runs (default 1, preserving
serial execution). Start at 4–8 for OpenRouter: rate limits belong to the model
route. A single serial native preflight runs before the fan-out; each task owns
its world, telemetry and in-memory stores, and retains its 120-second timeout.
Attempt and task rows stream to stdout and `--results-file` as runs complete;
stdout ends with the aggregate JSON result. The stderr table is printed once,
sorted by repetition, dialect, task and channel. For example, add
`--concurrency 8` to the paired command above.
