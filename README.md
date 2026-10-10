# lash

A durable, performant and customizable agent harness in Rust, built for scalable backend deployments rather than single-user coding agents.

Key features:

1. **Durable execution**
    1. Every turn commits atomically; a crashed turn resumes from its checkpoint on any node
    2. Many nodes share one PostgreSQL store, and survive being killed, partitioned and restarted
    3. Durable processes, waits and cancellation, for approvals, callbacks and triggers
    4. Sessions fork, park and resume
    5. Clients follow a turn or process with durable cursors, so a late follower catches up
2. **Performance**
    1. About 10 ms of engine overhead per tool round on PostgreSQL
3. **Deep customization** through the plugin system
4. **Swappable interfaces**
    1. Store: SQLite and PostgreSQL provided
    2. Attachments: S3-compatible storage provided
    3. Tracing: JSONL and OpenTelemetry sinks provided
    4. Models: Anthropic, OpenAI, Gemini and any OpenAI-compatible endpoint
    5. Tools: your own tool providers, plus MCP servers
    6. Deferred tools: a host resolver supplies tools on demand, so a large catalog need not be loaded up front
5. **Two main execution modes, as plugins**
    1. Standard: native provider tool calling
    2. RLM: the model works in a persistent REPL
6. **RLM, a custom code mode**
    1. The model writes code cells in a persistent REPL: state carries from cell to cell, and printed values come back as observations
    2. Tools are typed functions the code calls, so one cell can loop over and combine many calls, and run them concurrently as tasks
    3. Runs on the Lash VM, a small kernel language built for it; a cell saves its state whenever it waits, and a crashed cell resumes from there without re-running finished tool calls
    4. Dialects lower to the kernel and none is built into it; TypeScript provided
    5. Model code runs in resettable worker processes, and every effect crosses the host
    6. Deferred tools resolve mid-session, and a granted tool stays callable after a restart
7. **Workflows**
    1. Durable workflows built on the same kernel
    2. The agent writes them in TypeScript, or in any dialect that lowers to the kernel
    3. Every dialect lowers to one representation, the kernel document
    4. A host reads and edits that document through typed transactions, with no dialect code, which is what a workflow-editing UI needs


> **Alpha:** works today, API still moving fast — pin to an exact `=0.1.0-alpha.N` version when you embed.

## Examples

Runnable apps under `examples/

The agent workbench is the product E2E host: it owns its UI and product state,
opens a Lash session per chat, and resumes browser observation with durable
cursors. It carries Standard and RLM turns and durable processes. Hosts own
events, routing and scheduling. The upgrade node and external consumer remain
structural harness adapters for upgrade and public API proofs.

```bash
OPENROUTER_API_KEY=sk-or-... just agent-workbench 3000
# then open http://127.0.0.1:3000
```


## License

MIT
