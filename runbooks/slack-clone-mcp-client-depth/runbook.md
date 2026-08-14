# E2E Scenario: Slack-Clone MCP Client Depth

> **Read [../RULES.md](../RULES.md) first** — especially the real-token,
> polling, rendered-surface, cross-check, Abort/RCA, and teardown rules.

**Purpose.** Prove that the standard-mode Slack-clone reference host handles
MCP server-to-client sampling, form elicitation, URL elicitation completion, and
workspace roots while one outer MCP tool attempt remains open. The bundled
server must ask the host for every result; neither the server nor Lash core may
invent them.

The same four client-depth tools run with exact scripted answers inside the
token-free full-host CI companion documented at
[`../slack-clone-deterministic/runbook.md`](../slack-clone-deterministic/runbook.md).
This runbook remains the separate real-provider semantic judgement path.

## Scenario-specific golden rules

1. **The host owns all policy.** Sampling uses the provider/model configured by
   the bot. Form elicitation returns the example host's policy-checked `yes`
   answer. URL elicitation is accepted only for the bundled URL and the host
   observes its completion id. Roots contain the host-supplied `slack-clone`
   workspace root.
2. **One outer turn, four outer tool attempts.** The user sends once, the bot
   replies once, and the trace has one completed turn with successful calls to
   `mcp__slack_clone__sample_summary`,
   `mcp__slack_clone__elicit_confirmation`,
   `mcp__slack_clone__elicit_via_url`, and
   `mcp__slack_clone__list_host_roots`.
3. **Nested requests are in-attempt I/O.** The sampling provider call and the
   elicitation exchange must not appear as nested Lash commands or extra turn
   completions. Their returned values must instead be present in the four
   committed tool results of the outer turn.
4. **No exact prose gate.** The final bot wording and sampled summary are model
   output. Judge whether they reflect the supplied input; gate identities,
   counts, typed actions, and roots objectively.

## Working material

- Require `OPENROUTER_API_KEY` from the environment or the repository's
  gitignored `.env`. Never print it.
- Pick an unused platform port in `3060..3098`; the bot uses `<p> + 1`. Never
  use ports 3056 or 3057.
- Boot with state outside the repository:
  `SLACK_CLONE_STATE_DIR=<scratch> SLACK_CLONE_OPEN=0 bash scripts/slack-clone-dev.sh up --port <p>`.
- Teardown on success or Abort:
  `bash scripts/slack-clone-dev.sh down --port <p>`.
- Bot trace: `<scratch>/<host>_<p>/bot/lash/trace.jsonl`. Session store:
  `<scratch>/<host>_<p>/bot/lash/lash-sessions/durable-core.db`.
- The deterministic fallback is the repository integration test:
  `cargo test -p slack-clone --test mcp bundled_server_exercises_sampling_both_elicitation_modes_and_roots_through_host_seams -- --exact`.
  It uses the same bot factory and bundled stdio server with a scripted provider;
  its exact expected sample is `Host-generated summary.`, elicitation result is
  `{ "action": "accept", "answer": "yes" }`, URL result has
  `completion_notified = true`, and root is named `slack-clone`. Use this
  fallback only when no real model key is available and record that the
  browser/judgement layer was not exercised.

## Phase 0 — Boot and pin the empty baseline

Record `git status --short`, the selected ports, and both process logs. Gate the
platform and bot `/healthz` endpoints. Open the platform, identify one human,
select `#general`, and require an empty stream plus the rendered bot mention.
Capture `00-mcp-depth-baseline.png`.

## Phase 1 — Ask for all four client features

Post one message through the composer, substituting the rendered bot mention:

> `<@U…> call mcp__slack_clone__sample_summary with "Host policy stays with the embedding application", then call mcp__slack_clone__elicit_confirmation, mcp__slack_clone__elicit_via_url, and mcp__slack_clone__list_host_roots. Report the summary, form action and answer, URL action and completion status, and root name.`

Poll until exactly one bot reply renders. Require that the reply semantically
reports a summary, an accepted/yes form confirmation, an accepted URL flow with
completion notified, and a workspace root. Require the bot process log to contain
`MCP URL elicitation completed` with `slack-clone-demo-url-1`. Capture
`01-mcp-depth-result.png` with the request and reply visible and save the matching
log line as `01-url-completion.txt`.

## Phase 2 — Reconcile UI, durable truth, and trace

Gate all three layers before judging the sampled prose:

- **Rendered UI:** one user row and one bot row; no duplicate reply.
- **Durable session:** the `channel:<C…>` graph contains one admitted user
  message, one committed assistant answer, and exactly four successful tool
  records. Extract the structured outputs and require the sampled model plus
  summary, `{ "action": "accept", "answer": "yes" }`, an accepted URL result
  with `completion_notified = true` and `elicitation_id =
  "slack-clone-demo-url-1"`, and a root with `name = "slack-clone"` and a
  `file://` URI.
- **Trace:** exactly one new `turn_completed`; one successful
  `tool_call_started`/`tool_call_completed` pair for each of the four exact MCP
  tool names; no additional turn completion or nested direct-command envelope.

Save extracts as `02-session-tool-results.json` and `02-trace-gates.txt`. Any
count or identity disagreement is a contract violation → Abort/RCA.

## Phase 3 — Teardown and score

Stop both processes and confirm both ports are closed.

| Item | Objective gate | Verdict | Evidence |
| --- | --- | --- | --- |
| One interaction | one user row, one bot row, one completed turn | | `01-mcp-depth-result.png`, trace |
| Sampling | host model and sampled text in committed tool result | | `02-session-tool-results.json` |
| Form elicitation | typed `accept` plus schema-valid host answer `yes` | | `02-session-tool-results.json` |
| URL elicitation | typed `accept`, completion result, and matching host log id | | `02-session-tool-results.json`, `01-url-completion.txt` |
| Roots | host root named `slack-clone` with `file://` URI | | `02-session-tool-results.json` |
| Atomicity | four outer tools, no nested command or extra turn | | `02-trace-gates.txt` |
| Teardown | platform and bot ports closed | | command log |

**Aggregate:** did one rendered Slack-clone interaction prove all four host-owned
MCP client features without adding a nested Lash durability boundary?

---

_Stop triggers and the Abort/RCA + reporting protocol are in
[../RULES.md](../RULES.md)._
