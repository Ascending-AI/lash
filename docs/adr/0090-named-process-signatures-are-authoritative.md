# 0090: Named process signatures are authoritative

Status: Accepted

## Context

A process value may arrive through a tool or nested container. Its claimed type
must agree with the immutable executable before registration. Arity alone
cannot describe named calls.

## Decision

A known process type contains one checked, ordered signature: each parameter's
name and type, followed by the output type. Arity and named invocation shape
derive from it. Parameter names are valid identifiers and unique.
`Process<(), bool>`, `Process<(message: str), bool>`, and
`Process<(left: str, right: int), bool>` are canonical IR diagnostic renderings.
TypeScript is the source dialect under
[ADR 0096](0096-typescript-is-the-sole-rlm-dialect.md).

Hosts may declare an unknown process type when they cannot assert a signature.
It renders as bare `Process`, implies no parameter name or arity, and is
gradually consistent with known process types. Program-owned artifact IR
requires complete known process types. The tagged decoder refuses unknown
fields, invalid names, duplicates, and anonymous input/count shapes.

Known callable assignment requires equal arity and identical parameter names in
declaration order. Parameters are contravariant and the result is covariant.
[ADR 0073](0073-gradual-value-types-through-to-the-workflow-editor.md)'s gradual
`any` consistency applies recursively. The linker checks process-valued
arguments against receiving types, including nested containers and unions.

Engine resolution supplies the authoritative signature of an immutable
process definition. A value carries a claim: an unknown claim adopts the
derivation, and a known mismatch is refused before durable registration. The
descriptor must also derive the claimed definition id.

Source linking infers and materializes omitted process output annotations.
`ModuleArtifact::from_program` accepts complete IR and refuses a process without
an explicit output; it does not invent `any`. Parameter names, order, types,
and outputs participate in executable module and process-reference identity.
Artifact construction and store decoding refuse an unlifted process literal
at any expression root. Only linked, lifted process declarations are admitted.
The definition id excludes the signature claim because the engine derives it,
as specified by
[ADR 0095](0095-processes-are-values-and-process-controls-are-tools.md).

Evidence: `crates/lash-vm/src/ast.rs:219`, `:1252`, `:1354`,
`crates/lash-vm/src/linker/pass_validation.rs:4`, `:415`,
`crates/lash-vm/src/artifact.rs:273`, `:334`, `:1053`, and
`crates/lash-core-execution/src/runtime/process/definition.rs:417`.

## Alternatives considered

A flattened input plus an independent count loses parameter names and permits
contradictory invocation shapes. An invented `any` parameter asserts a name and
arity the host cannot verify. One signature and an opaque unknown type avoid
those contradictions.

## Consequences

- Zero, scalar, outer-named object, and multi-parameter callables use one type.
- Host schemas can describe a callable without fabricating a shape.
- A forged claim cannot authorize registration of a different executable.

[ADR 0137](0137-the-host-owns-events-routing-and-scheduling.md) owns host events, routing and scheduling.
