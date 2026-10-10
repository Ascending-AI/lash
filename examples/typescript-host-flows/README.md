# TypeScript host flows

These are the flagship host flows as RLM cells. They are source examples, not
language tutorials: the host API and lifecycle are the point.

- `turn.ts`: inspect two host results and finish a compact value.
- `durable-process.ts`: start a durable process that parks on the host's
  `approval` tool, resumes through a durable sleep, and returns.

A process is an ordinary uncalled `async` arrow, which the dialect lowers to
an entry of the cell's kernel document. Starting and awaiting one are effects
lash supplies (`processes.start`, `processes.await`; ADR 0139). The host declares
`host.approval` as a deferring tool, records its `call_id()` and
`completion_key()`, and resolves that key through `Completions::resolve`
after the wait commits. Host delivery is deduplicated by the call id;
the host chooses any approval deadline separately.

TypeScript is the only RLM dialect lash ships. Another dialect lowers to the
same kernel with its own semantics and examples (ADR 0139).

Both cells are lowered against a host catalogue by the worker law
`the_typescript_host_flow_examples_lower_against_their_host_catalogue`
(`crates/lash-vm-worker/src/embedding.rs`), so a retired spelling fails that
law instead of quietly rotting here.
