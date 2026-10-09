# Lash VM workflows use a code-graph-code lens

## Status

Accepted.

## Context

Hosts need to visualize and structurally edit workflows and correlate a run
with the program it executes. TypeScript is the source dialect (ADR 0096);
Lash VM names the language-neutral IR and VM. Source supports review and
diffing, while a graph supports structural editing. Hosts own presentation
and layout.

## Decision

`WorkflowGraph` is the serializable graph model for editing and run overlays.
It is a total typed document of the IR: the IR crate projects a program to it
and reconstructs the program from it exactly, with no dialect involved. The
source lens parses and canonicalizes TypeScript and prints a reconstructed
program through the TypeScript canonical printer; it is an optional view of
the document, not its validator. Projection and rendering do not execute the
workflow.

Structured expression fields carry authoritative serialized IR, including
bindings, assignment targets, conditions, loop iterables and call receivers
and arguments. Rendering consumes that IR rather than parsing an adjacent text
copy. A text edit uses the dialect's fragment parser to produce replacement
IR. No node carries source text: `try`, `throw` and a nested scope are typed
regions like every other admitted construct, and the document stamps the IR
interpretation version it was written under.

Argument slot paths are typed sequences of `call`, `arg`, `field` and `index`
segments. Type facets and diagnostics are derived facts; they do not select
execution semantics. Diagnostics carry an exact slot when it corresponds to a
projected expression. Graph and facet carriers have separate version contracts
under ADR 0100. Hosts decide whether a draft with diagnostics may be saved.

The lens laws are:

1. Canonical source reaches an exact textual fixpoint through source, graph
   and source.
2. Parsing rendered source preserves the span-insensitive IR.
3. Projecting a rendered projected graph recovers that graph, the PutGet law.

Formatting and ordinary comments are outside these laws. Semantic labels
survive through statement JSDoc `@label` comments. Invalid structure is a
typed document error from the IR crate. A valid document without a canonical
TypeScript spelling is a typed lens refusal rather than plausible source, and
stays a valid document.

Calls, effects, terminal values, data expressions, updates and computations
are typed nodes. Control flow and process bodies have nested subgraphs.
Dependency edges track variable versions; sequencing edges preserve effect
order. A composite computation retains one ordered expression, including
short-circuit and unwrap behavior, rather than scheduling its operands as
independent nodes. Loop projections summarize writes and preserve lexical
shadowing. A statement with no narrower kind is a computation that carries its
whole expression.

A draft projected from unadmitted source claims no runtime identity.
`workflow_graph_from_artifact` projects the admitted artifact's own IR and
carries its `source_identity`. Node identity derives from structural owner
and owner-relative AST path. Runtime execution sites use the same identity.
Artifact identity belongs to the graph and execution, not the node's preimage;
a lifted process's owner includes its body digest. ADR 0100 governs admitted
source and run correlation.

## Consequences

Code and graph authoring share one IR and one canonical printer. Rendering
exposes canonical formatting and does not preserve ordinary comment trivia.
Hosts own draft identity, mutation commands, document versions, layout and
interaction. The trace-derived execution graph can use the workflow graph
without becoming the authoring model.

A read-only lossy map is rejected because it has no authoring inverse. Storing
both expression text and IR is rejected because it adds competing authority.
Splitting effectful operands into scheduled nodes is rejected because it can
change evaluation order.

## Implementation

- [Graph types and structural identity](../../crates/lash-vm/src/workflow_graph.rs) and [IR projector](../../crates/lash-vm/src/workflow_graph/projection.rs).
- [TypeScript lens and source view](../../crates/lash-typescript/src/workflow_graph/mod.rs).
- [Editable-field and round-trip laws](../../crates/lash-typescript/tests/workflow_graph/adr_claims.rs).
