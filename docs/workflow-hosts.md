# Editing, publishing and showing workflows

Recovery and external-effect guarantees follow [What survives a crash](../README.md#what-survives-a-crash).

A host that lets people build workflows does four things with one: read it,
change it, publish the change as something that can run, and show what a run
did. `lash::workflow` is the whole surface for all four. The workflow is a
kernel document; the host never handles bytecode, and never has to print or
parse a source language to do any of this.

The [host-package guide](host-packages.md) explains the operations and code
a document can call, their installation, and the identities a published or
parked workflow retains. New cells select one dependency-closed package
set; published workflows retain their admitted function identities.

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
operations the code performs with the signature and contract id it expects
of each, every library function the code reaches by identity, and how an effect result's
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
effect ledger.record(input: Any) -> Any contract "1"
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

Every JSON site uses `Site`'s serialization: a unit's root body omits `path`;
child sites carry a nonempty `path` array. Operations, refusal diagnostics and
views use that same spelling, so hosts can compare their serialized sites
directly. Treat an omitted path as the empty path when constructing an edit.

Cell memory refusals report `memory limit exceeded (arena capacity in bytes)`.
The numeric limit is in bytes counted by the arena capacity meter, including
counted control state; it is not a logical value size or the worker's RSS.
The refusal still identifies the library function when the bound was crossed
inside one, and retains the `CELL_BOUND_EXCEEDED` code and heap-limit cause.

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
`lash::workflow::view::derive(&document, &packages)`: a `WorkflowView`, lash's wire
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
and its actions as parts of it, never as extra statements.

Each action has a `role`: `effect` (a `perform`, such as a tool call),
`wait` (a `sleep`), `sub_workflow` (a call or `spawn` of the document's own
code), `library` (a call or `spawn` of a library function, and an `await`
through its dialect's policy) or `task` (`join`, `join_many`, `cancel`,
`yield`). Pick the steps a person recognises by role, never by callee name.
An action's `applies` names the callables it runs that the view can
derive: TypeScript's `ts.promise.start(<closure>)` and Python's
`py.task.run(<closure>)` name that closure, and a call through a variable
that only ever holds one closure names it. A closure is named by its
expression's site, one of the view's `callables`; a declared function by
its body's root, which `derive_callable` opens. Draw "this step opens onto
that body" from `applies`, never by matching sites by position. A callable
passed through a parameter, a collection or a library call's result (such
as a TypeScript function token read through `ts.callable`) is not derived,
and `applies` is empty. The
projection matches every kernel statement and expression kind, so a new kind
reaches every host through this one read.

The view's `entry` states the callable it opens as a caller sees it: its
`name` (absent for `main`), its `params` with the type, optionality and
`dataflow` binding of each, and its declared `result`. A parameter's type
is the signature's (`triage(title: string, …)` declares `text`), not its
binding's facet, which is often `any`. `result_facet` is what a call can
end with: the union of the facets of the returns and `finish`es that end
the entry, leaving out dead ones after a statement that cannot complete,
plus `null` when the body can fall off its end. A TypeScript entry with no
annotation declares `any`; read its shape from `result_facet`. Do not
recompute either from `dataflow`.

A control node's `header.comparison` reads its condition as one comparison
when it is one: `eq`, `ne`, `lt`, `le`, `gt`, `ge`, `contains`,
`starts_with` or `ends_with` with a `left` and a `right`, or `not` with one
`operand`. Each operand is a `ValueRef`: a `source` (an unassigned
`parameter` by binding, a `literal`, or the `result` of the action at a
site, held by a variable nothing reassigns) and the record `fields` read
from it. TypeScript's `if (priority === "high")` and Python's
`if priority == "high":` both read as `eq` of the parameter `priority` and
the literal `"high"`. The view follows a condition variable only when the
statement right before the control binds it to the comparison and nothing
else reads or reassigns it, as a lowering's temporary is. Which library
functions compare is each package's display declaration
(`MemberDisplay::comparison` in the supplied `PackageView`), not a table of dialect names
in the view or the host. Anything else, such as a reassigned variable, a
call no library declares a comparison or a converted operand, has no
`comparison`: show the condition's expression instead, never a guess.

A host that vendors lash reads the schema's TypeScript declaration from
`lash::workflow::view::TYPESCRIPT_DECLARATION` — the same bytes as the
checked-in `v1.d.ts` — to check its own copy hermetically.

A document carries no labels and no presentation. A library call's
`CalleeView::Library { id, name, member }` names the function by identity and
by the name its `use` line gives it. `member` lists every installed
`{package, member}` mapping in the selected set; the two canonical dotted
paths stay paired. Each action's `performs` lists the canonical operations
it reaches through checked bodies. An `ActionObservation` carries the
request's authored `origin` alongside its unchanged effect identity, so
library effects can be drawn at their calling statement. Pass the selected `lash::package::PackageView`
into `derive` and `derive_callable`. Its `display(package, member)` reads a
canonical member's display facts, and `display_for_function(id)` joins a
private or exported definition by identity. `packages.title(&id, &name)`
answers the title, or the bare document name for a definition that set does
not label. Display data never enters execution identity.
Lash keeps no per-node host data: wording,
icons, layout and step labels are the host's, keyed by site.

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
the frame lives, across its cells, so a build that removed an artifact from
its serving package set still reads and draws the document. A declaration that is
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

environment.effects();     // each operation a document may perform, with its requirement
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

A call whose target is known only at run time goes through a tool the host
names itself, such as `call_tool(input)`, with its own name, schema and
description: the code calls it like any other tool. Only a tool its deferred
provider's registration grants dispatch may dispatch. Register the provider
with `DeferredToolProviderRegistration::new(provider)`, select its resolution
contract with `with_resolution_policy(DeferredResolutionPolicy { execution,
execution_policy })`, and grant the tool with
`with_dispatch_grant(DispatchGrant::new(definition, transform))`. The
documented default is 60 seconds and three repeatable attempts, with
exponential backoff from 100 to 1,000 ms. The transform is a deterministic,
pure Rust function from the tool's input to a `DispatchTarget { path, args }`,
or a `TransformRefusal`. Link the tool with the registration's
`dispatch_link(key, &tool_id)` in `ProcessEnvBuilder::link` at the effect its
operation names; a tool the registration does not grant is refused there with
`DispatchGrantRefusal::NotGranted`. The link seals the grant, its definition
and its policy into the environment; reopen, redrive and takeover use it
rather than the receiving registration's policy. The resolution bound is
clipped by the enclosing deadline as usual.

Such a call is two committed steps (FIG-6120). The first runs the transform
and resolves its `path` through the deferred provider, and commits the
decision. The commit that consumes a resolved decision admits the call of the
matched tool, under the tool's own policy, bound and call id; that call runs
the transform again for its `args`. Lash never journals the transform's
answer, so a transform that is not deterministic is a host bug: when the path
it names again differs from the committed one, the call fails with
`dispatch_diverged` and the matched tool does not run. A crash between the two
steps resumes the same target under the same call id, and the path is never
resolved again. A refused path, or one no tool answers, reaches the code as a
catchable `tool_failed` whose cause is `dynamic_call_refused` or
`dynamic_call_unavailable`. A transform that refuses the input fails the call
with `dispatch_input_refused`, and a grant the node's registration does not
hold with `dispatch_not_granted`, before the provider is asked.

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
references. Structural step operations accept separable perform/tool steps.
A whole-statement perform whose inline argument helpers are pure library calls,
such as `await inbox.sendItem(ts.boundary({...}))`, moves, removes and
wraps with those helpers. Library purity follows the called bodies; another
perform, a wait, a declared call or a callback application is not an argument
helper. Assignment receivers must remain action-free.

Hosts that match `EditDiagnosticKind::NonSeparableStep` must now read its
`enclosing_statement` and `separable_step` fields. The diagnostic location still
identifies the requested base site. Offer `separable_step`, when present, as the
`action` (or wrap endpoint) of a structural operation against that same base.
Offer `enclosing_statement` for a source edit of the whole statement. Both
fields are optional base Sites; a statement containing two performs has an
enclosing statement and no separable step. Use the regenerated kernel edit
refusal schema when decoding this cause; its code remains `non_separable_step`.

Wrap/unwrap supports if/for/try,
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
let packages = core.host_artifacts().cell_packages().await?;
match core
    .host_artifacts()
    .publish_workflow(
        &pin,
        &DocumentBytes::from(draft.document().clone()),
        &Name::new("order_review"),
        &env_spec,
        packages,
        Some((&source_map, &source)),   // or None
    )
    .await?
{
    WorkflowPublish::Published(publication) => {
        let definition = publication.definition;   // id and signature: what a run starts
        let reference = publication.reference;    // document id and selected entry
        let map = publication.source_map;          // Some(SourceMapId): keep it beside the id
        let document = core.host_artifacts().execution_document(&reference).await?; // read under the live pin
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

A tool takes a function only in a slot its input schema marks
`{"x-lash": {"kind": "function"}}` (optionally with a `signature`): JSON
Schema describes data, so a function anywhere else in a tool's input raises
`not_data` at the call, naming its path. The marker may stand under a
composition: `{"anyOf": [{"x-lash": {"kind": "function"}}, {"type": "null"}]}`
takes a function or null, as pydantic and MCP spell an optional slot. `anyOf`,
`oneOf` and a `type` list are typed as the union of their members (`oneOf`'s
exclusivity is left to the tool's own validation), and `allOf` as the meet of
its members; a schema whose `allOf` no kernel type states, such as a string
that is also an integer or a required field another member's closed object
leaves out, is refused when it is admitted, with
`SchemaAdmissionError::Unprojectable` naming the composition's JSON pointer.
So is a meet that grows as the product of two unions past
`lash::vm::MAX_PROJECTED_TYPE_NODES` (4096) type nodes and past both its sides
together. An object with `properties` is typed as a record of its fields
whether or not it closes `additionalProperties`, so the marker holds on a
field of the open objects pydantic and MCP servers write, and behind a local
`$ref` (`#/$defs/...`, `#/definitions/...`) too: a reference is typed as its
target, and at a cycle, or past `lash::vm::MAX_RESOLVED_REFERENCES` (4096)
resolutions, as `Json`, or as `Any` when its target can reach a marked
slot. A field an open object does not declare is data. A function a run handed to a tool
there arrives as a function value, `{"$lash_function": <FunctionValue>}`,
and needs no publication to start.
`ProcessStartRequest::inline(LASH_VM_ENGINE_KIND, value, args, launch, ..)`
starts it under your key (`with_host_start_key`): realization checks the
value against the build's library and its bounds, publishes its document
and descriptor under the start, and the process then holds them. Nothing is
published before that, and a refused value publishes nothing. An exact
retry under the key answers the same process, from any point a crash cut
the first attempt at; other bytes, arguments or launch under it are a
`StartKeyConflict`. To keep the function for later starts, publish
`value.document` at `value.entry` instead. Keep `value.captions` beside the
published workflow: it contains the statement captions spelled in the cell's
function, including when the function was saved in an earlier cell. Extraction
re-keys those captions to the new document's sites, accounting for frozen
captures and canonical function names. The transport payload has three fields:
`document`, `entry` and `captions`; the slot's schema is the marker, not the
payload's shape. Rust constructors without captions use
`Captions::empty(document.identity())`. Captions remain outside the document's
executable identity.

A host-observed start exports disjoint `lash.process.start.<phase>_us`
attributes on its `lash.process.start` producer. For a published definition:

| Phase | Work and waits included |
| --- | --- |
| `definition_load` | Store read and decoding of the definition descriptor. |
| `definition_resolve` | Engine signature derivation; the kernel reads the retained document, checks its helper manifest and entry, and may wait for its document cache or decode a cache miss. |
| `definition_acquire` | Acquiring the descriptor and its artifact closure under the start claim, including store transactions and their lock waits. |
| `definition_verify` | Checking a supplied signature claim, including the engine's independent derivation and document read. Absent when no claim was supplied. |
| `definition` | Constructing the definition's start payload, or realizing a function supplied inline. |

These are elapsed times, including scheduling and store waits, rather than CPU
measurements. They do not include worker checkout, package-registry preparation,
compilation or the first segment: registration does none of those. Retrying an
already-fenced start can read and resolve again; each field sums its repeated
operations. A failed operation also records its time before propagating its
refusal. A host diagnosing a long start should inspect the subphase that grew;
publication or a warm worker does not bypass the start's retention transactions.

Hosts that previously treated `definition_us` as the complete definition cost
must now sum it with `definition_load_us`, `definition_resolve_us`,
`definition_acquire_us` and `definition_verify_us` (when present). The trace
record's `ProcessStartPhase` vocabulary includes `DefinitionLoad`,
`DefinitionResolve`, `DefinitionAcquire` and `DefinitionVerify`; exhaustive
matches must handle them. Trace schema v37 is updated in place for the 1.0
baseline.

Draft storage, revision names, undo, collaboration and the decision to start
a run are yours. Lash stores definitions, not drafts.

## Showing a run

The document is the static truth. A process's observation snapshot carries
its `ProcessWorkflowOverlay`: committed counts plus any live provisional
counts, action states and current task positions. Fold its committed events
and accepted live observations with `ProcessWorkflowOverlayAccumulator`, as
[Observing processes](observing-processes.md) describes.

The overlay's `live_coverage` says whether that fold took any live evidence.
Only live observations from the process feed move it off `not_observed`; a
fold of committed events alone answers `not_observed` however far the run
got, because stored events carry no live evidence. To show live coverage, a
host must:

1. Read the overlay from the process feed, not from its own copy of the
   stored events. Either take `workflow` from the `Retained` read view of
   `core.processes().observe(&process_id).snapshot()`, or fold the feed
   yourself: `committed` for each stored event, then `observe(&document,
   &event.payload)` for every item of `subscribe_and_recover(cursor)`, and
   `live_gap(reason)` at a gap. The fold then answers `retained_window`
   while the replay window holds the run's frames, and `gap` once it lost
   them.
2. On a PostgreSQL deployment with more than one replica, install the
   shared replay store: `PostgresHost::process_replay()` through
   `LashCoreBuilder::process_replay_store`, configured by the
   `process_replay` section ([PostgreSQL operations](operations/postgres.md#process_replay)).
   The default in-memory store holds only the frames its own OS process
   produced, so a replica that reads a run another replica executes finds
   no frames and answers `not_observed`.

Frames leave the replay window by its age, event and byte bounds
(`DataRetention::process_replay`; 120 seconds in the standard preset). Once
a run's frames have left it, a fresh snapshot answers `not_observed` again
and its coverage comes from committed counts alone.

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

An occurrence has an opaque `EffectId`. Its `task()`, `position()` and
`visit()` readers return `TaskLabel`, `PositionLabel` and `VisitId` handles.
Store and compare these handles; their ordering and encoding belong to the
engine. `PositionLabel::site()` is the document site a position names, the
key to look it up by in a workflow view. JSON carries opaque string tokens. Display a typed handle with
`Display`, or use the process row's `label`; never parse either display labels
or tokens. `ObservationReader::counts(&at)` supplies exact semantic starts and
loop-entry counts, and `ObservationReader::fold` folds duplicate or reordered
live frames inside the kernel.

The process overlay lists observed task sites, one row per `(position, task)`,
with their latest occurrence state and occurrence count. `terminal_unobserved`
means the process ended without an observed terminal for that action. Its first
committed entry and latest settlement name event sequences; hosts read times
from those events. Tasks spawned at one site remain separate rows.

A failed occurrence whose error a `catch` took, so the run went on, is
`recovered` (`ActionState::Recovered` in the view); draw it as a handled
failure, not as the one that ended the run. Lash reports it directly, so do
not infer it from a catch block's coverage. The error a call fails with is
raised at its wait action and unwinds to the first `catch` that takes it,
the same rule that names a run's failing site. That catch recovered from it
when its block is left by anything but a raise (it ends, returns, breaks or
continues out) or when its block waits, for example on a fallback call.
The occurrence stays `failed` when:

- nothing took the error, so it ended its task;
- the catch that took it raised again before its block waited: `throw
  error`, a bare `raise`, or a Python `except` none of whose clauses
  matched. When it raised the same error again, an outer `catch` that takes
  it recovers from it;
- the call failed inside a catch block with no `try` around it of its own:
  the enclosing `try` already took its error;
- it ended a spawned task: the `join` that observes that task raises its
  own error at the join.

The evidence is committed. The `process.ran` of the run segment that caught
the error names the occurrence in its `caught` list, and the occurrence's
own `process.effect_outcome`, committed before that segment ran, stays a
failure. Until that segment commits, the row reads `failed`. A recovered
row survives a feed gap and a reload like any committed fact. Only a run
observed at `counts` records its catches: at `off`, as coverage reports
nothing, a caught failure reads `failed`. Across several occurrences of
one action, the view's activity state shows a failure over a recovery.

Lash stores no wall time on an occurrence, and a row's `first_entered` and
`last_settled` span all its occurrences. One effect attempt joins on its
`EffectId` across the process events you store, each timed by its own
`occurred_at_ms`:

- **Result and end:** the committed `process.effect_outcome` whose `at` is
  the attempt's id. It carries the outcome class, the failure code of a
  failure and, for a tool, the `call_id` that names the call's record and
  what it answered.
- **Start:** the committed `process.waiting` whose `wait.site` is the id,
  for a call still in flight when the process released; its
  `process.resumed` ends that wait. A call that settles inside the run
  segment that issued it commits no start. Your tool implementation sees
  its `call_id` when it begins, so record the start there if you need it.
- **Sleeps:** a sleep's `process.waiting` and `process.resumed` are its
  start and end. A sleep that never parks commits neither.
- **When a sleep resumes:** while a sleep waits, its row's state is
  `waiting` with `kind: sleep` and `resume_at_ms`, the store-clock
  millisecond its park resumes at: the instant it parked plus its duration,
  computed or literal. It is the `until_ms` its `process.waiting` recorded,
  so replay and a reload read the same value; say "resumes at 11:27" from
  it, never from the source's duration. Other waits carry no
  `resume_at_ms`.
- **Cancelled waits:** a cancel that cuts a parked wait short, a sleep
  included, ends it with `process.wait_cancelled`, never `process.resumed`.
  The fold settles that occurrence `cancelled`, so a cancelled sleep never
  reads completed. A sleep that elapsed before the cancel stays completed.
- **Omitted occurrences:** an occurrence past the evidence cap has no outcome
  event of its own. `process.effect_omissions` counts it.

Direct kernel embeddings select `MachineOptions` with `Machine::set_options`
for each run segment. `ObservationMode::Counts` records semantic `SiteCounts`;
a host supplies `RunObserver::counts` through `Host::observer` for live deltas,
and `Machine::take_counts` drains the remaining counts. Recorded paths travel as
opaque `ExecutionPath` values and are interpreted by the kernel's replay reader.
Checkpoint handles expose only their control header; the broker uses
`inspect_checkpoint` on an authenticated page and `AdmittedWait::encode` /
`AdmittedWait::decode` for admissions. Session bindings come from the session
readers, and page-port bounds from `PagePortLimits::STANDARD`.

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

## Captions

A caption is a step's plain-language title and optional description:
`lash::workflow::document::Caption`. `Captions` holds a document's captions
by site, keyed to its `DocumentId` like a source map but separate from it, and
serialises as the `kernel-captions` host schema. Your host holds it; lash
stores none, and no edit, draft, admission, checkpoint or identity reads it
(`K-CAP-001`).

A caption names a `CaptionTarget`: a site, a parameter or a field.
`CaptionKind::captionable(&document)` lists every target a document has.

- **A site:** a statement, a clause arm (an `if`'s else block, a `try`'s
  catch or finally block, a loop's non-empty else block) or a declared
  function's body. A then-arm or loop body is named by its statement, and
  the entry function's caption is the workflow's title.
- **A parameter** (`ParamTarget { function, param }`): a declared function's
  parameter, by name. An entry's parameters are its signature's, the names a
  start passes its arguments by.
- **A field** (`FieldTarget { function, path }`): a named field of a record
  type an entry's parameter or result has. `FieldPath { root, fields }`
  starts at a parameter (`FieldRoot::Param(name)`) or the result
  (`FieldRoot::Result`) and names one record field a step, so
  `event.answer.choice` is root `event`, fields `["answer", "choice"]`. A
  list is walked to its element and an optional value (a union whose
  members other than null and absent are one type) to that type, so
  `items.title` names the title of each record in a list `items`. A map, a
  set, a tuple, a record's rest and a union of two records are not walked.

Use parameter and field captions to say in plain words what a workflow takes
and what it gives back. In JSON an entry names its target by exactly one of
`site`, `param` and `field`:

```json
{ "param": { "function": "triage", "param": "event" },
  "caption": { "title": "The card that was answered" } }
{ "field": { "function": "triage",
             "path": { "root": { "param": "event" }, "fields": ["choice"] } },
  "caption": { "title": "What the reviewer picked" } }
```

Build a value with `Caption::new(title, description)` and
`Captions::new(&document, entries)`. Both refuse text outside the one domain
every dialect can spell (`CaptionRefusal`: one trimmed title line of at most
200 bytes, at most 2 KiB of description, no `*/`, no `@` line, no control
characters or line separators other than `\n`). `Captions::new` also refuses
a site that is not captionable, a parameter the function does not have and a
path its types do not resolve (`CaptionsRefusal`). There is no edit operation
for a caption: to change one, build a new `Captions` and keep it.
`captions.get(&target)` reads one caption, and `captions.at(&site)` a site's.

**Authority.** The sidecar you hold is authoritative. Importing a source
(`WorkflowImport::Imported { captions, .. }`) or reading back printed source
(`WorkflowImport::Read { captions, .. }`) answers the captions that source
spells; replace the sidecar with them wholesale. Once a version has any
change that did not come from its source, a canvas edit or a caption change,
show `print(document, Some(&captions), options)` in your source editor rather
than the stored authored source, so the next save cannot revert captions.
Captions a dialect cannot spell are listed, by target, in
`PrintedSource::dropped` (`K-CAP-006`): the source read back will not have
them, so tell the author before a source edit replaces them. A printer drops
an entry's parameter and field captions when the printed function takes other
parameters than its signature names (a TypeScript entry that reads `this` or
`arguments`), and Python drops a field's below a record with a field name no
`TypedDict` class body can declare.

**Through edits.** After an applied transaction,
`captions.rekey(&applied.correspondence, draft.document())` answers
`Rekeyed { captions, touched }`: the captions whose step survived, at their
new sites, and the sites of those whose step, or anything beneath it, the
edit wrote or made new (`K-CAP-004`). Mark the `touched` captions for review:
"Email the team" may no longer describe a step whose recipient changed. A
parameter's or field's caption is keyed by name, not position, so moving
statements never moves it. It survives while its function still has the
parameter, or still resolves the path, and is dropped once the parameter or
field is gone. It is never `touched` (`K-CAP-007`).

**Showing them.** `view.captions(&workflow_document, &captions)` joins them
with a `WorkflowView` by view location, as the `kernel-caption-view` schema.
`workflow_document` is the `WorkflowDocument` the view was derived from: its
entry's signature names and orders the parameters.

- An action or control node shows its statement's caption. A statement
  with several actions shows it on its first outermost action, the one it
  performs or awaits. A helper nested in that action's arguments gets none.
- A code node shows every caption among its `members` and their arms.
- A control shows its `arms`' captions, by `ControlArm.block`.
- A callable shows its function's caption, on `CallableView.block`.
- `params` lists the entry's captioned parameters as
  `ParamCaption { index, name, caption }`, in signature order.
- `fields` lists the captioned fields of its parameter and result types as
  `FieldCaption { path, caption }`, by path.
- `bindings` lists each `let` of the entry whose declaring statement has a
  caption as `BindingCaption { binding, name, site, caption }`, by binding
  id: that caption names the value the variable holds.

A Start/End panel reads `params` and `fields` and shows any parameter
without a caption by its name. The view itself is unchanged by captions, so
a caption-only change keeps its identity. Wording for a step without a
caption is yours.

**Naming values.** A binding has no caption syntax of its own: the caption
a source writes on a declaration is the caption of the statement that
declares the variable, so `/** The open orders */ const orders = …` and
`#: The open orders` above `orders = …` name `orders`, and printing and
reading back keep it there (`K-CAP-002`). For the value a variable holds,
read its `BindingCaption`. For one that holds a tool's reply, read
`BindingView.produced_by` too: the site of the `perform` it awaited, in
`dataflow.actions`, so "what Fetch the orders returned" can name the step
by its node's caption. It is set for TypeScript's
`const r = await tools.x(…)`, Python's `r = await tools.x(…)` for an
operation `tools.x` (through its
reply helper, `py.reply`) and an `await` of a promise or task that only
performs the call, for a `let` nothing reassigns. A value computed from a
reply, such as `(await inbox.lookup()).urgent`, has none: it comes from the code.
`produced_by` is derived from the document; nothing stores it. A condition
operand that is the `result` of an action joins to the binding whose
`value_site` is that action's site.

Lowered temporaries the source never named stay unnamed: lash makes up no
plain name for a variable a lowering declared (`t28`, `v2`), so show such a
value by what produced it or as "a value from the code", never by its
kernel name. A temporary declared inside an expression never has a
caption. When a lowering splits one source declaration over several
statements, as TypeScript does for a field read of an awaited value or a
destructuring, the declaration's caption stays the step's, on the statement
that holds its first action (`K-CAP-003`): the temporary that statement
declares lists it, and the source variable has no `BindingCaption`.

**Telling a model.** Each dialect declares the caption kinds it spells and a
short guide in its own syntax (`lash::typescript::CAPTIONS`,
`lash::python::CAPTIONS`, a `CaptionSupport`). Put the `guide` in a model's
prompt rather than hard-coding a syntax. Both dialects spell every kind.

- **TypeScript** uses a JSDoc block, `/** Send the weekly report */`, right
  before the statement, function or `else`/`catch`/`finally` keyword it
  names. Put one before an entry's parameter to caption it, and before a
  property of an object type written inline in a parameter's or the
  result's (`Promise<{ ... }>`) annotation to caption that field. A named
  `type` or `interface` is not resolved into an entry's signature, so its
  properties take no caption. `@param` tags are not captions.
- **Python** uses `#:` lines directly above what they caption, at its
  indentation: `#: Send the weekly report`. Write an entry's parameters one
  a line to caption them. To caption fields, annotate a parameter or the
  result with a `TypedDict` class the module declares and put `#:` lines
  above its fields. Field annotations may use `NotRequired[...]`.

## TypeScript is a lens

None of the above prints or parses a source language. With the `typescript`
feature, `lash::typescript::print(document.document(), captions, library, options)`
answers the TypeScript module an author writes for a document: each function
and `main` that reads back as itself is authored source, and any other is in
kernel form, opened by `"use kernel";`. `lash::typescript::read(source, &base,
library, limits)` reads it back, edited or not. `print_kernel` writes every
region in kernel form, keeping every temporary and statement boundary.
`dialect-typescript` and `dialect-python` select dialect support and never
link a parser into the host's own process:
name `typescript` or `python` to print or lower in-process.

A document can have no TypeScript spelling. The printer then answers a
diagnostic, and the document stays readable, editable, publishable and
runnable.

`HostArtifacts::import_workflow(language, source, &environment, packages, limits).await`
takes the gate's `CellPackages` and a whole workflow module in TypeScript or Python
(`SourceLanguage::TypeScript` or `SourceLanguage::Python`) and the
`ProcessEnv` you pass to `workflow_environment` and `publish_workflow`. Lash
builds the dialect environment from the operations that environment admits from
the process components it names and the selected package view; the
worker lowers the source, and lash admits the document. Obtain `packages` once
from `HostArtifacts::cell_packages`; keep the `packages` returned by an imported
workflow and pass it to `publish_workflow` after editing. It runs no effect,
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

`WorkflowImport::Imported { document, source_map, source, captions }` carries the
admitted opaque document bytes, the map from its sites to the source, that source
and the captions it spells: pass the map and source to `publish_workflow` to keep
them, and keep the captions in your sidecar ([Captions](#captions)). Exported source read back with
`import_exported_workflow` answers `WorkflowImport::Read { document, captions }`, with
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
| Claim by another build | The build must serve the recorded components, config formats and each linked tool; a perform whose tool the build serves under another contract than the code pins raises `operation_contract_changed` (see [When a linked tool changes](#when-a-linked-tool-changes)). It builds only the named components; nothing is re-selected. A component the build declares retired is left out, unless the env links an effect to it or it holds recorded state; then the process parks with the typed refusal (see [Retiring a process component](plugins.md#retiring-a-process-component)). A kernel process stays pinned to the helper functions it was written against. |

A committed model-start decision never re-runs its policy. An attempt abandoned
before commit may choose again. A policy refusal becomes a tool error; a policy
or publication fault retries the uncommitted attempt.
[ADR 0141](adr/0141-a-model-started-process-environment-is-the-host-start-policys-sealed-choice.md)
states these decisions.

## When a linked tool changes

Code pins each operation it performs to a contract: the `contract "<id>"` its
manifest states for the effect. The tool that serves the operation declares
that id (`ToolDraft::declared_contract`) or derives it from its input schema,
output schema, output contract, declared intents and host tag
(`ToolDraft::host_tag`), and from nothing else. A link records which tool and
provider serve an effect, the execution policy and the provider's payload. It
records no contract.

At each perform one judge compares the live tool's contract id with the one
the code pins, and nothing else:

- Equal: the call runs under the execution policy its link recorded. A
  reworded description, a new transport or server version, or changed retry
  bounds change no contract.
- Different: the perform raises `operation_contract_changed`, naming the
  effect, the tool and both ids. No body ran, and the program may catch it.
  This holds for a call not yet issued and for one admitted before the change,
  in a round, a cell or a process.
- Missing: the call parks, as a missing tool always has.

A committed outcome is never run again. There is no rebind: a run that meets
a changed contract raises at its next perform of that operation. A host whose
tool changes behaviour under the same schemas changes its host tag or its
declared contract.

An imported MCP tool's host tag is its durable id and a digest of its name and
schemas. Its transport, peer and server version are facts of the connection,
not of the contract, so an admitted call, `Once` included, may be delivered
to a redeployed server whose tool kept its name and schemas. A same-schema
behaviour change on an MCP server is not detected.

On an operation path a package owns, the serving tool must declare the
package's contract id, declare exactly the package's intents and serve the
package's signature. Otherwise the environment is refused when it is built,
with `runner_not_packaged` naming the tool and the package.

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
links each name a tool; a link whose tool the catalog does not offer under the
same id is excess, including a same-name tool with a different id. The
model-facing name, the serving provider's key and the tool's contract are not
authority: a call runs only under the contract its code requires, which the
one judge checks at each perform (`operation_contract_changed`). Capability refs are within only when
equal; opaque refs have no ordering of authority. Components and named-session
grants have separate helpers in `lash::process::authority`.

This short host check is compiled in
[processes_evidence.rs](../crates/lash/tests/processes_evidence.rs). It admits
engine processes whose linked tool identities are offered, and leaves child-session authorization for the host's separate policy:

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
