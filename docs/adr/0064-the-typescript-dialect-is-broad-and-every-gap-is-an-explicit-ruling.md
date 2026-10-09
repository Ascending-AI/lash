# The TypeScript dialect is broad, and every gap is an explicit ruling

## Status

Accepted.

## Context

Model-authored tool orchestration uses ordinary TypeScript constructs:
destructuring, spreads, collections, regular expressions, callbacks and Promise
aggregates. A narrow accepted slice makes routine code spend turns on repairs.
An undocumented gap makes its behavior depend on implementation accidents.

[ADR 0062](0062-the-typescript-dialect-is-an-exact-ecma-262-subset.md) governs
semantic agreement with ECMA-262 and the explicit deviations. This decision
sets the breadth and evidence requirements for the shipped TypeScript dialect.

## Decision

The dialect accepts a broad glue-code working set. Every unsupported construct
or deliberate divergence has a named ruling and executable evidence. Agreement
covers cells, sequences of cells and editable source projection.

### Accepted mechanisms

- Destructuring in binding and assignment positions, defaults and rest, spreads,
  whole-tail optional chaining, compound and logical assignment, updates,
  `switch`, `do...while`, `for...in`, `var` hoisting, TDZ and per-iteration
  bindings lower into the shared IR and heap VM.
- Map and Set use SameValueZero and insertion order. RegExp carries
  `lastIndex`; Date is immutable. The Error family, URL and URLSearchParams are
  heap kinds. A URL's `searchParams` is a stable live alias. The constructor
  and `instanceof` allowlists name the kinds the runtime supports.
- Iterator-shaped expressions are accepted at explicit consumers: `for...of`,
  spread, `Array.from`, Map and Set construction, and `Object.fromEntries`.
  An arbitrary iterator protocol is outside the dialect.
- `for...of` and `forEach` follow live iterables. Arrays and URLSearchParams read
  at the iterator's current index; Map and Set visit later insertions and skip
  entries deleted before their turn. Strings iterate code points.
- Object methods bind `this` to member-call receivers; plain calls use
  `undefined`, callbacks use the supplied `thisArg`, and arrows capture lexical
  `this`. Mutable closure captures share bindings with their defining frame;
  each per-iteration binding has its own cell. A closure does not persist
  across a session cell boundary. A closure writing a session slot updates
  the slot the next cell reads.
- Advertised built-in methods are function values with the method's identity,
  `name` and `length`. Own properties take precedence. Reading an unadvertised
  built-in member answers `undefined`; calling an unsupported built-in member
  has a named refusal. Functions reached by durable bindings are subject to
  `TS_FUNCTION_NOT_PERSISTED`.
- Async helpers await values grounded transitively in the durable agent
  operations. Async array callbacks run sequentially, the explicit
  `TS_ASYNC_MAP_SEQUENTIAL_V1` deviation. Promise aggregates over pending
  effects use the durable group contract of
  [ADR 0065](0065-concurrent-settlement-is-a-durable-group-at-the-effect-host-seam.md).
  `all` stops at its first consumed rejection; `allSettled` reports every input;
  `race` takes the first settlement; `any` takes the first fulfillment.
  Unawaited `sleep(ms)` contributes a pending timer to an aggregate.
- `Date.now()`, argumentless `new Date()` and `Math.random()` are host reads
  inside the VM. A read in an uncommitted stretch is drawn again after a
  crash; nothing outside the VM observed it, because every effect that could
  carry it commits with the snapshot that contains it
  ([ADR 0132](0132-durability-is-state-first-over-the-lash-store.md) §8).
  Durability does not require banning nondeterministic reads.
- Signature tables define accepted standard-library names and optional
  arguments. ECMA number formatting uses `ryu-js`. Regex execution uses
  `lash-regress` with a charged execution budget and an explicit exhaustion
  error.
- Ordinary type annotations, interfaces, aliases, generics and casts erase.
  Process parameter and return annotations declare the durable signature.
  Non-const enums lower to the pinned checker's runtime object shape; const-enum
  member reads inline their constant values. Runtime execution includes no
  TypeScript type checker.

Restricted `globalThis` member paths address session globals. `undefined`,
`NaN` and `Infinity` cannot be declared as session-global slots, while nested
bindings may use those names. The generated `__typescript_` binding namespace
is reserved.

### Every gap has evidence

The Test262 census explicitly classifies upstream feature tags and directories
as accepted, rejected with a diagnostic, or skipped with a stated reason.
Unclassified inventory fails the census check. Test262 pins language behavior,
the Node differential oracle pins observed runtime behavior, URL conformance
uses web-platform-tests, and the pinned TypeScript checker arbitrates enum
lowering and registered strictness.

ADR 0062's deviation register names deliberate semantic differences and their
rationales. The census and diagnostics are executable inventories; a parser
refusal is not permission for an undocumented approximation. Classes,
generators, accessors, prototype surgery, `eval`, modules, labels, sequence
expressions and unsupported host facilities have named diagnostics and repair
advice. `arguments` outside a non-arrow function refuses as
`TS_ARGUMENTS_UNSUPPORTED`.

Function-to-primitive conversion refuses as `TS_FUNCTION_STRING_COERCION`,
because the runtime does not keep source text for `Function.prototype.toString`.
Supported plain objects, Map and Set perform their supported ECMA coercions.
Non-iterable destructuring and spread throw a runtime `TypeError`.

The closed-shape field guard refuses a read or write of a field a statically
closed literal lacks. Spread, computed keys, computed-key writes and escapes
open the shape; a missing-field read then yields `undefined`. The guard is not
a general type checker.

Guest-number-derived allocations are bounded before allocation. ECMA limits
produce `RangeError`; heap-budget charging bounds otherwise legal allocations.
Regex execution likewise charges its work instead of admitting an unbounded
backtracking run.

### Dialect strictness: refusals `tsc --strict` shares

A refusal shared by the pinned `typescript@7.0.2` checker is registered
strictness, not automatically missing language coverage. Each row identifies
the precise shape both reject. A broader refusal under the same code still
needs its own ruling.

| tsc | Probe | Dialect refusal | Stage |
| --- | --- | --- | --- |
| TS2304 | `const x = notDeclaredAnywhere;` | `TS_UNKNOWN_BINDING` | Validate |
| TS2448 | `const x = x;` | `TS_TEMPORAL_DEAD_ZONE` | Validate |
| TS2339 | `const o = { a: 1 }; o.c;` | `TS_LINK_ERROR`, closed-shape field guard | Link |
| TS2339 | `Math.extra();` | `TS_METHOD_UNSUPPORTED` | Validate |
| TS2554 | `[1].map();` | `TS_METHOD_UNSUPPORTED` | Validate |
| TS2554 | `parseInt();` | `TS_EXPRESSION_UNSUPPORTED` | Validate |
| TS2554 | `new Map(1, 2);` | `TS_CONSTRUCTOR_UNSUPPORTED` | Runtime |
| TS7009 | `function F() {} new F();` | `TS_NEW_UNSUPPORTED` | Validate |
| TS2769 | `new RegExp(null);` | `TS_NEW_UNSUPPORTED` | Validate |
| TS2588 | `const v = 1; v = 2;` | `TS_ASSIGN_CONST` | Validate |
| TS2630 | `function f() {} f = () => 1;` | `TS_ASSIGN_CONST` | Validate |
| TS2358 | `'a' instanceof String;` | `TS_INSTANCEOF_UNSUPPORTED` | Validate |
| TS1101 | `with ({}) {}` | `TS_WITH_UNSUPPORTED` | Validate |
| TS2695 | `(1, 2);` | `TS_SEQUENCE_UNSUPPORTED` | Validate |
| TS2393, TS2300 | `function f() {} function f() {}` | `TS_FUNCTION_REDECLARATION_UNSUPPORTED` | Validate |
| TS2703 | `delete 1;` | `TS_DELETE_NON_REFERENCE_UNSUPPORTED` | Validate |

Sequence expressions with side effects remain outside the dialect even where
`tsc` accepts them. Arbitrary constructors and `instanceof` targets remain
outside the explicit kind lists. An authored function called with fewer
arguments runs with `undefined` for absent parameters; listed built-ins enforce
their declared arity. Registered strictness does not imply that the runtime
performs advisory type checking.

### Session and projection laws

The session oracle compares successive cells with classic Scripts in one Node
realm under ADR 0062's cell mapping. Its corpus records explicit divergences.
The print, reparse and admit law checks that rendered source preserves the
artifact's module reference and source identity. Artifact laws cover declarations,
execution sites, visible bindings, process origins and reload sufficiency.
Corpus deviations and round-trip refusal lists fail if an entry stops being
necessary, so resolved differences cannot remain in the inventory unnoticed.

## Alternatives considered

A small syntax subset imposes repair work on ordinary model output. Breadth is
bounded by explicit support and evidence rather than by an arbitrary short list.

Quiet approximation makes an accepted program differ from the language the
model knows. A named refusal or recorded deviation makes the difference
inspectable and repairable.

An approximate Rust type checker creates a second authority that can disagree
with `tsc`. Erasure keeps runtime semantics separate from advisory host checking.

Per-host language acceptance forks the census and prompt contract. Parsing and
lowering are independent of the executing host; execution may return a typed
unsupported-host refusal.

## Consequences

A change to accepted syntax moves the census, diagnostics, prompt vocabulary and
oracle evidence together. Durable formats follow
[ADR 0115](0115-the-1-0-binary-carries-its-half-of-every-upgrade.md); their constants
live in code. The dialect has no migration decoder for incompatible parked state.

## Executable evidence

- [Parser and lowering entry](../../crates/lash-typescript/src/lib.rs#L70),
  [construct lowering](../../crates/lash-typescript/src/lower/constructs.rs) and
  [standard-library signatures](../../crates/lash-typescript/src/signatures.rs)
  implement the accepted mechanisms.
- [Strictness probes](../../crates/lash-typescript/tests/rejections.rs#L646),
  [checker pin](../../crates/lash-typescript/tests/differential/generate.mjs#L18)
  and [census](../../crates/lash-typescript/tests/test262/census) name refusals
  and deviations. [Census synchronization](../../crates/lash-typescript/tests/test262/sync.mjs)
  checks the inventory.
- [Session oracle](../../crates/lash-typescript/tests/differential/sessions/oracle.mjs#L1),
  [round-trip law](../../crates/lash-typescript/tests/corpus_laws/round_trip.rs#L1)
  and [artifact laws](../../crates/lash-typescript/tests/corpus_laws/invariants.rs#L1)
  cover more than isolated expressions.
- [Number formatting](../../crates/lash-vm/src/runtime/javascript.rs#L521),
  [regex budget](../../crates/lash-vm/src/runtime/vm/javascript_regexp.rs#L150)
  and [array allocation bounds](../../crates/lash-vm/src/runtime/vm/javascript.rs#L529)
  implement runtime constraints.
