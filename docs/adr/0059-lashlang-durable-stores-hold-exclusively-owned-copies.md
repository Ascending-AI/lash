# Lashlang durable stores hold exclusively owned copies

## Status

Accepted.

## Context

Lashlang values used to be trees: every binding held its own structure, and a
store copied. Representing tuples, lists and records as mutable heap objects
addressed by identity makes in-place mutation cheap, but it also makes sharing
representable — two bindings can name the same object, and mutating through one
becomes visible through the other. That is a different language.

The persisted form makes the question sharper than a taste argument about
aliasing. A snapshot names its roots and lists the objects they reach. If two
roots share an object, the wire has to describe a graph rather than a forest,
and every consumer — the decoder, the checkpoint component split, a future
reader in another process — has to agree on how the sharing is expressed and
restored. The fix round tried to keep isolation shallow: copy the entering
object, keep its descendants shared, and argue inductively that no store could
observe another's mutation. Two independent reviews broke that argument with
ordinary programs, and one of them produced a state whose emitted snapshot the
matching decoder rejected. A snapshot that a live session can write and a cold
restore cannot read is a durability defect, not a performance trade.

## Decision

The heap object graph is a forest of exclusively owned trees.

A heap reference may be duplicated only in transient VM operand flow — the
stack, the last-value register, an iterator cursor mid-loop. Every durable
store holds an exclusively owned recursive copy: name and slot stores, global
stores, every container member (literal, push, concat, comprehension append,
indexed and field assignment, path assignment right-hand side), iterator
bindings, effect-result bindings, and the `State` patch APIs.

One recursive isolation operation implements this. It reallocates the entire
graph reachable from the stored value under fresh IDs, and it deliberately does
not consult the boundary materialization cache, whose whole purpose is to make
an export/import round trip identity-preserving — precisely the sharing an
isolation must not reintroduce. Isolation is staged: IDs are reserved and
objects built before anything is charged or committed, so a copy that would
cross the memory bound leaves the heap byte-identical.

Sharing between roots is therefore unreachable by construction. One validator
enforces it rather than trusting the argument, in release builds, at every
durable boundary — snapshot decode and encode, continuation decode, resume and
encode:

- every reachable object has at most one *ownership edge*, where an ownership
  edge is a reference held by a durable root or by an object member;
- the ownership edges must not form a cycle, and every object must be reachable
  from some root; and
- a heap object member is a scalar or a reference, never an inline compound, so
  a reference can never hide below the member level.

Durable roots are the ones whose value survives the boundary: slots, globals,
and a parked loop binding waiting to be restored into its slot. A continuation
also carries transient roots — the operand stack, the last-value register, and
an iterator's captured cursor — which may hold duplicate references, because a
VM legitimately holds a value on the stack and in the slot it was just stored
into, and a cursor holds the elements it is handing out. Those handles confer no
ownership, so they can never create a second owner. Every insertion into a
durable store copies, so a transient duplicate can never become a durable one.

User-function suspension adds one frames-aware qualification. The saved root
frame remains the durable owner of root slots and globals while a function is
active; the active callee and every saved function frame are transient borrowers
for forest validation, although their values are serialized and traced as GC
roots. This deliberately weakens the simple "all frame slots are durable roots"
reading: a named function's `self_slot` legitimately aliases the closure owned
by its caller, and caller/callee operands can temporarily retain the same heap
value. One frame-aware root enumerator supplies both GC and wire validation so
the two boundaries cannot disagree about that classification.

Applying the validator to the encoders as well means a violation fails at the
write that introduced it rather than at a later cold restore in another process,
and cannot reach durable storage at all.

The transient side of that rule rests on a property of the language rather than
of the heap: assignment in Lashlang is a statement, so no durable store can run
while operands are pending, and a borrowed handle on the stack cannot outlive
the store that created it. Two changes would break it — making assignment an
expression, so that `f(x = [1], x)` puts a store between two live operands, or
adding an opcode that writes a slot while unrelated operands are still on the
stack. The heap layer does not detect either: it would keep accepting the
duplicate as a transient borrow. A parse-level test pins the statement property;
an opcode that wanted to violate the second would have to declare itself in the
instruction heap plan, which is where someone would have to notice.

One recursive enumerator answers child discovery for allocation bookkeeping,
reverse parent edges, mark, sweep, wire validation and root traversal, so no
consumer can see a shallower answer than another.

The cost is accepted: copying an alias-heavy graph is proportional to the graph,
and a program that repeatedly copies a large container pays for it. That is
inherent to copy semantics, which is the language Lashlang already had. Storing
a copy-on-write representation behind identical observable semantics is possible
later and is recorded as future work, not taken here.

A dialect that wants reference semantics — a future TypeScript lowering, for
instance — omits the isolation lowering. The heap primitives stay
reference-preserving; the decision lives in the compiler.

Function values are VM-private durable values, not host values. Snapshot and
continuation checkpoints retain closures, but materializing runtime globals for
a host omits an entire binding if a closure occurs anywhere below it. This is a
deliberate silent-omission policy: a host that round-trips only its materialized
globals drops those closure-bearing bindings. Direct host-boundary uses such as
effect arguments, formatting, projection, JSON conversion, and validation fail
with the typed `FunctionValueAtHostBoundary` error instead.

## Consequences

- An emitted snapshot always decodes: the encoder and the decoder describe the
  same language.
- Mutation through one binding is never observable through another, matching the
  pre-heap tree language, including across a snapshot boundary.
- Container insertion costs a copy of the inserted value's graph. In-place
  appends stay O(1) because the accumulator owns its object outright.
- Sharing is not merely discouraged: a persisted state that expresses it is
  refused at both ends, so nothing downstream has to decide how shared ownership
  would restore.
