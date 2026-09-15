# E2E Scenario: Tic-Tac-Toe Full Game — agent-service Browser Run

> **Read [../RULES.md](../RULES.md) first** — especially "The browser surface (example
> apps)": tooling, gate discipline, screenshot evidence, real-token designation, and
> boot/teardown ownership. This runbook only adds the scenario-specific parts.

**Purpose.** Play one complete human-vs-agent tic-tac-toe game through the real
`examples/agent-service` web UI against a real model, from `New chat` to a terminal state
(win, loss, or draw). Proves the whole loop: a board click becomes a chat turn, the RLM
agent answers through the app-owned `board.play` tool, the app persists the authoritative
board, and the UI, the HTTP API, and the on-disk stores all agree at every ply.

**Why this matters.** The board is **app-owned state** — lash never sees it except through
the tools the app contributes. If the rendered board, the `/board` endpoint, and the
model's tool calls can drift apart, the whole "host owns the domain, lash runs the turn"
story breaks. This scenario is the agreement check, ply by ply.

**Real tokens.** This drives OpenRouter with the key from the environment / repo `.env`.
The model plays O however it likes — do not gate on which cell it picks or its prose.

## Scenario-specific golden rules

1. **The `/board` endpoint is the truth.** After every completed ply, `GET
   /api/chats/{chat_id}/board` is authoritative. The UI must match it exactly (cells,
   turn, status). UI ≠ endpoint is a contract violation → Abort/RCA.
2. **You play X only when it is X's turn.** Cells are disabled while the agent is
   thinking and on terminal boards. A click that lands while `turn != "X"` (or after the
   game ended) mutating anything is a finding.
3. **One legal O per reply, and a bounded recovery when there is none.** Each of your
   plies must produce exactly one agent O move (the system prompt mandates exactly one
   `board.play` call per O turn); two moves is a finding. Zero moves on a live board is a
   model failure the **host absorbs** rather than a wedge: it re-prompts the agent once
   with an explicit "call `board.play` now" nudge, and if that turn plays nothing either it
   forfeits the O move, hands the board back (`turn` returns to `"X"`, no O is invented)
   and writes one `system` transcript row saying so. So a ply may legitimately end with
   zero new O **and** that recovery visible — record it, do not fail it. A ply that ends
   with zero new O and *no* recovery — `turn` still `"O"`, `legal_moves` non-empty, every
   cell disabled — is a product finding → FAIL.
4. **Play to the end.** The game must reach a terminal state (`X won`, `O won`, or
   `draw`) — a run stopped mid-game scores nothing. Any terminal outcome passes; play
   naturally (win if the model lets you).

## Working material

- **Boot** (fresh data dir every run):
  ```bash
  AGENT_SERVICE_ADDR=127.0.0.1:<port> \
  AGENT_SERVICE_DATA_DIR=<fresh-tmp>/agent-service-e2e \
  OPENROUTER_API_KEY=... \
  cargo run -p agent-service --profile judged
  ```
  Readiness: the `agent-service listening on http://...` line, then `GET /api/settings` →
  200.
- **UI affordances** (discover selectors yourself; ids current at time of writing):
  `#newChat`, the chat list `#chats`, the 3×3 board `#board` of `button.cell` elements
  with `aria-label="cell 0"`…`"cell 8"`, the status line `#gameStatus` (`X to move` /
  `O to move` / terminal) inside the status block `#gameStatusBlock`, the hint line
  `#gameHint` directly under it, the transcript `#messages`, the composer `#text` +
  `#send`, `#resetBoard`.
- **`#gameHint` is a gateable affordance, not decoration.** It is the element that explains
  the current state in a full sentence, and it has exactly three forms: the terminal
  sentence plus `Reset the board to start another round.`, `Agent is thinking and may call
  board tools.` while a turn is in flight, and `Your turn: click any empty square.` /
  `Agent turn: waiting for O to play.` otherwise. It is the most legible witness for both
  the mid-turn lock of Phase 2 step 3 and the terminal of Phase 3.
- **`#resetBoard` is enabled on a terminal board** — it is gated on `busy || !activeChat`,
  not on the game being live — so "everything is disabled at the end" is true of the nine
  cells and not of the page. Do not gate on it being disabled, and do not click it: it
  discards the game whose terminal state is this row's evidence.
- **Three cell vocabularies exist and they do not match.** The transcript row Phase 2 gates
  on is built from the UI's own space-separated names — `top left`, `top middle`, `center`,
  `bottom right`. The `/board` endpoint's `index_map` and the `board.play` tool contract
  both use a hyphenated form — `0 top-left`, `1 top-middle`, `8 bottom-right`. Gate the
  transcript against the space-separated form or compare semantically; a literal
  `I played X in the top-right.` built from `index_map` never matches a correct render.
- **The zero-move forfeit is a `system` transcript row.** When the agent finishes two
  turns in a row without playing, the host inserts one message with role `system` —
  rendered in `#messages` like any other, with its own styling — whose payload carries the
  yielded board. Its role is neither `user` nor `assistant`, so it does not disturb the
  Phase 4 row counts; it is the witness that the recovery ran.
- **Backend truth**: `GET /api/chats`, `GET /api/chats/{id}/messages`,
  `GET /api/chats/{id}/board` → `{cells, turn, legal_moves, status, winner}`.
- **Disk** (under the data dir): `app.db` (chats/messages/boards),
  `lash-sessions/durable-core.db` (the shared durable Lash catalog), and
  `trace.jsonl` (turn/tool trace).

## Phase 0 — Boot and pre-flight

Boot as above. Gates: the listening line; `GET /api/settings` returns the configured
model. Open the app in the browser, gate on the composer rendering, screenshot
`00-fresh.png`. A missing `OPENROUTER_API_KEY` fails the boot — that is a harness gap →
Abort (per RULES.md), not something to stub.

## Phase 1 — The opening chat

The app **auto-creates a chat on first load** — do not click `New chat` on a fresh boot
or you will have two. Gates:

- `GET /api/chats` now lists exactly one chat; record its `id` — the list objects are keyed
  `id` (plus `title`, `created_at`, `model`), and that value is what the per-chat routes below
  spell `{chat_id}`.
- `GET /api/chats/{chat_id}/board` is the default board: nine `null` cells, `"turn":
  "X"`, `"status": "X to move"`, `legal_moves` = 0..8.
- `#gameStatus` renders `X to move` (compare the DOM `textContent`, or
  case-insensitively — the CSS uppercases the visible text to `X TO MOVE`).

Screenshot `01-new-chat.png`.

## Phase 2 — The game loop

Repeat until the board is terminal **or a stop trigger below fires**. The loop is bounded:
it never runs without an exit. For each ply:

1. **Pick** any cell that is a `legal_move` per the endpoint (play naturally).
2. **Click** it. Gate: the cell renders `X` immediately and the transcript gains the user
   row `I played X in the <cell name>.`.
3. **Wait for the agent's reply** — gate on the transcript gaining an assistant row (be
   generous: up to ~120s; real model). While it thinks, cells must be disabled (spot-check
   once during the run: a mid-turn click must not mutate the board — golden rule 2).
4. **Cross-check** `GET /api/chats/{chat_id}/board`: your X is at the clicked index;
   **exactly one** new O appeared (golden rule 3) and `turn` is back to `"X"` — unless
   **your click ended the game** (an X win, or the draw: X always fills the ninth cell),
   in which case the agent has no legal reply move, **zero** new O is correct, and
   `status`/`winner` must say so.
5. **Agree**: the nine rendered cell marks equal the endpoint's `cells` array exactly
   (golden rule 1).

Screenshot each ply as `10-ply-<n>.png` after step 4. The X-count/O-count on the board
must track your click count / reply count exactly (counting a forfeited O as a legitimate
zero under golden rule 3) — any other drift is an Abort.

**Stop triggers.** Check after every step 4:

- **Recovered zero-move ply** — zero new O, `turn` back to `"X"`, and the forfeit `system`
  row present. Not a stop: continue the loop from step 1. Record it in the scorecard's
  Zero-move recovery row with the ply screenshot and the `system` row as evidence.
- **Wedge** — zero new O, `turn` still `"O"`, `legal_moves` non-empty, no forfeit row, and
  a re-read ~30 s later identical. The board can never progress and golden rule 4 is
  unreachable, so waiting longer buys nothing. **Stop and score FAIL** (product liveness,
  not Abort: the harness is sound, the product wedged). Evidence: the `/board` body, a
  screenshot showing all nine cells disabled, and the ply's transcript rows.
- **Ply budget** — nine X clicks fill the board. If a tenth iteration begins without the
  endpoint reporting a terminal `status`, **stop and score FAIL** and treat the extra ply
  as UI/endpoint drift under golden rule 1.

Never click `#resetBoard` to escape a wedge: it discards the very state this row exists to
record.

## Phase 3 — Terminal state

Gates:

- `#gameStatus` shows the terminal text and `#gameStatusBlock` — the block that wraps the
  status line and the hint — gains the `done` class. Sample the class on
  `#gameStatusBlock`; it is not and never was on `#gameStatus` itself
  (`examples/agent-service/src/ui.rs`, `gameStatusBlockEl.classList.toggle('done', done)`).
  The UI label is a **human re-phrasing** of the endpoint's status (`You won` / `Agent won`
  / `Draw` vs `X won` / `O won` / `draw`) — map them semantically, they never string-match.
- **The `win` highlight is a win-only clause, with a draw branch.** On `X won` / `O won`,
  exactly the three winning cells carry the `win` class and no other cell does. On a
  `draw` there is no winning line: gate that **no** cell carries `win`, and score the
  highlight clause n/a rather than unobserved.
- `#gameHint` carries the fuller phrasing of the same outcome and is the easier witness to
  read: `You won this round.` / `Agent won this round.` / `The round ended in a draw.`,
  each followed by ` Reset the board to start another round.` Gate it alongside
  `#gameStatus`; the two must name the same outcome.
- The endpoint agrees: `status` ∈ {`X won`, `O won`, `draw`}, `winner` matches, and on a
  win `legal_moves` is `[]`. Ignore `turn` once terminal — a game-ending X click leaves a
  residual `"turn": "O"` that no one will ever play.
- All cells render disabled — the game is over; clicking any cell mutates nothing.
- The final assistant message states the outcome (won / draw / your turn — judged, not
  string-matched).

Screenshot `20-terminal.png` (when your own click ended the game this duplicates the last
ply screenshot — expected, keep both names for the scorecard).

## Phase 4 — Backend and disk evidence

- `GET /api/chats/{chat_id}/messages`: the transcript is **semantically streamed** — each
  agent turn stores several rows (assistant reasoning/code segments, `tool` rows, a
  final assistant prose row), and a zero-move recovery adds one `system` row. Gates: one
  `user` row per click, in order (the host's nudge is turn input, never a `user` row);
  every user row is followed by at least one `assistant` row; **exactly one `play_move`
  tool row per agent O move** — count only the move rows, not all `tool` rows: the model may legitimately
  call `board.read` too, which also persists a `tool` row.
- `trace.jsonl` records the `board.play` executions. Do **not** substring-count the whole
  file (`board.play` also appears in prompts and streamed model output) — the
  authoritative per-execution marker is the `protocol_step` record
  (`plugin_id: "rlm_protocol"`, an `RlmTrajectoryEntry` payload containing
  `board.play(...)`); count those.
- `lash-sessions/durable-core.db` holds the shared durable session catalog; the chat's
  rows are keyed by its session id rather than stored in a per-session database file.

## Phase 5 — Teardown and score

Stop the app with Ctrl-C or SIGTERM. The example stops accepting connections, lets
in-flight requests finish, then closes its provider and flushes its trace sink.

**Signal the binary, not the wrapper.** The boot command is `cargo run`, so the process
that holds the listener and installs the drain handler is the `agent-service` binary cargo
spawned as a child, not cargo itself; the drain is armed on the child's own Ctrl-C and
SIGTERM. Ctrl-C in the launching terminal reaches the whole foreground process group and is
fine. A `kill` aimed at the pid you backgrounded is not: it stops the wrapper and leaves the
real listener serving the port, so the next row's port check finds it bound and the drain
never ran. Take the exact PID from the socket itself — `ss -ltnp` on this row's port — and
confirm it is `agent-service` before signalling. Then require the port free and
`GET /api/settings` refused before scoring teardown. Never `pkill`, and never match on a
name.

Then fill:

| Item | Objective gate | Verdict | Evidence |
|------|----------------|---------|----------|
| Boot + fresh board | listening line; default board via endpoint | | `00-fresh.png`, `01-new-chat.png` |
| Ply agreement (every ply) | UI cells == `/board` cells; one O per reply | | `10-ply-*.png` |
| Mid-turn input locked | disabled cells while agent thinks; no mutation | | ply screenshot + endpoint |
| Terminal state | `#gameStatus` text + `#gameStatusBlock` `done` + endpoint `status`/`winner` agree; `win` highlight on a win, absent on a draw | | `20-terminal.png` |
| Transcript integrity | messages API rows match click/reply counts | | API output |
| Tool-call evidence | `trace.jsonl` `board.play` count == O count | | trace excerpt |
| Zero-move recovery | n/a if no ply played zero O; otherwise `turn` back to `"X"` + one forfeit `system` row, and no wedge | | ply screenshot + `system` row |

**Aggregate:** did one full game run to a terminal state with the UI, the board endpoint,
the transcript, and the trace in exact agreement at every ply.

---

_Stop triggers and the Abort/RCA + reporting protocol are in [../RULES.md](../RULES.md)._
