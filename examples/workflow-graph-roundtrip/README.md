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
(`kernel_engine_defaults()?.observing(lash::process::ObservationMode::Counts)` in the
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

The picker comes from [the shared corpus](../workflows/README.md). It lists
workflow modules, cell crossings and module refusals. Both dialects import
unchanged through `HostArtifacts::import_workflow`. Module counts come from
`view::derive`; entry names follow the shared module contract. Cell and refusal
cases open their source and recorded witness without replacing the saved workflow.
The shared `workflow_corpus::tools` declaration binds `corpus.echo` for data and `corpus.capture` for a nested function; the mock
returns its argument and displays it. No external service is called.

The read-only pane offers three lenses:

- **Source** shows the imported text read with its map from stored artifacts.
  The host keeps the publication's source-map id beside the definition id.
  Browser linking converts UTF-8 offsets to UTF-16 offsets before selecting
  lines. Authoring edits keep the historical text and drop its map.
- **Printed** shows the selected dialect's exact printer output and fresh map.
- **Kernel** shows canonical text with the view's canonical line ranges.

The canvas derives from the stored document and its retained declarations;
reading a saved view does not link against the current registry. Library action
labels join `CalleeView::Library.name` against `FunctionLabels`; an unknown name
shows itself. Roles, layout and wording remain host-owned.

**Edit source** distinguishes **Workflow module** from **Printed document**.
Choose the module's language explicitly before importing authored source.
Printed source reads against the current document with its printer format;
reading it unchanged preserves canonical identity. The language of a printed
lens follows the imported revision, including after canvas edits.

## Captions

A caption is a step's plain-language title and optional description. The
corpus writes one on every workflow, step and branch, and each dialect has
its own spelling. In TypeScript it is a JSDoc block right before the
function, statement or `else`/`catch`/`finally` keyword it names:

```ts
/** Send the weekly report */
export async function report(team: string) {
  /** Email the team */
  await mail.send({ to: team });
}
```

In Python it is `#:` lines directly above, at the same indentation; lines
after the first are the description:

```python
#: Send the weekly report
async def report(team: str):
    #: Email the team
    #: Only the leads receive it.
    await mail.send(to=team)
```

The host keeps a version's `Captions` beside its document and source map
(`SavedWorkflow::captions`); lash stores none. Importing a module or reading
printed source replaces them with the ones that source spells.

- **On the canvas.** A step's caption is its tile's title, with the host's
  own wording (the tool's name, `If value < 0` for a condition) underneath;
  a step without one shows that wording as its title. The tiles come from
  lash's join, `WorkflowView::captions`: an action or control tile shows its
  statement's caption, a code tile every caption among its statements. A
  branch's caption names its button and its entry in the scope menu, and the
  entry function's caption is the workflow's title in the header.
- **Through canvas edits.** An edit moves the captions with their steps
  (`Captions::rekey`). The ones whose step the edit changed, or anything
  inside it, get a **Review** badge: "Email the team" may no longer fit once
  its recipient changed.
- **Renaming.** Open a step to rename, add or remove its caption; open
  **Workflow Input** for the workflow's title. Saving sends the whole new
  caption list to `POST /workflow/captions`, which builds a new `Captions`
  value, saves it as the next version of the same document and clears that
  caption's review mark. Lash refuses text no dialect can spell (`*/`, a line
  starting with `@`, a title over 200 bytes) and nothing changes.
- **In Edit source.** Once a version has a canvas edit or a caption change
  its imported module lacks, the editor opens on lash's print of the document
  with its captions (`print(document, Some(&captions), options)`), so reading
  it back keeps them. **Writing captions** shows the dialect's own guide
  (`CaptionSupport::guide`).

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

## Verification

The focused HTTP laws cover captions through import, edit, rename and a
printed read, retained source maps, both dialects' imports,
printed read identity, exact edit preview/replay, display output, catalog entry
and refusal contracts, and views derived from stored declarations. The shared
corpus law pins readable per-dialect outputs and counts. It compares entry names
and refusal codes across dialects; it does not require their values to agree.

```sh
. ./env.sh
kiln test //examples/workflow-graph-roundtrip:roundtrip__test --test_arg=--exact --test_arg=authored_source_is_read_from_its_published_map --test_arg=corpus_catalog_imports_both_dialects_and_draws_the_stored_read --test_arg=captions_follow_import_edit_rename_and_printed_read
kiln gate lash "$(basename "$PWD")" -- node --test --test-name-pattern=EXAMPLE-SOURCE examples/workflow-graph-roundtrip/frontend/codepane.test.mjs
kiln gate lash "$(basename "$PWD")" -- node --test examples/workflow-graph-roundtrip/frontend/captions.test.mjs
```

The [browser runbook](../../runbooks/workflow-editor-authoring/runbook.md)
inspects every corpus case in both dialects and exercises import, edit, read
and display through the browser. It records observed counts and screenshots;
it is product verification, not a correctness gate.

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

Saved workflow source is standalone: print, edit the text, then ordinarily import and independently publish under the chosen environment. Typed editing uses Draft operations on the materialized document.
