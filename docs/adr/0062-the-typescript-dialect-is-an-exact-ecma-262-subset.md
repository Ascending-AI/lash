# The TypeScript dialect is an exact ECMA-262 subset

## Status

Accepted.

## Context

Models write familiar TypeScript constructs. Accepting a construct with an
approximate meaning can produce wrong output without a diagnostic. A named
refusal makes the unsupported operation visible. The shared heap VM supplies
identity, frames, closures, exceptions and durable state; this decision defines
the source-language fidelity contract.

## Decision

### Fidelity: exact, or rejected by name

Accepted operations follow ECMA-262 except for the closed deviation register
below and the explicit host extensions. An unsupported construct receives a
stable `TS_*` diagnostic. The front end rejects statically when it can identify
the unsupported shape; shape-dependent runtime refusals have executable probes.
ADR 0064 owns the accepted language inventory and explicit gap rulings.

The dialect accepts declarations, functions and arrows, mutable lexical
captures, blocks, conditionals, loops, exceptions, arrays, records, calls,
operators and its declared standard library. Type annotations, aliases and
interfaces are erased after parsing. The crate README, lowerer allowlists,
rejection tests and Test262 census specify the detailed accepted inventory.
The standard-library test compares the documented and accepted sets in both
directions.

A non-arrow function's `this` is its strict-mode call receiver; a plain call
receives `undefined`. Arrows retain lexical `this`. Member calls use the
receiver's own property before a built-in method of the same name. Function
receivers live in frame slots and survive suspension as ordinary rooted values.
`arguments` belongs to a non-arrow function. References outside that context
receive the corresponding named diagnostic.

Mutable lexical captures share binding cells. A local assignment is visible to
every closure over that binding. Each binding instance has its own cell,
including per-iteration `let` environments. A top-level session binding stays a
session slot, read and written live by a closure. Closures and their local cells
end at the cell boundary; durable session roots preserve the current global
values. The boundary for function-valued globals is register entry 17.

Classic `for` accepts declaration, expression or empty heads and arbitrary or
absent conditions and updates. Head `let` bindings follow per-iteration
copying. The loop-epilogue/`finally` refusal is precisely register entry 23.

Coercions run an object's own `valueOf` and `toString` in hint order through the
ordinary guest call path, with the object as receiver and normal VM limits.
The requesting instruction resumes with the hook answers. A coercion hook
cannot perform an effect, following register entry 2. Function source-string
coercion receives `TS_FUNCTION_STRING_COERCION` rather than fabricated source.

### The rejection classes

The detailed rejection list lives in `tests/rejections.rs`, the diagnostic
inventory and `tests/test262/census/`. Rejections include unsupported modules,
classes, generators, dynamic code and unsupported object operations.
Identifiers beginning with `__typescript_` belong to generated lowering slots.

General async functions are outside the accepted callable-function model.
An uncalled async arrow represents durable process work, including catalogue
argument positions that expect `Process`; cells use top-level `await`.

Mutually recursive declarations receive a named cycle diagnostic. Shared cells
for those declarations would create a cycle that the durable heap cannot
capture. Self-recursion and acyclic declaration chains use the supported
function machinery. Mutable captures themselves are accepted.

### The agent surface

A foreground cell is top-level TypeScript with `await` for tool calls, process
control tools, timers and promise aggregates. Tool paths are declared under the
TypeScript tool modules and return promise handles. Unknown module paths use
the deferred tool-resolution path. ADR 0087 owns runtime promise arrays.

Durable work is a `Process` value. A top-level const-bound uncalled async arrow
or an inline arrow in a catalogue slot typed as `Process` lifts to an admitted
process body. Process controls are catalogue tools under ADR 0095.
Approvals and outside callbacks use host tools that can defer under ADR 0137;
`sleep` is available in a cell too. Tools return
declared work for runtime realization under ADR 0116.

A process-body `return` runs every enclosing `finally` before the wrapper
finishes the process with the value. An uncaught `throw` fails the process.
A process body cannot end a session turn: its catalog offers no tool that
declares a turn control (FIG-5781), so a process document that calls
`control.finish` names an effect its boundary does not offer and is refused
at admission. A function return owes its cleanups; no control call stands in
for it.

### Errors

The VM classifies errors as catchable faults, uncatchable terminals or host
cancellation. Instruction, memory and frame-depth exhaustion are terminals.
Catchability is an exhaustive match on the error variant.

Tool and effect failures throw heap `Error` objects branded `EffectError`.
Catchable faults without an ECMA counterpart use `RuntimeError`. An operation
with a specified ECMA error throws its corresponding `TypeError`, `RangeError`
or `SyntaxError`. A branded error's `message` is the host text; its `cause`
carries code and details, and tool errors also carry class, source and retry
disposition. `allSettled` rejection reasons use the same error values.

Host export detaches a supported error into data containing its name, message,
optional cause and optional aggregate errors. Unsupported exotics inside that
data still fail export. Other exotics such as `Map`, `Set`, `Date`, `RegExp`,
`URL` and `URLSearchParams` cannot detach as ordinary host data. Inside the VM,
error enumeration and JSON rendering follow their guest property contract.
A host's result always enters as a copy (ADR 0076), so an error a host hands
back is an ordinary record and does not become a heap error again.

### Promise aggregates settle on recorded order

`Promise.all`, `allSettled`, `race` and `any` consume runtime array expressions.
Tool handles are awaited, settled values pass through, and mixed arrays are
accepted. A raw process handle in an aggregate operand is refused with a repair
naming `processes.await(handle)`. Direct async maps have the separately
registered callback discipline. Non-arrays receive a typed runtime failure.

A mixed aggregate is one resource-operation batch. The logical Run records
settlement ranks. `all` rejects at the first consumed rejection; `allSettled`
returns outcomes in input order. `race` returns the first settlement; `any`
returns the first fulfilment or an `AggregateError` with reasons in input order.
Resume reads the committed decision rather than reconstructing it from a clock.
A host answer incompatible with the requested consumer mode fails closed.

Lowering records the aggregate consumer mode explicitly. The VM does not infer
that mode from the heap's stored forest/graph form. An unawaited `sleep(ms)` is
a timer handle; `await Promise.race([call, sleep(ms)])` returns `undefined` if
the timer wins. A plain operand answers ahead of dispatched settlements, after
the aggregate admits its pending operands.

### Two host lifetime contracts

ADR 0099 owns Run admission, aggregate selection and logical Closing. These rules describe the
execution host's lifetime rather than alternate ECMA promise meanings.

#### 1. Opener close cancels an unfinished arm

Selection leaves losing arms running while their opener lives. At opener end,
Lash cancels unfinished arms and fences further unprotected semantic writes.
It does not guarantee that external I/O already issued stops. The Node lifetime
oracle keeps its host alive after the async function returns so that it can
observe writes from remaining arms.

An attempt with an already-committed final result realizes its protected
intents before opener settlement. Cancellation does not retract a side effect
already performed. A losing `processes.await` releases the wait at opener close
without cancelling the separately named durable process.

Declarations drain in durable final-commit order under ADR 0099. They do not
wait for an unsettled earlier operand merely because it appears first in source.

#### 2. An await that nothing can resolve ends the cell

`Promise.race([])` is forever pending. With no operand there is no durable group
to open and nothing can resolve the await. The host ends the cell with the
uncatchable `AggregateAwaitUnsettled` terminal, whose code is
`aggregate_await_unsettled`; it does not synthesize a catchable rejection.

`all([])` and `allSettled([])` return `[]`. `any([])` rejects with an
`AggregateError` whose `errors` is empty. These need no extra lifetime rule.

### Parser: SWC, pinned, behind a lash-owned adapter

SWC is pinned exactly at `swc_common 25.0.0`, `swc_ecma_ast 28.0.0` and
`swc_ecma_parser 44.0.0`. The adapter converts SWC nodes into a Lash-owned
normalized tree; lowering produces `lash_vm::Program`. Public APIs and durable
formats contain no SWC types. An alternative parser can target the same adapter
boundary without changing the language contract.

A cell is a strict Script extended with top-level `await`. The adapter first
parses under the Module goal to admit that await, then retries under strict
Script when needed for a Script identifier spelled `await`. Module authoring
itself remains outside the dialect.

Recursive parsing runs on a thread reserving 8 MiB plus 40,000 stack bytes per
source byte. The source cap is 64 KiB. This proportional reservation supports
the no-abort argument independently of the preflight. The 28-unit source
nesting preflight provides the earlier diagnostic and limits parsing cost;
the lowered AST also stays within the shared structural bound of ADR 0060.

A cap-sized source needs more than 2 GiB of virtual address space. Failure to
reserve it returns `TS_PARSE_RESOURCES_UNAVAILABLE`. Preflight is also part of
the parse-allocation bound; stack reservation alone does not limit heap work.
A parser subprocess with an overall resource limit is a separate option.

### Conformance evidence

Checked-in Node differential answers exercise accepted operations. The corpus
records the pinned Node and regenerates its answers deliberately. Inventory
checks compare the full documented and accepted standard-library sets.
Every fixed case expressible by a corpus belongs in that corpus.

Every register refusal has an executable probe. The probe must produce the
entry's named refusal, and every named `TS_*` diagnostic has a matching probe.
`tests/deviation_register.rs` reads this ADR and the crate README to check that
contract. Reserved numbers carry no active deviation and are never reused.

The Test262 census derives its selected tests from directory, flag and feature
rows. Accepted tests run through lowering, linking, compilation and the heap
VM with the in-dialect harness. A selected outcome is a pass, a backed named
refusal, an owned defect or a named unsupported harness capability. Expanding
the selection requires implementing the admitted construct exactly.

### Beyond one script

The Node session oracle runs ordered cells as successive classic Scripts in one
realm. The Lash side runs each session live and with durable reloads between
cells. Its cell mapping observes printed lines, termination and binding probes.
`await control.finish(value)` at the top level ends the cell (FIG-5781):
nothing after the settled call runs, and the call must be awaited from the
cell's top level. A control call anywhere else is refused at lowering
(`TS_CONTROL_CALL_PLACEMENT`), and a top-level binding named after a tool
namespace root (`control`, `tools`, ...) is refused as `TS_SHADOWS_BUILTIN`.
Script completion values are not observed.
Static unknown-binding diagnostics correspond to reference failures in probes.
The classic-Script corpus contains no top-level-await cell.

A known divergence names its register entry and expected Lash answer. A pinned
open defect names the README defect entry. Unnecessary deviation or defect
answers fail the corpus rather than silently persisting.

The round-trip law lowers and admits each corpus program, projects and prints
it, reparses and admits it again, and compares module and source identity.
Typed printer refusals have explicit rows with reasons; a row fails when the
program can round-trip. Artifact laws check declaration uniqueness, lifted
literal ownership, trace-map coverage, session-visible exports, process origins,
draft/admitted identity and standalone reload.

### Generated sessions and snapshot laws

Seeded differential sessions draw from accepted census constructs and the URL
contract. They compare pinned Node answers with live and reloaded Lash runs.
Checked-in seeds and answers run without network access; longer investigations
can draw fresh seeds. A discovered divergence becomes a minimized corpus case,
with either a fix or an owned defect disposition.

Snapshot laws cover primitives and heap kinds. A global value used after reload
must behave like the equivalent value without a cell boundary, subject to the
register's named restrictions. Exhaustive heap-kind coverage requires evidence
when a new kind enters the heap.

RegExp `lastIndex` is a raw `Value`, represented by `CanonicalValue` in snapshots
and fragments and `ValueWire` in continuations. RegExp operations derive
ToLength when they use it. Fractional, non-finite, negative-zero, string and
supported reference values survive the property boundary. References held by
the property participate in collection, charging and mutation tracking.

## Deviation register

These are the only deliberate departures from ECMA-262 for an operation that is
otherwise in the accepted surface. They are runtime-system constraints, not
alternate language semantics. `crates/lash-typescript/README.md` holds the
executable register; this list is the decision that the register is closed and
that each entry is a limit taken knowingly.

1. **Runtime limits.** Instruction, logical-memory and call-frame
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
5. Reserved.
6. **Mutual recursion.** Rejected, for the durable-cycle reason above.
7. **The JSON-shaped host boundary.** Object properties whose value is
   `undefined` are omitted and array elements become `null`; incoming JSON
   cannot manufacture `undefined`. `undefined` and `null` remain distinct inside
   the VM, as ECMA-262 requires — the erasure is at the boundary only, and it is
   the erasure the specification itself defines.
8. **Lone surrogates.** Not representable in the UTF-8 value model. Literals
   reject statically. At runtime, on an astral receiver, four paths reject
   rather than diverge: indexing at one UTF-16 unit, string-backed
   `Object.values` and `Object.entries`, and the two empty-separator expansions
   `split('')` and `replaceAll('', …)`, both of which ECMA defines per UTF-16
   code unit where the value model can only advance per code point. BMP
   receivers are unaffected, and other methods whose result is unavoidably a
   lone surrogate are absent from the surface. The register carries no silent
   divergence in this family: every case where the UTF-8 model cannot reproduce
   ECMA's code-unit answer is refused by name.
9. **Source size and address space.** A cell is capped at 64 KiB of source. The
   cap is what makes the parse-stack reservation finite; the host must be able
   to hand out more than 2 GiB of address space for a cap-sized cell, and under
   a tighter `RLIMIT_AS` or `vm.overcommit_memory=2` a large cell fails closed
   with `TS_PARSE_RESOURCES_UNAVAILABLE` — a resource diagnostic deliberately
   distinct from any diagnostic describing the program. Required reservation scales with source size.
10. **Parse-time allocation depends on preflight.** Stack reservation does
    not bound SWC's heap allocation. The source and nesting preflights reject
    pathological shapes before parsing. A parser subprocess would provide a
    separate process resource bound.
11. **Nesting budget.** 28 budget units, cumulative across delimiters and
    operators, pinned on a 2 MiB stack.
12. **Dense arrays.** Appending at exactly `array.length` is supported; a write
    that would skip an index rejects as `TS_SPARSE_ARRAY_UNSUPPORTED`, and a
    negative or non-index write rejects as
    `TS_ARRAY_NON_INDEX_PROPERTY_UNSUPPORTED`. Neither path mutates an element.
    Array literal elisions are admitted. `Lash.SparseArray` records holes in
    the heap's side table, distinct from explicit `undefined`, so `in` and
    `hasOwnProperty` distinguish them. This includes trailing elisions such as
    `[1, , ]`; a single trailing comma is not an elision. Gap and non-index
    writes retain the refusals above.
13. **`console.log` is host-defined**, outside ECMA-262. It joins arguments
    with a space and renders plain objects and arrays as compact JSON for the
    host observation. Other values use their ECMA string representation. Byte
    and depth bounds apply. Node inspector formatting is outside this contract.

    Other coercions use ToPrimitive, including an object's own
    `valueOf`/`toString` in hint order. Objects without their own string method
    have the corresponding ECMA type tag.
14. Reserved.
15. Reserved.
16. Reserved.
17. **Closure boundary** (`closure-boundary`). A binding whose value reaches a
    function does not survive its cell
    ([ADR 0076](0076-lash-vm-durable-stores-hold-exclusively-owned-copies.md)):
    a function's index means something only inside the program that compiled
    it. Where Node still holds the function, a later cell's reference to the
    name is refused as `TS_FUNCTION_NOT_PERSISTED`: the session keeps the
    names it dropped, across a durable reload too, until one is bound again,
    so the value never degrades to an unknown or undefined name.
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
    See "Errors" above.
21. **Process literal as a value** (`process-literal-is-a-process-value`). A
    top-level `const`-bound uncalled `async` arrow is a `Process` value
    ([ADR 0095](0095-processes-are-values-and-process-controls-are-tools.md)),
    so `typeof` answers `"object"`.
22. **Closed-shape field guard** (`closed-shape-field-guard`). A
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
    `for` loop that crosses a `finally` rejects with `TS_FOR_UNSUPPORTED` when
    the loop has an update expression or emits per-iteration binding-cell
    copies. The lowering would otherwise run that epilogue before the
    `finally` body. A loop with neither an update nor binding-cell copies has
    no epilogue, so its `continue` is accepted.

## Consequences

- Unsupported constructs fail visibly with stable diagnostics.
- Refusal probes, inventory checks, differential corpora and specification
  tests describe distinct parts of the fidelity contract.
- Parser upgrades require renewed stack evidence and AST classification checks.
- Hosts supply enough address space for the largest accepted source or receive
  a resource refusal.
- Shared acyclic heap state persists; cyclic durable capture remains refused.

## Code evidence

- [Parser and lowering boundary](../../crates/lash-typescript/src/lib.rs#L70),
  [parse goals](../../crates/lash-typescript/src/adapter/goal.rs#L14), and
  [resource limits](../../crates/lash-typescript/src/adapter/mod.rs#L405).
- [Error taxonomy](../../crates/lash-vm/src/runtime/error.rs#L570) and
  [guest coercion](../../crates/lash-vm/src/runtime/vm/guest_coercion.rs).
- [Aggregate and call lowering](../../crates/lash-typescript/src/lower/calls.rs).
- [Refusal probes and register reader](../../crates/lash-typescript/tests/deviation_register.rs#L375).
- [Session corpus laws](../../crates/lash-typescript/tests/corpus_laws/sessions.rs),
  [generated differential sessions](../../crates/lash-typescript/tests/differential/sessions/),
  and [Test262 census](../../crates/lash-typescript/tests/test262/census/).
- [Durable shared heap](../../crates/lash-vm/src/runtime/state.rs#L699) and
  [fragment partition](../../crates/lash-vm/src/runtime/heap/partition.rs#L1).

[ADR 0137](0137-the-host-owns-events-routing-and-scheduling.md) owns host events, routing and scheduling.
