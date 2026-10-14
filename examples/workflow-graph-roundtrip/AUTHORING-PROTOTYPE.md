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

The canvas projects effect calls and effect-containing conditions from the
canonical statements. It hides lowering scaffolding such as temporary bindings,
boundary conversions, and async closure wrappers. Connections show execution
order and conditional paths; they are not editable data wires. This projection
is a prototype for the included examples, not a complete visual language for
arbitrary kernel programs. Computed inputs and conditions remain available in
Advanced. Node positions are derived, not persistent user layout.

**Advanced** contains TypeScript import/export, the kernel document, and the
original helper/direct-edit transaction experiment. Queue a direct edit between
two helpers, preview their combined transaction, then apply it. The preview
validates a cloned draft without publishing. Generated statements remain
available to direct edits. Import validates before replacing the document and
resets site selection; direct edits carry correspondence within a draft.

Visual settings currently edit literal input records and boolean conditions.
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
