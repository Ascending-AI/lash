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
**Canvas scope** selects canonical blocks, and **Advanced** contains source,
the kernel document and the helper/direct-edit transaction queue. It is one
editor with one saved document. See [the HTTP contract](../../examples/workflow-graph-roundtrip/CONTRACT.md).

Save screenshots at each phase and the corresponding HTTP requests, responses
and SSE events. Treat UI success messages alone as insufficient evidence.

## 1. Open and inspect

Open **Examples**, select **Onboarding**, and capture `/workflow/select`.
Require that its response and a fresh `/workflow` agree on `version`,
`identity`, `definition` and `entry`, with no `notAdmitted` error. The canvas
must show the selected entry, with branches and explicit scope navigation.

Open **Advanced**. The TypeScript pane must contain
`Welcome to the workflow graph`. Import that unchanged export and verify that
`identity` and the canonical `document` are unchanged. The host revision may
advance. Save `01-opened.json` and `01-opened.png`.

Find the text expression containing that greeting in `workflow.view.dataflow.expressions`.
Record its full `site` and the current revision. Do not use a site from an earlier
revision after an edit.

## 2. Refuse an invalid edit without saving

Expand **Direct typed edit** and enter:

```json
[{"replace_expression":{"expression":<greeting-site>,"with":{"variable":"nowhere"}}}]
```

Press **Queue direct edits**, then **Preview transaction**. Require HTTP 422
`plan_refused` with a diagnostic, a visible error, and unchanged saved document,
identity and revision. **Apply changes** must remain disabled. Clear the queue.
Save `02-refused.json` and `02-refused.png`.

## 3. Preview and apply a valid edit

Queue the same edit with `with` set to
`{"literal":{"text":"Edited as a kernel document"}}`. Preview it.
Require a successful plan with the expected ordinary typed edit and a preview
containing the new greeting, while GET `/workflow` remains unchanged.

Press **Apply changes**. Require revision +1, changed document identity,
and correspondence mapping the edited expression. The TypeScript and Kernel
document panes must now show the new greeting. Save `03-edited.json` and
`03-edited.png`.

The `lash-kernel-edit` authoring law separately checks a helper/direct/helper
batch, fresh names across the complete batch, base-document anchors, and atomic
refusal. The visual **Add step** flow must use the public
`lash::workflow::authoring::plan` API and the same admission path.

## 4. Run and approve

Close Advanced and press **Run workflow**, recording SSE. Require the execution
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
constant promotion cannot hoist a reevaluated condition; generated names avoid
all binding kinds; negative zero survives controls; pending approvals survive
concurrent waits and late responses. Backend admission owns type compatibility.
Use the example's focused Rust laws for canonical facets, atomic edits, named
inputs, trace gaps and repeated call correlation.

The draft is intentionally in memory and this example has no historical-run
browser. Do not claim restart restoration, full durable trace capture, or generic
tool-live progress based on this scenario.
