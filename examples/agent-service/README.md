# Agent Service

SQLite-backed localhost chat example for `lash`, RLM mode, typed session
plugin activation, app-owned board tools, semantic streaming, and optional
Restate-backed turns.

Run it:

```bash
OPENROUTER_API_KEY=... cargo run -p agent-service
```

## Raw turn activities

`POST /api/chats/{chat_id}/activities` accepts `{"text":"..."}` and returns
`application/x-ndjson; charset=utf-8`. Each line is exactly one
`RemoteTurnActivity` JSON record. Sequence numbers are scoped to the request and
start at `0`; a complete response contains exactly one `final_value`, and it is
the last line.

This is a raw lane: its response contains no app-owned product rows and no replay
envelope. The fanout sends each activity to the remote sink before the
persistence sink, so a client can observe an activity before its corresponding
app row exists; clients must not perform read-after-see database reads. Once the
HTTP 200 stream has started, a failed turn or a client that disconnects produces
a silently truncated stream, with the error reported only in the server logs.
That behavior is deliberate for this raw transport lane.

In Restate mode the endpoint returns 501. Restate clients must use the app-owned
turn outbox keyed by `turn_id` that the progress stream reads, as described in
the Restate section below.

Validate the example build and unit tests:

```bash
cargo test -p agent-service --all-targets
```

## Retention

The host runs one maintenance pass per hour. It audits the shared runtime
catalog with `gc_unreachable`, vacuums terminal evidence for the app's chat
sessions, reclaims unreferenced attachments after a 7-day grace window, and
prunes terminal processes older than the same window. Attachment reclamation
refuses and reports an empty live root set; it never treats an empty catalog as
standing authorization to delete every eligible blob. A non-zero store-GC
result is a verify/repair finding to investigate, not routine throughput.

This example's chat list is the vacuum catalog. Process-owned sessions are not
listed there and need retention owned by their process host. The attachment
backend and session factory must also belong exclusively to the same
deployment; pairing the backend with the wrong factory can classify live bytes
as unreachable.

## Coverage

The [example coverage matrix](../../runbooks/RULES.md#example-coverage-matrix) is the
source of truth for the CI split:

- **Deterministic CI:** `Test docs + build cache` compiles all workspace targets, and
  `Test shard ${{ matrix.shard }}/4` runs workspace tests.
- **Full-host CI:** `Functional E2E (agent-service)` runs `agent-service-restate-e2e`,
  including the Restate ingress and process-workflow live test; it does not judge the
  browser journeys.
- **Manual judged:** [`agent-service-branching`](../../runbooks/agent-service-branching/runbook.md)
  and [`tictactoe-full-game`](../../runbooks/tictactoe-full-game/runbook.md).

Optional environment:

```bash
OPENROUTER_MODEL=anthropic/claude-sonnet-4.6
OPENROUTER_MODEL_VARIANT=high
AGENT_SERVICE_ADDR=127.0.0.1:3000
AGENT_SERVICE_DATA_DIR=.agent-service
AGENT_SERVICE_TRACE=.agent-service/trace.jsonl
AGENT_SERVICE_DURABILITY=local
```

The durability mode can also be passed as `--durability local`. Restate mode is
feature-gated and uses these local defaults:

| Path | App | Restate endpoint | Ingress | Admin |
| --- | --- | --- | --- | --- |
| App run | `127.0.0.1:3000` | `127.0.0.1:9080` | `127.0.0.1:8080` | `127.0.0.1:9070` |
| Live E2E | in-process test | `127.0.0.1:19080` | `127.0.0.1:18080` | `127.0.0.1:19070` |

For the app run, start Restate on the host network, run the feature-gated binary,
then register the endpoint:

```bash
docker run --rm --network host restatedev/restate:1.7.0
OPENROUTER_API_KEY=... \
AGENT_SERVICE_DURABILITY=restate \
AGENT_SERVICE_RESTATE_ADDR=127.0.0.1:9080 \
RESTATE_INGRESS_URL=http://127.0.0.1:8080 \
cargo run -p agent-service --features restate -- --durability restate

restate deployments register http://127.0.0.1:9080
```

If you run Restate in Docker bridge mode instead, bind the app endpoint to a
container-reachable interface such as `AGENT_SERVICE_RESTATE_ADDR=0.0.0.0:9080`
and register `http://host.docker.internal:9080` (or add
`--add-host=host.docker.internal:host-gateway` on Linux).

For the live E2E, use the one-command recipe. It starts the agent-service
Restate endpoint in-process, registers it through the Restate Admin API, submits
a turn through Restate ingress, runs a named Lashlang background process against
the tic-tac-toe board through `LashProcessWorkflow`, verifies app
outbox/message persistence, and removes the container on exit:

```bash
just agent-service-restate-e2e
```

The recipe starts `restatedev/restate:1.7.0` with host networking on admin
`19070`, ingress `18080`, and node `15122`. Override
`AGENT_SERVICE_RESTATE_IMAGE`, `AGENT_SERVICE_RESTATE_CONTAINER`,
`RESTATE_ADMIN_PORT`, `RESTATE_INGRESS_PORT`, `RESTATE_NODE_PORT`,
`AGENT_SERVICE_E2E_ENDPOINT_BIND`, or `AGENT_SERVICE_E2E_ENDPOINT_URL` if your
local Docker networking needs different addresses.

In Restate mode the Axum app still serves `AGENT_SERVICE_ADDR`, the same process
also serves a Restate endpoint on `AGENT_SERVICE_RESTATE_ADDR`, and browser
turns finish the app-specific `AgentServiceTurnWorkflow/{turn_id}/run/send`
through `RESTATE_INGRESS_URL`. The endpoint also binds Lash's generic
`LashProcessWorkflow`, backed by `RestateCoreProcessRunner` and the same
deployment-level `processes.db`, so background process starts from a turn are
reconstructed from the SQLite durable-core catalog instead of running in the route
process. `AgentServiceTurnWorkflowRequest` carries only stable turn, chat, text,
model, and model-variant data; board state stays in the app database. The
workflow creates a `RestateRuntimeEffectController` and calls
`session.turn(...).turn_id(...).effects(&controller).stream_to(...)`.
The stable chat/session id and turn id keep Restate replay and Lash final
commit addressed to the same operation.
Every message response includes that stable id in the `x-lash-turn-id` header.
An authenticated host can request cooperative cancellation from any process,
without retaining the session handle that submitted the turn:

```http
POST /api/chats/{chat_id}/turns/{turn_id}/cancel
Content-Type: application/json

{
  "request_id": "browser-stop-1",
  "reason": "user pressed Stop"
}
```

The response reports `requested`, `already_requested`,
`completion_won_race`, or `unknown_or_revoked`. In Restate mode the cancel
request and terminal attachment use `RestateTurnDeployment` and
`LashDurableWaitWorkflow`, so the request survives an Axum/web-process restart
and is observed by a replayed turn owner. Cancellation is cooperative and does
not guarantee detached effects have stopped. Chat and turn ids are routing
identity, not authorization; this localhost example verifies that the chat
exists, while a production service must authenticate and authorize callers.
Turn progress is written to an app-owned
SQLite outbox keyed by `turn_id`, so the NDJSON route can stream progress after
route restart or while the workflow is running in the Restate handler.
Semantic progress is sourced from the Lash session observation API rather than
from a route-local side channel: the route captures
`session.observe().current_observation().cursor` before the turn, emits that
opaque cursor as `replay_cursor`, and forwards live
`RemoteSessionObservationEvent` values from
`ObservableSession::subscribe_and_recover_remote(...)` as `observation` NDJSON rows
until the session commit is observed. If the cursor is no longer in the bounded
replay window, the stream emits `replay_gap` with the public
`RemoteLiveReplayGap` payload plus a fresh remote observation snapshot,
including the requested cursor, latest cursor, latest revision, and reason.
Restate mode uses the same live replay path for turn activity and keeps the app
outbox focused on durable product rows such as user messages, assistant
messages, errors, and `done`.

Then open `http://127.0.0.1:3000`.

The model and reasoning variant are also editable in the browser. The
environment values are just the defaults for new chats; each chat persists its
own OpenRouter model id and variant, and each turn applies that selection with
the public `TurnBuilder::model(...)` API.

The example opts into provider-level thinking exposure for demonstration,
attaches a trace sink to `LashCore`, and writes JSONL trace records to stderr
and `AGENT_SERVICE_TRACE` so provider payloads, RLM response, extracted
lashlang, terminal output, and tool calls are visible while you run it.

The app builds a `LashCore` via `LashCore::rlm_builder`, activates `DemoPlugin` per chat session with
`SessionBuilder::plugin::<DemoPlugin>(...)`, and lets the plugin provide
its fixed app tools through the normal `ToolProvider` hook.

The plugin demonstrates:

- Typed session activation through `PluginBinding::SessionConfig`.
- App-owned tic-tac-toe board state in the `chat_boards` SQLite table.
- User message payload board snapshots for browser replay only.
- `read_board` and `play_move` app tools provided by the plugin's
  `ToolProvider`. `read_board` reads the canonical board from SQLite;
  `play_move` validates and mutates that canonical board.
- Prompt contribution that reflects the current canonical board state.
- Additive semantic streaming: thinking is shown live from
  `TurnEvent::ReasoningDelta`, assistant prose as
  `TurnEvent::AssistantProseDelta`, code/tool activity as structured cards, and
  RLM `finish` as `TurnEvent::FinalValue`.
- Runtime persistence is handled by `SqliteSessionStoreFactory`; all requests
  share one factory-wide durable-core SQLite catalog, and each store handle
  remains bound to one session. Each request opens the Lash session from the
  chat id and store instead of keeping runtime
  sessions in a process-global map.
- Product persistence is app-owned: chat rows, board snapshots, reasoning, code
  blocks, tool cards, tool outbox events, and titles stay in the app database.
  The final assistant row is derived from `TurnOutput` terminal semantics,
  preferring `finish` / tool terminal values over streamed prose.
- The app database uses `rusqlite` on `tokio::task::spawn_blocking`, keeping the
  example dependency-light without blocking Axum worker tasks on SQLite calls.

This example opts into `.require_finish()`, so the assistant's final user-facing
text should be placed in `finish`. RLM also supports `.allow_prose_or_finish()`
for turns where direct prose may finish without a lashlang block.
`FinalValue` appears when a turn finishes through `finish`; `ToolValue`
appears when a tool terminal control finishes the turn. Prose-only completion is
already visible through assistant prose deltas.

In lashlang, the model calls the demo tools through their host-declared module
surface. `DemoPlugin` maps the underlying `read_board` tool to `board.read`
and `play_move` to `board.play` with `ToolBinding`:

```text
<lashlang>
board = await board.read({})?
move = await board.play({ cell: 4 })?
finish "I played the center."
</lashlang>
```

The browser also listens for submitted/tool value stream events and renders
their JSON-shaped value with the same display rule as Lash: strings pass
through, `null` is empty, and other values pretty-print.
