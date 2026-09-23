# lash

A Rust runtime for durable LLM agents.

Most agent stacks treat the LLM as the runtime and stitch state around it — a database for memory, a queue for retries, a sandbox for code. `lash` inverts that. The runtime is the durable end of the pair; the LLM is the variable call. Your app owns the outer boundaries — storage, auth, transport, product state. `lash` owns the turn — model calls, modes, tools, plugins, semantic stream events, usage, and terminal outcomes.

> **Alpha:** works today, API still moving fast — pin to an exact `=0.1.0-alpha.N` version when you embed.

## What's inside

- **Durable per-turn commits** — every completed turn lands as one atomic `RuntimeCommit` against a `SessionGraph`. Effects are the replay boundary; turns are the semantic commit boundary.
- **Workflow-host integration** — a sans-IO turn machine behind one `EffectHost` boundary. The default `NativeEffectHost` runs in-process; the first-party Restate adapter replays effects from host history, exposes durable exact-turn cancellation and terminal attachment through `TurnWorkDriver`, and retries the final idempotent commit.
- **Two execution modes, one commit unit** — `standard` uses native provider tool-calling with concurrent dispatch; `rlm` runs model-authored TypeScript, lowered into the `lashlang` IR, in a sandboxed VM where every effect crosses the host.
- **Tool providers and plugins** — ordinary host operations are `ToolProvider`s; plugins add runtime/session behavior such as prompts, planning, memory, subagents, history transforms, UI activity, catalog policy, and tool-output budgeting. Hosts compose only what they embed.
- **Provider portability** — Anthropic, OpenAI Responses, any OpenAI-compatible Chat Completions endpoint, OpenAI Codex, and Google Gemini / Code Assist. MCP servers attach through `lash-plugin-mcp`.
- **Tracing as a first-class sink** — attach a `TraceSink` for structured turn, tool, LLM, prompt, and usage records. Bundled JSONL sink + self-contained HTML viewer; optional OpenTelemetry export.

## Examples

Runnable apps under `examples/` drive the facade end-to-end, with real
persistence, remote DTO streams, and optional durable execution.

Two of them are hosts that **own** their UI. Start with `agent-service` for the
smallest production-shaped browser embedding: app-owned product state, a Lash
session per chat, and session observation live replay for reconnect. Move to
`agent-workbench` when you want the advanced Restate host with durable processes,
triggers, cron, and the same cursor-based browser stream — it is also the
reference for RLM mode.

`slack-clone` is the inverted shape, and the reference for **standard mode**: a
Slack-compatible chat platform with no Lash dependency at all, plus a Lash bot
living inside it as a guest over HTTP. Read it for the integration questions —
session per channel, ambient room context as queued turn input, idempotent
consumption of at-least-once webhooks, restart recovery — and for the native tool
loop.

```bash
# Durable chat app from a Kiln fork: SQLite or Postgres, RLM, app-owned tools, Restate turns
OPENROUTER_API_KEY=sk-or-... AGENT_SERVICE_DATA_DIR="$PWD/.agent-service" \
  kiln run //examples/agent-service:agent-service  # then open http://127.0.0.1:3000

# From a checkout without Kiln
OPENROUTER_API_KEY=sk-or-... cargo run -p agent-service

# Adds durable background work: durable processes, subagents, cron triggers (Restate required)
OPENROUTER_API_KEY=sk-or-... just agent-workbench 3000         # then open http://127.0.0.1:3000

# Lash as a bot inside someone else's product: standard mode, session per channel
OPENROUTER_API_KEY=sk-or-... just slack-clone 3040             # then open http://127.0.0.1:3040
```

See each example's README for environment knobs and Restate recipes.

Each one is a Host Application in Lash's sense: it picks the runtime crates,
providers, plugins, and Execution Mode it wants, and keeps ownership of its own
storage, transport, and presentation. A terminal application embeds Lash the same
way a web service does.

## Contributing

Feature requests and bug reports welcome — open an [issue](https://github.com/Ascending-AI/lash/issues). At this alpha stage detailed write-ups (what you tried, expected, and saw) help more than drive-by PRs — see [CONTRIBUTING.md](CONTRIBUTING.md).

## License

MIT
