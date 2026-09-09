# Aggregate await shapes and `?` placement

## Status

Accepted.

## Context

`await` over a literal list, tuple or record of direct module-operation calls
is an aggregate: the compiler lowers it to one host batch, every leaf starts
before any is awaited, and `?` on a leaf unwraps that leaf. A list
comprehension of the same calls was not an aggregate shape. `await [op(x)? for
x in xs]` therefore ran every `op(x)?` sequentially (each already unwrapped),
then re-awaited the finished values, and the VM's terminal `await` arm wrapped
every one of them as a `{ ok, value }` record. The cell "succeeded" with the
wrong values while the imperative `for` loop returned the right ones; the same
wrapping turned any `await` of an already-resolved value into a silent
`{ ok: false }` that read like a host failure. Both defects surfaced as model
failures in the toolbench (FIG-2764).

## Decision

1. **List comprehensions are aggregate-await shapes.** When the element of an
   awaited comprehension is a direct operation call, with or without `?`, the
   comprehension evaluates its clauses in source order (filters and nested
   clauses included), collects one `(receiver, args...)` tuple per accepted
   element, and starts every call as one host batch after the loop. `?` on
   the element unwraps every leaf and fails the cell on the first source-order
   rejection with the same diagnostic a literal aggregate raises; without `?`
   every element is a result record in comprehension order. `[await op(x)? for
   x in xs]` is unchanged: it awaits each call before starting the next. A
   comprehension whose element is not a direct call keeps the plain
   evaluate-then-await path. The grammar has no record comprehension, so
   nothing else changes.
2. **Awaiting a resolved value is a guest error.** `await` accepts a process
   handle, or a tuple, list or record whose leaves are handles. Any other value
   raises `AwaitExpectsHandle`, a catchable runtime error whose hint tells the
   model the value is already resolved, instead of returning a wrapped
   `{ ok: false }`. Real handle failures inside an aggregate still settle as
   per-item error records.
3. **TypeScript inline maps reuse the runtime-sized batch.** A direct tool call
   returned by `Promise.all(xs.map(...))` or `Promise.allSettled(xs.map(...))`
   is collected like the comprehension element. For an async callback with one
   leading direct tool await, the batch settles first and the remaining pure
   projection runs per input element afterward. TypeScript `all` selects the
   first rejection by host-recorded settlement order; Lashlang comprehensions
   retain their source-order selection. Callbacks with another await keep the
   sequential async-map driver.

The lowering adds a bytecode instruction and changes how identical source
compiles, so `BYTECODE_FORMAT_VERSION` and `LASHLANG_SEMANTIC_HASH_VERSION`
move. Workflow-graph node emission is unchanged: comprehension elements were
never execution sites and still are not.

## Consequences

`await [op(x)? for x in xs]`, `await [op(x) for x in xs]` and `[await op(x)?
for x in xs]` are pinned by runtime and compiler tests, the RLM prompt teaches
the comprehension form next to the literal one, and its claim is pinned by a
prompt-claim test. Test hosts that modelled a started process as a bare value
now mint handle records, because a bare value is no longer awaitable.
