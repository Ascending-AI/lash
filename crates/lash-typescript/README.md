# Lash TypeScript dialect

`lash-typescript` is the source front-end for the durable Lash heap VM. SWC is
confined to `src/adapter/`; the adapter produces a Lash-owned normalized tree,
which lowers into `lashlang::Program`. Runtime type annotations are erased.

## Dialect contract

The accepted v1 surface is deliberately bounded: `let`/`const`, functions and
arrows with immutable captures, blocks, `if`, `while`, the canonical
`for (let i = start; i < end; i++)` form, `for...of`, `break`, `continue`,
`try`/`catch`/`finally`, `throw`, `return`, arrays, records, field/index access
and assignment, calls, primitive unary/arithmetic/comparison/equality/logical
operators, conditionals, templates, array/string `.length`, the standard-library
inventory below, and free `console.log` calls. `console.log`
accepts any arity and prints its arguments after ECMA `ToString` conversion,
joined by one space; lexical bindings named `console` take precedence.
Accepted operations follow ECMA-262 coercion, truthiness, operand-return, and
reference rules. TypeScript type annotations, aliases, and interfaces are
erased after parsing or used for signature/type work.

Cells are scripts and may use top-level `await` for tools, process handles,
`sleep`, `waitSignal`, `Promise.all`, and `Promise.allSettled`. General async
function authoring remains a named rejection; the one async function literal
surface is the `run` field of a top-level literal `defineProcess` definition.
Tool calls require `await` and use explicit `typescript.tool` module paths;
their prompt signatures return `Promise<T>`. Unknown module paths participate
in the executor's deferred tool-resolution path.

Durable work has the static shape
`const worker = defineProcess({ name: "worker", signals: {}, run: async (...) => { ... } })`.
`start`, `registerTrigger`, `wake`, `waitSignal`, `sleep`, and `finish` lower to
the shared process/effect machinery. `wake(value)` emits progress from a run;
`wake(handle, "signal", payload)` sends a declared signal to another run.
`finish` is cell-only. A normal return from `run` finishes the
process only after all enclosing `finally` blocks execute; an uncaught throw
fails it. Dynamic process definitions and targets reject with dedicated
`TS_PROCESS_*` diagnostics.

`Promise.all` and `Promise.allSettled` aggregate top-level tool promises and
already-resolved values through the shared batch machine. Nested tool promises,
non-array iterables, and process/timer promises are named rejections in v1.
`Promise.all` rejects with the reason of the leaf that settled first, and
`Promise.allSettled` keeps its results in input order, both as ECMA specifies.
The host records the order its leaves settled in as part of the journaled batch
result, so replay selects the same reason rather than re-deriving one.

A rejected `Promise.all` still waits for every leaf to settle before it reports.
ECMA specifies which reason surfaces, not when: it has no wall times, and a
conforming program cannot observe the difference except through timing. v1 has
no fail-fast cancellation of an in-flight batch leaf, so the aggregate settles
at the pace of its slowest leaf while rejecting with its first-settled reason.
This is a runtime-system constraint, not an alternate semantics.

`Date.now()` and `Math.random()` are host effects, so their result is recorded
at the same journal boundary as other effects and replay never samples the VM's
clock or RNG. `new Date()` rejects with `TS_NEW_UNSUPPORTED`.

Everything outside the accepted surface is rejected with a stable `TS_*`
diagnostic. Most rejection is static; the deviation register names every
shape-dependent runtime rejection. The executable inventories in
`tests/rejections.rs`, `tests/structural_contract.rs`, and the checked-in Node
differential suite under `tests/differential/` are the source of truth. In
particular, v1 excludes classes, generators, general async functions, `var`,
destructuring, `for...in` and non-canonical classic `for` forms, modules/imports, JSX, enums, namespaces,
decorators, `eval`/`Function`, prototype access, accessors, methods, regular
expressions, BigInt, spread, optional chaining, compound assignment operators
(`x += 1` and `a[0] += 5` alike reject with
`TS_ASSIGNMENT_OPERATOR_UNSUPPORTED`), and operators not represented by the
accepted VM semantics. Identifiers beginning with `__typescript_` are reserved for the
lowerer's generated bindings and reject with `TS_RESERVED_IDENTIFIER`.
Mutually recursive function declarations reject with
`TS_MUTUAL_RECURSION_UNSUPPORTED`; a function *expression* may still be named
and call itself by that name, and self-recursive declarations are unaffected.

The canonical classic `for` lowering rejects a `continue` that crosses a
`finally`, because the current loop epilogue would otherwise run before the
`finally`. `for...of` snapshots arrays and strings before iteration; until a
resumable iterator protocol exists, loop bodies that mutate the source or make
user-authored calls reject with `TS_FOR_OF_ITERATOR_UNSUPPORTED`.

## Deviation register

These are the only deliberate deviations from an otherwise accepted
ECMA-262 operation. They are runtime-system constraints rather than alternate
language semantics:

- Instruction, wall-clock, logical-memory, and call-frame limits may terminate
  execution with the existing typed VM bound errors.
- A `map` callback runs inside the VM and cannot perform effects. `console.log`,
  a tool call, or any other effect inside one terminates with the typed
  `EffectInBuiltinCallback` error. The callback is ordinary synchronous code:
  an `await` inside it is a parse-level rejection, so there is no suspension
  point inside `map` to make durable.
- A single JavaScript string result is capped at **8 MiB**. Multiplicative
  growth paths such as `repeat` and replacement-token expansion preflight the
  result before allocation; exceeding the cap terminates as the uncatchable
  `MemoryLimitExceeded` resource exhaustion error.
- A TypeScript cell is capped at **64 KiB** of source and rejects with
  `TS_SOURCE_TOO_LARGE`. The bound is what makes the parse-stack reservation
  finite, and 64 KiB is roughly 1 600 lines — far more than a cell should be.
  Two consequences of the reservation belong with it. Parsing a cell at the cap
  costs about **30 ms**, nearly all of it mapping and unmapping the reservation
  rather than parsing; a small cell costs well under a millisecond, and the cost
  scales with source size. And the host must be able to hand out **more than
  2 GiB of address space** for a cap-sized cell (at most 4 GiB): under a tighter
  `RLIMIT_AS`, or `vm.overcommit_memory=2`, a large cell fails closed with
  `TS_PARSE_RESOURCES_UNAVAILABLE` — a resource diagnostic, deliberately distinct
  from any diagnostic that describes the program — while small cells keep working.
- Allocation during parsing is not bounded. Stack exhaustion is arithmetically
  unreachable (see below), but a source the nesting preflight fails to reject
  could allocate without limit: SWC's duplicate-label check is quadratic in
  memory, and 64 KiB of one repeated label peaks near 37 GB when the preflight is
  disabled. No shape reaches that on the shipping path — the preflight rejects
  them all before the parse, and the worst measured peak across 164 adversarial
  shapes is 17 MB — so this is a bound the preflight carries rather than one the
  arithmetic provides.
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
  literals reject with `TS_LONE_SURROGATE_LITERAL_UNSUPPORTED`. Indexing an
  astral string at one UTF-16 unit, and `Object.values`/`Object.entries` when
  their string receiver would produce those units, reject at runtime with
  `TS_LONE_SURROGATE_UNSUPPORTED`. String methods that could manufacture a lone
  surrogate are absent from the shipped surface.
- Appending at exactly `array.length` is supported. An assignment that skips an
  index would create holes the v1 dense-list representation cannot distinguish
  from explicit `undefined`, so it rejects as `TS_SPARSE_ARRAY_UNSUPPORTED`.
  Negative and other non-index writes would create named object properties and
  reject as `TS_ARRAY_NON_INDEX_PROPERTY_UNSUPPORTED`; neither path mutates an
  element.
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
surface below.

## Standard-library inventory

The v1 inventory contains 64 method names: 37 static methods and
27 instance method names (with `toString`, `concat`, `includes`,
`indexOf`, and `lastIndexOf` shared by more than one receiver kind).

`instance_method_inventory_matches_the_lowerer` pins the list below against
`is_instance_stdlib_method`, so the register cannot drift from what the lowerer
actually accepts.

The shipped static methods are `Object.keys`, `values`, `entries`,
`fromEntries`, `hasOwn`, and `is`; `Array.isArray` and `of`;
`String.fromCodePoint`; `Number.isFinite`, `isInteger`,
`isNaN`, `isSafeInteger`, `parseFloat`, and `parseInt`; `JSON.parse` and
`stringify`; and `Math.abs`, `acos`, `asin`, `cbrt`, `ceil`, `cos`, `exp`,
`floor`, `log`, `log10`, `log2`, `round`, `sin`, `tan`, `trunc`, `max`, `min`,
`pow`, `sqrt`, and `sign`.

The shipped instance methods are `at`, `charAt`, `charCodeAt`, `codePointAt`, `concat`, `endsWith`, `includes`, `indexOf`, `join`, `lastIndexOf`, `map`, `padEnd`, `padStart`, `repeat`, `replace`, `replaceAll`, `slice`, `split`, `startsWith`, `substring`, `toLowerCase`, `toString`, `toUpperCase`, `trim`, `trimEnd`, `trimStart`, and `valueOf`. Missing methods reject with
`TS_METHOD_UNSUPPORTED` when the receiver is statically known and with the same
named typed runtime failure when only its runtime type is known. Mutating array
methods are deliberately absent; index assignment remains the supported
mutation surface.

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

### Why parsing cannot exhaust the stack

The nesting budget is not what makes this safe. SWC parses by recursive descent
and aborts the process on stack exhaustion rather than returning an error, and
five review rounds showed that a hand-written pre-parse scan cannot be relied on
to agree with SWC about every shape — each round's guard was right about the axis
it modelled and the next abort sat just outside it.

So the stack bound is arithmetic. A nesting level can cost as little as one
source byte — an unclosed `(` recurses one level per byte, and is also the most
expensive shape per level — and the measured requirement for that shape is about
22 500 bytes of stack per source byte. The parse runs on a thread reserving 8 MiB
plus 40 000 bytes per source byte, roughly 1.8x the worst measurement, on a
source that cannot exceed 64 KiB. The reservation is address space, not memory:
pages commit when touched, the worst shape at the bound touches 1.2 GB of the
2.5 GB reserved, and an ordinary cell touches a few hundred kilobytes.
`tests/no_abort_guarantee.rs` keeps the margin honest by disabling the nesting
preflight entirely and running every shape that aborted in any round — including
the unclosed-delimiter worst cases at the bound — through what remains.

The preflight stays for the diagnostic and for cost: `TS_SOURCE_NESTING_LIMIT`
with source-level wording beats a parser-depth error, and rejecting before the
parse keeps a pathological cell at 17 MB instead of 1.2 GB.

**The arithmetic covers stack, not memory.** Nothing bounds what the parser may
allocate, and with the preflight disabled a 64 KiB cell of one repeated label
peaks around 37 GB, because SWC's duplicate-label check is quadratic in memory.
On the shipping path the preflight rejects those shapes before the parse and the
worst peak across 164 adversarial shapes is 17 MB, so there is no reachable
vector — but memory is carried by the preflight being right, where stack is not.
Parsing in a subprocess, which would bring both axes under one limit, is the
change that would close it.

The budget depends on a second property besides charging the right productions:
the preflight's lexer has to agree with SWC's about where each token ends, since
a charge gated on "the previous token was an identifier" is disarmed by an
identifier that was cut in half. Identifier scanning therefore treats every byte
at or above `0x80` as an identifier character, walks `\uXXXX` and `\u{…}`
identifier escapes, and stops at U+2028/U+2029, which end a line here as they do
in ECMAScript. That classification over-approximates `ID_Continue` deliberately;
`src/adapter/nesting.rs` argues why over-approximating is the safe direction,
and both standing guards carry lexical-fidelity cases.

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
| `as` / `satisfies` cast, type operator (`keyof`, `readonly`, …) | 1 | 26 |
| flat statement sequence | 0 | unbounded |

A 26-interpolation template and a 26-link call chain are the practical ceilings
worth knowing; both reject at 27 with `TS_SOURCE_NESTING_LIMIT`.

Forms the dialect excludes are charged too, and reject on their own terms once
the budget lets them through — a labelled statement is `TS_LABEL_UNSUPPORTED`,
never an accepted construct. They are charged because the budget protects the
parser, which reads them, not the dialect, which refuses them.

A template hole costs a unit for the same reason a `+` term does: a template
lowers into a left-nested concatenation chain, so its holes deepen the tree
after they close. Charging them keeps the source budget binding before the
shared AST's generic limit, which no accepted-grammar source can reach.

The Node differential table carries 345 rows, of which 272 are distinct
expressions: duplicates are retained deliberately so each review lane's
provenance count stays executable, and the table's effective corner coverage is
that of the 272 unique rows rather than of 345 distinct behaviours. Both counts
are pinned against the table by `committed_row_counts_match_the_register`, and
the generator pins each lane's own row count, so neither this paragraph nor a
lane can drift from the corpus in silence.

The curated test262-derived slice and its selection rule live under
`tests/test262/`.
