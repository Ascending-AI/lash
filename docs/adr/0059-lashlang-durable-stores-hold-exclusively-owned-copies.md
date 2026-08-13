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

Sharing between roots is therefore unreachable by construction. Two independent
checks keep it that way rather than trusting the argument:

- the decoder refuses a snapshot whose roots share an object, and refuses a heap
  object whose member is an inline compound rather than a scalar or a reference;
  a reference can never hide below the member level in an accepted wire; and
- the encoder asserts the same root-isolation property in debug builds, so a
  violation fails at the write that introduced it rather than at a later cold
  restore in another process.

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

## Consequences

- An emitted snapshot always decodes: the encoder and the decoder describe the
  same language.
- Mutation through one binding is never observable through another, matching the
  pre-heap tree language, including across a snapshot boundary.
- Container insertion costs a copy of the inserted value's graph. In-place
  appends stay O(1) because the accumulator owns its object outright.
- Sharing is not merely discouraged: it is not representable in a persisted
  state, so the wire needs no way to express it.
