# Agent Workbench

A production-grade recoverable-chat host reference for RLM background
processes, subagents, web tools, deferred-tool discovery, button triggers, and
cron triggers. Its subagents are the host-written delegation tool of
[`examples/delegation`](../delegation/README.md): each child is created with
the workbench's own session defaults, never copied from its parent.

Run the example from the repo root with the bundled entrypoint. The default
command starts the workbench as a detached local service, waits for readiness,
and then exits after printing the URL:

```bash
OPENROUTER_API_KEY=... just agent-workbench
```

SQLite is the default. To run every durable workbench facet on a managed,
port-isolated Postgres container instead:

```bash
OPENROUTER_API_KEY=... AGENT_WORKBENCH_POSTGRES=1 just agent-workbench 3000
```

Alternatively, set `AGENT_WORKBENCH_DATABASE_URL` to an existing Postgres database.

Open `http://127.0.0.1:3030`. Pass a port, for example `just agent-workbench 3000`, to
override the web port; the helper derives the dependent Postgres port from that value. Useful lifecycle commands:

```bash
just agent-workbench-status 3000
just agent-workbench-logs 3000
just agent-workbench-logs-follow 3000
just agent-workbench-restart 3000
just agent-workbench-reset 3000
just agent-workbench-down 3000
```

`restart` replaces only the workbench process and keeps the application data. `reset` is
explicitly destructive: for a wholly launcher-owned disposable stack, it clears the SQLite/data
directory or managed Postgres state, then starts fresh. It refuses legacy, external, mixed, or
ambiguous ownership. `down` stops the workbench and every exactly identified container the
entrypoint started, and leaves the stopped stack's application data and ownership records in
place. Running `just agent-workbench <port>` again resumes that stopped stack: the same data
directory and durable state, a fresh engine and process. `just agent-workbench-reset <port>`
clears a stopped stack as readily as a running one.

Durability scenarios that require state to survive a process replacement use `restart`. Do not
use `agent-workbench-reset` for them; it deliberately deletes the evidence they assert survives.

## Sessions

The workbench serves many sessions. The sidebar lists them, switches between
them, and adds new ones. TypeScript is the sole RLM language (ADR 0096); the
language menu is still served by `GET /api/sessions`, never written into the
page.

- `GET /api/sessions` — the roster, each session's RLM language, the current
  selection, the registered language ids, and the default.
- `POST /api/sessions` — `{"name": "…", "dialect": "typescript"}`. The language
  field is optional; any id other than `typescript` is refused at creation
  rather than quietly served TypeScript.
- `POST /api/sessions/select` — `{"session_id": "…"}` makes a rostered session
  the one a query-less `/api/` call resolves to.

Two durable files carry the selection: `<data-dir>/session-id` is the
plain-text current selection the runbook drivers read and write, and
`<data-dir>/sessions.json` beside it is the roster, one row per session.

Validate the example build and unit tests:

```bash
kiln test //examples/agent-workbench:agent-workbench__unit_test
```

## MCP integrations

The workbench owns MCP integrations through `GET /api/mcp/servers`,
`POST /api/mcp/servers` with `{"name":"workspace_http","url":"http://127.0.0.1:3032/mcp","token":"workbench-mcp-fixture-token"}`,
and `DELETE /api/mcp/servers/{name}`. Attach and detach use the public MCP
factory; the next turn receives the refreshed catalog.

The workbench binary also serves a deterministic peer with
`agent-workbench mcp-fixture stdio`, or `agent-workbench mcp-fixture http`
with `AGENT_WORKBENCH_MCP_ADDR=127.0.0.1:3032`. The HTTP fixture requires
the bearer token above. Set `AGENT_WORKBENCH_MCP_FIXTURE_BIN` to the same
binary to connect the stdio fixture beside search at host startup.
Its tools exercise provider-backed sampling, form and URL elicitation,
workspace roots, and a stored binary attachment. Retrieval preserves the retained
bytes, serving PNG as `image/png` and other binary resources as
`application/octet-stream`. `AGENT_WORKBENCH_SEARCH_MCP_URL`
overrides the search peer URL for an isolated fixture run.

## Coverage

The [example coverage matrix](../../runbooks/RULES.md#example-coverage-matrix) is the
source of truth for the CI split.

- **Manual judged:** [`workbench-process-lifecycle`](../../runbooks/workbench-process-lifecycle/runbook.md),
  [`workbench-session-resume`](../../runbooks/workbench-session-resume/runbook.md), and
  [`workbench-deferred-tools`](../../runbooks/workbench-deferred-tools/runbook.md), plus
  the other `workbench-*` runbooks.

For the old attached process style, use `just agent-workbench-foreground 3000`.

The entrypoint starts the workbench, then opens the browser.
It writes PID, log, and run metadata under `.agent-workbench/run/`; stale PID
files are cleaned up automatically. Readiness is checked with
`/healthz`, so a random process on the same port is reported as a port conflict
instead of being mistaken for the workbench.

Configuration is read from `.env` or the process environment:

- `OPENROUTER_API_KEY`: model provider key. Startup refuses to continue when it is
  unset or empty unless `AGENT_WORKBENCH_DEV_PROVIDER_SCENARIO` is active.
- `AGENT_WORKBENCH_ADDR`: bind address, default `127.0.0.1:3030`. Passing a
  port to the `just` recipes, for example `just agent-workbench 3000`, binds
  `127.0.0.1:<port>`.
- `AGENT_WORKBENCH_DATA_DIR`: persistence directory, default
  `.agent-workbench`.
- `AGENT_WORKBENCH_LIVE_REPLAY_STORE`: the live replay store session feeds
  tail. `memory` (the default when unset) keeps one process's observation
  events in process; `postgresql` shares them through PostgreSQL, so every
  workbench replica's feed carries every replica's events. A feed's snapshot
  is the session's durable head either way.
- `AGENT_WORKBENCH_LIVE_REPLAY_DATABASE_URL`: the database the `postgresql`
  live replay store uses; unset falls back to `AGENT_WORKBENCH_DATABASE_URL`.
- `AGENT_WORKBENCH_LIVE_REPLAY_CONFIG`: the selected live replay store's
  configuration as one JSON object; absent fields keep their defaults and an
  unknown or out-of-range field refuses startup. `memory` takes
  `max_events_per_session` (2048), `max_age_ms` (120000), `max_sessions`
  (4096) and `max_retained_bytes` (67108864). `postgresql` takes
  `lash::postgres::LiveReplayPolicy`, the `live_replay` section of the host
  configuration (`docs/operations/postgres.md`): `data` (`schema`,
  `schema_mode`, `publish_tick_ms`, `publish_concurrency`, retention and
  cleanup), `pool` (the data pool, 7 connections), `listener` and
  `reconnect`. The workbench defaults `data.schema_mode` to `install`;
  `verify_only` runs no DDL and refuses tables that differ from
  `crates/lash/postgres-live-replay-schema.sql`.
- `AGENT_WORKBENCH_DATABASE_URL`: use the `lash-postgres-store` session, process,
  trigger, artifact, and process-environment stores at this URL. Unset defaults to
  SQLite.
- `AGENT_WORKBENCH_POSTGRES`: set to `1` to have the dev entrypoint start a managed
  Postgres 18 container and synthesize `AGENT_WORKBENCH_DATABASE_URL`.
- `AGENT_WORKBENCH_POSTGRES_PORT`, `AGENT_WORKBENCH_POSTGRES_IMAGE`, and
  `AGENT_WORKBENCH_POSTGRES_CONTAINER`: managed Postgres overrides. The default port
  and container name are derived from the workbench port so concurrent runs remain
  isolated.
- `AGENT_WORKBENCH_TRACE`: JSONL trace path, default
  `.agent-workbench/trace.jsonl`.
- `AGENT_WORKBENCH_LASHLANG_EXECUTION_TRACE`: JSONL Lashlang execution graph
  trace path, default `.agent-workbench/lashlang-execution.jsonl`.
- `AGENT_WORKBENCH_OPEN`: set to `0` to skip opening the browser.
- `AGENT_WORKBENCH_TOKIO_STACK_BYTES`: Tokio worker thread stack for the
  workbench process, default `8388608`. Override only when diagnosing stack
  regressions or comparing runtime stack-size lanes.
- `AGENT_WORKBENCH_CONTEXT_WINDOW_TOKENS`: model context window used by every
  workbench session, default `200000`. Values must be integers of at least
  `40000`, twice the plugin's compaction buffer (currently 20,000), so the
  threshold leaves a useful prompt band before compaction.
- `AGENT_WORKBENCH_DELTA_FRAME_MS`, `AGENT_WORKBENCH_DELTA_FRAME_MAX_BYTES`,
  `AGENT_WORKBENCH_DELTA_FIRST_IMMEDIATE`: how the live feed coalesces
  streamed prose and reasoning deltas (`lash::DeltaCoalescing`). Defaults
  `50`, `8192` and `true`: 50 ms frames of at most 8 KiB, with each block's
  first delta sent at once. `AGENT_WORKBENCH_DELTA_FRAME_MS=off` (or `0`)
  sends one event per delta. Out-of-range values refuse to start.
- `AGENT_WORKBENCH_LEASE_HOST_ID`: identity of this workbench's PID namespace
  among every instance sharing the session store. Set it to a unique pod or
  container id when `/etc/machine-id` may be baked into the image; otherwise
  the workbench uses machine id, then hostname as a fallback.
- `OPENROUTER_MODEL`: default `z-ai/glm-5.3-flash`.
- `OPENROUTER_MODEL_VARIANT`: default `high`; choose `provider default` in
  the UI to send no variant for models without configurable thinking.
- `AGENT_WORKBENCH_DEV_PROVIDER_SCENARIO`: development-only deterministic provider;
  unset in normal use. Accepted values are `auth-failure-once` (one non-retryable 401,
  then recovery), `rate-limit-once` (one pre-output 429 with `Retry-After`, then success),
  `partial-output-failure` (paid partial output followed by a retryable stream failure), `failed-process`
  (starts a Runtime Process that reports a deterministic failure), and `exec-blocked`
  (parks the first foreground cell execution for break-glass practice, then lets the
  next turn prove recovery), and `tool-value` (calls a dev-only terminal control tool so the
  live stream carries a deterministic `tool_value`). Rendered-surface gates also use
  `rendered-surface` (reasoning plus a structured final value), `code-failure` (a failing
  cell), and `retry-reset-partial` (partial text, correlated reset, visible retry,
  then replacement output). These scenarios make
  no provider network calls, print a startup warning, and use the visible
  `dev/failure-paths` model id. Unknown values fail startup.

Web search and fetch come from the free Parallel Search MCP server (`parallel`),
attached at startup with no API key or auth headers. An unreachable server never
fails startup: the workbench serves without web tools and reconnects in the
background.

Open the workbench at `http://127.0.0.1:3030` by default, or at the port passed
to the `just` recipe.

The browser UI has three work areas: the left rail contains red and blue trigger
buttons, a cron schedule card, per-turn model controls, and the persisted session token
ledger; the center
pane is a chat/event stream; and the right rail polls the process registry for
visible background work. A **chat / accounts** tab switch at the top of the
center pane opens a dedicated mock-email view (see below). The buttons emit
`ui.button.pressed` trigger occurrences. Ask the agent to schedule something and
it can construct a typed `cron.Schedule` source; the cron card lists its
registrations. Started background processes appear in the right rail. The rail is a
runtime-wide view, so a process remains visible after the session that started it is
deleted or reset. Non-terminal cards expose **cancel**, which submits cooperative
cancellation through `POST /api/work/{process_id}/cancel`; the resulting
`process.cancel_requested` and terminal status come back through the durable process
registry. Hosts can delete and rotate the current session with `DELETE /api/session`
(`POST /api/reset` remains the UI-compatible alias) without deleting Runtime Processes.
The response waits for the durable delete to finish: success rotates to
the returned id, while a terminal delete failure returns `409` and identifies
the old session as still live. Rotation is required after success because a
deleted session id is permanently retired and cannot be reopened in the same
store.

The workbench's prompt is prompt sections of its `agent_workbench` plugin:
it replaces the standard protocol's intro with its own identity, renders
`instructions` and `accounts` from the host text the session records in the
plugin's `agent_workbench` config, and adds `context_budget`, which reports
the prepared history's size late in the call. Connected inbox accounts are
recorded prompt context. Adding or removing an account, or clearing accounts
on reset, applies `SetWorkbenchPromptContext` to live sessions. New sessions
record the current accounts at creation. Every turn source uses that config,
while a running run and its replay keep their recorded config, and a call's
recorded prompt keeps what it was sent.

Six low-frequency data utilities under `text`, `json`, and `list` are kept out
of the resident RLM tool catalog. The prompt carries only a capped catalogue
preview and the resident `tools.search` contract. Search results persist full
execution grants in `<data-dir>/deferred-tool-grants.db`; the RLM factory's
production `DeferredToolResolver` authorizes only those stored call paths. The
search tool's own description states the handshake constraint: discovered
operations become callable in the next code block. Grants remain available in later turns.
Persistence across a cold process replacement remains the required contract, but its live step is
currently blocked by the launcher lifecycle constraint above. See
[`runbooks/workbench-deferred-tools`](../../runbooks/workbench-deferred-tools/runbook.md) for the
preserved three-layer acceptance gates.

### Durable approval is host policy

The resident `ops.apply_change` demo tool shows the approval pattern without
adding an approval concept to Lash. The provider calls
`AttemptContext::completion_key()`, writes the key, tool arguments, requesting
session, and request time to the host-owned `<data-dir>/approvals.db`, and
returns `ToolOutcome::Pending`. The right-rail approval ledger and
`GET /api/approvals` list those waits; approve and deny actions resolve the
existing key through `LashCore::completions()`.

`GET /api/sessions/{session_id}/waits` separately demonstrates the
deployment-administrative discovery read. It returns every currently
registered, unresolved durable wait for that session, not approval requests;
tool arguments, classification, and decision history remain in the approval
ledger. The result is a concurrent snapshot, so a key can settle before an
operator acts on it. Because each returned key carries the authority accepted
by `Completions::resolve`, the example protects the route with both session
observation and deployment-operator authorization; production hosts must apply
their own equivalent policy.

Approval is host policy. Lash core will never grow a manifest approval flag or
approval/revert API: hosts decide which tools need sign-off, how operators are
authorized, and what approve or deny means. Lash supplies only the durable
completion-key park/resume primitive. The workbench's local authorization is
an allow-all example, not a production admin boundary. See
[`runbooks/workbench-approval`](../../runbooks/workbench-approval/runbook.md)
for approve, deny, and parked-restart checks.

The **stop turn** button (or **Esc**) cooperatively cancels the exact running
turn: `POST /api/turn/cancel` sends its stable session and turn address through
`DurableSession::cancel(CancelTarget::Run(id))`, which uses the atomic
`TurnWorkDriver::request_cancel` path. A queued input returns `Withdrawn`;
an open run returns `Cancelled` with its accepted request detail; an ended or
unknown run returns `UnknownOrRevoked`. The request lives on Lash's durable
keyed-promise seam, so it survives a workbench web-process restart and is
observed by the current or recovered owner. The authoritative terminal
result is `TurnStop::Cancelled` with the original request id, opaque
host-defined origin, and optional reason; the UI clears only after the request
is accepted or the turn has already won the completion race.

Cancellation is cooperative: detached effects and non-cooperative external
work are not guaranteed to stop. Disconnecting either browser observation
stream only drops that subscription; it never invokes cancellation. The Stop
control is the separate, explicit `POST /api/turn/cancel` operation.

Session and turn ids are routing identity, not authorization. `AppState` carries
a `WorkbenchAuthorization` hook over the `WorkbenchAuthorizer` trait. The
reference invokes it before snapshot/observation, turn enqueue, turn-input
enqueue, and cancellation. Its local default is intentionally allow-all; a
production host replaces `AllowAllWorkbenchAuthorizer` with its identity and
policy adapter. Lash does not define product-specific auth.
The Lashlang graph panel is backed by `TraceLashlangGraphStore`, a public
trace-derived observation store for foreground blocks, durable process runs,
and child execution links; command operations still go through the session's
`SessionProcessAdmin` facade.

## Recoverable-chat host structure

The browser starts with exactly one `GET /api/state` materialization. That
authoritative response contains the transcript, durable
`remote_turn_input_applications()`, the Lash observation snapshot/cursor, and
the workbench product-event snapshot/cursor. It then attaches two independent
lanes after those cursors:

- `/api/observations` is Lash's lane. The server enters
  `ObservableSession::subscribe_recoverable_chat` directly, then encodes its
  updates for HTTP. It forwards provisional turn activity,
  `RemoteLiveReplayGap`, and terminal replacement. Event identity is
  `(session_id, replay_incarnation_id, cursor)`, so a consumer may safely retain
  its bounded identity cache when the server restarts and reuses cursor values.
  A replay gap still clears pre-gap identities before the stream continues from
  its authoritative snapshot cursor.
- `/api/events` is the product lane. `SessionEventRegistry` first appends every
  event to `.agent-workbench/product-events.json` with a monotonic per-session
  sequence and stable event id, then broadcasts it as a freshness hint. A
  lagged subscriber receives an authoritative `resync` snapshot instead of an
  error. The registry deduplicates stable ids across recovery and process
  restart. This log is now the durable home of the user half of the transcript,
  so its growth is intentional and bounded by reset, which drops the session
  history and persists that removal. Rewriting the full snapshot after every
  mutation is a known reference-host simplification, not the intended shape of
  a production event store.

The lanes never share a broadcast channel. Internal observation, provider, and
serialization failures are traced server-side; no raw error string is a
product-stream variant. The UI renders stable safe failure copy.

At a `continue_as` boundary, all old-frame assistant replies and trigger-driven
inputs collapse by design; user chat rows persist across the switch.

Provisional prose and reasoning are keyed by Lash correlation id. A
`model_attempt_reset` retracts only the superseded chunks. Provisional rows
also retain their producing turn id; a `done` event retracts only rows from its
own turn, so late settlement cannot erase a newer turn's output. A terminal
replacement carries its commit's rows, not the session (FIG-5100), and the
timeline upserts them in place. A replay gap, product lag, or cancellation
settlement rebuilds state from `/api/state`. Recovery fetches are generation-fenced: an out-of-order
response or a response overtaken by a newer product event is discarded.
Authoritative replacement rebuilds both dedup sets and assigns, rather than
monotonically preserves, the snapshot cursor. Cursor and dedup state are scoped
to the returned session id, so reset cannot carry positions into the new
session. The state endpoint merges product-only rows onto the complete
canonical Lash transcript by stable id; it never replaces canonical history
with a partial product log. The live agent row is always
`workbench-assistant:<turn_id>`, and it retires from the product log when its
turn stops running rather than when a committed message happens to share its id,
so a live/canonical pair is one row, never two. Which copy is canonical depends
on how the turn terminated. A turn that finishes *as* an assistant message —
bare prose, the shape a queued or wake turn reaches because it runs without
`require_finish` — already has that reply committed by the runtime as the turn's
terminal message under a runtime-minted id, and the workbench commits nothing on
top of it. When that answer carries reasoning the copy is committed one layer
earlier still, by the RLM protocol itself, as a plugin-origin assistant message
holding the reasoning and the prose; the runtime then adds no terminal message
because the answer is already the transcript's last one. The projection admits
that last plugin-authored prose message as the turn's reply — and only that one,
so the protocol's mid-turn prose stays out of the chat (FIG-1406). A turn that
finishes with a terminal value — `finish`, which
`require_finish` forces on the send path — has no runtime-committed assistant
message, so the workbench commits the reply it renders. Either way a completed
turn leaves exactly one committed assistant copy.

### One turn at a time, admitted honestly

A session runs one turn at a time — shift epoch admission and the
commit-CAS fence enforce that durably — so `POST /api/turn` cannot start a turn
on a session that already has one running. It admits the send as the next turn's
input instead and says so: `{"accepted":true,"queued":true,"queued_input":{…}}`
carries the same `TurnInputReceipt` `/api/turn/input` returns, and the same
`turn_input` product event reaches every viewer, so a second client's message is
held durably, rendered as a queued receipt, and answered as its own turn once the
session's engine executes the next run over it. No optimistic user row is published for it: the receipt is the
row, and the drained turn's committed message reconciles against it through
`turn_input_applied`.

That check is advisory, exactly like the one behind `inject now`. Two sends can
both read an idle session and race, and the lease and CAS — not the handler —
decide who commits. So the losing side has to be visible rather than silent: the
turn that fails retires the rows the workbench published for it, renders one
failure row, and publishes `done` with `outcome: "failed"`. A viewer that already
rendered those rows re-derives from `/api/state` on that outcome, which is the
only way a rendered row is removed, so no viewer keeps a conversation row whose
commit was refused.

`turn_input_applied` is the only application signal. The live path consumes
its typed application objects; snapshot recovery uses
`remote_turn_input_applications()`. The host does not inspect
an untyped diagnostic, and it never infers application from
`pending_turn_inputs()`: pending input is admission state, not proof that a
canonical message was committed.

### Recovery is not retry-as-copy

Recovery resumes the same Lash turn id. The stable product ids and observation
cursors converge the resumed turn onto the same rows. A user-facing “retry turn” is different: it submits a new turn with a new
turn id and is therefore a new transcript copy with new product identities.

Do not use provider `retryable` classification or `had_tool_calls` as evidence
that retrying is free of duplicate external effects. ADR 0042 makes one tool
attempt atomic to Lash, but opaque work performed inside it is at-least-once if
the worker dies after the external effect and before the attempt outcome is
recorded. Recovery must never re-execute an uncertain tool merely to rebuild UI
state; rebuild from durable snapshots, and make externally visible tool effects
idempotent or split them into explicit durable process steps.

Leaf providers use sealed `AttemptContext` and return versioned `ToolIntents`;
Lash records the final attempt before realizing each declaration. Process
starts, signalling, cancellation, and typed process-event emission are
therefore declarations. A detached start records `on_parent_end: Abandon`;
owned children use the default `Cancel`. Lash processes are cooperative and
Lash deliberately has no hard-kill primitive; engines own kill semantics, so
v1 records the deviation from Temporal's three-way Parent Close Policy as the
two-policy set `{Abandon, Cancel}`. A future hard-kill policy must ship together
with a real process primitive under reject-and-recreate versioning. Command-time
failures and replay outcomes are durable evidence and do not depend on live
visibility during redrive.

The chat composer can upload one PNG (up to 1 MiB) through
`POST /api/attachments`, then includes the returned content-addressed id as
`attachment_id` in `POST /api/turn`. The turn resolves the durable file-store
blob and supplies it as a stored MIME-tagged `AttachmentSource` through Lash's generic turn
contract. The Workbench's PNG-only check is a host-surface policy: Lash's provider transports
enforce their own image/file allowlists from the recorded `lash::provider::AttachmentCapabilitySnapshot`, and an
unsupported MIME/source combination returns the typed `unsupported_attachment_capability`
refusal before wire serialization. The same bytes remain available at
`GET /api/attachments/{attachment_id}` across a workbench restart.
That retrieval route is deliberately not session-gated so reloads and retired sessions still render: the unguessable SHA-256 content address is an unexpiring bearer capability with no session data in its URL, blobs outlive sessions pending ADR 0024 reclamation, and hosts MUST gate the route if their ids are not content addresses or ids can reach viewers who may not read the blob.
The left rail renders reported usage from observed turn events. It resets when
the browser session resets; hosts retain billing receipts at the provider seam
(ADR 0127). The attachment persistence gate also verifies usage in model traces.

The **accounts** tab is a mocked multi-account inbox world you control live.
Type a name (for example `Work`) and press **add account** to connect one;
**delete** disconnects it. Each account card has a compose form that delivers a
message into its inbox and shows that inbox inline, with a per-message delete.
Each account is projected into the RLM host environment as a typed module
authority of type `Inbox` at `inbox.<slug>`, exposing three operations — a
message is just a title and text, with no recipient address:

```text
<typescript>
await inbox.work.send({ title: "Standup", text: "Notes attached." });
const listed = await inbox.work.list({});      // { account, messages: [{ id, title, text }] }
await inbox.work.delete({ id: listed.messages[0].id });
</typescript>
```

Because every account shares the `Inbox` authority type, one account-parametric
process can be started against any account, which is the point of the
multi-account showcase:

```text
<typescript>
const triage = async (box: Inbox) => {
  const items = await box.list({});
  await processes.emit({ value: { kind: "triage", account: items.account, count: items.messages.length } });
  return true;
};

const work = await processes.start({ definition: triage, args: { box: inbox.work } });
const personal = await processes.start({ definition: triage, args: { box: inbox.personal } });
finish(await Promise.all([work, personal]));
</typescript>
```

Adding or removing an account enqueues a durable tool-catalog refresh that the
engine drains and commits — nothing executes in the HTTP handler.
The next opened turn picks up the new `inbox.<slug>` authority automatically.
Inbox tools resolve by parsing the tool name rather than scanning live
accounts, so a session persisted with a since-removed account's tools still
reopens cleanly; the refresh then drops the stale entries, and executing one
fails with the world's unknown-account error.

Delivering a message is the third trigger
source in the demo: the host appends it to the inbox and emits `mail.received`
with payload `mail.Received { account: str, title: str, text: str }`. Like the
button, the emission runs inside a durable execution scope so any registered
trigger starts a durable process. Register an inbox concierge once and it fires on every delivery:

```text
<typescript>
const onMail = async (event: mail.Received) => {
  const [work, personal] = await Promise.all([
    inbox.work.list({}),
    inbox.personal.list({})
  ]);
  await processes.emit({ value: {
    kind: "mail_brief",
    arrived_in: event.account,
    title: event.title,
    waiting: work.messages.length + personal.messages.length
  } });
  return true;
};

const handle = await triggers.register({
  source: mail.received({}),
  target: { definition: onMail },
  inputs: (event) => ({ event }),
  name: "inbox concierge"
});
finish(`Inbox concierge registered as \`${handle.subscription_key}\`.`);
</typescript>
```

This gives the demo three kinds of trigger source — a UI button trigger
occurrence, an inbound email data occurrence, and a cron schedule tick — all
activating durable processes through the same registry. Source constructors such
as `cron.Schedule` and `mail.received` live in the plugin's
`lashlang_resources()` hook. The button source is zero-config and exposed from
its trigger declaration.

The button source config is `{}`. Red/blue selection arrives in the event
payload:

```text
<typescript>
const onButton = async (event: ui.button.Pressed) => {
  await processes.emit({ value: { kind: "button_pressed", button: event.button, message: event.message } });
  return true;
};

const handle = await triggers.register({
  source: ui.button.pressed({}),
  target: { definition: onButton },
  inputs: (event) => ({ event }),
  name: "button watcher"
});
const registrations = await triggers.list({ name: "button watcher" });
finish(
  `Registered button watcher \`${handle.subscription_key}\`. ` +
  `Active matching registrations: ${registrations.length}.`
);
</typescript>
```

The cron card is the schedule reference integration: there is no `schedule`
syntax in the language and no UI tick button. The workbench plugin declares the `cron.Schedule` source; the cell
builds a `cron.Schedule` value and registers it with the runtime trigger
registry:

```text
<typescript>
const dailyDigest = async (tick: cron.Tick) => {
  await processes.emit({ value: { kind: "daily_digest_due", tick } });
  return true;
};

const source = cron.Schedule({ expr: "0 8 * * *", tz: "UTC" });
const handle = await triggers.register({
  source,
  target: { definition: dailyDigest },
  inputs: (tick) => ({ tick }),
  name: "daily_digest"
});
const registrations = await triggers.list({ target: { definition: dailyDigest } });
finish(
  `Registered daily digest \`${handle.subscription_key}\`. ` +
  `Active matching registrations: ${registrations.length}.`
);
</typescript>
```

Lash has no scheduler: a host dispatches every trigger occurrence itself. The
workbench runs its own `cron.Schedule` timer (`src/cron.rs`). Every quarter
second it reads the enabled `cron.Schedule` registrations and, for each
session's source, emits the latest tick it has not passed yet through the
host trigger emit (`core.triggers().emit`), scoped to that session. The
occurrence carries `cron.Tick { fired_at }`, and its idempotency key names the
session, the source and the tick instant, so a second workbench over the same
database, a restart or a retried emission lands each tick once. On boot the
timer catches up the latest tick missed since the registration last changed,
once. Because the timer reads the registration before it emits, a disable, a
re-enable or a delete takes effect at the next tick, and a re-enabled
schedule keeps its subscription. While no workbench runs, nothing ticks.

Host wiring has two pieces: source constructors such as `cron.Schedule` and
`mail.received` are declared through the plugin's `lashlang_resources()` hook,
while the button is a zero-config source exposed by its trigger declaration. The
button payload is validated by that registration:

```rust
fn schedule_config_type() -> lashlang::TypeExpr {
    lashlang::TypeExpr::Object(vec![
        lashlang::TypeField {
            name: "expr".into(),
            ty: lashlang::TypeExpr::Str,
            optional: false,
        },
        lashlang::TypeField {
            name: "tz".into(),
            ty: lashlang::TypeExpr::Str,
            optional: true,
        },
    ])
}

fn cron_tick_event_type() -> lashlang::NamedDataType {
    lashlang::NamedDataType::object(
        "cron.Tick",
        vec![lashlang::TypeField {
            name: "fired_at".into(),
            ty: lashlang::TypeExpr::Str,
            optional: false,
        }],
    )
    .expect("valid cron tick type")
}

fn button_trigger_event_type() -> lashlang::NamedDataType {
    lashlang::NamedDataType::object(
        "ui.button.Pressed",
        vec![
            lashlang::TypeField {
                name: "button".into(),
                ty: lashlang::TypeExpr::union(vec![
                    lashlang::TypeExpr::Enum(vec!["Red".into()]),
                    lashlang::TypeExpr::Enum(vec!["Blue".into()]),
                ]),
                optional: false,
            },
            lashlang::TypeField {
                name: "message".into(),
                ty: lashlang::TypeExpr::Str,
                optional: false,
            },
            lashlang::TypeField {
                name: "pressed_at".into(),
                ty: lashlang::TypeExpr::Str,
                optional: false,
            },
        ],
    )
    .expect("valid button trigger event type")
}

fn mail_received_event_type() -> lashlang::NamedDataType {
    lashlang::NamedDataType::object(
        "mail.Received",
        vec![
            lashlang::TypeField {
                name: "account".into(),
                ty: lashlang::TypeExpr::Str,
                optional: false,
            },
            lashlang::TypeField {
                name: "title".into(),
                ty: lashlang::TypeExpr::Str,
                optional: false,
            },
            lashlang::TypeField {
                name: "text".into(),
                ty: lashlang::TypeExpr::Str,
                optional: false,
            },
        ],
    )
    .expect("valid mail received event type")
}

fn workbench_lashlang_resources() -> lashlang::LashlangHostCatalog {
    let mut resources = lashlang::LashlangHostCatalog::new();
    resources.add_trigger_source_constructor(
        ["cron", "Schedule"],
        schedule_config_type(),
        cron_tick_event_type(),
    )
    .expect("valid cron trigger source");
    resources.add_trigger_source_constructor(
        ["mail", "received"],
        lashlang::TypeExpr::Object(vec![]),
        mail_received_event_type(),
    )
    .expect("valid mail trigger source");
    resources
}

reg.triggers().declare(
    TriggerEvent::new("Button", "ui.button", "pressed", button_trigger_event_type()),
)?;
```

## Conformance and projection gates

The ownership split is intentional:

- Lash's recoverable-chat conformance cases enter the real observation/recovery
  API and cover snapshot-then-subscribe recovery, trimmed-gap forwarding,
  redelivery identity deduplication, terminal replacement, and the rule that
  dropping observation does not cancel server work.
- Workbench projection cases cover correlation-based provisional retraction,
  the real `/api/events` resync response after forced broadcast lag,
  generation-fenced browser recovery, session-scoped reset cursors, canonical
  history merged with a partial product log, the real turn-output
  live/canonical identity across reload, interleaved turn settlement that keeps
  newer provisional output, distinct cancel settlement events, typed turn-input
  application, and fixed public copy from a real provider failure. The browser
  cases execute the production JavaScript reducer under Node; the workspace
  test shards install Node explicitly.

Each gate asserts the violated invariant directly: duplicate identity changes a
row count, a swallowed gap prevents recovery, a missing terminal replacement
leaves the authoritative message absent, disconnect-as-cancel prevents the turn
from completing, lag without resync loses ordered product events, and raw
failure text fails the safe-copy assertion.

## Recovery

Use `lashctl` for deployment operations. Set `LASH_SQLITE_PATH` to the
workbench's database file `<data-dir>/lash-sessions.db`, or `LASH_POSTGRES_DATABASE_URL` for a
PostgreSQL store.

- `lashctl --json stalled list <kind> [--after <id>] [--limit 50]` pages stalled
  deliveries, including their last typed error. `stalled rearm <kind> <id>`
  resets that delivery in its owning ledger.
- `lashctl --json deployment-status --accepting-new-work false` reports live
  and parked work. The flag describes host admission policy; it changes no routing.
- `drain <generation>` and `end-drain <generation>` remain the PostgreSQL
  generation drain verbs.

Recovery commands also accept `--sqlite-path <database-file>` instead of
`LASH_SQLITE_PATH`.
Page limits are nonzero and at most 200; retain `next` to read the next page.
The standard lashctl JSON envelope and exit codes apply to every verb.

Lash automatically compacts context as needed. Recorded settings and session
command submission, settlement and withdrawal are embedder APIs on
`session.admin()`; the workbench has no deployment operator dialog.
Usage is provider result data, metered at the host's provider seam (ADR 0127).

The commented builder block in `src/main_sections/bootstrap.rs` demonstrates
output retention, attachment limits and expiry, recovery lease and pass
budgets, termination, abort drain grace, trigger route restoration, process
observation, live replay and trace context. Those deployment policies are
chosen before building the core; session changes use recorded commands.
