# E2E Scenario: Agent Service — Pin, Fork, and Diverge

> **Read [../RULES.md](../RULES.md) first** — especially the browser-surface,
> screenshot, polling, real-token, Abort/RCA, and teardown rules. This runbook adds only
> the user-visible branching scenario.

**Purpose.** Prove that a user can retain a completed turn, advance the source chat, fork
the retained turn into a new chat, and continue both siblings independently. The browser,
host-owned chat database, and Lash durable store must agree at every checkpoint.

**Real tokens.** Turns use OpenRouter. Judge exact app state and structural transcript
changes, not model prose.

## Scenario-specific golden rules

1. **Pin before advancing.** The browser must render `Pinned … messages as a retained
   turn.` before another source turn starts. A past unpinned turn is ordinarily gone and
   `ForkPointNotRetained` is a valid API outcome, not permission to choose another point.
2. **The pin spans both stores.** `POST /api/chats/{source}/branch-points` pins the Lash
   continuation and records the app-owned message cutoff plus board snapshot. The
   corresponding `GET` response is the backend truth for the visible selector.
3. **Fork means a new session.** The source chat id must remain unchanged. The fork must
   receive a different chat/session id and restore exactly the pinned product projection,
   even though the source advanced afterward.
4. **Siblings diverge without cross-talk.** A turn on either sibling may change only that
   sibling's transcript and board. Reopening the other chat must reproduce its own state.
5. **Durability is not DOM memory.** Replace the service process once both branches have
   diverged and require both chats to reconstruct from the same data directory.

## Working material

- Require `OPENROUTER_API_KEY`.
- Use a fresh `<data-dir>` and free `<port>`, then boot with:
  `AGENT_SERVICE_DATA_DIR=<data-dir> AGENT_SERVICE_ADDR=127.0.0.1:<port> cargo run -p agent-service`.
  Gate on the listening line and `GET /api/settings` → 200.
- Restart with the same command, port, and data directory after terminating only the
  service process. Teardown the process at the end.
- Browser affordances: board cells, transcript, chat list, **Pin current turn**, pinned
  turn selector, and **Fork from pin**.
- Backend truth:
  `GET /api/chats`,
  `GET /api/chats/{id}/messages`,
  `GET /api/chats/{id}/board`, and
  `GET /api/chats/{id}/branch-points`.
- Durable truth: `<data-dir>/app.db` and
  `<data-dir>/lash-sessions/durable-core.db`. Save query results as JSON artifacts; do
  not count a terminal printout alone as evidence.

## Phase 0 — Boot

Open the browser, wait for the composer, board, and branch controls, and record the active
source chat id from `GET /api/chats`. Require the rendered active chat to match that row.
Save `00-ready.png`.

## Phase 1 — Complete and pin one turn

Click one empty board cell. Poll until the response stream closes, the board returns to
`X to move`, and both the rendered transcript and messages API contain the completed user
turn plus its terminal assistant row.

Click **Pin current turn**. Poll for the rendered `Pinned … messages as a retained turn.`
status. Save:

- the exact messages response as `01-pinned-messages.json`;
- the exact board response as `01-pinned-board.json`;
- the branch-point response as `01-pinned-point.json`.

Require exactly one visible selector option whose message count equals the saved message
array length. Query `node_anchors` by its node id and require one row. Save the fully
scrolled UI as `01-pinned-source.png`.

The product database publishes a branch only after Lash creates its durable head.
Until that second write finishes, the copied product projection remains pending and
is omitted from chat reads. On restart, the service rolls back every pending
projection and its possibly-created Lash session before admitting traffic.

## Phase 2 — Advance only the source

Click a second legal source-board cell different from the first. Poll until the turn
settles. Require the source messages count to be greater than the saved pinned count and
the source board response to differ from `01-pinned-board.json`. The branch-point API
must still return the same node id and pinned message count. Save
`02-advanced-source-state.json` and `02-advanced-source.png`.

## Phase 3 — Fork the retained turn

Select the saved point and click **Fork from pin**. Poll for the rendered
`Branched from … at a pinned turn.` status and a newly active chat whose title ends in
`· branch`.

Require:

- `GET /api/chats` contains distinct source and fork ids;
- the fork messages response exactly matches `01-pinned-messages.json` after normalizing
  database-assigned message ids and `chat_id`;
- the fork board response exactly matches `01-pinned-board.json`;
- the source messages and board still equal `02-advanced-source-state.json`;
- `session_head` contains both session ids, while the fork's active ancestry includes the
  pinned node id.

Save the API/store extracts and the fully scrolled branch UI as
`03-fork-restored.json` and `03-fork-restored.png`.

## Phase 4 — Prove sibling independence

On the fork, click a legal cell that produces a board different from the advanced source.
Poll until settled and save the fork messages/board. Switch to the source chat in the
browser and require its rendered transcript and board to match the unchanged source APIs.
Switch back to the fork and require its own rendered state to match its APIs.

Query the durable graph and require two distinct live heads whose ancestry converges at
the pinned node. Save `04-sibling-heads.json`, `04-source.png`, and `04-fork.png`.

## Phase 5 — Cold reconstruction

Terminate the service, restart it with the same command and data directory, and poll the
settings endpoint. Reload the browser. Visit each sibling and require its rendered
transcript and board to match the Phase 4 API artifacts exactly. Save
`05-reconstructed-source.png` and `05-reconstructed-fork.png`.

## Phase 6 — Teardown and score

Stop the service and confirm the port is closed.

| Item | Objective gate | Verdict | Evidence |
|------|----------------|---------|----------|
| Browser surface | composer, board, pin, selector, and fork affordances render | | `00-ready.png` |
| Retained turn | UI status, branch-point API, app snapshot, and one anchor row agree | | `01-pinned-*` |
| Source advance | source changes while the retained point stays fixed | | `02-advanced-source*` |
| Zero-copy fork story | new id restores the pinned app projection and shares the durable prefix | | `03-fork-restored.*` |
| Sibling independence | each UI agrees with its API; two live heads converge only in shared ancestry | | `04-*` |
| Cold durability | both divergent chats reconstruct after process replacement | | `05-*` |

**Aggregate:** did the browser make pin → advance → fork → independent continuation
understandable, while the API and durable stores proved that the new chat shares history
without moving or corrupting the source?

---

_Stop triggers and the Abort/RCA + reporting protocol are in [../RULES.md](../RULES.md)._
