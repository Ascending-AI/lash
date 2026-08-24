# Workflow graph backend contract

The backend listens on `http://127.0.0.1:3031` by default. The conventional
demo address is selected explicitly with
`WORKFLOW_GRAPH_ADDR=127.0.0.1:3057`; any other `IP:PORT` is also accepted.

```sh
CARGO_TARGET_DIR=/tmp/lash-workflow-graph \
  cargo run -p workflow-graph-roundtrip
```

All JSON property names are camelCase. CORS permits `GET`, `POST`, and
`OPTIONS` with a `content-type` header, so a Vite/Svelte dev server can call the
backend directly. A built frontend may instead place `index.html` and assets
in `examples/workflow-graph-roundtrip/frontend/dist/` (or directly in
`frontend/`) for the backend to serve.

## Endpoints

| Method | Path | Response |
| --- | --- | --- |
| `GET` | `/workflows` | Built-in catalog as a list of `WorkflowCatalogEntry` objects |
| `GET` | `/workflow` | Current saved workflow as a `WorkflowDocument` |
| `POST` | `/workflow/select` | Reset the current workflow to a built-in example and return its `WorkflowDocument` |
| `POST` | `/workflow` | Save a mutated `WorkflowDocument`; returns the new canonical version |
| `POST` | `/run` | Create a fresh invocation and return its `text/event-stream` |
| `GET` | `/healthz` | `{ "service": "workflow-graph-roundtrip", "status": "ok" }` |
| `GET` | `/` and `/{path}` | Optional files from `frontend/` |

## Built-in workflow catalog

`GET /workflows` returns the four built-in examples in display order:

```json
[
  {
    "id": "onboarding",
    "name": "Onboarding",
    "description": "A labeled onboarding flow with a signal wait, branch, and mixed display updates."
  },
  {
    "id": "traffic-lights",
    "name": "Traffic Lights",
    "description": "A visual red, amber, and green light sequence repeated twice."
  },
  {
    "id": "branching-approval",
    "name": "Branching Approval",
    "description": "An if-heavy approval flow with a visible signal wait and distinct outcomes."
  },
  {
    "id": "counter-loop",
    "name": "Counter Loop",
    "description": "A structured while loop followed by an editable for container and progress updates."
  }
]
```

Select an example with its catalog ID:

```http
POST /workflow/select
Content-Type: application/json

{ "id": "traffic-lights" }
```

A successful selection resets the current workflow to that example's canonical
source, appends a new in-memory version, and returns the resulting
`WorkflowDocument` in the same shape as `GET /workflow`. Subsequent
`GET /workflow`, `POST /workflow`, and `POST /run` requests use that selected
version. Selecting an example again discards any edits to the current draft and
loads the built-in source again.

An unknown ID returns HTTP `404`:

```json
{
  "error": {
    "code": "unknown_workflow",
    "message": "workflow example `missing` does not exist",
    "details": { "id": "missing" }
  }
}
```

## WorkflowDocument

`GET /workflow`, a successful `POST /workflow`, and a successful
`POST /workflow/select` return this exact shape:

```json
{
  "schemaVersion": 3,
  "version": 1,
  "source": "canonical Lashlang source",
  "nodes": [
    {
      "id": "call:stable-id",
      "type": "call",
      "parentId": "process:stable-id",
      "data": {
        "kind": "call",
        "title": "show_message",
        "description": "optional @label description",
        "nameSource": "label",
        "operation": "show_message",
        "effect": "sleep",
        "fields": { "text": "Welcome", "pct": 35 },
        "binding": "optional assignment binding",
        "target": "state.count",
        "expression": "state.count + 1",
        "condition": "state.count < 3",
        "iterable": "[70, 85, 100]",
        "clauses": [
          { "kind": "for", "binding": "item", "iterable": "items" },
          { "kind": "if", "condition": "item.enabled" }
        ],
        "source": "one canonical opaque statement, when fallback is required",
        "children": [
          {
            "slot": "then",
            "scope": "container:stable-id:then",
            "nodeIds": ["call:child-id"]
          }
        ]
      }
    }
  ],
  "edges": [
    {
      "id": "sequence:stable-id",
      "source": "call:source-id",
      "target": "effect:target-id",
      "data": {
        "kind": "sequence",
        "scope": "process:stable-id",
        "variable": "optional data-edge variable",
        "version": 1
      }
    }
  ],
  "roots": {
    "main": [],
    "processes": ["process:stable-id"]
  }
}
```

Optional properties are omitted, so a real call node has `operation` but no
`effect`, expression slot, `source`, or `children`. The combined example above
shows every possible property in one place.

`schemaVersion: 3` is the clean-cutover contract in which every expression
owned by a structured Lashlang graph node is canonical editable text. Version
2 serialized retained AST payloads and is not accepted by the v3 renderer.

Node `type` and `data.kind` use `process`, `data`, `call`, `effect`,
`computation`, `state_update`, `terminal`, `container`, or `opaque`.
`data.effect` uses `start_process`,
`await_join`, `signal_run`, `wait_signal`, `sleep`, `cancel`, `print`, `yield`,
`wake`, `break`, or `continue`. `data.nameSource` is `label` for an authored
`@label` and `derived` for an automatic name. It is required on every node and
tags the name: a `label` name carries `data.title` and an optional
`data.description`, a `derived` name carries only the recomputed `data.title`.
A missing or unrecognized `nameSource` is a decode error, never a `derived`
name — a title sent without its tag would otherwise be dropped on save.

`data.fields` contains JSON-editable literal arguments. In the default graph
that includes strings such as `text`, `key`, `value`, `name`, `state`, `list`,
`item`, and `target`; numeric `pct`; sleep `duration`; and wait `signal`.
Values may be null, booleans, numbers, strings, lists, or objects. An opaque
node is edited through `data.source` as one complete Lashlang statement.

Structured expression slots are canonical Lashlang text rather than AST JSON:

- `data.condition` is present on `if` and `while` containers.
- `data.iterable` is present on `for` containers.
- `data.clauses` exposes every list-comprehension `for` iterable and `if`
  condition, plus each clause binding.
- `data.target` and `data.expression` are present on `state_update` nodes.
- `data.binding` and `data.expression` are present on `computation` nodes.

Bindings are also returned on other assignment-producing structured nodes.
On save, these strings replace the graph payload. Lashlang parses each string
back into its typed AST field, validates that it matches the owning node kind,
and only then runs the canonical printer. No original AST expression is used
as a fallback.

Containers carry ordered child groups in `data.children`. `roots.processes`
lists the top-level process containers. A process container uses slot `body`;
`if` uses `then` and `else`; `for` and `while` use `body`; list comprehension
uses `element`. `parentId` is supplied for SvelteFlow
nesting. `roots` and each `children[].nodeIds` are the source order and are
therefore also the reorder controls.

Edges have `data.kind` equal to `sequence` or `data`. Data edges additionally
carry `variable` and SSA `version`. `data.scope` identifies the subgraph that
owns the edge and must be preserved.

There is intentionally no `position` property anywhere. The frontend should
auto-layout, then persist dragged positions outside this document keyed by
node ID (for example, in `localStorage`).

## Saving

Send the complete mutated `WorkflowDocument` back as JSON:

```http
POST /workflow
Content-Type: application/json
```

The editable surface is:

- `data.title`, `data.description`, and `data.nameSource`; set `nameSource` to
  `label` to author a label, and send it with every node. A `derived` title is
  recomputed after save and must not carry a description.
- `data.fields` literal values.
- `data.binding`, `data.target`, `data.expression`, `data.condition`,
  `data.iterable`, and `data.clauses` canonical Lashlang text where present.
- Opaque `data.source`.
- `nodes`, `edges`, `roots`, and `children[].nodeIds` for delete/reorder edits.

The structured text fields and their render-time validation are:

| Field | Accepted node kinds | Typed error for an invalid edit |
| --- | --- | --- |
| `data.condition` | `if`, `while` | `invalid_expression` |
| `data.iterable` | `for` | `invalid_expression` |
| `data.binding` | `data`, `call`, `effect`, `computation`, `if`, `for`, `list_comprehension` | `invalid_assignment_target`; a syntactically valid non-simple binding is `invalid_node_payload` |
| `data.target` | `state_update` | `invalid_assignment_target` |
| `data.expression` | `computation`, `state_update` | `invalid_expression` |
| `data.clauses[].iterable` / `.condition` | `list_comprehension` `for` / `if` clauses | `invalid_expression` (`clause iterable` or `clause condition`) |
| `data.clauses[].binding` | `list_comprehension` `for` clauses | `invalid_assignment_target` (`clause binding`); a syntactically valid non-simple binding is `invalid_node_payload` |

Changing `schemaVersion` yields `unsupported_schema_version`. Removing a
required child group yields `missing_required_child`: `if` requires `then` and
`else`, `for` and `while` require `body`, and `list_comprehension` requires
`element`.

To delete a node, remove it from `nodes`, remove its ID from its root or child
group, and remove incident edges. To reorder nodes, reorder the relevant root
or child `nodeIds`. A save is optimistic: the submitted `version` must still be
current.

The backend rebuilds a typed `WorkflowGraph`, calls
`workflow_graph_to_source`, stores a new in-memory version, then reprojects it.
The success response is a new `WorkflowDocument` with incremented `version`,
canonical `source`, and newly projected node IDs. Replace the frontend's whole
document with this response before Play.

Invalid graph edits return HTTP `422`:

```json
{
  "error": {
    "code": "invalid_expression",
    "message": "node `...` has invalid `condition` expression text: ...",
    "details": { "nodeId": "container:...", "field": "condition", "reason": "..." }
  }
}
```

Typed render codes are `unsupported_schema_version`, `duplicate_node_id`,
`unknown_node_reference`, `missing_required_child`, `invalid_node_payload`,
`invalid_expression`, `invalid_assignment_target`, `invalid_opaque_source`,
`duplicate_process_name`, `canonical_source`, and `rendered_source_invalid`.
Malformed host DTO structure uses
`invalid_graph_document`. A stale save returns HTTP `409` with
`version_conflict`.

## Running and SSE

`POST /run` snapshots the current saved version, creates a new run, and
responds with `Content-Type: text/event-stream`. Consume it with streaming
`fetch`; browser `EventSource` cannot issue POST. Each Play click must make a
new POST.

Every SSE frame is named `run_event`; its SSE `id` equals `sequence`:

```text
event: run_event
id: 7
data: {"runId":"uuid","workflowVersion":2,"sequence":7,"nodeId":"call:stable-id","status":"succeeded","displayDelta":{"messagesAppended":["Welcome"]},"display":{"messages":["Welcome"],"statuses":{},"lists":{},"lights":{},"progress":0.0}}
```

The data JSON shape is:

```json
{
  "runId": "fresh UUID per POST",
  "workflowVersion": 2,
  "sequence": 7,
  "nodeId": "call:stable-id",
  "status": "started",
  "displayDelta": {
    "messagesAppended": ["message"],
    "statuses": { "phase": "starting" },
    "listItemsAppended": { "steps": ["Approved"] },
    "lights": { "ready": "green" },
    "progress": 35.0,
    "highlighted": "checklist"
  },
  "display": {
    "messages": ["message"],
    "statuses": { "phase": "starting" },
    "lists": { "steps": ["Approved"] },
    "lights": { "ready": "green" },
    "progress": 35.0,
    "highlighted": "checklist"
  },
  "error": "present only for failed"
}
```

`status` is `started`, `succeeded`, `waiting`, or `failed`. Empty delta fields
are omitted; `displayDelta` itself is always present. `display` is the full
state after that event, which lets a client either apply deltas or replace its
view. Sleep and `wait_signal` emit `waiting`. The host auto-fires the declared
`continue` signal after 650 ms. Loop body nodes can emit multiple occurrences.
Stream EOF means the run is complete; the correlated
terminal node's `succeeded` event is the final normal event.

Every emitted event has a `nodeId` obtained through
`node_id_for_execution_site` and refers to a node in the exact saved graph
version identified by `workflowVersion`. A preparation failure before SSE
starts is JSON with HTTP `500`; an execution failure is a correlated `failed`
event before EOF.

## Toy display tools

The in-process `display` module has no network or external dependencies:

| Lashlang call | Persistent effect |
| --- | --- |
| `display.show_message({ text })` | Append to `display.messages` |
| `display.set_status({ key, value })` | Set `display.statuses[key]` |
| `display.add_item({ list, item })` | Append to `display.lists[list]` |
| `display.set_light({ name, state })` | Set `display.lights[name]` |
| `display.set_progress({ pct })` | Clamp and set progress to 0–100 |
| `display.highlight({ target })` | Set the highlighted panel/node |
