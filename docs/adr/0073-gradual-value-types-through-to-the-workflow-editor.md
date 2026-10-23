# 0073: Gradual value types through to the workflow editor

## Context

A workflow editor needs types for bindings, value slots and diagnostics. The kernel Document is authoritative; source and graph are ways to author or view it, and facets never become a second program.

## Decision

Value types are derived, read-only facts from the kernel checker and the host's explicit operation contracts. They apply equally to captured values materialized as declarations and other document content.

### D1: Type diagnostics and draft policy

Draft::open permits a document that is not admitted in the current environment. Draft::apply admits a whole transaction atomically or leaves the draft unchanged. The host owns draft storage. A published definition supplies its own runtime identity; source publication independently applies current source policy, entry checking and fulfilment. Runtime argument/result validation remains separate from editor facets.

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

Recomputing facets is not an edit. Print/import laws compare authoritative content and behavior under the stated equivalence relation, never facet caches.

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

- `crates/lash-kernel-check/src/graph.rs`, `link.rs`, `workflow.rs`.
- `crates/lash-kernel-edit/src/draft.rs` and `crates/lash-kernel-view`.

[ADR 0137](0137-the-host-owns-events-routing-and-scheduling.md) owns host events, routing and scheduling.
