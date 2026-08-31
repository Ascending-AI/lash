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
just toolbench openai/gpt-5.2 --task string-owner --dialect typescript
```

JSON is written to stdout and a compact human table to stderr. The process
exits nonzero when any task fails; pass `--allow-partial` when collecting a
result set where model failures are expected.

The grader requires a completed turn, bounds failed execution iterations,
rejects repeated identical execution errors, checks the exact final mock-world
state (including untouched records), checks the deterministic finish value,
and enforces each task's expected tool-call count. A 120-second per-turn outer
limit converts a provider stall into a failed row so the rest of the pack can
still be graded.
