# Workflow round-trip backend

This example is a host that reads, edits, publishes, runs and shows workflows
through `lash::workflow` alone
([Editing, publishing and showing workflows](../../docs/workflow-hosts.md)).

The workflow is a kernel document. The backend keeps one `Draft` of it and
the version it last saved:

- **Catalog.** The built-in examples are kernel documents written in kernel
  notation (`src/catalog/documents.rs`). Opening one fills in the manifest
  this host admits it under: the signature the host offers each effect
  under, and the identity of each library function.
- **Editing.** `POST /workflow/edits` takes a list of kernel edits and
  applies it to the draft as one transaction. A refused transaction changes
  nothing and answers its diagnostics.
- **Publishing.** Every save publishes the draft with
  `core.host_artifacts().publish_workflow()` under a pin the version holds.
  The response carries the correspondence from the edited version's sites to
  the new one's.
- **Running.** Run calls `core.processes().start()` on the published
  definition. The durable engine executes the process over SQLite.
- **Showing.** The run view reads one snapshot from
  `core.processes().observe()` and follows its recovering feed. It folds the
  feed into lash's execution overlay over the document the process names
  (`core.host_artifacts().execution_document()`), and settles it with the
  committed end. Every run event names a site of that document.

TypeScript is a read-only lens: the source pane shows what the kernel
printer (`WorkflowDocument::typescript()`) spells for the document. Nothing
is imported from source.

The page in `frontend/index.html` is a renderer of that contract: the
entry's statements by site, the document as kernel text, TypeScript and
JSON, an edit-transaction box, and the run with its display. It has no build
step. Wording a statement ("perform display.set_status") is this host's
presentation; lash's document carries none.

Run it from the repository root:

```sh
just workflow-graph-roundtrip
```

The code default is `http://127.0.0.1:3031`; set `WORKFLOW_GRAPH_ADDR` to
any available `IP:PORT`. See [CONTRACT.md](CONTRACT.md) for the API and the
[workflow editor runbook](../../runbooks/workflow-editor-authoring/runbook.md)
for the browser journey.

SQLite process records live in the database file `WORKFLOW_GRAPH_SQLITE_PATH`,
default `.workflow-graph/lash.db`. The saved version is in memory and resets
when the backend restarts.

Display operations are ordinary host tools. The sample email, web and agent
tools return fixed example data through the same engine dispatcher. They
make no provider calls. Sleeps use their authored duration. The host's
`host.approval` tool parks until an operator resolves its completion key
with `POST /approvals/{key}` and `{ "approved": true }`. The key appears in
the SSE stream only after the process wait commits. Closing the SSE stream
stops observation and leaves the process running.

Display effects are host-owned and deduplicated by call id. The run view
applies one when the overlay completes its call from the recorded result.
This toy host keeps its call records in memory; a production host persists
its effect and approval records beside its external effects.
