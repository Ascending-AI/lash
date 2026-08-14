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
Mutually recursive function declarations reject with
`TS_MUTUAL_RECURSION_UNSUPPORTED`; a function *expression* may still be named
and call itself by that name, and self-recursive declarations are unaffected.

## Deviation register

These are the only deliberate deviations from an otherwise accepted
ECMA-262 operation. They are runtime-system constraints rather than alternate
language semantics:

- Instruction, wall-clock, logical-memory, and call-frame limits may terminate
  execution with the existing typed VM bound errors.
- TypeScript source nesting is capped at **28 budget units** and rejects with
  `TS_SOURCE_NESTING_LIMIT`. The cap is pinned on a 2 MiB stack; it protects both
  SWC parsing and adapter conversion, and it binds before the shared AST's own
  nesting limit for every shape the grammar accepts. See
  [Source nesting budget](#source-nesting-budget) for what a unit costs.
- Mutually recursive function declarations reject with
  `TS_MUTUAL_RECURSION_UNSUPPORTED`, naming the cycle
  (`cycle: isEven -> isOdd -> isEven`). v1 captures by value, so a declaration
  cycle has no emission order; routing it through a shared mutable record would
  build a heap cycle reachable from a durable root, which is exactly what the
  deferred cycle-capable durable graph encoding below cannot hold — the program
  would run and then fail to suspend or snapshot. Failing closed at compile time
  is the honest form of that same deferral. Self-recursion, named self-recursive
  function expressions, nested declarations, and acyclic declaration chains are
  all unaffected.
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
- A block-scoped binding whose name shadows one already in scope is lowered to
  a generated slot, so that the inner binding cannot overwrite the outer one.
  At root that slot is a runtime global, which makes it the one place a
  `__typescript_` name appears in persisted session state. It is dead by any
  turn boundary and the dialect filters the reserved prefix out of the
  bound-variables prompt, so it is never shown; a block binding that shadows
  nothing keeps the name its author wrote.

No other semantic deviation is intentionally accepted for an operation in the
surface above.

## Source nesting budget

The 28 is a budget in units, not a count of visible levels: it is cumulative
across delimiters *and* operators, so an apparent level often costs two units.
Every open delimiter costs one unit until it closes; every nested recursive
operator or statement form costs one unit until its statement ends; and every
postfix tail — a call, a subscript, a member step, a tagged template — costs one
unit that survives the tail closing, because the tail leaves the tree one level
deeper than it found it.

A statement boundary — `;`, `,`, the `}` that closes a statement block, or a
newline in automatic-semicolon-insertion position — releases the operator run it
terminates, so a flat sequence of statements stays one level deep however long
it runs, punctuated or not. A newline releases nothing while a statement form is
still open (`if (1)` on its own line), when the previous token opens an operand
(`typeof` on its own line), or when the next token continues the expression (a
leading `.` or `+`), because none of those is a statement end. A trailing `//`
comment does not suppress the release.

The families the budget charges are the recursive productions of **the grammar
SWC parses** — all of TypeScript, not the subset this dialect accepts, because
the preflight runs before the parser and a production rejected later still
recurses in it. They are prefix, infix, postfix, delimiter and statement form,
the last including labelled statements. `src/adapter/nesting.rs` argues why the
list is exhaustive; `tests/depth_guard.rs` turns the argument into a generative
regression — every family and mixed combinations of them, repeated to 100,000
in a child process on the 2 MiB stack contract, inline and one per line, must
return `TS_SOURCE_NESTING_LIMIT` and exit cleanly. `tests/grammar_coverage.rs`
cross-checks the list mechanically: an exhaustive match over SWC's own AST node
kinds that stops compiling when SWC gains a variant, and a deterministic fuzzer
whose sources are parsed inside a child process where an abort fails the test.

Measured ceilings inside a single `const x = …;` statement, which itself spends
one unit on the `=`:

| Form | Cost per level | Max nesting |
| --- | ---: | ---: |
| grouping `(…)`, array `[…]`, object `{a: …}`, nested call `f(f(…))` | 1 | 26 |
| prefix operator (`!`, `typeof`, unary `-`) | 1 | 26 |
| member step `.a`, ternary `?:`, template hole `${…}` | 1 | 26 |
| postfix chain link (`f(1)(1)…`, `a[0][0]…`) | 1 | 26 |
| binary chain term (`1 + 1 + …`) | 1 | 27 |
| statement block `{ … }` | 1 | 27 |
| `if (…) { … }` / `while (…) { … }` block | 2 (keyword + brace) | 13 |
| `else if` branch | 1 | 25 |
| label `a:` | 1 | 27 |
| `as` / `satisfies` cast, type operator (`keyof`, `readonly`, …) | 1 | 26 |
| flat statement sequence | 0 | unbounded |

A 26-interpolation template and a 26-link call chain are the practical ceilings
worth knowing; both reject at 27 with `TS_SOURCE_NESTING_LIMIT`.

A template hole costs a unit for the same reason a `+` term does: a template
lowers into a left-nested concatenation chain, so its holes deepen the tree
after they close. Charging them keeps the source budget binding before the
shared AST's generic limit, which no accepted-grammar source can reach.

The Node differential table carries 310 rows, of which 237 are distinct
expressions: duplicates are retained deliberately so each review lane's
provenance count stays executable, and the table's effective corner coverage is
that of the 237 unique rows rather than of 310 distinct behaviours.

The curated test262-derived slice and its selection rule live under
`tests/test262/`.
