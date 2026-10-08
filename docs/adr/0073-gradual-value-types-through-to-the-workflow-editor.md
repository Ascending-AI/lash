# 0073: Gradual value types through to the workflow editor

## Context

A workflow editor needs types for available variables, argument slots, and
local diagnostics. The source and resolved IR remain authoritative; editor
annotations must not become a second language representation.

## Decision

Value types are derived, read-only workflow facets. The TypeScript projection
and Lashlang linker share the host operation contracts and resolved IR under
ADRs 0037, 0091, and 0100.

### D1: Type diagnostics and draft policy

Lash reports link and type diagnostics. The host editor decides whether to
save a workflow as a draft. A projection of admitted source uses the admitted
artifact's resolved IR and paths, including lifted process declarations.
A draft projection carries diagnostics and has no admitted artifact identity.
Runtime tool-schema validation enforces tool boundaries independently of
editor facets.

### D2: `Any` is consistent

`Any` is assignable in either position. A known value can fill an unknown slot,
and an unknown value can fill a known slot. Known incompatible types can be
rejected. `workflow_slot_accepts_value(value_type, expected_slot_type)` names
the source-to-target direction explicitly and uses the shared assignability
rule.

This gradual rule admits incomplete host information. It does not establish
runtime soundness at `Any`; runtime validation owns that guarantee.

### D3: Tool-schema import and bounded typed outputs

The runtime bridge preserves each tool's input and output schemas in an
`OperationContract`. The host catalog imports them into `TypeExpr` through the
shared schema importer. Unsupported schema shapes degrade to `Dict` or `Any`.
Host-declared signatures are trusted static facts.

For `ToolOutputContract::FromInputSchema`, the bridge preserves `input_field`
and `default_schema`. Linker-only schema witnesses let closed descriptors
supply an output type. The rule follows the operation contract, rather than a
tool name. A descriptor whose shape depends on a runtime value cannot promise
a closed static result. A declared default supplies the fallback; otherwise
the result is `Any`.

### D4: Local inference and bounded loops

The linker synthesizes local value types and records expected argument types.
Scope joins combine branch bindings, and loop widening joins the entry scope
with one analyzed pass. There is no fixpoint loop. Assignment paths update
binding types. Process and host operation signatures participate in local
inference under ADR 0090.

TypeScript coercion and operand semantics follow ADR 0064. Facets do not impose
a separate strict binary-operand rule on JavaScript programs.

### D5: Type facets in the graph contract

Each node can carry `WorkflowNodeTypeFacets` with typed available variables,
expected argument slots, and diagnostics. Argument slots have structured paths.
Facets describe the same IR node paths as the graph and run observations.

The facet schema has its own version. Decoding drops facets stamped with a
different facet version while retaining the authoritative graph. Projection
recomputes facets. Source printing and semantic graph comparison do not treat
them as edits, so the code-to-graph-to-code lens retains its laws.

## Alternatives considered

Strict unknown types would reject programs for which host information is
incomplete. Gradual `Any` leaves enforcement at runtime boundaries instead.
Authoritative editor types would create another representation to reconcile
with source. Derived facets avoid that conflict. Global inference and general
dependent typing add obligations beyond the local contract and closed schema
witnesses this design uses.

## Consequences

Editors can filter slot choices and display local diagnostics using host-derived
facts. Missing information degrades precision. A facet cannot authorize execution
or identify an admitted artifact by itself.

## Code references

- `crates/lashlang/src/linker/` defines assignability.
- `crates/lash-lashlang-runtime/src/lib.rs:492-534` preserves tool contracts.
- `crates/lashlang/src/linker/catalog.rs:60-82` imports operation schemas.
- `crates/lashlang/src/linker/pass_setup.rs:330-379` resolves closed schema witnesses.
- `crates/lashlang/src/linker/type_helpers.rs:61-98` joins and widens local bindings.
- `crates/lash-typescript/src/workflow_graph/mod.rs:44-114` distinguishes draft and artifact projections.
- `crates/lashlang/src/workflow_graph/facets.rs:12-45,389-430` defines and derives facets.
- `crates/lashlang/src/workflow_graph.rs:174-184` discards incompatible facet data.

[ADR 0137](0137-the-host-owns-events-routing-and-scheduling.md) owns host events, routing and scheduling.
