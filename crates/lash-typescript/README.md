# Lash TypeScript dialect

`lash-typescript` is the source front-end for the durable Lash heap VM, and the
only one: TypeScript is the sole RLM authoring language, and `lashlang` names
the dialect-neutral IR and VM it lowers into (ADR 0096). SWC is
confined to `src/adapter/`; the adapter produces a Lash-owned normalized tree,
which lowers into `lashlang::Program`. Runtime type annotations are erased.
There is no dialect choice to make: no language selector, no session pin, and
no second surface to be at parity with.

## Dialect contract

The accepted v1 surface is deliberately bounded, but includes ordinary model-authored
constructs: `let`/`const`/`var` (including multiple declarations), functions and
async helpers, blocks, `if`, `while`, `do...while`, the canonical
`for (let i = start; i < end; i++)` form, `for...of`, `for...in`, `switch`,
`break`, `continue`, `try`/`catch`/`finally`, `throw`, `return`, destructuring in
every binding and assignment position, defaults/rest, optional chains, array/call/object
spread, compound/logical assignment, update operators, arrays, records, and calls.
A spread argument passes the array's items as the call's arguments, to a
function the program defines and to a builtin alike (`Math.max(...xs)`,
`items.push(...more)`, `console.log(...parts)`). A builtin whose lowering
depends on how many arguments it takes (a callback method such as `map`, a
coercion, an agent primitive) refuses a spread argument by name
(`TS_METHOD_UNSUPPORTED`).
Arithmetic includes exponentiation and ECMA `ToInt32`/`ToUint32` bitwise and shift
operators. `in` is an own-property query because dialect objects have no prototypes;
`instanceof` accepts the Error family, Map, Set, Date, RegExp, URL,
URLSearchParams, Array, and Object.
`console.log`, `console.warn`, `console.error`, `console.info`, and
`console.debug` accept any arity and emit through the existing print-observation
channel, joining their arguments with one space; each argument is rendered for
the observation — plain objects and arrays as compact JSON, every other value as
its ECMA `ToString` — and lexical bindings named `console` take precedence.
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
`sleep`, `Promise.all`, `Promise.allSettled`, `Promise.race`, and `Promise.any`;
`waitSignal` is
process-only and rejects at the cell top level by name. Async functions and arrows
are accepted when every awaited value is transitively grounded in this agent surface;
`await Promise.all(xs.map(async x => ...))` and its `Promise.allSettled`
counterpart use the durable sequential async-map driver. The all-settled form
wraps each callback in guest `try`/`catch`, so a rejection becomes that input's
`{status: "rejected", reason}` record and later callbacks still run. Promise
chaining and synthetic promises remain named rejects.
Tool calls require `await` and use explicit `typescript.tool` module paths;
their prompt signatures return `Promise<T>`. Unknown module paths participate
in the executor's deferred tool-resolution path.

Durable work is an ordinary value: a process is a top-level `const`-bound
uncalled `async` arrow — `const worker = async (...) => { ... }` — that the
linker lifts wherever a `Process` is expected. Starting, awaiting, signalling
and cancelling one are catalogue tools rather than language constructs:
`await processes.start({ definition: worker, args: { ...args } })` returns a handle,
`await handle` its result, `await processes.emit({ value })` emits progress
from a run and `await processes.signal({ handle, name, payload })` sends a
declared signal to another run. `waitSignal`, `sleep` and `finish` remain
constructs; `finish` is cell-only. A normal return from the arrow finishes the
process only after all enclosing `finally` blocks execute; an uncaught throw
fails it. A capture the lift cannot carry by value rejects as a non-liftable
capture.

A trigger registration binds the fired event through the `inputs` arrow:
`inputs: (event) => ({ tick: event })`, on `triggers.register` / `update` /
`revive` alike. The arrow is a template the
compiler erases, not a callback: exactly one plain parameter, no `async`, an
object-expression body with static unique keys, and the parameter usable only
as a whole, direct property value — never projected, nested, called or
captured. Every other value is an ordinary expression evaluated once, in the
enclosing scope, when the registration runs. `inputs` may be omitted when the
target's signature has exactly one parameter and the event type is assignable
to it; a zero-parameter target is refused and told to take an event parameter.
Writing `.event` on a source descriptor, the retired `trigger.event` global, or
an object-valued `inputs` each reject by name with
`TS_TRIGGER_SOURCE_EVENT_ACCESS`, `TS_TRIGGER_EVENT_REMOVED` and
`TS_TRIGGER_INPUTS_LITERAL_REQUIRED`.

Every `Promise` aggregate evaluates any array-valued expression and aggregates
its pending handles and already-settled values as one durable effect group.
Unawaited tool calls create handles, and so does an unawaited `sleep(ms)` — a
pending timer whose start point is the aggregate that admits it; abandoning
either at cell end is a typed runtime error. Non-array values and awaiting a settled value also
fail loudly. A mixed aggregate is **one** batch on **one** recorded settlement
order: a `processes.await` leaf parks on a durable wait and takes its place in
that order when its completion arrives, so a tool rejection has no precedence
over a process rejection and there is no tool-then-process phase split. A raw
process handle at an element position is refused, with a repair naming
`processes.await(handle)`; a handle carried inside a value bound to a name is
passed through untouched. As ECMA specifies, `Promise.all` rejects with the
reason of the leaf that settled first, `Promise.allSettled` keeps its results in
input order, `Promise.race` resolves with the first settlement and `Promise.any`
with the first fulfilment — or rejects with an `AggregateError` whose `errors`
hold one rejection per input position, in input order. The group's settlement
order is durable, so replay selects the same answer rather than re-deriving one.

An aggregate answers as soon as its first deciding settlement is consumed; the
leaves that lost keep running, as losing promises do, while the cell's turn or
process lives. When that turn or process ends, an unfinished loser is cancelled
and a loser whose result already committed still realizes its declared effects
before the end is final (ADR 0099). `Promise.race([])` never settles, so the host
ends the cell with the typed `aggregate_await_unsettled` error rather than
parking it; `Promise.any([])` rejects with an empty `AggregateError`.

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
`finally` with `TS_FOR_UNSUPPORTED`, because the current loop epilogue would
otherwise run before the `finally`. `for...of` follows its iterable live, as
ECMA-262 does (FIG-3625): an array or a `URLSearchParams` is read at the
iterator's index on every step, so a body that appends to, removes from or
rewrites the iterable (through any name, a function, or a pattern default)
changes what the loop visits next; a `Map` or a `Set` visits entries added
during the loop and skips ones deleted before their turn. A string iterates its
code points.

## Conformance

Test262 runs every upstream test the census accepts at a pinned commit (18,970
of 53,578) through the same lower → link → compile → heap VM path as a real
cell. Each has one ratcheted outcome:

- 4,062 pass;
- 14,124 are refused by a named `TS_*` code;
- 763 fail, each owned by a ticket;
- 21 wait on a harness capability.

That is a pass rate of 21.4% of the selection and 84.2% of the tests that run.
`//crates/lash-typescript:test262__test` checks a stratified 515-test sample in
the developer loop. `//crates/lash-typescript:test262_full__test` runs the whole
selection in the workspace partition and nightly. This proves spec agreement
for what the dialect accepts. It does not claim that the bounded dialect
accepts all of ECMAScript. The Node differential oracle independently pins
agreement with the deployed Node version, while Test262 pins agreement with
ECMA-262.

The inventory/census pair is the exhaustive policy index. Every upstream
feature tag, top-level directory and flag is one of:

- accepted;
- rejected by a real `TS_*` code;
- skipped by an explicit ticket or deviation ruling.

The same holds for each tagless dialect decision. A rejected row also carries a
**probe**, a source that must reject with exactly the diagnostic the row names,
or an explicit `probe-exempt:` reason. Naming a diagnostic is a claim about the
code, and until the probes existed the claim and the code were connected by
nothing. Every refusal code the selection shows must be named by a rejected
row. Tests use no network or wall clock. See
[`tests/test262/README.md`](tests/test262/README.md) for the selection rule,
the outcome classes, the ratchet, the harness renderings and the
inventory-first sync procedure.

Sequences of cells are checked against Node too (FIG-3599, FIG-3608): a
hand-written session corpus, sessions a seeded generator draws from the
census's accepted grammar, and a snapshot round-trip law over every value
type the dialect accepts, each run live and reloading through the durable
snapshot between cells. See
[`tests/differential/sessions/README.md`](tests/differential/sessions/README.md).

The token-sequence fuzzer in `tests/grammar_coverage.rs` (shared with
`tests/no_abort_guarantee.rs`) is not differential and stays separate: it
feeds the parser sources mostly outside the accepted grammar and asserts only
that parsing never aborts, in a child process on the 2 MiB stack contract. A
differential check needs accepted programs with a meaning to compare, so the
session generator draws only accepted grammar and cannot stand in for it, and
it cannot stand in for the session generator.

## Deviation register

These are the only deliberate deviations from an otherwise accepted
ECMA-262 operation. They are runtime-system constraints rather than alternate
language semantics. Every refusal an item below names is proven by an
executable probe in `tests/deviation_register.rs`, which holds ADR 0062's
numbered register to the same rule: an entry that promises a refusal and has
no probe that fires it fails that test.

- Instruction, wall-clock, logical-memory, and call-frame limits may terminate
  execution with the existing typed VM bound errors.
- The closed-shape field guard (`closed-shape-field-guard`, FIG-3626): a read
  or write of a field that a statically closed object literal lacks is refused
  at link as `TS_LINK_ERROR`, naming the field and the literal's fields
  (``object has no field `c`; its fields are `a`, `b` ``), where Node answers
  `undefined`. TypeScript's checker refuses the same program. A literal stays
  closed only while nothing can give it a field the linker cannot see: a
  spread (`{ ...base, b: 2 }`), a computed key (`{ [k]: 1 }`), a computed-key
  write (`o[k] = v`), or an escape (the object passed to a function or a
  builtin, bound to another name, stored in an array or object, returned, or
  reached through `globalThis`) opens it for the whole cell, and an open
  object reads a missing field as `undefined`. `console.log`, `Object.keys`
  and `Array.isArray` read their argument without opening it.
- Two forms `tsc --strict` rejects are refused rather than implemented (ADR
  0064, FIG-3651). A function declaration that shares its var scope's name
  with another function, a `var` or a parameter refuses as
  `TS_FUNCTION_REDECLARATION_UNSUPPORTED` (TS2393, TS2300), where Node lets
  the last declaration win. `delete` of an operand that is not a property
  reference, such as `delete 1` or `delete f()`, refuses as
  `TS_DELETE_NON_REFERENCE_UNSUPPORTED` (TS2703), where Node evaluates the
  operand and answers `true`. `delete` of a bare identifier stays the early
  `SyntaxError` strict code makes it.
- Await permission stops at every function boundary: an async IIFE or async
  `map` callback must await its own tool calls, `sleep`, `waitSignal`, and
  `triggers.register` operations.
- Durable state is a value *tree*. A cycle is usable inside a cell —
  `JSON.stringify` throws Node's catchable
  `TypeError: Converting circular structure to JSON` — but a durable binding
  that still holds one when the cell ends cannot be written down, and refuses
  at the boundary as `TS_CYCLIC_VALUE_UNSUPPORTED`. Hold a key or index
  instead of the parent object.
- The value model is dense records with no prototype chain, so `__proto__`,
  `__defineGetter__`, `__defineSetter__`, `__lookupGetter__`, and
  `__lookupSetter__` all reject as `TS_PROTOTYPE_MUTATION_UNSUPPORTED` — as a
  member name, as a quoted property, and as an object-literal key. A computed
  key that only resolves to one of these names at the access rejects at runtime
  under the same code, because the two alternatives are both silent: a read
  would answer `undefined` where Node answers the prototype, and a write would
  store a data key that nothing ever reads through. The same names are refused
  where a value *enters* — `JSON.parse`, a host or tool result, and a decoded
  snapshot — so no value in the runtime ever carries one. Two over-rejections
  follow, both registered: `{ [key]: v }` with a computed `"__proto__"`, and
  `JSON.parse('{"__proto__":1}')`, are ordinary data properties in Node and
  refuse here. The alternative was worse than the divergence: a parsed
  `__proto__` key used to land as an enumerable property that `Object.keys`
  listed, every read refused, and `JSON.stringify` then failed on — state with
  no way out, reachable from ordinary untrusted-JSON round-tripping.
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
  `RegExpBudgetExceeded` execution-bound error. The published `lash-regress`
  workspace crate is a fork of `regress` 0.11.1 that charges bytecode dispatch
  and backtrack transitions. The instrumentation has not been filed upstream;
  the fork is released in lockstep with Lash.
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
- `lastIndex` applies ECMA `ToLength` at the **write**, not at the read. ECMA
  makes it an ordinary writable data property and coerces on use, so Node reads
  back exactly what was stored: `r.lastIndex = -1` reads `-1`, and `Infinity`
  reads `Infinity`. Here the same writes read back `0` and `2 ** 53 - 1`. The
  coerced value is what every accepted operation would have used anyway, so the
  divergence is confined to reading the property straight back; storing the
  clamp is what keeps `lastIndex` a durable integer rather than an arbitrary
  double the snapshot has to carry.
- `matchAll` is accepted only in a direct iterable sink and otherwise rejects as
  `TS_REGEX_ITERATOR_POSITION` with a spread repair. This keeps the iterator from becoming durable state. The
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
- Captures are by value: a closure copies each binding it captures when it is
  created, which is exact only while nothing assigns that binding afterwards.
  Until durable lexical cells exist, a capture that an assignment can reach
  after the closure is created rejects with `TS_MUTABLE_CAPTURE_UNSUPPORTED`,
  on the read path as well as the write path: an assignment later in the same
  frame, one in a later iteration of a loop the binding outlives, a
  `globalThis.name` write, or an assignment from inside the closure itself. A
  `const`, a `let` assigned only before the closure exists, and a binding
  declared inside the loop body or head (including a classic-for iteration
  value, whose `i++` writes the next iteration's copy) are captured exactly.
  A closure never outlives its cell (`closure-boundary`), so only its own
  cell can reassign what it captured.
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
  interleaving and shared-mutation order can differ. The census records it as
  `registered-deviation:TS_ASYNC_MAP_SEQUENTIAL_V1` on the
  `typescript async-array-callbacks` row, so the deviation is indexed where
  every other ruling is indexed rather than living only in this list. That row
  indexes the deviation; the callback semantics themselves are pinned by the
  async-driver tests, not by the census.
- `globalThis.name` addresses the durable session slot `name` from anywhere in
  a cell. A read at the top level, in a function or in a nested closure reads
  the slot's current value live through the root-global read intrinsic: never
  a copy a closure holds, and never a parameter or local of the same name. A
  global nothing has written reads `undefined`. Writes (replacement from inside
  a function uses the root-global set intrinsic and returns the assigned
  value), nested-path mutation, membership and deletion address the same slot.
  A top-level block binding is not a global, so it never answers
  `globalThis.name`, and a global that only a function's write creates exists
  once that write runs. A process body runs apart from the session and sees
  only the values it started with, so every `globalThis` form inside one
  refuses with `TS_NON_LIFTABLE_CAPTURE`.
- The five accepted `console` methods are host-defined rather than ECMA-262 and
  share one print-observation channel, joining their arguments with a single
  space. Each argument is rendered for the observation rather than string-
  coerced: plain objects and arrays print as compact JSON, matching the host's
  own print projector, so `console.log({a: 1})` prints `{"a":1}`. Every other
  value keeps ECMA `ToString`, which is already the useful text for numbers,
  booleans, `null`, `undefined`, dates, regexps and errors — a `Map` or `Set`
  prints as `[object Map]`/`[object Set]` because it has no JSON body. Node's
  inspector formatting is still not reproduced.
- String coercion of a plain object, a `Map` or a `Set` refuses as
  `TS_OBJECT_STRING_COERCION` instead of producing a type tag: `"" + {a: 1}`,
  `` `${{a: 1}}` `` and `String({a: 1})` all lower to `+` and all three refuse,
  pointing at `console.log` or `JSON.stringify(value)`. Every value with a
  string of its own keeps its exact ECMA text, and property keys,
  `map.toString()`, `Number({})`, loose equality and `console.log` are
  untouched.
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
  comparison. String coercion—`d.toString()`, `d.toUTCString()`, `String(d)`,
  `d + ''`, template interpolation, or through an array/Error-message join—
  produces the deterministic UTC-pinned ECMA DateString, such as
  `Thu Jan 01 1970 00:00:00 GMT+0000 (Coordinated Universal Time)`; the VM
  never substitutes a host-local date string.
- Map, Set, and URLSearchParams `forEach` all use live durable cursors: entries
  appended during a callback are visited, while entries deleted before their
  turn are skipped. Deleting and reinserting a Map key or Set value schedules
  it at the tail; URLSearchParams retains its WHATWG list-index behavior.
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

The session deviations below are where a *sequence* of cells departs from
successive classic Scripts in one realm, the reference the Node session oracle
runs (`tests/differential/sessions/`). Each is named by the slug the session
corpus cites:

- `closure-boundary`: a binding whose value reaches a function — a function
  declaration, an arrow, an array or object holding one — does not survive
  its cell ([ADR 0076](../../docs/adr/0076-lashlang-durable-stores-hold-exclusively-owned-copies.md)):
  a function's index is only meaningful inside the program that compiled it,
  where Node still holds the function. The session remembers the name, live
  and across a reload, so a later cell that reads it (by name, with `typeof`,
  or as `globalThis.name` it does not write) is refused as
  `TS_FUNCTION_NOT_PERSISTED` rather than degraded to an unknown or undefined
  name, until something binds the name again. A closure used within its own
  cell, capturing earlier cells' globals, is exact.
- `cross-cell-redeclaration`: a cell's top-level declaration may rebind a name
  an earlier cell declared, whatever either declaration's kind. ECMA-262's
  GlobalDeclarationInstantiation throws a `SyntaxError` for `let`/`const` over
  any earlier binding and for `var` over an earlier lexical one; the dialect
  follows the REPL rule instead (V8's REPL mode, which a model's prior of a
  console session expects), because a cell re-running `const result = ...` is
  the ordinary shape of iterating on a session.
- `global-object-aliases-lexical-bindings`: the session has one namespace.
  `globalThis.name` reads and writes the session slot of a top-level `let` or
  `const` of that name, where ECMA-262 keeps a global lexical binding apart
  from a global object property, so `globalThis.x = 2` beside `let x = 1`
  leaves `x` reading `1` in Node and `2` here. `var` and function declarations
  alias the global object in both.
- `runtime-fault-brand`: a fault the VM raises — reading a member of `null`,
  calling a non-function — is an `Error` branded `RuntimeError`, with its
  typed code on `cause` ([ADR 0062](../../docs/adr/0062-the-typescript-dialect-is-an-exact-ecma-262-subset.md)),
  not the ECMA class (`TypeError`) Node throws. `instanceof Error` holds;
  `instanceof TypeError` does not. An error the program or a builtin throws
  keeps its own class.
- `process-literal-is-a-process-value`: a top-level `const`-bound uncalled
  `async` arrow is a `Process` value
  ([ADR 0095](../../docs/adr/0095-processes-are-values-and-process-controls-are-tools.md)):
  `typeof` answers `"object"`, not `"function"`, and the value is the
  process's durable reference, which survives its cell.

No other semantic deviation is intentionally accepted for an operation in the
surface below.

## Open conformance defects

The Node oracles found these divergences (FIG-3599, and the generated
sessions and snapshot round-trip law of FIG-3608). They are defects, not
rulings: each corpus row or round-trip law row that shows one states the
current wrong answer, and fails once the defect is fixed, until it is promoted
to an ordinary row. The session generator draws none of their shapes until
then, each exclusion naming its entry here. They are listed so no divergence
is silent while its fix is owed.

None is open: FIG-3625, FIG-3626, FIG-3627 and FIG-3631 fixed the last four.

## Syntax, iteration, and Node traps

SWC parses modern TypeScript syntax, including ASI, comments, trailing commas,
Unicode escapes, numeric separators, and hexadecimal/octal/binary literals.
Annotations, interfaces, type aliases, generics, `as`, `satisfies`, and non-null
assertions are erased. Enums lower to their `tsc` runtime object or const-enum
literals; decorators and namespaces are parsed but reject as
`TS_DECORATOR_UNSUPPORTED` and `TS_NAMESPACE_UNSUPPORTED`. `"use strict"` is an accepted no-op; functions see
`this` as `undefined`, top-level `this` rejects, and `arguments` rejects with a
rest-parameter replacement.

Iterator-returning `.entries()`, `.keys()`, and `.values()` calls, and
`matchAll`, are accepted only when consumed directly by `for...of`, spread,
`Array.from`, `new Map`/`Set`, or `Object.fromEntries`; bind `[...expr]` when the
values must outlive that sink. There is one sink list, not two: every position on
it is a bounded materialization, which is the whole property the restriction
exists to guarantee, so `matchAll` accepting three of the five was an asymmetry
with nothing behind it.
Property enumeration has one order everywhere: integer-like keys first, then
other strings in insertion order.

The dialect intentionally reproduces these frequently surprising Node results:
`arr[-1]` is `undefined`; `typeof null` is `"object"`; `Object.keys(new Map())`
and `{...new Map()}` are empty; string `.length` counts UTF-16 units while
`for...of` walks code points.
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

The v1 inventory contains 149 owner-qualified method names: 60 static methods
and 89 instance method names. The signature table is also the source of the
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
  ordinary ECMA arities, plus `random()`. `PI`, `E`, `LN2`, `LN10`, `LOG2E`,
  `LOG10E`, `SQRT2`, and `SQRT1_2` are accepted constants.
  `Math.random()` is not a computation: it is a journaled host effect on the
  same seam as `Date.now()`, so the draw is recorded on the first execution and
  replayed exactly on every later one. A replayed turn sees the same sequence
  it saw the first time, which is what makes it admissible in a durable
  program at all.
- URL: `canParse(input[, base])`.

The shipped instance names are `at`, `concat`, `charAt`, `charCodeAt`,
`codePointAt`, `append`, `add`, `clear`, `delete`, `entries`, `exec`, `endsWith`, `filter`, `fill`,
`find`, `findIndex`, `findLast`, `findLastIndex`, `flat`, `flatMap`, `forEach`,
`get`, `getAll`, `has`, `includes`, `indexOf`, `join`, `lastIndexOf`, `map`, `match`, `matchAll`,
`every`, `padEnd`, `padStart`, `repeat`, `replace`, `replaceAll`, `reduce`,
`reduceRight`, `reverse`, `slice`, `sort`, `some`, `splice`, `push`, `pop`,
`shift`, `unshift`, `split`, `search`,
`startsWith`, `substring`, `toExponential`, `toFixed`, `toPrecision`,
`toReversed`, `toSorted`, `toSpliced`, `set`, `keys`, `toLowerCase`,
`toUpperCase`, `toString`, `trim`, `trimEnd`, `trimStart`, `test`, `valueOf`, `values`,
`with`, `hasOwnProperty`, `union`, `intersection`, `difference`,
`symmetricDifference`, `isSubsetOf`, `isSupersetOf`, `isDisjointFrom`,
`toJSON`, `getTime`, `getUTCFullYear`, `getUTCMonth`, `getUTCDate`,
`getUTCDay`, `getUTCHours`, `getUTCMinutes`, `getUTCSeconds`,
`getUTCMilliseconds`, and `toISOString`. The signature table in
`src/signatures.rs` gives every optional form.

`Number.EPSILON`, `MIN_SAFE_INTEGER`, `MAX_SAFE_INTEGER`, `MAX_VALUE`, and
`NaN` are accepted constants. Array callbacks run synchronously and sequentially inside
the durable VM callback driver. `sort` is stable, mutates and returns its
receiver; `toSorted`, `toReversed`, `toSpliced`, and `with` return fresh arrays.
The array representation is dense: `arr.length = 0` is accepted, while writes
that would create holes reject as `TS_SPARSE_ARRAY_UNSUPPORTED` instead of
silently changing callback semantics. `push`, `pop`, `shift`, and `unshift`
mutate the receiver in place with their ECMA return values — the new length for
`push`/`unshift`, the removed element or `undefined` for `pop`/`shift` — and
compose with the callback methods, so accumulating into an array inside
`forEach` is the ordinary form it is everywhere else.

`localeCompare`, `toLocaleString`, and `Intl` remain rejected because locale
data is host-dependent. Rewrite comparisons as
`a < b ? -1 : a > b ? 1 : 0`; format numbers with `toFixed(digits)`.
`String.normalize` also remains rejected because the pinned VM has no Unicode
normalization database; normalize in a deterministic host tool. Missing
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

The Node differential table carries 724 rows, of which 651 are distinct
expressions: duplicates are retained deliberately so each review lane's
provenance count stays executable, and the table's effective corner coverage is
that of those 651 unique expressions rather than of all 724 rows. Every count in
this paragraph is pinned against the table by
`committed_row_counts_match_the_register`, and the generator pins each lane's
own row count, so neither this paragraph nor a lane can drift from the corpus in
silence.

The census-derived Test262 selection, its outcome record and its selection
rule live under `tests/test262/`.
