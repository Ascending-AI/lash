# Workflow graph round-trip backend

This example is the Rust half of a visual Lashlang workflow editor. It exposes
the source → graph → edited graph → canonical source seam over HTTP, then runs
the saved version and streams node-correlated display events over SSE.

The backend owns in-memory editor versions. Run publishes the saved artifact
and process definition through `core.host_artifacts()`, then calls
`core.processes().start()` inside the host command's Restate handler. Restate executes the durable process over SQLite.
The overlay folds `core.processes().events()` and uses
`lash::process::trace_lashlang_process_map` to validate node identities.
Live process observation supplies transient node starts and waits. Display events
carry the stable tool-call ID used to correlate their deltas with observed nodes. Lashlang owns
graph projection, validation/rendering, and execution-site correlation. Canvas
layout is deliberately frontend-owned and never appears in source or API graph
documents.

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

SQLite process records live in `WORKFLOW_GRAPH_DATA_DIR`, default
`.workflow-graph`. The backend starts a local Restate server and retains it while
serving. Its journal lasts for that server's lifetime. Editor versions remain
in memory and reset when the backend restarts.

Display operations are leaf tools whose committed intents append
`workflow.display` events. The sample email, web, and agent tools return fixed
example data through the same engine dispatcher. They make no provider calls.
Sleeps use their authored duration. A signal wait remains pending until the
operator clicks **Send continue**, which calls
`POST /runs/{process_id}/signals/{name}` with a JSON payload. Closing the SSE
stream stops observation; it does not cancel the process.

## Coverage

The [example coverage matrix](../../runbooks/RULES.md#example-coverage-matrix) is the
source of truth for the CI split.

- **Manual judged:** [`workflow-editor-authoring`](../../runbooks/workflow-editor-authoring/runbook.md).
