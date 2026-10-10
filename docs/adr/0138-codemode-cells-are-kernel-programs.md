# 0138: Code mode cells are kernel programs

Status: Accepted. Amends ADR 0096 (the dialect selection contract and the
shared IR), ADR 0061 (session pinning) and ADR 0056 (the code mode snapshot
root). Design: the kernel workflow spec, rev 4.

## Decision

A cell is one kernel program. The session's dialect, a kernel dialect
package (`lash-kernel-dialect`) installed in the worker under a name, lowers
the cell's source to a kernel document; a worker-hosted kernel machine runs
the document's `main`; and the kernel broker commits every park with the
admission of the tool calls the cell asked for since the last. The RLM
protocol holds no source language, no IR and no VM of its own.

## The session's dialect

A dialect is a `CellDialect`: the name its kernel package is installed
under, how its documents read numbers, and the `DialectPrompts` that word
prompts, tool paths and types in it. TypeScript (`CellDialect::typescript()`)
and Python (`CellDialect::python()`) ship. A host passes the dialect its new
sessions write to `RlmProtocolPluginFactory::new(config, dialect)` and may
install others (`with_installed_dialect`); the worker service must have each
package installed.

A session records its dialect's name at creation and reads it back from its
record on every later run. Its cells, prompts and saved bindings go through
the recorded dialect, never through the one a host would select today. A
host that has not installed the recorded dialect refuses the session, and
bindings a session of one dialect saved are refused by a state of another.

## Session state is bindings

Each cell is its own document (`K-SES-001`). What carries between cells is
the session's bindings: the variables `main` left, and the objects they
reach, which stay shared (`K-SES-002`). The protocol saves them per root
with `lash-kernel-state` fragments: one fragment per binding, inline in the
root when small and a content-addressed leaf otherwise, so a capture writes
only the bindings that changed.

A function or a task does not outlive its cell (`K-SES-003`). A binding
that reaches one is not carried, and the run's end names it. A later cell
that uses the name fails with the typed refusal `SESSION_BINDING_NOT_CARRIED`
(`CellDefect::BindingNotCarried`), which names the binding and says to
define the function again or keep the awaited result. This is the behaviour
`TS_FUNCTION_NOT_PERSISTED` had, stated once for every dialect. The bound
variables prompt lists such a name with why it is not bound.

## A cell's end

`main` running off its end completes the cell. A settled control call
(`await control.finish(value)` or `await control.continue_as(...)`, FIG-5781)
ends `main` where it settles, and the cell's outcome is
`CellOutcome::Controlled`, read from that call's settled record: the turn's
answer is the value the call carried. The control is decided at
BeforeCompletion, where input that arrives overrides it. A value no handler
took fails the cell with the error's kind and message. A cell that ends with a task unfinished, or failed with
an error nothing awaited, ends in the kernel's `TasksOutstanding`
(`K-TASK-018`): the model reads `CELL_TASKS_OUTSTANDING`
(`CellDefect::TasksOutstanding`) with the source line of the code still
running and the dialect's repair. A bound the run passed is
`CELL_BOUND_EXCEEDED`, with the typed worker limit where one corresponds.

## Tools are effects

Each catalog tool is offered to a cell as a kernel effect under the path a
cell of the session's dialect writes (`web.fetch` in TypeScript, `web_fetch`
in Python), with a signature typed from the tool's schemas in the cell's
manifest. A call path the source writes that no catalog tool answers goes
to the host's deferred resolver once per cell; a granted tool is offered as
an effect of that cell and recorded with it. A `perform` is admitted as the
tool's own execution under the call identity the broker derives, and the
cell is answered from the call's committed outcome alone. A tool's failure
is the catchable error `tool_failed`; the session's `max_tool_calls`
refusal is `tool_call_limit` and fails the cell whatever its program made
of it.

## Durability

A cell is durable through its parked state (ADR 0132 §8). Every park
commits, beside the machine's state, the cell's envelope: the document, the
effects and grants it was lowered against, its prints and the calls it
admitted. A cell with a park under its execution resumes from it on any
node, on the recorded document, and lowers and resolves nothing again. A
call that was running when its node died is not run twice: the program sees
it fail.

## What this removes

The `Dialect` trait and `TypescriptDialect`; the IR-shaped cell envelope and
every `lash_vm` type in the protocol; the Lash VM compile surface and module
artifact store of the factory; the `lash_vm_language_features` and
`binding_summary` recorded knobs; the lazy `history` projection provider
(`history` is bound as data for a cell that names it); and the cell
conformance suite whose subject was the old heap.

## Evidence

- `crates/lash-protocol-rlm/src/executor/tests.rs`: bindings carried between
  cells and across a reload, printed observations, a tool loop, resume from a
  park after a node dies, a deferred grant callable after a restart, the
  closure rule, the unjoined-task observation, and a Python session's cell.
- `crates/lash-protocol-rlm/src/executor/session/tests.rs`: a capture writes
  only the bindings that changed; another dialect's bindings are refused.
