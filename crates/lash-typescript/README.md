# Lash TypeScript dialect

`lash-typescript` is the source front-end for the durable Lash heap VM. SWC is
confined to `src/adapter/`; the adapter produces a Lash-owned normalized tree,
which lowers into `lashlang::Program`. Runtime type annotations are erased.

## Dialect contract

The accepted v1 surface is deliberately small: `let`/`const`, functions and
arrows with immutable captures, blocks, `if`, `while`, `break`, `continue`,
`try`/`catch`/`finally`, `throw`, `return`, arrays, records, field/index access
and assignment, calls, primitive unary/arithmetic/comparison/equality/logical
operators, conditionals, templates, and the explicitly mapped String methods.
Accepted operations follow ECMA-262 coercion, truthiness, operand-return, and
reference rules. TypeScript type annotations, aliases, and interfaces are
erased after parsing or used for signature/type work.

Everything else is rejected before execution with a stable `TS_*` diagnostic.
The executable rejection inventory in `tests/rejections.rs` is the source of
truth. In particular, v1 excludes classes, generators, async/await, `var`,
destructuring, all `for` variants, modules/imports, JSX, enums, namespaces,
decorators, `eval`/`Function`, prototype access, accessors, methods, regular
expressions, BigInt, spread, optional chaining, and operators not represented
by the accepted VM semantics.

## Deviation register draft

These are the only deliberate deviations from an otherwise accepted
ECMA-262 operation. They are runtime-system constraints rather than alternate
language semantics:

- Instruction, wall-clock, logical-memory, and call-frame limits may terminate
  execution with the existing typed VM bound errors.
- Cyclic heap objects are rejected at durable capture. Shared acyclic object
  identity is preserved byte-for-byte. Cycle-capable durable graph encoding is
  deferred; the front-end does not silently copy a cycle.
- Mutable lexical captures reject with `TS_MUTABLE_CAPTURE_UNSUPPORTED` until
  durable lexical cells exist. Immutable captures and mutation through captured
  object references are supported.
- The host boundary is JSON-shaped: object properties whose value is
  `undefined` are omitted and array elements become `null`; incoming JSON
  cannot manufacture `undefined`.

The curated test262-derived slice and its selection rule live under
`tests/test262/`.
