# Editing, publishing and showing workflows

A host that lets people build workflows does four things with one: read it,
change it, publish the change as something that can run, and show what a run
did. `lash::workflow` is the whole surface for all four. The workflow is a
typed document; the host never handles bytecode, and never has to print or
parse a source language to do any of this.

[`examples/workflow-graph-roundtrip`](../examples/workflow-graph-roundtrip/README.md)
is a host built this way, and the code below is the shape of its backend.

## The document

A `WorkflowGraph` is one program as a typed document. `main` and each
process hold an ordered body of nodes. A node is a statement: a call, an
effect, a computation, an assignment, a terminal, a `throw`, or a container
(`if`, `for`, `while`, `try`, a nested block) that owns ordered child bodies.
Declared functions and the program's private bindings are part of the
document too.

The document is total. Every construct a program can contain is a typed node
or a typed expression inside one. Nothing is kept as text, and there is no
"code" node whose content the host cannot reach: a closure, a `try` with its
`catch` and `finally`, a computed assignment target and a process literal are
all structure you can read and change.

Expressions are Lash's own IR (`lash::vm::ir::Expr`). Each node spells one
statement, which `workflow_node_statement(&node)` answers, and every `Expr`
variant names its children as typed slots (`Expr::slots`), in evaluation
order. A place inside a node is therefore a slot path from its statement
(`WorkflowSlotPath`): the `Arg(0)` of the call that is the `Value` of an
assignment, for example. Slot paths are how you address an expression to
edit, how an execution site is named, where a diagnostic points, and which
argument a type facet's expected type is for: one grammar for all four.

Some of what a document carries is derived from the rest: node ids, edges,
the variables in scope at a node, type facets and execution sites.
Read them; do not write them. Lash recomputes them whenever the
document changes, and ignores what a submitted document claims for them.
Source coordinates live only in the optional `SourceView`: look up a node
or diagnostic by its node id in that view's `spans` map.

A document states the version of the IR it was written under
(`WORKFLOW_IR_VERSION`). A document this build does not read is refused when
it is opened, with a typed `WorkflowIrVersionRefusal`. It is never read
partially.

### Reading one

```rust,ignore
// The workflow a retained process runs.
let read = core.processes().graph(&process_id).await?;
// The workflow of a published definition.
let read = core.host_artifacts().definition_graph(&definition_id).await?;

match read {
    WorkflowRead::Inspected(inspection) => {
        let graph = &inspection.document.graph;   // the WorkflowGraph
        let entry = &inspection.document.entry;   // the process the definition starts
    }
    WorkflowRead::Unavailable(what) => { /* nothing retains it any more */ }
    WorkflowRead::Unsupported { engine_kind } => { /* the engine has no document */ }
}
```

You can also build a document yourself, from a `lash::vm::ir::Program` with
`lash::vm::ir::workflow_graph_from_program`. A generated workflow needs no
source text at any point.

## Editing

Open the document as a `WorkflowDraft` and apply typed edits to it.

```rust,ignore
let mut draft = WorkflowDraft::open(&graph)?;
let node = draft.handle(&node_id).expect("a node of the document");

let correspondence = draft.apply(WorkflowEditTransaction {
    base: draft.revision(),
    edits: vec![
        WorkflowEdit::ReplaceExpression {
            node,
            slot: slot_path,          // a WorkflowSlotPath from the node's statement
            expression: new_expr,     // typed IR
        },
        WorkflowEdit::InsertNode {
            body: WorkflowBodyRef::Child { node: region, slot: WorkflowBodySlot::TryBody },
            before: None,             // at the end of the body
            statement,
        },
    ],
})?;
```

A transaction applies whole or not at all. A refused one leaves the draft
unchanged and answers `WorkflowEditDiagnostic`s, each at a node and slot
path, with a stable `code()`.

`WorkflowEdit` has an edit for every authoritative field of the document:

| To change | Use |
| --- | --- |
| Which statements a body holds, and their order | `InsertNode`, `CloneNode`, `RemoveNode`, `MoveNode` |
| A whole statement | `ReplaceNode` |
| Any expression inside a statement, at any depth | `ReplaceExpression` with a slot path |
| What a statement binds, and the name of a variable everywhere it is used | `SetBinding`, `RenameBinding` |
| A condition, a loop's element, a `try`'s clauses | `SetCondition`, `SetLoopBinding`, `SetCatch`, `SetFinally` |
| A label | `SetLabel` |
| Processes | `InsertProcess`, `RemoveProcess`, `RenameProcess`, `SetProcessSignature`, `SetProcessWrapper` |
| Declared functions and private bindings | `InsertFunction`, `ReplaceFunction`, `RemoveFunction`, `SetPrivateBindings` |
| How a body arranges its statements: its completion value and groups | `SetBodyLayout` |

A lifted process (an inline process, shown as a container named by a
digest) is derived from its content. `RenameProcess` and `RemoveProcess`
refuse it with `DerivedProcess`: remove the statement that holds its literal,
or the references to it, and it goes with them. `SetProcessSignature` sets
its authored parameters and keeps the captures that follow them. An edit
that makes a lifted process reference itself is refused the same way.

Two rules follow from the document being a program, not a drawing:

- **Edges are derived.** A sequence edge is the order of a body, and a data
  edge is a use of a binding. There is no edit that writes an edge. When a
  person drags one, ask the draft what the drag means
  (`WorkflowDraft::edit_for_edge_drag`): a sequence drag is a `MoveNode`, a
  data drag puts a use of the producer's binding in a slot of the consumer.
  Repetition is a loop container, never a back edge.
- **Bindings resolve lexically.** A read needs a binder that precedes it in
  scope. The draft checks this once, when the transaction commits, so one
  transaction may remove a producer and reconnect its uses in any order.
  Removing a producer that still has uses is refused
  (`unresolved_binding`).

### A form editor and a generic editor

Most hosts want purpose-built forms for the common nodes (a tool call's
arguments, a condition, a loop) and still need a way to change everything
else. Both are the same edits:

- A form turns into one or two edits: `SetCondition` for an `if`,
  `ReplaceNode` for a call whose arguments changed, `SetBinding` for a
  renamed result.
- A generic structured editor lists a node's statement and its slots
  (`Expr::slots`, recursively) and sends `ReplaceExpression` for whichever
  slot the person replaced. Because every `Expr` variant must name its slots
  to compile, this route reaches every construct that exists, including ones
  added after your host shipped.

A host may show a construct it has no form for as a collapsed "advanced"
card. That is presentation. The card is still an ordinary typed region, and
the generic editor can still open and change it.

## Identity through edits

Three identities are easy to confuse.

| Identity | What it is | How long it lasts |
| --- | --- | --- |
| `WorkflowNodeId` | A node's id inside one document, derived from where the node sits | One document. An insertion renumbers later siblings, and an edit inside a process literal renames its process. |
| `WorkflowDraftHandle` | A node of one draft | As long as edits keep the node: replacing its expression, renaming or moving it keeps the handle; inserting or cloning mints one; removing retires it. |
| The definition id | The published, content-addressed definition | Forever; equal content is the same id. |

Never match nodes across two documents by id or by position. Each applied
transaction answers a `WorkflowCorrespondence`: every node of the document
before and after, each with its outcome.

| Outcome | Meaning |
| --- | --- |
| `Retained { from, to }` | The same node, where it was. Its id may still have changed. |
| `Moved { from, to }` | An edit moved it. |
| `Inserted { to, source }` | New: authored by an edit, cloned from a node, or derived from one by normalization. |
| `Deleted { from }` | Removed. |
| `Split { from, into }` | Normalization spelled one statement as several. |
| `Unmatched { from }` | Its provenance was lost. Nothing says which new node it is, and none is guessed. |

Key anything you keep per node (canvas position, comments, selection) by
draft handle, and move it to the new id through the correspondence.
`WorkflowDraft::correspondence_since_open` covers every transaction since
the draft was opened.

A wholesale replacement has no edit history. If a person rewrites the
workflow as source and you import it, open a new draft: nothing corresponds
to the old one unless your host decides it does.

## Publishing

Publishing admits the draft's document as a definition.

```rust,ignore
let pin = HostArtifactPin::mint();
match core
    .host_artifacts()
    .publish_workflow(&pin, &draft, WorkflowEntry::Sole, &environment)
    .await?
{
    WorkflowPublish::Published(publication) => {
        let definition = publication.definition;      // id and signature: what a run starts
        let admitted = publication.document.graph;    // the document as admitted
        let moved = publication.correspondence;       // opened ids to admitted ids
    }
    WorkflowPublish::Refused(refusal) => { /* diagnostics at node and slot paths */ }
    WorkflowPublish::Unsupported { .. } => { /* no engine admits documents */ }
}
```

Lash reconstructs the document's IR in its VM workers and links it against
`environment`, the environment a process of the definition would run under,
including the tool catalogue its plugins resolve. It then publishes the
module and the definition of the selected entry (`WorkflowEntry::Sole`, or
`WorkflowEntry::Process(id)` when the document exports several) under your
pin. Every other process the document defines, an inline process among
them, is published with it, so one publication is all a run needs.

- **Nothing the document says about itself is trusted.** Ids, types,
  signatures, lifted processes and host requirements are derived again.
- **A reference means the process it names.** An admitted document names a
  lifted process by reference wherever it is used. Admission links that
  reference by identity, never through a variable, so renaming a binding or
  cloning, moving or re-binding a statement cannot change which process a
  reference starts. A lifted process's name is a digest of its content: the
  same process has the same name whether it was linked from source or
  published from an edited document.
- **A refusal publishes nothing.** `WorkflowAdmissionRefusal` lists what the
  linker refused, each at a node and slot path, by kind: a missing host
  operation, an unresolved name, a type, a placement, the entry.
- **A definition is immutable.** An edit publishes a new one. Processes
  already started keep the definition they were admitted under.
- **Equal content is the same definition.** Publishing an unchanged admitted
  document answers the definition it was read from.
- **The pin is what retains it.** Hold the pin for as long as you may start
  the definition, and release it (`host_artifacts().release(pin)`) when you
  will not. A started process keeps what it needs without the pin.

The admitted document can differ from the draft's: admission lifts process
literals into declarations and derives types. A run reports against the
admitted document's ids, so serve those to whatever draws the run, and keep
the draft for the next edit. `publication.correspondence` ends at the
admitted ids, which is how you get from a handle to the id to show.

Draft storage, revision names, undo, collaboration and the decision to start
a run are yours. Lash stores definitions, not drafts.

## Showing a run

The document is the static truth. What one execution did is a
`WorkflowExecutionOverlay` you fold over it from the process's feed. Lash
keeps no second graph, and events carry no labels, kinds or edges.

```rust,ignore
let observed = core.processes().observe(&process_id);
let snapshot = observed.snapshot().await?;
let mut feed = observed.subscribe_and_recover(snapshot.cursor);

// The snapshot names the document the process runs. Read it once.
let ProcessReadView::Retained(view) = &snapshot.read_view else { /* gone */ };
let ProcessDocumentIdentity::Available(reference) = &view.document else { /* none */ };
let WorkflowDocumentRead::Read(document) =
    core.host_artifacts().execution_document(reference).await? else { /* not retained */ };

let mut overlay = WorkflowExecutionOverlayAccumulator::default();
overlay.set_document(document.overlay_document());

while let Some(item) = feed.next().await {
    match item? {
        ProcessObservationStreamItem::Event(event) => match &event.payload {
            ProcessObservationEventPayload::LanguageExecution(o) => overlay.observe(o)?,
            ProcessObservationEventPayload::StepBodyStarted(o) => overlay.step_body_started(o)?,
            ProcessObservationEventPayload::Committed { event } => { /* a terminal: overlay.settle(..) */ }
        },
        ProcessObservationStreamItem::Gap { .. } => overlay.reset_live(),
    }
    let view = overlay.snapshot();   // what to draw now
}
```

The overlay lists the execution sites that were observed, each with the
state of its latest occurrence (running, waiting, completed, failed,
cancelled, or incomplete because the process ended first), a count of
starts and ends, the branch arm a branch took, and the call an occurrence is
bound to. A site is `(node_id, site_path)`: two calls in one statement are
two sites of one node. Draw a node from its sites; read its label, kind and
the arms of its branch from the document.

Follow one feed per process. It recovers after a gap, replays what it
retains to a late subscriber, and delivers the committed facts that settle
the process. Do not poll for status beside it, and do not resubscribe when
the process ends. [Observing processes](observing-processes.md) covers the
feed, the durable facts and what the overlay does when evidence is missing.

A run's observations belong to the definition it ran. After an edit, a new
run reports against the new definition's document. Do not paint an old run
onto an edited document; if you compare revisions, use the correspondence.

## TypeScript is a lens

None of the above prints or parses a source language. TypeScript is an
optional view over the same document (`lash::typescript::workflow_graph`,
with the `typescript` feature):

- **Import.** `workflow_graph_from_source(source)` lowers TypeScript to a
  document, which a draft opens like any other.
- **Export.** `source_view(&graph)` (or `WorkflowInspection::source_view`)
  answers canonical TypeScript and a span per node. Comments and original
  formatting are not part of the document; keep the authored text yourself
  if you need them.
- **Free-text fields.** `parse_typescript_expression` turns the text a
  person typed into a field into an `Expr` for a `ReplaceExpression`.

A valid program can have no TypeScript spelling. `source_view` then answers
a typed refusal, and the document stays readable, editable, publishable and
runnable. "Open in source" is a convenience; it must never be the only way
your host can change a workflow.

## Namespaces

| Namespace | For |
| --- | --- |
| `lash::workflow` | The document, drafts and edits, correspondence, publication, reads, and the execution overlay. Ordinary host work stays here. |
| `lash::vm::ir` | The IR the document is made of: `Expr`, slots, declarations, types, and the projection from a `Program`. |
| `lash::typescript::workflow_graph` | The optional TypeScript lens. |
| `lash::vm` | Integrating the VM and its workers. A workflow host does not need it. |
