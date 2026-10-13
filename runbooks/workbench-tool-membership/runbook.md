# Workbench tool selection

Read [RULES.md](../RULES.md) first. This economy browser referee uses the real
`deepseek/deepseek-v4-flash` provider. It proves that an operator can leave a
tool out of the session's selection and offer it again through the browser,
and that the model loses and regains that capability on subsequent turns.
Mock inbox messages are the only tool effects.

Which tools a session is offered is its providers' own config: the workbench's
mail tools are offered under the workbench owner's `ToolSelection`, which the
browser changes with the owner's `set_tools` config command
(`POST /api/tools/selection`). There is no session-wide tool membership.

The companion host law is
`tests::tool_catalog_route_removes_and_restores_the_models_tool` on
`//examples/agent-workbench:agent-workbench__unit_test`, over SQLite memory.
It cross-checks the HTTP selection writes, the listed tools a selection
leaves out, and the next provider request's current catalogue instructions.

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
- API: `/api/tools?session_id=<S>` (each tool with `offered` and
  `selectable`), `/api/state?session_id=<S>`, and
  `/api/accounts/test/inbox`.
- Disk: read `lash-sessions.db` without writing it. Follow `session_head`
  into `session_revisions.config_ref` and read that `blobs` row: the
  session's recorded config. The workbench owner's namespace records the
  selection as `tools: {"all_except": [...]}`; the tools it names must be
  exactly the selectable tools the API lists with `offered: false`. No
  recorded selection means every mail tool is offered. Save the extracted
  namespace with its head revision, and save each phase's new `trace.jsonl`
  slice.

## Initial capability

Connect a mock account named `test` using the inbox UI. Send a browser turn:
“Send a mock inbox message using inbox.test.send, title membership-before,
text baseline, then reply with the returned message id.” Wait for the turn to
settle and require that exact title in the inbox API. Open **tools** next to
the model picker. The checkbox labelled `inbox__test__send` is checked.
Save `01-present.png`, the API snapshot, disk entry, and new turn trace.
Require a successful completed `inbox__test__send` tool call in the new trace.

## Leave it out

Uncheck that tool in the browser. Wait for the pending state to clear; do not
interpret an accepted command as an applied change. The checkbox must stay
listed, unchecked and available to offer again. Save `02-removed.png` and
cross-check the API (`offered: false`) and the disk selection, which names it
in `all_except`.

Send another browser turn: “If inbox.test.send is available, send title
membership-disabled, text unavailable probe, and reply with its id. If it is
unavailable, report that and finish; do not substitute another tool.” Require
the settled inbox to contain no `membership-disabled` message. Require no `inbox__test__send` tool call in that turn's trace. The host
law separately checks the current provider instructions, since the production
trace omits large outgoing request bodies; historical messages and the user's prompt can still name the removed tool. Save
`03-unavailable.png`, the state/inbox snapshots and this turn's trace.
Do not gate on the model's exact wording.

## Offer it again

Check the same listed checkbox. Wait for settlement and require the API to
list it `offered: true` and the disk selection to no longer name it. Send a browser turn asking it to send title
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
| Leaving it out persists and stays selectable | | `02-removed.png`, API, disk selection |
| Disabled model cannot send and produces no message | | `03-unavailable.png`, inbox, no send-tool trace |
| Offering it again advertises and executes the tool | | `04-restored.png`, inbox, API, disk, trace |
| Reload retains the selection; mobile control fits | | `05-reloaded.png`, `06-mobile.png` |
| Operator can understand applied, pending and error states | | visible checkbox labels and status |
