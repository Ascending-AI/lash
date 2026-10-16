# Editing, publishing and showing workflows

Recovery and external-effect guarantees follow [What survives a crash](../README.md#what-survives-a-crash).

A host that lets people build workflows does four things with one: read it,
change it, publish the change as something that can run, and show what a run
did. `lash::workflow` is the whole surface for all four. The workflow is a
kernel document; the host never handles bytecode, and never has to print or
parse a source language to do any of this.

[`examples/workflow-graph-roundtrip`](../examples/workflow-graph-roundtrip/README.md)
is a host built this way, and the code below is the shape of its backend.

## Keeping and running a document

Hosts retain `lash::workflow::document::DocumentBytes` by default: opaque
bytes, their Lash-issued `DocumentId`, and a token grammar tag. Workflow
import returns this value, publication accepts it, and a published definition
starts by id. No JSON encode or decode is needed to run it. Store the three
parts verbatim; `DocumentBytes::from_parts` reassembles them, and Lash validates
the grammar, tokens, limits and id on receipt. A typed
`WorkflowAdmissionRefusal::Bytes(DocumentBytesRefusal)` refuses bad blobs.

Use `bytes.to_document()?` only when a viewer or editor needs the typed tree,
then `document.to_json()?` for readable JSON. After editing (including
`Document::from_json`), `DocumentBytes::from(document)` derives the id again.
Graph-read APIs continue to answer a typed `WorkflowDocument` explicitly.

The blob is not a public durable format: a grammar change refuses old bytes
with the found and supported versions. Keep readable JSON or source to
re-lower when longevity across versions matters. See the
[format note](kernel/document-bytes.md) for receipt and compatibility rules.

## The typed document

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

A minimal whole-workflow example that compiles and runs — the import path,
a parse, the printed round-trip and the typed refusal a wrong spelling gets
— lives in the `lash::workflow::document` module docs and on
`parse_document`.

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

`lash::workflow::WorkflowDocument` is the checked holder: a document with
the entry a definition starts, what lash derives from the two, and the
declarations of the library functions it was linked against. It lives in
`lash-kernel-check` and needs no runtime, so a viewer holds the same value
a running host reads:

| Read | Answers |
| --- | --- |
| `document()` | The kernel document. |
| `reference()`, `identity()` | The content identity of the document, and the entry (`Main` or `Entry { function }`). |
| `entry_unit()`, `entry_signature()` | The unit a run starts in, and its signature. |
| `graph()` | The derived graph (`lash::workflow::graph::Graph`): a node per block, statement, action and expression, each at its site; control-flow, call, spawn and reference edges; and `execution_sites()`, the sites a run can report an occurrence at, with their kind and enclosing loops. |
| `declarations()` | The definition of every library function the document's `use` lines name, as it was linked: the signatures a view draws. |

`WorkflowOverlayDocument::of(&document)` is what an execution overlay is
held to.

Read the graph; do not write it. It is recomputed from the document, and
there is no edit that writes an edge. `Graph` has no serialized form.

A host that serves a canvas enables `workflow-view` and serves
`lash::workflow::view::derive(&document)`: a `WorkflowView`, lash's wire
shape (schema `schemas/host/kernel-workflow-view`). The feature links the
kernel's document, checker and view crates and the label data, and no
dialect, editor or runtime: a viewer draws TypeScript and Python workflows
without either parser, from a decoded document and the declarations
`FunctionRegistry::export_declarations` ships. Narrow those to the ones the
document lists with `document::required_declarations` and hold them with
`WorkflowDocument::new(document, declarations)` (or `WorkflowDocument::entered`
for an entry): each declaration is checked against the identity it is
listed under before anything is derived, so an import draws before it is
published and a corrupt declaration is a typed refusal, never a view.
It holds the entry's `statements` (each with its site, kernel statement,
block, depth, document ordinal and `actions`: the sites of its own action and
of every action nested in its expressions, in the order a run issues them),
its `scopes` (every block with its ordinal and depth,
typed by what opens it: `entry`, `closure`, `then`, `else`, `for`, `while`,
`try`, `catch`, `finally`), its `dataflow` (bindings, expressions, every
action, nested ones included, with its kind, callee and arguments as written,
lexical availability, parameters, returns and assignments) and every
`execution_sites` entry with its typed `kind` and, for a nested action, the
action it is an argument of (`within`). A host draws a statement as one node
and its actions as parts of it, never as extra statements. The
projection matches every kernel statement and expression kind, so a new kind
reaches every host through this one read.

A host that vendors lash reads the schema's TypeScript declaration from
`lash::workflow::view::TYPESCRIPT_DECLARATION` — the same bytes as the
checked-in `v1.d.ts` — to check its own copy hermetically.

A document carries no labels and no presentation. A library call's
`CalleeView::Library { id, name }` names the function by identity and by the
name its `use` line gives it. `lash::helper_labels::labels()` reads the
shipped helper labels, keyed by name, with no dialect or helper release;
`helper_labels::title(&labels, &name)` answers the label, or the bare name for a
helper this build does not know. Lash keeps no per-node host data: wording,
icons, layout, roles and step labels are the host's, keyed by site.

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
    WorkflowRead::Unavailable(what) => { /* not retained, or retained corrupt */ }
    WorkflowRead::Unsupported { engine_kind } => { /* the engine has no document */ }
}
```

A stored document is held with the definition of every library function it
lists, each under its own identity, and every read links it against those:
never against the reading build's library. A process keeps them for as long
as it keeps its document, and a code cell's frame keeps them for as long as
the frame lives, across its cells, so a build that retired the helper
release a document was written against still reads and draws it. A declaration that is
gone or does not hash to its identity is `WorkflowUnavailable::Artifact` or
`WorkflowUnavailable::Corrupt`, not a fallback to another definition.

`host_artifacts().execution_document(&reference)` answers an
`ExecutionDocument`: the `document`, and `rewritten_from`, the document it
was rewritten from with the `Correspondence` of the rewrite, when a kernel
migration or a successor carry made it. A host holding a source map of the
earlier document carries it across with `SourceMap::rekey(&correspondence)`.

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

A process's environment links exactly the effects its entry performs: what
its code calls or spawns, never what it only passes on, such as the child's
`&worker` (FIG-6074). A start whose environment leaves one unlinked, or
links one the entry does not perform, is refused, naming the effect. One
effect is easy to miss: an `await` of a value the checker cannot prove is a
local task lowers to a guarded `processes.await`, so an entry that awaits a
handle it was handed performs `processes.await` even when its source never
names it, and its environment must link it.

A call whose target is known only at run time goes through the fixed
`call_dynamic` endpoint: the code performs `lash.call_dynamic` with
`{path, args}`, and the environment links it to the deferred provider one of
its components declares, with `ProcessEnvBuilder::link_dynamic(key)`. Such a
call is two committed steps (FIG-6120). The first resolves `path` through
the deferred provider and commits its decision. The commit that consumes a
resolved decision admits the call of the matched tool with `args`, under the
tool's own policy, bound and call id. A crash between the two resumes the
same target under the same call id, and the path is never resolved again. A
refused path, or one no tool answers, reaches the code as a catchable
`tool_failed` whose cause is `dynamic_call_refused` or
`dynamic_call_unavailable`.

## Editing

Open a `Draft` and submit closed canvas operations. Every operation carries the
base document identity and exact Sites in that base.

```rust,ignore
use lash::workflow::authoring::{DataValue, Operation, ValueTarget};
use lash::workflow::edit::{Draft, Position, Transaction};

let mut draft = Draft::open(document)?;
let base = draft.identity();
let applied = draft.apply(&Transaction { base, edits: vec![
    Operation::SetValue { base, target: ValueTarget::Literal { expression: threshold_site }, value: new_value },
    Operation::InsertStep { base, at: Position::end(try_body_site), effect, args, result, binding: Some(output_name) },
]}, &environment.checker())?;
```

The supported operations are SetValue, InsertStep, RemoveStep, MoveStep,
WrapSteps, UnwrapSteps and ChangeCondition. Values are data or in-scope binding
references. Structural step operations accept separable perform/tool steps;
nonseparable nested actions require source. Wrap/unwrap supports if/for/try,
with empty discarded arms and preserved binding/control targets. While loops
support condition changes only. Source replacement and re-import own renames,
declarations, arbitrary code, handlers and terminal structure.

A batch applies whole after checker admission or changes neither draft nor
correspondence. Refusals preserve typed target, structural and checker causes.
Operations, transactions, correspondence and refusal data serialize; hosts own
their transport and revision envelope. The schemas are under `schemas/host/`.
Labels and layout stay host data, keyed by Sites and carried through the
returned correspondence. Editing never relocates a parked run.

To make a workflow from a cell's entry, call
`extract_entry(&document, &entry, &environment.checker())`. The atomic result
contains the admitted document and exact effect requirements, without exposing
the removal/cleanup primitives. Store that document directly. Its optional
`applied` admission/correspondence is in-process only and is omitted on the wire.
A missing entry refuses with typed `NoSuchEntry`; admission causes remain typed.

A definition id is a content address. Holding or copying one proves content
only, not possession. Lash performs no possession check; the host authorizes
who may create or run a workflow from an id.

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
by an insertion or a new wrapper. `successor(&site)` and `predecessor(&site)` walk it
in either direction, and `then` composes two.

Key anything you keep per node (canvas position, comments, selection) by
site within one document, and move it through the correspondence when the
document changes. `Draft::correspondence_since_open` covers every
transaction since the draft was opened.

A wholesale replacement has no edit history. If you replace the document,
open a new draft: nothing corresponds to the old one unless your host
decides it does.

## Publishing

Publishing admits a document as a definition of one entry. It takes the
document, not a draft, so a host that only runs and publishes needs no
editor; a host that edited one converts `draft.document().clone()` to
`DocumentBytes` and keeps its own
`draft.correspondence_since_open()`.

```rust,ignore
let pin = HostArtifactPin::mint();
match core
    .host_artifacts()
    .publish_workflow(
        &pin,
        &DocumentBytes::from(draft.document().clone()),
        &Name::new("order_review"),
        &env_spec,
        Some((&source_map, &source)),   // or None
    )
    .await?
{
    WorkflowPublish::Published(publication) => {
        let definition = publication.definition;   // id and signature: what a run starts
        let bytes = publication.document;          // opaque DocumentBytes to retain
        let reference = publication.reference;    // document id and selected entry
        let map = publication.source_map;          // Some(SourceMapId): keep it beside the id
    }
    WorkflowPublish::Refused(refusal) => { /* why admission refused it */ }
    WorkflowPublish::SourceMapRefused(refusal) => { /* the map is not this document's or source's */ }
    WorkflowPublish::Unsupported { .. } => { /* no engine admits documents */ }
}
```

A source map is optional and never changes behaviour. Published with the
document, it is checked against the document and the source (identity,
source digest, every site, every range) and stored beside the source under
the same pin; the answer's `SourceMapId` is the digest of its canonical
bytes. Lash keeps no association between a definition and a map: the host
keeps the id, and `host_artifacts().read_source_map(&id)` answers the map
and its source again, checked.

Lash checks the document against `env_spec`, the `ProcessEnv` a process of
the definition would run under: the tools it admits, contributed by the
process components it names (each registered on the builder with
`LashCoreBuilder::process_component`), and publishes the definition of the
named entry under your pin. Start the process under the same environment:
publish it with `host_artifacts().publish_process_env(&pin, &env_spec)`,
and pass that reference, with the engine settings
`core.processes().engine_defaults(kind, &payload)` answers, as the
`EngineLaunch` of `ProcessStartRequest::definition`.
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

A function a run handed to a tool arrives as a function value,
`{"$lash_function": <FunctionValue>}`, and needs no publication to start.
`ProcessStartRequest::inline(LASH_VM_ENGINE_KIND, value, args, launch, ..)`
starts it under your key (`with_host_start_key`): realization checks the
value against the build's library and its bounds, publishes its document
and descriptor under the start, and the process then holds them. Nothing is
published before that, and a refused value publishes nothing. An exact
retry under the key answers the same process, from any point a crash cut
the first attempt at; other bytes, arguments or launch under it are a
`StartKeyConflict`. To keep the function for later starts, publish
`value.document` at `value.entry` instead.

Draft storage, revision names, undo, collaboration and the decision to start
a run are yours. Lash stores definitions, not drafts.

## Showing a run

The document is the static truth. A process's observation snapshot carries
its `ProcessWorkflowOverlay`: committed counts plus any live provisional
counts, action states and current task positions. Fold its committed events
and accepted live observations with `ProcessWorkflowOverlayAccumulator`, as
[Observing processes](observing-processes.md) describes.

Pass that overlay's `ran_counts()` to the view helper, with the document
and the unit the run entered:

```rust,ignore
let coverage = lash::workflow::view::coverage(
    document.document(),
    &document.entry_unit(),
    &overlay.ran_counts(),
);
// coverage.statements: site and runs; coverage.scopes: block and entries.
```

`runs` and `entries` are optional counts. `None` means unobserved; `Some(0)`
means observed and never run. Observation defaults to `off`, so an empty count
vector alone never means zero. At `counts`, the helper answers zero for sites
that never ran. Scope entries tell how often an arm was taken, a loop iterated,
a catch caught an error or a function was called. Lash interprets the raw
ordinals; hosts render the helper's answers. A pure expression's statement
start is evidence of entry, not a claim that every nested call completed.
Process totals include the overlay's live provisional contribution until its
next committed count replaces it. Action state and current task position still
come from `sites` and `current_sites`.

For a retained process segment, request replay through
`core.processes().replay(..)`. Only `SegmentAnswer::Verified { path }` carries
a path; pass it to
`view::path(document.document(), &document.entry_unit(), &path)` for ordered
statement and scope steps per task. Diverged or unreplayable segments supply
no path. Pass each unit of the document you want to show: one unit's path
omits other units. Neither helper needs a derived view or the engine library.

Cells provide counts through `WorkflowExecutionOverlay::ran_counts()` and the
same `view::coverage` helper. Import `RanCounts`, `RunObservation`, `ReplayPath`
and `TaskPath` from `lash::workflow::view`. Set `RunSpec::observe` to
`RunObservation::Counts` for a run that should count its cells. Each cell's
`ExecutionStarted` carries its actual level, so later runs in the same session can change it. A missing
start stays unobserved. Cell reports carry cumulative totals within a stable
activation segment; their overlay deduplicates and sums them independently of
its occurrence-history limit. Cells offer counts only, with no replay or path
trace. Hosts persisting cell overlays retain `ran_segments` with the totals
so folding can continue after reopen.

An occurrence is identified by an `EffectIdentity`: the task (`Main`, or a
spawned task), the site, its zero-based occurrence in that task, and the
iteration of each enclosing loop. The process overlay lists observed task
sites, one row per `(site, task)`, with their latest occurrence state and
occurrence count. `terminal_unobserved` means the process ended without an
observed terminal for that action. Its first committed entry and latest
settlement name event sequences; hosts read times from those events. Tasks
spawned at one site remain separate rows.

Draw an action's state from its site's row, statement and scope counts from
`view::coverage`, and wording and nesting from the document. A missing action
row alone gives no evidence about whether a pure statement ran.

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
feature, `lash::typescript::print(document.document(), options)` answers
equivalent TypeScript for a document, keeping every temporary and statement
boundary. It is a read-only view. `dialect-typescript` and `dialect-python`
select helper releases and never link a parser into the host's own process:
name `typescript` or `python` to print or lower in-process.

A document can have no TypeScript spelling. The printer then answers a
diagnostic, and the document stays readable, editable, publishable and
runnable.

`HostArtifacts::import_workflow(language, source, &environment, limits).await`
takes a whole workflow module in TypeScript or Python
(`SourceLanguage::TypeScript` or `SourceLanguage::Python`) and the
`ProcessEnv` you pass to `workflow_environment` and `publish_workflow`. Lash
builds the dialect environment from the tools that environment admits from
the process components it names, and the worker's function registry; the
worker lowers the source, and lash admits the document. It runs no effect,
publishes nothing and links no parser into your process. `limits`
(`lash::dialect::workflow::WorkflowSourceLimits`) narrows the dialect's source
policy; `None` keeps its standard one.

Both dialects implement one workflow contract (`lash::dialect::workflow`):

- Every top-level async function is an entry: in TypeScript an
  `async function` declaration, exported or not, or a `const` bound to an
  async arrow or function expression; in Python an `async def`. A function
  declared inside a block (an `if`, a loop, a function) is ordinary code of
  the module, and the rest of the top level is `main`. Entries need no
  process-start tools.
- An entry captures nothing of the module: reading a top-level value, a
  top-level helper or another entry is refused. Pass values as parameters
  and define helpers inside the entry's body.
- An entry's signature is what its annotations mean in its dialect
  (TypeScript `string[]` is a list of text; Python `dict[str, int]` is a map
  of text to integers); an annotation with no kernel type is refused, and so
  is a parameter default.
- Refusals of these rules carry the same code in both dialects:
  `WORKFLOW_DUPLICATE_ENTRY`, `WORKFLOW_EMPTY_MODULE`,
  `WORKFLOW_CAPTURED_MODULE_BINDING`, `WORKFLOW_UNMAPPABLE_ENTRY_ANNOTATION`
  and `WORKFLOW_ENTRY_DEFAULT`. Syntax errors keep each dialect's own codes.

Import returns opaque `DocumentBytes`; select an entry when publishing them.
Decode explicitly with `to_document()` when you need to inspect the entries.

Printed source reads back through
`HostArtifacts::import_exported_workflow(source, format, &base, &environment)`,
where `base` is the stored Document it was printed from. The source carries
only code; the manifest, entries, helper identities and renames come from
`base`, and the result is admitted against `environment`. An edit the reader
has no exact inverse for is a typed refusal naming the construct.

`WorkflowImport::Imported { document, source_map, source }` carries the
admitted opaque document bytes, the map from its sites to the source and that source:
pass both to `publish_workflow` to keep them. Exported source read back with
`import_exported_workflow` answers `WorkflowImport::Read { document }`, with
no map of its own. `Refused(diagnostics)` carries front-end
diagnostics with byte spans, `Admission(WorkflowAdmissionRefusal)` names a
checker or catalogue refusal, and `Unsupported(WorkflowImportUnsupported)`
names a missing kernel workflow engine, a dialect the worker does not install,
or an unusable named library. These types live under `lash::workflow`. Both
calls require only the `codemode` feature; the worker must install the
dialect (`dialect-typescript`, `dialect-python`).

"Open in source" is a convenience and must never be the only way your host
can change a workflow.

## Namespaces

| Namespace | For |
| --- | --- |
| `lash::workflow` | Publication, reads, the derived `WorkflowDocument`, the environment, and the execution overlay. Ordinary host work stays here. |
| `lash::workflow::document` | The kernel document: the AST, sites, names, identities, kernel notation. |
| `lash::workflow::graph` | The derived graph, execution sites and manifest requirements. |
| `lash::workflow::view` | The canvas read and run coverage (`workflow-view`). |
| `lash::workflow::edit` | Drafts, edits, transactions and correspondence (`workflow-authoring`). |
| `lash::workflow::authoring` | Authoring operations planned into transactions (`workflow-authoring`). |
| `lash::typescript` | The optional TypeScript dialect: its front end and printer (`typescript`). |
| `lash::python` | The optional Python dialect: its front end and exact codec (`python`). |


## Who supplies a process's environment

Whatever starts an engine process supplies its complete `ProcessEnv` and
engine settings. Lash builds only its ordered named process components from
the separate component registry. It fills no session or deployment defaults.

| Start site | Who supplies the environment and settings |
| --- | --- |
| Host engine or definition start | The host publishes `ProcessEnv` under a host pin, then states `EngineLaunch { env, settings }` in the request. It may explicitly call `processes().engine_defaults(kind, payload)`. |
| Model start in a session | The host's required async `ProcessEnvPolicy` sees the recorded run and authoritative resolved target. Declaration holds and seals its complete choice with the tool outcome. |
| Nested model start in a process | The same host policy sees the enclosing process. `cx.restrict()` explicitly chooses the enclosing environment, restricted to the effects the target performs, and its settings; `policy::restrict_or` does so inside a process and asks another policy outside one. A new environment is also an explicit choice. |
| Other tool-outcome or runtime child start | Its producer states the complete choice and declares/holds it under the attempt's execution claim. Session-turn declarations carry no environment. |
| Isolated tool call | The tool provider's `isolated_start` states payload, environment choice and settings. Binding holds that choice without overwriting it. |
| Workflow publication | The host names `ProcessEnv` for admission against the component catalog; the eventual start states its launch. |
| Trigger delivery | The host publishes the trigger's stated environment under its pin and starts the definition with its explicit launch. |
| Host or tool session-turn start | No process environment. The create request supplies the child session's configuration; its stated session id or the process-derived child id names it. |
| Restart, crash resume, redrive or takeover | The recorded environment ref and engine settings are loaded. No policy or default runs again. |
| Claim by another build | The build must serve the recorded components, config formats and each linked tool's retained surface. It builds only the named components; nothing is re-selected. A component the build declares retired is left out, unless the env links an effect to it or it holds recorded state; then the process parks with the typed refusal (see [Retiring a process component](plugins.md#retiring-a-process-component)). A kernel process stays pinned to the helper functions it was written against. |

A committed model-start decision never re-runs its policy. An attempt abandoned
before commit may choose again. A policy refusal becomes a tool error; a policy
or publication fault retries the uncommitted attempt.
[ADR 0141](adr/0141-a-model-started-process-environment-is-the-host-start-policys-sealed-choice.md)
states these decisions.

## Reading and comparing process authority

Authority is the recorded environment. `core.processes().environment(id)` reads
it together with the process's provenance and engine settings in one store
statement, without building components or loading a session. A session-turn
answer instead names the child session, because it has no process environment.
Unknown process ids answer `None`; pruned processes refuse with
`ProcessNoLongerRetained`. Missing or corrupt environment bytes are typed errors.
An engine answer's `retired` lists the components the environment names that
this core declares retired: the process builds without them.

Narrowing a session does not change processes it already started. Lash enforces
no process authority policy. A host that wants revocation gates
`processes.await/status/cancel` with its own tool check using `environment(id)`
and `tools_excess`, filters `processes.list` results the same way, or cancels the
process. The host also decides how to authorize a session-turn's child session.

Compare against the session's resolved offered catalog. A process's effect
links each name a tool; a link whose tool the catalog does not offer with the
same id and dispatch surface is excess, including a same-name tool with a
different id. The model-facing name, the binding path and the serving
provider's key are not authority. Capability refs are within only when
equal; opaque refs have no ordering of authority. Components and named-session
grants have separate helpers in `lash::process::authority`.

This short host check is compiled in
[processes_evidence.rs](../crates/lash/tests/processes_evidence.rs). It admits
engine processes whose linked tool identities and dispatch surfaces are
offered, and leaves child-session authorization for the host's separate policy:

```rust
async fn may_address_process(
    processes: &lash::process::Processes,
    id: &lash::ProcessId,
    offered: &lash::plugins::ToolCatalog,
) -> lash::Result<bool> {
    Ok(match processes.environment(id).await? {
        Some(lash::process::ProcessEnvView {
            authority: lash::process::ProcessAuthority::Engine { env, .. }, ..
        }) => lash::process::authority::tools_excess(&env, offered).is_empty(),
        _ => false,
    })
}
```

A host holding only an `env_ref`, such as a promoted workflow definition, uses
`core.processes().env(&env_ref)` to recover the recorded environment in one store
statement, including capability binding ids, contracts and arguments. The ref
is content identity and confers no retention; keep the environment pinned.
`host_artifacts().process_catalog(&env)` admits the environment and builds its
components with no process to return the tools they serve for `env`'s links.
That catalog observes the serving build, rather than the tools at start time.
