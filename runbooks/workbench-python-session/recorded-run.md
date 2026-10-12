# FIG-5989 — recorded Python session

One frontier/SQLite/Python browser row, 2026-10-12, source `6cdb45a4e75c09b0b2fa2900db4cf48b5951e5a6`
with this lane's runbook/matrix edits. Judged workbench and worker build:
`.kiln/FIG-5989/workbench-build.json`. Provider: `openai/gpt-6.1-sol`, variant
`high`, confirmed in all five completed call receipts and served-model witnesses.
One composer prompt, no operator repair or paid retry; four executed code rows
(three successful, one refused). Actual provider cost: **$0.0726761**.

Artifact root: `.kiln/FIG-5989/run-root/workbench-python-session/python/sqlite/`.
The startup record is `type=custom`, `name=agent_workbench.startup`, with
Python/SQLite and no dev scenario. Session:
`workbench-374c0d279da94b8d8256458c15f2915e`; turn:
`workbench-turn-64736865-3534-4ed6-9b4b-c59600465307`.

| Gate | Observed | Verdict/evidence |
| --- | --- | --- |
| Companions | five full selectors, executed=5 passed=5 | PASS; five Kiln receipts/logs |
| Identity | Python startup, all four transcript code rows Python, served matrix model | PASS; `00-startup.json`, `fluency-hits.json` |
| First-shot fluency | first cell refused `PY_ASYNC_UNSUPPORTED`: gather's `return_exceptions=True`; model repaired it | FAIL; first cell source/error retained |
| Fan-out | repaired cell starts Utrecht/Berlin tasks and awaits gather; both searches issued | PASS for execution; two weather results unavailable because Berlin was rate-limited |
| Cancel | Paris task reports cancelled/contacted_source=False; no Paris fetch/search issued | PASS for task cancellation; Abort for admitted host-tool cancellation, which was not exercised |
| Dict surface | keys iteration, `.get`, `result['search_id']` execute; five keys, correct search id/default | Partial/Abort: key count uses `len(keys)`, so direct `len(dict)` was not exercised |
| Failure | Berlin search and verification fetch fail with typed `mcp_json_rpc_error`, caught as `tool_failed` | PASS; trace and cell output. Invalid URL itself returned an error dict; locally raised RuntimeError is not a host exception |
| Finish | chat_reply, one completed/finished terminal, no active turns | PASS; `trace.jsonl`, `02-state.json` |
| Product agreement | DOM/transcript retain failures, exception and final reply; read-only reload captures latest reply | PASS; `02-dom.txt`, `03-final-reply.png`, `03-reloaded-state.json` |
| Authority | only Parallel web search/fetch and reply agent tools | PASS; five completed tool receipts |
| Teardown/resources | owned host stopped; app max RSS 173228 KiB, Chromium max RSS 199944 KiB | PASS; resource logs and stopped status |

**Overall: FAIL for first-shot fluency, with unexercised gates retained as Abort.**
The rate limit prevented the requested two successful weather results. The
final reply accurately disclosed the rate limit and conflicting/unverified
source evidence. No law, gate, model tier or runtime bound was relaxed.

The app ran under `ulimit -v 8388608`, confirmed in `/proc/<owned-pid>/limits`;
Chromium ran uncapped under its own `/usr/bin/time -v` wrapper. Evidence:
`python-app-process.json`, `python-app-rss.txt`, `browser-rss.txt` and
`python-final-status.log`, under `.kiln/FIG-5989/`. The timed foreground
launcher holds its lifecycle lock, so a separate `down` was refused; a
verified PID/start-time SIGTERM stopped only this owned host, and foreground
cleanup completed successfully. A read-only same-session reload corrected the final screenshot; it submitted
no prompt and produced no new model call. Its app/browser RSS is retained
separately. No second paid run, browser-cap retry or model substitution occurred.
