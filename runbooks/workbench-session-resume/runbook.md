# E2E Scenario: Workbench Session Resume — Committed Transcript Fidelity

> **Read [../RULES.md](../RULES.md) first** — especially the browser-surface,
> screenshot, polling, real-token, Abort/RCA, and teardown rules. This runbook adds only
> the session-resume scenario.

**Purpose.** Prove that the Agent Workbench reconstructs committed conversation history
from Lash's durable session store after replacing the entire web process, and that the
next turn continues with that history in its provider request. This is deliberately
different from active-turn recovery: every pre-restart turn must settle before restart.

**Deterministic provider.** Run this scenario with the opt-in
`replay-route-change` development LLM Provider and initial model
`dev/replay-route-a`. It returns numbered terminal values and mints one opaque reasoning
replay carrier per call, so the continuity and route-filter gates cannot pass by
coincidence. The run is still browser-driven and must be judged by `gpt-5.6-sol`; retain
its prompt and verdict with the run artifacts.

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
6. **Composition re-emits after cold reconstruction.** Save the final pre-restart
   `composition_changed` snapshot. The first post-restart model request must emit another
   `composition_changed`; its exact `rendered_system_prompt` and ordered `tool_schemas`
   must match the saved snapshot when the host changed neither input.
7. **Replay routing stays observable.** Provider-owned response-text, reasoning, and tool
   replay state may be served only by the exact LLM Provider replay route that minted it:
   provider kind, normalized configured endpoint, and model. The runtime preserves neutral text
   while stripping unstamped or foreign opaque state
   and emits one standard-level `provider_replay_dropped` trace event per stripped carrier,
   including on failure, protocol abort, or cancellation. This same-route resume scenario
   requires zero such events after the Phase 3 trace boundary. Phase 4 then changes only the
   model component of that route and requires matching drop rows in both the trace and operator
   surfaces. Those rows are emitted by the same structural pass that removes each foreign
   carrier; the pre-filter `llm_call_started` request is context evidence, not a wire capture.

## Working material

- Boot with a fresh durable directory:
  `AGENT_WORKBENCH_DEV_PROVIDER_SCENARIO=replay-route-change OPENROUTER_MODEL=dev/replay-route-a AGENT_WORKBENCH_DATA_DIR=<fresh-tmp> AGENT_WORKBENCH_OPEN=0 just agent-workbench <port>`.
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
- Route endpoints in replay-drop rows are operator-visible routing metadata. URL userinfo is
  rejected; do not put credentials in endpoint paths or query strings because those bytes are
  deliberately identity-significant and are not redacted from traces.

## Phase 0 — Boot and identify the durable session

Boot with the exact deterministic-provider environment above, poll `/healthz`, and open the
browser. Require the model control to render `dev/replay-route-a` and retain the startup log row
that names `replay-route-change`; any other provider/model is a harness gap → Abort. Record the
workbench PID, rendered session id,
`/api/state.settings.session_id`, and `<data-dir>/session-id`; require all three ids to
match. Screenshot `00-ready.png`.

## Phase 1 — Commit two distinguishable turns

Submit two short turns sequentially with unique literal markers such as
`FIG425-RESUME-ONE-<run-id>` and `FIG425-RESUME-TWO-<run-id>`. After each submission, poll until the UI is idle,
`active_turns` is empty, and `/api/state.messages` has gained an ordered user/assistant
pair. Require the assistant rows to be exactly `FIG-1374 replay-route response 1` and
`FIG-1374 replay-route response 2`.

Save `/api/state` as `01-before-restart-state.json`. Extract the active-path message
records from `graph_nodes` and save them as `01-before-restart-store.json`; require the
two exact user markers and the exact assistant texts returned by `/api/state`, in the
same order. Screenshot the fully scrolled transcript as `01-committed-transcript.png`.
Save the final pre-restart `composition_changed` record as
`01-final-composition.json`.

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

Extract the first post-restart `composition_changed` record to
`03-composition-reopen.json`. Require exact equality of its
`rendered_system_prompt` and ordered `tool_schemas` with
`01-final-composition.json`; compare those fields directly, not only the fingerprint.

Also extract any `provider_replay_dropped` records after the boundary to
`03-provider-replay-drops.json` and require the array to be empty: all replay state in this
same-route continuation was minted by the selected route. A non-empty array is typed evidence
of a route/configuration mismatch, not harmless trace noise, and requires Abort/RCA.

Finally require the store's active path and `/api/state.messages` to contain all six
committed rows in identical order. Save them as `03-continuity-state.json` and
`03-continuity-store.json`; screenshot the fully scrolled transcript as
`03-continuity-transcript.png`.

## Phase 4 — Minted-carrier route switch proves filtering and drop evidence

This phase is mandatory. It is the mutation-resistant proof that replay filtering and
drop-evidence emission actually ran.

From the active `graph_nodes` path after Phase 3, extract every reasoning replay carrier whose
provider-owned signature starts with `FIG1374-OPAQUE-REPLAY-` to
`04-minted-replay-carriers.json`. Require at least one row, and require each row's origin to name
provider `workbench-dev-failure`, endpoint `workbench-dev-failure`, and model
`dev/replay-route-a`. Record the current end offset or record count of `trace.jsonl`.

In the browser's model control, replace `dev/replay-route-a` with
`dev/replay-route-b`; leave the provider kind and endpoint unchanged. Submit a fourth turn with
marker `FIG425-RESUME-ROUTE-SWITCH-<run-id>` and poll until idle. Require the assistant result
`FIG-1374 replay-route response 4` and eight committed user/assistant rows.

From trace rows after the Phase 4 boundary:

- save the first, pre-filter `llm_call_started` request as
  `04-pre-filter-provider-request.json` and require it contains all prior neutral
  `FIG-1374 replay-route response` text, portable `FIG-1374 portable reasoning` summaries,
  and the provider-owned reasoning candidates (`has_encrypted: true`) that the serving-route
  fence must inspect. This trace record intentionally precedes `prepare_completion`; it does
  not claim to show the serialized provider wire;
- save `provider_replay_dropped` rows as `04-provider-replay-drops.json` and require at least
  one `reasoning` / `foreign_route` row whose minting route model is
  `dev/replay-route-a` and serving route model is `dev/replay-route-b`;
- save the model-call record exposed by the workbench product-event/operator surface as
  `04-model-call-record.json` and require its `replay_drops` contains the same foreign-route
  evidence.

Any missing minted carrier, missing pre-filter candidate, absent trace drop, absent operator-surface
drop, or route mismatch is a contract violation → Abort/RCA. Screenshot the model control,
terminal response, and execution scorecard as `04-route-filtered.png`.

## Phase 5 — Teardown and score

Run `just agent-workbench-down <port>` and confirm the workbench and its Restate
container are gone.

| Item | Objective gate | Verdict | Evidence |
|------|----------------|---------|----------|
| Boot identity | rendered/API/disk session ids agree | | `00-ready.png` |
| Pre-restart commits | four ordered rows agree in UI, API, and store | | `01-committed-transcript.png`, `01-before-restart-*.json` |
| Cold reconstruction | PID changed; session id and all four rows survived | | `02-reconstructed-transcript.png`, `02-resumed-*.json` |
| Provider continuity | post-restart provider request contains five required history/input markers | | `03-provider-request.json` |
| Composition continuity | cold reopen re-emits and exact prompt plus ordered schemas match | | `01-final-composition.json`, `03-composition-reopen.json` |
| Replay-route continuity | no replay carrier is dropped on the same-route continuation | | `03-provider-replay-drops.json` |
| Continued commit | six ordered rows agree in UI, API, and store | | `03-continuity-transcript.png`, `03-continuity-*.json` |
| Minted replay precondition | stored opaque carrier is stamped with provider kind + endpoint + model A | | `04-minted-replay-carriers.json` |
| Foreign-route filter context | model-B pre-filter trace request contains neutral history and provider-owned candidates for the serving-route fence | | `04-pre-filter-provider-request.json` |
| Drop evidence | trace and product-event surfaces expose matching `reasoning` / `foreign_route` evidence | | `04-provider-replay-drops.json`, `04-model-call-record.json` |
| No local-cache credit | replacement PID plus unchanged durable identity recorded | | command log, state artifacts |

**Aggregate:** did a replacement web process reconstruct every committed turn from the
session store, continue with the full history, then prove a minted opaque carrier was filtered
and evidenced when the exact LLM Provider replay route changed?

---

_Stop triggers and the Abort/RCA + reporting protocol are in [../RULES.md](../RULES.md)._
