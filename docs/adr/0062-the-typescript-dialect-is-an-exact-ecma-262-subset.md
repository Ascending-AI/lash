# The TypeScript dialect is an exact ECMA-262 subset

## Status

Accepted.

Amended 2026-09-13 (FIG-2990): [ADR 0095](0095-processes-are-values-and-process-controls-are-tools.md) removes `defineProcess`, `start`,
`wake`, `registerTrigger` and the `signals` block from this dialect. Durable
work is a `Process` value, process controls are leaf tools, and an inline
async arrow in argument position lifts when the expected catalogue type says
`Process`, so static extractability of a top-level definition object is
replaced by an engine-resolvable definition reference. The aggregate rule in
"Promise aggregates settle on journaled order" loses its process/tool phase
split: one batch, one recorded settlement order. `waitSignal`, `sleep` and
cell-only `finish` are unchanged, as is the `return`/`throw`/`finally`
contract for a process body.

Amended 2026-09-13 (FIG-3016): [ADR 0096](0096-typescript-is-the-sole-rlm-dialect.md) makes this the only RLM
dialect; there is no second surface to be at parity with. Every reference to
"Lashlang" below names the IR and VM that this dialect lowers into, never a
second authored language.

Amended 2026-09-21 (FIG-3392): two **host lifetime contracts** are recorded —
opener-close cancellation of an unfinished tool call, and an await that nothing
can resolve — with a Node oracle that keeps the host alive after an async
function returns. Neither is a deviation-register entry. See
["Two host lifetime contracts"](#two-host-lifetime-contracts-fig-3392) below.
Decided, not yet implemented; the full contract is
[ADR 0099](0099-tool-children-of-effect-groups-are-live-closing-settled.md).

Amended 2026-09-23 (FIG-3599): conformance reaches beyond one script. A
**Node session oracle** runs multi-cell sessions against the pinned Node, a
**print → reparse → admit round-trip law** and **structural artifact
invariants** run over every corpus, and the corpus rule extends to all three.
The cell-to-Script mapping, register entries 17–21 and retired entry 14 are
recorded below, in ["Beyond one script"](#beyond-one-script-fig-3599).

Amended 2026-09-24 (FIG-3604): register entry 5's read path is implemented —
until then only captured *writes* were refused, and a closure reading a `let`
reassigned after it was created answered a stale value — and every register
entry that promises a refusal now carries an executable probe that must fire
it, checked against the text of this register. See entry 5 and "Conformance
evidence" below.

Amended 2026-09-24 (FIG-3700, decision 42): **function receivers are exact.**
The dialect already admitted object-literal methods and `this` inside function
bodies, and lowered that `this` to `undefined`, a silent divergence in accepted
code. A non-arrow function's `this` is now its call's receiver, as ECMA-262's
OrdinaryCallBindThis gives it in strict code: the object a member call reads
the callee from (`o.f()`, `o[k]()`, optional chains, parenthesized members,
spread arguments), a builtin callback's `thisArg`, the holder of a JSON
replacer or `toJSON` call, and `undefined` for a plain call. An arrow's `this`
is lexical: it is its enclosing function's. Top-level `this` still rejects
(`TS_THIS_UNSUPPORTED`), and so does `this` in an arrow outside every function,
which reads the same top-level value. A member call on a name that is not a
built-in prototype method calls the receiver's own property, and an own
property wins over a built-in method of the same name. The IR carries the
receiver as data — `FunctionExpr.receiver`, `Expr::MethodCall` and
`Expr::ThisCall` — and the VM binds it into an ordinary frame slot, only in a
function that reads it, so roots, collection, suspension and snapshots carry it
unchanged. The bytecode, VM ABI, VM continuation and semantic-hash versions move
together; a continuation from before receivers is refused by its format
version.

Amended 2026-09-24 (FIG-3706, decision 43): classic `for` is ECMA-262's
ForStatement in every form. The head may be `let`, `const`, `var`, an
expression or empty; the condition and the update may be any expression or
absent. A head `let` is copied per iteration exactly as
CreatePerIterationEnvironment copies it, so a closure keeps its iteration's
binding. The "non-canonical classic `for` forms" rejection class below is
overruled: `TS_FOR_UNSUPPORTED` now names only register entry 23.

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
captures, blocks, `if`, `while`, classic `for` in every head, condition and
update form (amended by FIG-3706), `for...of`, `break`, `continue`, `try`/`catch`/`finally`, `throw`,
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
destructuring, `for...in`, non-canonical classic `for` forms (overruled
by FIG-3706: every form is accepted), modules and imports, JSX, enums, namespaces, decorators, `eval` and `Function`,
prototype access, accessors, object methods, regular expressions, BigInt,
spread, optional chaining, `switch`, `do`/`while`, labels, `this`, `super`,
`new`, `delete`, `in`, `instanceof`, exponentiation, bitwise operators, sequence
expressions, tagged templates, computed properties, array-literal elisions
(`[0, , 2]`), parameter defaults and rest parameters, and the compound
assignment operators (`x += 1` and `a[0] += 5` alike). Identifiers beginning with `__typescript_` are reserved for the
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
until durable lexical cells exist. A capture is mutable when an assignment to
the binding can run after the closure copied it; a binding nothing assigns
after that point is captured exactly, however it was declared. Immutable
captures and mutation *through* a captured object reference are supported.

### The agent surface

**Cells are scripts.** A foreground cell is ordinary top-level TypeScript, and
may use top-level `await` for tools, process handles, `sleep`, `Promise.all`
and `Promise.allSettled`. Tool calls must be awaited directly or consumed as pending handles before the execution ends ([ADR 0087](0087-typescript-runtime-promise-arrays.md)); the pending-tool runtime error (`TS_PENDING_TOOL`) is `Catchable`, so a `try/catch` in the cell observes it. Tool calls use explicit
`typescript.tool` module paths; their rendered signatures return `Promise<T>`,
and unknown module paths enter the executor's deferred tool-resolution path.

**Amendment (FIG-2999, 2026-09-15): `defineProcess`, `start`, `wake` and
`registerTrigger` are deleted.** A process is an ordinary uncalled `async` arrow
bound at top level or passed to a tool whose slot expects one, its signals are
inferred from the `waitSignal` calls in its body, and starting, signalling,
yielding and registering a trigger are leaf tools the catalogue declares
([ADR 0095](0095-processes-are-values-and-process-controls-are-tools.md)). The
rest of this section records the design those forms had, and no longer describes
the dialect.

**Durable work was a static definition object**, in exactly the shape
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

**Amendment (FIG-2986, 2026-09-13): the fired trigger event is a parameter, not
a global.** `registerTrigger` used to bind it through `trigger.event`, an
identifier bound nowhere in the program — the one place the dialect stopped
being TypeScript, and the place a model's natural guess (`source.event`, on the
descriptor it just built) failed with a diagnostic that only said what was
forbidden. It is now the parameter of an `inputs` arrow,
`inputs: (event) => ({ tick: event })`, on `triggers.register`, `update` and
`revive` as well. The arrow is a *template erased at lowering*, not a callback:
exactly one plain identifier parameter, synchronous, an object-expression body
with static unique keys, and the parameter admitted only as a whole, direct
property value. It lowers to the same `$lash.trigger.event` IR marker the
record form produced, so an artifact, semantic hash, process identity, bytecode
and registration payload built from the arrow are byte-identical to the ones
built from the record it replaces; no callback exists to run at fire time, and
replay is untouched. Every other `inputs` value keeps its old contract: an
ordinary expression, evaluated in the enclosing scope when the registration
runs and frozen as a fixed input. `inputs` may be omitted when the target's
authoritative signature ([ADR 0090](0090-named-process-signatures-are-authoritative.md))
has exactly one parameter and the event type is assignable to it; a
zero-parameter target stays refused and is told to take an event parameter,
because no `inputs` record could make it valid. The retired spelling, `.event`
on a source descriptor, and an object-valued `inputs` are three named
diagnostics rather than a binding error.

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
as `message`; a catchable runtime fault with no ECMA-262 counterpart is the
same value branded `RuntimeError`. A fault in an operation ECMA-262 specifies
to throw is not branded: it is that operation's own error, a `TypeError` for
reading a member of `null` or calling a non-function and each built-in's own
`TypeError`, `RangeError` or `SyntaxError`, with Node's message and no
`cause` (FIG-3653). One mapping on the error variant decides it, and the VM's
error routing throws the error object in the fault's place, so a `catch`, an
uncaught exception and `instanceof TypeError` all see what Node shows. The
typed payload rides on `cause`, the one ECMA-documented slot
an error carries for exactly this. Branded runtime faults expose `code` and `details`;
tool failures additionally expose their stable `class`, `source`, and full
`retry` disposition while the tool's message remains the Error's `message`.
The brand is what a
JavaScript library would write as `class EffectError extends Error`: the value
model has no prototype to subclass and no own slot to write `name` into, so
`name` is a property of the error object itself and `instanceof` answers `Error`
and nothing narrower. Only the substrate mints those two brands —
`new EffectError(...)` is not in the dialect. This was FIG-1477: the delivered
record failed `instanceof Error`, stringified as `[object Object]`, and sent a
frontier model's standard try/catch discrimination down its fallback branch.

An `Error` is therefore also the one JavaScript exotic that crosses the host
boundary, detaching into `{ name, message, cause?, errors? }`. Its only
mutable surface is its own data properties (`message`, `cause`, `errors`; any
other name has no slot and refuses as `TS_EXOTIC_PROPERTY_UNSUPPORTED`), and it
has no internal slot the guest cannot already read, so nothing is destroyed or exposed by detaching
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

An `allSettled` rejection reason is that same `Error`. `ExecutionHostError`
still accepts message-only failures from general hosts, which retain their
generic runtime code. A tool bridge instead attaches the tool failure's stable
classification to the host error, so a leaf that is never unwrapped keeps the
same discriminable code, source, and retry disposition without requiring every
unrelated host to fabricate them.

Without reference semantics Lashlang has no way to construct a JavaScript error
object, so its `catch` clause keeps the flat record it has always been handed.
For tool failures that record adds direct `class`, `source`, and `retry` fields
to `{ name, message, code, details }`. (The heap
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

As revised by [ADR 0087](0087-typescript-runtime-promise-arrays.md) and
replaced in part by [ADR 0095](0095-processes-are-values-and-process-controls-are-tools.md),
`Promise.all` and `Promise.allSettled` evaluate arbitrary array-valued
expressions at runtime. Tool-handle elements are awaited and settled values
pass through; mixed arrays and arrays stored in bindings are accepted. Direct
async maps retain the existing callback driver. Non-array values fail with a
typed runtime error.

ADR 0087's two-phase tool-then-process rule is **gone** (FIG-2996, landed
2026-09-14). A mixed aggregate is one resource-operation batch settling on one
recorded order: a parked `processes.await` leaf takes its place in that order at
the moment its completion arrives, so `Promise.all([tools.x.op(),
processes.await(h)])` reports whichever failure the batch recorded first, and a
tool rejection has no precedence over a process rejection. A **raw** process
handle at an element position is refused, with a repair naming
`processes.await(handle)`; a handle carried inside a value bound to a name is
passed through untouched.

`Promise.all` rejects with the reason of the leaf that settled **first**, as
ECMA specifies, and `allSettled` keeps results in input order. Since FIG-3397
the aggregate is a durable effect group whose settlement order is durable
rank, and the host answers the VM with the one settlement that decided it — or
with every result — under the aggregate's consumer mode (ADR 0099 §10). The
answer is not re-derived at replay: the group's ranks are durable facts. A host
answer that does not fit the consumer mode that asked for it fails closed with a
typed error rather than being repaired — a repair that produces a plausible
answer is indistinguishable downstream from a real one, which is exactly how an
earlier defect read as delivered in three places while being false in one.

The consumer mode is recorded per aggregate at lowering, where the compiler
already knows the dialect, rather than read from the VM's reference-semantics
flag at run time. That flag answers a heap-ownership question, and one predicate
answering two questions is the defect shape that cost an earlier layer three
rounds.

`Promise.race` and `Promise.any` join them (FIG-3397). `race` resolves with the
first settlement and `any` with the first fulfilment, or rejects with an
`AggregateError` whose `errors` hold one rejection per input position, in input
order. An unawaited `sleep(ms)` is a pending timer — one handle like a pending
tool call — so `await Promise.race([call, sleep(ms)])` is the dialect's timeout,
resolving `undefined` when the timer wins. A plain value in the operand array
answers ahead of any dispatched settlement, and every pending operand is
admitted first.

### Two host lifetime contracts (FIG-3392)

**Implemented by FIG-3397.** `Promise.race` and `Promise.any` are accepted. The
full contract is
[ADR 0099](0099-tool-children-of-effect-groups-are-live-closing-settled.md);
[ADR 0065](0065-concurrent-settlement-is-a-durable-group-at-the-effect-host-seam.md)
carries the matching group-side amendment.

A **host lifetime contract** is not a deviation. The deviation register below
covers operations whose *meaning* departs from ECMA-262. Both rules here keep the
program's meaning exactly and describe what happens to the host that was running
it — a subject ECMA-262 does not address at all, since it has neither a `finish`,
nor a host shutdown, nor a process. Neither becomes a register entry.

#### 1. Opener close cancels an unfinished arm

**Selection never cancels.** While the opener lives, `all`, `race` and `any`
leave every losing arm running, which is ECMA-262's meaning.

**At opener end an unfinished arm is cancelled.** The comparison cannot be made
against the specification, and it must not be made against Node *process death*
either — a process that exits kills its pending work, which would flatter lash by
hiding the difference. **The Node oracle therefore keeps the host alive after the
function returns:** in a still-running Node process, returning from an async
function does not cancel a losing timer or socket, and that arm's later write can
land after the caller returned. Lash suppresses that write at opener close.

This statement concerns **cancellation only**. It claims no general Node
scheduling or lifetime equivalence — the registered sequential async-map
deviation still produces observable ordering differences while the opener is live
— and opener close fences further unprotected Lash semantic writes without
guaranteeing that external I/O already issued stops.

One ordering divergence that exists today is **removed** rather than accepted.
Every terminal leaf of a batch currently drains its declarations in *source*
order, whether or not it declared any, so an aggregate resolves in source order
and a hung source-first tool blocks every later sibling. Under ADR 0099 §5 the
order becomes the durable **final-commit** order, which is what a host does with
`Promise.all([a(), b()])`: each call's side effects happen as it settles, not in
argument order. The observable consequence is that intent realization order for
`Promise.all` moves from argument order to completion order.

**An attempt whose final result already committed is exempt.** Its declared
intents are realized before the opener settles and survive a crash in that window
(ADR 0099 §4). Cancellation stops delivery of a *result*, never a side effect
already performed — [ADR 0042](0042-tool-attempts-are-atomic.md) already makes
in-attempt effects at-least-once.

**Work that must outlive the opener is a process the program named.** A losing
`processes.await` stays admitted while the opener lives; opener close releases
the wait without cancelling the process
([ADR 0095](0095-processes-are-values-and-process-controls-are-tools.md)).

#### 2. An await that nothing can resolve ends the cell

`Promise.race([])` returns a forever-pending promise in ECMA-262, and **the
dialect keeps exactly that meaning**: there is no exception to catch, no
synthesized rejection, and no registered deviation. Because the dialect awaits
aggregates in place, zero operands open no group at all — ADR 0065 already
refuses empty groups — so the host detects an await nothing can resolve and
**fails the cell with a typed host-level unsettled-await error**:
`RuntimeErrorCode::AggregateAwaitUnsettled` (`aggregate_await_unsettled`), raised
by the VM as the uncatchable `AggregateAwaitUnsettled` terminal.

That is the analogue of Node exiting with code 13 on an unsettled top-level
await: the program's semantics are ECMA's, and the host's lifetime ends rather
than parking a durable execution forever.

The other empty aggregates need no host rule: `Promise.all([])` and
`Promise.allSettled([])` return `[]`, and `Promise.any([])` rejects with an
`AggregateError` whose `errors` is empty, all as ECMA-262 specifies.

#### Register bookkeeping

Neither contract above enters the numbered register. Register entry 15
(aggregate rejection timing) retired with FIG-3397: every aggregate is on
first-settlement wake, so a rejected `Promise.all` answers as soon as its first
rejection is consumed — as ADR 0065's consequences anticipated.

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

**Every refusal the register promises is executable.** An entry below that
says it rejects, refuses or fails closed carries at least one probe — a source
that must be refused with the named diagnostic, or fail at run time with the
named error — and every `TS_*` code the entry names is fired by one of them;
the crate README's register is held to the same rule for the codes it names.
The check (`crates/lash-typescript/tests/deviation_register.rs`) reads this
register's own text, so a new refusing entry without a probe fails it, as does
a probe whose refusal stops firing. Entry 5 is why: it promised a read-path
refusal no code produced, and nothing connected the promise to the code.

**Test262** carries the specification's own cases. Every test at a pinned
test262 commit whose census rows (directory, flags, feature tags) are all
accepted is vendored and run through the real lower → link → compile → heap VM
path, with the upstream harness rendered in-dialect (FIG-3646). Each selected
test has exactly one ratcheted outcome: pass, refused by a named `TS_*` code a
census row backs, fail owned by a ticket, or a named harness capability the
dialect lacks. The selection is derived rather than curated, so the pass rate
it reports is the dialect's, not a sample's. The complementary rule matters
more: **a test262 case is never admitted by weakening the dialect.** A test
whose constructs fall outside the accepted set is a named refusal until the
construct is implemented exactly.

### Beyond one script (FIG-3599)

The two mechanisms above check one script, run once. Four FIG-3571 defects
passed them for that reason — block and generated bindings leaking into the
next cell, printer faults, a lifted literal declared twice, a draft carrying
an artifact's identity — so three more mechanisms join them. All three run in
the cacheable Bazel test partition, with no network.

**The Node session oracle** (`crates/lash-typescript/tests/differential/sessions/`)
holds sessions: ordered cells. Its reference answer is the pinned Node v25.2.1
running each cell as a successive classic Script in one realm
(`vm.createContext` plus `vm.Script`), so top-level `let`/`const`/`class` live
in the realm's global lexical environment and `var` and functions on its global
object, as ECMA-262's GlobalDeclarationInstantiation specifies. Regeneration is
the same deliberate, byte-identical step as the expression table's. The lash
side runs each session through the production RLM executor twice: with one
live state, and restarting through the durable snapshot path between every
pair of cells. The mapping from a cell to its Script is stated here once:

- A cell's observation is its printed lines, how it ended, and one
  binding-visibility probe per binder name of its session, each run after the
  cell as its own Script: `console.log(typeof NAME, JSON.stringify(NAME))`.
- `console.*` is the host printer of register entry 13. `finish(value)` ends
  the cell with that value; a cell never catches it. An uncaught error is
  observed by its class; a VM fault's class is its `RuntimeError` brand
  (entry 20). A Script's completion value is not observed, since a cell
  surfaces none.
- A `ReferenceError` from a probe answers `unbound` (`tdz` when the binding is
  uninitialized); the dialect's static `TS_UNKNOWN_BINDING` is its exact
  counterpart. A cell the dialect rejects statically never enters the realm.
- Top-level `await` has no classic-Script meaning (ECMA-262 admits it only in
  a Module, whose declarations are not global), so the session corpus holds no
  await cell.
- A divergence is never a special case: a cell names its register entry and
  states the lash answer, which must differ from Node's and which ends its
  session; a session-wide probe rule (entry 17) names its entry too. A defect
  the oracle found but the change could not fix is a *defect* cell naming the
  crate README's open-defect list, ratcheted the same way.

**The round-trip law** holds every program of every corpus — the expression
table, the Test262 selection, every session cell and the workflow-graph goldens —
to: lower, admit, project, print through the lens, reparse and admit again,
reaching the same `module_ref` and `source_identity`. A program the printer
cannot spell is refused with a typed `TypeScriptSourceError`, and every
refusal is a row of an explicit allowlist with its reason
(`tests/corpus_laws/round_trip_refusals.tsv`); a row whose program now
round-trips fails until it is deleted.

**The artifact invariants** hold every admitted artifact of every corpus to:
unique declarations, each lifted literal declared once; every compiled
execution site in the trace map with the same kind, owner path and branch
memberships, and nothing else in the map; only session-visible bindings
exported, no generated slot among them; each `ProcessOrigin` derived from the
literal at its site; no identity on a draft, and the admitted identity and
structure on a host's view; and a stored artifact that reloads, alone, to the
same module.

The corpus rule extends to all three: every fixed session, round-trip or
invariant case lands in its corpus, and each harness fails when a row goes red
or an allowlist entry, deviation answer or defect answer becomes unnecessary.

### Divergences nobody wrote down (FIG-3608)

A corpus checks the divergences someone thought of. Two more mechanisms look
for the rest.

**Generated differential sessions.** A seeded generator draws multi-cell
sessions from the census's accepted grammar — every construct names the
accepted census row, or the WHATWG URL surface, it draws from, and every cell
is plain JavaScript so Node runs it as written — and each runs on the pinned
Node under the session mapping above and on lash, live and reloading between
every pair of cells. A bounded set of seeds, with Node's answers checked in
and regenerated by the same deliberate byte-identical step, runs in the
cacheable partition; the test regenerates each session from its seed, so a
generator change is a corpus change. Longer runs draw fresh seeds against the
pinned Node live. Every divergence found is minimized into a session corpus
row: a small defect is fixed with its row; a larger one becomes an open
defect with its row and a ticket, and the generator stops drawing its shape,
naming the entry, until it is fixed.

**The snapshot round-trip law.** For every value type the dialect accepts, a
value created in one cell, stored in a session global, reloaded from the
durable snapshot and used in the next behaves exactly as the same code in one
cell, where it is never stored — and that single-cell answer is Node's. The
rows cover every heap object kind (an exhaustive match names them, so a new
kind cannot enter the heap without a row) and the primitives. A type that
cannot round-trip is refused with a named diagnostic, never silently
degraded; a row the law fails today is pinned by the open defect or registered
deviation that breaks it and fails once that is fixed.

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
   durable lexical cells exist, as `TS_MUTABLE_CAPTURE_UNSUPPORTED`. A closure
   copies what it captures when it is created, so the read path refuses a
   capture that an assignment can reach afterwards: later in the same frame, in
   a later iteration of a loop the binding outlives, or through a
   `globalThis.name` write. The write path refuses an assignment to a captured
   binding from inside the closure. A `const`, a `let` assigned only before the
   closure exists, and a per-iteration loop binding are captured exactly.
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
    An elision in an array literal — a hole anywhere, including a trailing one
    as in `[1, , ]`, where a single trailing comma is not an elision — creates
    the same hole, so it rejects statically as `TS_SPARSE_ARRAY_UNSUPPORTED`
    rather than silently storing `undefined` (FIG-3702). Holes are
    indistinguishable from explicit `undefined` in the v1 dense
    representation, which is why they are refused rather than approximated.
13. **`console.log` is host-defined**, not ECMA-262. *(Superseded on the
    coercion point by FIG-2767: this ruling originally said the arguments are
    printed as their ECMA `ToString`, so `console.log({a: 1})` printed
    `[object Object]`. Rendering the value is the whole purpose of the call —
    it is the observation the model reads back — and a host-defined method is
    free to render it.)* It joins its arguments with a space and renders each
    one for the observation: plain objects and arrays as the compact JSON the
    host's print projector already produces, so `console.log({a: 1})` prints
    `{"a":1}`, and every other value as its ECMA `ToString`, which is the
    informative answer for numbers, booleans, `null`, `undefined`, dates,
    regexps and errors. The observation is bounded by the same byte and depth
    limits as any other string this dialect builds. Node's inspector formatting
    is still not reproduced.

    String coercion elsewhere is *not* ECMA-262's answer for the three values
    whose only string is a type tag. `"" + {a: 1}`, `` `${{a: 1}}` `` and
    `String({a: 1})` all lower to `+`, and each of them refuses as
    `TS_OBJECT_STRING_COERCION` for a plain object, a `Map` or a `Set`
    (FIG-3166); the refusal names the value and points at `console.log` or
    `JSON.stringify(value)`. Refusal, not an automatic JSON body, for three
    reasons. Exactness: this dialect's promise is that an accepted program means
    what ECMA-262 says it means, and quietly answering `{"a":1}` where ECMA says
    `[object Object]` would be a silent divergence in the one direction a cell
    cannot detect. Gaps are refusals: every other place this dialect cannot
    honour its own promise — sparse arrays, `Date` coercion, cyclic values,
    prototype mutation — stops with a stable `TS_*` code rather than guessing,
    and this is the same kind of gap. And the habit: `[object Object]` reaching
    an observation is almost always a cell finishing a whole tool result it
    never examined, so the refusal is the signal that sends the model back to
    read the value instead of shipping a placeholder for it. Everything with a
    string of its own is untouched — arrays, `Error`, `RegExp`, `URL`, numbers,
    booleans, `null` and `undefined` keep their exact ECMA-262 strings; so do
    property-key coercion (`obj[{a: 1}]` still reads the `"[object Object]"`
    slot), `map.toString()`, `Number({})`, loose equality, and the `console.log`
    rendering above.
14. **Shadowing residual** — *retired by FIG-3571.* A block binding that
    shadows a name in scope still lowers to a generated slot, but a generated
    slot is private: the VM neither imports nor exports it, so none reaches
    session state. The session oracle pins that no generated slot is a session
    global (FIG-3599). The number stays reserved.
15. **Aggregate rejection timing** — *retired by FIG-3397.* A rejected
    `Promise.all` used to wait for every leaf to settle before it reported. It
    now answers at its first consumed rejection, and the leaves still in
    flight run on as losers under their opener (ADR 0099 §0, §10). The number
    stays reserved so later entries keep their names.
16. **`for...of` snapshots** — *retired by FIG-3625.* `for...of` used to walk
    a snapshot and refuse, by the iterable's name, a body that might mutate
    it. That check was unsound both ways: it refused a shadowing binding that
    never touched the iterable, and it missed an alias made before the loop,
    whose writes the snapshot then hid. The loop now follows its iterable live,
    as ECMA-262's iterators do: an array or a `URLSearchParams` is read at the
    iterator's index on every step, and a `Map` or a `Set` visits entries added
    during the loop and skips ones deleted before their turn. The number stays
    reserved. The classic-loop `continue` refusal it also carried is entry 23.

17. **Closure boundary** (`closure-boundary`). A binding whose value reaches a
    function does not survive its cell
    ([ADR 0076](0076-lashlang-durable-stores-hold-exclusively-owned-copies.md)):
    a function's index means something only inside the program that compiled
    it. Where Node still holds the function, a later cell's reference to the
    name is refused as `TS_FUNCTION_NOT_PERSISTED`: the session keeps the
    names it dropped, across a durable reload too, until one is bound again,
    so the value never degrades to an unknown or undefined name (FIG-3608).
18. **Cross-cell redeclaration** (`cross-cell-redeclaration`). A cell's
    top-level declaration may rebind a name an earlier cell declared, where
    GlobalDeclarationInstantiation throws a `SyntaxError`; the dialect follows
    the REPL rule a console session expects.
19. **One session namespace** (`global-object-aliases-lexical-bindings`).
    `globalThis.name` reads and writes the session slot of a top-level
    `let`/`const`, which ECMA-262 keeps apart from the global object.
20. **Runtime fault brand** (`runtime-fault-brand`). A fault the VM raises
    with no ECMA-262 counterpart (a host-boundary, tool or process-control
    failure) is an `Error` branded `RuntimeError`. A fault in an operation
    ECMA-262 specifies to throw is that operation's own class, as in Node
    (FIG-3653); see "Errors" above.
21. **Process literal as a value** (`process-literal-is-a-process-value`). A
    top-level `const`-bound uncalled `async` arrow is a `Process` value
    ([ADR 0095](0095-processes-are-values-and-process-controls-are-tools.md)),
    so `typeof` answers `"object"`.
22. **Closed-shape field guard** (`closed-shape-field-guard`, FIG-3626). A
    read or write of a field that a statically closed object literal lacks is
    refused at link with `TS_LINK_ERROR`, naming the field and the literal's
    fields, where Node answers `undefined` or adds the field. `tsc` refuses the
    same program, so the guard is TypeScript-faithful, and it catches the typo
    a model writes into code it cannot step through. A literal is closed only
    while nothing can have given it a field the linker cannot see: a spread, a
    computed key, a computed-key write (`o[k] = v`), or an escape (its
    reference reaching anything but a field read: a call argument, another
    binding, a container, a return value, `globalThis`) opens it for the whole
    cell, and an open object reads a missing field as JavaScript does. A shape
    a host schema declares closed (`additionalProperties: false`) is guarded
    the same way.
23. **Classic-loop `continue` across `finally`.** A `continue` in a classic
    `for` loop with an update expression that crosses a `finally` rejects with
    `TS_FOR_UNSUPPORTED` rather than running the update before the `finally`
    body, which is the order the lowering would otherwise produce. A loop
    with no update has nothing to run before the `finally`, so its `continue`
    is accepted.

## Consequences

- Model output that uses a construct outside the surface fails at parse or link
  with a named code, early and visibly, rather than running with an
  approximated meaning.
- The register is a maintained artifact with mechanical guards: the
  standard-library pin asserts equality in both directions, so growing the
  lowerer without documenting the growth fails the build, and every refusal
  the register promises has a probe that must fire it.
- Two conformance mechanisms must both stay green, and they fail differently —
  the Node oracle catches real-engine divergence in accepted operations, the
  Test262 selection catches specification divergence the oracle's corpus never
  thought to express.
- The SWC pin is an exact-version dependency. Upgrading it re-opens the
  parse-stack measurement and the AST-classification coverage test, both of
  which are written to fail rather than drift when SWC gains a node kind.
- Host operators inherit a deployment requirement: more than 2 GiB of address
  space must be available for a cap-sized cell, or large cells fail closed with
  a resource diagnostic.
- Cycle-capable durable graph encoding remains owed work; several rejections
  above stand in for it.
