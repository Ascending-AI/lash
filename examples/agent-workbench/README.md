# Agent Workbench

A production-grade recoverable-chat host reference for RLM background
processes, subagents, web tools, button triggers, and Restate-backed cron
triggers.

Restate is required. Run the example from the repo root with the bundled
entrypoint. The default command starts the workbench as a detached local
service, waits for readiness, registers the Restate deployment, and then exits
after printing the URL:

```bash
OPENROUTER_API_KEY=... just agent-workbench 3000
```

SQLite is the default. To run every durable workbench facet on a managed,
port-isolated Postgres container instead:

```bash
OPENROUTER_API_KEY=... AGENT_WORKBENCH_POSTGRES=1 just agent-workbench 3000
```

Alternatively, set `AGENT_WORKBENCH_DATABASE_URL` to an existing Postgres database.

Open `http://127.0.0.1:3000`. Useful lifecycle commands:

```bash
just agent-workbench-status 3000
just agent-workbench-logs 3000
just agent-workbench-logs-follow 3000
just agent-workbench-restart 3000
just agent-workbench-down 3000
```

`restart` replaces only the workbench web process and preserves the Restate and
managed Postgres containers. `down` stops the workbench and every container the
entrypoint started.

Validate the example build and unit tests:

```bash
cargo test -p agent-workbench --all-targets
```

For the old attached process style, use `just agent-workbench-foreground 3000`.

The entrypoint checks for Restate ingress/admin on the configured ports. If
they are not already running, it starts `restatedev/restate:1.7.0` in Docker,
waits for ingress/admin, starts the workbench and its in-process Restate
endpoint, registers the endpoint through Restate Admin, then opens the browser.
It writes PID, log, and run metadata under `.agent-workbench/run/`; stale PID
files are cleaned up automatically. Readiness is checked with
`/healthz`, so a random process on the same port is reported as a port conflict
instead of being mistaken for the workbench.

Configuration is read from `.env` or the process environment:

- `OPENROUTER_API_KEY`: model provider key. Startup refuses to continue when it is
  unset or empty unless `AGENT_WORKBENCH_DEV_PROVIDER_SCENARIO` is active.
- `TAVILY_API_KEY`: Tavily key for `web.search(...)` and `web.fetch(...)`, matching the
  CLI web tools.
- `AGENT_WORKBENCH_ADDR`: bind address, default `127.0.0.1:3030`. Passing a
  port to the `just` recipes, for example `just agent-workbench 3000`, binds
  `127.0.0.1:<port>`.
- `AGENT_WORKBENCH_RESTATE_ADDR`: Restate endpoint bind address, default
  `127.0.0.1:9081`. The `just agent-workbench` entrypoint starts Restate with
  host networking, so Restate can call this localhost endpoint directly.
- `RESTATE_INGRESS_URL`: Restate ingress URL, default `http://127.0.0.1:8080`.
- `AGENT_WORKBENCH_DATA_DIR`: persistence directory, default
  `.agent-workbench`.
- `AGENT_WORKBENCH_DATABASE_URL`: use the `lash-postgres-store` session, process,
  trigger, artifact, and process-environment stores at this URL. Unset defaults to
  SQLite.
- `AGENT_WORKBENCH_POSTGRES`: set to `1` to have the dev entrypoint start a managed
  Postgres 16 container and synthesize `AGENT_WORKBENCH_DATABASE_URL`.
- `AGENT_WORKBENCH_POSTGRES_PORT`, `AGENT_WORKBENCH_POSTGRES_IMAGE`, and
  `AGENT_WORKBENCH_POSTGRES_CONTAINER`: managed Postgres overrides. The default port
  and container name are derived from the workbench port so concurrent runs remain
  isolated.
- `AGENT_WORKBENCH_TRACE`: JSONL trace path, default
  `.agent-workbench/trace.jsonl`.
- `AGENT_WORKBENCH_LASHLANG_EXECUTION_TRACE`: JSONL Lashlang execution graph
  trace path, default `.agent-workbench/lashlang-execution.jsonl`.
- `RESTATE_ADMIN_URL`: Restate Admin URL used by the entrypoint for deployment
  registration, default `http://127.0.0.1:19070` so the dev runner does not
  collide with other local Restate/admin listeners.
- `AGENT_WORKBENCH_RESTATE_ADMIN_PORT`: host port for the Restate Admin
  container started by the entrypoint, default `19070`.
- `AGENT_WORKBENCH_RESTATE_NODE_PORT`: host port for the Restate node endpoint
  started by the entrypoint, default `19071`.
- `AGENT_WORKBENCH_RESTATE_ENDPOINT_URL`: URL Restate should use to reach the
  workbench endpoint, default `http://127.0.0.1:9081` for the host-networked
  Docker Restate container started by the entrypoint.
- `AGENT_WORKBENCH_OPEN`: set to `0` to skip opening the browser.
- `AGENT_WORKBENCH_RESTATE_IMAGE`: Restate Docker image for the entrypoint,
  default `restatedev/restate:1.7.0`.
- `AGENT_WORKBENCH_RESTATE_CONTAINER`: Restate Docker container name for the
  entrypoint, default `lash-agent-workbench-dev-restate`.
- `AGENT_WORKBENCH_TOKIO_STACK_BYTES`: Tokio worker thread stack for the
  workbench process, default `8388608`. Override only when diagnosing stack
  regressions or comparing runtime stack-size lanes.
- `AGENT_WORKBENCH_LEASE_HOST_ID`: identity of this workbench's PID namespace
  among every instance sharing the session store. Set it to a unique pod or
  container id when `/etc/machine-id` may be baked into the image; otherwise
  the workbench uses machine id, then hostname as a fallback.
- `OPENROUTER_MODEL`: default `anthropic/claude-sonnet-4.6`.
- `OPENROUTER_MODEL_VARIANT`: default `high`; choose `provider default` in
  the UI to send no variant for models without configurable thinking.
- `AGENT_WORKBENCH_DEV_PROVIDER_SCENARIO`: development-only deterministic provider;
  unset in normal use. Accepted values are `auth-failure-once` (one non-retryable 401,
  then recovery), `rate-limit-once` (one retryable 429, then success), `failed-process`
  (starts a Runtime Process that reports a deterministic failure), and `exec-blocked`
  (parks the first foreground Lashlang execution for break-glass practice, then lets the
  next turn prove recovery). These scenarios make
  no provider network calls, print a startup warning, and use the visible
  `dev/failure-paths` model id. Unknown values fail startup.

Open the workbench at `http://127.0.0.1:3030` by default, or at the port passed
to the `just` recipe. Restate ingress is
`http://127.0.0.1:8080`; the local Restate admin/UI is on
`http://127.0.0.1:19070`.

The browser UI has three work areas: the left rail contains red and blue trigger
buttons, a cron schedule card, per-turn model controls, and the persisted session token
ledger; the center
pane is a chat/event stream; and the right rail polls the process registry for
visible background work. A **chat / accounts** tab switch at the top of the
center pane opens a dedicated mock-email view (see below). The buttons emit
`ui.button.pressed` trigger occurrences. The cron card is
backed by Restate: ask the agent to schedule something and it can construct a
typed `cron.Schedule` source whose registrations sync to Restate virtual
objects. Started Lashlang background processes appear in the right rail. The rail is a
runtime-wide view, so a process remains visible after the session that started it is
deleted or reset. Non-terminal cards expose **cancel**, which submits cooperative
cancellation through `POST /api/work/{process_id}/cancel`; the resulting
`process.cancel_requested` and terminal status come back through the durable process
registry. Hosts can delete and rotate the current session with `DELETE /api/session`
(`POST /api/reset` remains the UI-compatible alias) without deleting Runtime Processes.
Rotation is required: a deleted session id is permanently retired and cannot
be reopened in the same store.
The **stop turn** button (or **Esc**) cooperatively cancels the exact running
turn: `POST /api/turn/cancel` sends its stable session and turn address through
`TurnWorkDriver::request_cancel`. The request lives on Lash's durable
keyed-promise seam, so it survives a workbench web-process restart and is
observed by the current or replayed Restate owner. The authoritative terminal
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
  error. The registry deduplicates stable ids across Restate replay and process
  restart.

The lanes never share a broadcast channel. Internal observation, provider, and
serialization failures are traced server-side; no raw error string is a
product-stream variant. The UI renders stable safe failure copy.

Provisional prose and reasoning are keyed by Lash correlation id. A
`model_attempt_reset` retracts only the superseded chunks. Provisional rows
also retain their producing turn id; a `done` event retracts only rows from its
own turn, so late settlement cannot erase a newer turn's output. A replay gap,
product lag, cancellation settlement, or terminal replacement rebuilds state
from `/api/state`. Recovery fetches are generation-fenced: an out-of-order
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
top of it. A turn that finishes with a terminal value — `finish`, which
`require_finish` forces on the send path — has no runtime-committed assistant
message, so the workbench commits the reply it renders. Either way a completed
turn leaves exactly one committed assistant copy.

`turn_input_applied` is the only application signal. The live path consumes
its typed application objects; snapshot recovery uses
`remote_turn_input_applications()`. The host does not inspect
`RuntimeDiagnostic`, and it never infers application from
`pending_turn_inputs()`: pending input is admission state, not proof that a
canonical message was committed.

### Recovery is not retry-as-copy

Restate recovery replays the same invocation and the same Lash turn id. The
stable product ids and observation cursors converge that replay onto the same
rows. A user-facing “retry turn” is different: it submits a new turn with a new
turn id and is therefore a new transcript copy with new product identities.

Do not use provider `retryable` classification or `had_tool_calls` as evidence
that retrying is free of duplicate external effects. ADR 0042 makes one tool
attempt atomic to Lash, but opaque work performed inside it is at-least-once if
the worker dies after the external effect and before the attempt outcome is
recorded. Recovery must never re-execute an uncertain tool merely to rebuild UI
state; rebuild from durable snapshots, and make externally visible tool effects
idempotent or split them into explicit durable process steps.

The chat composer can upload one PNG (up to 1 MiB) through
`POST /api/attachments`, then includes the returned content-addressed id as
`attachment_id` in `POST /api/turn`. The Restate workflow resolves the durable file-store
blob and supplies it through Lash's `TurnInput::with_image_ref` contract. The same bytes
remain available at `GET /api/attachments/{attachment_id}` across a workbench restart.
`GET /api/state` includes the canonical `session.usage_report()` projection; the left rail
renders its total plus input/output counters. Run the model-free SQLite/Postgres persistence
gate with `just agent-workbench-attachment-usage-gate <port>`.

The **accounts** tab is a mocked multi-account inbox world you control live.
Type a name (for example `Work`) and press **add account** to connect one;
**delete** disconnects it. Each account card has a compose form that delivers a
message into its inbox and shows that inbox inline, with a per-message delete.
Each account is projected into the RLM Lashlang host environment as a typed module
authority of type `Inbox` at `inbox.<slug>`, exposing three operations — a
message is just a title and text, with no recipient address:

```text
<lashlang>
await inbox.work.send({ title: "Standup", text: "Notes attached." })?
listed = await inbox.work.list({})?            // { account, messages: [{ id, title, text }] }
await inbox.work.delete({ id: listed.messages[0].id })?
</lashlang>
```

Because every account shares the `Inbox` authority type, one account-parametric
process can be started against any account, which is the point of the
multi-account showcase:

```text
<lashlang>
process triage(box: Inbox) {
  items = await box.list({})?
  wake { kind: "triage", account: items.account, count: len(items.messages) }
  finish true
}

work = start triage(box: inbox.work)
personal = start triage(box: inbox.personal)
results = await { work: work, personal: personal }
</lashlang>
```

Adding or removing an account enqueues a durable tool-catalog refresh that a
Restate workflow drains and commits — nothing executes in the HTTP handler.
The next opened turn picks up the new `inbox.<slug>` authority automatically.
Inbox tools resolve by parsing the tool name rather than scanning live
accounts, so a session persisted with a since-removed account's tools still
reopens cleanly; the refresh then drops the stale entries, and executing one
fails with the world's unknown-account error.

Delivering a message is the third trigger
source in the demo: the host appends it to the inbox and emits `mail.received`
with payload `mail.Received { account: str, title: str, text: str }`. Like the
button, the emission runs inside a Restate execution scope
(`WorkbenchMailReceivedWorkflow`) so any registered trigger starts a durable
process. Register an inbox concierge once and it fires on every delivery:

```text
<lashlang>
process on_mail(event: mail.Received) {
  work = start inbox.work.list({})
  personal = start inbox.personal.list({})
  inboxes = await { work: work, personal: personal }
  wake { kind: "mail_brief", arrived_in: event.account, title: event.title }
  finish true
}

handle = await triggers.register({
  source: mail.received({}),
  target: on_mail,
  inputs: { event: trigger.event },
  name: "inbox concierge"
})?
finish format("Inbox concierge registered as `{}`.", handle)
</lashlang>
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
<lashlang>
process on_button(event: ui.button.Pressed) {
  wake { kind: "button_pressed", button: event.button, message: event.message }
  finish true
}

handle = await triggers.register({
  source: ui.button.pressed({}),
  target: on_button,
  inputs: { event: trigger.event },
  name: "button watcher"
})?
registrations = await triggers.list({ name: "button watcher" })?
finish format("Registered button watcher `{}`. Active matching registrations: {}.", handle, len(registrations))
</lashlang>
```

The cron card is the schedule reference integration: there is no Lashlang
`schedule` syntax and no UI tick button. In this example, Restate owns the timer
policy. The workbench plugin declares the `cron.Schedule` source; Lashlang builds a
`cron.Schedule` value and registers it with the runtime trigger registry:

```text
<lashlang>
process daily_digest(tick: cron.Tick) {
  wake { kind: "daily_digest_due", tick: tick }
  finish true
}

source = cron.Schedule({ expr: "0 8 * * *", tz: "UTC" })
handle = await triggers.register({
  source: source,
  target: daily_digest,
  inputs: { tick: trigger.event },
  name: "daily_digest"
})?
registrations = await triggers.list({ target: daily_digest })?
finish format("Registered daily digest `{}`. Active matching registrations: {}.", handle, len(registrations))
</lashlang>
```

After a Restate-backed turn registers an enabled `cron.Schedule`, the workbench
syncs that source key to `WorkbenchCronJob/{session_id}:{source_key}`. The
virtual object stores the source request and next execution timestamp in
Restate K/V state. Its `run` handler emits a validated
`cron.Tick` trigger occurrence for that stored source key:

```json
{ "fired_at": "2026-06-02T12:00:00Z" }
```

The occurrence idempotency key includes the journaled fire time, so it is
unique per tick and stable across retries of the same tick. The `run` handler
re-arms the next delayed `run` *before* emitting, so a tick that fails cannot
kill the schedule, and a job whose session is no longer the live workbench
session terminates itself on its next fire. Resetting the workbench cancels
the old session's cron jobs derived from its durable trigger registrations
(plus anything armed in-process), clears the mocked mail world, and rotates
the session; an equal-request re-sync revives a chain whose stored next
execution is already in the past. The trace JSONL files include
`agent_workbench.cron.restate.sync_upserted`,
`agent_workbench.cron.restate.run`, and
`agent_workbench.cron.restate.zombie_cancelled` events.

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
                ty: lashlang::TypeExpr::Union(vec![
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

For a model-free live smoke that starts Restate, registers the endpoint through
Admin, submits a deterministic turn through Restate `/send`, verifies the
returned invocation completed successfully, schedules
`cron.Schedule({ expr: "*/2 * * * * *", tz: "UTC" })`, waits for a tick, checks
the JSONL trace, and asserts no workbench/process invocation remains active,
run:

```bash
just agent-workbench-restate-e2e
```

## Conformance and projection gates

The ownership split is intentional:

- Lash's recoverable-chat conformance cases enter the real observation/recovery
  API and cover snapshot-then-subscribe recovery, trimmed-gap forwarding,
  redelivery identity deduplication, terminal replacement, and the rule that
  dropping observation does not cancel server work.
- The existing `live_restate_ingress_owner_restart_resumes_and_remains_cancellable`
  gate supplies owner-loss evidence by killing the real workbench endpoint
  process, starting a replacement, observing the superseding lease generation,
  and settling the same turn. This runs for SQLite and PostgreSQL under
  `just agent-workbench-restate-e2e`; no separate receipt sink or standalone
  checker is introduced.
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
