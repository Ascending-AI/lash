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

Load an example, select a statement, and queue an effect helper. Queue a direct
edit between two helpers, preview their combined transaction, then apply it.
The preview validates a cloned draft without publishing. Applying uses the
existing version guard, typed transaction and admission path. Inspect the
generated transaction and canonical document to see the ordinary bindings the
helper creates. Run the workflow to see the actual recorded display effects.

The TypeScript editor imports a whole workflow into a new document. An import
refusal preserves the existing version. Export uses Lash's exact TypeScript
printer. Source import resets site selection because sites belong to the
replaced document; direct edits carry correspondence within a draft.

The nested example keeps branches visible and permits insertion in their blocks.
Canvas order indicates statement sequence, not implicit data wiring. Inputs are
explicit kernel expressions or literal JSON values converted by this host.

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
