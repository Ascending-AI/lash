# Codemode examples

These are the flagship host flows as RLM cells. They are source examples, not
language tutorials: the host API and lifecycle are the point.

- `turn.ts`: inspect two host results and finish a compact value.
- `durable-process.ts`: start a durable process that suspends on a named
  signal, resumes through a durable sleep, emits progress, and returns.

A process is an ordinary uncalled `async` arrow, and starting, signalling and
awaiting one are leaf tools the catalogue declares (ADR 0095): there is no
`defineProcess`, no bare `start` or `wake`, and no `signals:` block — the signal
set is inferred from the `waitSignal` calls the body reaches.

TypeScript is the sole RLM language (ADR 0096), so there is no dialect to state
at session creation and no second spelling of either flow to keep in parity.

Both cells are linked against a host catalogue by
`crates/lash-typescript/tests/codemode_parity_examples.rs`, so a retired
spelling fails that target instead of quietly rotting here.
