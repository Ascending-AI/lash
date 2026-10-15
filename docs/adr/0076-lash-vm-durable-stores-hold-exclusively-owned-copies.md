# Durable VM state preserves shared references and owns its roots

## Context

Guest bindings can refer to the same mutable heap object. Durable state must
preserve that identity, so a mutation through one alias stays visible
through another after a park, a resume in another worker or the next cell.
A detached host value cannot represent every value the machine can retain.

## Decision

The kernel uses reference semantics. Bindings, object fields, collection
members and task records share heap references (`EntityId`s); they never
universally copy the graph they reach. Operation-specific copies do not
change this language-wide aliasing rule.

### The durable form is the heap itself

The machine's state is one persistent heap
([ADR 0145](0145-kernel-execution-and-its-checkpoints-share-one-persistent-heap.md)).
An object is one directory entry under one identity, and every holder names
that identity. A checkpoint seals the pages a stretch changed and names the
heap header; resume opens the same directory. So aliases, cycles, live
iteration and first-written key spelling survive parks, worker resets and
cell boundaries without a separate shape for durable graphs, and with no
validator that chooses between a forest and a shared form. Page shapes,
summaries and child membership are checked when a page is read (ADR 0145
§3). Physical page sharing never merges identities, and equal objects stay
distinct.

The persistent roots are the checkpoint root's trees: tasks and their
frames (bindings, loops, `finally` departures, iterated values and library
arguments), waits, joins and a session cell's bindings. The memory census
traces from the same roots, plus the registers, pins, native arguments and
fresh entities of the statement under way.

### Session roots and the host view

A session's carried bindings and saved functions are heap roots in its
`SessionRoot` (ADR 0145 §9). The host reads them through
`lash_kernel_vm::SessionEditor`: a binding whose value is data is copied out
as a `Datum`; one that is not data, a function reference, is listed by name
as `SessionValue::Opaque`. It never exposes a partial value. A binding that
reaches a closure or a task handle is not carried to the next cell
(`K-SES-003`), and the session lists its name with why.

### Host results are copies

A value a host returns enters the guest as new heap objects. It never
aliases a guest object, including when an in-process host answers with the
very value it received as an argument: the guest then holds two objects, and
a write through one is invisible through the other. A committed outcome is
frozen once and delivered as a fresh identity family over its frozen pages,
so equal subtrees at distinct positions are distinct objects, and two
deliveries of one frozen value are two families.

A host across a wire can only return a copy. So can a run parked on an
operation and resumed from its root, which is answered with an outcome its
host held. Applying the same rule to a host that answers at once makes a
parked run and a straight run reach the same state.

## Alternatives considered

Copying on every durable store changes guest mutation and identity.
Requiring a forest at every boundary refuses valid aliasing programs. A
separate durable graph shape, encoded at park and rebuilt at resume, costs
the whole retained state at every boundary; the heap's own directory keeps
identity at the cost of what changed.

Using the host view as authoritative state loses bindings that have no
detached host shape. Persisting both the roots and the view creates two
records to reconcile.

## Consequences

Aliases and cycles survive parks, resumes and cells. Host-visible bindings
are a projection, so an absent entry alone does not prove that a name is
absent from the session: the session's descriptor lists every name it binds
and every name it does not carry.

## Code references

- `crates/lash-kernel-state/src/paged/directory.rs`: the object directory
  and the frame and task records.
- `crates/lash-kernel-state/src/paged/laws/heap.rs`
  (`aliases_see_one_mutation_across_seal_and_open`,
  `cycles_seal_as_a_page_dag`).
- `crates/lash-kernel-vm/src/session_editor.rs`: the host's view of a
  carried session.
- `crates/lash-kernel-state/src/paged/family_impl/laws.rs`
  (`a_noop_next_cell_copies_nothing`): carried aliases keep their
  identities.
