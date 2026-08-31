# E2E Scenario: Slack-Clone Bot — Lash as a Guest in Someone Else's Product

> **Read [../RULES.md](../RULES.md) first** — especially the browser-surface, polling,
> **don't-blind-yourself-with-the-fault-you-inject**, named-checkpoint screenshot,
> **cross-check**, real-token, Abort/RCA, and teardown rules. This runbook adds only the
> slack-clone scenario, and extends the cross-check to **four** layers.

**Purpose.** Referee the **integration shape**, not a host's own UI. Every other browser
runbook drives an app that *owns* its surface: the browser talks to Lash, and Lash's
projection is the thing under test. This one is inverted, which is the shape most real
integrations have — somebody else's product already exists, it has its own users, its own
database and its own wire contract, and the agent is one more app in it reached only over
HTTP. `examples/slack-clone` is the repository's canonical downstream reference for that
shape. Its bot host opens no RLM session, so it is the repository's standard-mode example
and what this runbook gates is whether a Lash bot behaves correctly *as a guest*: it hears
everything, answers only when addressed, spends nothing while merely listening, answers
exactly once, and survives both processes dying.

The token-free, exact-answer subset is executable on every PR through
[`../slack-clone-deterministic/runbook.md`](../slack-clone-deterministic/runbook.md).
This document remains the real-token judged path; it is not replaced by the CI
companion.

**Why four layers.** The other runbooks reconcile three (DOM / durable state / logs) inside
one process. Here there are **two independent processes with two independent durable stores**,
and the interesting failures live precisely in the seam between them — a reply the platform
has and the bot does not think it sent, a context line the bot folded that the platform never
delivered, a turn that ran twice because a retry crossed a restart. A three-layer check
inside either process would pass through all of them. So:

| Layer | What it is | Where |
| --- | --- | --- |
| **1. Rendered platform UI** | what each human actually sees, per tab | `#stream .msg` rows, `.msg.is-bot` for app rows |
| **2. Platform durable truth** | the product's own database and its Slack-shaped API | `<data>/platform/workspace.db` (`messages`, `event_outbox`); `/api/conversations.history`, `/platform/history` |
| **3. Bot durable truth** | the Lash session and the bot's consumer record | `<data>/bot/lash/lash-sessions/durable-core.db` (`graph_nodes`, `pending_turn_inputs`); `<data>/bot/events.db` (`handled_events`) |
| **4. Traces** | what the bot decided, and what the runtime did | bot stdout `Disposition::…` lines; `<data>/bot/lash/trace.jsonl` |

Reconcile them **pairwise**. Any mismatch is a **FAIL**, even when each layer is internally
consistent: one rendered bot row over two committed assistant messages and two rendered rows
over one delivered event are different defects, and only the cross-check tells them apart.
When they disagree, record which layers agreed — that split is the diagnosis.

**What can go wrong here that nothing else catches.** The example reproduces three real Slack
behaviours on purpose, and each is a bot-side hazard rather than a platform bug:

- the platform delivers a `message` event for the bot's **own** posts (carrying `bot_id`), so
  without the app-authored guard the bot answers itself forever;
- a message that mentions the app arrives **twice**, as `message` *and* `app_mention`, under
  two different `event_id`s — so deduplication cannot help, and the bot must drop the twin
  whose meaning is ambiguous;
- delivery is genuinely at-least-once, with three retries carrying `x-slack-retry-num`.

Ambient traffic is admitted as **queued turn input with no turn**
(`session.enqueue(...).id(...)`), and only a mention drains it
(`session.queued_turn().drain_id(...)`), with the queued-work driver deliberately switched
off so nothing but a mention can make the bot speak. That is the property phase 2 exists to
prove, and it is invisible to any single layer: "no reply" is not evidence of "heard and
remembered", and "remembered" is not evidence of "cost nothing".

**Real tokens.** Turns go through OpenRouter, so prose, termination style, whether the model
reaches for a tool, and how long a turn stays in flight are all nondeterministic. No exact
wording is an answer key. Neither is a **recovery path**: which `ReplySource` a restart
resolves through depends on where the kill landed relative to the turn's commit, and all
three are correct. The answer key is **counts, typed dispositions, and typed provenance**.

## Scenario-specific golden rules

1. **Count rows and messages; never match id shapes or prose.** A rendered row is a
   `#stream .msg`; a bot row is `.msg.is-bot`. Correlate the bot's work to the session
   transcript through Lash's typed `MessageOrigin::TurnInput { turn_id, input_id }`
   provenance and through the reply's own Slack `metadata.event_payload.event_id` — never by
   parsing `m_turn_…`, `Ev…` or `C…` strings. The example itself is careful about this; a
   runbook that cheats undoes the lesson.
2. **Assert the typed disposition, not the absence of a reply.** The bot reports one of
   `Rejected { reason }` / `Duplicate { stage }` / `Ignored { reason }` / `Folded` /
   `Replied { source }` / `Deferred { reason }` / `RecoverableFailure { reason }` /
   `Silent { reason }` / `ReplyLost`. "Nothing appeared in the channel" is satisfied by a bot
   that crashed. Require the disposition *and* the durable evidence behind it. This is the full
   `Disposition` vocabulary from [`examples/slack-clone/src/bot/channel.rs`](../../examples/slack-clone/src/bot/channel.rs).
3. **Ambient must cost nothing.** For every ambient message: a `pending_turn_inputs` row
   exists in the bot's session store, no new `turn_completed` appears in `trace.jsonl`, and
   the usage total is unchanged. A bot that answers ambient traffic and a bot that silently
   drops it both fail this, in opposite directions.
4. **A mention is exactly one reply everywhere.** One `.msg.is-bot` row per tab, one bot row
   in `messages`, one `Replied`, one `handled_events` row at stage `replied` with a
   `reply_ts`. Two rows carrying the same text is the classic failure and is still two rows.
5. **Record which `ReplySource` fired; do not require one.** `Turn` (the model ran),
   `Ledger` (recorded text reposted), `Transcript` (answer read back out of the committed
   session graph). Phase 5 must report which one resolved it and why that is consistent with
   where the kill landed — a runbook that demands a specific source is testing timing, not
   correctness.
6. **Use the ledger stage to time the fault, not a sleep.** `handled_events.stage` names the
   exact window: `accepted` is "claimed, work not finished" (which spans the whole model
   turn), `reply_pending` is "answer known, post owed". Poll for the stage you want, then
   inject.
7. **Observe the fault window from inside the page.** Per [../RULES.md](../RULES.md), a
   driver sitting in a `kill`/restart command cannot poll. Install a `MutationObserver` in
   each tab that appends every added row to an array with a timestamp, and read it afterwards:
   that is what turns "no duplicate appeared" into evidence rather than an absence of
   observation. Launch the restart non-blocking and poll `/healthz`.
8. **Two humans means two browser contexts.** Identity is per-context (`localStorage`
   `slack-clone-name` plus `POST /platform/identify`). One context with two tabs is one
   human and does not exercise the fan-out.
9. **A thread is a fork, never a second root session.** Its deterministic id is
   `thread:<C…>:<thread_ts>`, but require a persisted fork relation whose source is the
   channel's retained boundary. A session with that id created independently is a **FAIL**.
10. **Thread traffic has one context route.** A normal thread reply renders only in the
    thread panel and is admitted only to the thread session. `reply_broadcast` may add a
    channel-surface projection, but it never admits the reply to the channel session.
11. **Gate branch isolation in both directions.** The thread inherits committed channel
    context through its fork boundary; channel traffic after that boundary is absent from
    the thread, and every thread admission/reply is absent from the channel transcript.
    Browser placement alone is not session evidence.
12. **Count thread replies separately from channel rows.** In each tab, gate the parent
    `.thread-badge`, `#threadStream .msg`, and `#stream .msg` independently. A correct
    non-broadcast reply increments the badge and thread count without adding a main-stream row.

## Working material

- Require `OPENROUTER_API_KEY` (environment or repo `.env`; the platform needs no key).
  Boot the processes on a dedicated port with all state outside the repo:
  `SLACK_CLONE_STATE_DIR=<scratch> SLACK_CLONE_OPEN=0 bash scripts/slack-clone-dev.sh up --port <p>`.
  The **bot port is `<p> + 1`** and the **HTTP MCP server's is `<p> + 2`**
  (unattached until phase 3M-A), and state lands under
  `<scratch>/<host>_<p>/{platform,bot}` with logs in `<scratch>/run/`. Gate both
  `GET /healthz` endpoints. Teardown on success or Abort:
  `bash scripts/slack-clone-dev.sh down --port <p>`.
- UI affordances: the name picker (`#namePicker` / `#nameInput`), the channel list
  (`#channels`), the current channel (`#channelName`, `#channelId`), the rendered mention
  token (`#botMention`), the composer (`#composer`, `#text`, `#send`), and the message stream
  (`#stream`), thread panel (`#threadPanel`, `#threadStream`, `#threadComposer`,
  `#threadText`, `#threadClose`) and parent count (`.thread-badge`). The client dedupes
  rendered rows by `message.ts`, so a duplicate **post** —
  which gets a fresh `ts` — renders as a second row and cannot hide.
- Platform HTTP truth: `GET /platform/bootstrap` (identity, channels, users),
  `POST /platform/identify`, `POST /platform/messages` (with `thread_ts` for a reply),
  `GET /platform/history?channel=<C…>`,
  `GET /platform/history?channel=<C…>&thread_ts=<root-ts>`,
  and the Slack-shaped `POST /api/conversations.history` (bearer bot token, form-encoded,
  `include_all_metadata=true` to read reply metadata).
- Bot HTTP truth: `GET /healthz` (resolved identity), `POST /slack/events` (the Events API
  request URL, used directly for the redelivery phase).
- Store queries. Platform:
  `SELECT ts, author_user_id, bot_id, subtype, text, metadata_json FROM messages WHERE channel_id = ? ORDER BY ts`
  and the outbox's `delivered_at` / `attempts` / `abandoned_at`. Bot ledger:
  `SELECT event_id, kind, stage, reply_ts, deliveries, input_text FROM handled_events`. Bot
  session graph: `graph_nodes` filtered to `session_id = 'channel:<C…>' AND tombstoned = 0`,
  reading `node_json` for `kind = 'event'` nodes whose `event.Conversation` names a role and,
  for admitted inputs, an `origin` of `{"kind":"turn_input", …}`.
- Trace truth: `trace.jsonl` records with `context.session_id = 'channel:<C…>'`; count
  `type = "turn_completed"` for executions and look for `tool_call_started` /
  `tool_call_completed` for the tool loop.
- **Normalize `ts` before comparing layers.** The platform *stores* `ts` as an integer of
  epoch **microseconds** (`messages.ts`, per that table's own comment) and *renders* Slack's
  `<secs>.<micros>` string on the wire; the bot's `handled_events.reply_ts` and the rendered
  row both carry the string form. Comparing the raw column against either of the others fails
  on encoding while the layers agree on the instant — a false FAIL that looks exactly like a
  correlation defect. Convert with `micros // 1_000_000` and `% 1_000_000` zero-padded to six
  digits, and compare in the wire form. The same applies to role and stage vocabularies: the
  session graph writes `User`/`Assistant`, the ledger writes lowercase stage names.
- The bot's channel session id is `channel:<C…>` and thread session id is
  `thread:<C…>:<thread_ts>` — **constructed** from platform identities, never parsed.

Save every named artifact, both tabs' screenshots, and all four layer extracts per phase.

## Phase 0 — Boot, identify two humans, and pin the empty baseline

Boot, gate both `/healthz`, and record the bot's `bot_user_id`, `bot_id` and `team_id`. Open
two browser contexts, name them (e.g. `ada` and `brix`), and require in **both**: the
rendered identity, `#general` selected with the same `#channelId`, the same `#botMention`
token as the bot's own `bot_user_id`, and an empty stream. `GET /platform/bootstrap` must list
both humans plus the bot. All four layers start empty for this channel: no `messages` rows, no
`handled_events` rows, no session graph, no `turn_completed`. Screenshot `00-both-tabs.png`.

## Phase 1 — The bot is present but silent

Before any traffic, require that merely booting produced no chat: zero `.msg` rows in both
tabs and zero rows in `messages` for the channel. A bot that greets the room on boot fails
this. Screenshot folded into `00-both-tabs.png` is sufficient.

## Phase 2 — Human A posts ambient facts: heard, remembered, free

As **A**, post two ambient lines through the composer, each carrying a unique literal marker
(e.g. `FIG999-AMBIENT-ONE-<run-id>`). Neither mentions the bot. Record the usage/trace
baseline first.

Gate, in order, and require **all** of:

- **Layer 1:** each line renders exactly once in **both** tabs (A's own and B's, via the live
  stream), and **no** `.msg.is-bot` row appears.
- **Layer 2:** two `messages` rows, both with `author_user_id` set and `bot_id` NULL; the
  `event_outbox` rows for them reach `delivered_at IS NOT NULL`.
- **Layer 3:** the bot's `handled_events` has one row per delivered event at stage `folded`,
  and the bot's session store holds a `pending_turn_inputs` row per ambient line — the context
  is durably queued, undrained.
- **Layer 4:** one `Folded` disposition per ambient event, and **zero** new `turn_completed`
  records for the channel session.

The twin rule already applies: an ambient `message` produces exactly one event, so
`handled_events` must not contain an `app_mention` row yet. Screenshot `02-ambient-both-tabs.png`.

**This phase fails if the bot replies, and equally if nothing reaches layer 3.** Silence with
an empty queue is a bot that is not listening.

## Phase 3 — Human B mentions the bot: one reply, folding the ambient context, through a tool

As **B**, post one message that mentions the bot (using the rendered `#botMention` token) and
that requires *both* the ambient context and a workspace lookup — e.g.
`<@U…> what did ada say about <marker>, and which channels exist in this workspace?`

Gate the mention's `app_mention` event through to a settled reply, then require:

- **Layer 1:** exactly **one** new `.msg.is-bot` row, identical in both tabs, and the
  user-row count increased by exactly one. Not two bot rows.
- **Layer 2:** exactly one new `messages` row with `bot_id` set and `subtype = 'bot_message'`,
  and its `metadata_json` carries the originating `event_id`. Confirm the same through
  `conversations.history` with `include_all_metadata=true` — the wire, not just the table.
- **Layer 3:** `handled_events` shows the `app_mention` row at `replied` with a `reply_ts`
  equal to the bot row's `ts`, **and** the `message` twin at `ignored` with reason
  `superseded_by_app_mention`. In the session graph, the drained turn's committed messages
  include the ambient markers, each carrying `MessageOrigin::TurnInput` — this is the proof
  the fold happened, and it must be read from provenance, not from the reply's prose.
- **Layer 4:** exactly **one** new `turn_completed`; `Replied { source: Turn }`; a
  `tool_call_started` / `tool_call_completed` pair for `list_channels` or `channel_history`;
  and a usage total that increased.

The reply's *wording* is not gated. That it names A's fact and a real channel is judged
behaviour; that exactly one turn ran and folded the queued input is the objective gate.
Screenshot `03-mention-both-tabs.png`.

## Phase 3M — MCP client depth: the host answers, over a real provider

This phase absorbs the former `slack-clone-mcp-client-depth` runbook. The four
client-depth features themselves — sampling, form elicitation, URL elicitation
completion, and workspace roots — are proven headlessly with exact scripted
oracles by [`slack-clone-deterministic`](../slack-clone-deterministic/runbook.md)
("MCP phases 0-3"), so re-running them as their own paid judged row bought a
second boot and no additional claim. Two things the scripted harness cannot say
survive here, because they need a real provider: that **sampling is served by
the model the bot configured**, and that the reply reflects the sampled input.

As **B**, post one message that mentions the bot and asks for all four features,
naming them:

> `<@U…> call mcp__slack_clone__sample_summary with "Host policy stays with the embedding application", then call mcp__slack_clone__elicit_confirmation, mcp__slack_clone__elicit_via_url, and mcp__slack_clone__list_host_roots. Report the summary, form action and answer, URL action and completion status, and root name.`

Poll until exactly one new bot reply renders, then require:

- **Layer 3:** the committed tool results carry the **sampled model id equal to
  the bot's own configured provider model**, plus
  `{ "action": "accept", "answer": "yes" }`, an accepted URL result with
  `completion_notified = true` and `elicitation_id = "slack-clone-demo-url-1"`,
  and a root named `slack-clone` with a `file://` URI.
- **Layer 4:** exactly **one** new `turn_completed`, and one successful
  `tool_call_started`/`tool_call_completed` pair for each of the four exact MCP
  tool names. **A `batch` envelope is permitted**: unwrap its per-entry `tool`
  field and count the four names inside it. Do not gate on four *top-level*
  tool records — a real model legitimately batches, and the former runbook's
  literal "exactly four tool records" gate failed a correct run on exactly that
  (battery Finding H). What stays prohibited is a second `turn_completed` or a
  nested direct-command envelope.
- **Judged:** the reply reports a summary that reflects the supplied input, the
  form answer, the URL completion, and the root name. Wording is not gated.

Require the bot process log to contain `MCP URL elicitation completed` with
`slack-clone-demo-url-1`. Screenshot `03M-mcp-depth-both-tabs.png` and save
`03M-session-tool-results.json` and `03M-url-completion.txt`.

### Phase 3M-A — an integration attached while the bot serves

`scripts/slack-clone-dev.sh up` also starts the HTTP-served MCP server on
platform port + 2. The bot does **not** wire it at boot: it is an integration an
operator attaches over the bot's own admin API, which is the point of this
sub-phase — nothing the model can call reaches that API, so growing the tool
catalog stays an operator act. The mechanics (a connected status row, the
attachment write, an emptied catalog after detach) are proven headlessly by
[`slack-clone-deterministic`](../slack-clone-deterministic/runbook.md); what
survives here is the model half.

Attach it, using the bot's operator credential as the admin bearer:

```bash
curl -sS -X POST "http://127.0.0.1:$((p + 1))/admin/mcp/servers" \
  -H "authorization: Bearer ${SLACK_CLONE_ADMIN_TOKEN:-slack-clone-dev-admin}" \
  -H 'content-type: application/json' \
  -d "{\"name\":\"workspace_http\",\"url\":\"http://127.0.0.1:$((p + 2))/mcp\",\"token\":\"${SLACK_CLONE_MCP_HTTP_TOKEN:-slack-clone-mcp-http-dev-token}\"}"
```

The response must report `"connected": true` with the five
`mcp__workspace_http__*` tools. As **B**, post one message mentioning the bot and
asking it for the workspace badge — **without naming the tool**. Then require:

- **Layer 3:** the committed tool result for `mcp__workspace_http__workspace_badge`
  carries an attachment whose `source.source` is `stored`, and the host's
  attachment store under `<bot data dir>/lash/attachments` holds a file of
  exactly that `byte_len`. The bytes are the server's, persisted by the host
  because this server was attached with binary-content attachments on.
- **Layer 4:** exactly **one** new `turn_completed` and a successful
  `tool_call_started`/`tool_call_completed` pair for that exact tool name.
- **Judged:** the model reached for a tool that did not exist when the session's
  first turn ran. That it found it is the claim; the wording is not gated.

Now detach it and ask again, in a new mention, whether the badge tool is still
available:

```bash
curl -sS -X DELETE "http://127.0.0.1:$((p + 1))/admin/mcp/servers/workspace_http" \
  -H "authorization: Bearer ${SLACK_CLONE_ADMIN_TOKEN:-slack-clone-dev-admin}"
```

- **Layer 3/4:** the new turn contains **no** `mcp__workspace_http__*` tool
  record, and `GET /admin/mcp/servers` lists no `workspace_http` row.
- **Judged:** the reply says the capability is gone rather than inventing a
  badge. A model that fabricates the badge after detach fails this phase.

Screenshot `03MA-attached-both-tabs.png` and `03MA-detached-both-tabs.png`; save
`03MA-attach-status.json`, `03MA-session-tool-results.json`, and
`03MA-after-detach-servers.json`.

## Phase 3T — Human B opens a thread: inherited context, one threaded reply, no channel leak

Use Human A's first ambient message from phase 2 as the thread root. In **B's** tab click that
message to open `#threadPanel`, then post a thread reply that mentions the bot and asks about
the root's unique ambient marker. Open the same parent in **A's** tab. Gate all four layers:

- **Layer 1:** both tabs show the same parent plus B's mention and exactly one bot reply in
  `#threadStream`; the parent's `.thread-badge` increments to two replies; neither tab gains
  a main `#stream .msg` row for either thread reply. The bot answer must refer to the unique
  pre-fork ambient fact, judged semantically rather than by exact prose.
- **Layer 1, root recall:** ask the thread mention *which message this thread started from*
  and require the answer to name the root's own marker and **not** the phase-3 room mention.
  Inheriting the prefix is not the same property: the child forks at the boundary of the turn
  that drained the room, so the root, the room mention and the bot's own reply are all in the
  prefix, and nothing in Lash says which of them the thread hangs from. That is host domain
  knowledge, and the host supplies it by seeding the root into the child at fork time
  (`THREAD_ROOT_SEED_PREFIX`, `examples/slack-clone/src/bot/threads.rs`). A child that answers
  with the room mention is the FIG-1403 defect, not a model wobble; a child whose prompt has
  no seed line fails this gate on layer 3 as well.
- **Layer 2:** `messages` holds B's mention and the bot reply with `thread_ts = <root-ts>`;
  `/api/conversations.replies` returns parent first and both replies; normal
  `/api/conversations.history` contains neither reply. The bot reply metadata carries the
  thread `app_mention` event id.
- **Layer 3:** a session named `thread:<C…>:<root-ts>` exists as a fork of the channel's
  retained boundary. Its **committed transcript** contains the pre-fork ambient marker, the
  host's thread-root seed line naming the root exactly once — **on a line of its own**, since
  queued text inputs concatenate with no separator and a label that starts mid-line labels the
  tail of the message copied ahead of it — and the thread mention. Read
  inheritance through `fork_lineage` — the ancestor chain from the recorded `fork_node_id` —
  never as rows in the child's own `graph_nodes`: `fork_at` adds a session head *without
  writing graph nodes*, so ancestor content never appears under the child's session id, and
  an inclusion gate written that way fails on a correct fork while the matching exclusion gate
  passes on a broken one. The `channel:<C…>` graph and pending-input table contain **no** message
  with the thread event's `input_id`, `MessageOrigin::TurnInput`, text marker, or turn id.
  Gate the transcript, not "the prompt": see the observability note below.
- **Layer 4:** the turn and `Replied { source: Turn }` are scoped to the thread session id;
  there is no channel-scoped turn for this mention.

Now post a fresh ambient marker in the channel **after** the fork, allow it to reach `Folded`,
and mention the bot a second time in the thread. The second thread turn must not carry that
post-fork channel marker. This is the opposite-direction isolation gate; without it the test
proves only that the channel cannot see the child.

**Gate it on what this host can show you.** The trace retains exact request JSON only up to
`MAX_PROVIDER_REQUEST_BODY_JSON_BYTES` (2 KiB, `crates/lash-core`); a bot prompt carrying
folded channel history is normally larger, so the record carries
`body_json_omitted_reason: "size_limit"` with `body_len` and `body_sha256` instead of the
text. A gate written as "the request body excludes the marker" therefore cannot fire on a
real row — you cannot read absence out of a hash — and a row that claims it did is either
reading a body under the cap or fabricating. Require, in this order:

- the **committed thread transcript** for the second turn excludes the post-fork marker, and
  the channel session still holds that marker as its own pending input or committed row;
- the request record is accounted for: `body_len` and `body_sha256` present, plus
  `body_json_omitted_reason: "size_limit"` whenever `body_len` exceeds the cap. A present
  `body_json` is usable evidence only when `body_len` is genuinely under 2 KiB, and then it
  must also exclude the marker;
- the exact request-content claim — that the assembled prompt for the child turn never
  contained the post-fork channel line — is owned by a deterministic law rather than by this
  row: `thread_and_channel_traffic_are_isolated_after_the_fork`
  (`examples/slack-clone/src/tests/bot_events.rs`), which reads the model requests directly
  in-process. Run it, and record the exact command, the **passed count**, and the exit code.

Do not raise the cap to make a gate fire: it is paid on every JSONL record and OTEL
attribute by every user of tracing.

Two counting traps, both of which turn correct behavior into a false failure:

- **`#threadStream` renders the parent first, then the replies.** Counting `.msg` rows there as
  replies overcounts by one, and if the root happens to be bot-authored the parent is counted as
  a bot *reply*. Exclude the first row before counting.
- **Pick the root by author identity, narrowed by its marker — never by marker text alone,
  and never as "the newest message".** The newest message by this point is the bot's own reply
  from an earlier phase, and threading on that tests nothing about a human root. But the
  marker alone is not an identity either: the phase-3 mention asks the bot to recall the
  ambient facts, so a real model quotes the root's marker straight back and "the row
  containing the marker" now matches two rows. Select the human-authored row, treat a second
  match as an error rather than taking the first, and click it by its `ts`.

Save `03T-thread-both-tabs.png` plus four extracts: thread DOM rows/badge; platform parent and
reply rows; channel and thread session graphs/pending inputs; trace records grouped by session.

## Phase 4 — Redelivery: the same event again changes nothing

Re-POST the **same** `app_mention` envelope to the bot's `/slack/events` with
`x-slack-retry-num: 1` and an `x-slack-retry-reason`, exactly as the platform's retry does.
Require: disposition `Duplicate` at stage `replied`; `handled_events.deliveries` incremented
(the retry left evidence) with the stage and `reply_ts` unchanged; **no** new `turn_completed`;
and the bot-row count still one in every layer and both tabs. Screenshot
`04-after-redelivery-both-tabs.png`.

## Phase 5 — Kill the bot mid-mention; recovery answers it (the crown)

Arm the in-page row recorders. As **A**, post a second mention. Poll the bot's
`handled_events` until that event's row is at stage `accepted` — the window that spans the
whole model turn — then **`kill -9` the bot process** (non-blocking; its pid is in
`<scratch>/run/bot-<host>_<p>.pid`).

While the bot is down, require from the page and the platform — never from inside the killing
shell — that no bot row exists for this mention and the platform is still serving. Screenshot
`05-bot-down-both-tabs.png`.

Restart the bot (`bash scripts/slack-clone-dev.sh up --port <p>` is idempotent and restarts
the missing process; launch it non-blocking) and poll the bot's `/healthz`. Boot recovery
walks the unfinished ledger rows.

**The answer is not immediate, and a deferral is not a failure.** A boot that restarts inside
the previous boot's session-execution lease TTL cannot take that lease, so the interrupted
turn's admission is still fenced and recovery reports
`Deferred { reason: "drain_did_not_reach_admission" }` while leaving the ledger row
**non-terminal**. A background retry then re-attempts on an interval until the lease lapses.
So gate on the *settled* outcome, and allow at least one lease TTL (30s by default) plus the
retry interval before declaring anything — a render gate of a few seconds fails a correct bot.
Do not gate on a tight wall-clock band: require the ordered sequence kill → bot down with no row
→ restart and `/healthz` → any `Deferred` retry(s) → one settled `Replied`. Under load, a correct
recovery took 66.6s from kill to answer; allow that lease/retry sequence and load-related delay.

**Read the final disposition from the settle line.** Three log shapes carry a disposition, and
a deferred event's outcome only appears in the third:
`handled <id>: …`, `recovered event <id> (<kind>, <stage>): …`, and
`settled deferred event <id>: …`. A parser that knows only the first two reports `Deferred`
for an event that was in fact answered, which reads as a failure of the very property this
phase exists to prove. Require:

- the **final** disposition for that event is `Replied` — **record which `source`** resolved it
  (`Turn` if the queued input was still undrained or was re-drained after the lease lapsed,
  `Transcript` if the pre-kill turn had committed, `Ledger` if the text had been recorded) and
  state why that is consistent with the kill point;
- record the **deferral evidence**: the `Deferred` disposition, how many retry attempts ran,
  and the kill-to-answer latency, so the deferral is shown to be bounded rather than lucky;
- **exactly one** bot row for this mention in both tabs, in `messages`, and in the in-page
  recorder's full history — no duplicate at any instant, which is the recorder's whole purpose;
- `handled_events` for it at `replied` with a `reply_ts` matching that row;
- the reply's `metadata` carries this event's id;
- the channel's total `turn_completed` count is consistent with the number of turns that
  actually ran (one if recovery re-ran it, unchanged if recovery read it back) — reconcile
  against the reported `source` rather than assuming.

`ReplyLost` here is a **FAIL**, and so is **any terminal stage** reached without a reply: the
ledger stages exist precisely so the accepted window is resumable, and a terminal row is one
no redelivery and no later boot ever revisits. `Deferred` on its own is not a failure —
`Deferred` that never settles is. Screenshot `05-recovered-both-tabs.png`.

**The `accepted` window has two halves — capture which one you hit.** Alongside the ledger row,
extract the mention's `pending_turn_inputs` row from the bot's session store and record `state`
*and* `claim_owner_incarnation_id`:

- killed **before** the drain claimed the input → the row is unclaimed and still pending, the
  restarted bot's drain finds it immediately, and the outcome is `Replied { source: Turn }`
  with no deferral;
- killed **after** the drain claimed it but **before** the turn committed → the row is still
  `deferred_next_turn` yet carries a `claim_id` owned by the **dead boot's** incarnation, and
  there are **no** graph nodes for that turn. This is the half that defers, and it is the
  common one, because the claim is taken at the start of the turn.

In the second half, a `queued_turn().run()` that returns nothing does **not** mean "a previous
process already answered this" — nothing was committed. The bot must discriminate on committed
evidence (a turn-input application record) rather than on the ambiguous empty drain:
`reply_lost_after_commit` is only an honest label when a turn provably consumed the admission.
Report the incarnation ids, the graph-node count for the turn, and the lease generation the
claim is pinned to; a stranded claim terminalized as `ReplyLost` is the FIG-1008 regression.

## Phase 5T — Kill the bot mid-thread mention; recover on the child session

Repeat phase 5 inside the phase-3T thread. Arm row recorders in both tabs, post a fresh thread
mention, correlate its `app_mention` by `thread_ts = <root-ts>`, poll its ledger row to
`accepted`, and kill the bot. Reuse every phase-5 lease/deferral gate, with these additional
thread requirements:

- every drain, lease diagnostic, turn-input application, trace, and final turn is scoped to
  `thread:<C…>:<root-ts>`; the channel session's head, graph, pending rows and turn count are
  unchanged by the thread mention and its recovery;
- the final reply appears exactly once in each open `#threadStream`, exactly once in
  `conversations.replies`, and nowhere in the main channel list/history;
- the parent badge increments exactly once; the in-page recorder sees no transient duplicate;
- the ledger retains the original `thread_ts` through `accepted`, any `Deferred`,
  `reply_pending`, and `replied`, and the posted reply metadata names the original event id.

Allow the same lease/retry sequence and load-related delay, and accept `Turn`, `Transcript`, or
`Ledger` only when the four-layer evidence matches the kill point. `ReplyLost`, a terminal row
without a reply, a channel-scoped recovery turn, or a reply in the channel surface is a **FAIL**. Save
`05T-thread-bot-down-both-tabs.png` and `05T-thread-recovered-both-tabs.png`.

## Phase 6 — Platform restart: the durable outbox converges

Prove the outbox survives its own process. Stop the **bot** so a delivery must fail, then as
**A** post one ambient line with a fresh marker. Poll the platform's `event_outbox` until that
event's row shows `attempts >= 1` and `delivered_at IS NULL` — a real undelivered, retrying
row. Now restart the **platform** (non-blocking; poll `/healthz`), and require the row is
still there, undelivered, after the restart: that is the durability claim.

Bring the bot back and require convergence: the row reaches `delivered_at IS NOT NULL`, the
bot records exactly one `folded` row for it, a `pending_turn_inputs` row appears, **no** bot
reply is produced, and no `turn_completed` is added. Screenshot `06-outbox-converged-both-tabs.png`.

## Phase 7 — Reload both tabs: identical row multisets

Record each tab's row multiset (bot-or-human class plus body text). Reload both contexts, then
**re-select the scenario's channel and re-gate the rendered `#channelId`**: a reload
re-identifies from `localStorage` and then selects `channels[0]`, which is not necessarily the
channel under test, so a driver that compares straight after reload compares against a
different room and reads as catastrophic row loss. Wait for backfill from `/platform/history`,
and require: each tab's post-reload multiset equals its
own pre-reload multiset; **and** the two tabs equal each other; and both equal the `messages`
table's ordered projection for the channel. The live stream and the history backfill are two
independent renderings of one conversation, and a row present in only one localizes the defect.
Screenshot `07-reloaded-both-tabs.png`.

**Normalize mentions before comparing rendered text to stored text.** The client renders
`<@U…>` as an `@display-name` chip, so a rendered body is deliberately not byte-equal to the
stored text; resolve the token through the `users` map from `/platform/bootstrap` and compare
the normalized forms. Comparing raw-to-rendered reads as a total row mismatch — and falling
back to counting a marker's occurrences is not a substitute, because a marker legitimately
appears three times: in the ambient line, in the mention that quotes it, and in the reply that
answers it. Exact normalized multiset equality is both stronger and correct.

**Once threads exist, scope the table projection to top-level rows.** The main `#stream`
deliberately renders no thread reply, so the `messages` rows this multiset is compared against
must be filtered to `thread_ts IS NULL`. Comparing the main list against every row in the
channel reads as missing rows equal to the thread traffic — a failure produced entirely by the
comparison, on a surface that is behaving correctly.

## Phase 8 — Teardown and score

Run `bash scripts/slack-clone-dev.sh down --port <p>` and confirm all three processes are
gone — platform, bot, and HTTP MCP server.

| Item | Objective gate | Verdict | Evidence |
|------|----------------|---------|----------|
| Boot/identity | both `/healthz`; two humans + bot in bootstrap; `#botMention` equals the bot's `bot_user_id`; all layers empty | | `00-both-tabs.png` |
| Silent on boot | zero `.msg` rows and zero `messages` rows before traffic | | `00-both-tabs.png` |
| Ambient heard | each line once in both tabs; outbox delivered; one `folded` per event | | `02-ambient-both-tabs.png` |
| Ambient remembered | a `pending_turn_inputs` row per ambient line in the bot's session store | | layer-3 extract |
| Ambient free | zero new `turn_completed`; usage unchanged | | layer-4 extract |
| One mention, one reply | one `.msg.is-bot` row per tab = one `messages` bot row = one `Replied` | | `03-mention-both-tabs.png` |
| Reply is attributable | reply `metadata` carries the `event_id`, on the wire and in the table | | layer-2 extract |
| Fold is provable | ambient markers committed in the drained turn with `TurnInput` provenance | | layer-3 extract |
| Twin dropped | the `message` twin `ignored` as `superseded_by_app_mention` | | layer-3 extract |
| Tool loop ran | `tool_call_started`/`completed` pair; exactly one `turn_completed` | | layer-4 extract |
| MCP client depth | four host-owned results committed; four exact tool names, `batch` envelope unwrapped; one `turn_completed` | | `03M-session-tool-results.json`, `03M-url-completion.txt` |
| MCP sampling is the host's | sampled model id equals the bot's configured provider model | | `03M-session-tool-results.json` |
| Integration attached at runtime | attach reports `connected` with five `mcp__workspace_http__*` tools; the model calls one it was not booted with | | `03MA-attach-status.json`, `03MA-attached-both-tabs.png` |
| Binary MCP content is stored | the committed result carries a `stored` attachment and the host attachment store holds the bytes | | `03MA-session-tool-results.json` |
| Detach removes the capability | no `mcp__workspace_http__*` record in the later turn; the operator view is empty; the reply admits the loss | | `03MA-after-detach-servers.json`, `03MA-detached-both-tabs.png` |
| Thread fork | deterministic child session with retained channel ancestry | | `03T-thread-both-tabs.png`, layer-3 extract |
| Thread inheritance | reply uses the pre-fork ambient fact; post-fork marker absent from the ancestor chain named by `fork_lineage` and from the committed thread transcript | | phase-3T transcript extract |
| Thread root recall | the child names the thread root, not the later room mention; the seeded root line appears exactly once in the child's committed transcript, starting its own line | | `03T-thread-both-tabs.png`, phase-3T transcript extract |
| Request body accounted for | `body_len` + `body_sha256` present; `body_json_omitted_reason` when over the 2 KiB cap | | phase-3T trace extract |
| Prompt-content isolation law | deterministic law green (the claim is unreadable live over the cap) | | exact test command, passed count, exit |
| Thread isolation | no thread rows in channel UI/history/session; no post-fork channel traffic in thread | | phase-3T four-layer extracts |
| Redelivery inert | `Duplicate` at `replied`; `deliveries` incremented; counts unchanged | | `04-after-redelivery-both-tabs.png` |
| Mid-turn kill recovered | `Replied` (source recorded); exactly one reply in all four layers and in the in-page history | | `05-bot-down-*.png`, `05-recovered-*.png` |
| Mid-thread kill recovered | child-scoped deferral settles once; badge increments once; channel remains untouched | | `05T-thread-*.png` |
| Outbox durable | an undelivered retrying row survives a platform restart, then converges once | | `06-outbox-converged-both-tabs.png` |
| Reload identity | each tab's multiset unchanged, equal to the other tab's and to `messages` | | `07-reloaded-both-tabs.png` |
| Four-layer cross-check | every phase reconciles UI / platform / bot / trace pairwise | | all per-phase extracts |

**Aggregate:** did a Lash bot living as a guest in someone else's product hear everything two
humans said, stay silent and free until addressed, answer a mention exactly once with the
room's context and a real tool call, ignore a redelivery, answer a mention it was killed in
the middle of, survive the platform restarting under it, and render one identical conversation
to both humans — with the platform's database, the bot's Lash stores, and the runtime trace
agreeing on every count?

---

_Stop triggers and the Abort/RCA + reporting protocol are in [../RULES.md](../RULES.md)._
