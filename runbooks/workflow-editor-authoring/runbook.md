# Workflow editor: open, edit, republish and run

Read [the shared rules](../RULES.md) first. This scenario uses the existing
`examples/workflow-graph-roundtrip` host and deterministic mock effects. It
opens no RLM session and calls no external model, email or research service.

## Start and evidence

From an isolated Kiln fork, run `just workflow-graph-roundtrip`. The script
uses a gate-owned port and a fresh SQLite database; open its printed URL at `/`.
Poll `/healthz` for `{ "service": "workflow-graph-roundtrip", "status": "ok" }`.
Use a fresh artifact directory. Stop only this test's server when finished.

The default page is the visual editor. **Examples** selects workflows,
**Canvas scope** selects canonical blocks, and **Edit source** contains source,
and whole-source replacement. It is one
editor with one saved document. See [the HTTP contract](../../examples/workflow-graph-roundtrip/CONTRACT.md).

Save screenshots at each phase and the corresponding HTTP requests, responses
and SSE events. Treat UI success messages alone as insufficient evidence.

## 1. Open and inspect

Open **Examples**, select **Onboarding**, and capture `/workflow/select`.
Require that its response and a fresh `/workflow` agree on `version`,
`identity`, `definition` and `entry`, with no `notAdmitted` error. The canvas
must show the selected entry, with branches and explicit scope navigation.

Open **Edit source**. The TypeScript pane must contain
`Welcome to the workflow graph`. Import that unchanged export and verify that
`identity` and the canonical `document` are unchanged. The host revision may
advance. Save `01-opened.json` and `01-opened.png`.

Find the text expression containing that greeting in `workflow.view.dataflow.expressions`.
Record its full `site` and the current revision. Do not use a site from an earlier
revision after an edit.

## 2. Refuse an invalid edit without saving

Send `/workflow/plan` a `set_value` operation against the saved document identity,
with the greeting's literal Site and value `{"variable":"nowhere"}`. Require
HTTP 422 `plan_refused`, a typed unbound-variable cause, and unchanged saved
identity and revision. Repeat a raw `replace_statement` request against both
`/workflow/edits` and `/workflow/plan`; require `unsupported_canvas_edit` and
unchanged identity. There is no raw-edit UI.

## 3. Apply a supported edit

Double-click the greeting's input, set its constant to `Edited as a kernel
document`, and save. Inspect the closed `set_value` operation and correspondence.
Require revision +1 and a changed identity. Add a mock message with **Add step**;
its preview uses `insert_step` and its data arguments remain inside the step.
Save the requests/responses and screenshots.

Use **Edit source** for renames, code/declaration changes and terminal structure.
Import validates the whole replacement before saving it.

## 4. Run and approve

Close Edit source and press **Run workflow**, recording SSE. Require the execution
panel to show the parked approval and a usable approval control. Approve it and
poll until the invocation settles. Every site-bearing event must reference the
run's document; call-only events must not replace aggregate node status.

Require terminal `succeeded`, the edited greeting in the display, the expected
Onboarding list/progress updates, and input/output data for retained calls.
Missing observations must remain unavailable or incomplete. Save
`04-waiting.png`, `04-settled.png` and `04-run.jsonl`.

## 5. Research and KPI data mappings

Select **Deep research brief**, edit the workflow Input node's topic to
`Reducing support escalation`, save inputs and run. Require a successful output
whose title/topic reflects that input, not the template default, and three
inspectable mock calls. Repeat with **Weekly business KPIs**, a distinct workspace
name, and four calls. Select a result port and inspect the Return step's mapping.
Save `05-research.png`, `05-kpis.png` and each run's events.

## Regression companions

Run the example's Node tests using `node --test examples/workflow-graph-roundtrip/frontend/*.test.mjs`.
They must exercise the active canvas and editing/approval implementations:
closed operations reject arbitrary code; generated result names avoid
all binding kinds; negative zero survives controls; pending approvals survive
concurrent waits and late responses. Backend admission owns type compatibility.
Use the example's focused Rust laws for canonical facets, atomic edits, named
inputs, trace gaps and repeated call correlation.

The draft is intentionally in memory and this example has no historical-run
browser. Do not claim restart restoration, full durable trace capture, or generic
tool-live progress based on this scenario.
