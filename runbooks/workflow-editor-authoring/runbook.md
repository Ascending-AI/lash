# Workflow corpus: import, edit, read and display

Read [the shared rules](../RULES.md). This browser pass uses
`examples/workflow-graph-roundtrip` and the committed Design 8 corpus. It
opens no RLM session and calls no provider or external service. `corpus.echo`
is the shared mock tool: it returns its input and displays it. Cell witnesses
are recorded corpus artifacts; displaying one does not execute a session.

## Start and evidence

Source `env.sh`, then run `just workflow-graph-roundtrip` under the host's
8-GiB memory cap, with `WORKFLOW_GRAPH_SQLITE_PATH` inside this run's evidence
directory. The launcher uses `kiln gate` and prints its private URL.
Poll `/healthz` for `status: ok`. Launch Chromium without a virtual-memory cap
and record its peak RSS with `/usr/bin/time -v`. Stop this run's server at the
end. No credentials are needed.

Save HTTP replies, SSE, a screenshot at each named checkpoint, actual counts,
browser errors and the final scorecard in the run's evidence directory.
A refusal or timeout aborts the pass: capture the page/API discrepancy and
report its stage. Do not repair during a recorded pass.

## Inspect every corpus case in each dialect

Read `/workflows` and compare its ids and order with
`examples/workflows/catalog.json`. Iterate every case in both **TypeScript**
and **Python** using the picker and dialect buttons. Record the actual number
of inspected module, refusal and cell views; do not infer success from an exit
code. Save `<case>-<dialect>.png` at every checkpoint.

For a module, capture `/workflow/select` and a fresh `/workflow`. Require equal
revision, identity, definition and entry, no admission refusal, and canvas
nodes from the returned view. The document's entry names must match the corpus
contract, including both entries of `entry-boundary` and no block-local entries.
Select **Source** and require exact source text, a held `sourceMapId`, and a
map naming the current identity. Click a linked source line and require a
corresponding selected canvas node. Inspect the non-ASCII case in both languages:
Japanese and the crab character must remain intact and selection must land on
the right line. Library action titles use the returned `functionLabels` keyed
by the view's callee name; roles remain presentation owned by the example.

For a refusal, require the dialog to show its exact source and named diagnostic,
then verify an explicit `/workflow/source` import returns 422 with that code
and leaves the saved identity/revision unchanged. For a cell, require the dialog
to show its source and crossing/finish witness. The saved revision must not
change. Do not claim that the browser executed a cell.

## Import, edit, read and display in both languages

For each language select `straight-tool`. Open **Edit source**, select
**Workflow module**, verify the language selector and import the unchanged
source. Capture the request's `dialect` and the returned source/map.
Save `import-<dialect>.png` and the HTTP replies.

In **Printed document** mode, read the unchanged output. Require equal
canonical document and identity, and that the printed lens still uses the
selected language. Save `read-<dialect>.png`.

Re-import the authored module. Find the `corpus.echo` action in the returned
view. Preview and apply a `set_value` on its first tool argument to `edited λ`.
Require matching preview/applied documents, revision +1, changed identity,
correspondence from the immediate base, and no authored map/id after the edit.
The Source pane must explain that it no longer links this revision; Printed
and Kernel still link. Save `edit-<dialect>.png` and both replies.

Press **Run workflow** and record SSE. Require terminal `succeeded`, result
`edited λ`, and the same text in the rendered output display. Missing call data
must stay unavailable. Save `display-<dialect>.png` and `run-<dialect>.jsonl`.

Finally capture the current editor at desktop and narrow mobile widths. Require
usable source controls and no document-level horizontal overflow.

## Scorecard

Record each gate separately: catalog membership; module views; refusal codes
and atomicity; cell source/witness inspection; non-ASCII linking; language
selection on import; exact printed reads; preview/replay; map invalidation;
terminal/output/display agreement; desktop/mobile usability; teardown.
Give observed numerator/denominator counts, screenshot paths and API evidence.
State any abort or unobserved property explicitly. This pass does not certify
restart restoration, durable cell execution, or historical-run browsing.
