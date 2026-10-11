# Workbench tool catalogue membership

Read [RULES.md](../RULES.md) first. This economy browser referee uses the real
`deepseek/deepseek-v4-flash` provider. It proves that an operator can remove and
restore a tool through the browser, and that the model loses and regains that
capability on subsequent turns. Mock inbox messages are the only tool effects.

The companion host law is
`tests::tool_catalog_route_removes_and_restores_the_models_tool` on
`//examples/agent-workbench:agent-workbench__unit_test`, over SQLite memory.
It cross-checks HTTP membership writes, retained removed entries, active
manifests, and the next provider request's current catalogue instructions.

## Boot and evidence

Choose a free port and fresh data/run directories under the run's artifact
root. Source `env.sh` and the keys from the repo `.env`, export
`OPENROUTER_MODEL=deepseek/deepseek-v4-flash`, `AGENT_WORKBENCH_OPEN=0`,
`AGENT_WORKBENCH_DATA_DIR` and `AGENT_WORKBENCH_RUN_DIR`, then launch
`just agent-workbench <port>`. Use SQLite file storage and gate readiness on
`GET /healthz`. No development provider scenario is selected. Record the
served model from the turn trace, rather than trusting the environment.

Use these three authorities at every checkpoint:

- Page: the tools panel's labelled checkboxes, status, and rendered replies.
- API: `/api/tools?session_id=<S>` (retained entries and pending changes),
  `/api/state?session_id=<S>`, and `/api/accounts/test/inbox`.
- Disk: read `lash-sessions.db` without writing it. Follow `session_head`
  into `session_revisions.checkpoint_ref` and read that `blobs` row, whose
  content is one checkpoint-graph node's encoded envelope. The node's
  component set maps `tool_state` to the component blob ref; decode the
  referenced tool-state blob. Its entry and `member` bit must agree with the
  API. An omitted `member` means true. Save the extracted entry with its head
  revision, and save each phase's new `trace.jsonl` slice.

## Initial capability

Connect a mock account named `test` using the inbox UI. Send a browser turn:
“Send a mock inbox message using inbox.test.send, title membership-before,
text baseline, then reply with the returned message id.” Wait for the turn to
settle and require that exact title in the inbox API. Open **tools** next to
the model picker. The checkbox labelled `inbox__test__send` is checked.
Save `01-present.png`, the API snapshot, disk entry, and new turn trace.
Require a successful completed `inbox__test__send` tool call in the new trace.

## Remove membership

Uncheck that tool in the browser. Wait for the pending state to clear; do not
interpret an accepted command as an applied change. The checkbox must stay
listed, unchecked and available for restoration. Save `02-removed.png` and
cross-check API and disk membership false.

Send another browser turn: “If inbox.test.send is available, send title
membership-disabled, text unavailable probe, and reply with its id. If it is
unavailable, report that and finish; do not substitute another tool.” Require
the settled inbox to contain no `membership-disabled` message. Require no `inbox__test__send` tool call in that turn's trace. The host
law separately checks the current provider instructions, since the production
trace omits large outgoing request bodies; historical messages and the user's prompt can still name the removed tool. Save
`03-unavailable.png`, the state/inbox snapshots and this turn's trace.
Do not gate on the model's exact wording.

## Restore membership

Check the same retained checkbox. Wait for settlement and require API and
disk membership true. Send a browser turn asking it to send title
membership-after, text restored through `inbox.test.send` and reply with its
id. Require that message in the inbox and a successful completed
`inbox__test__send` call in the new trace. Save `04-restored.png` and the corresponding API,
disk and trace evidence. Reload the page, reopen tools, and require the
checkbox still checked. Save `05-reloaded.png`. At a narrow mobile viewport,
require the panel and toggle to remain visible without horizontal overflow;
save `06-mobile.png`.

## Teardown and scorecard

Always stop the stack with `just agent-workbench-down <port>` using the same
run/data environment. Abort on a timeout, host panic, browser error or any
page/API/disk disagreement; save the failing checkpoint and report its stage.

| Gate | Result | Evidence |
| --- | --- | --- |
| Initial tool is callable and advertised | | `01-present.png`, inbox, successful tool trace |
| Browser removal persists and stays restorable | | `02-removed.png`, API, disk entry |
| Disabled model cannot send and produces no message | | `03-unavailable.png`, inbox, no send-tool trace |
| Restoration advertises and executes the tool again | | `04-restored.png`, inbox, API, disk, trace |
| Reload retains membership; mobile control fits | | `05-reloaded.png`, `06-mobile.png` |
| Operator can understand applied, pending and error states | | visible checkbox labels and status |
