# lash

A Rust runtime for durable LLM agents.

Most agent stacks treat the LLM as the runtime and stitch state around it — a database for memory, a queue for retries, a sandbox for code. `lash` inverts that. The runtime is the durable end of the pair; the LLM is the variable call. Your app owns the outer boundaries — storage, auth, transport, product state. `lash` owns the turn — model calls, modes, tools, plugins, semantic stream events, usage, and terminal outcomes.

> **Alpha:** works today, API still moving fast — pin to an exact `=0.1.0-alpha.N` version when you embed.

## What's inside

- **Durable per-turn commits** — every completed turn lands as one atomic `RuntimeCommit` against a `SessionGraph`. Effects are the replay boundary; turns are the semantic commit boundary.
- **Workflow-host integration** — a sans-IO turn machine behind one `EffectHost` boundary. Lash's durable engine runs it over the SQLite or PostgreSQL store, exposes durable exact-turn cancellation and terminal attachment through `TurnWorkDriver`, and commits each turn once.
- **Two execution modes, one commit unit** — `standard` uses native provider tool-calling with concurrent dispatch; `rlm` runs model-authored TypeScript, lowered into the `lashlang` IR, in resettable worker processes where every effect crosses the host. Language limits bound guest authority and processes contain native crashes; OS confinement belongs to the host.
- **Tool providers and plugins** — ordinary host operations are `ToolProvider`s, delivered at least once and keyed for idempotency on the `call_id()` lash mints for each call; plugins add runtime/session behavior such as prompts, planning, memory, delegation, history transforms, UI activity, catalog policy, and tool-output budgeting. Hosts compose only what they embed.
- **Provider portability** — Anthropic, OpenAI Responses, any OpenAI-compatible Chat Completions endpoint, OpenAI Codex, and Google Gemini / Code Assist. MCP servers attach through `lash-plugin-mcp`.
- **Tracing as a first-class sink** — attach a `TraceSink` for structured turn, tool, LLM, prompt, and usage records. Bundled JSONL sink; optional OpenTelemetry export through a host-installed `OtelTelemetry` adapter (`lash::tracing`), whose span, attribute and metric contract is published in `crates/lash/docs/instrumentation-contract.md`.

## Examples

Runnable apps under `examples/` shift the facade end-to-end, with real
persistence, local observation streams, and optional durable execution.

The agent workbench is the product E2E host: it owns its UI and product state,
opens a Lash session per chat, and resumes browser observation with durable
cursors. It carries Standard and RLM turns, durable processes. Hosts own events, routing and scheduling
([ADR 0136](docs/adr/0136-the-host-owns-events-routing-and-scheduling.md)).
The upgrade node and external consumer remain structural harness adapters for
upgrade and public API proofs.

Lash ships no subagent implementation: creating a session is explicit and only
a fork clones ([ADR 0134](docs/adr/0134-creating-a-session-is-explicit-only-a-fork-clones.md)).
`examples/delegation` shows a host-written delegation tool on the facade alone:
it creates a child session with explicit input, runs it and routes its result
back to the calling turn.

```bash
OPENROUTER_API_KEY=sk-or-... just agent-workbench 3000
# then open http://127.0.0.1:3000
```

See each example's README for environment knobs and recipes.

Each one is a Host Application in Lash's sense: it picks the runtime crates,
providers, plugins, and Execution Mode it wants, and keeps ownership of its own
storage, transport, and presentation. A terminal application embeds Lash the same
way a web service does.

## Contributing

Feature requests and bug reports welcome — open an [issue](https://github.com/Ascending-AI/lash/issues). At this alpha stage detailed write-ups (what you tried, expected, and saw) help more than shift-by PRs — see [CONTRIBUTING.md](CONTRIBUTING.md).

## License

MIT
