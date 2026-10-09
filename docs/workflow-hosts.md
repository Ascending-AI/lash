# Editing, publishing and showing workflows

A host that lets people build workflows does four things with one: read it,
change it, publish the change as something that can run, and show what a run
did. `lash::workflow` is the whole surface for all four. The workflow is a
kernel document; the host never handles bytecode, and never has to print or
parse a source language to do any of this.

[`examples/workflow-graph-roundtrip`](../examples/workflow-graph-roundtrip/README.md)
is a host built this way, and the code below is the shape of its backend.

## The document

A `lash::workflow::document::Document` is one program. It holds a `main`
block, the functions it declares, the entries (the declared functions a host
may start, each with a signature), and a manifest: the kernel version, the
effects the code performs with the signature it expects of each, every
library function the code reaches by identity, and how an effect result's
bare number decodes.

A block is an ordered list of statements: `let`, `set`, `remove`, `do`,
`if`, `for`, `while`, `try`, `throw`, `return`, `break`, `continue`,
`finish`. A statement holds expressions, blocks and at most one action. An
action is the right-hand side that does something: `call` a declared
function, `apply` a function value, `invoke` a library function with a
kernel body, `perform` an effect, `sleep`, `spawn`, `join`, `yield` or
`cancel`. Actions take atoms (a variable or a literal) as arguments, so the
value an action receives is always bound by a statement before it.

The document is total and dialect-free. Nothing in it is source text, and
it names no source language. It serializes as JSON (`serde`), and
`parse_document` and `print_document` read and write the same document in
kernel notation:

```text
kernel 1
numbers float
effect ledger.record(input: Any) -> Any
entry audit(summary: Any) -> Any

fn audit(summary) {
  let note = {entry: summary.status}
  do perform ledger.record(note) as Any
  return summary
}

main {
  finish null
}
```

### Sites

Every node of a document has a `Site`: the unit it is in (`main`, a declared
function, or a library function) and the path of child positions from that
unit's body, counted in `Node::children` order. A block's children are its
statements; a statement's are its expressions, its action and its blocks, in
the order kernel text writes them. Sites are how you address a node to edit,
where a diagnostic points, and what a run reports against. A site belongs to
one document: an edit moves the nodes after it, so never carry a site from
one document to another without a correspondence.

### What is derived

`lash::workflow::WorkflowDocument` pairs a document with the entry a
definition starts and with what lash derives from the two:

| Read | Answers |
| --- | --- |
| `document()` | The kernel document. |
| `reference()`, `identity()` | The content identity of the document, and the entry (`Main` or `Entry { function }`). |
| `entry_unit()`, `entry_signature()` | The unit a run starts in, and its signature. |
| `graph()` | The derived graph (`lash::workflow::graph::Graph`): a node per block, statement, action and expression, each at its site; control-flow, call, spawn and reference edges; and `execution_sites()`, the sites a run can report an occurrence at, with their kind and enclosing loops. |
| `overlay_document()` | What an execution overlay is held to. |

Read the graph; do not write it. It is recomputed from the document, and
there is no edit that writes an edge.

A document carries no labels and no presentation. The kernel's annotation
layer (labels and host data per node) lives beside a draft, not in the
document lash stores. Wording, icons and layout are the host's.

### Reading one

```rust,ignore
// The workflow a retained process runs.
let read = core.processes().graph(&process_id).await?;
// The workflow of a published definition.
let read = core.host_artifacts().definition_graph(&definition_id).await?;

match read {
    WorkflowRead::Inspected(inspection) => {
        let document = inspection.document.document();   // the kernel document
        let entry = &inspection.document.reference().entry;
    }
    WorkflowRead::Unavailable(what) => { /* nothing retains it any more */ }
    WorkflowRead::Unsupported { engine_kind } => { /* the engine has no document */ }
}
```

### Writing one

A generated workflow needs no source text at any point: build the
`Document`, or write kernel notation and `parse_document` it. To be admitted
it must be written against the environment it will run under:

```rust,ignore
let environment = core
    .host_artifacts()
    .workflow_environment(&env_spec)
    .await?
    .expect("an engine that reads workflow documents");

environment.effects();     // each effect a document may perform, with its signature
environment.functions();   // the library functions, by identity
```

- Each effect in the manifest carries the signature the environment offers
  it under. A tool `ledger_record` bound to `ledger.record` is the effect
  `ledger.record`.
- A library function is called by identity (`use num.lt = @<hash>` in
  kernel notation). Look the identity up by the name its definition carries.
- The manifest lists every library function the code reaches, directly or
  through another function's body.
  `lash::workflow::graph::requirements(&document, environment.functions())`
  derives that list from the code.

Starting another entry of the same document as a process of its own is two
effects: `processes.start` with `{definition: &worker, args}`, where
`&worker` is a function reference to the entry, then `processes.await` with
`{handle}`.

## Editing

Open the document as a `Draft` and apply kernel edits to it.

```rust,ignore
use lash::workflow::edit::{Draft, Edit, Position, Transaction};

let mut draft = Draft::open(document, None)?;

let applied = draft.apply(
    &Transaction {
        base: draft.identity(),
        edits: vec![
            Edit::ReplaceExpression { expression: threshold_site, with: new_expr },
            Edit::InsertStatement { at: Position::end(try_body_site), statement },
            Edit::CloneStatement { statement: record_site, to: Position::end(loop_body_site) },
        ],
    },
    &environment.checker(),
)?;
```

Every site in a transaction is a site of its `base`, the document the draft
held when the transaction was written; an edit never has to account for the
edits before it. A transaction applies whole or not at all. The result is
checked against the environment before the draft takes it, so a draft never
holds a document its last transaction broke. A refused transaction leaves
the draft unchanged and answers an `EditRefusal` whose diagnostics each name
the edit or the site they are about.

`Edit` has an edit for every part of the document:

| To change | Use |
| --- | --- |
| Which statements a block holds, and their order | `InsertStatement`, `CloneStatement`, `RemoveStatement`, `MoveStatement` |
| A whole statement | `ReplaceStatement` |
| Any expression, at any depth | `ReplaceExpression` |
| An action, or one of its arguments | `ReplaceAction`, `SetArgument` |
| A condition, a `try`'s clauses | `SetCondition`, `SetCatch`, `SetFinally` |
| The name of a variable everywhere it resolves | `RenameVariable` |
| Declared functions | `InsertFunction`, `ReplaceFunction`, `RemoveFunction`, `RenameFunction` |
| Entries | `InsertEntry`, `RemoveEntry`, `RenameEntry`, `SetEntrySignature` |
| The manifest | `SetEffectSignature`, `SetNumberPolicy`, `ReplaceFunctionIdentity` |
| `main`'s own variables | `SetPrivateBindings` |
| The annotation layer beside the draft | `SetLabel`, `SetData` |

`Edit`, `Transaction` and `Correspondence` are serializable, so a client can
send a transaction as JSON and get the correspondence back.

Statements, expressions and actions in an edit are kernel AST values. A host
that takes text from a person can accept kernel notation and parse it; a
form builds the value directly.

## Identity through edits

| Identity | What it is | How long it lasts |
| --- | --- | --- |
| `Site` | Where a node sits in one document | One document. An insertion moves the statements after it. |
| `DocumentId` | The content identity of a document | Forever; equal content is the same id. |
| The definition id | The published definition | Forever; equal content and entry is the same id. |

Never match nodes across two documents by site. Each applied transaction
answers a `Correspondence` from its base to the result: one `Survivor` per
node of the base that is still there, with where it was (`from`), where it
is (`to`), and whether an edit wrote it (`edited`). A node with no survivor
was removed. A node of the result that no survivor ends at is new: authored
by an edit, or a clone. `successor(&site)` and `predecessor(&site)` walk it
in either direction, and `then` composes two.

Key anything you keep per node (canvas position, comments, selection) by
site within one document, and move it through the correspondence when the
document changes. `Draft::correspondence_since_open` covers every
transaction since the draft was opened.

A wholesale replacement has no edit history. If you replace the document,
open a new draft: nothing corresponds to the old one unless your host
decides it does.

## Publishing

Publishing admits the draft's document as a definition of one entry.

```rust,ignore
let pin = HostArtifactPin::mint();
match core
    .host_artifacts()
    .publish_workflow(&pin, &draft, &Name::new("order_review"), &env_spec)
    .await?
{
    WorkflowPublish::Published(publication) => {
        let definition = publication.definition;   // id and signature: what a run starts
        let admitted = publication.document;       // the WorkflowDocument, with its graph
        let moved = publication.correspondence;    // the draft's base sites to admitted sites
    }
    WorkflowPublish::Refused(refusal) => { /* why admission refused it */ }
    WorkflowPublish::Unsupported { .. } => { /* no engine admits documents */ }
}
```

Lash checks the document against `env_spec`, the environment a process of
the definition would run under, including the tool catalogue its plugins
resolve, and publishes the definition of the named entry under your pin.
Every other entry of the document is reachable from it by function
reference, so one publication is all a run needs.

- **Nothing the document says about itself is trusted.** The manifest is
  checked against the code and the environment; the graph and the sites are
  derived again.
- **A refusal publishes nothing.** `WorkflowAdmissionRefusal` says what
  admission refused: an effect the environment does not offer or offers
  under another signature, a library function the manifest does not list, a
  name that does not resolve, the entry.
- **A definition is immutable.** An edit publishes a new one. Processes
  already started keep the definition they were admitted under.
- **Equal content is the same definition.** Publishing an unchanged document
  answers the definition it was read from.
- **The pin is what retains it.** Hold the pin for as long as you may start
  the definition, and release it (`host_artifacts().release(pin)`) when you
  will not. A started process keeps what it needs without the pin.

The admitted document is the draft's document: publication does not rewrite
it. A run reports against its sites.

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

An occurrence is identified by an `EffectIdentity`: the task (`Main`, or a
spawned task), the site, the occurrence of that site in that task counted
from 0, and the iteration of each enclosing loop. The overlay lists the task
sites that were observed, one row per `(site, task)`, each with the state of
its latest occurrence (running, waiting, completed, failed, cancelled, or
incomplete because the process ended first), counts of starts and ends, and
the call the occurrence is bound to. Tasks a fan-out spawns at one site are
separate rows.

The kernel engine reports effect performs and sleeps. It reports no branch
choice: which arm an `if` took is visible as the sites that ran inside it.
A site the overlay does not list was not observed; that is not evidence it
did not run when the overlay's coverage says the start of the execution was
missed.

Draw a statement from the row of its action's site; take its wording, kind
and nesting from the document.

Follow one feed per process. It recovers after a gap, replays what it
retains to a late subscriber, and delivers the committed facts that settle
the process. Do not poll for status beside it, and do not resubscribe when
the process ends. [Observing processes](observing-processes.md) covers the
feed, the durable facts and what the overlay does when evidence is missing.

A run's observations belong to the document it ran. After an edit, a new
run reports against the new document. Do not paint an old run onto an
edited document; if you compare revisions, use the correspondence.

## TypeScript is a lens

None of the above prints or parses a source language. With the `typescript`
feature, `WorkflowDocument::typescript()` (the kernel printer,
`lash::typescript::print`) answers equivalent TypeScript for a document,
keeping every temporary and statement boundary. It is a read-only view.

A document can have no TypeScript spelling. The printer then answers a
diagnostic, and the document stays readable, editable, publishable and
runnable.

The TypeScript front end lowers source to a kernel document inside a
session, where cells are written. A host-callable lowering of a whole
workflow source, with process literals lowered to entries, follows; until
then a host that wants text entry takes kernel notation. "Open in source"
is a convenience and must never be the only way your host can change a
workflow.

## Namespaces

| Namespace | For |
| --- | --- |
| `lash::workflow` | Publication, reads, the derived `WorkflowDocument`, the environment, and the execution overlay. Ordinary host work stays here. |
| `lash::workflow::document` | The kernel document: the AST, sites, names, identities, kernel notation. |
| `lash::workflow::graph` | The derived graph, execution sites and manifest requirements. |
| `lash::workflow::edit` | Drafts, edits, transactions and correspondence. |
| `lash::typescript` | The optional TypeScript dialect: its printer. |
