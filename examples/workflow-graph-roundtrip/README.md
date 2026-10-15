# Visual workflow editor

This example demonstrates a visual editor over Lash's canonical workflow
documents and typed edits. It consumes the public
`lash::workflow::authoring::{Operation, plan}` API to compose higher-level
helpers with direct edits. `src/authoring_http.rs` and `frontend/index.html`
own the transport and presentation; the planner lives in `lash-kernel-edit`.
Both paths produce the same canonical transaction and use the same admission.

From an isolated Kiln fork, run:

```sh
just workflow-graph-roundtrip
```

Open the printed URL. The script chooses a private port from `KILN_GATE_ID` and
creates a scratch SQLite database under `/tmp`. Workflow editing state lives in
memory. The existing Lash runtime executes the examples; email and display tools
are deterministic mocks. No model or external email service is called.

The default screen splits into the graph on the left and the workflow's code on
the right, with a draggable divider. **Graph | Split | Code** in the toolbar
shows both panes or one of them; the browser remembers the choice and the
divider position. The graph is a visual workflow editor: app action tiles on a dotted
canvas, named and typed ports, true/false paths, and explicit workflow input and
Return or Finish steps. Blue connections carry values; dashed gray connections show execution
order. Click an output port and then an input port to connect them, or select a
source in the input inspector. Click a tile to edit its fields, or a connection's
**+** to choose another action. Adding,
editing, and deleting steps plan and validate a transaction, then save it through
the existing version-guarded endpoint. Drag or scroll with two fingers to pan.
The mouse wheel and a trackpad pinch zoom around the cursor, between 30% and
160%; **Fit** shows the whole graph, and loading an example fits it.
**Run workflow** executes the saved document and opens its recorded outputs.

The canvas scope selector uses the blocks of Lash's view, including empty
bodies, named by what opens them. It defaults to the selected entry's own block. Function
bodies, loop bodies, and try/catch blocks are inspected explicitly; their actions
are never silently flattened into the caller's execution sequence. Within a
selected block, conditions show their true/false branches and calls remain opaque
nodes. Running still invokes the whole workflow entry, not the inspected block.

The host does not recognize dialect library names or async wrapper patterns.
Adding a first step targets the selected canonical block. Global additions go
before its terminal statement. Selection follows the immediately previous
transaction's correspondence and resets on whole-document replacement.

This is a small visual editor for canonical actions and scopes, not an inverse
compiler. It deliberately does not infer a high-level form for arbitrary computed
inputs. Node positions are derived, not persistent user layout.

## Examples and the code pane

Every example is authored TypeScript in `catalog/`, written the way a person
would write it, and imported at startup through lash's public
`import_typescript_workflow`, exactly as written: no kernel document is written
by hand and the host rewrites nothing, so the canvas shows lash's current
lowering. The **Example** dropdown lists each one once, grouped as apps or
control-flow examples, with the statement and scope count of its lowering
(the same counts appear under the canvas caption). Choosing one loads it at
once. The dialect toggle beside it offers TypeScript; Python is disabled until
lash has a public Python workflow import (FIG-6380).

The code pane is read-only and shows the saved revision as:

- **Source**: the authored TypeScript the workflow was imported from, as
  imported. Edits do not change it; the pane says when the revision was edited
  since. It is not linked to the canvas: lash's import returns no map from
  authored source to the document.
- **TypeScript**: lash's exact readable printer. Lines are linked through the
  printer's source map.
- **Kernel**: the kernel document text. The kernel printer starts every
  statement on its own line in document order; the pane matches each statement
  of lash's view to its line and checks its keyword. If any statement does not
  match, the tab is shown without linking.

Selecting a node marks the line its statement starts on and scrolls to it.
Clicking a linked line selects that statement: the canvas switches to its scope
and opens its node. Edits and imports refresh the pane.

**Advanced** contains TypeScript import/export and the
helper/direct-edit transaction inspector. Queue a direct edit between
two helpers, preview their combined transaction, then apply it. The preview
validates a cloned draft without publishing. Generated statements remain
available to direct edits. Import validates before replacing the document and
resets site selection; direct edits carry correspondence within a draft.

The TypeScript pane uses Lash's exact readable printer: named async functions,
awaited tool calls and ordinary expressions. The `k.document(...)` header carries
structured metadata needed for exact reconstruction; it is not an escaped JSON
document. Importing an unchanged export preserves the kernel document identity.
The host does not rewrite the printed source or recover dialect patterns.

Inputs support constants, connected values and canonical expressions. Constants
can stay inline or become named value nodes. Only constants can be promoted;
reads and computed expressions retain their original evaluation point.
Promoting a constant inserts a binding and replaces its use in one transaction. Named values can be renamed;
record values expose fields and a whole-object editor. Adjacent, single-use,
unassigned and uncaptured input records can appear inside their consuming action.
Shared records keep their own nodes. Explicit expansion is browser presentation
state and follows edit correspondence.

Lash's view carries binding identity, lexical availability, expression sites,
assignment destinations and callable ownership from the canonical graph. The
frontend consumes that metadata without recognizing dialect syntax. The kernel
checker decides type compatibility during edit admission. Inferred
binding and expression types come from its conservative facets; the browser
keeps unknown types as `Any`. Entry input controls and immediate effect-result
ports retain their declared signatures. A mutable record with an unknown facet
has a whole-value source choice; use canonical expressions for fields the
checker cannot establish. Display hints never narrow an unconstrained binding
or replace a declared return contract.
Assignment nodes show state writes, and closure returns show function outputs.
Scope inputs expose enclosing bindings without turning them into workflow entry
parameters. Running always invokes the entry.

Add-step forms support the mocked Gmail, web search, message and status actions.
The editor chooses a fresh result binding for each visual insertion. A rejected
edit preserves the saved document and reports the error in the current panel.

Return and Finish are actual terminal statements. Return exits the current
function after enclosing `finally` blocks run; a `finally` can override its
result. Finish ends the whole invocation and skips `finally` blocks. Each has
execution order entering it and a typed `result` input, with no outgoing
execution connection. The editor does not invent a terminal tile for a block
without an explicit terminal statement.

Click a named output port and choose **End here with this output**, or use the
same shortcut in the step inspector. Choose Finish or Return. The transaction
inserts the selected statement immediately after the producer, or replaces an
immediately following Return or Finish. Later statements remain in the document
but are unreachable on that path. Deleting the terminal step restores the path.
Editing an existing terminal's result mapping changes only its value; it does
not move termination earlier or skip intervening effects. The shortcut emits
ordinary canonical edits and adds no kernel or dialect behavior.

## Public authoring API

`lash::workflow::authoring::plan(&draft, operations)` produces an ordinary
`Transaction` from an array, vector or iterator of `Operation`s.
`Operation::InsertEffect`
accepts a base-document position, effect name, expression arguments, result
type and explicit result binding. `Operation::Edit` accepts any existing `Edit`.
Computed arguments become visible bindings before the effect statement.
Generated temporary names avoid names throughout the base and complete batch.
The planner performs no I/O, publication, admission or runtime execution.

```rust
use lash::workflow::authoring::{Operation, plan};
use lash::workflow::document::{EffectName, Name, Type};

// The host supplies a draft, insertion position, input expression and raw edit.
let transaction = plan(&draft, [
    Operation::InsertEffect {
        at: insertion,
        effect: EffectName::new("research.search")?,
        args: vec![search_input],
        result: Type::Any,
        binding: Name::new("evidence"),
    },
    Operation::Edit(raw_edit),
]);
let applied = draft.apply(&transaction, &environment)?;
```

The same helpers accept documents imported from TypeScript or changed by direct
edits. Hosts retain their own templates, HTTP transport, revisions, layout and
publication policy. Lash owns the serialized operation data: `edit` and
`insert_effect` are externally tagged cases, with unknown tags and structural
fields refused. The example sends `{ version, operations: Operation[] }`
directly to `/workflow/plan`. Its JSON Schema and generated TypeScript types are
in `schemas/host/kernel-authoring-operation/`. Save the expanded `Transaction`
for exact edit replay against its base and admission context; re-planning an
operation batch later is a new plan because temporary names are not part of the
contract. Host layout and presentation do not belong in the authoring API.

Admission diagnostics index the flattened canonical edits, including generated
argument bindings. Temporary-name spelling is an implementation detail. Name
reservation covers the batch supplied to `plan`; if a host later changes the
returned transaction, it owns the effects of those additional edits.

All edit targets are sites in the transaction's base document. Original nodes
remain targetable after preceding insertions in the same batch. A newly inserted
node receives a targetable site after admission; edit it in the next transaction,
or construct its desired AST before insertion. Explicit output binding names
and raw transformations remain caller choices, checked by normal admission.
This example does not introduce persistent step groups, stable wrapper
objects, a second graph model or unrestricted same-batch references to new nodes.

## Dialect independence verification

Run the focused host projection laws with:

```sh
node --test examples/workflow-graph-roundtrip/frontend/*.test.mjs
```

These pin the unused-closure, opaque-library-call, shared/shadowed-input,
empty-scope and terminal-insertion rules, zooming around the cursor and the
wheel/pinch/pan split, and the code pane's statement-to-line linking. The Rust law
`every_example_opens_as_its_imported_source_and_runs_with_the_mocks` imports
every catalog example and runs it against the mocks. The HTTP regression
`each_edit_response_maps_its_immediate_base` pins per-transaction correspondence
across consecutive edits. The dataflow value-encoding law preserves integral
floats, negative zero and typed nested record values.

Lash's own laws (`lash-kernel-edit`'s `view_tests`) cover lexical source
choices and shadowing, callable return ownership and assignment identity. The
example's Rust laws cover the served view being Lash's, named invocation inputs
reaching effects and the final output, and imported parameterized entries. Browser checks exercise
promotion, renaming, rewiring, type mismatch rejection, actual research and KPI
executions, output reset between runs and mobile layout.

## Dataflow examples

The examples include **Deep research brief** and **Weekly business KPIs**.
Research maps the search result's topic and
sources into a cited brief. KPIs maps current and previous period records into a
comparison with calculated percentage changes. Their last step displays the
report summary. Edit `topic` or `workspace` in the workflow Input node to try
another run. These values are invocation arguments, separate from document edits.
Research evidence, URLs and business metrics are synthetic fixtures; the mock
synthesis and comparison consume their actual mapped inputs.

Each action shows named inputs and result ports. Record outputs also expose
fields as connection sources. Click an input to edit its constant, select an
available source or use the canonical expression editor. **Execution data** shows
each retained call's input and output, with separate occurrences for repeated
calls. Execution log links open the relevant node. The entry Return or Finish inspector
shows the actual terminal result. A nested function Return never borrows the
workflow's terminal result. Data resets when the document changes or a new run
begins; this example does not persist a run-history browser.

The backend serves `lash::workflow::view::derive` verbatim: Lash projects the
canonical document and graph into statements, scopes, dataflow and execution
sites. `frontend/dataflow.mjs` projects ports from that view and plans ordinary
canonical edits; it introduces no second persisted dataflow graph.
The example host owns port labels, layout, expansion state, invocation forms,
mock defaults and transport. Kernel types, lexical bindings, Sites, edits and
runtime records remain authoritative.

The host attaches `callId` from `ProcessEffectOccurrence`, then reads the settled
record through `core.processes().tool_call`. Output display uses the public
`value_for_projection` API. Missing retained data is shown as unavailable. Lash
owns the definition/site/call identities, execution and records; this example
owns the SSE fields, mock tools, app labels and inspector. No dialect-specific
analysis or new core observation API is required.

## Observation adapter behavior

The adapter preserves `incomplete` when an observed step has no terminal evidence;
a successful invocation does not prove that step succeeded. The canvas marks this
state with a dashed border and question mark.

A recovering-feed gap emits a reset before replaying retained evidence. The
editor discards prior node status and approval state, along with call rows,
trace links and the old invocation output, then reports
that live evidence was lost. The host's applied display state remains intact.

Live completed and failed occurrences expose their admitted call IDs to the
inspector independently of the bounded durable effect summary. When a terminal
observation omits the call ID, the adapter matches its full occurrence identity
to the body-start binding retained in the existing overlay. Missing bindings
remain unavailable. Call-only updates do not overwrite a
node's aggregate status, and the visual editor merges repeated live/durable
updates by call ID. This improves continuous live inspection; it does not make
the retained history complete after a gap or restart. The runtime's existing
history cap and retention rules still apply. With the current runtime, this
example inherits `EvidenceLimits::process_effect_occurrences`' default of 64
retained effects per node; hosts can configure up to 4096. Live call inspection
continues independently of that retained summary.

Lash also offers a separate bounded tool-live feed through `core.tool_live()`.
Host tools publish through `AttemptContext::live()`; MCP tools use the same lane.
Events identify the execution owner, call and attempt, and support resumption
within their own retention window with explicit gaps. This example does not
yet subscribe to that feed or publish mock tool-live facts. Display progress
here remains the mock host's display state, separate from tool progress.
