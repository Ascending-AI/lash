# Workflow graph round-trip backend

This example is the Rust half of a visual Lashlang workflow editor. It exposes
the source → graph → edited graph → canonical source seam over HTTP, then runs
the saved version and streams node-correlated display events over SSE.

The backend owns in-memory versions and execution. Lashlang owns graph
projection, graph validation/rendering, and execution-site correlation. Canvas
layout is deliberately frontend-owned and never appears in source or API graph
documents.

Run it from the repository root:

```sh
CARGO_TARGET_DIR=/tmp/lash-workflow-graph \
  cargo run -p workflow-graph-roundtrip
```

The code default is `http://127.0.0.1:3031`. The conventional demo uses
`WORKFLOW_GRAPH_ADDR=127.0.0.1:3057`; set that variable to any available
`IP:PORT`. See [CONTRACT.md](CONTRACT.md) for the complete API used by the
frontend.

The server serves files from `frontend/dist/` (or directly from `frontend/`)
when present. A frontend dev server
can also run separately because the API permits cross-origin GET, POST, and
OPTIONS requests.

See the suite's
[workflow editor authoring runbook](../../runbooks/workflow-editor-authoring/runbook.md)
for the judged browser journey. [RUNBOOK.md](RUNBOOK.md) remains as a stable
compatibility link and records the deterministic integration command.

## Coverage

The [example coverage matrix](../../runbooks/RULES.md#example-coverage-matrix) is the
source of truth for the CI split:

- **Deterministic CI:** `Test docs + build cache` compiles all workspace targets,
  `Test shard ${{ matrix.shard }}/4` runs workspace tests, and `Lint` runs `Check workflow
  graph model`.
- **Full-host CI:** Partial. `Functional E2E (workflow-graph-roundtrip)` runs
  `workflow-graph-integration-verify` for the frontend production build, backend tests,
  and model check; it does not judge the browser journey.
- **Manual judged:** [`workflow-editor-authoring`](../../runbooks/workflow-editor-authoring/runbook.md).
