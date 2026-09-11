# 0090: Named process signatures are authoritative

Status: Accepted

## Context

Lashlang process types previously stored a flattened input type beside an
independent `input_count`. That representation lost the parameter name for a
single scalar or object input and could disagree about invocation shape. Host
trigger operations also described an unconstrained process by inventing one
`any` parameter. Neither representation was strong enough to verify a process
value that arrived indirectly against the immutable artifact export it named.

## Decision

A known process type contains one checked, ordered signature: every parameter's
Lashlang name and type, followed by its output type. Arity and named invocation
shape are derived from that signature. Parameter names must be valid source
identifiers and unique. The sole source forms reuse declaration-list syntax:

```lash
Process<(), bool>
Process<(message: str), bool>
Process<(payload: { value: str }), bool>
Process<(left: str, right: int), bool>
```

The former anonymous `Process<input, output>` source and the artifact shape with
`input` and `input_count` are refused. There is no compatibility reader.

Hosts may publish an unknown process type when an operation cannot truthfully
assert a signature. Unknown is an opaque Rust API value and canonical Serde
shape, renders as bare `Process` in diagnostics and TypeScript, and is refused
in Lashlang source and program-owned artifact IR. It does not imply an arity or
parameter name. Unknown is gradually consistent with known process types and
with no non-process type.

Known callable assignment requires equal arity and identical parameter names
in declaration order. Parameter types are contravariant, result types are
covariant, and ADR 0073's gradual `any` consistency applies recursively.

Process identities continue to contain immutable module, requirements, and
process references rather than duplicating the signature. Before durable child
registration, process-start ingress loads each process-valued argument's
artifact, verifies the identity against its export, resolves aliases, and
checks the artifact signature against the receiving parameter type. This walk
covers nested objects, lists, and unions; a rejected union arm does not prevent
a later arm from accepting the value. Existing trigger execution verification
remains in place, as does the requirement that trigger registration map
`trigger.event`, including for a zero-parameter target.

The process type has a new canonical wire and semantic hash encoding. The
Lashlang semantic hash advances from v6 to v7, the workflow graph schema from 5
to 6, and its type-facet schema from 1 to 2. The ModuleRef envelope remains v2.
Bytecode, continuation, snapshot, and VM ABI versions do not change because no
instruction or runtime-value representation changed. Stored artifacts using
the anonymous process shape must be recompiled and republished.

## Consequences

- Process parameter names, order, types, and outputs participate in immutable
  module and process identity.
- Zero, scalar, outer-named object, and multiple-parameter callables use one
  representation through parsing, linking, artifacts, schemas, and TypeScript.
- Host schemas can remain honest when they know only that a value is callable.
- An externally or indirectly supplied process identity cannot satisfy a
  different named signature before durable work is registered.
- This adds only process-signature invariants. Existing enum, union, and object
  construction rules are unchanged.

This decision extends [ADR 0011](0011-self-contained-processes.md)'s immutable
captured artifact identity and uses [ADR 0073](0073-gradual-value-types-through-to-the-workflow-editor.md)'s
gradual assignment policy.
