# The TypeScript dialect is broad, and every gap is an explicit ruling

## Status

Accepted.

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
- **For-of snapshot safety** refuses a loop body that may mutate the iterable,
  including calls carried by destructuring defaults, parameter defaults, and
  computed pattern keys; the v1 iterator snapshots rather than observes live
  mutation.
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
   stringify replacers. Silent divergence remains the one forbidden outcome.
   `forEach` is **not** on this list: it iterates live and node-exact across
   arrays, `Map`, `Set`, and `URLSearchParams`, and an earlier revision of this
   ADR claimed a snapshot deviation the code does not have.
3. **Repair-carrying rejection.** Everything still rejected — classes,
   generators as a protocol, getters/setters, prototype surgery, `eval`,
   labels, locale surfaces, timers, `Promise.race`/`any`
   until first-settlement durability exists (FIG-1416) — rejects with a
   diagnostic that names the construct and the in-dialect rewrite. The
   rejected set shrinks only by evidence: observed collision traffic
   promotes a construct into a ruling, in either direction.

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
