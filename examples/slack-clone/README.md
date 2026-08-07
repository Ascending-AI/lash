# slack-clone

A multiplayer, Slack-shaped chat platform, and a Lash bot living inside it as a
guest.

Every other example in this repository is a host that **owns** its UI — Lash is
the product, and the browser talks to Lash. This one is inverted, which is the
shape most real integrations have: somebody else's product already exists, it has
its own users and its own database, and your agent is one more app in it, reached
only over HTTP.

It is also the repository's **standard-mode reference host**. Turns here are plain
chat turns driven by the native tool loop (`LashCore::standard_builder()`).
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
 │  Events API outbox, at-least-once ───────┼───────►│  /slack/events         │
 └──────────────────────────────────────────┘        └────────────────────────┘
```

## Run it

```bash
export OPENROUTER_API_KEY=sk-...          # the bot needs a model; the platform does not
just slack-clone                          # platform on :3040, bot on :3041
```

Open <http://127.0.0.1:3040>, pick a display name, and open a second tab with a
different name — two tabs are two people. Type anything: the bot stays quiet but
is listening. Mention it (`<@U…>`, shown in the sidebar) and it answers with the
whole room's recent traffic already in context.

```bash
just slack-clone-status        # both processes, plus /healthz
just slack-clone-logs-follow   # tail both logs
just slack-clone-down
```

State lives under `.slack-clone/`. `cargo test -p slack-clone` needs no model key
— the suite drives a scripted provider.

## What this demonstrates

| Concern | Where |
| --- | --- |
| Session per durable conversation | `bot/runtime.rs::session_id`, `bot/channel.rs` |
| Ambient context as queued turn input, with no turn | `bot/channel.rs::ingest` |
| Mention-triggered turn that drains the queue | `bot/channel.rs::run_mention_turn` |
| Standard-mode native tool loop | `bot/tools.rs` |
| Idempotent event consumption | `bot/ledger.rs` |
| Restart recovery, stage by stage | `bot/channel.rs::recover` |
| Reading a lost reply back out of the transcript | `bot/channel.rs::reply_from_transcript` |
| Transactional outbox | `platform/state.rs::post_message` |
| A liftable API client | `bot/slack_api.rs` |
| One wire contract for both processes | `wire/methods.rs`, `wire/events.rs` |

## Session mapping doctrine

**One channel, one Lash session, forever.** The session id is
`channel:<C…>` (`bot::runtime::session_id`).

A channel is a durable, long-lived conversation with a stable id the platform
already guarantees is unique — the same shape a Lash session has. Keying on
anything shorter-lived throws the room's memory away:

- keyed per mention → the bot has amnesia between questions;
- keyed per user → the bot cannot see a conversation, only one side of it;
- keyed per thread → reasonable *in addition* (see [Deferred](#deferred)), but as
  the only key it loses everything said in the channel body;
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

Both ledger writes are single statements, and that is deliberate. A claim is one
`INSERT … ON CONFLICT(event_id) DO UPDATE … RETURNING`, so insert-or-bump is
atomic without a surrounding transaction: two concurrent deliveries of one event
cannot both see "fresh". An advance is one compare-and-set
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
platform — `conversations.history` with `include_all_metadata=true` — whether its
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
| Mid-turn | `accepted` | Same — the queued input was never drained, so the drain still has it. |
| After the turn committed, before the reply text was recorded | `accepted` | Reads the answer back out of the committed transcript and posts it (`ReplySource::Transcript`). |
| After the text was recorded, before the post | `reply_pending` | Posts the recorded text without asking the model again (`ReplySource::Ledger`). |
| After the post, before recording it | `reply_pending` | Finds its own reply by the `event_id` in the reply's `metadata` and records it. **No second post.** |

The third row is the interesting one. When a drain returns nothing because a
previous process already consumed the input, the answer is still in the session
transcript, and it can be found without guessing: the committed copy of the
admission carries Lash's typed `MessageOrigin::TurnInput { turn_id, input_id }`
provenance, so `reply_from_transcript` matches on `input_id`, takes that message's
`turn_id`, and reads the last assistant message before the next turn begins. No id
strings are parsed and no adjacency is assumed.

**The residual gap** is now narrow and specific:

> If a turn committed and its transcript contains no assistant message at all —
> the drain consumed the input and produced nothing — there is nothing to post and
> nothing to recover. The bot reports `Disposition::ReplyLost` and marks the event
> `ignored` with `reply_lost_after_commit` rather than silently dropping it.

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

The bot is built with `LashCore::standard_builder()`. Turns are native tool-loop
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
| `chat.postMessage` | `channel`, `text`, `thread_ts`, `username`, `metadata`; response `{ok, channel, ts, message}` with `subtype: "bot_message"` + `bot_id` on app posts |
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
  bot/ledger.rs       staged idempotent-consumer record
  bot/slack_api.rs    the liftable client
  bot/tools.rs        native tools for the standard tool loop
  bot/webhook.rs      the Events API request URL

  tests/platform_wire.rs     wire shapes, asserted on raw JSON keys
  tests/bot_events.rs        dedupe, ambient fold, isolation, tool loop
  tests/restart_recovery.rs  restart, every recovery stage, retries, client encoding
  tests/support.rs           harness: real sockets, scripted provider
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
| `SLACK_CLONE_BOT_HANDLE` | `lashbot` | platform |
| `SLACK_CLONE_TEAM_NAME` | `Slack Clone` | platform |
| `SLACK_CLONE_RETRY_BACKOFF_MS` | `1000` | platform |
| `SLACK_CLONE_DELIVERY_TIMEOUT_MS` | `3000` | platform |
| `SLACK_CLONE_BOT_TRACE` | `<bot data dir>/lash/trace.jsonl` | bot |
| `OPENROUTER_API_KEY` | — | bot (required) |
| `OPENROUTER_MODEL` | `anthropic/claude-sonnet-4.6` | bot |

## Deferred

Tracked for follow-up rather than half-built:

- **Threads.** The wire shape is complete — `thread_ts` on posts,
  `conversations.replies` with parent statistics, thread replies excluded from
  channel history — and the tests cover it, but the UI does not render threads and
  the bot does not reply in one. The open design question is the session mapping:
  a thread is plausibly its own session with the channel session as its parent.
- **DMs and group DMs.** `channel_type` is modelled; `im`/`mpim` conversations are
  not. A DM is a different session-mapping question again (per user, not per
  channel).
- **Socket Mode.** Only relevant once the bot runs somewhere Slack cannot reach.
- **Restate effect host, both halves.** The `build_core` swap removes the
  `ReplyLost` case; journalling `post_reply` as an effect is the separate second
  half that makes the post durably at-most-once instead of relying on the metadata
  lookup. See [the upgrade path](#durability-and-the-upgrade-path).
- **A leased delivery outbox**, for a platform running more than one process.
- **Judged runbook.** A scripted downstream-host walkthrough, per
  `docs/agents/way-of-working.md`.
