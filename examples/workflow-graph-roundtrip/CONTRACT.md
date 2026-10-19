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
| `POST` | `/workflow/captions` | `{ version, entries, reviewed? }`: save a new caption list for the saved document as the next version; its `WorkflowView` |
| `POST` | `/workflow/plan` | `{ version, operations }`: preview Lash authoring operations without publishing; `{ version, transaction, preview, correspondence }` |
| `POST` | `/run` | `{ version?, inputs? }`: start a run with named input values; its `text/event-stream` |
| `POST` | `/approvals/{key}` | `{ approved }`: resolve a parked approval |
| `GET` | `/healthz` | `{ "service": "workflow-graph-roundtrip", "status": "ok" }` |
| `GET` | `/` and `/{path}` | Files from `frontend/` |

The catalog is `examples/workflows/catalog.json`, with embedded source and
cell witnesses from the same corpus. `group` is `module`, `cell` or `refusal`.
`entries` lists the declared workflow entries. `statements`/`scopes` and `python`
count the first entry's derived view in that dialect; non-modules have no view.
`refusal` names an expected module-contract diagnostic. `typescriptSource`,
`pythonSource`, `typescriptWitness` and `pythonWitness` support inspection of
cell/refusal cases without changing the saved workflow. A source is null when
that dialect is not declared by the manifest; the picker disables that choice.
Selecting an undeclared dialect returns 422 without changing the saved workflow.

`POST /workflow/select` takes `{ id, dialect?, entry? }`; the default is
TypeScript and the case's first entry. Cell/refusal selections return 422.
`POST /workflow/source` takes `{ version, source, entry, dialect?, format? }`.
Without `format`, it imports a workflow module in the selected language.
With `type_script_v1` or `python_v1`, it reads generated source against the
current document. An unchanged printed read preserves canonical bytes.

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
| `source` | The exact export in `sourceDialect` (`typescript` or `python`). Editing this text has no effect until `POST /workflow/source` admits it. Absent with a `sourceUnavailable` reason when the document has no spelling in that dialect. |
| `sourceDialect` | The dialect used to print the saved document. |
| `functionLabels` | Shipped helper titles keyed by name, joined against view callees. Unknown names fall back to themselves. |
| `sourceMap` | Lash's `SourceMap` for `source` (snake_case, lash's shape): UTF-8 byte ranges of `source` for each site of the document. |
| `authored` | `{ dialect, text, version, sourceMapId?, sourceMap?, mapUnavailable? }`: the imported source, read back through the publication's held map id. Authoring edits retain historical text but drop the map and id. A printed read or opening a kernel document drops authored source. |
| `captions` | The version's captions, lash's `kernel-captions` shape (snake_case): `{ document, entries: [{ site, caption: { title, description? } }] }`, keyed to `identity`. Import and printed reads replace them with the source's; an edit rekeys them; a kernel document opens with none. |
| `captionView` | `captions` joined with `view` by lash's `WorkflowView::captions`, the `kernel-caption-view` shape: `{ document, nodes: [{ node, site, caption }], arms: [{ node, arm, block, caption }], callables: [{ callable, block, caption }], params: [{ index, name, caption }], fields: [{ path: { root, fields }, caption }] }`, indexing `view.nodes`, a node's `arms` and `view.callables`; `params` and `fields` caption what the entry takes and gives back. Absent with `view`, or with `captionsUnavailable`. |
| `captionsToReview` | Captioned sites an edit has written at or beneath since their caption was saved (`Rekeyed.touched`, carried through later edits). |
| `captionsDropped` | The targets (`{ site }`, `{ param }` or `{ field }`) of captions `source` could not spell; absent when it spells all of them. |
| `captionGuide` | How to write a caption in `sourceDialect`, in that dialect's words. |

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

`POST /workflow/edits` accepts `{ version, edits: Operation[] }`. Each operation
carries `base`, the saved document identity, and exact base Sites. The closed
operation families are `set_value`, `insert_step`, `remove_step`, `move_step`,
`wrap_steps`, `unwrap_steps`, and `change_condition`.

For example, a literal slot can be changed with:

```json
{"set_value":{"base":"<saved identity>","target":{"literal":{"expression":{"unit":{"function":"onboarding"},"path":[3,0,0]}}},"value":{"literal":{"text":"Hello"}}}}
```

The batch applies whole or not at all. Success saves the new version and returns
`{ workflow, correspondence }`, whose entries are `{ from, to, edited }`.
Refusals leave the saved version unchanged and answer HTTP 422 `edit_refused`
with `details.diagnostics: [{ edit, site, location, message, cause }]`. `cause` is Lash's
serialized `EditDiagnosticKind`, retaining nested checker fields. Unsupported
or malformed operations answer `unsupported_canvas_edit` and its typed cause.

`POST /workflow/captions` accepts `{ version, entries: CaptionEntry[], reviewed?: Site[] }`:
every caption the saved document is to have. The host builds a lash `Captions`
value from them, saves it as the next version of the same document and
republishes it; the sites in `reviewed` leave `captionsToReview`. A caption
text lash refuses (`CaptionRefusal`) or a site it may not caption
(`CaptionsRefusal`) answers HTTP 422 `caption_refused` and changes nothing.
An edit transaction's response and a plan's preview carry the rekeyed
captions.

`POST /workflow/plan` accepts `{ version, operations: Operation[] }` and previews
the same closed batch against a cloned draft, returning
`{ version, transaction, preview, correspondence }`. It cannot expand raw edits.
Send its transaction's `edits` to `/workflow/edits` against the same version to
save. Preview refusals are HTTP 422 `plan_refused`, with typed diagnostics.

Lash publishes the operation schema and TypeScript declarations under
`schemas/host/kernel-authoring-operation/`. The example owns its HTTP and
revision envelope. Source replacement and re-import handle other changes.

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
