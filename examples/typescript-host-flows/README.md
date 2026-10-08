# TypeScript host flows

These are the flagship host flows as RLM cells. They are source examples, not
language tutorials: the host API and lifecycle are the point.

- `turn.ts`: inspect two host results and finish a compact value.
- `durable-process.ts`: start a durable process that parks on the host's
  `approval` tool, resumes through a durable sleep, and returns.

A process is an ordinary uncalled `async` arrow. Starting and awaiting one
are leaf tools the catalogue declares (ADR 0095). The host declares
`host.approval` as a deferring tool, records its `call_id()` and
`completion_key()`, and resolves that key through `Completions::resolve`
after the wait commits. Host delivery is deduplicated by the call id;
the host chooses any approval deadline separately.

TypeScript is the only shipped RLM dialect today (ADR 0096). Future dialects
may lower into the same IR and VM with their own semantics and examples.

Both cells are linked against a host catalogue by
`crates/lash-typescript/tests/host_flow_examples.rs`, so a retired
spelling fails that target instead of quietly rotting here.
