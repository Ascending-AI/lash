# Workbench host-trigger evidence

Read [RULES.md](RULES.md) first. This shared procedure supports the five
trigger-era runbooks; each scenario supplies its own phases and scorecard.

## Current contract

[ADR 0137](../docs/adr/0137-the-host-owns-events-routing-and-scheduling.md)
puts sources and delivery in the host. [ADR 0101](../docs/adr/0101-one-session-ingress-carries-every-admitted-item.md)
puts every admitted input through one session ingress. [ADR 0139](../docs/adr/0139-the-lash-vm-is-a-dialect-free-kernel.md)
defines the dialect-free kernel; these host/UI rows serve TypeScript.

The host tool `workbench.register_trigger` (`workbench_register_trigger` in the
catalog) takes `source`, `definition`, `event_arg`, optional `args` and `name`.
A mail source is `{"kind":"mail"}`; a cron source is
`{"kind":"cron","expr":"0 8 * * *","tz":"UTC"}`. Mail events contain
`account`, `title`, and `text`; cron events contain `fired_at`. A registration
result contains `subscription_id`; the list record uses `id`, `owner_session`,
`name`, `source`, `event_arg`, `args`, and `created_at_ms`. Capture that `id`;
compare owner and source as well as display name.

**Mail is shared.** Every connected inbox delivery fans out to every pinned
mail subscription, across sessions. The request's `session_id` is an admission
fence for the injecting caller, not a recipient selector. An account filter
inside the definition can return without taking an effect, but that delivery
still starts a process and still produces its process-end notice.

The host stores subscriptions, occurrences and deliveries in
`<data-dir>/host-triggers.db`. A subscription goes from `pending` to `pinned`;
only pinned rows list and dispatch. Removal marks it `deleting`, releases its
artifact pin, then deletes it. Boot recovery finishes either transition.
Each delivery starts with host key `{occurrence}:{subscription}` and records
its process id. A restart between start and bind reuses that same process.
Five failed starts record a failure and stop retries. A cron tick also records
its tick progress atomically with the occurrence.

The host follows committed process lifecycle facts and sends text beginning
`Triggered process <pid> ended:` to the subscribing session with turn id
`process-end:<pid>`. Re-reading the cursor repeats the same input, not a new
notice. This is an ordinary text input: its committed row has `kind = user`
and typed `provenance.input_id` / `provenance.turn_id`; its sealed answer has
`kind = assistant_reply` and `provenance.is_turn_reply = true`. There is no
`notice` or `trigger` variant in `EntryProvenance`. Join the host delivery,
process id, notice input and admitted run; do not infer identity from prose.
During a foreground turn this input uses the engine's ingress. Inspect the
pending input and committed admission to establish when it was consumed;
never require a second foreground address or a client-created turn.

## Setup and surfaces

Source the fork's `env.sh`, load the keys from the repository environment
without printing them, and export the row's `OPENROUTER_MODEL` from
[judged-matrix.toml](judged-matrix.toml). Use a fresh data and run directory
under the run's artifact root, with `AGENT_WORKBENCH_OPEN=0`, and boot through
`bash scripts/agent-workbench-dev.sh up --port <port>`. This builds the judged
geometry through Kiln. Gate `/healthz` and save the startup dialect, backend,
and model evidence. Use explicit `/?session_id=<S>` pages; assert URL,
`#sessionId`, and `/api/state?session_id=<S>.settings.session_id` agree.

Navigate with `.view-link[data-view="accounts"]` and
`.view-link[data-view="triggers"]`; `[data-view-back]` returns to chat.
Create `Work` in Accounts and confirm slug `work` with `GET /api/accounts`.
Entering Accounts forces an asynchronous refresh that replaces its forms.
Wait for that refresh to render before filling the title and text; record the
actual POST response so an unsent form cannot count as mail delivery.
The compose form posts `{"title":"<unique marker>","text":"<body>"}` to
`POST /api/accounts/work/messages?session_id=<S>`. Save the inbox response
from `GET /api/accounts/work/inbox`. Each POST is a new occurrence: retrying
an HTTP POST is not an idempotency probe.

`GET /api/triggers?session_id=<S>` lists that session's subscriptions. The
Triggers page renders `.trigger-registration[data-subscription-id]`, its
name, source, event argument, and **cancel**. Click cancel twice to confirm,
or use `DELETE /api/triggers/<id>?session_id=<S>` and gate `accepted: true`.
Save both the API list and the rendered registration at every checkpoint.

A delivered process is host-originated, scoped `workbench-trigger:<S>`.
The session-scoped work drawer lists processes observed by that session;
a host delivery is not automatically such an observation. Prove delivery
ownership with a read-only query on the host tables, then await that exact id
with `GET /api/work/<pid>/await`. An unscoped `GET /api/work` is a runtime-wide,
age-filtered view and is only corroboration, never an isolation or absence gate.
Do not change process-deletion expectations while that audit ruling is pending.

The await endpoint transports `ToolCallOutput` with its trust envelope
([tool_output.rs](../crates/lash-sansio/src/tool_output.rs)). Require
`outcome.type = settled` and `outcome.output.outcome.status = success`.
For a JSON return tagged `"$lash_tool_value": "untrusted_json"`, compare the
`value` inside that payload with the expected title or fixed result marker.
Retain the complete raw await response as evidence.

Read host evidence without altering it, for example with Python's sqlite3 and
`file:<data-dir>/host-triggers.db?mode=ro`:

```sql
SELECT d.occurrence_id, d.subscription_id, s.owner_session,
       d.process_id, d.attempts, d.failure
FROM deliveries d JOIN subscriptions s ON s.id = d.subscription_id
ORDER BY d.occurrence_id, d.subscription_id;
```

For a post-delete silence probe save all occurrence ids and process bindings
before deletion. After deletion, deliver a unique marker and find its occurrence
in `occurrences.payload_json`; require zero deliveries for the deleted id.
Other pinned registrations must still receive it. A completed short composer
turn fences the probe; save the host rows and terminal evidence rather than
using elapsed time or the shrinking runtime-wide work list to prove absence.

## Projection and recording

At every settled checkpoint save the full scoped `/api/state`,
`/api/state.product_events.events`, and the NDJSON `/api/observations` replay
(the browser consumes this stream; bound any separate capture). Canonical
rows are `/api/state.transcript[]`, with `row_id`, `kind`, `provenance`,
`content`, `timestamp`, and `suppressed`. Compare unsuppressed rows in order
against the DOM's `data-transcript-row-id` and their rendered content. Keep
reasoning, code and nested `.code-tools > .tool` summaries with their parent
identities. Record suppressed rows separately; do not count them as visible.
An optimistic draft or `.ingress-receipt` cannot satisfy a committed-row gate.

For each process require one committed notice input for `process-end:<pid>`
and one sealed reply for the run admitting it. Record input and run identities
from typed provenance, pending inputs, and the trace; a repeated delivery trace
line alone does not imply a repeated committed input. Preserve prior composer
replies and the relative order of all canonical rows across replay and reload.
If the product cannot show a notice, queues it twice, or the DOM disagrees with
canonical state, capture the discrepancy and Abort under RULES.

Use an explicit timeout for every API or browser gate, and no blind sleeps.
Take each scenario's named screenshots with the relevant view visible. On
success or Abort run `bash scripts/agent-workbench-dev.sh down --port <port>`
with the same environment and confirm it stopped. Fill every scorecard cell:
PASS, FAIL, or NOT RUN with the prerequisite that aborted. Record model,
dialect, tier, store, usage/cost (or why cost is unavailable), source HEAD,
command receipts, and screenshot paths in the run receipt. Never rerun a
judged row to improve its verdict.
