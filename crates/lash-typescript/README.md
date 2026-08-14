# Lash TypeScript dialect

`lash-typescript` is the source front-end for the durable Lash heap VM. SWC is
confined to `src/adapter/`; the adapter produces a Lash-owned normalized tree,
which lowers into `lashlang::Program`. Runtime type annotations are erased.

## Dialect contract

The accepted v1 surface is deliberately small: `let`/`const`, functions and
arrows with immutable captures, blocks, `if`, `while`, `break`, `continue`,
`try`/`catch`/`finally`, `throw`, `return`, arrays, records, field/index access
and assignment, calls, primitive unary/arithmetic/comparison/equality/logical
operators, conditionals, templates, array/string `.length`, the explicitly
mapped String methods, array `join`, and free `console.log` calls. `console.log`
accepts any arity and prints its arguments after ECMA `ToString` conversion,
joined by one space; lexical bindings named `console` take precedence.
Accepted operations follow ECMA-262 coercion, truthiness, operand-return, and
reference rules. TypeScript type annotations, aliases, and interfaces are
erased after parsing or used for signature/type work.

This layer treats a TypeScript cell as a pure calculator: it has no tool calls,
no `await`, and no deferred tool resolution. Tool signatures are synchronous
descriptions for a later integration layer; execution support is FIG-1305.

Everything outside the accepted surface is rejected with a stable `TS_*`
diagnostic. Most rejection is static; the deviation register names every
shape-dependent runtime rejection. The executable inventories in
`tests/rejections.rs`, `tests/structural_contract.rs`, and the checked-in Node
differential suite under `tests/differential/` are the source of truth. In
particular, v1 excludes classes, generators, async/await, `var`,
destructuring, all `for` variants, modules/imports, JSX, enums, namespaces,
decorators, `eval`/`Function`, prototype access, accessors, methods, regular
expressions, BigInt, spread, optional chaining, compound assignment operators
(`x += 1` and `a[0] += 5` alike reject with
`TS_ASSIGNMENT_OPERATOR_UNSUPPORTED`), and operators not represented by the
accepted VM semantics. The mapped String methods take exactly one argument:
their optional second parameter (`'abc'.startsWith('bc', 1)`,
`'abc'.includes('b', 2)`, `'abc'.endsWith('b', 2)`, `'abc'.split('', 2)`)
rejects. Identifiers beginning with `__typescript_` are reserved for the
lowerer's generated bindings and reject with `TS_RESERVED_IDENTIFIER`.

## Deviation register

These are the only deliberate deviations from an otherwise accepted
ECMA-262 operation. They are runtime-system constraints rather than alternate
language semantics:

- Instruction, wall-clock, logical-memory, and call-frame limits may terminate
  execution with the existing typed VM bound errors.
- TypeScript source nesting is capped at 28 levels and rejects with
  `TS_SOURCE_NESTING_LIMIT`. The cap is pinned on a 2 MiB stack; it protects both
  SWC parsing and adapter conversion before the shared AST's own limit. The
  budget is cumulative and shared: every open delimiter (`(`, `[`, `{`, and a
  template hole) and every nested recursive operator or statement form — prefix
  operators, `?:` and binary chains, `if`/`while` — draws on the same 28 units,
  in the preflight scan and in the adapter's conversion counter alike. A
  statement boundary (`;`, `,`, or the `}` that closes a statement block)
  releases the operator run it terminates, so a long flat sequence of statements
  stays one level deep.
- Cyclic heap objects are rejected at durable capture. Shared acyclic object
  identity is preserved byte-for-byte. Cycle-capable durable graph encoding is
  deferred; the front-end does not silently copy a cycle.
- Mutable lexical captures reject with `TS_MUTABLE_CAPTURE_UNSUPPORTED` until
  durable lexical cells exist. Both captured reads and captured writes take the
  same rejection path. Immutable captures and mutation through captured object
  references are supported.
- The host boundary is JSON-shaped: object properties whose value is
  `undefined` are omitted and array elements become `null`; incoming JSON
  cannot manufacture `undefined`.
- Lone UTF-16 surrogates are not representable in the v1 UTF-8 value model, so
  literals reject with `TS_LONE_SURROGATE_LITERAL_UNSUPPORTED` instead of being
  replaced or corrupted. Two further shapes produce a lone surrogate only at
  runtime and reject there with `TS_LONE_SURROGATE_UNSUPPORTED`: splitting a
  string containing an astral character into units (`'\u{1F600}'.split('')`)
  and indexing into one (`'\u{1F600}'[0]`). Both raise a catchable TypeScript
  exception, so a program can swallow the deviation and continue with a result
  Node would not produce.
- Out-of-range non-negative array writes extend with `undefined` holes, matching
  ECMAScript. Negative and other non-index array writes would create named
  object properties, which the v1 array representation cannot carry; they
  reject at runtime with `TS_ARRAY_NON_INDEX_PROPERTY_UNSUPPORTED` and never
  mutate an element.

- `console.log` is host-defined rather than ECMA-262, and prints ECMA
  `ToString` of each argument. Node's inspector formatting is not reproduced:
  `console.log({a: 1})` prints `[object Object]` where Node prints `{ a: 1 }`.

No other semantic deviation is intentionally accepted for an operation in the
surface above.

The Node differential table carries 310 rows, of which 237 are distinct
expressions: duplicates are retained deliberately so each review lane's
provenance count stays executable, and the table's effective corner coverage is
that of the 237 unique rows rather than of 310 distinct behaviours.

The curated test262-derived slice and its selection rule live under
`tests/test262/`.
