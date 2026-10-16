# Workflow round-trip backend contract

The backend listens on `http://127.0.0.1:3031` by default;
`WORKFLOW_GRAPH_ADDR` selects another `IP:PORT`. CORS permits `GET`, `POST`
and `OPTIONS` with a `content-type` header.

Property names of the example's own objects are camelCase. A kernel
document, a site, an edit, an authoring operation, a correspondence and a workflow view are lash's
own serialized shapes and pass through unchanged (snake_case).

## Endpoints

| Method | Path | Response |
| --- | --- | --- |
| `GET` | `/workflows` | The example catalog: `[{ id, name, description, group, statements, scopes }]` |
| `GET` | `/environment` | What a document must be written against: `{ effects, functions }` |
| `GET` | `/workflow` | The saved workflow as a `WorkflowView` |
| `POST` | `/workflow/select` | `{ id }`: save an example's imported document as the next version; its `WorkflowView` |
| `POST` | `/workflow` | `{ document, entry }`: save a kernel document as the next version; its `WorkflowView` |
| `POST` | `/workflow/edits` | `{ version, edits }`: apply one edit transaction and publish; `{ workflow, correspondence }` |
| `POST` | `/workflow/source` | `{ version, source, entry, dialect? }`: import and admit source (`typescript` by default, or `python`) before replacing the current document |
| `POST` | `/workflow/plan` | `{ version, operations }`: preview Lash authoring operations without publishing; `{ version, transaction, preview, correspondence }` |
| `POST` | `/run` | `{ version?, inputs? }`: start a run with named input values; its `text/event-stream` |
| `POST` | `/approvals/{key}` | `{ approved }`: resolve a parked approval |
| `GET` | `/healthz` | `{ "service": "workflow-graph-roundtrip", "status": "ok" }` |
| `GET` | `/` and `/{path}` | Files from `frontend/` |

Every example is authored twice in `catalog/`, as TypeScript (`<name>.ts`)
and as its Python twin (`<name>.py`), and both are imported once at startup
through `HostArtifacts::import_workflow`, exactly as written. A catalog
entry's `group` is `app` or `controlFlow`; `statements` and `scopes` count the
statements and blocks of lash's view of its TypeScript entry, the size of what
the import lowered it to; `python`, when present, holds the same
`{ statements, scopes }` for the Python twin. `pythonRefused`, when present,
is lash's refusal of the Python twin, which then cannot be opened. A TypeScript example lash
refuses to import stops the host at startup. `POST /workflow/select` takes
`{ id, dialect? }`, `typescript` by default; a revision's `authored` names the
`dialect` of its text.

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
| `view` | Lash's canvas read of the entry, exactly as `lash::workflow::view::derive` answers it: see [The view](#the-view). Absent when the document has none; `viewUnavailable` then says why. |
| `source` | The exact TypeScript export. Editing this text has no effect until `POST /workflow/source` admits it. Absent with a `sourceUnavailable` reason when the document has no TypeScript spelling. |
| `sourceMap` | Lash's `SourceMap` for `source` (snake_case, lash's shape): UTF-8 byte ranges of `source` for each site of the document. |
| `authored` | `{ text, version }`: the TypeScript this workflow was imported from (an example or `POST /workflow/source`) and the version the import saved. Edits keep it unchanged; opening a kernel document with `POST /workflow` drops it. |

A site is `{ "unit": { "function": "onboarding" }, "path": [3, 0] }`.

## The view

`view` is lash's wire shape, not this example's: its schema is
`schemas/host/kernel-workflow-view`, and every host that serves
`lash::workflow::view::derive(&document)` serves the same JSON. The checked
document holds the declarations it was linked against, which supply the
parameter names that document `use` lines do not carry; a remote reader can
decode `FunctionRegistry::export_declarations()` as `FunctionDeclarations`
and pass it to `WorkflowDocument::derive`.

| Property | Meaning |
| --- | --- |
| `statements` | The entry's statements in document order: `{ site, statement, block, depth, action? }`. `statement` is the kernel statement; `action` is the site of its action, which is where a run reports. |
| `scopes` | Every block of the entry, including empty bodies: `{ block, depth, kind }`, where `depth` counts the statements enclosing the block (the entry body is `0`) and `kind` is `entry`, `closure`, `then`, `else`, `for`, `while`, `try`, `catch` or `finally`. |
| `dataflow` | Bindings, expression sites, actions with their arguments, lexical availability, the entry's parameters, returns with the callable they end, and assignments. Inferred types are the checker's conservative facets; `any` is not proof of compatibility. |
| `execution_sites` | The sites a run can report at: `{ site, statement, kind, loops }`. |

Everything a canvas words or draws is this example's: statement summaries,
scope names, port labels (an argument with no effect parameter `name` is
`argumentN`), layout and colours live in `frontend/`.

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

`POST /workflow/plan` accepts `{ version, operations: Operation[] }` using
Lash's `lash::workflow::authoring::Operation` wire contract:
`{ "edit": <Edit> }` or `{ "insert_effect": { at, effect, args, result, binding } }`.
Unknown operation tags and structural fields are refused during decoding.
Lash publishes its JSON Schema and TypeScript declarations under
`schemas/host/kernel-authoring-operation/`. The example owns the HTTP transport,
revision and authentication envelope; Lash owns the operation data.
The adapter calls `plan` before admitting the transaction against a cloned
draft. It returns the canonical transaction and preview without changing the
saved version. Send the returned edits to `/workflow/edits` to save against that
same version. Save the expanded `Transaction` for exact edit replay against its
base and admission context. Each operation plans to one edit, and a computed
effect input stays an argument of a nested `perform`. Planning refusals are
HTTP 422 `plan_refused`, with diagnostic indices into the planned edits.

## Running

`POST /run` answers an SSE stream. Each `run_event` is:

| Property | Meaning |
| --- | --- |
| `runId` | The process id. |
| `workflowVersion`, `definition` | The saved version the run started from and the identity of the document it executes. |
| `sequence` | The event's position in this stream. |
| `site` | The site the event is about, a site of that document. Absent for the run as a whole. |
| `status` | `started`, `waiting`, `succeeded`, `failed` or `incomplete`. |
| `displayDelta`, `display` | What this event changed in the host's display, and the display after it. |
| `error` | A failure or explanation of missing observation evidence. |
| `reset` | Discard provisional node, call and approval state before applying replacement evidence. |
| `callOnly` | Call inspection or coverage update; does not replace a node’s or the run’s status. |
| `callId`, `call` | Admitted call identity and retained input/output data when available. |
| `output` | The workflow’s terminal result when available. |
| `approvalKey` | The key to resolve when the run is parked on `host.approval`. |
| `coverage` | Lash's `view::coverage` of the run's document from its process overlay's counts: `observation`, each statement's `runs` and each scope's `entries`. Sent in a `callOnly` event when it changes, and on the last event. |
| `path` | On the last event: `{"state": "verified", "steps": [...]}`, each verified segment's `view::path` steps in segment order, or `{"state": "unavailable", "reason": ...}`. |

Runs start observed at `counts`. The last event has no `site` and carries the
run's terminal status, its final coverage and its path. A
`run_error` event carries an observation/start error and ends the stream; it does
not prove the underlying invocation failed. Multiple approval keys can be pending
at once and must each remain resolvable.

The editor draft and revision are in memory. Publishing retains a Lash workflow
artifact for execution; this example does not restore an editable draft or a run
history after a host restart. These HTTP/SSE shapes are example-owned, not Lash
facade contracts; only `view` is Lash's.
