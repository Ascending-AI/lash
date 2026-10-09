# Workflow graph round-trip backend

This example is the Rust half of a visual Lash VM workflow editor. It reads,
edits, publishes, runs and shows workflows through `lash::workflow` alone
([Editing, publishing and showing workflows](../../docs/workflow-hosts.md)).

The workflow is Lash's typed document. The backend keeps one
`WorkflowDraft` per workflow and in-memory editor versions over it:

- **Editing.** The canvas forms cover the common nodes: calls, effects,
  values, `if`, `for`, `while`, `try`, blocks and `throw`. The form records
  typed operations against the draft, including each insert, move and removal. The
  structured editor reaches everything else: it lists any node's statement
  and its expressions (a closure's body, a computed target) and replaces one
  by its slot path, as typed IR.
- **Publishing.** Every save publishes the draft with
  `core.host_artifacts().publish_workflow()`. Lash admits the IR in its VM
  workers and the version holds the new definition under a pin. Node ids
  change across a save; the response maps old ids to new ones from Lash's
  edit correspondence.
- **Running.** Run calls `core.processes().start()` on the published
  definition. The durable engine executes the process over SQLite.
- **Showing.** The run view reads one snapshot from
  `core.processes().observe()` and follows its recovering feed. It folds
  the feed into Lash's execution overlay over the document the process
  names (`core.host_artifacts().execution_document()`), and settles it with
  the committed end.

TypeScript is an optional lens. The built-in examples are imported from
TypeScript, and the source pane shows the workflow's canonical TypeScript and
imports edits of it. The host retains its `lash/typescript` dependency for the built-in source
catalog, source import/display, fragment validation and text fields in forms.
The generic IR editor, admission, publication and execution use the typed
workflow directly. Import is an explicit save request choice; source views
never decide whether a save is an import.

Display events carry the stable tool-call ID used to correlate their deltas
with observed nodes. Canvas layout is frontend-owned and never appears in
API documents.

Run the frontend and backend from the repository root:

```sh
just workflow-graph-roundtrip
```

The code default is `http://127.0.0.1:3031`. The conventional demo uses
`WORKFLOW_GRAPH_ADDR=127.0.0.1:3057`; set that variable to any available
`IP:PORT`. See [CONTRACT.md](CONTRACT.md) for the complete API used by the
frontend.

The recipe builds the frontend and starts the judged backend with its
repository-relative frontend lookup. For the local integration check, run
`just workflow-graph-integration-verify`.

The server serves files from `frontend/dist/` (or directly from `frontend/`)
when present. A frontend dev server
can also run separately because the API permits cross-origin GET, POST, and
OPTIONS requests.

See the suite's
[workflow editor authoring runbook](../../runbooks/workflow-editor-authoring/runbook.md)
for the judged browser journey. [RUNBOOK.md](RUNBOOK.md) remains as a stable
compatibility link and records the deterministic integration command.

SQLite process records live in the database file `WORKFLOW_GRAPH_SQLITE_PATH`,
default `.workflow-graph/lash.db`. Editor versions remain in memory and reset
when the backend restarts.

Display operations are ordinary host tools. The sample email, web, and agent tools return fixed
example data through the same engine dispatcher. They make no provider calls.
Sleeps use their authored duration. The host's `approval` tool parks until
an operator resolves its completion key with
`POST /approvals/{key}` and `{ "approved": true }`. The key appears in the SSE
stream only after the process wait commits. Closing the SSE stream stops
observation and leaves the process running.

Display effects are host-owned and deduplicated by `call_id()`. The overlay
applies them when language observation completes the call from its recorded result. This toy host keeps its
call records in memory; a production host persists its effect and approval
ledger, authorizes access to completion keys, and chooses its own deadlines.

## Coverage

The [example coverage matrix](../../runbooks/RULES.md#example-coverage-matrix) is the
source of truth for the CI split.

- **Manual judged:** [`workflow-editor-authoring`](../../runbooks/workflow-editor-authoring/runbook.md).
