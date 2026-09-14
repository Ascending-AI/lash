# Codemode examples

These are the flagship host flows as RLM cells. They are source examples, not
language tutorials: the host API and lifecycle are the point.

- `turn.ts`: inspect two host results and finish a compact value.
- `durable-process.ts`: define a durable process that suspends on a named
  signal, resumes through a timer, emits progress, and returns.

TypeScript is the sole RLM language (ADR 0096), so there is no dialect to state
at session creation and no second spelling of either flow to keep in parity.
