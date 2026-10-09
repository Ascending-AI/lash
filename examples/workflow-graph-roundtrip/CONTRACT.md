# Workflow round-trip backend contract

The backend listens on `http://127.0.0.1:3031` by default;
`WORKFLOW_GRAPH_ADDR` selects another `IP:PORT`. CORS permits `GET`, `POST`
and `OPTIONS` with a `content-type` header.

Property names of the example's own objects are camelCase. A kernel
document, a site, an edit and a correspondence are lash's own serialized
shapes and pass through unchanged (snake_case).

## Endpoints

| Method | Path | Response |
| --- | --- | --- |
| `GET` | `/workflows` | The built-in catalog: `[{ id, name, description }]` |
| `GET` | `/environment` | What a document must be written against: `{ effects, functions }` |
| `GET` | `/workflow` | The saved workflow as a `WorkflowView` |
| `POST` | `/workflow/select` | `{ id }`: save a built-in example as the next version; its `WorkflowView` |
| `POST` | `/workflow` | `{ document, entry }`: save a kernel document as the next version; its `WorkflowView` |
| `POST` | `/workflow/edits` | `{ version, edits }`: apply one edit transaction and publish; `{ workflow, correspondence }` |
| `POST` | `/run` | Start a run of the saved version; its `text/event-stream` |
| `POST` | `/approvals/{key}` | `{ approved }`: resolve a parked approval |
| `GET` | `/healthz` | `{ "service": "workflow-graph-roundtrip", "status": "ok" }` |
| `GET` | `/` and `/{path}` | Files from `frontend/` |

An error is `{ "error": { code, message, details } }`. An unknown catalog id
is HTTP 404 `unknown_workflow`; a stale `version` is HTTP 409
`version_conflict`; everything the host refuses about a request is HTTP 422.

## Environment

`effects` maps each effect name to the signature the host offers it under.
`functions` maps each library function's name to its identity. `POST
/workflow` fills both into the manifest of the document it is given, so a
client writes `effect display.show_message(input: Any) -> Any` and the host
replaces the signature; a library function is called by the identity read
here.

## WorkflowView

| Property | Meaning |
| --- | --- |
| `version` | The host's revision of the saved workflow. An edit names the version it was written against. |
| `entry` | The entry of the document a run starts. |
| `identity` | The content identity of the document: the base of an edit transaction, and the `definition` of a run event. |
| `definition` | The id of the definition lash admitted this version as. Absent when lash refused it; `notAdmitted` then says why. Such a version is saved and editable but cannot run. |
| `document` | The kernel document. |
| `text` | The document in kernel notation. |
| `statements` | The entry's statements in document order: `{ site, block, depth, summary, action? }`. `action` is the site of the statement's action, which is where a run reports. `summary` is this host's wording. |
| `executionSites` | The sites a run can report at: `{ site, statement, kind, loops }`. |
| `source` | The document as the TypeScript printer spells it. Read-only. Absent with a `sourceUnavailable` reason when the document has no TypeScript spelling. |

A site is `{ "unit": { "function": "onboarding" }, "path": [3, 0] }`.

## Editing

```http
POST /workflow/edits
Content-Type: application/json

{
  "version": 2,
  "edits": [
    { "replace_expression": {
        "expression": { "unit": { "function": "onboarding" }, "path": [3, 0, 0] },
        "with": { "literal": { "text": "Hello" } } } }
  ]
}
```

`edits` is a list of `lash::workflow::edit::Edit` values; every site in it
is a site of the saved version's document. The transaction applies whole or
not at all. On success the draft is saved as the next version and published,
and the response is `{ workflow, correspondence }`: `correspondence.entries`
lists each surviving node as `{ from, to, edited }`. A node of the old
version with no entry was removed; a site of the new version no entry ends
at is new.

A refused transaction is HTTP 422 `edit_refused` with
`details.diagnostics: [{ edit, site, message }]`, and the saved version is
unchanged.

## Running

`POST /run` answers an SSE stream. Each `run_event` is:

| Property | Meaning |
| --- | --- |
| `runId` | The process id. |
| `workflowVersion`, `definition` | The saved version the run started from and the identity of the document it executes. |
| `sequence` | The event's position in this stream. |
| `site` | The site the event is about, a site of that document. Absent for the run as a whole. |
| `status` | `started`, `waiting`, `succeeded` or `failed`. |
| `displayDelta`, `display` | What this event changed in the host's display, and the display after it. |
| `error` | Why a site or the run failed. |
| `approvalKey` | The key to resolve when the run is parked on `host.approval`. |

The last event has no `site` and carries the run's terminal status. A
`run_error` event carries a message and ends the stream.
