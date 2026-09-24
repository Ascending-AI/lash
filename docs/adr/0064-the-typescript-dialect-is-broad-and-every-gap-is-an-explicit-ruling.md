# The TypeScript dialect is broad, and every gap is an explicit ruling

## Status

Accepted.

Amended 2026-09-13 (FIG-3016): [ADR 0096](0096-typescript-is-the-sole-rlm-dialect.md) makes this the only RLM
dialect. A gap ruling here no longer has a second surface to be weighed against;
it is the whole authoring contract. References to "Lashlang" name the IR and VM.

Amended 2026-09-23 (FIG-3599): "every gap is an explicit ruling" now covers
sequences of cells and the lens, not only single expressions. The Node
session oracle compares multi-cell sessions with successive classic Scripts in
one realm under the mapping [ADR 0062](0062-the-typescript-dialect-is-an-exact-ecma-262-subset.md#beyond-one-script-fig-3599)
records; its divergences are register entries 17–21 there, and the ones found
but not yet fixed are the crate README's open conformance defects. A
print/reparse/admit round-trip law and artifact invariants run over every
corpus, and the test262 slice gains block-scope, per-iteration binding and TDZ
probes. The session corpus and the round-trip allowlist are ratchets like the
census: a row that stops diverging, or an allowlist entry that stops being
needed, fails CI.

Amended 2026-09-24 (FIG-3626): the linker's missing-field check is a named
deviation, the **closed-shape field guard**
([ADR 0062](0062-the-typescript-dialect-is-an-exact-ecma-262-subset.md#deviation-register)
register entry 22), and it is kept only where it is sound. It refuses a read
or write of a field that a statically closed object literal lacks, as `tsc`
does; a literal that a spread, a computed key, a computed-key write or an
escape opens reads a missing field as `undefined`, as JavaScript does. It is
not a type checker: it refuses nothing a closed literal could hold at run
time. Each open trigger has a law, and the refusal names the missing field
and the literal's fields so a model can repair the program.

Amended 2026-09-24 (FIG-3625): `for...of` follows its iterable live, as
ECMA-262 does, and the name-based snapshot-safety refusal is retired with ADR
0062 register entry 16. Runtime semantics are exactly ECMA's; the only static
refusal of an otherwise accepted program is the closed-shape field guard.

Amended 2026-09-24 (FIG-3651): the dialect may refuse, rather than implement,
a construct ECMA-262 accepts where `tsc --strict` rejects it, and only there.
Such a refusal is sound: it refuses exactly the shape the pinned `tsc`
(7.0.2) rejects and never a program `tsc` accepts. It names its own `TS_*`
code, cites the `tsc` diagnostic, and carries a census row with a probe and a
rejection test like any other ruling. Two join the closed-shape field guard:
`TS_FUNCTION_REDECLARATION_UNSUPPORTED`, a function declaration that shares
its var scope's name with another function, a `var` or a parameter (TS2393,
TS2300); and `TS_DELETE_NON_REFERENCE_UNSUPPORTED`, `delete` of an operand
that is not a property reference (TS2703). An ECMA-262 early error in the
same cell still reports first.

## Context

ADR 0062 fixed the dialect's contract shape: everything accepted behaves
exactly like Node, everything else rejects loudly, and the checked-in Node
differential oracle arbitrates. It said nothing about how *large* the accepted
slice should be, and v1 shipped deliberately narrow: no destructuring, no
spread, no `switch`, no regex, no collections, a 64-name stdlib.

Three independent evidence streams then converged on the same verdict. The
judged dialect-parity battery showed real models hitting the rejection wall in
first-shot code — destructuring in callback parameters, `Promise.all` over
async arrows, `new Set(...)` dedupe, regex extraction — and burning turns on
repairs. A read of the two peer codemode systems (ax, opencode) showed both
steering models toward constructs we rejected, and ax's recorded model-output
corpus showed the same collisions live, with `globalThis` state idioms on top.
A four-lens completeness panel (scenario programs, corpus frequency, an
ECMA-262 spec walk, adversarial sibling-asymmetry hunting) ranked the gaps and
exposed a second failure mode as bad as any rejection: surface the contract
never mentioned at all, where behavior is whatever the implementation happens
to do.

The ruling that resolves the tension: the bar for v1 is that a model rarely if
ever reaches outside the accepted surface, and every remaining gap must be an
explicit, recorded decision — never an accident of omission.

## Decision

The dialect accepts the broad surface, delivered by mechanism class rather
than by construct list:

- **Desugars** lower onto existing machinery with no new semantics:
  destructuring in every binding position, spread, optional chaining with
  whole-tail short-circuit, compound assignment and updates, `switch`,
  `do-while`, `for-in` over the prototype-free universe, parameter defaults
  and rest, `var` hoisting, TDZ, and per-iteration loop bindings.
- **Heap kinds** carry stateful built-ins durably: Map and Set
  (SameValueZero, insertion-ordered, reference-identity keys), RegExp with a
  durable `lastIndex`, immutable Date, the eight-member Error family, URL and
  URLSearchParams with a live `searchParams` alias. Constructing them is a
  *designed* exception list to the general `new` rejection; `instanceof`
  accepts exactly the kinds that exist.
- **Contextual acceptance** admits iterator-shaped expressions only where the
  observable behavior is Node-exact without an iterator protocol: for-of
  heads, spread, `Array.from`, `new Map`/`new Set` arguments, and
  `Object.fromEntries` — with one shared repair diagnostic everywhere else.
  Restricted `globalThis` member paths work the same way: any depth rooted at
  an identifier member, with reserved value identifiers (`undefined`, `NaN`,
  `Infinity`) that can never name a session global.
- **For-of follows its iterable live** (FIG-3625). The v1 iterator walked a
  snapshot and refused, by the iterable's name, a body that might mutate it;
  that check refused shadowing bindings that never touched the iterable and
  missed aliases made before the loop, so it is gone. An array or a
  `URLSearchParams` is read at the iterator's index on every step, and a `Map`
  or a `Set` visits entries added during the loop, exactly as ECMA-262's
  iterators do.
- **Async helpers** are accepted by moving the restriction from where `await`
  may appear to what it may await: operands must ground transitively in the
  durable agent surface. The async array driver executes callbacks
  sequentially — a registered deviation (`TS_ASYNC_MAP_SEQUENTIAL_V1`), not a
  quiet approximation.
- **Nondeterministic reads are journaled, not banned.** `Date.now()`, argless
  `new Date()`, and `Math.random()` are host effects recorded at the same
  journal boundary as every other effect, so a replayed turn draws the same
  values it drew the first time. Journaled is replay-deterministic, which is
  the property a durable program actually needs; banning them would have
  bought nothing and cost the most ordinary idioms in the language. An earlier
  revision of this ADR listed `Math.random` among the rejected constructs,
  which it has never been.
- **The stdlib** is the full glue-code working set, each optional argument an
  explicit signature-table entry, with `ryu-js` at the single
  number-to-string choke point because Rust's native formatting is not
  ECMA-exact at the edges.
- **Built-in methods are values** (FIG-3701). A read of an advertised
  instance method that misses the value's own properties (`'x'.includes`,
  `[].map`, `new Set().keys`) answers ECMA's one function object for it on
  that value's prototype: `'a'.includes === 'b'.includes`,
  `'a'.includes !== [].includes`, `new Set().keys === new Set().values`,
  `typeof` is `"function"`, and its own `name` and `length` are node's. Field,
  computed, optional and destructuring reads agree, and an own property wins.
  A plain call passes `undefined` as the receiver and answers as node does: a
  TypeError, except `Object.prototype.toString`. The value is a function, so
  the closure boundary drops a binding that reaches one
  (`TS_FUNCTION_NOT_PERSISTED`). Only advertised names are readable; an ECMA
  method outside the call surface still reads `undefined`.
- **Regex** is ECMA semantics on the published, fuel-instrumented
  `lash-regress` fork of the `regress` engine: every bytecode dispatch and
  backtrack transition is charged against a deterministic budget, because a backtracking engine
  running model-authored patterns on model-chosen inputs is otherwise an
  unbounded-runtime hole. The instrumentation is shaped for upstreaming; the
  workspace crate is the fork.
- **TypeScript type syntax is erased, never checked.** Annotations,
  interfaces, aliases, generics, and casts parse and vanish, exactly as
  Deno, Node, and every SWC-based system does; `enum` is the one type-syntax
  form with runtime semantics, so it lowers to its exact tsc object shape
  rather than erasing wrongly. No type checker enters the pipeline: the only
  trustworthy checker is tsc, tsc means running JavaScript tooling inside a
  deterministic Rust runtime, and a checker that disagrees with tsc anywhere
  gaslights a model that knows the real language. Checking, where a host
  wants it, is an out-of-band advisory concern.

Three regimes make "every gap is a ruling" enforceable rather than
aspirational:

1. **A strict conformance census.** The vendored Test262 harness carries an
   exhaustive census: every upstream feature tag and test directory is
   explicitly accepted, rejected with a named diagnostic, or skipped with a
   reason that cites a register entry. An uncensused feature fails CI. The
   official suites — Test262 for the language, web-platform-tests for URL,
   the pinned-Node oracle for behavior, pinned-tsc for enum lowering — are
   the arbiters; hand-written rows supplement them, never replace them.
2. **A named deviation register.** Where the dialect deliberately diverges,
   the divergence has a name, a rationale, and tests: UTC-pinned dates with a
   loud error on string coercion, sequential async callbacks, ToLength at
   `lastIndex` write, lone-surrogate match output, dense arrays, no prototype
   chain, value trees at the durable boundary, cycle preflight before
   stringify replacers, the closed-shape field guard. Silent divergence
   remains the one forbidden outcome.
   `forEach` is **not** on this list: it iterates live and node-exact across
   arrays, `Map`, `Set`, and `URLSearchParams`, and an earlier revision of this
   ADR claimed a snapshot deviation the code does not have.
3. **Repair-carrying rejection.** Everything still rejected — classes,
   generators as a protocol, getters/setters, prototype surgery, `eval`,
   labels, locale surfaces, host timer callbacks — rejects with a
   diagnostic that names the construct and the in-dialect rewrite. The
   rejected set shrinks only by evidence: observed collision traffic
   promotes a construct into a ruling, in either direction.
   `Promise.race` and `Promise.any` left this set once first-settlement
   durability existed: FIG-3397 accepted them on durable effect groups, with
   an unawaited `sleep(ms)` as a pending timer
   ([ADR 0099](0099-tool-children-of-effect-groups-are-live-closing-settled.md)
   §10, §11).

One safety invariant joins the no-abort guarantee: any operation whose
allocation size derives from a guest-supplied number bounds the allocation
*before* allocating — Node-exact `RangeError` at the ECMA limits and a heap
budget pre-charge above them. Allocate-then-check is how guest code becomes
host memory pressure; it is treated as a P0 wherever found.

## Consequences

> **Historical versions.** The version numbers in this ADR record the state at ratification. The current values live in `lash::formats`; see `scripts/check_format_versions.py`.

- Durable formats moved once, as one coordinated clean cutover. The versions
  in tree when this was ratified were bytecode 9, VM continuation 7,
  snapshot 6, VM ABI `lashlang-vm-abi-v6`, RLM snapshot envelope 12, and
  Lashlang segment handover 3. Several have moved since, which is what the
  note above means: read the current values from `lash::formats`, never from
  here. Older parked state does not resume across the boundary;
  deployments drain first. Per ADR 0055 there is no migration decoder.
- The accepted surface is now large enough that its integrity depends on the
  census and the register, not on reviewers' memory. A change that widens or
  narrows the surface must move the census, the register, the prompt
  vocabulary, and the oracle together; the walkers and count pins fail CI
  when they drift apart.
- The model-facing prompt describes the same surface the tables enforce,
  in both directions, checked by the existing prompt walkers.
- What remains deliberately absent is recorded where it can be acted on:
  first-settlement combinators and interleaved async execution share one
  durability design (FIG-1416); everything else rejected-by-design sits in
  the census with its diagnostic.
