# A first Python code-mode session

Read [the shared rules](../RULES.md). This Python-only SQLite row is funded
at `frontier`, `openai/gpt-6.1-sol`: first-shot program fluency is the judged
subject. [ADR 0139](../../docs/adr/0139-the-lash-vm-is-a-dialect-free-kernel.md)
binds the dialect and kernel distinction. Tool results are Python dicts
(FIG-5904); cancelling a gather stops its children (FIG-5907). This row uses
the real provider and read-only Parallel Search MCP tools.

## Phase 1 — Companion laws

Source `env.sh`. Run each command under `(ulimit -v 8388608; ...)`.
Run these full selectors once, retain the logs and Kiln
receipts, and require executed=1, passed=1 for each. Zero executed is Abort.

```sh
kiln test //examples/agent-workbench:agent-workbench__unit_test --test_arg=--exact --test_arg=tests::typescript_dialect_tests::a_python_workbench_serves_python_turns_and_records_the_dialect
kiln test //crates/lash-dialect-python:lash-dialect-python__unit_test --test_arg=--exact --test_arg=tests::replies::a_nested_tool_object_is_a_dict_at_every_depth
kiln test //crates/lash-dialect-python:lash-dialect-python__unit_test --test_arg=--exact --test_arg=tests::replies::a_tool_array_is_a_list_of_dicts_however_it_is_awaited
kiln test //crates/lash-dialect-python:lash-dialect-python__unit_test --test_arg=--exact --test_arg=tests::witness::gather_cancel
kiln test //crates/lash-protocol-rlm:lash-protocol-rlm__unit_test --test_arg=--exact --test_arg=executor::tests::python::failures_keep_their_causes_and_name_the_work
```

The laws supply deterministic dialect, dict, cancellation and typed-failure
proof. They do not substitute for the browser's first-shot observations.

## Phase 2 — Boot a fresh Python host

Use an unused port and fresh owned directories. Load the real OpenRouter key
from the ignored repository `.env` without printing it. Do not select a dev
provider. The selector is a host boot choice; session creation has no dialect
field. Save the judged build report and `00-startup.json` from the
`agent_workbench.startup` trace; require `dialect=python`, SQLite and the
served model matching the matrix. Python tool paths join module and operation
with `_`: the workbench finish is `chat_reply`, not `control_finish`.

```sh
. ./env.sh
export AGENT_WORKBENCH_DIALECT=python OPENROUTER_MODEL=openai/gpt-6.1-sol
export AGENT_WORKBENCH_DATA_DIR="$PWD/target/python-session-data"
export AGENT_WORKBENCH_RUN_DIR="$PWD/target/python-session-run"
(ulimit -v 8388608; /usr/bin/time -v just agent-workbench 7549)
```

Keep evidence in `.kiln/<ticket>/run-root/workbench-python-session/python/sqlite/`.
Launch the headless browser **without** the virtual-memory cap and record its
peak RSS with `/usr/bin/time -v`; Chromium reserves more than 8 GiB of address
space. The app and companion tests remain capped at 8 GiB. Navigate to the
page, require readiness and a connected chat, and take `00-ready.png`.

## Phase 3 — Ask once through the composer

Submit this outcome prompt once, without supplying source or repairing the
model's program:

> Look up the current weather for Utrecht, Berlin and Paris concurrently.
> Deliberately delay the Paris lookup by 30 seconds before it contacts the
> weather source; once the other two return, cancel that pending lookup and
> report the two results. List the keys of the first tool result, its key
> count, one value by key and a missing-key default. Also attempt a read-only
> fetch of https://invalid.invalid/ and catch the tool failure, reporting the
> exception. End with a one-line summary. Use only web search/fetch and the
> chat reply; do not send mail, start processes or change host data.

Require a rendered running state; save `01-running.png`. Poll for up to five
minutes until the UI is idle, `active_turns` is empty and the scoped trace
records a completed, finished turn. Save `02-settled.png`,
`02-state.json`, `02-dom.txt` and the complete scoped trace. Expand every
code disclosure and tool result, including failed iterations, before judging.
No second prompt or paid retry belongs to this row. A read-only reload may
complete screenshot collection without admitting another input.

Extract `fluency-hits.json` with cell ids/source, the gather and cancellation
sites, dict operations, caught exception, tool call ids/statuses, terminal
reply and the served model/usage/cost. Match each source site to actual
execution evidence; merely containing the words is insufficient.

## Gates and scorecard

| Gate | Expected | Observed | Verdict/evidence |
| --- | --- | --- | --- |
| Companions | five receipts, executed=5 passed=5 | | |
| Identity | startup Python/SQLite; served matrix model; Python code rows/tags | | |
| First-shot fluency | working program without a dialect refusal/repair loop | | |
| Fan-out | one executed asyncio.gather cell with concurrent child work | | |
| Cancel | delayed child cancelled; no later fetch or completion from that child | | |
| Dict surface | key access, len, iteration and .get execute on a tool result | | |
| Failure | real tool failure caught as a Python exception; typed cause in trace | | |
| Finish | chat_reply; completed/finished terminal | | |
| Product agreement | transcript, trace and rendered DOM agree on results and order | | |
| Authority | only read-only search/fetch and chat reply agent tools | | |
| Teardown/cost | owned app stopped; actual usage/cost and browser peak RSS recorded | | |

Task cancellation before source contact does not prove cancellation of an
admitted host tool. Retain the outstanding child's cancelled tool receipt if
one exists; otherwise mark that audit claim unexercised/Abort. Likewise,
`len(keys)` alone does not prove direct `len(dict)`. A returned error dict or
a locally raised exception does not prove a caught host-tool exception.

A missing cancellation, failure, identity or running-state observation is
Abort for that gate; a contradicted result is FAIL. Do not turn an unexercised
branch into a pass because its companion law passed. A model repair loop or
unsupported program is a product finding in this first-shot row. Retain all
iterations and costs. Stop the app even after Abort:

```sh
bash scripts/agent-workbench-dev.sh down --port 7549
```

A deterministic twin requires its own fixed Python provider program and dev-only
startup warning. Existing TypeScript dev scenarios cannot stand in for this
paid fluency row. [Recorded scorecard](recorded-run.md).
