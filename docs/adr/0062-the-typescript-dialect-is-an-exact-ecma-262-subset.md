# The TypeScript dialect is an exact ECMA-262 subset

## Status

Accepted.

## Context

Lash accepts model-authored code, and a model's prior on TypeScript is far
stronger than its prior on any bespoke language. That is the reason for the
dialect, and it is also the trap. A language the model believes it already knows
punishes approximation much harder than one it has to read the prompt for: if a
construct looks like TypeScript and runs like something else, nothing in the
loop notices. The model has no signal that it guessed wrong, the author reading
the diff has none either, and the divergence surfaces as wrong output in
production rather than as an error.

The substrate is not the constraint. ADR 0060 settled that the VM is
reference-semantic and that a dialect is a lowering, and it supplies what this
dialect needs: heap objects with deterministic identity, stackless frames and
closures, exceptions with a three-layer catchability taxonomy, and durable error
origins. ADR 0061 settled that this dialect is permanent, at parity, pinned per
session. What remains is a contract question — which slice of ECMA-262 is
implemented, what happens at the edge of that slice, and what an agent writes.

## Decision

### Fidelity: exact, or rejected by name

Every construct the dialect accepts behaves exactly as ECMA-262 specifies.
Everything else is rejected with a stable `TS_*` diagnostic. Nothing is accepted
with a nearby meaning.

The asymmetry is deliberate: a rejection is cheap and visible, while a near-miss
is a silent defect, so the dialect refuses where it cannot be exact. Rejection is
static wherever the shape can be seen at parse or lowering time, and the
deviation register below names every shape-dependent runtime rejection that
remains. The register is small and closed — outside it, no semantic deviation is
intentionally accepted for an operation in the accepted surface.

The accepted v1 surface is `let`/`const`, functions and arrows with immutable
captures, blocks, `if`, `while`, the canonical `for (let i = start; i < end; i++)`
form, `for...of`, `break`, `continue`, `try`/`catch`/`finally`, `throw`,
`return`, arrays, records, field and index access and assignment, calls, the
primitive unary, arithmetic, comparison, equality and logical operators,
conditionals, templates, `.length`, a fixed standard-library inventory, and free
`console.log`. TypeScript type annotations, aliases and interfaces are erased
after parsing.

The executable inventories are the source of truth, not this ADR:
`crates/lash-typescript/README.md` carries the register and the standard-library
inventory, and `tests/rejections.rs`, `tests/structural_contract.rs` and the
checked-in Node differential suite carry the behavior. The inventory pin asserts
set equality in **both** directions against the lowerer's allowlist, so the
register cannot fall behind the code — a one-directional pin is what previously
let the register understate the surface by nine methods for a full round.

### The v1 rejection classes

Rejected with a stable code, statically: classes, generators, `var`,
destructuring, `for...in` and non-canonical classic `for` forms,
modules and imports, JSX, enums, namespaces, decorators, `eval` and `Function`,
prototype access, accessors, object methods, regular expressions, BigInt,
spread, optional chaining, `switch`, `do`/`while`, labels, `this`, `super`,
`new`, `delete`, `in`, `instanceof`, exponentiation, bitwise operators, sequence
expressions, tagged templates, computed properties, parameter defaults and rest
parameters, and the compound assignment operators (`x += 1` and `a[0] += 5`
alike). Identifiers beginning with `__typescript_` are reserved for the
lowerer's generated bindings.

Three rejections are dialect-specific enough to state their reasons here.

**General async functions.** The one authored async function is the `run` field
of a static `defineProcess` definition; cells otherwise use top-level `await`.
Async authoring beyond that would require suspension points the durable
machinery does not yet place.

**Mutually recursive function declarations** reject, naming the cycle
(`cycle: isEven -> isOdd -> isEven`). v1 captures by value, so a declaration
cycle has no emission order, and routing it through a shared mutable record
would build a heap cycle reachable from a durable root — which the durable graph
encoding cannot hold. The program would run and then fail to suspend or
snapshot, so failing closed at compile time is the honest form of the same
deferral. Self-recursion, named self-recursive function *expressions*, nested
declarations and acyclic chains are unaffected.

**Mutable lexical captures** reject, on both captured reads and captured writes,
until durable lexical cells exist. Immutable captures and mutation *through* a
captured object reference are supported.

### The agent surface

**Cells are scripts.** A foreground cell is ordinary top-level TypeScript, and
may use top-level `await` for tools, process handles, `sleep`, `Promise.all`
and `Promise.allSettled`. Tool calls require `await` and use explicit
`typescript.tool` module paths; their rendered signatures return `Promise<T>`,
and unknown module paths enter the executor's deferred tool-resolution path.

**Durable work is a static definition object**, in exactly the shape
`const worker = defineProcess({ name: "worker", signals: {}, run: async (...) => { ... } })`,
declared at top level. `start`, `registerTrigger`, `wake`, `waitSignal`, `sleep`
and cell-only `finish` lower to the shared process and effect machinery;
`wake(value)` emits progress from a run, and `wake(handle, "signal", payload)`
sends a declared signal to another run. `waitSignal` is the one primitive scoped
to a process body — outside one it is refused, while `sleep` is equally valid in
a cell — and the refusal names the keyword in the dialect the author actually
wrote, rather than leaking the Lashlang spelling of the same primitive into a
TypeScript program. The keys of `start`'s second argument are the `run`
function's own parameter names rather than a fixed input field, and
`registerTrigger`'s inputs work the same way.

Static extractability is the point of the shape, not a stylistic preference: the
host registers a process definition from the artifact without executing it, so
the name, the declared signals and the `run` literal must be readable from the
source. Dynamic definitions, dynamic process targets, non-literal config, and
definitions below top level each reject with their own `TS_PROCESS_*`
diagnostic rather than being resolved at run time.

**`return` finishes, `throw` fails, and cleanups run.** A `return` from `run` is
a real function return: every enclosing `finally` block executes, and only then
does the generated wrapper finish the process with the returned value. An
uncaught `throw` fails it. This discharges the constraint FIG-1303 recorded —
that a TypeScript `return` must never lower to `Expr::Finish`, which is a
process terminal that deliberately skips pending `finally` blocks. `finish`
inside `run` is statically rejected for the same reason, so authored process
code has no way to bypass a cleanup.

### Errors

The dialect adopts the substrate's three-layer catchability taxonomy unchanged.
Tool and effect failures throw real `Error` objects; ordinary runtime errors are
catchable per specification; instruction, deadline, memory and frame-depth
exhaustion are uncatchable terminals; and host cancellation is uncatchable in
v1. Catchability is a single exhaustive match on the error variant, so a new
variant does not compile until it declares its class.

**A delivered rejection is an `Error`, not a record shaped like one.** A caught
tool or effect failure satisfies `error instanceof Error`, renders as
`EffectError: <host text>` under `String(error)`, and carries the host's own text
as `message`; a catchable runtime fault is the same value branded
`RuntimeError`. The typed payload — `code` and `details` — rides on `cause`, the
one ECMA-documented slot an error carries for exactly this. The brand is what a
JavaScript library would write as `class EffectError extends Error`: the value
model has no prototype to subclass and no own slot to write `name` into, so
`name` is a property of the error object itself and `instanceof` answers `Error`
and nothing narrower. Only the substrate mints those two brands —
`new EffectError(...)` is not in the dialect. This was FIG-1477: the delivered
record failed `instanceof Error`, stringified as `[object Object]`, and sent a
frontier model's standard try/catch discrimination down its fallback branch.

An `Error` is therefore also the one JavaScript exotic that crosses the host
boundary, detaching into `{ name, message, cause?, errors? }`. It has no live
mutation surface (assigning to an error is a `TypeError`) and no internal slot
the guest cannot already read, so nothing is destroyed or exposed by detaching
it, and a caught rejection is returnable whenever its `cause` is data — which is
how a cell reports a tool failure. A `cause` holding another exotic (a `Map`, a
`Date`) still refuses at the child export. `Map`, `Set`, `Date`, `RegExp`, `URL`
and `URLSearchParams` refuse outright.

The detachment is the host boundary's own operation, closer to `structuredClone`
than to anything the guest can write: inside a cell an error's properties are not
own data, so `Object.keys(error)` is `[]`, `JSON.stringify(error)` is `{}`, and
`{ ...error }` copies nothing — exactly as in ECMA. The conversion is also
one-way. Only a host handing back the identical exported value hits the boundary
cache and resolves to the same error object; a host that rebuilds the record —
anything that round-trips through JSON — hands back a plain record, and the guest
sees `instanceof Error` false and an ordinary mutable object. Nothing re-brands a
record as an error.

An `allSettled` rejection reason is that same `Error`, and that is where a
contract limit surfaces. `ExecutionHostError` carries a message and nothing
else, so the finest identity available for a leaf that is never unwrapped is the
host's own text; the reason reports the generic `ResourceOperationFailed` rather
than claiming an unwrap error's code, which would be simply wrong for a leaf
nothing unwrapped. Giving rejections a discriminable code requires a code
channel on the effect-host contract that every host would have to populate. That
is named here as an accepted v1 limit rather than faked at the dialect.

Lashlang is untouched: without reference semantics it has no way to construct a
JavaScript error object, so its `catch` clause keeps the flat
`{ name, message, code, details }` record it has always been handed. (The heap
itself is shared machinery, not a per-dialect one: the error-family branch that
answers an assignment to an exotic with a heap `TypeError` sits above this
choice and is reachable wherever such a receiver exists.) One seam decides which
shape is delivered — the VM's error routing — and it reads the
reference-semantics flag, the same predicate that gates every other JavaScript
heap constructor: the question being asked is "can this run allocate a
JavaScript error object at all". In production that flag is set from
`program.dialect == Typescript` and nothing else, so the two questions do
coincide; the pairing is a convention the runtime does not enforce, and this
rule is deliberately written against the heap-ownership meaning rather than
against the dialect, which is what the aggregate rule records at lowering
instead.

### Promise aggregates settle on journaled order

`Promise.all` and `Promise.allSettled` accept array literals of top-level tool
promises and already-resolved values, and reuse the shared batch machinery.
Nested aggregates, non-array iterables, and process or timer promises inside an
aggregate reject by name.

`Promise.all` rejects with the reason of the leaf that settled **first**, as
ECMA specifies, and `allSettled` keeps results in input order. Settlement order
is not re-derived at replay: the host records the order its leaves settled in as
a required field on the journaled batch result, with no serde default, so an
entry written before the field existed is refused rather than replayed as input
order. A host that reports an order that is not an ordering of its own results
fails closed with a typed error rather than being repaired — a repair that
produces a valid permutation is indistinguishable downstream from a real one,
which is exactly how this defect previously read as delivered in three places
while being false in one.

The selection rule is recorded per batch at lowering, where the compiler already
knows the dialect, rather than read from the VM's reference-semantics flag at run
time. That flag answers a heap-ownership question, and one predicate answering
two questions is the defect shape that cost an earlier layer three rounds.

### Parser: SWC, pinned, behind a lash-owned adapter

Parsing uses SWC, pinned exactly — `swc_common 25.0.0`, `swc_ecma_ast 28.0.0`,
`swc_ecma_parser 44.0.0` — and confined to `src/adapter/`, which produces a
lash-owned normalized tree that lowers into `lashlang::Program`. No SWC type
appears in a public API, a durable format, or the lowering. oxc is not rejected
on the merits; it is deferred to a bounded spike against this same seam, and the
seam is what keeps that spike bounded.

SWC parses by recursive descent and **aborts the process** on stack exhaustion
rather than returning an error, so the safety argument cannot rest on a
pre-parse guard: five review rounds showed a hand-written scan cannot be relied
on to agree with SWC about every shape, each round's guard being right about the
axis it modelled while the next abort sat just outside it. The bound is
therefore arithmetic and proportional. A nesting level can cost as little as one
source byte, the measured worst requirement is about 22,500 bytes of stack per
source byte, and the parse runs on a thread reserving 8 MiB plus 40,000 bytes
per source byte — roughly 1.8x the worst measurement — over a source that cannot
exceed the cap. The reservation is address space, not memory. A guard test keeps
the margin honest by disabling the nesting preflight entirely and running every
shape that aborted in any round through what remains.

The 28-unit source nesting budget therefore exists for the diagnostic and for
cost, not for safety: a source-level `TS_SOURCE_NESTING_LIMIT` beats a
parser-depth error, and rejecting before the parse keeps a pathological cell at
17 MB instead of 1.2 GB. The budget binds before the shared AST's own nesting
limit (ADR 0060) for every shape the grammar accepts, so the dialect's front end
lands inside the substrate's cap by construction rather than by coincidence.

### Conformance evidence: two mechanisms, deliberately different

**A Node differential oracle** is the primary gate. A checked-in expectation
table carries 345 rows — 272 of them distinct expressions — generated against
Node v25.2.1 and regenerated byte-identically, covering coercion,
UTF-16-sensitive behavior, key ordering, replacement tokens, JSON number
formatting, numeric edge cases, optional arguments, and the method inventory.
Rows are retained per review lane, so duplicates across lanes are deliberate
and the effective corner coverage is that of the distinct rows; both counts are
pinned against the table by a test, so neither the register's prose nor a lane
can drift from the corpus in silence. The standing rule is that every fixed
dialect case the oracle can express lands in the corpus: the hand-written test
is the diagnosis, the corpus row is the permanent guard.

**A curated test262 slice** carries the specification's own cases, adapted from
a pinned test262 commit, with the upstream harness replaced by a single
`finish(boolean)` and the semantic expression unchanged. Fixtures run through
the real parse → normalized AST → shared AST → heap VM path. The selection rule
is at least one positive case per accepted semantic class that does not depend
on a rejected feature. The complementary rule matters more: **a test262 case is
never admitted by weakening the dialect.** If its dependencies fall outside the
accepted set, it becomes a named rejection test or waits until the feature is
implemented exactly.

## Deviation register

These are the only deliberate departures from ECMA-262 for an operation that is
otherwise in the accepted surface. They are runtime-system constraints, not
alternate language semantics. `crates/lash-typescript/README.md` holds the
executable register; this list is the decision that the register is closed and
that each entry is a limit taken knowingly.

1. **Runtime limits.** Instruction, wall-clock, logical-memory and call-frame
   bounds may terminate execution with the substrate's typed VM bound errors.
2. **Effects in builtin callbacks.** A `map` callback runs inside the VM and
   cannot perform effects; one that tries terminates with the typed
   `EffectInBuiltinCallback`. `await` inside a callback is a parse-level
   rejection, so there is no suspension point inside `map` to make durable.
3. **String result cap.** A single string result is capped at 8 MiB.
   Multiplicative growth paths preflight the result before allocating, so
   exceeding the cap is an uncatchable `MemoryLimitExceeded` rather than a host
   allocation panic.
4. **Cycles at durable capture.** Cyclic heap objects are rejected when durable
   state is captured; shared *acyclic* object identity is preserved
   byte-for-byte. Cycle-capable durable graph encoding is deferred, and the
   front end never silently copies a cycle to avoid the question.
5. **Mutable captures.** Rejected on both the read and the write path until
   durable lexical cells exist.
6. **Mutual recursion.** Rejected, for the durable-cycle reason above.
7. **The JSON-shaped host boundary.** Object properties whose value is
   `undefined` are omitted and array elements become `null`; incoming JSON
   cannot manufacture `undefined`. `undefined` and `null` remain distinct inside
   the VM, as ECMA-262 requires — the erasure is at the boundary only, and it is
   the erasure the specification itself defines.
8. **Lone surrogates.** Not representable in the v1 UTF-8 value model. Literals
   reject statically. At runtime, on an astral receiver, four paths reject
   rather than diverge: indexing at one UTF-16 unit, string-backed
   `Object.values` and `Object.entries`, and the two empty-separator expansions
   `split('')` and `replaceAll('', …)`, both of which ECMA defines per UTF-16
   code unit where the v1 value model can only advance per code point. BMP
   receivers are unaffected, and other methods whose result is unavoidably a
   lone surrogate are absent from the surface. The register carries no silent
   divergence in this family: every case where the UTF-8 model cannot reproduce
   ECMA's code-unit answer is refused by name.
9. **Source size and address space.** A cell is capped at 64 KiB of source. The
   cap is what makes the parse-stack reservation finite; the host must be able
   to hand out more than 2 GiB of address space for a cap-sized cell, and under
   a tighter `RLIMIT_AS` or `vm.overcommit_memory=2` a large cell fails closed
   with `TS_PARSE_RESOURCES_UNAVAILABLE` — a resource diagnostic deliberately
   distinct from any diagnostic describing the program — while small cells keep
   working.
10. **Parse-time allocation is bounded by the preflight, not by arithmetic.**
    The stack argument above does not cover memory: SWC's duplicate-label check
    is quadratic, and with the preflight disabled a 64 KiB cell of one repeated
    label peaks near 37 GB. No shape reaches that on the shipping path — the
    preflight rejects them all and the worst measured peak across 164
    adversarial shapes is 17 MB — but this is a bound the preflight carries
    rather than one the arithmetic provides. Parsing in a subprocess is the
    change that would bring both axes under one limit.
11. **Nesting budget.** 28 budget units, cumulative across delimiters and
    operators, pinned on a 2 MiB stack.
12. **Dense arrays.** Appending at exactly `array.length` is supported; a write
    that would skip an index rejects as `TS_SPARSE_ARRAY_UNSUPPORTED`, and a
    negative or non-index write rejects as
    `TS_ARRAY_NON_INDEX_PROPERTY_UNSUPPORTED`. Neither path mutates an element.
    Holes are indistinguishable from explicit `undefined` in the v1 dense
    representation, which is why they are refused rather than approximated.
13. **`console.log` is host-defined**, not ECMA-262. It prints ECMA `ToString`
    of each argument joined by a space; Node's inspector formatting is not
    reproduced, so `console.log({a: 1})` prints `[object Object]`.
14. **Shadowing residual.** A block-scoped binding that shadows a name already
    in scope lowers to a generated slot so the inner binding cannot overwrite
    the outer one. At root that slot is a runtime global, which is the one place
    a `__typescript_` name appears in persisted session state. It is dead by any
    turn boundary and filtered out of the bound-variables prompt, so it is never
    shown; a binding that shadows nothing keeps the name its author wrote.
15. **Aggregate rejection timing.** A rejected `Promise.all` still waits for
    every leaf to settle before it reports. ECMA specifies which reason
    surfaces, not when — it has no wall times, and a conforming program cannot
    observe the difference except through timing — but v1 has no fail-fast
    cancellation of an in-flight batch leaf, so the aggregate settles at the
    pace of its slowest leaf while rejecting with its first-settled reason.
16. **`for...of` snapshots.** Arrays and strings are snapshotted before
    iteration, strings by code point. Until a resumable iterator protocol
    exists, a body that mutates, aliases, or passes the iterable itself rejects
    with `TS_FOR_OF_UNSUPPORTED`; calls that do not touch the iterable are
    unaffected, because the restriction is about reaching the snapshotted
    source, not about calling. A classic-loop `continue` that crosses a
    `finally` rejects rather than running the loop epilogue early.

## Consequences

- Model output that uses a construct outside the surface fails at parse or link
  with a named code, early and visibly, rather than running with an
  approximated meaning.
- The register is a maintained artifact with a mechanical guard: the
  standard-library pin asserts equality in both directions, so growing the
  lowerer without documenting the growth fails the build.
- Two conformance mechanisms must both stay green, and they fail differently —
  the Node oracle catches real-engine divergence in accepted operations, the
  test262 slice catches specification divergence the oracle's corpus never
  thought to express.
- The SWC pin is an exact-version dependency. Upgrading it re-opens the
  parse-stack measurement and the AST-classification coverage test, both of
  which are written to fail rather than drift when SWC gains a node kind.
- Host operators inherit a deployment requirement: more than 2 GiB of address
  space must be available for a cap-sized cell, or large cells fail closed with
  a resource diagnostic.
- Two limits are named as owed work rather than closed: a code channel on the
  effect-host contract, which would make aggregate rejections discriminable,
  and cycle-capable durable graph encoding, which several rejections above are
  standing in for.
