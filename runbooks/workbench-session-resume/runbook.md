# E2E Scenario: Workbench Session Resume — Committed Transcript Fidelity

> **Read [../RULES.md](../RULES.md) first** — especially the browser-surface,
> screenshot, polling, real-token, Abort/RCA, and teardown rules. This runbook adds only
> the session-resume scenario.

**Purpose.** Prove that the Agent Workbench reconstructs committed conversation history
from Lash's durable session store after replacing the entire web process, and that the
next turn continues with that history in its provider request. This is deliberately
different from active-turn recovery: every pre-restart turn must settle before restart.

**Real tokens.** Turns use OpenRouter. Gate on exact nonce text supplied by the operator,
committed row counts, request history, and cross-surface agreement—not on prose quality.

## Scenario-specific golden rules

1. **Restart only committed history.** Wait for the idle pill and an empty
   `/api/state.active_turns` after each pre-restart turn. Uncommitted streamed prose is not
   evidence for this scenario.
2. **The replacement process starts cold.** Use `just agent-workbench-restart <port>`;
   do not reload only the page and do not restart Restate or replace the data directory.
3. **The store is authoritative.** Before and after restart, the active `graph_nodes`
   path must contain every committed user and assistant nonce: use
   `<data-dir>/lash-sessions/durable-core.db` in SQLite mode or `lash_graph_nodes` in the managed
   database in Postgres mode. `/api/state.messages` and the rendered transcript must
   project the same ordered rows.
4. **Continuity reaches the provider.** The first post-restart `llm_call_started` record
   in `trace.jsonl` must contain both earlier user nonces and their committed assistant
   replies, as well as the new user nonce. A plausible answer is not a substitute for
   provider-request evidence.
5. **No local-cache credit.** The pass is invalid unless the workbench PID changes while
   the rendered session id and `<data-dir>/session-id` remain unchanged.

## Working material

- Boot with a fresh durable directory:
  `AGENT_WORKBENCH_DATA_DIR=<fresh-tmp> AGENT_WORKBENCH_OPEN=0 just agent-workbench <port>`.
  Gate `GET /healthz` → 200. The entire Restate stack is port-isolated by default: the
  helper derives its endpoint, ingress, admin port, node port, and container name from
  `<port>`, so concurrent runs on distinct workbench ports do not need manual Restate
  overrides. Teardown:
  `just agent-workbench-down <port>`.
- Postgres boot variant:
  `AGENT_WORKBENCH_POSTGRES=1 AGENT_WORKBENCH_DATA_DIR=<fresh-tmp> AGENT_WORKBENCH_OPEN=0 just agent-workbench <port>`.
  Gate the startup trace's `store_backend: "postgres"`. The helper owns a port-isolated
  Postgres 16 container and marker file, preserves it across
  `agent-workbench-restart`, and removes it on `agent-workbench-down`. For store evidence,
  query `lash_graph_nodes` through the managed database coordinates recorded as
  `postgres_host`/`postgres_port` in the run metadata (managed credentials are
  `lash`/`lash`, database `lash`); filter by the rendered session id and the active
  non-tombstoned path, then save the same normalized JSON rows required below. Do not
  look for SQLite files in this variant.

  **Wrong-database trap:** the helper derives `postgres_port` as 15432 plus the
  workbench port's 10-port stride offset, then runs the container with `--network host`
  and `postgres -p "$postgres_port"`. When using `docker exec ... psql -h 127.0.0.1`,
  always pass `-p "$postgres_port"` from the run metadata. Omitting it uses psql's
  default port 5432 and can silently return plausible data from an unrelated host
  Postgres.
- Browser affordances: chat composer, transcript, idle/running pill, rendered session id.
- Backend truth: `GET /api/state`; `POST /api/turn`. The state endpoint returns a
  flattened `StateReadSnapshot`: `.messages`, `.settings.session_id`, and `.transcript`
  are top-level fields. Use `.messages` for committed chat rows and `.transcript` for
  execution-disclosure rows such as settled reasoning and code blocks.
- Durable truth: `<data-dir>/session-id`, `<data-dir>/trace.jsonl`, and either the SQLite
  `graph_nodes` table in `<data-dir>/lash-sessions/durable-core.db` or Postgres
  `lash_graph_nodes` (`node_json`, excluding tombstoned rows), selected by the boot mode.
  Save extracted JSON rows rather than treating a terminal printout as the artifact.
- `trace.jsonl` records use serde-flattened payloads: fields such as `type` and `request`
  are at the record's top level, and request messages have the shape
  `{ "role": ..., "blocks": [{ "kind": ..., "text": ... }] }`. Role vocabulary also
  differs by surface: store rows use `User`/`Assistant`, API rows use
  `user`/`assistant`, and the DOM renders `YOU`/`AGENT`. Normalize roles before comparing
  ordered transcripts; do not treat casing or presentation labels as content drift.

## Phase 0 — Boot and identify the durable session

Require `OPENROUTER_API_KEY`; a missing key is a harness gap → Abort. Boot, poll
`/healthz`, and open the browser. Record the workbench PID, rendered session id,
`/api/state.settings.session_id`, and `<data-dir>/session-id`; require all three ids to
match. Screenshot `00-ready.png`.

## Phase 1 — Commit two distinguishable turns

Submit two short turns sequentially. Include unique literal markers such as
`FIG425-RESUME-ONE-<run-id>` and `FIG425-RESUME-TWO-<run-id>`, and ask that each marker be
included verbatim in the answer. After each submission, poll until the UI is idle,
`active_turns` is empty, and `/api/state.messages` has gained an ordered user/assistant
pair.

Save `/api/state` as `01-before-restart-state.json`. Extract the active-path message
records from `graph_nodes` and save them as `01-before-restart-store.json`; require the
two exact user markers and the exact assistant texts returned by `/api/state`, in the
same order. Screenshot the fully scrolled transcript as `01-committed-transcript.png`.

## Phase 2 — Replace the web process and reconstruct the transcript

Run `just agent-workbench-restart <port>` and poll `/healthz` until ready. Require a new
PID, the unchanged rendered/API/disk session id, and the same idle state. Reload the
browser and gate all of the following before sending another turn:

- the transcript renders all four pre-restart rows in their original order;
- `/api/state.messages` exactly matches `01-before-restart-state.json` for role and text;
- a fresh `graph_nodes` extraction exactly matches the saved committed message sequence.

Any missing row, reordered row, or UI/API/store disagreement is a contract violation →
Abort/RCA. Save state/store extracts as `02-resumed-state.json` and
`02-resumed-store.json`; screenshot `02-reconstructed-transcript.png`.

## Phase 3 — Continue the session and prove provider history

Record the current end offset or record count of `trace.jsonl`. Submit a third turn with
a new literal marker such as `FIG425-RESUME-THREE-<run-id>`. Poll until idle and six
ordered user/assistant rows are present in both the UI and `/api/state`.

From trace records written after the saved boundary, extract the first
`llm_call_started` payload for this turn to `03-provider-request.json`. Require its
serialized request messages to contain:

- both pre-restart user markers;
- the exact two pre-restart assistant texts from `01-before-restart-state.json`;
- the third user marker.

Finally require the store's active path and `/api/state.messages` to contain all six
committed rows in identical order. Save them as `03-continuity-state.json` and
`03-continuity-store.json`; screenshot the fully scrolled transcript as
`03-continuity-transcript.png`.

## Phase 4 — Teardown and score

Run `just agent-workbench-down <port>` and confirm the workbench and its Restate
container are gone.

| Item | Objective gate | Verdict | Evidence |
|------|----------------|---------|----------|
| Boot identity | rendered/API/disk session ids agree | | `00-ready.png` |
| Pre-restart commits | four ordered rows agree in UI, API, and store | | `01-committed-transcript.png`, `01-before-restart-*.json` |
| Cold reconstruction | PID changed; session id and all four rows survived | | `02-reconstructed-transcript.png`, `02-resumed-*.json` |
| Provider continuity | post-restart provider request contains five required history/input markers | | `03-provider-request.json` |
| Continued commit | six ordered rows agree in UI, API, and store | | `03-continuity-transcript.png`, `03-continuity-*.json` |
| No local-cache credit | replacement PID plus unchanged durable identity recorded | | command log, state artifacts |

**Aggregate:** did a replacement web process reconstruct every committed turn from the
session store and send that complete history into the next provider request?

---

_Stop triggers and the Abort/RCA + reporting protocol are in [../RULES.md](../RULES.md)._
