# 0091: One lowering walk owns expression semantics

Status: Accepted

## Context

Lashlang previously interpreted an expression through several independent
recursive walks. Lowering, inferred bindings, workflow type facets, completion
facts, and compiler-generated trigger subscription keys each encoded their own
variant dispatch or lexical scope rules. Those copies had already diverged:
JavaScript operators and maps could report different types to the linker and
editor, index and unary operands could escape validation, and the trigger-key
collector treated a `try` body assignment as visible in its catch path even
though normal lowering does not.

That last mismatch changed durable identity. Given an outer static trigger
source, a different source assignment in a `try` body, and a registration in
the catch path, the collector selected the body source. The catch path actually
starts from the pre-`try` scope, so the generated subscription key had been
derived from a source that was not visible on that path.

## Decision

The linker's lowering pass is the sole recursive structural dispatcher and
lexical-scope authority for `Expr`. Every expression lowers to one total
`Binding`; an empty block has the `any` binding. Optional bindings remain only
for lookups and save/restore operations.

Completion and workflow-facet observations attach to lowering entry and exit.
They may record facts or best-effort diagnostics, but do not recurse or maintain
a second scope table. Workflow facets retain their AST-pointer association.
The existing bounded lowering methods remain separate so the 2 MiB stack-budget
mechanism does not acquire one large recursive frame.

Static trigger facts live beside ordinary bindings in the same `Scope`. Binding
or restoring a name changes both facts together. Branch joins retain a static
fact only when it is identical on every reachable path. Comprehension binders
hide and later restore an outer fact. A `try` body and catch each start from the
pre-`try` scope, their results join before `finally`, and the body cannot leak a
source into the catch. Default-trigger analysis consumes these lowering facts;
a separate non-scoping postorder pass only writes already-derived keys into the
lowered tree. If lowering cannot prove one static source and target at the
registration site, compilation refuses the derived key rather than selecting
another path's value.

The shared result rules are `bool` for JavaScript comparisons, `bool`, `str`,
or `float` for the supported JavaScript unary operators, and `any` for maps and
other JavaScript binary operators. Index and unary expressions always lower
their operands.

The corrected `try` scope is an accepted canonical-output change: the same
accepted program AST now generates its catch-path subscription key from the
outer source that is actually visible. The Lashlang semantic hash therefore
advances from v7 to v8. A v7 module is not reinterpreted as v8; deployments that
require the current generation must refuse it and recompile and republish its
source, recomputing compiler-generated registrations. The parser and AST,
bytecode, continuation format, and VM ABI do not change.

## Consequences

- Linker diagnostics, inferred process outputs, completion facts, and workflow
  facets observe the same expression traversal and binding result.
- New expression variants or scope forms have one structural implementation
  point. Observers can add facts without becoming traversal authorities.
- Generated trigger keys follow the scope of the execution path that registers
  them. Shadowed or path-dependent sources are rejected when no single static
  key can be proven.
- All identities rooted in the Lashlang semantic hash move to v8 even when a
  particular module does not contain the corrected `try` shape.
