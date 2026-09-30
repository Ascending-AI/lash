# 0091: One lowering walk owns expression semantics

Status: Accepted

## Context

Validation, inferred outputs, completion facts, and workflow type facets need
the same expression and lexical-scope semantics. Independent recursive walks
can disagree about operands, shadowing, and branch visibility.

## Decision

The linker's lowering pass owns recursive structural dispatch and lexical
scope for `Expr`. Every successful lowering returns one total `Binding`; an
empty block has the `any` binding. Optional bindings represent lookups and
save/restore operations, not successful expression results.

Completion, expected-type, and workflow observations attach to lowering entry
and exit. Facts use the expression's `AstPath` in the program. Observers consume
that walk rather than implement another expression dispatcher or scope table.

Each variant lowers in a bounded method, and dispatch arms return its `Result`
directly. Combining variants or result temporaries into one recursive frame
exceeds the host's 2 MiB stack budget at admitted nesting depth.

`try` body and catch scopes each start from the pre-`try` scope. The catch
binder hides and restores its outer binding. Body and catch results join before
`finally`; a body assignment cannot define the catch's incoming scope. A `try`
has the `any` binding while all paths lower for validation, scope effects, and
completion. Index expressions lower both target and index operands.

Trigger subscription-key materialization belongs to runtime registration.
Lowering validates supplied keys; the runtime derives an absent key from the
process and trigger source. Scope stores no static trigger facts.

Evidence: `crates/lashlang/src/linker/lower_expr.rs:4`, `:19`, `:60`, `:206`,
`:1251`, `:1269`. Trigger-key validation and derivation live in
`crates/lashlang/src/linker/pass_validation.rs` and
`crates/lash-core-execution/src/triggers/router.rs:299` and
`crates/lash-lashlang-runtime/src/trigger_commands.rs:511`.

## Alternatives considered

A separate observer walk duplicates structural and scope semantics. Entry and
exit observations attach facts to validated lowering. One large recursive
method increases stack cost at every nesting level; variant methods preserve
the stack bound.

## Consequences

- Diagnostics, inferred outputs, completion facts, and facets share lowering.
- Expression variants have one structural implementation point.
- Durable trigger keys depend on materialized registration inputs.
