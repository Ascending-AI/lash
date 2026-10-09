# E2E Scenario: Workflow Editor — Open, Edit, Republish, Run

> **Read [../RULES.md](../RULES.md) first** — especially the browser-surface,
> screenshot, polling, objective-gate, Abort/RCA, and teardown rules. This runbook adds
> only the workflow-editor scenario.

**Purpose.** Follow the workflow round-trip example as an author working on a kernel
document: open a catalog example, send one edit transaction, see it saved and published
as a new version, run that version, and watch the run settle on the document's sites.
This proves that the document the page shows, the definition lash admitted and the run
the overlay reports are the same workflow.

**No real tokens.** `examples/workflow-graph-roundtrip` uses deterministic host-owned
mock operations. Do not configure OpenRouter for this run.

**There is no source language in this scenario.** The host opens no RLM session and
prompts no model. The workflow is a kernel document; the TypeScript tab is what the
kernel printer spells for it and is read-only. Edits are kernel edits addressed by site.

## Scenario-specific golden rules

1. **A refused transaction changes nothing.** After the refusal in Phase 2, `GET
   /workflow` must return the version and `identity` it returned before it.
2. **An edit is a new version and a new definition.** The applied transaction answers
   `version + 1`, a `definition` and an `identity` that both differ from the opened
   version's, and a correspondence whose only `edited` entry ends at the replaced
   expression.
3. **A run reports sites of its own document.** Every `run_event` carries the saved
   `workflowVersion`, a `definition` equal to the view's `identity`, and either no `site`
   or one listed in the view's `executionSites`.
4. **Terminal means UI and SSE terminal.** The last `run_event` has no `site` and
   `status == "succeeded"`; the page's run status reads `succeeded`, every statement that
   ran shows a completed dot, and the display shows the edited message.

## Working material

- Choose an unused `<port>` and an empty `<artifacts>` directory. Boot from the repository
  root with `just workflow-graph-roundtrip <port>`. Gate the printed
  `workflow-graph-roundtrip listening` line, then poll `GET /healthz` until it returns 200
  with `{ "service": "workflow-graph-roundtrip", "status": "ok" }`.
- Teardown is Ctrl-C/SIGTERM to that foreground recipe. Confirm the port is closed.
- Page affordances: the workflow selector; the **Statements** list (click a statement to
  copy its site); the **Kernel text** / **TypeScript** / **Document** tabs; the **Edit
  transaction** box with **Apply and publish**; **Run**, **Approve**, the run status, the
  progress bar and the display list. The page carries no `data-testid`; the controls have
  ids (`catalog`, `statements`, `edits`, `apply`, `run`, `approve`, `run-status`,
  `display`).
- Backend truth: `GET /workflows`, `GET /environment`, `GET /workflow`,
  `POST /workflow/select`, `POST /workflow/edits`, and the `POST /run` SSE stream
  ([CONTRACT.md](../../examples/workflow-graph-roundtrip/CONTRACT.md)).

Save every named screenshot plus the relevant HTTP request/response bodies in
`<artifacts>`. Capture exact sites, versions and identities rather than shortening them.

## Phase 0 — Boot and contract pre-flight

After readiness, gate before opening the page:

- `GET /workflows` includes `{ id: "onboarding", name: "Onboarding" }`;
- `GET /environment` lists the effect `display.show_message` and the function `num.lt`.

Open the page; gate the selector, the statement list, the three tabs and the Run button.
Screenshot `00-ready.png`.

## Phase 1 — Open the catalog example

Select **Onboarding** and capture `POST /workflow/select`. Gate:

- the response and a fresh `GET /workflow` agree on `version`, `identity` and
  `definition`, and `notAdmitted` is absent;
- the statement list shows the entry's statements, nested under `if` and `while`, and the
  header reads the same version and definition;
- the **TypeScript** tab shows printer output containing `Welcome to the workflow graph`.

Record the view as `01-opened.json`. Find the statement `let shown_2 = a value`: its
value is the record `{text: "Welcome to the workflow graph"}`, at the statement's site
with `0` appended; the text literal is that site with another `0` appended. Record the
literal's site. Screenshot `01-opened.png`.

## Phase 2 — A refused transaction

Put this in the edit box, with the literal's site, and press **Apply and publish**:

```json
[{ "replace_expression": { "expression": <site>, "with": { "variable": "nowhere" } } }]
```

Require HTTP 422 `edit_refused` with one diagnostic at the literal's site, the message
rendered under the box, and golden rule 1. Save the exchange as `02-refused.json`;
screenshot `02-refused.png`.

## Phase 3 — Edit and republish

Replace the box with:

```json
[{ "replace_expression": { "expression": <site>, "with": { "literal": { "text": "Edited as a kernel document" } } } }]
```

Apply. Gate golden rule 2 on the response, then on the page: the header shows the new
version and definition, the result line reports one edited node, and both the **Kernel
text** and **TypeScript** tabs contain `Edited as a kernel document`. Save the exchange as
`03-edited.json`; screenshot `03-edited.png`.

## Phase 4 — Run and watch the overlay

Press **Run** while capturing the SSE stream. The run parks on `host.approval`: gate that
the run status reads `waiting`, the approval statement shows a waiting dot and **Approve**
appears. Screenshot `04-waiting.png`. Press **Approve**.

Poll until the run status reads `succeeded`. Gate golden rules 3 and 4, and that the
display shows the message `Edited as a kernel document`, the list `steps` as `Approved,
Loop item, Loop item` and a full progress bar. Save the events as `04-run.jsonl`;
screenshot `04-settled.png`.

## Evidence

`00-ready.png`, `01-opened.{png,json}`, `02-refused.{png,json}`, `03-edited.{png,json}`,
`04-waiting.png`, `04-settled.png`, `04-run.jsonl`.
