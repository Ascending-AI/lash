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
Installed source dialects lower public signatures to the same kernel entry contract.

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

Promoting source to a workflow entry requires written annotations for every public parameter and the settled result. FunctionExport.signature and a saved FunctionValue's public signature are checked against their callable presentation and convention. Opening a saved callable preserves that contract in the initial editable document; the host may rename parameters or change the contract before publishing a new independent definition. B's own entry is checked afresh. No rule requires its signature or adapter to match A. Definition identity is derived from the engine-owned descriptor, and signature claims cannot authorize different code.

Evidence: `crates/lash-kernel-doc/src/function_value.rs`,
`crates/lash-kernel-check/src/workflow.rs` and
`crates/lash-kernel-vm/src/machine/export_abi.rs`.

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
