# ADR 0037: Lashlang workflows use a code-graph-code lens

## Status

Accepted.

Amended by [ADR 0100](0100-the-run-observation-contract.md): expression-valued
graph fields carry authoritative IR, dialects only parse and print them, and
the projector lives in the IR crate with dialect print and parse operations
injected.

Amended 2026-09-13 (FIG-2990): under [ADR 0095](0095-processes-are-values-and-process-controls-are-tools.md) starting a process is an ordinary
call node rather than a dedicated start effect kind, and a process literal in
an argument projects as a process container node. The lens laws are unchanged;
the workflow graph schema advances 8 to 9 and joins the durable-format
manifest.

Amended 2026-09-13 (FIG-3016): [ADR 0096](0096-typescript-is-the-sole-rlm-dialect.md) retires the Lashlang
surface, so "Lashlang" below names the IR and VM. The lens laws are unchanged
and operate on the IR and the graph; a TypeScript canonical printer with the
same exact-fixpoint round trip remains unbuilt and is separate future work with
its own ADR.

## Context

Hosts need to visualize and structurally edit Lashlang workflows and correlate
live execution with the displayed workflow. The language previously exposed a
lossy, read-only visualization map. That map had no inverse, so it could not be
an authoring contract, and it duplicated concerns that belong to the host such
as presentation and layout.

Lashlang source and a JSON graph optimize for different authors. Source is
compact, expressive, reviewable, diffable, and already has a complete parser
and canonical printer. A graph document is easier for a visual host to mutate
structurally and annotate out of band. Treating either form as universally
superior would discard the strengths of the other.

## Decision

Lash provides a pure source → graph → source lens. `WorkflowGraph` is the
single serializable graph model for visualization, editing, and live-run
overlays. Projection parses source and operates on the AST; rendering
reconstructs an AST and delegates all text generation to the existing canonical
printer. It does not compile or link the workflow.

Every expression-valued field owned by a structured node is stored as
serialized Lashlang IR. This includes assignment expressions and targets,
computation expressions and bindings, `if` and `while` conditions, `for`
iterables, and list-comprehension iterable and filter clauses. Call nodes store
their receiver and positional or named arguments separately. Effect nodes store
their exact effect kind and the same structured IR argument list. Their slot
paths are serialized lists of typed `call`, `arg`, `field`, and `index`
segments. Nodes with several nested receiver calls add a `call` segment using
depth-first IR walk order. A text-only host may derive paths such as
`call[1].arg[0]["a.b"][0]`; field segments use JSON string quoting, so dots,
brackets, quotes, and empty field names cannot collide with structural
segments. Rendering consumes the IR directly and parses no expression text.

Facet diagnostics carry an optional slot path, a closed diagnostic kind, and a
serialized `definite` or `advisory` class. `definite` diagnostics block a save
under ADR 0073. A diagnostic carries a slot only when its exact AST path
matches a projected slot. `TypeExpr` is closed: a host refuses an unknown
variant or an unknown field inside a variant payload after accepting the graph
or facet carrier version.

Canonical text belongs to a dialect and is derived, non-authoritative output.
A host text edit enters through that dialect's existing fragment parser, which
returns replacement IR for the selected field. The graph never stores that
text beside the IR. Opaque statement nodes remain statement text because the
lens has not decomposed them into an expression-valued field.

The lens laws are:

1. On canonical source, source → graph → source is an exact textual fixpoint.
2. Parsing the rendered source yields the same span-insensitive AST as the
   canonical input.
3. A graph produced by projection is recovered exactly after graph → source →
   graph (PutGet).

Authored formatting and comments are outside these laws because this is not a
lossless CST. `@label` is semantic graph metadata and does survive. Its
TypeScript spelling is a one-line JSDoc comment on the statement it names —
`/** @label Title — Description */`, the em-dash separating the two — so it is
the one comment the lens reads and writes; every other comment is trivia and is
dropped by canonicalization. Rendering a
structurally invalid graph returns a typed error; Lash refuses to emit source
that it cannot prove parses rather than presenting a plausible but false view.

The derived projection is n8n-shaped:

- calls, effects, terminals, pure value-shaping data, reference-bearing type
  literals, state updates, and sequenced computations are typed nodes;
- `if`, `for`, `while`, list comprehension, and named process are containers
  with child subgraphs;
- data-dependency edges carry SSA-like variable versions, while explicit
  sequencing edges preserve the source order of effects;
- `is_pure_expr` is the shared discriminator for the ordinary data subset,
  while source-valid `TypeLiteral` values remain typed data even when their
  references require runtime resolution;
- a computation retains one complete canonical effectful expression, preserving its
  evaluation order, short-circuit behavior, unwrap timing, and canonical text
  instead of splitting operands into independently scheduled nodes;
- `for` and `while` summarize body writes as one post-loop version per visible
  root, including nested-path writes and variables first introduced in a loop;
  loop and comprehension bindings shadow outer dependency state and do not
  leak from their scope;
- opaque fallback is local to one statement and is reserved for a genuinely
  unisolable construct. A projectable `if`, `for`, `while`, reassignment, type
  literal, or effectful expression never makes its enclosing region opaque.

An explicit `@label` supplies the node name and description. Otherwise the
projection derives a name from the operation or construct. Node ids are hashes
of canonical source, owner, AST path, and node kind, making repeated projection
deterministic. Draft identity remains a host sidecar concern. An `@id`
annotation is reserved as a future hook; it is not implemented because adding
it to the language and excluding it from semantic hashing is not required for
the deterministic-id path.

Runtime execution sites carry a thin source owner/path descriptor. Lash exposes
a helper that joins that descriptor to a workflow node id, allowing a host to
answer “node N of process P did X” without adding graph policy to `TurnEvent`.

Layout is deliberately out of band. Lash owns the conversions, graph type,
deterministic identity, validation, and run correlation. The host owns drafts,
mutation commands, versioning, layout, rendering, and interaction policy.

## Consequences

Hosts can choose code authoring, graph authoring, or both without maintaining a
second language printer or a private inverse. Canonical reformatting is visible
after graph rendering, and comments are not preserved. Effectful composites
retain their source semantics without sacrificing typed structure. Any future
opaque fallback remains a single editable statement, so surrounding control
flow and dependencies stay visible. The execution-trace graph remains a
separate trace-derived runtime view and may consume the workflow graph as its
static skeleton without becoming the authoring model.
