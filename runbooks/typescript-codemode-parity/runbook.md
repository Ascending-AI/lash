# E2E Scenario: TypeScript codemode parity

> **Read [../RULES.md](../RULES.md) first.** This TypeScript-only runbook adds
> the surface that has no Lashlang twin. It still obeys the real-token,
> screenshot, objective-gate, Abort/RCA, and teardown rules.

**Purpose.** Prove first-shot TypeScript fluency for aggregate settlement,
`for...of` agent work, and the complete durable `defineProcess` lifecycle,
including suspension across a real Workbench worker restart.

## Golden rules

1. Pin the session to `typescript` at creation. Before sending work, require the
   rendered system prompt and stream metadata to name `typescript`, never
   `lashlang`.
2. Use `gpt-5.6-sol` or newer for every RLM step. Record the actual execution
   and judge models in `00-models.json`; substitutions are evidence, not prose.
3. Ask for outcomes and constraints, not source. The model authors every cell
   first-shot. A missing method or named `TS_*` rejection is recorded verbatim
   in `fluency-hits.json` and fails that row; do not extend the dialect during
   the judged run.
4. Restart with the repository helper and the exact original run/data
   directories. The pre-restart process id, execution-state engine id, and
   post-restart process id must agree.

## Phase 0 — Boot and dialect gate

Boot a fresh Workbench with `LASH_RUNBOOK_DIALECT=typescript`, unique ports,
and a fresh persistent data directory. Gate `/healthz`, `/api/state`, the
rendered session id, and the prompt/trace language id. Save `00-ready.png`,
`00-state.json`, and `00-models.json`.

## Phase 1 — First-settled `Promise.all`

Expose two test tools through the normal host catalog: input row A rejects
after five seconds with marker `A`; input row B rejects after ten milliseconds
with marker `B`. Ask the model to run both in one `Promise.all`, catch the
failure, and finish with the observed marker. Require `B`, two completed tool
attempts, and one aggregate execution. Save `01-promise-{dom,state,trace}.json`
and `01-promise.png`.

## Phase 2 — `for...of` agent loop

Ask the model to fetch one array of three work items, iterate that returned
array with `for...of`, and finish with an ordered summary. The loop body stays
pure, as required by the documented v1 iterator guard. Require one tool
outcome, all three ordered items, and no `TS_FOR_OF_ITERATOR_UNSUPPORTED`. Save
`02-for-of-{dom,state,trace}.json` and `02-for-of.png`.

## Phase 3 — Durable process definition and signal

Ask the model to define and start a process with one named signal. The process
must emit progress, wait for the signal, sleep durably, and return the signal
payload. Require the process artifact's compilation dialect to be
`typescript`, a running handle, and a visible waiting state. Save
`03-suspended-{dom,state,store,trace}.json` and `03-suspended.png`.

## Phase 4 — Worker restart and resume

Restart the Workbench worker while the process is waiting, using the same run
and data directories. Gate readiness, reopen the same session, and require the
restored execution engine id to remain `typescript`. In a new full judged
codemode turn, ask the model to signal the existing handle, observe the resumed
process through its documented host surface, and finish with the returned
payload. Require one durable run (not a replacement), terminal success, the
pre-restart process id, and a TypeScript cell in the resumed turn. Save
`04-resumed-{dom,state,store,trace,judge}.json` and `04-resumed.png`.

## Phase 5 — Teardown and score

Stop everything started by this row. Write `fluency-hits.json` even when empty.

| Gate | Result | Evidence |
| --- | --- | --- |
| Production session pinned to TypeScript | | `00-state.json`, trace |
| First-settled rejection is `B` | | `01-promise-*` |
| `for...of` agent loop completes in order | | `02-for-of-*` |
| Process artifact and suspended engine are TypeScript | | `03-suspended-*` |
| Worker restart preserves process id and dialect | | `04-resumed-*` |
| Full resumed judged turn finishes correctly | | `04-resumed-judge.json` |
| Missing-method/rejection hit list recorded | | `fluency-hits.json` |
