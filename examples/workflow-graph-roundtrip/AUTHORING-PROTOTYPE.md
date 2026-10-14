# Workflow authoring experiment

This throwaway branch tests whether an optional kernel authoring library is a
useful boundary between hosts and typed edits. The portable candidate lives in
`src/authoring_prototype.rs`; `src/prototype_host.rs` and
`frontend/authoring-prototype.html` are its example host. No new production API
or wire compatibility contract is introduced.

From the prototype's Kiln fork, run:

```sh
bash examples/workflow-graph-roundtrip/run-authoring-prototype.sh
```

Open the printed URL. The script chooses a private port from `KILN_GATE_ID` and
creates a scratch SQLite database under `/tmp`. Workflow editing state lives in
memory. The existing Lash runtime executes the examples; email and display tools
are deterministic mocks. No model or external email service is called.

The default screen is a visual workflow editor: app action tiles on a dotted
canvas, named and typed ports, true/false paths, and explicit workflow input and
Return or Finish steps. Blue connections carry values; dashed gray connections show execution
order. Click an output port and then an input port to connect them, or select a
source in the input inspector. Click a tile to edit its fields, or a connection's
**+** to choose another action. Adding,
editing, and deleting steps plan and validate a transaction, then save it through
the existing version-guarded endpoint. The canvas supports panning and zooming.
**Run workflow** executes the saved document and opens its recorded outputs.

The canvas scope selector uses blocks from Lash's canonical derived graph,
including empty bodies. It defaults to the selected entry's own block. Function
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

**Advanced** contains TypeScript import/export, the kernel document, and the
original helper/direct-edit transaction experiment. Queue a direct edit between
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
can stay inline or become named value nodes. Promoting a constant inserts a
binding and replaces its use in one transaction. Named values can be renamed;
record values expose fields and a whole-object editor. Adjacent, single-use,
unassigned and uncaptured input records can appear inside their consuming action.
Shared records keep their own nodes. Explicit expansion is browser presentation
state and follows edit correspondence.

The backend derives binding identity, lexical availability, expression sites,
assignment destinations and callable ownership from the canonical graph. The
frontend consumes that metadata without recognizing dialect syntax. Known type
mismatches are rejected; unknown types remain `Any`. Inferred display types do
not narrow an unconstrained binding or replace a declared return contract.
Assignment nodes show state writes, and closure returns show function outputs.
Scope inputs expose enclosing bindings without turning them into workflow entry
parameters. Running always invokes the entry.

Add-step forms support the mocked Gmail, web search, message and status actions.
The helper chooses a fresh result binding for each visual insertion. A rejected
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

## API experiment

`plan(&Draft, Vec<Step>)` produces an ordinary `Transaction`. `Step::Effect`
accepts a base-document position, effect name, expression arguments, result
type and explicit result binding. `Step::Direct` accepts any existing `Edit`.
Computed arguments become visible bindings before the effect statement.
Generated temporary names avoid names throughout the base and complete batch.
The planner performs no I/O, publication, admission or runtime execution.

The same helpers accept documents imported from TypeScript or changed by direct
edits. Hosts retain their own templates, HTTP contracts, revisions, layout and
publication policy. The production candidate would belong alongside
`lash-kernel-edit`, exposed through `lash::workflow`; this branch leaves it in
the example until the experiment is judged.

All edit targets are sites in the transaction's base document. Original nodes
remain targetable after preceding insertions in the same batch. A newly inserted
node receives a targetable site after admission; edit it in the next transaction,
or construct its desired AST before insertion. Explicit output binding names
and raw transformations remain caller choices, checked by normal admission.
This prototype does not introduce persistent step groups, stable wrapper
objects, a second graph model or unrestricted same-batch references to new nodes.

## Dialect independence verification

Run the focused host projection laws with:

```sh
node --test examples/workflow-graph-roundtrip/frontend/authoring-projection.test.mjs examples/workflow-graph-roundtrip/frontend/authoring-dataflow.test.mjs
```

These pin the unused-closure, opaque-library-call, shared/shadowed-input,
empty-scope and terminal-insertion rules. The HTTP regression
`each_edit_response_maps_its_immediate_base` pins per-transaction correspondence
across consecutive edits. Existing Graph scopes, Sites and Draft edit receipts
support this behavior; no Lash core API was added. The dataflow value-encoding
law preserves integral floats, negative zero and typed nested record values.

Focused Rust laws cover lexical source choices and shadowing, callable return
ownership and assignment identity, named invocation inputs reaching effects and
the final output, and imported parameterized entries. Browser checks exercise
promotion, renaming, rewiring, type mismatch rejection, actual research and KPI
executions, output reset between runs and mobile layout.

## Dataflow examples

The Examples menu includes **Deep research brief** and **Weekly business KPIs**.
Both are canonical kernel documents. Research maps the search result's topic and
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
begins; this prototype does not persist a run-history browser.

`src/dataflow.rs` derives the view from the canonical document and graph.
`frontend/authoring-dataflow.mjs` projects ports and plans ordinary canonical
edits. Neither changes Lash core or introduces a second persisted dataflow graph.
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

A recovering-feed gap emits a reset before replaying retained evidence. Both
example clients discard prior node status and approval state. The visual editor
also discards call rows, trace links and the old invocation output, then reports
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
prototype inherits `TraceLimits::process_effect_occurrences`' default of 64
retained effects per node; hosts can configure up to 4096. Live call inspection
continues independently of that retained summary.

Upstream MCP progress and notifications are live-only, correlated by call and
attempt on the session observation feed. These examples use host-owned mocked
tools, not MCP servers, so they do not emit MCP notifications. Display progress
here is the mock host's display state; it is not generic tool partial output.
