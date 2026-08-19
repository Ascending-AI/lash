# The lashlang VM is a heap substrate with dialect-lowered value semantics

## Status

Accepted.

## Context

RLM execution runs authored code in a durable VM. Until now that VM had exactly
one language above it, so the machine and the language were free to be the same
thing: Lashlang's value semantics could live in the machine, because nothing
else executed there.

Adding a second dialect ends that freedom. A TypeScript dialect is
reference-semantic by definition — two bindings can name one object, mutation
through either is visible through both, and `===` compares identity. Lashlang is
value-semantic: a store copies, and mutation through one binding is never
observable through another (ADR 0059). One of the two has to be the machine's
semantics and the other has to be produced by lowering, or there have to be two
machines.

Two machines is the expensive answer, and the expense is not in the interpreter
loop. It is in everything durable: continuation and bytecode formats, the
persisted heap, the allocation counter, the GC schedule, the logical-byte
metering schedule (ADR 0055), checkpoint components (ADR 0056), the execution
bounds contract, the exception machinery's handler and finally stacks and their
scope-extent validation. Each of those is a durable contract that a host
operates and a cold restore has to agree with. Forking them per dialect means
two determinism stories, two drain procedures, two sets of format versions, and
a fix in one that silently does not land in the other.

So the question is not whether to share a machine but which semantics the shared
machine has.

## Decision

The VM is a heap substrate with reference semantics. Value semantics is a
dialect lowering.

Heap objects are addressed by identity and the heap primitives are
reference-preserving: they duplicate a reference where the machine's operation
says to duplicate a reference, and nothing in the VM copies a graph on its own
initiative. Lashlang's value semantics is produced entirely by the Lashlang
compiler, which inserts a recursive isolation copy at every durable store — name
and slot stores, global stores, every container member, iterator bindings,
effect-result bindings, and the `State` patch APIs, enumerated in ADR 0059. A
dialect that wants reference semantics omits that lowering and writes the
reference straight through. The decision lives in the compiler, in one direction
only.

The proof obligation for the lowering is behavioral, not structural: Lashlang's
full existing battery stays green unchanged. An expectation edit in a Lashlang
test is evidence that the lowering is wrong, not that the test was — the
language is defined by what it already does, and the heap substrate is only
allowed to change how that is achieved.

### Why the reverse design is not available

Putting value semantics in the machine and building a reference dialect on top
is not a symmetric trade that we happened to decide the other way. It does not
work.

A value-semantic machine has no identity to hand out. Its durable form names a
forest — roots and the trees they own — and sharing has no encoding in it at
all, which is exactly the property ADR 0059 relies on. A reference dialect
lowered onto it would have to synthesize identity in guest-visible state: an
object table held as an ordinary machine value, every property read and write
indirected through it, `===` and aliasing implemented in emitted code. That
table is then the real heap, and every substrate contract measures the wrong
thing. The collector would sweep the machine's trees while liveness actually
lived in the dialect's table, so nothing would ever be collected until the
dialect implemented its own reachability; the logical-byte schedule would meter
table entries rather than objects; the deterministic identity a continuation
persists would be the machine's, not the one the guest can observe. The dialect
would have re-implemented a heap VM inside a VM, and paid for both.

The asymmetry is the point. Removing a copy is a compiler edit. Adding identity
is a second machine.

### What the substrate owns, and no dialect may tune

Because both dialects share one wire and one determinism story, the following
are substrate contracts, fixed for every dialect:

- **Deterministic identity.** Object IDs are allocation-ordered and never
  reused, so two independent runs of the same program dump byte-identical
  continuations.
- **Deterministic collection.** A non-moving stop-the-world mark-sweep triggered
  purely by the monotonic allocation counter every 1,024 allocations, and
  additionally wherever a boundary needs the live set exactly — at a park, at
  snapshot capture, and at a committing batch of global patches. A dialect does
  not get its own trigger; two triggers on one wire is two determinism stories.
- **Metering.** Live logical bytes under the versioned Lashlang heap size
  schedule, never allocator or RSS measurements, bounded by the explicit
  `memory_limit` of ADR 0055.
- **Persistence of the counters.** The allocation counter, live logical bytes,
  and the size-schedule version ride in the continuation, so a resumed segment
  meters as one continuous execution rather than restarting the schedule.
- **Measured structural bounds.** The AST nesting cap of 64 is derived from the
  measured stack cliff of the link/compile/execute pipeline, not from a grammar.
  Every dialect front end must land inside it: Lashlang's parser cap was reduced
  from 40 to 30 so that its worst two-AST-levels-per-syntactic-level shape lands
  at 63, and a TypeScript front end owes the same measurement rather than its
  own budget.

Function values, closures and stackless call frames, and the exception layer's
handler and finally stacks with their scope-extent validation and durable
error origin are likewise substrate machinery: a dialect selects which of it to
emit, never how it behaves once emitted. One consequence is already binding on
the TypeScript lowering: a TypeScript `return` must lower to a real function
return and never to `Expr::Finish`, which is a process terminal and
deliberately does not run pending `finally` blocks.

### Relationship to ADR 0059

ADR 0059 stands as written and is not restated here. Read together, the split is
this: ADR 0059 decides what *Lashlang's* durable state may look like — a forest
of exclusively owned trees, enforced by a validator at every durable boundary in
release builds — and this ADR decides that the same rule is a dialect invariant
rather than a property of the machine.

That distinction has a consequence which must be named rather than discovered.
A reference-semantic dialect omits the isolation lowering and therefore produces
graphs with sharing, which the forest validator refuses by construction. So the
durable-boundary validator becomes dialect-scoped: the segment's pinned dialect
selects it, and Lashlang's remains exactly the validator ADR 0059 specifies,
with no relaxation. The general object graph a reference dialect persists needs
its own wire contract — how sharing is expressed, and how a cold restore
reconstructs it identically — and that contract is an obligation of the
TypeScript dialect work, not a decision taken here. Until it exists and is
enforced with the same "refused at both ends" discipline, no reference-semantic
dialect may write durable state.

### Format versions

The substrate layers shipped so far move the durable contracts to bytecode
format 8, continuation format 6, `lashlang-vm-abi-v6`, and snapshot format 5.
ADR 0055's clean-cutover rule applies unchanged: deployments drain or recreate
parked Lashlang processes, older bytes are neither migrated nor decoded, and no
compatibility decoder exists at any of these version boundaries.

> **Historical versions.** The version numbers in this ADR record the state at ratification. The current values live in `lash::formats`; see `scripts/check_format_versions.py`.

## Consequences

- One VM, one durable format family, one determinism story, and one metering
  schedule serve both dialects. A defect fixed in the substrate is fixed for
  every dialect at once.
- Lashlang pays the isolation copy on every durable store, which is inherent to
  copy semantics and already accepted in ADR 0059. A copy-on-write
  representation behind identical observable semantics remains possible later.
- A reference-semantic dialect gets identity, aliasing, and `===` for free from
  the substrate, and owes only its own durable-graph wire contract and
  validator.
- The forest rule is no longer a global truth about persisted VM state. Any
  future reader of durable heap state must consult the segment's dialect before
  assuming ownership structure.
- A dialect that needs something the substrate cannot express must argue it at
  the substrate, where both dialects will get it, rather than forking the
  machine. Adding a second VM is a decision that reopens this ADR.
