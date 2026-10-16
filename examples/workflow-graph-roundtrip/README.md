# Visual workflow editor

This example demonstrates a visual editor over Lash's canonical workflow
documents and typed edits. It consumes the public
`lash::workflow::authoring::{Operation, plan}` API to compose higher-level
closed value and step operations. `src/authoring_http.rs` and `frontend/index.html`
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
source in the input inspector. Double-click a tile (or select it and press
Enter) to edit its fields, and double-click an input to edit its value; a
single click only selects. A connection's **+** chooses another action. Adding,
editing, and deleting steps plan and validate a transaction, then save it through
the existing version-guarded endpoint. **Run workflow** executes the saved
document and plays the run out on the canvas.

## Navigation

The canvas navigates like Figma, with pointer events, so mouse, pen and touch
behave alike:

- **Pan**: drag the canvas with the left button, from anywhere: node positions
  are derived, so a drag that starts on a node pans too. Hold Space and drag
  (the hand cursor), drag with the middle button, scroll with two fingers on a
  trackpad, or drag with two fingers on a touch screen. The canvas is padded by
  half a view on every side, so it pans even when the graph fits.
- **Zoom** around the cursor with a mouse wheel, Ctrl/Cmd+wheel, or a trackpad
  or touch pinch, between 30% and 160%. Cmd/Ctrl+`=` and `-` zoom in and out,
  Cmd/Ctrl+`0` goes to 100%, Shift+1 fits the whole graph (as **Fit** and
  loading an example do), and Shift+2 frames the selection at up to 100%.
- **Clicks**: a press is a click until it moves more than 3px; past that it is
  a pan, which never selects. A click on empty canvas clears the selection.
- **Cursors**: `grab` over empty canvas, `grabbing` while panning, the normal
  pointer over nodes and ports.
- The canvas never selects text: it is `user-select: none`, and a press on it
  clears text selected elsewhere. The code pane, inputs and inspector fields
  keep text selection.

## Selecting and opening

Nodes, connections and code lines share two states (`frontend/selection.mjs`):

- **Selected**: a single click on a node, a connection or a linked code line.
  The item is outlined and its counterpart highlighted: a node's or
  connection's line in the code pane, a line's node on the canvas. The other
  pane scrolls to show the counterpart; the pane you clicked in stays put. No
  inspector or popup opens. Arrow keys move the selection to the nearest node
  in that direction in the current scope, or, in the code pane, to the next or
  previous statement line.
- **Open**: double-click, Enter on the selection, or the selected node's
  **Open** button opens its inspector. Opening a connection opens the input it
  feeds. Escape closes the inspector first, then clears the selection.

Hover is a subtle border change; selection an accent outline; an open item
adds a halo and the inspector panel.

## Runs on the canvas

**Run workflow** starts the saved version observed at statement counts
(`kernel_engine_defaults()?.observing(lash::process::Observe::Counts)` in the
settings it publishes). The run streams lash's overlay: per-site statuses from
the execution overlay, and the coverage `lash::workflow::view::coverage` computes
from the process overlay's counts, re-read whenever a segment's counts change.
On the canvas:

- the node running a tool call pulses, and the canvas follows it across
  scopes until you pan or scroll it yourself; a waiting step (a sleep, an
  approval) shows dashed;
- each executed node is marked done with its run count (×N); after the run,
  nodes that did not run are dimmed;
- a branch's taken edge is drawn solid green and its untaken edge faded;
- a tool node shows its result as a chip, a failed node turns red with its
  error, and the Return or Finish node shows the workflow's return value.
  Lash records no value for pure statements, so only tool calls and the
  return carry chips. Open a node for its full inputs and outputs.

A docked trace panel lists the run's steps in order: time, step, tool, result
and duration. Clicking a row selects its node and code line; double-clicking
opens it. When the run ends, the terminal event carries the path lash's replay
verified for each segment, mapped through `lash::workflow::view::path`; the
replay scrubber steps through it, moving the active marker and the counts. The
overlay stays until the document is edited or another run starts; **Clear
run** removes it, and **Trace** in the toolbar shows or hides the panel. There
is no results dialog.

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

Every example is authored in `catalog/` as TypeScript and as a Python twin,
written the way a person would write each, and imported at startup through
lash's public `import_workflow`, exactly as written: no kernel document is
written by hand and the host rewrites nothing, so the canvas shows lash's
current lowering of either dialect. The **Example** dropdown lists each one
once, grouped as apps or control-flow examples, with the statement and scope
count of its lowering in the chosen dialect (the same counts appear under the
canvas caption). Choosing one loads it at once. The **TypeScript | Python** toggle
beside it chooses which twin opens; an example whose Python twin lash refuses
keeps Python off, and the button's title is lash's refusal.

The code pane is read-only and shows the saved revision as:

- **Source**: the authored TypeScript or Python the workflow was imported
  from, as imported. Edits do not change it; the pane says when the revision
  was edited since. It is not linked to the canvas yet.
- **TypeScript**: lash's exact readable printer. Lines are linked through the
  printer's source map.
- **Kernel**: the kernel document text. The kernel printer starts every
  statement on its own line in document order; the pane matches each statement
  of lash's view to its line and checks its keyword. If any statement does not
  match, the tab is shown without linking.

Selecting a node marks the line its statement starts on and scrolls the pane to
it. Clicking a linked line selects that statement: the canvas switches to its
scope and scrolls to its node, without opening it; double-click the line to
open it. Edits and imports refresh the pane.

**Edit source** opens whole-source replacement for renames, declarations and
code changes. Import validates before replacing the document and resets site
selection. Canvas edits carry correspondence within a draft.

The TypeScript pane uses Lash's exact readable printer: named async functions,
awaited tool calls and ordinary expressions. The `k.document(...)` header carries
structured metadata needed for exact reconstruction; it is not an escaped JSON
document. Importing an unchanged export preserves the kernel document identity.
The host does not rewrite the printed source or recover dialect patterns.

Inputs support constants and in-scope data values. Records expose fields and a
whole-object editor. Arbitrary computation, constant promotion and renaming
belong in source. Explicit expansion is presentation state and follows
correspondence.

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

Terminal structure is edited in source. The canvas can set an existing literal
without inserting or replacing Return or Finish statements.

## Public authoring API

`lash::workflow::authoring::Operation` is the closed canvas vocabulary:
SetValue, InsertStep, RemoveStep, MoveStep, WrapSteps, UnwrapSteps and
ChangeCondition. Every operation carries the document identity it addresses.
`Transaction { base, edits }` contains those operations; `Draft::apply` admits
the whole batch or changes nothing. InsertStep accepts data arguments and an
optional result binding, not arbitrary statements or callbacks. Nested actions
that cannot be moved as a complete step require source.

```rust,ignore
use lash::workflow::authoring::{DataValue, Operation};
use lash::workflow::edit::Transaction;
let base = draft.identity();
let transaction = Transaction { base, edits: vec![Operation::InsertStep {
    base, at: insertion, effect: EffectName::new("research.search")?,
    args: vec![DataValue::Variable(Name::new("input"))],
    result: Type::Any, binding: Some(Name::new("evidence")),
}] };
let applied = draft.apply(&transaction, &environment)?;
```

Hosts own transport, revisions, layout and publication. The operation schema
and TypeScript types live in `schemas/host/kernel-authoring-operation/`.
`/workflow/plan` previews the same operations on a clone; `/workflow/edits`
saves them. Unknown operations have a typed `unsupported_canvas_edit` cause.
All edit diagnostics retain their typed `cause`, including checker payloads.

Cell-to-definition slicing uses
`extract_entry(&document, &entry, &environment)`. It returns an admitted document
and exact effects, with no raw cleanup transaction. The document/effects and
refusals have schemas under `kernel-entry-extraction` and `kernel-edit-refusal`.
Admission and correspondence in `Extraction.applied` stay in-process; the wire
omits them, and deserialization sets that field to `None`.

## Dialect independence verification

Run the focused host projection laws with:

```sh
node --test examples/workflow-graph-roundtrip/frontend/*.test.mjs
```

These pin the unused-closure, opaque-library-call, shared/shadowed-input,
empty-scope and terminal-insertion rules, zooming around the cursor and the
wheel/pinch/pan split, the click-versus-drag slop, two-finger pinch, arrow-key
neighbours and framing, the select/open/Escape state machine, the code pane's
statement-to-line linking and line stepping, and how lash's statuses, coverage
and replay path become node states and taken branches. The Rust law
`every_example_opens_as_its_imported_source_and_runs_with_the_mocks` imports
every catalog example and runs it against the mocks, and
`a_counted_run_reports_each_statement_as_often_as_its_loop_ran_it` pins a
counted digest run's loop and branch counts and its verified path. The HTTP regression
`each_edit_response_maps_its_immediate_base` pins per-transaction correspondence
across consecutive edits. The dataflow value-encoding law preserves integral
floats, negative zero and typed nested record values.

Lash's own laws (`lash-kernel-edit`'s `view_tests`) cover lexical source
choices and shadowing, callable return ownership and assignment identity. The
example's Rust laws cover the served view being Lash's, named invocation inputs
reaching effects and the final output, and imported parameterized entries. Browser checks exercise
data-value rewiring, type mismatch rejection, actual research and KPI
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
fields as connection sources. Double-click an input to edit its constant, select an
available source or use the data-value mapping. **Execution data** in an
open node shows each retained call's input and output, with separate occurrences
for repeated calls. Trace rows select the relevant node. The entry Return or Finish inspector
shows the actual terminal result. A nested function Return never borrows the
workflow's terminal result. Data resets when the document changes or a new run
begins; this example does not persist a run-history browser.

The backend serves `lash::workflow::view::derive` verbatim: Lash projects the
canonical document and graph into statements, scopes, dataflow and execution
sites. `frontend/dataflow.mjs` projects ports from that view and plans closed
canvas operations; it introduces no second persisted dataflow graph.
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

## Lowering shape ratchet (SHAPE)

Print the current table and check its ratchet with one command, from a sourced
Kiln fork:

```sh
kiln test //examples/workflow-graph-roundtrip:workflow-graph-roundtrip__unit_test --test_arg=catalog::shape::every_catalog_and_probe_workflow_stays_within_its_shape_ratchet --test_arg=--exact --test_arg=--nocapture --test_output=all
```

The instrument discovers every `.ts` and `.py` under `catalog/`, including the
original four probes in `catalog/probes/`. It uses each dialect's real module
lowering (TypeScript and Python alike), with the catalog's host effects and the
worker's helper library. It counts all document-owned statements, scopes
(including empty blocks) and actions: module roots, entry adapters, closures and
cloned bodies count; library helper bodies do not. This includes one module
statement and scope beyond the entry-view counts quoted in FIG-6411.

Author statements come from SWC and Ruff ASTs, including nested function bodies;
function/type declarations and braces are containers. Loop-header declarations
belong to their loop. Each kernel statement with a source note belongs to the
smallest author statement containing that note's span. Ownership is exclusive:
a loop's maximum measures its own expansion, without counting its child author
statements again. Unowned statements remain explicit `overhead`. The table gives
the statements/author ratio and the worst author's line, byte range and source.

`catalog/shape.ratchet` is sorted by relative path, one workflow per row, with
columns `statements scopes actions author max overhead`. A count increase fails;
a decrease also fails until the improving change lowers the corresponding
ratchet. The command prints replacement rows. Investigate increases rather than
raising a baseline to accept a regression. Corpus additions and removals must
update the ratchet in the same change. Source formatting changes need no update.
