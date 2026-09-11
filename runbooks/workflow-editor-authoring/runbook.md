# E2E Scenario: Workflow Editor — Blank-to-Terminal Authoring

> **Read [../RULES.md](../RULES.md) first** — especially the browser-surface,
> screenshot, polling, objective-gate, Abort/RCA, and teardown rules. This runbook adds
> only the workflow-editor authoring scenario.

**Purpose.** Follow the workflow-graph example as a non-technical author: start with its
Blank workflow, add and configure actions in both the Steps and Canvas views, reorder and
move an action between scopes, recover from a typed malformed-expression rejection, save
without losing canvas context, then run the saved version to a terminal state. This
proves that the friendly editor, the graph/code lens, and execution all describe the same
workflow.

**No real tokens.** `examples/workflow-graph-roundtrip` uses deterministic host-owned
mock operations. Do not configure OpenRouter or a Restate stack for this run.

## Scenario-specific golden rules

1. **The draft is not the saved workflow.** Before Save, `GET /workflow` must still
   return the Blank baseline even while the browser shows authored cards. Play only after
   a successful Save, because `/run` executes the saved version.
2. **Both directions of the lens must agree.** `POST /project` is source → graph.
   `POST /workflow` is graph → canonical source plus save (the example's render seam).
   At each save gate, the response document, a fresh `GET /workflow`, canonical source
   pane, Steps order, and Canvas nesting must agree.
3. **Malformed means a typed 422.** Deliberately save one malformed result expression.
   Require HTTP 422 with `error.code == "invalid_expression"`, the owning `nodeId` and
   `field == "expression"`. The UI must retain the draft and show the typed error. Fix the
   same field; never reload or reselect to escape the error.
4. **Save reconciles identity; it does not reset context.** Record the selected node and
   one deliberately dragged position before Save. The successful response must map every
   posted node through `idMap`; selection and the stored position must move to the new id,
   and the viewport must not refit when the graph shape is unchanged.
5. **Order and scope are execution semantics.** The final canonical process body must
   put Set progress before Show message, with both inside `blank` and before Finish. The
   SSE display result must therefore contain progress 73 and `Built from blank`.
6. **Terminal means UI and SSE terminal.** A green-looking display alone is insufficient.
   The final `/run` event must be `succeeded`, target the saved terminal node, and carry
   the saved workflow version; the Canvas terminal and Display panel must visibly settle.

## Working material

- Choose an unused `<port>` and an empty `<artifacts>` directory. Boot from the repository
  root with `just workflow-graph-roundtrip <port>`. The recipe builds the production
  frontend, derives an isolated Cargo target directory from the port, and starts the Rust
  server on `127.0.0.1:<port>`. Gate the printed `workflow-graph-roundtrip listening`
  line, then poll `GET /healthz` until it returns 200 with
  `{ "service": "workflow-graph-roundtrip", "status": "ok" }`.
- Teardown is Ctrl-C/SIGTERM to that foreground recipe. Confirm the port is closed.
- Browser affordances: workflow selector; **Steps** / **Canvas** tabs; rail **+** menus;
  Canvas **+ Add node · main** palette; editable value chips; `</>` raw-expression
  toggle; node `⤴` scope menu; `▲` / `▼` order controls; **Save**; **Play**; canonical
  source; Display panel and node status badges. Discover stable selectors yourself.
- Backend truth: `GET /workflows`, `GET /operations`, `POST /project`, `GET /workflow`,
  `POST /workflow/select`, `POST /workflow`, and the `POST /run` SSE stream.
- Browser sidecar truth: localStorage key `lash.wfgraph.positions.v1`. Positions never
  appear in the workflow document by design.

Save every named screenshot plus the relevant HTTP request/response bodies in
`<artifacts>`. Capture exact node ids and versions rather than shortening them in JSON
evidence.

## Phase 0 — Boot and contract pre-flight

After readiness, gate these API facts before opening the editor:

- `GET /workflows` includes `{ id: "blank", name: "Blank workflow" }`;
- `GET /operations` identifies Show message's `text` as `string`, Set progress's `pct`
  as `number`, and Finish's `expression` as `expression`;
- projecting `process probe() { finish 0 }` through `POST /project` returns 200, canonical
  source, one process, and one terminal without changing `GET /workflow`'s version.

Open the browser, gate the workflow selector, Steps view, Save/Play controls, canonical
source pane, and Display panel. Screenshot `00-ready.png`.

## Phase 1 — Establish the Blank baseline

Select **Blank workflow** through the browser and capture `POST /workflow/select`. Poll
until all three surfaces agree:

- the selector renders **Blank workflow** and Steps shows only the Finish card;
- the select response and fresh `GET /workflow` have identical version/source/nodes,
  with canonical source `process blank() { finish 0 }` (whitespace-insensitive);
- the source pane renders that version and source, while Display says no run yet.

Record the baseline version and ids in `01-blank.json`. Screenshot `01-blank.png`.

## Phase 2 — Author a value-typed action in Steps

In Steps, open **Add a step at the start**, choose **Show message**, open its settings,
and set `text` to the literal `Built from blank`. Leave the card before Finish. Gate:

- the Steps card visibly renders Show message and the configured literal;
- the browser draft has a new temporary node id and the Save control is enabled;
- `GET /workflow` still equals `01-blank.json` — this is an unsaved client draft, not an
  implicit backend mutation.

Screenshot `02-steps-authored.png` and save the unchanged GET response as
`02-saved-still-blank.json`.

## Phase 3 — Author on Canvas, move scope, and reorder

Switch to Canvas. Through **+ Add node · main**, add **Set progress** and edit its numeric
`pct` value to `73`. This proves Canvas authoring independently of the Steps rail.

The new action begins in top-level `main`. Use its `⤴` menu to move it into the `blank`
process body, then use `▲` once to place it before Show message. Gate in the rendered
Canvas that Set progress is nested inside `blank`, above Show message, with Finish last.
Switch briefly to Steps and require the same order and values, then return to Canvas.

`GET /workflow` must still be the Blank baseline. Screenshot
`03-canvas-scope-order.png`; save a DOM-derived ordered id/title list as
`03-draft-order.json`.

## Phase 4 — Prove typed malformed-expression recovery

On Finish, use the raw-expression toggle and replace `0` with malformed `1 +`. Commit the
field and press Save while capturing the request and response. Require:

- `POST /workflow` returns 422 and the JSON error satisfies golden rule 3;
- the UI renders `invalid edit · invalid_expression`, names the message and node, and
  keeps Set progress, Show message, their values, order, and scope;
- a fresh `GET /workflow` still equals the Blank baseline version and source.

Screenshot `04-typed-422-draft-kept.png`; save the exchange as
`04-malformed-save.json`. Replace the same Finish expression with the valid string
literal `"done"`. Do not Save yet.

## Phase 5 — Save and reconcile ids, position, and selection

Select Show message and drag it horizontally far enough to create a distinct stored
position without changing its vertical order. Record its old id, selected state,
bounding box, viewport transform, and localStorage position. Capture the outgoing Save
document, press Save, and require a 200 `SaveWorkflowResponse`.

Gate all of the following before judging the UI:

- `idMap` contains every posted node id, including both new action ids; each mapped id
  exists in the returned document;
- the returned source has Set progress before Show message before `finish "done"`, all in
  `process blank`, and its version is exactly baseline + 1 (the rejected save did not
  create a version);
- fresh `GET /workflow` equals the returned document, and re-projecting the returned
  source through `POST /project` has the same canonical source and graph shape;
- the source pane renders the new version/source and the UI reports `saved as v<N>`;
- selection moved to Show message's reconciled id; localStorage moved the exact position
  to that id and removed the old key; its post-save bounding box and viewport transform
  have not jumped materially;
- Steps and Canvas still agree on values, order, and nesting after the id remint.

Save the response/GET/project/sidecar evidence as `05-save-reconciliation.json` and
screenshot `05-saved-context-preserved.png`.

## Phase 6 — Run the saved workflow to terminal

Stay on Canvas and press Play while capturing the complete `POST /run` SSE stream. Poll,
never sleep, until Play is no longer running and the Display done indicator appears.
Require:

- every SSE event has the saved workflow version and one run id; the last event is
  `succeeded` for the canonical Finish node, with no failed event;
- the final SSE display has `progress: 73` and includes `Built from blank`;
- the rendered Display shows 73%, the message, the same saved version/run identity, and
  a terminal done state;
- Canvas marks the progress, message, and terminal nodes succeeded, with the terminal
  node correlated to the final SSE `nodeId`;
- `GET /workflow` after execution still equals the saved document (running does not edit
  the workflow).

Save the stream and final GET as `06-terminal-run.json`. Screenshot
`06-terminal-run.png` with the succeeded Canvas nodes and matching Display evidence in
view.

## Phase 7 — Teardown and score

Stop the foreground recipe and confirm `GET /healthz` can no longer connect.

| Item | Objective gate | Verdict | Evidence |
|------|----------------|---------|----------|
| Blank baseline | selector, Steps, source pane, select response, and GET agree | | `01-blank.png`, `01-blank.json` |
| Steps authoring | Show message literal visible; saved GET unchanged pre-Save | | `02-steps-authored.png`, `02-saved-still-blank.json` |
| Canvas authoring | numeric Set progress added on Canvas, moved into process, reordered | | `03-canvas-scope-order.png`, `03-draft-order.json` |
| Typed recovery | malformed expression returns typed 422; complete draft remains | | `04-typed-422-draft-kept.png`, `04-malformed-save.json` |
| Save reconciliation | render/GET/project agree; ids, selection, position, viewport survive | | `05-saved-context-preserved.png`, `05-save-reconciliation.json` |
| Terminal execution | final SSE and Canvas terminal succeed; Display shows 73 + message | | `06-terminal-run.png`, `06-terminal-run.json` |

**Aggregate:** did a non-technical author build one workflow from Blank across both editor
views, recover from a typed rejection, preserve editing context through canonical id
reconciliation, and watch the exact saved version reach terminal success?

---

_Stop triggers and the Abort/RCA + reporting protocol are in [../RULES.md](../RULES.md)._
