# slack-clone

A multiplayer, Slack-shaped chat platform, and a Lash bot living inside it as a
guest.

Every other example in this repository is a host that **owns** its UI — Lash is
the product, and the browser talks to Lash. This one is inverted, which is the
shape most real integrations have: somebody else's product already exists, it has
its own users and its own database, and your agent is one more app in it, reached
only over HTTP.

It is also the repository's **standard-mode reference host**. Turns here are plain
chat turns driven by the native tool loop (`LashCore::standard_builder(TurnBudget::Unbounded)`).
`agent-workbench` remains the RLM-mode reference; see
[Modes](#modes-this-is-the-standard-mode-reference) below.

```
                 browser tabs (one per human)
                            │
                            ▼
 ┌──────────────────────────────────────────┐        ┌────────────────────────┐
 │  slack-clone-platform                    │        │  slack-clone-bot       │
 │  (no Lash dependency)                    │        │  (the Lash embedding)  │
 │                                          │        │                        │
 │  users / channels / messages in SQLite   │        │  session per channel   │
 │  Slack-compatible Web API      ◄─────────┼────────┤  slack_api.rs client   │
│  Events API outbox, at-least-once ───────┼───────►│  channel sessions      │
│                                          │        │  thread session forks │
 └──────────────────────────────────────────┘        └────────────────────────┘
```

## Run it

```bash
export OPENROUTER_API_KEY=sk-...          # the bot needs a model; the platform does not
just slack-clone                          # platform :3040, bot :3041, HTTP MCP server :3042
```

Open <http://127.0.0.1:3040>, pick a display name, and open a second tab with a
different name — two tabs are two people. Type anything: the bot stays quiet but
is listening. Mention it (`<@U…>`, shown in the sidebar) and it answers with the
whole room's recent traffic already in context.

```bash
just slack-clone-status        # every process, plus /healthz
just slack-clone-logs-follow   # tail the logs
just slack-clone-down
```

State lives under `.slack-clone/`. `cargo test -p slack-clone` needs no model key
— the suite drives a scripted provider.

## Coverage

The [example coverage matrix](../../runbooks/RULES.md#example-coverage-matrix) is the
source of truth for the CI split:

- **Deterministic CI:** `Test docs + build cache` compiles all workspace targets, and
  `Test shard ${{ matrix.shard }}/4` runs the workspace tests, including the Slack
  package tests.
- **Full-host CI:** the `slack-clone-full-host` functional E2E leg runs
  `just slack-clone-full-host-e2e` (token-free, deterministic). CI still does not
  run `just slack-clone` interactively or the real-token MCP client-depth path.
- **Manual live-model CI:** the dispatch-only `Slack-clone live-model acceptance`
  workflow runs one RLM agent and one standard agent through OpenRouter. It is
  never triggered by pushes, pull requests, or schedules.
- **Manual judged:** [`slack-clone-bot`](../../runbooks/slack-clone-bot/runbook.md),
  whose Phase 3M covers MCP client depth and runtime integration attach/detach.

For the executable, token-free downstream acceptance used by CI, run:

```bash
just slack-clone-full-host-e2e
```

That companion boots the platform, bot, and HTTP MCP server as separate
processes, lets the bot spawn the bundled MCP stdio child, drives two independent
headless Chromium contexts, kills and restarts the bot during a claimed turn,
attaches and detaches the HTTP MCP server mid-run, reloads both humans,
and emits a machine-readable DOM/platform/bot/trace scorecard. State and evidence
live in a temporary directory outside the checkout by default; set
`LASH_SLACK_CLONE_E2E_ARTIFACT_DIR` to retain them at a chosen location. See the
[deterministic coverage boundary](../../runbooks/slack-clone-deterministic/runbook.md).

The `e2e` Cargo feature and `SLACK_CLONE_E2E_PROVIDER=scripted-v1` selector are
test-harness implementation details. Both are required together; ordinary
launches always require `OPENROUTER_API_KEY` and keep using the manual OpenRouter
path.

### Real-model two-agent acceptance

`just slack-clone-live-model-e2e` is the manual FIG-1388 companion to the
token-free full-host leg. It starts only the existing platform, then runs two
fresh Lash sessions in one fresh channel: Agent A uses RLM/Lashlang with
`anthropic/claude-sonnet-5`; Agent B uses the standard native tool loop with
`deepseek/deepseek-v4-flash-0731`. Override those with
`LASH_LIVE_E2E_RLM_MODEL` and `LASH_LIVE_E2E_STANDARD_MODEL`; the harness
refuses unpriced slugs instead of guessing their cost.

For a local run, copy the gitignored key into the shell environment without
printing it, select the required private target directory, and invoke the
recipe:

```bash
set -a
source /workspace/code/lash/.env
set +a
export CARGO_TARGET_DIR=/workspace/.cargo-target-lash-fig1388
just slack-clone-live-model-e2e
```

An unset `OPENROUTER_API_KEY` prints an explicit skip and exits zero. The same
policy applies in the manual workflow when its repository secret is absent.
The default invocation cap is `$2`, configured by
`LASH_LIVE_E2E_MAX_SPEND_USD`. Provider retries are disabled and every request
atomically reserves its conservative worst-case input/output cost before it is
sent; actual OpenRouter usage metadata then settles that reservation. At the
defaults, 21 possible Sonnet calls plus 24 possible DeepSeek calls, each capped
at 32,768 input and 512 output tokens, reserve at most `$1.94138112` across the
three smoke probes and both swap attempts.

The behavior verdict is deterministic despite using real models: Agent A must
submit Agent B's fresh random nonce, Agent B must submit Agent A's, and a
headless Chromium assertion must find both exact nonces in the rendered
channel. There is no LLM judge. A failed first attempt retries once with a new
channel, sessions, and nonce pair. Failure artifacts include both traces, the
two Lash session transcripts, full platform transcript, spend ledger,
screenshot, and DOM dump. Set
`LASH_SLACK_CLONE_LIVE_E2E_ARTIFACT_DIR` to retain them at a chosen path.

## What this demonstrates

| Concern | Where |
| --- | --- |
| Session per durable conversation | `bot/runtime.rs::session_id`, `bot/channel.rs` |
| Thread as a forked child session | `bot/threads.rs`, `bot/runtime.rs::thread_session_id` |
| Bounded thread-root admission deferral | `bot/threads.rs::open_thread_session` |
| Ambient context as queued turn input, with no turn | `bot/channel.rs::ingest` |
| Mention-triggered turn that drains the queue | `bot/channel.rs::run_mention_turn` |
| Standard-mode native tool loop | `bot/tools.rs` |
| MCP tools in that same standard tool loop | `mcp_server.rs`, `bot/runtime.rs` |
| Idempotent event consumption | `bot/ledger.rs` |
| Restart recovery, stage by stage | `bot/channel.rs::recover` |
| Acting on the typed reason an empty drain reports | `bot/channel.rs::settle_empty_drain` |
| Bounded retry of fenced work and delayed thread roots | `bot/channel.rs::retry_deferred` |
| Reading a lost reply back out of the transcript | `bot/channel.rs::reply_from_transcript` |
| Transactional outbox | `platform/state.rs::post_message` |
| A liftable API client | `bot/slack_api.rs` |
| One wire contract for both processes | `wire/methods.rs`, `wire/events.rs` |

## MCP

The bot registers `lash-plugin-mcp` when it builds its `LashCore`. The plugin
spawns the bundled `slack-clone-mcp-server` over stdio and imports its tools into
the same catalog as `list_channels` and `channel_history`:

- `mcp__slack_clone__list_channels_summary` returns channel ids, names, topics,
  and member counts.
- `mcp__slack_clone__workspace_stats` returns aggregate channel and active-member
  counts. The explicit `active_members` field excludes deleted users; channel
  summaries expose the platform's workspace-wide `num_members` value instead.
- `mcp__slack_clone__sample_summary` sends `sampling/createMessage` back to the
  bot, whose host-owned handler runs its configured provider through
  `DirectLlmClient` and returns the sampled summary to the still-open tool call.
- `mcp__slack_clone__elicit_confirmation` sends a typed form elicitation; this
  example's host policy checks the requesting server, prompt, and schema, then
  builds `{ "answer": "yes" }` from the requested string property.
- `mcp__slack_clone__elicit_via_url` sends URL elicitation and then
  `notifications/elicitation/complete`; the host checks the URL policy and logs
  the matching elicitation id when completion arrives.
- `mcp__slack_clone__list_host_roots` sends `roots/list` and returns the static
  workspace root supplied by the bot host.

The server uses the official `rmcp` server-side SDK. Its results are not fixtures:
both tools call the platform's Slack-compatible HTTP API with the bot token. A
mention asking for one of these summaries therefore traverses the standard model
tool loop, `lash-plugin-mcp`, stdio JSON-RPC, the separate server process, and the
platform HTTP API before the result reaches the transcript.

The server-to-client features follow the same ownership rule: Lash routes
the MCP request but supplies no model, answer, or root. The bot wires all three
handlers explicitly with `McpPluginFactory::builder`; removing one handler also
removes its capability from the initialize handshake. Sampling uses a direct
provider call inside the outer MCP tool attempt. It does not open a nested Lash
turn or emit another durable command.

Handler work must stop when its request cancellation token fires. These seams
are in-attempt host I/O and must not emit journaled Lash effects. Host-owned
sampling is billed by the host: its usage is visible in provider/host traces,
not in the session usage ledger or `TurnReport` usage.

[Run the judged MCP client-depth walkthrough](../../runbooks/slack-clone-bot/runbook.md)
(`slack-clone-bot` Phase 3M) for real-provider semantic judgement, or the
[deterministic full-host companion](../../runbooks/slack-clone-deterministic/runbook.md)
for the exact four-tool, four-layer CI contract.

`lash-plugin-mcp` prefixes imported tools as `mcp__<server>__<tool>`. Ordinary
native names therefore do not collide. If a host deliberately registers the exact
same fully prefixed name, Lash rejects the catalog update as a duplicate instead
of shadowing either implementation.

The stdio child is demonstration wiring, not a deployment prescription — a real
deployment more often reaches a separately operated endpoint over
`McpServerConfig::streamable_http(...)`, which is what the second bundled server
below demonstrates. `SLACK_CLONE_MCP_SERVER` can override
the local server executable while developing this example; the bot passes the API
origin and token to its child process without placing the token in argv. Bot boot
fails with a clear error when a configured stdio executable cannot be found, so
an absent binary cannot leave the prompt advertising unavailable bundled tools.
HTTP headers are static across reconnects: Lash does not enable rmcp OAuth or
token refresh, so a deployment that rotates tokens must rebuild or reattach the
server config with a host-managed credential.

Each stdio or streamable-HTTP server has the same timeout, liveness, and reconnect
policy knobs. In Rust these fields live in `McpCallPolicy`, one flattened
`call_policy` field on either transport variant; serialized configuration keeps
the flat shape shown below.

| Field | Default | Behavior |
| --- | ---: | --- |
| `startup_timeout_ms` | `10_000` | Bounds initialization and tool discovery. Initialization is never cancelled. |
| `call_timeout_ms` | `60_000` | Idle clock for a tool call. Matching progress notifications reset it by default. |
| `call_max_total_timeout_ms` | `600_000` | Mandatory wall-clock cap, regardless of progress. |
| `reset_call_timeout_on_progress` | `true` | Lets a progressing call extend past the idle clock. |
| `timeout_disconnect_policy` | `ping_probe` | `never`, `ping_probe`, or `consecutive_timeouts`; decides whether an idle timeout disconnects the peer. |
| `liveness_probe_timeout_ms` | `5_000` | Bounds a timeout-triggered or interval `ping`. |
| `consecutive_timeouts_before_disconnect` | `3` | Counting threshold; a successful tool call resets the counter. |
| `liveness_probe_interval_ms` | `0` | Optional background probe interval; `0` disables keepalive. About `30_000` is a useful starting point for half-open streamable-HTTP connections. |
| `reconnect_initial_backoff_ms` | `500` | Initial reconnect backoff before full jitter. |
| `reconnect_max_backoff_ms` | `30_000` | Maximum exponential reconnect backoff before full jitter. |
| `reconnect_max_attempts` | `0` | Reconnect attempt limit; `0` retries without a limit. |

`call_timeout_ms` is now an idle clock rather than a total wall-clock deadline.
Deployments whose servers emit matching progress can therefore run beyond that
duration, up to the default 600-second `call_max_total_timeout_ms` cap. Set the
wall cap explicitly when that longer upper bound is not appropriate.
Interactive elicitation is the sharp edge: the 60-second idle default collides
with a prompt a person leaves open for a minute. Raise both
`call_timeout_ms` and `call_max_total_timeout_ms` above the longest supported
interaction (with the wall cap strictly greater than the idle timeout), and
dismiss the host prompt when its cancellation token fires.

Tool-call expiry uses rmcp's cancellable request path, so a timeout sends the MCP
`notifications/cancelled` notification. Under the default `ping_probe` policy, an
answered probe returns a typed timeout while preserving the healthy connection; a
failed probe returns unavailable, records the disconnect cause, and starts the
configured reconnect loop. MCP protocol `2026-07-28` removed `ping`, so Lash warns
once per server and automatically degrades `ping_probe` to
`consecutive_timeouts`; interval probes become a no-op for that negotiated version
and later versions, and the keepalive task exits instead of polling pointlessly.

When keepalive is enabled, a disconnected entry re-arms a reconnect loop after a
bounded attempt set is exhausted (including a failed boot connection). With
keepalive disabled, a server with bounded reconnect attempts stays down after
exhaustion until it is attached again.

### A second server, over HTTP, attached while the bot runs

`slack-clone-mcp-http-server` is the other transport: an `axum` process on
loopback (platform port + 2) serving `rmcp`'s streamable-HTTP transport behind a
bearer-token layer. The bot does **not** wire it at boot. It arrives through the
bot's operator API (`bot/mcp_admin.rs`), which is the ownership point worth
copying: **attaching a tool source is an operator act, never a model act**. The
routes sit behind their own operator credential (`SLACK_CLONE_ADMIN_TOKEN`, not
the platform's event verification token) and no tool in the catalog can reach
them; a host that let a turn attach its own server would have handed the model
its own permission system.

```bash
curl -sS -X POST http://127.0.0.1:3041/admin/mcp/servers \
  -H 'authorization: Bearer slack-clone-dev-admin' \
  -H 'content-type: application/json' \
  -d '{"name":"workspace_http","url":"http://127.0.0.1:3042/mcp","token":"slack-clone-mcp-http-dev-token"}'
curl -sS http://127.0.0.1:3041/admin/mcp/servers -H 'authorization: Bearer slack-clone-dev-admin'
curl -sS -X DELETE http://127.0.0.1:3041/admin/mcp/servers/workspace_http \
  -H 'authorization: Bearer slack-clone-dev-admin'
```

`POST` calls `McpPluginFactory::attach_server` and answers with the server's
status row, because attach registers the server even when the eager connect is
refused: a wrong token comes back `connected: false` with the 401 in
`last_error`, and the pool keeps retrying in the background. That is also this
example's proof that `.with_headers(...)` does something — the server's auth layer
is the oracle. `GET` merges `McpPluginFactory::server_statuses()` with
`McpConnectionPool::advertised_tools()`, since "is this integration healthy" needs
both halves. `DELETE` calls `detach_server`, and the next session the bot opens
no longer sees the tools. `POST /admin/mcp/roots` publishes a workspace root and
then calls `notify_roots_changed`, so connected servers re-read `roots/list`.

The five tools exist to make host-side policy observable rather than to be
useful:

- `mcp__workspace_http__workspace_badge` returns a binary blob resource. This
  server is attached `.with_binary_content_attachments(true)`, so the blob is
  persisted through the host's attachment store and reaches the model as an
  attachment reference; the same call against a server configured without that
  opt-in stays inline in the tool result.
- `mcp__workspace_http__roots_change_report` counts the roots-changed
  notifications the server received and reports the roots it re-read, which is
  what makes `notify_roots_changed` observable from the server's side.
- `mcp__workspace_http__elicit_unknown_prompt` asks a question the host has no
  standing answer for, using a field name the host *does* answer elsewhere. The
  host declines: its answer book is keyed by prompt and field together, because
  elicitation is a consent primitive and consent keyed by field name alone is
  blind consent.
- `mcp__workspace_http__elicit_pick_count` asks for a form field the host's
  answer book cannot satisfy; the host declines instead of sending content that
  fails the server's schema (`McpElicitationValidationError` is what catches it).
- `mcp__workspace_http__stall` never answers, so the host's configured
  `with_timeouts(...)` is what ends the call. The host keeps the **default**
  timeout-disconnect policy, which treats an idle timeout as a question rather
  than a verdict: it probes the peer, and because this server is alive and
  answers the probe, the call comes back as a typed tool failure the model sees
  while the connection survives to serve the next call. A dead peer would fail
  the probe instead, and the entry would be marked disconnected with the cause
  recorded and reconnect started. The obvious-looking alternative,
  `TimeoutDisconnectPolicy::Never`, is not what this host ships: with the
  default liveness probe interval of `0` nothing else ever tests the peer, so a
  server that died mid-call would keep reporting `connected: true` forever.
  `a_host_can_opt_out_of_timeout_disconnects_entirely` in `tests/mcp.rs`
  exercises that opt-out and records the trade-off.

### What this host does not exercise

Two public `lash-plugin-mcp` types are not reachable from this example, for
different reasons, and the difference matters to anyone reading the example as
the coverage story for the crate:

- `McpToolProvider` is registered by the plugin itself when a session is built,
  so a host that wires MCP through `McpPluginFactory` — the supported path —
  never names the type. Reaching it here would mean bypassing the plugin, which
  would make the example lie about how hosts integrate MCP.
- `McpDeferredToolProvider` **is** reachable, just not from here. The plugin
  registers the eager provider, so an RLM host that wants MCP tools as deferred
  grants constructs `McpDeferredToolProvider::new(factory.pool())` itself.
  slack-clone is a standard-mode host, so it has no deferred-grant seam to hang
  that on; this is an open coverage gap belonging to an RLM example, not a type
  no example can use.

### Plugin lifecycle

A real host owns the plugin lifecycle as well as plugin registration. At boot,
the bot reads `McpPluginFactory::server_statuses()` and logs every configured
server's connection state and imported tool count. On exit, Axum first stops
accepting webhook events; only then does the bot call `LashCore::shutdown()`.
That method awaits the protocol factory first and then common factories in
configured order; the MCP factory uses the opportunity to stop every connection
and reap every stdio child. Factories own disjoint resources, so this fixed order
exists only for deterministic, auditable logs and carries no dependency
semantics. A host that shares resources across factories must not depend on it.

The ordering is the contract: stop intake, then call `LashCore::shutdown()`.
Plugin shutdown does not drain or abort active turns. Each plugin implementation
must make shutdown idempotent and bound its own cleanup time. Shutdown continues
past factory errors and returns the first error after the full walk. rmcp's kill-on-drop
behavior remains a last-resort fallback for a host that exits without running
the explicit lifecycle, not the normal shutdown path.

## Session mapping doctrine

**One channel, one Lash session, forever.** The session id is
`channel:<C…>` (`bot::runtime::session_id`).

A channel is a durable, long-lived conversation with a stable id the platform
already guarantees is unique — the same shape a Lash session has. Keying on
anything shorter-lived throws the room's memory away:

- keyed per mention → the bot has amnesia between questions;
- keyed per user → the bot cannot see a conversation, only one side of it;
- keyed per thread as the only mapping → loses everything said in the channel body;
- keyed per process → the bot forgets the room on every deploy.

Because the session id is derived from platform data and the stores are SQLite,
nothing needs to be handed from one boot to the next.

### Ambient traffic is queued input, not a turn

Most messages in a channel are not for the bot. It still needs to have heard
them, or its first answer of the day is context-free.

- **Ambient** (`message` events with no mention) →
  `session.enqueue(TurnInput::text(...)).id(...).send()`. Durable, ordered,
  model-visible admission. **No turn runs, no token is spent, nothing is posted.**
- **Mention** (`app_mention`) → the mention text is enqueued the same way, then
  `session.queued_turn().drain_id(...).run()` folds *every* queued input —
  accumulated room context and the mention — into **one** turn.

So a room can be busy for an hour and cost nothing, and the answer when it comes
has the hour in it. The queued-work driver is deliberately switched off
(`disable_queued_work_driver()`) so that nothing but a mention can ever cause the
bot to speak.

Every enqueue carries a source key derived from the message's `ts`
(`ambient:<channel>:<ts>`), and `ts` *is* message identity. A redelivered event
therefore resolves to the admission record Lash already holds instead of adding a
duplicate context line — idempotence at the runtime layer, independent of the
bot's own ledger.

## Threads

The reference mapping is **workspace → one Lash core, channel → one durable
session, thread → one forked child session**. A thread id is deterministic:
`thread:<C…>:<thread_ts>`, but that id is never opened as a fresh root. The bot
creates it only through `LashCore::pin` plus `fork_at`, so the child shares the
channel graph through its source boundary and owns a new branch after it.

The lazy trigger is the **first reply in the thread**, whether ambient or a
mention. This is slightly more eager than waiting for the first mention, but it
has one durable state instead of a separate pre-engagement buffer: ambient
replies enqueue directly on the child, cost no model call, and the first mention
drains them there. No thread event is ever admitted to `channel:<C…>`.

### Seeding the thread root

Inheriting the prefix is not the same as knowing the root. Lash forks at a
committed graph boundary and has no concept of a "thread root", so it cannot
mark one — and the inherited prefix normally extends *past* the root, because an
ambient root only commits when a later mention drains the channel queue and that
same turn commits the mention and the bot's answer too. A child asked "what did
the root say?" would then have three equally-committed candidates and answer
about the wrong one.

The distinction is host domain knowledge, so the host writes it down: at fork
time the bot seeds one labelled admission naming the root message
(`THREAD_ROOT_SEED_PREFIX` in `bot/threads.rs`), under a deterministic source
key, before the child's first turn runs. Every forking host with a similar
notion of an anchor message pays the same few lines — the price of hosts, not
the substrate, owning their own semantics.

The seed carries its own newlines on both sides, and that is not cosmetic:
queued text inputs concatenate into a single user message with no separator, so
a seed enqueued behind copied pre-root context would begin in the middle of that
line and read as its tail rather than as a label. The deterministic source key
makes re-seeding a no-op, resolved against the `(session_id, source_key)` row
Lash already holds; a host that vacuums live sessions tombstones that row and
would re-seed on a later redelivery, which this bot never does.

### Locating the fork boundary

The ledger records two different boundaries because they mean different things.
A folded top-level message records and retains the exact channel graph boundary
observed while its admission held the channel lock; the queued root is copied
into a child forked there. After a channel turn commits, the bot instead reads
`turn_input_applications`, finds the application for the root's `input_id`, groups
every application with the same typed `turn_id`, pins the committed leaf, and
records that later boundary. If a crash lands after the pin but before that
ledger write, thread-open uses the durable application to re-derive and repair
the missing boundary before it forks. Nothing parses an input, turn, message, or
node id.

Thread-open chooses only from evidence durably tied to the root:

| Durable root evidence | Fork boundary and context policy |
| --- | --- |
| Recorded `fork_node_id` | Fork at that retained turn boundary. |
| `input_id` with a committed application, but no `fork_node_id` | Re-derive the applied turn boundary, repair the ledger row, then fork there. |
| Folded root with a recorded admission boundary | Fork at that retained pre-root boundary and copy the pre-root top-level admissions that are not already in the child graph; the root itself arrives as the seed. |
| Accepted root with `input_id` and an admission boundary | Treat the durable enqueue as valid immediately, even if the process died before advancing the ledger to Folded. |
| Non-terminal root without an authoritative boundary yet | Poll from 250ms with exponential backoff capped at 8s, for at most 45s. |
| Terminal ignored root with no admission evidence | Fail immediately; this ledger state proves the bot will never route it. |
| No root row | Keep the bounded wait because delivery may be racing; record `thread_root_not_available` on exhaustion. |

There is no current-head fallback: that leaf may already contain post-root
channel turns. If the 45s budget expires, a mention gets an honest in-thread
explanation that the bot has not caught up and will follow up. Its ledger row
remains at the non-terminal FIG-1008 state. The bot continues under the remaining
75s of one 120s in-process deadline, so a root that commits after the foreground
wait can recover without a new mention or restart. The error notification has its
own metadata identity, preventing it from being mistaken for the eventual answer
or posted twice. Copied admissions remain queued and are folded by the thread's
first mention.

A no-row exhaustion is distinct from a known, still-processing root. Boot recovery
handles `thread_root_not_available` with one zero-budget probe and does not enqueue
another long poll, avoiding a repeated 45s serial stall on every boot. Top-level
unfinished rows are recovered before thread rows, so a root that was accepted
before a crash gets its admission boundary before its replies are re-driven.

Fork isolation is directional in both cases and is asserted against the real
store semantics:

- thread nodes and pending inputs never appear in the channel session;
- channel nodes and pending inputs added after the fork never appear in the
  thread session;
- the ancestry present at the retained boundary is shared, not copied.

The staged event ledger carries `thread_ts`, so recovery opens the same child
session, takes the same per-session lock, and applies the same accepted / reply
pending / terminal protocol. Locks are keyed by routed session id: events in one
thread serialize, while sibling threads and their channel can progress in
parallel. An idle lock entry is removed after its last holder or waiter finishes,
so old thread ids do not accumulate in memory. A deleted thread id is a permanent
single-use tombstone; attempting to re-engage it settles as
`thread_session_retired` rather than silently creating an unrelated conversation
under the same id.

On the platform side, clicking a parent opens the right-hand thread panel,
replies stay out of the main list, and the parent carries a reply count. Slack's
`reply_broadcast` is also accepted: it projects the one threaded message onto the
channel surface, while bot context routing remains on the thread session. Real
Slack additionally has notification preferences, thread subscriptions, broadcast
pointer subtypes, edits/deletes, private-conversation scope rules, and retention
policies; this example does not pretend those product concerns are session
semantics.

### Text the model sees

`compose` renders `alice: the deploy is stuck`. The bot's own mention token is
stripped, other `<@U…>` tokens resolve to `@display-name` from a `users.list`
cache, and the author is named. The model reasons about people, not about Slack
markup.

## Idempotence and dedupe

Slack's Events API is at-least-once: retries, redeliveries after a slow
acknowledgement, and a second event for the same message all happen. In a chat
channel, getting this wrong is visible to humans — the bot answers twice, or not
at all.

Three mechanisms, each doing one job:

**1. A staged ledger** (`bot/ledger.rs`). Keyed on `event_id`, recording a
*stage* rather than a boolean:

| Stage | Meaning | Terminal |
| --- | --- | --- |
| `accepted` | recorded, nothing done yet | no |
| `reply_pending` | turn committed, reply text on record, post owed | no |
| `folded` | absorbed as context (or an empty answer) | yes |
| `replied` | posted; `reply_ts` recorded | yes |
| `ignored` | deliberately not acted on, with a reason | yes |

A boolean cannot distinguish "already answered" (drop it) from "accepted and then
crashed" (resume it), and guessing wrong loses a reply or duplicates one.

The handled-event claim is one
`INSERT … ON CONFLICT(event_id) DO UPDATE … RETURNING`, followed by the additive
thread-route row in the same SQLite transaction, so two concurrent deliveries of
one event cannot both see "fresh". An advance is one compare-and-set
`UPDATE … WHERE stage = <expected>`, so a stale handler — a task from a previous
boot, or a redelivery racing the recovery pass — cannot regress `replied` back to
`reply_pending` and cause the duplicate this module exists to prevent. Neither
write depends on the caller holding the per-channel lock to be correct, because
the concurrency being guarded against is not the bot's to schedule.

`deliveries` is incremented on every claim, so the retry path leaves evidence.

**2. Lash's own admission idempotence**, via the `ts`-derived source keys above.
Even with the ledger deleted, redelivery does not duplicate the transcript.

**3. Reply metadata for the last gap.** Each reply is posted with Slack's
`metadata` field carrying its originating `event_id`. A recovering bot asks the
platform — `conversations.history` for a channel reply or
`conversations.replies` for a thread reply, with metadata included — whether its
reply already landed, rather than guessing. That closes the
crash-between-posting-and-recording window without needing an idempotency key
Slack does not have.

Two ignore rules are worth calling out, because both are real production bugs:

- **app-authored messages are inert.** The platform delivers `message` events for
  the bot's own posts, exactly as Slack does. Without the `bot_id` guard the bot
  answers itself forever.
- **the `message` twin of a mention is dropped.** Slack delivers *both* a
  `message` and an `app_mention` for a message that mentions your app, under two
  different `event_id`s — so deduplication cannot help. The bot keeps the event
  whose meaning is unambiguous.

## Durability, and the upgrade path

What the bot uses today:

- **SQLite session stores** (`SqliteSessionStoreFactory`). Committed transcripts
  and undrained queued input survive a restart. This is the load-bearing choice.
- **A durable event ledger** (its own SQLite database), recording both the text
  admitted to the session and the reply owed, so a new boot can replay either.
- **A durable, transactional outbox on the platform side** — the message and the
  events it implies commit together, so the retries the bot's design assumes
  actually happen, and no event is lost to a crash between two commits.
- **Per-boot session-execution leases** (`LeaseOwnerIdentity::opaque` with a fresh
  incarnation), so a new boot reclaims what a crashed boot held instead of
  deadlocking against its own ghost.
- **`InlineEffectHost`** — process-local effect journalling.

### What a crash costs, stage by stage

Every stage is resumable because every step is idempotent: the admission by its
Lash source key, the drain by its `drain_id`, and the post by the `event_id` its
`metadata` carries. `ChannelBot::recover` walks the unfinished rows at boot and
finishes each one:

| Crash point | Ledger stage | What recovery does |
| --- | --- | --- |
| Before any work | `accepted` | Re-admits the message; for a mention, runs the turn and posts. |
| **Mid-turn, inside the dead boot's lease TTL** | `accepted` | **Defers.** The dead turn claimed the input and the claim is fenced to a lease this boot cannot take yet. Retried until the lease lapses; never terminalized. |
| Mid-turn, after the dead boot's lease lapsed | `accepted` | Steals the stale claim and runs the turn (`ReplySource::Turn`). |
| After the turn committed, before the reply text was recorded | `accepted` | Reads the answer back out of the committed transcript and posts it (`ReplySource::Transcript`). |
| After the text was recorded, before the post | `reply_pending` | Posts the recorded text without asking the model again (`ReplySource::Ledger`). |
| After the post, before recording it | `reply_pending` | Finds its own reply by the `event_id` in the reply's `metadata` and records it. **No second post.** |

#### An empty drain names why it ran no turn

`queued_turn().run()` answers with `QueuedTurnDrain::Ran(output)` or
`QueuedTurnDrain::Empty(reason)`, and acting on that reason is the single most
important thing to get right in a host that recovers queued work. The two
outcomes it separates are opposite:

- **The queue held nothing for this drain.** The durable queue holds no pending
  work for this lane: a committed turn already consumed the input, so the answer
  is in the transcript, or provably nowhere. Terminal. Only
  `EmptyQueuedDrainReason::ClaimRefused(QueuedWorkClaimRefusal::Empty)` proves
  it.
- **This drain never reached the input.** Every other reason. The lane was busy —
  a boot that restarts inside the previous boot's lease TTL gets
  `ExecutionLaneBusy`, because
  `try_claim_session_execution_lease_with_token` returns `Busy` for a live lease
  held by a *different* incarnation — or the work exists but is not claimable
  yet (`NotYetAvailable`, a row whose `available_at_ms` has not arrived), or the
  head was withheld, another writer won the row, or the host policy admitted
  none. Nothing was consumed, so the work is **retryable**, and the bot re-polls
  on its own cadence: no reason carries a timestamp.

`settle_empty_drain` matches the reason **exhaustively**, with no catch-all arm:
a refusal variant added later is a new decision for this bot to make, and the
compiler makes it say so rather than defaulting it into the terminal branch.

Getting this wrong is not hypothetical — it was FIG-1008, found by the judged
runbook. Reading an ambiguous `None` as "committed" terminalized the row as
`ignored` / `reply_lost_after_commit`, and because a terminal row is never
revisited by a redelivery or a later boot, the mention was **permanently**
unanswered. FIG-1575 removed the ambiguity at the source: the reason the claim
state machine already computed now reaches the host instead of being logged and
dropped.

#### Why the deferral has to wait, and for how long

The lease generation is what fences the stale claim, and it only moves when the
old lease **lapses**: `acquire_session_execution_lease_conn` sets
`fencing_token = previous + 1`, and it is reached only after the previous lease
has expired. A new boot therefore cannot shortcut the wait by acquiring the lease
first — while the dead lease is live it gets `Busy`, and lash exposes no
host-supplied liveness assertion that would let a bot declare its own previous
incarnation dead. So the wait is bounded by the session-execution lease TTL
(`LeaseTimings`, **30s** by default, host-configurable) and nothing shorter will do.

`ChannelBot::retry_deferred` therefore re-attempts both lease-fenced admissions and
recoverable thread-root races on an interval with a finite deadline. Each attempt
is a real, idempotent state test. A foreground thread-open may spend 45s; its
background continuation gets the remaining 75s, and every root wait inside that
loop is capped by the remaining time rather than receiving a fresh budget. Boot
does not block on root waits: the immediate pass gives thread routes one quick
probe, registration proceeds, and roots known to be processing are retried on a
background task. If the deadline is exhausted the row is *still* left resumable —
a later boot picking it up beats a silently dropped mention. A transient failure
inside an attempt counts as a retryable iteration, not an abort: the ledger row is
untouched by a failed attempt, so only the deadline ends the loop.

**The residual gap** is now narrow and specific:

> If the queue is provably exhausted for this drain — the claim was refused as
> `Empty` — and neither the ledger nor the committed transcript holds any
> assistant text, there is nothing to post and nothing to recover. The bot reports
> `Disposition::ReplyLost` and marks the event `ignored` with
> `reply_lost_after_commit` rather than silently dropping it. This is now the
> *only* route to `ReplyLost`.

### The Restate upgrade, precisely

Replacing `InlineEffectHost` with a Restate-backed effect host is **half** the
change, and it is worth being exact about which half:

- **`bot/runtime.rs::build_core` — the drain.** The queued drain becomes a
  journalled, replayable effect: after a restart, re-running with the same
  `drain_id` replays the recorded result instead of re-executing. This is what
  removes the "turn committed but its result is gone" case entirely, rather than
  recovering from it after the fact.
- **`bot/channel.rs::post_reply` — the post.** This is *not* covered by the
  builder swap. `chat_post_message` is a plain HTTP call outside any effect scope,
  so the effect host cannot see it or replay it. Closing the
  crash-between-post-and-record window durably means wrapping the post as a
  journaled effect inside the same scope as the drain, so the journal records
  "posted, ts=…" and a replay returns it instead of posting again.

Until the second half is done, the metadata lookup described above is what keeps
the post at-most-once — which is why it is a real mechanism here and not a
placeholder.

`examples/agent-workbench` has the full Restate harness (`restate.rs`,
`restate_ingress.rs`) and `runbooks/restate-postgres-workers` shows the
distributed-worker shape. Neither is duplicated here on purpose: this example's
subject is the *integration* shape, and a Restate deployment alongside it would
double the reader's setup cost.

## Modes: this is the standard-mode reference

The bot is built with `LashCore::standard_builder(TurnBudget::Unbounded)`. Turns are native tool-loop
turns: the model answers in prose, or calls a host tool and then answers.

Two native tools, both backed by real `conversations.*` calls, so the loop leaves
the runtime and comes back:

- `list_channels` — the workspace's channels, topics and member counts. Follows
  the pagination cursor to exhaustion.
- `channel_history` — recent messages from a channel by id or name, oldest first.

Neither can be answered from the session transcript, which is what makes them
worth having rather than decorative.

**Mode-exclusive features do not appear here, deliberately.** No Lashlang, no code
cells, no durable processes, no triggers or cron, no `continue_as`. Those are
RLM-mode capabilities; `examples/agent-workbench` is their reference host. If you
want to see what standard mode is for, read this example; if you want to see what
RLM mode is for, read that one.

## Slack fidelity

The real Slack API documentation was the guide, and the shapes below are copied
from Slack's own reference responses. A client written against real Slack works
against this platform unmodified, which is the point: the migration this example
advertises should fail *here*, not in production.

### Mirrored exactly

**Identity.** Prefixed opaque ids — `T…` team, `A…` app, `B…` bot, `U…` user,
`C…` channel, `Ev…` event. Message identity is `ts`, a string of epoch seconds
with a six-digit fraction (`"1503435956.000247"`), unique and strictly increasing
per channel.

**Transport.** `Authorization: Bearer <token>`. Arguments accepted as a query
string, as `application/x-www-form-urlencoded`, or as JSON, with object arguments
(`metadata`) JSON-encoded in form bodies. **Failures are HTTP 200 with
`{"ok": false, "error": "..."}`** — including `invalid_auth`, so a client that
only checks status codes fails here the way it would fail on Slack.

The platform accepts all three encodings, but the bot's client deliberately does
not use whichever is convenient. Real Slack accepts form-encoding for *every*
method and JSON for only some, so `bot/slack_api.rs` form-encodes everything and
posts JSON only for `chat.postMessage` (which needs it for the `metadata` object).
A client that JSON-posted `conversations.history` would work here and fail against
Slack — the worst possible failure mode for an example that advertises a
migration — so the encoding is asserted by a test, not assumed.

**Web API methods.**

| Method | Notes |
| --- | --- |
| `auth.test` | `ok, url, team, user, team_id, user_id, bot_id` |
| `chat.postMessage` | `channel`, `text`, `thread_ts`, `reply_broadcast`, `username`, `metadata`; response `{ok, channel, ts, message}` with `subtype: "bot_message"` + `bot_id` on app posts |
| `conversations.list` | full channel object; `base64("team:<C…>")` cursor |
| `conversations.history` | newest-first, top-level only, `has_more`, `pin_count`, `base64("next_ts:<micros>")` cursor, `include_all_metadata` |
| `conversations.replies` | parent first with `reply_count` / `reply_users_count` / `latest_reply`, then replies oldest-first with `parent_user_id` |
| `users.list` | full member object with `profile`, plus `cache_ts` |

Cursor encodings were verified against Slack's documented values:
`base64("team:C061FA5PB")` = `dGVhbTpDMDYxRkE1UEI=`,
`base64("next_ts:1512085861000543")` = `bmV4dF90czoxNTEyMDg1ODYxMDAwNTQz`.

**Events API.** The `event_callback` envelope (`token`, `team_id`, `api_app_id`,
`event`, `event_id`, `event_time`, `authorizations`); the `url_verification`
challenge handshake, gating registration; `message` events with `channel`, `user`
or `bot_id`, `text`, `ts`, `channel_type`, `event_ts`; `app_mention` events with
`<@U…>` mention syntax (the `<@U…|label>` form included); **both** a `message` and
an `app_mention` for a mention, under distinct `event_id`s; at-least-once delivery
with **three** retries carrying `x-slack-retry-num: 1|2|3` and
`x-slack-retry-reason`.

### Deliberately divergent

| Divergence | Why |
| --- | --- |
| Retry delays are ~1s/2s/4s, not Slack's immediate/1min/5min | Nobody watches an example for six minutes to see a retry. The retry *count* and headers are exact. |
| `x-slack-retry-reason` only ever `http_error`, `http_timeout` or `connection_failed` | Those are the only ways this platform can fail a delivery. The bot handles all six real values. |
| Envelope `token` is the verification mechanism; no `X-Slack-Signature` | Signing is in "what real Slack adds" below. The `token` field is real (deprecated) Slack. |
| `pin_count` is always `0`; `num_members` is the workspace size; `color`, `tz` are fixed | The platform has no pins, no per-channel membership and no timezones. The fields are part of the contract, so they are reported honestly rather than omitted. |
| `conversations.replies` pages only the replies; the parent is returned on the first page | Pagination the reader can follow, with the wire shape unchanged. |
| `reply_broadcast` projects the original reply into channel history instead of a separate `thread_broadcast` pointer event | The example has one durable message identity and no separate broadcast-event product model; both surfaces still agree on the same `ts`. |
| Cursor paging walks the ordered list rather than seeking an index | The whole workspace is smaller than one page here. |
| The `event_callback` envelope omits `event_context`, `is_ext_shared_channel` and `context_team_id`/`context_enterprise_id` | One workspace, no shared channels and no Enterprise Grid, so there is nothing truthful to put in them. A bot must not require them. |
| The delivery outbox is read, not leased: exactly one dispatcher per process | Two dispatchers would double-deliver — which the bot's ledger tolerates, since Slack redelivers anyway — but the retry counters and backoff would stop meaning anything. A multi-process platform needs a real claim lease. |

### Not implemented

No Block Kit (`blocks`, `attachments`), reactions, pins, files, edits or
deletions, private channels, DMs, group DMs, user groups, presence, or
rate limiting (`429` / `Retry-After`).

### What real Slack adds

- **OAuth 2.0 installation.** Bot tokens are minted per installation through an
  OAuth flow with granular scopes (`chat:write`, `channels:history`,
  `app_mentions:read`, `users:read`), stored per workspace, and revocable. Here
  there is one static token from configuration, and the platform's `/platform/*`
  endpoints have no authentication at all.
- **Request signing.** Real apps verify `X-Slack-Signature`, an HMAC-SHA256 of
  `v0:<X-Slack-Request-Timestamp>:<raw body>` under a signing secret, and reject
  timestamps older than five minutes. The deprecated shared `token` this example
  checks does not prevent replay.
- **Socket Mode.** A WebSocket alternative to a public request URL, for apps that
  cannot be reached from the internet. The bot here needs a URL the platform can
  POST to.
- **Rate limits.** Per-method tiers, `429` with `Retry-After`, and a ~1
  message/second/channel posting limit. A real bot must back off.
- **Scale realities.** Enterprise Grid, shared channels
  (`is_ext_shared_channel`), Discovery/DLP, message edits and deletions arriving
  as `message_changed` / `message_deleted` subtypes, and event payload
  truncation.

## Migrating this bot to real Slack

1. Point `SlackApi::new` at `https://slack.com` and pass a real `xoxb-` token.
   `bot/slack_api.rs` needs no other change: the method names, arguments, response
   types **and body encodings** already match what Slack accepts.
2. Replace the envelope-`token` check in `ChannelBot::ingest` with
   `X-Slack-Signature` verification.
3. Delete the `/platform/apps` self-registration call and paste the request URL
   into the Slack app configuration instead; the `url_verification` handler
   already answers Slack's challenge.
4. Add rate-limit handling: retry `429` after `Retry-After` in `SlackApi::call`.
5. Subscribe to `app_mention` and `message.channels`, and keep the `bot_id` and
   `superseded_by_app_mention` guards — Slack's double delivery is why they exist.

Everything else — session-per-channel, ambient ingress, the ledger, recovery — is
Slack-independent and unchanged.

## Layout

```
src/
  ids.rs            Slack-shaped ids and `ts` (message identity)
  wire.rs           shared contract: error envelope, cursor encoding
  wire/methods.rs     Web API arguments and responses
  wire/events.rs      Events API envelope and event bodies
  store.rs          async wrapper over one blocking SQLite connection
  secrets.rs        constant-time comparison for the shared tokens both sides check

  platform.rs       config, router, boot          ← no Lash dependency
  platform/db.rs      workspace schema and queries
  platform/apps.rs    app install and delivery outbox
  platform/args.rs    Slack's argument dialect; the `ok: false` envelope
  platform/web_api.rs Slack-compatible methods
  platform/dispatch.rs at-least-once delivery with retry headers
  platform/human_api.rs the product's own surface (`/platform/*`)
  platform/ui.rs, assets/index.html  the browser client

  bot.rs            config, boot, registration    ← the Lash embedding
  bot/runtime.rs      standard-mode LashCore, stores, prompt, session mapping
  bot/channel.rs      session per channel; ambient fold; mention turn; recovery
  bot/threads.rs      thread fork location, inheritance and child-session open
  bot/ledger.rs       staged idempotent-consumer record
  bot/slack_api.rs    the liftable client
  bot/tools.rs        native tools for the standard tool loop
  bot/webhook.rs      the Events API request URL

  bot/mcp_admin.rs  operator API: attach, detach, list, publish a root

  mcp_server.rs          rmcp stdio server and read-only workspace tools
  bin/mcp_server.rs      stdio server process entry point
  mcp_http_server.rs     rmcp streamable-HTTP server behind bearer auth
  bin/mcp_http_server.rs HTTP server process entry point

  tests/platform_wire.rs     wire shapes, asserted on raw JSON keys
  tests/bot_events.rs        dedupe, ambient fold, isolation, tool loop
  tests/restart_recovery.rs  restart, every recovery stage, retries, client encoding
  tests/support.rs           harness: real sockets, scripted provider
tests/
  mcp.rs            live MCP catalog, loop, death/recovery, collision
```

The two `/api/*` and `/platform/*` namespaces are kept visibly apart so a reader
can tell at a glance which routes are contract and which are scaffolding. The UI
reads its backlog from `/platform/history`, never from `conversations.history`:
that method needs the bot token, and a bot token has no business in a browser.

## Configuration

| Variable | Default | Used by |
| --- | --- | --- |
| `SLACK_CLONE_ADDR` | `127.0.0.1:3040` | platform |
| `SLACK_CLONE_DATA_DIR` | `.slack-clone/platform` | platform |
| `SLACK_CLONE_BOT_ADDR` | `127.0.0.1:3041` | bot |
| `SLACK_CLONE_BOT_DATA_DIR` | `.slack-clone/bot` | bot |
| `SLACK_CLONE_API_BASE_URL` | `http://127.0.0.1:3040` | bot |
| `SLACK_CLONE_BOT_PUBLIC_URL` | `http://<bot addr>/slack/events` | bot |
| `SLACK_CLONE_BOT_TOKEN` | `slack-clone-local-dev-token` | both |
| `SLACK_CLONE_VERIFICATION_TOKEN` | `slack-clone-dev-verification` | both |
| `SLACK_CLONE_ADMIN_TOKEN` | `slack-clone-dev-admin` | bot (MCP admin API) |
| `SLACK_CLONE_BOT_HANDLE` | `lashbot` | platform |
| `SLACK_CLONE_TEAM_NAME` | `Slack Clone` | platform |
| `SLACK_CLONE_RETRY_BACKOFF_MS` | `1000` | platform |
| `SLACK_CLONE_DELIVERY_TIMEOUT_MS` | `3000` | platform |
| `SLACK_CLONE_BOT_TRACE` | `<bot data dir>/lash/trace.jsonl` | bot |
| `SLACK_CLONE_MCP_SERVER` | sibling `slack-clone-mcp-server` binary | bot |
| `SLACK_CLONE_MCP_HTTP_ADDR` | `127.0.0.1:3042` | HTTP MCP server |
| `SLACK_CLONE_MCP_HTTP_TOKEN` | `slack-clone-mcp-http-dev-token` | HTTP MCP server |
| `OPENROUTER_API_KEY` | — | bot (required) |
| `OPENROUTER_MODEL` | `anthropic/claude-sonnet-4.6` | bot |

## Deferred

Tracked for follow-up rather than half-built:

- **DMs and group DMs.** `channel_type` is modelled; `im`/`mpim` conversations are
  not. A DM is a different session-mapping question again (per user, not per
  channel).
- **Socket Mode.** Only relevant once the bot runs somewhere Slack cannot reach.
- **Restate effect host, both halves.** The `build_core` swap removes the
  `ReplyLost` case; journalling `post_reply` as an effect is the separate second
  half that makes the post durably at-most-once instead of relying on the metadata
  lookup. See [the upgrade path](#durability-and-the-upgrade-path).
- **A leased delivery outbox**, for a platform running more than one process.
- **Shortening the recovery wait.** An interrupted mention is answered within one
  session-execution lease TTL (30s by default). A bot that wanted faster resumption
  would lower `LeaseTimings`, trading recovery latency against the risk of losing a
  live lease during a slow model call.
