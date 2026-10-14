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
canvas, curved connections, true/false paths, and a manual trigger. Click a tile
to edit its fields, or a connection's **+** to choose another action. Adding,
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

Visual settings edit literal records only when the effect's input comes from an
adjacent, single-use binding in the same block. The use check is conservative
across the document, so repeated names in independent scopes may also require
Advanced. Boolean conditions accept literals or the same safe binding case.
The host never follows a call to guess its result or edits a shared input record.
Add-step forms support the mocked Gmail, web search, message and status actions.
The helper chooses a fresh result binding for each visual insertion. A rejected
edit preserves the saved document and reports the error in the current panel.

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
node --test examples/workflow-graph-roundtrip/frontend/authoring-projection.test.mjs
```

These pin the unused-closure, opaque-library-call, shared/shadowed-input,
empty-scope and terminal-insertion rules. The HTTP regression
`each_edit_response_maps_its_immediate_base` pins per-transaction correspondence
across consecutive edits. Existing Graph scopes, Sites and Draft edit receipts
support this behavior; no Lash core API was added.

## Dataflow examples

The Examples menu includes **Deep research brief** and **Weekly business KPIs**.
Both are canonical kernel documents. Research maps the search result's topic and
sources into a cited brief. KPIs maps current and previous period records into a
comparison with calculated percentage changes. Their last step displays the
report summary. Change the first step's topic or workspace to try another run.
Research evidence, URLs and business metrics are synthetic fixtures; the mock
synthesis and comparison consume their actual mapped inputs.

Click an effect to see **Input mappings**. These display canonical variable and
member expressions from the same safe adjacent-record shape the editor supports;
unknown computations remain opaque. **Execution data** shows each retained call's
input and output, with separate occurrences for repeated calls. Execution log
links also open the relevant node. Data resets when the document changes or a
new run begins; this prototype does not persist a run-history browser.

The host attaches `callId` from `ProcessEffectOccurrence`, then reads the settled
record through `core.processes().tool_call`. Output display uses the public
`value_for_projection` API. Missing retained data is shown as unavailable. Lash
owns the definition/site/call identities, execution and records; this example
owns the SSE fields, mock tools, app labels and inspector. No dialect-specific
analysis or new core observation API is required.
