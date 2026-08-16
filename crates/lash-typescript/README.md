# Lash TypeScript dialect

`lash-typescript` is the source front-end for the durable Lash heap VM. SWC is
confined to `src/adapter/`; the adapter produces a Lash-owned normalized tree,
which lowers into `lashlang::Program`. Runtime type annotations are erased.

## Dialect contract

The accepted v1 surface is deliberately bounded, but includes ordinary model-authored
constructs: `let`/`const`/`var` (including multiple declarations), functions and
async helpers, blocks, `if`, `while`, `do...while`, the canonical
`for (let i = start; i < end; i++)` form, `for...of`, `for...in`, `switch`,
`break`, `continue`, `try`/`catch`/`finally`, `throw`, `return`, destructuring in
every binding and assignment position, defaults/rest, optional chains, array/call/object
spread, compound/logical assignment, update operators, arrays, records, and calls.
Arithmetic includes exponentiation and ECMA `ToInt32`/`ToUint32` bitwise and shift
operators. `in` is an own-property query because dialect objects have no prototypes;
`instanceof` accepts the Error family, Map, Set, Date, RegExp, URL,
URLSearchParams, Array, and Object.
`console.log`, `console.warn`, `console.error`, `console.info`, and
`console.debug` accept any arity and emit through the existing print-observation
channel after ECMA `ToString` conversion, joined by one space; lexical bindings
named `console` take precedence.
Accepted operations follow ECMA-262 coercion, truthiness, operand-return, and
reference rules. Type-level TypeScript syntax is erased: annotations,
interfaces, type aliases, generics and type arguments, `as`/angle-bracket
assertions, `satisfies`, and postfix non-null `!` do not exist at runtime.
Non-const enums create the same runtime object shape as `tsc`: numeric members
have forward and reverse mappings, string members have forward mappings, and
computed initializers run in declaration order. Const-enum member reads inline
their constant number or string. Decorators and namespaces/modules remain
named rejections: `TS_DECORATOR_UNSUPPORTED` and `TS_NAMESPACE_UNSUPPORTED`.

Type-level TypeScript syntax is erased, not executed: annotations, interfaces,
type aliases, generics, `as`, `satisfies`, and postfix non-null `!` all lower to
the same runtime program as their untyped form. `enum`, namespaces, and
decorators are not type-only in this contract and reject as
`TS_ENUM_UNSUPPORTED`, `TS_NAMESPACE_UNSUPPORTED`, and
`TS_DECORATOR_UNSUPPORTED`. The checked-in Test262 census records these
TypeScript-only rulings beside the official ECMAScript inventory.

Cells are scripts and may use top-level `await` for tools, process handles,
`sleep`, `Promise.all`, and `Promise.allSettled`; `waitSignal` is
process-only and rejects at the cell top level by name. Async functions and arrows
are accepted when every awaited value is transitively grounded in this agent surface;
`await Promise.all(xs.map(async x => ...))` and its `Promise.allSettled`
counterpart use the durable sequential async-map driver. The all-settled form
wraps each callback in guest `try`/`catch`, so a rejection becomes that input's
`{status: "rejected", reason}` record and later callbacks still run. Promise
chaining, synthetic promises, `race`, and `any` remain named rejects.
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

`Date.now()`, argless `new Date()`, and `Math.random()` are host effects, so their result is recorded
at the same journal boundary as other effects and replay never samples the VM's
clock or RNG. The Error family plus Map, Set, Date, and RegExp are the explicit
exceptions to the general `new` rejection.

`new Date(milliseconds)`, UTC-pinned multi-argument construction,
`Date.UTC`, ISO-only `Date.parse`, all `getUTC*` getters, `getTime`, `valueOf`,
`toISOString`, and `toJSON` are accepted. Date values are immutable. The
Map/Set surface includes `size`, `get`/`set`/`add`, `has`, `delete`, `clear`,
`forEach`, and the iterator-sink forms of `keys`/`values`/`entries`.

`encodeURIComponent`, `decodeURIComponent`, `encodeURI`, and `decodeURI` lower
to deterministic pure VM codecs. Malformed percent encodings and lone-surrogate
encoder literals throw a real heap-backed `URIError("URI malformed")`. `btoa`
and `atob` remain named rejections with a host-tool repair because Node exposes
their failures as `DOMException`, which is not a runtime heap kind.

`URL` and `URLSearchParams` are durable mutable heap objects with reference
identity. `new URL(input, base?)` accepts an absolute URL or resolves against a
base. Its `href`, `protocol`, `username`, `password`, `host`, `hostname`,
`port`, `pathname`, `search`, and `hash` setters reparse and normalize with
WHATWG semantics. `origin` and `searchParams` are WHATWG getter-only
attributes; assignment is Node's non-strict no-op. `searchParams` is one stable,
live object: params mutations immediately update `href`, and URL `href` or
`search` assignment refreshes that same params object.

The accepted URL signatures are:

```typescript
new URL(input: unknown, base?: unknown)
URL.canParse(input: unknown, base?: unknown): boolean
url.toString(): string
url.toJSON(): string

new URLSearchParams(init?: string | Array<[unknown, unknown]> | Record<string, unknown> | URLSearchParams)
params.get(name: unknown): string | null
params.getAll(name: unknown): string[]
params.set(name: unknown, value: unknown): void
params.append(name: unknown, value: unknown): void
params.delete(name: unknown, value?: unknown): void
params.has(name: unknown, value?: unknown): boolean
params.sort(): void
params.size: number
params.toString(): string
params.forEach(callback: (value: string, name: string, params: URLSearchParams) => void, thisArg?: unknown): void
params.keys(): Iterable<string>
params.values(): Iterable<string>
params.entries(): Iterable<[string, string]>
```

The params constructor preserves duplicate keys and insertion order. Object
keys follow the dialect-wide ECMA property-enumeration order. Serialization is
UTF-8 `application/x-www-form-urlencoded`, so a
space becomes `+` while a literal plus becomes `%2B`. Params are directly
iterable as entries; `keys()`, `values()`, `entries()`, and the params object
itself may be consumed directly by `for...of`.

Everything outside the accepted surface is rejected with a stable `TS_*`
diagnostic. Most rejection is static; the deviation register names every
shape-dependent runtime rejection. The executable inventories in
`tests/rejections.rs`, `tests/structural_contract.rs`, and the checked-in Node
differential suite under `tests/differential/` are the source of truth. In
particular, v1 excludes classes, generators, non-canonical classic `for` forms,
modules/imports, JSX, namespaces, decorators, `eval`/`Function`, prototype
access, accessors, BigInt, sequence expressions, labels, `for await`,
and arbitrary constructors or `instanceof` right-hand sides. Identifiers beginning
with `__typescript_` are reserved for the
lowerer's generated bindings and reject with `TS_RESERVED_IDENTIFIER`.
Mutually recursive function declarations reject with
`TS_MUTUAL_RECURSION_UNSUPPORTED`; a function *expression* may still be named
and call itself by that name, and self-recursive declarations are unaffected.

The canonical classic `for` lowering rejects a `continue` that crosses a
`finally`, because the current loop epilogue would otherwise run before the
`finally`. `for...of` snapshots arrays and strings before iteration; until a
resumable iterator protocol exists, a loop body that mutates, aliases, or
passes the iterable itself rejects with `TS_FOR_OF_UNSUPPORTED`. Calls that do
not touch the iterable are unaffected.

## Conformance

`cargo test -p lash-typescript` runs an official, commit-pinned Test262 subset
through the same parse -> normalized AST -> shared AST -> heap VM pipeline as a
real cell. This proves spec agreement for the selected accepted constructs; it
does not claim that the bounded dialect accepts all of ECMAScript. The Node
differential oracle independently pins agreement with the deployed Node
version, while Test262 pins agreement with ECMA-262.

The inventory/census pair is the exhaustive policy index: every upstream
feature tag and top-level directory is accepted, rejected by a real `TS_*`
code, or skipped by an explicit ticket/deviation ruling. The path-level skip
register accounts for every non-passing upstream test. Counts are pinned by
area, and executable skips are negative ratchets: a new failure, a changed
rejection, or an unexpectedly compiling skip fails CI. Tests use no network or
wall clock. See [`tests/test262/README.md`](tests/test262/README.md) for the
pinned commit, harness shims, and deliberate inventory-first sync procedure.

## Deviation register

These are the only deliberate deviations from an otherwise accepted
ECMA-262 operation. They are runtime-system constraints rather than alternate
language semantics:

- Instruction, wall-clock, logical-memory, and call-frame limits may terminate
  execution with the existing typed VM bound errors.
- The value model is dense records with no prototype chain, so `__proto__`,
  `__defineGetter__`, `__defineSetter__`, `__lookupGetter__`, and
  `__lookupSetter__` all reject as `TS_PROTOTYPE_MUTATION_UNSUPPORTED` — as a
  member name, as a quoted property, and as an object-literal key. A computed
  key that only resolves to one of these names at the access rejects at runtime
  under the same code, because the two alternatives are both silent: a read
  would answer `undefined` where Node answers the prototype, and a write would
  store a data key that nothing ever reads through. One over-rejection follows
  from having only the runtime name: `{ [key]: v }` with a computed
  `"__proto__"` is an ordinary data property in Node, and refuses here.
- A `map` callback runs inside the VM and cannot perform effects. `console.log`,
  a tool call, or any other effect inside one terminates with the typed
  `EffectInBuiltinCallback` error. The callback is ordinary synchronous code:
  an `await` inside it is a parse-level rejection, so there is no suspension
  point inside `map` to make durable.
- A function-valued `replace`/`replaceAll` replacement uses that same durable,
  synchronous callback driver. VM preemption and continuation restore are safe
  between its instructions, but an effect inside the callback terminates with
  `EffectInBuiltinCallback` just as it does for `map`.
- A RegExp pattern is capped at **4,096 UTF-16 code units** and **32 nested
  groups**, with `TS_REGEX_PATTERN_TOO_LONG` and
  `TS_REGEX_PATTERN_NESTING_LIMIT` repairs. Matching is capped at **1,000,000
  deterministic matcher steps** per operation; exhaustion is the uncatchable
  `RegExpBudgetExceeded` execution-bound error. The pinned `regress` 0.11.1
  engine is patched locally to charge bytecode dispatch and backtrack
  transitions; [upstream issue placeholder](https://github.com/ridiculousfish/regress/issues/PLACEHOLDER).
- RegExp flags `d` and `v` reject as `TS_REGEX_INDICES_FLAG_UNSUPPORTED` and
  `TS_REGEX_UNICODE_SETS_FLAG_UNSUPPORTED`: match indices and Unicode-set syntax
  are not in the accepted surface. Use `match.index` plus capture lengths
  instead of `d`, and `u` plus ordinary Unicode character classes instead of
  `v`. The `g` and `y` state machines are Lash-owned because the backing engine
  does not implement JavaScript `lastIndex` semantics.
- The runtime value model cannot represent a lone UTF-16 surrogate. A non-`u`
  RegExp match that would produce one therefore fails closed as
  `TS_REGEX_LONE_SURROGATE_MATCH_UNSUPPORTED`; add `u` or avoid matching half
  of an astral character.
- `matchAll` is accepted only in a direct iterable sink (`for...of`, spread, or
  `Array.from`) and otherwise rejects as `TS_REGEX_ITERATOR_POSITION` with a
  spread repair. This keeps the iterator from becoming durable state. The
  shared sink lowers the operation as one bounded materialization, so a later
  `break` in `for...of` does not make matching lazy; matcher fuel is charged for
  the complete direct-sink operation.
- A single JavaScript string result is capped at **8 MiB**. Multiplicative
  growth paths such as `repeat` and replacement-token expansion preflight the
  result before allocation; exceeding the cap terminates as the uncatchable
  `MemoryLimitExceeded` resource exhaustion error. Regex inputs, match objects,
  split plans, and replacement plans likewise check or pre-charge every
  guest-sized native allocation before reserving host memory.
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
  deferred; the front-end does not silently copy a cycle. `JSON.stringify`
  therefore detects an existing cycle before invoking a function replacer: a
  replacer cannot erase that cycle first, unlike Node. This is an explicit v1
  deviation until the durable graph encoding can represent cycles.
- Captures are by value. A closure may read a `let` (including a classic-for
  iteration value), but assigning to a captured lexical binding still rejects
  with `TS_MUTABLE_CAPTURE_UNSUPPORTED` until durable lexical cells exist.
- The host boundary is JSON-shaped: object properties whose value is
  `undefined` are omitted and array elements become `null`; incoming JSON
  cannot manufacture `undefined`.
- Lone UTF-16 surrogates are not representable in the v1 UTF-8 value model, so
  literals reject with `TS_LONE_SURROGATE_LITERAL_UNSUPPORTED`, except that a
  direct `encodeURI`/`encodeURIComponent` literal is preserved long enough to
  throw Node's real `URIError`. Indexing an
  astral string at one UTF-16 unit, `Object.values`/`Object.entries` when their
  string receiver would produce those units, and the two empty-separator
  expansions — `split('')` and `replaceAll('', …)`, both of which ECMA defines
  per UTF-16 code unit — reject at runtime with `TS_LONE_SURROGATE_UNSUPPORTED`
  on an astral receiver. BMP receivers are unaffected. Other string methods
  that could manufacture a lone surrogate are absent from the shipped
  surface.
- Appending at exactly `array.length` is supported. An assignment that skips an
  index would create holes the v1 dense-list representation cannot distinguish
  from explicit `undefined`, so it rejects as `TS_SPARSE_ARRAY_UNSUPPORTED`.
  Negative and other non-index writes would create named object properties and
  reject as `TS_ARRAY_NON_INDEX_PROPERTY_UNSUPPORTED`; neither path mutates an
  element.
- Deleting an object field preserves aliases and returns the ECMA boolean.
  Deleting a present dense-array index would create a hole, so it rejects at
  runtime with `TS_DELETE_ARRAY_INDEX_UNSUPPORTED` and directs the author to
  `splice(index, 1)`.
- Async array callbacks run sequentially in v1
  (`TS_ASYNC_MAP_SEQUENTIAL_V1`): result order matches Node, while callback
  interleaving and shared-mutation order can differ.
- Direct `globalThis.name` reads and writes, including replacement from inside
  a function, plus nested-path mutation, membership, and deletion share the
  same durable session slots as top-level bindings. Nested-function replacement
  uses the root-global set intrinsic and returns the assigned value.
- The five accepted `console` methods are host-defined rather than ECMA-262,
  share one print-observation channel, and print ECMA
  `ToString` of each argument. Node's inspector formatting is not reproduced:
  `console.log({a: 1})` prints `[object Object]` where Node prints `{ a: 1 }`.
- Multi-argument Date construction and ISO date-times without an explicit
  offset are interpreted as UTC, never the host timezone. `Date.parse` and
  string construction accept only ECMA date-time syntax; a structurally valid
  but invalid date produces `NaN`, while non-ISO fallback syntax rejects as
  `TS_DATE_PARSE_NON_ISO` with an ISO rewrite.
- Durable Date values are immutable. `setUTC*` methods reject as
  `TS_DATE_IMMUTABLE` and direct the author to
  `new Date(d.getTime() + n)`. Local-time getters reject with the corresponding
  `getUTC*` replacement; locale and local string methods direct the author to
  `toISOString()`.
- Date numeric coercion is supported, including subtraction and relational
  comparison. String coercion—directly or through an array/Error-message join—
  rejects as `TS_DATE_STRING_COERCION_PENDING` and directs the author to
  `.toISOString()`; the VM never substitutes a host-local date string.
- Map, Set, and URLSearchParams `forEach` all use live durable cursors: entries
  appended during a callback are visited, while entries deleted before their
  turn are skipped. Deleting and reinserting a Map key or Set value schedules
  it at the tail; URLSearchParams retains its WHATWG list-index behavior.
- A block-scoped binding whose name shadows one already in scope is lowered to
  a generated slot, so that the inner binding cannot overwrite the outer one.
  At root that slot is a runtime global, which makes it the one place a
  `__typescript_` name appears in persisted session state. It is dead by any
  turn boundary and the dialect filters the reserved prefix out of the
  bound-variables prompt, so it is never shown; a block binding that shadows
  nothing keeps the name its author wrote.
- URL parsing is backed by exactly `url` 2.5.8. Unicode IDNA hosts are accepted
  only where that parser matches the pinned Node/WPT oracle, including ordinary
  Unicode-to-punycode conversion. Four known backing-version gaps fail closed
  instead of returning an approximate URL: `file:` and non-special schemes
  (`TS_URL_SCHEME_UNSUPPORTED`, rewrite with `http(s)`); raw malformed `xn--`
  A-label cases (`TS_URL_IDNA_BACKING_DIVERGENCE`, use Unicode or a complete
  valid A-label); current caret encoding
  (`TS_URL_PERCENT_ENCODING_BACKING_DIVERGENCE`, pre-encode `^` as `%5E`); and
  the newest special relative triple-slash rows
  (`TS_URL_RELATIVE_SLASH_BACKING_DIVERGENCE`, provide an absolute URL). One
  port-setter edge containing only ASCII tabs/newlines rejects as
  `TS_URL_SETTER_BACKING_DIVERGENCE`; provide decimal digits or the empty
  string. Invalid relative/absolute input rejects as `TS_URL_PARSE_ERROR` and
  directs the author to add an absolute URL or a valid base.

No other semantic deviation is intentionally accepted for an operation in the
surface below.

## Syntax, iteration, and Node traps

SWC parses modern TypeScript syntax, including ASI, comments, trailing commas,
Unicode escapes, numeric separators, and hexadecimal/octal/binary literals.
Annotations, interfaces, type aliases, generics, `as`, `satisfies`, and non-null
assertions are erased. Enums lower to their `tsc` runtime object or const-enum
literals; decorators and namespaces are parsed but reject as
`TS_DECORATOR_UNSUPPORTED` and `TS_NAMESPACE_UNSUPPORTED`. `"use strict"` is an accepted no-op; functions see
`this` as `undefined`, top-level `this` rejects, and `arguments` rejects with a
rest-parameter replacement.

Iterator-returning `.entries()`, `.keys()`, and `.values()` calls are accepted
only when consumed directly by `for...of`, spread, `Array.from`, `new Map`/`Set`,
or `Object.fromEntries`; bind `[...expr]` when the values must outlive that sink.
Property enumeration has one order everywhere: integer-like keys first, then
other strings in insertion order.

The dialect intentionally reproduces these frequently surprising Node results:
`arr[-1]` is `undefined`; `typeof null` is `"object"`; `Object.keys(new Map())`
and `{...new Map()}` are empty; object string coercion is `"[object Object]"`;
string `.length` counts UTF-16 units while `for...of` walks code points.
Numbers use the ECMA binary64 (`f64`) model. One pinned `ryu-js` conversion
provides shortest-round-trip decimal text for template interpolation,
`String(number)`, `join`, and JSON; those string forms print negative zero as
`0`, while numeric operations still preserve its sign. `%` has JavaScript
remainder semantics for negative operands, `**` is right-associative and agrees
with `Math.pow`, `Math.min()` is `Infinity`, and `Math.round(-0.5)` is `-0`.

Regular-expression literals and `new RegExp(pattern?, flags?)` accept `g`, `i`,
`m`, `s`, `u`, and `y`; constructor arguments must be strings or `undefined`,
with an explicit-string repair for other values. RegExp objects are durable
mutable heap values: pattern, flags, and `lastIndex` persist across suspension,
while the compiled matcher is a rebuildable cache and is never serialized.
Node-shaped exec/match results use an unforgeable durable `RegExpMatch` heap
kind. This is a fail-closed wire cutover: bytecode format 9, snapshot format 6,
VM continuation format 7, RLM snapshot envelope 12, and Lashlang segment
handover 3. Deployments must drain or recreate parked processes created by
older formats. The accepted surface is `source`, `flags`, `global`,
`ignoreCase`, `multiline`, `sticky`, `unicode`, and writable `lastIndex`;
`exec`, `test`, `toString`, and `valueOf`; plus string `match`, `search`,
`matchAll`, `replace`, `replaceAll`, and `split`. Exec match values have Node's
array shape, capture slots, `index`, `input`, and named `groups`. Replacement
strings implement `$$`, `$&`, ``$` ``, `$'`, `$1` through `$99`, and
`$<name>`.

## Standard-library inventory

The v1 inventory contains 144 owner-qualified method names: 59 static methods
and 85 instance method names. The signature table is also the source of the
model prompt; optional arguments are explicit rather than hidden behind an
"ECMA optional arguments" qualifier.

`instance_method_inventory_matches_the_lowerer` pins the list below against
`is_instance_stdlib_method`, so the register cannot drift from what the lowerer
actually accepts.

The shipped static families are:

- Object: `keys(value)`, `values(value)`, `entries(value)`,
  `fromEntries(iterable)`, `assign(target, ...sources)`,
  `groupBy(iterable, callback)`, `hasOwn(value, key)`, `is(left, right)`.
- Array: `from(source[, mapFn[, thisArg]])`, `isArray(value)`, `of(...values)`.
- String: `fromCharCode(...codeUnits)`, `fromCodePoint(...codePoints)`.
- Map: `groupBy(iterable, callback)`.
- Date: `parse(value)`,
  `UTC(year[, month[, date[, hours[, minutes[, seconds[, milliseconds]]]]]])`.
- Number: `isFinite(value)`, `isInteger(value)`, `isNaN(value)`,
  `isSafeInteger(value)`, `parseFloat(value)`, `parseInt(value[, radix])`.
- JSON: `parse(text)`, `stringify(value[, replacer[, space]])`.
- Math: `abs`, `acos`, `asin`, `acosh`, `asinh`, `atan`, `atan2`, `atanh`,
  `cbrt`, `ceil`, `clz32`, `cos`, `cosh`, `exp`, `expm1`, `floor`, `fround`,
  `hypot`, `imul`, `log`, `log1p`, `log10`, `log2`, `round`, `sin`, `sinh`,
  `tan`, `tanh`, `trunc`, `max`, `min`, `pow`, `sqrt`, and `sign`, with their
  ordinary ECMA arities. `PI`, `E`, `LN2`, `LN10`, `LOG2E`, `LOG10E`,
  `SQRT2`, and `SQRT1_2` are accepted constants.
- URL: `canParse(input[, base])`.

The shipped instance names are `at`, `concat`, `charAt`, `charCodeAt`,
`codePointAt`, `append`, `add`, `clear`, `delete`, `entries`, `exec`, `endsWith`, `filter`, `fill`,
`find`, `findIndex`, `findLast`, `findLastIndex`, `flat`, `flatMap`, `forEach`,
`get`, `getAll`, `has`, `includes`, `indexOf`, `join`, `lastIndexOf`, `map`, `match`, `matchAll`,
`every`, `padEnd`, `padStart`, `repeat`, `replace`, `replaceAll`, `reduce`,
`reduceRight`, `reverse`, `slice`, `sort`, `some`, `splice`, `split`, `search`,
`startsWith`, `substring`, `toExponential`, `toFixed`, `toPrecision`,
`toReversed`, `toSorted`, `toSpliced`, `set`, `keys`, `toLowerCase`,
`toUpperCase`, `toString`, `trim`, `trimEnd`, `trimStart`, `test`, `valueOf`, `values`,
`with`, `hasOwnProperty`, `union`, `intersection`, `difference`,
`symmetricDifference`, `isSubsetOf`, `isSupersetOf`, `isDisjointFrom`,
`toJSON`, `getTime`, `getUTCFullYear`, `getUTCMonth`, `getUTCDate`,
`getUTCDay`, `getUTCHours`, `getUTCMinutes`, `getUTCSeconds`,
`getUTCMilliseconds`, and `toISOString`. The signature table in
`src/signatures.rs` gives every optional form.

`Number.EPSILON`, `MIN_SAFE_INTEGER`, `MAX_SAFE_INTEGER`, and `MAX_VALUE` are
accepted constants. Array callbacks run synchronously and sequentially inside
the durable VM callback driver. `sort` is stable, mutates and returns its
receiver; `toSorted`, `toReversed`, `toSpliced`, and `with` return fresh arrays.
The array representation is dense: `arr.length = 0` is accepted, while writes
that would create holes reject as `TS_SPARSE_ARRAY_UNSUPPORTED` instead of
silently changing callback semantics.

`localeCompare`, `toLocaleString`, and `Intl` remain rejected because locale
data is host-dependent. Rewrite comparisons as
`a < b ? -1 : a > b ? 1 : 0`; format numbers with `toFixed(digits)`.
`String.normalize` also remains rejected because the pinned VM has no Unicode
normalization database; normalize in a deterministic host tool. JSON parse
revivers remain rejected: parse first and walk the result explicitly. Missing
methods reject with `TS_METHOD_UNSUPPORTED`.

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

The Node differential table carries 527 rows, of which 454 are distinct
expressions: duplicates are retained deliberately so each review lane's
provenance count stays executable, and the table's effective corner coverage is
that of the 448 unique rows rather than of 521 distinct behaviours. Both counts
are pinned against the table by `committed_row_counts_match_the_register`, and
the generator pins each lane's own row count, so neither this paragraph nor a
lane can drift from the corpus in silence.

The curated test262-derived slice and its selection rule live under
`tests/test262/`.
