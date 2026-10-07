# E2E Scenario: Relay Execution Policy in the Workbench

> **Read [../RULES.md](../RULES.md) first.** This runbook adds only the relay scenario.

**Automated check.** The model-free laws are
`kiln test //crates/lash:lash__unit_test --test_arg=tests::relay::` (commit,
rollback, budget, receipts, the plain-text answer, the last reply, text beside
a call, tag escaping, between-turns input, cancel, drain-resume, the stable
prefix),
`kiln test //crates/lash-protocol-rlm:lash-protocol-rlm__unit_test --test_arg=relay`
(baton validation, the turn-input view, relay on the native channel only) and
`kiln test //crates/lash-sim:lash-sim__unit_test --test_arg=cache_regression::`
(the cache markers on both Messages-dialect wires). Run those first; this
runbook covers what they cannot: a real model working under relay.

**Purpose.** Referee an RLM session running the relay execution policy. Relay
runs on the native tool channel. A work step is one `execute_code` call whose
program ends with `await control.next({ context, vars })`. A reply with no
tool call is the answer to the user and ends the turn. Each model call sees
exactly the context its previous step committed, plus one step message, and
nothing else. The scenario proves that steps commit their batons, that a
failed step commits nothing, that a plain-text reply ends the turn as its
answer, and that the next turn starts from the last committed context.

**Real tokens.** This scenario uses OpenRouter with the workbench's default
model, `z-ai/glm-5.3-flash`. Model prose and step counts are
nondeterministic. Gate on the provider requests in the trace, never on prose.

## Answer key: state this before observing

- Every relay request carries one tool, `execute_code`, and exactly two
  messages, both user role:
  - the context message: the constant block `Your context: notes you wrote in
    earlier steps. Only you write here.`, then one block per entry of the last
    committed `next`, each prefixed with its index (`[0] …`, `[1] …`);
  - the step message, which starts `<step n="N">` and holds, in order:
    `<user_request>` (this turn's user message, verbatim), `<last_reply>`
    (only on a turn's first step, and only after an earlier turn answered),
    `<last_step status="committed">` or `status="not committed: <reason>"`
    with `<code>`, `<output>`, `<calls>`, `<error>` and `<receipts>` as they
    apply, `<memory>`, any `<note>`, and `<your_move>`.
- Cache breakpoints: the last context block, and, when the context kept a
  prefix of the previous commit's entries, that prefix's last block. With an
  empty context, the header carries the end-of-context marker. The step
  message carries none. The provider adds the system prompt's and the tool's.
- The system-prompt hash is the same on every request of a session until its
  tools change (connecting an account adds tools).
- `<user_request>` holds only the user's words. A host note on the user
  channel (the workbench's `Context budget: prepared …`) is a `<note>`.
- A step after a committed one has a context equal to the `context` argument
  of the previous step's `next` call, entry for entry.
- A step after a failed one says `not committed` and shows the same context
  as the step before it. Calls that ran in the failed step are listed under
  `<receipts>` and are not repeated.
- The turn ends on a reply with no tool call. The assistant reply row in
  `/api/state.transcript` is exactly that reply's text. Text a reply puts
  beside an `execute_code` call never reaches the user or the transcript.
- The next turn's first request has the context the last `next` committed,
  and its step message carries the previous answer in `<last_reply>`.

Any mismatch is a **finding and FAIL**.

## Phase 0: launch

In a Kiln fork, with `env.sh` sourced and an OpenRouter key exported:

```sh
export AGENT_WORKBENCH_RLM_POLICY=relay
export AGENT_WORKBENCH_OPENROUTER_PROVIDER=z-ai
export AGENT_WORKBENCH_DATA_DIR=$PWD/.kiln/<ticket>/wb-data
export AGENT_WORKBENCH_RUN_DIR=$PWD/.kiln/<ticket>/wb-run
scripts/agent-workbench-dev.sh up --port <port>
```

`AGENT_WORKBENCH_RLM_POLICY` accepts `chronological` (the default when unset
or empty) and `relay`; anything else refuses to boot. Relay selects the native
channel: leave `LASH_RLM_CHANNEL` unset or set it to `native`; relay with
`LASH_RLM_CHANNEL=cell` refuses to boot. In code, the switch is
`RlmProtocolPluginConfig::with_execution_policy(RlmExecutionPolicy::Relay)` on
`RlmChannel::NativeTool`; the factory refuses relay on the cell channel. The
context budget is the RLM `continue_as` soft-warning threshold (100,000 tokens
by default) at four characters per token.

Drive turns with `POST /api/turn` and poll `/api/state` until `active_turns`
and `queued_work` are empty. Extract the per-step prompts:

```sh
python3 runbooks/relay/extract_live.py "$AGENT_WORKBENCH_DATA_DIR/trace.jsonl" \
  .kiln/<ticket>/<scenario> <turn_id> [<turn_id> ...]
```

The script writes `stepN-prompt.json`, `stepN-response.json`, a readable
`transcript.md` (context entries, step message, and the `execute_code`
program or the answer) and `usage.md`, which it also prints: one row per
request with its step, its reply shape (`work` or `answer`), its
breakpoints, the system-prompt hash, the tools, the upstream, the provider's
uncached input, cache read, cache write and output tokens, and the
provider-reported cost.

## Phase 1: S1, an append turn and a follow-on (caching)

Send a turn that commits a large first entry and then appends, for example:
"This is a caching experiment, so follow the step plan exactly. Use exactly 5
work steps, one execute_code call per step, each ending with control.next, and
only append to your context … Step 1: build a reference table of n, n squared
and n cubed for n = 1..400 as one string … Then answer me in plain text."
Then a follow-on turn that reads the table from its context and answers.

Expected: every row of turn 1 is `work` except the last, which is `answer`;
the follow-on's first request shows the full prior context and `<last_reply>`.
Within the provider cache lifetime, a step reads the system prompt and tool,
then also the unchanged context once the provider has it.

Provider caches are per upstream, and OpenRouter spreads a model across
upstreams, so pin one that caches with
`AGENT_WORKBENCH_OPENROUTER_PROVIDER=<slug>` (comma-separated slugs; it sets
the request's `provider.only`) and check the `upstream` column. For
`z-ai/glm-5.3-flash`, `z-ai` caches. Its caching is automatic: it ignores the
breakpoints, reads in 64-token blocks, and a new prefix becomes readable some
seconds after the request that first sent it, so the steps right after a new
prefix can still read only the older one. A turn's first request sometimes
reads nothing even when its prefix is byte-identical to the previous
request's; compare the two `stepN-prompt.json` files before calling that a
finding.

## Phase 2: S3b, a blocked request (plain-text answer)

Connect the `inbox.work` mock account (`POST /api/accounts` with
`{"name": "work"}`) and add two unrelated messages
(`POST /api/accounts/work/messages` with `{"title", "text"}`). Accounts are
global; the mock inbox is in memory. On main today the message route answers
`a mail delivery needs the durable engine` (FIG-5172), yet the message lands
in the inbox; check `GET /api/accounts/work/inbox`. Adding an account queues a
tool-catalog refresh that a session which already ran turns never applies, and
its next turn then fails with `session config command has not settled yet`,
so run this phase in a fresh session (`POST /api/sessions`, then
`POST /api/sessions/select` with its `session_id`). Then ask: "Open the message from Dana in
my work inbox about the Q3 budget and tell me the approved amount."

Expected: the model lists the inbox in one or more `work` steps, then answers
in plain text that there is no such message, and the turn ends. No step is
refused for its reply shape.

## Phase 3: a task that works, then answers

In the same session: "Read my work inbox and tell me what time the team lunch
is and when the badges expire." Expected: one or more `work` rows, then one
`answer` row whose text is the reply row in `/api/state.transcript`.

## Tic-tac-toe memory

**Purpose.** Measure what each policy keeps of results it produced in earlier
turns. The agent plays N games of tic-tac-toe, one user turn each, and a final
turn asks who won the first K. Relay keeps only what the agent passes to
`next`; chronological RLM keeps its history; standard keeps the transcript.

**Automated check.** The world's laws:
`kiln test //examples/agent-workbench:agent-workbench__unit_test --test_arg=ttt::`
(win, draw and illegal-move detection, the perfect opponent never loses, the
random opponent and the mixed schedule replay for a seed, the log records each
result, and the scorer, including a malformed answer and K < N).

**The world.** `AGENT_WORKBENCH_TTT=on` adds two tools, taught in the system
prompt from a session's creation on (the world is fixed at boot, so nothing is
added mid-session):

- `ttt.view_board({})` (standard: `ttt__view_board`) returns `game`, `board`
  (9 cells row by row, `"X"`, `"O"` or `""`), `picture` (empty cells shown by
  number), `empty_cells`, `to_move` and `status`, for the current game only;
- `ttt.make_move({ cell })` (standard: `ttt__make_move`) places X on cell
  0–8 (row by row: 0 1 2 / 3 4 5 / 6 7 8); the opponent (O) moves inside the
  same call. It returns the board, `your_move`, `opponent_move` and `status`:
  `ongoing`, `you won`, `opponent won` or `draw`. An illegal move (a taken
  cell, a cell outside 0–8, no game, or a game that is over) is an error and
  changes nothing.

`POST /api/ttt/games` starts the next game (a game in progress has a null
result in the log), `GET /api/ttt` returns the settings and the game log,
and `POST /api/ttt/score` with `{"answer", "ask"}` scores a memory answer
against the log.

| Variable | Default | Meaning |
|---|---|---|
| `AGENT_WORKBENCH_TTT` | `off` | `on` adds the world, its tools and its routes |
| `AGENT_WORKBENCH_TTT_SEED` | `1` | the seed of every opponent draw |
| `AGENT_WORKBENCH_TTT_SCHEDULE` | `mixed` | `mixed` (each game draws `perfect` or `random` from the seed), `perfect`, `random`, or a comma list such as `perfect,random,random`, repeated when the games outnumber it |
| `AGENT_WORKBENCH_TTT_FIRST` | `agent` | who moves first in every game: `agent` or `opponent`; the agent is always X |

`perfect` is minimax; a tie between equally good cells is broken by the
game's seeded stream. `random` is uniform over the legal cells. Both draw from
a stream keyed by the seed and the game number, so a seed replays the same
games for the same agent moves.

**The driver.** `runbooks/relay/ttt_live.py` launches a fresh workbench (its
own data directory, so a fresh session), plays the games and the final turn,
scores the answer and stops the workbench. Game g is the user turn
`Game g: you are X. Play it to the end.`; with `--told`, game 1's turn adds
`After the games I will ask you who won each of the first K games.`
If a turn answers, fails or stops before the game finishes, the driver sends
`Game g is not finished. Continue playing it to the end.` up to five times.
A game still ongoing after that makes the run invalid; the driver records
why and never scores it. The final turn asks for exactly a JSON array whose
items are `"ASSISTANT"`, `"USER"` or `"DRAW"` (`"ASSISTANT"` = the agent won,
`"USER"` = we, the opponent, won).

| Flag | Default | Meaning |
|---|---|---|
| `--out DIR` | required | the run directory |
| `--games N` | `10` | games played |
| `--ask K` | `5` | games asked about; `1 <= K < N`, and the driver refuses otherwise |
| `--told` | off | warn the agent in game 1 |
| `--seed S`, `--schedule`, `--first` | `1`, `mixed`, `agent` | passed to the world's variables |
| `--policy` | `relay` | `relay` (`AGENT_WORKBENCH_RLM_POLICY=relay`, native channel), `rlm` (chronological RLM) or `standard` (`AGENT_WORKBENCH_PROTOCOL=standard`) |
| `--channel` | `native` | RLM channel, `native` or `cell`; relay requires native; standard has no channel |
| `--model`, `--upstream` | `z-ai/glm-5.3-flash`, `z-ai` | `OPENROUTER_MODEL` and `AGENT_WORKBENCH_OPENROUTER_PROVIDER` |
| `--port`, `--env-file`, `--budget` | `4491`, none, `0.5` | the port; a `KEY=VALUE` file with `OPENROUTER_API_KEY`; stop after the game whose spend passes this many dollars |

It writes `results.json` (per game the result, misplays, refused moves, the
turns and continuation count, requests, work steps, tokens, cache reads and cost;
the final turn; the totals; the score), `log.json` (the world's log), `game-<g>/` and `final/`
(`extract_live.py`'s per-request files and `usage.md`), and the workbench's
`wb-data/` and `wb-run/`.

All six variants (three policies, each told and not told), in a Kiln fork with
`env.sh` sourced:

```sh
for policy in relay rlm standard; do
  python3 runbooks/relay/ttt_live.py --out .kiln/<ticket>/ttt/$policy-told-no \
    --policy $policy --env-file /path/to/.env
  python3 runbooks/relay/ttt_live.py --out .kiln/<ticket>/ttt/$policy-told-yes \
    --policy $policy --told --env-file /path/to/.env
done
```

## Answer key: tic-tac-toe memory

State this before observing.

- The answer key is the world's log (`log.json`, `GET /api/ttt`), never the
  schedule: each finished game's `result` is the typed value `ASSISTANT`,
  `USER` or `DRAW`. An ongoing game has no result and cannot be scored.
- The score reads the first well-formed JSON array in the final reply. String
  items are trimmed and uppercased, then parsed as those three enum values;
  any other item scores wrong. A wrong-length array scores all items wrong.
  `per_game` compares item i with game i's key; `exact` needs K correct items.
  A reply with no array scores 0 of K.
- The perfect opponent never loses: `ASSISTANT` against a `perfect` game is a
  **finding and FAIL**. `misplays` lists the agent's moves (1-based) that
  lowered its minimax outcome; a loss always has one.
- Every game must finish before the next starts. `continuation_turns` records
  how many additional turns each game needed (0–5). The run is invalid if the
  cap is reached with the game ongoing. Every failed turn, including an
  initial turn repaired by a continuation, remains in `totals.failed_turns`
  and must be explained.
- Relay execution-bound exhaustion is a failed step: it commits no baton,
  preserves effect receipts, and the next step's status says
  `not committed: execution bound exhausted (instructions)` (or `memory`).
  The existing no-progress and iteration budgets still bound the turn.
- Spend is the sum of the provider-reported costs in the `usage.md` tables.

## Teardown

```sh
scripts/agent-workbench-dev.sh down --port <port>
```

with the same environment as launch. Keep the trace and extracts as evidence.

## Choose-your-own-adventure memory

The agent explores a seeded story tree for 12 rounds, then answers 12 questions
about its journey. Each passage introduces a named person and one other entity,
with factual attributes drawn from seeded vocabularies: age, a place's gate code,
an object's colour or material, or a dog's name. Names are reserved across the
path and every sibling before exposing a destination; collisions receive a
numeric suffix. Only those nodes are allocated, never a subtree. Coins start at
zero and a passage's delta is applied when
the agent leaves it. Each round permits exactly one choice; `choose` returns the
next round's passage, whose delta has not yet been applied.

At boot, `AGENT_WORKBENCH_STORY=on` registers the tools and their prompt for every
new session. RLM uses `await story.read({})` and
`await story.choose({ option: "A" })`; standard uses `story__read` and
`story__choose`. `read` returns top-level `passage`, `facts`, `choices` (option and label),
`coins`, `round`, `passage_round`, `round_finished`, `choice` and `path`; `choose` returns the new passage with `round_finished=true`; work steps
are never new user rounds. Invalid options are errors
and change nothing. Tools never expose the path log, questions or answer key.

| Workbench variable | Default | Meaning |
|---|---|---|
| `AGENT_WORKBENCH_STORY` | `off` | `on` enables the scenario |
| `AGENT_WORKBENCH_STORY_SEED` | `1` | u64 tree seed |
| `AGENT_WORKBENCH_STORY_BRANCHING` | `3` | number of choices, 2–26, labelled A–Z |
| `AGENT_WORKBENCH_STORY_VOCABULARY_SEED` | `1` | u64 vocabulary draw seed |

The driver `runbooks/relay/cyoa_live.py` starts a fresh workbench, sends one user
turn per round and stops the workbench after the quiz. A round that ends without
a choice gets `Round r is not finished. Choose one option.` up to five times.
A round still unfinished makes the run invalid: it gets no score. All turn
outcomes and continuation counts are retained, including failed initial turns.

| Driver flag | Default | Meaning |
|---|---|---|
| `--out DIR` | required | evidence directory, with fresh data/session |
| `--seed`, `--branching`, `--vocabulary-seed` | `1`, `3`, `1` | world variables; seed also seeds the quiz |
| `--rounds`, `--questions` | `12`, `12` | positive counts |
| `--types` | `fact,order,state,negative` | cyclic type mix; repeat entries for weights |
| `--told` | false | round 1 adds “At the end I will quiz you on details of your journey: ages, codes, objects, pets, coins and the order of people.” |
| `--delivery` | `tool` | `tool`: read through tools; `message`: passage and choices in the round's user message |
| `--policy`, `--channel` | `relay`, `native` | relay, chronological `rlm`, standard; relay requires native, standard has no channel |
| `--model`, `--upstream` | `z-ai/glm-5.3-flash`, `z-ai` | model and OpenRouter provider pin |
| `--port`, `--env-file`, `--budget` | `4492`, none, `$0.15` | port, key file, budget guard; reserve enough for the next turn before starting it |

The generator samples without replacement from finite pools of facts, unordered
person pairs, coin states and visited/unvisited people. Every question names an
entity or the final total; questions never refer to rounds or turns. Negative
questions alternate unvisited sibling people (`NO`) and visited people (`YES`).
For R completed passages the capacities are 2R facts, R(R−1)/2 order pairs,
R states, and 2R negatives (2R+1 with branching greater than two, keeping the
NO/YES mix balanced). The final coin state is phrased as either “now” or an
entity anchor, never both in one quiz. Both the driver and server refuse counts beyond the
requested type mix's capacity, empty mixes, zero questions and unfinished paths.

Run every policy/told variant with `env.sh` sourced and a key exported, or with
`--env-file /workspace/code/lash/.env`. Both RLM baselines use native:

```sh
for policy in relay rlm standard; do
  python3 runbooks/relay/cyoa_live.py --out .kiln/FIG-4441/cyoa/$policy-told-no \
    --policy "$policy" --env-file /workspace/code/lash/.env
  python3 runbooks/relay/cyoa_live.py --out .kiln/FIG-4441/cyoa/$policy-told-yes \
    --policy "$policy" --told --env-file /workspace/code/lash/.env
done
```

Add `--delivery message` to test message delivery. Use `--types fact,state`
for a restricted mix or `--types fact,fact,order,state,negative` to weight
facts. `--rounds`, `--questions`, seeds, branching, model and upstream are
independent settings subject to the validation above.

`results.json` records each question's type, lookback, expected/given answer
and correctness; overall accuracy and breakdowns by type and lookback; each
round's turns, continuation count, work steps, requests, tokens, cache reads
and provider cost; the final turn and totals. `log.json` is the authoritative
path log; `questions.json` holds the generated answer key. Per-round and final
directories contain `extract_live.py`'s request/response files, `transcript.md`
and `usage.md`. `GET /api/story`, `POST /api/story/rounds`,
`POST /api/story/questions` and `POST /api/story/score` are driver-only routes;
the last two take `{seed, questions, types}` (score also takes `answer`).

**Answer key: choose-your-own-adventure memory.** State this before observing:

- Fact answers are each named entity's attribute value: strings for colours,
  materials and pet names; integers for ages and gate codes. Order asks whether
  one named person was met before another, with `YES` or `NO` as the answer.
- State answers are integers: the final coin total, or the total immediately
  after the delta of the passage introducing the named person.
- Negative answers are exactly `YES` or `NO`. A `NO` person comes from an
  unchosen sibling, whose reserved name cannot appear elsewhere on the path.
- Lookback is R minus the source passage's internal round number; the last
  completed passage has lookback 0. Order uses the earlier person's passage;
  an unvisited negative uses its parent branchpoint. These numbers appear in
  evidence only, never question text.
- The scorer reads the first JSON object, including inside fences, and scores
  each key independently. Missing,
  nonscalar, wrongly typed or malformed values score zero. Strings ignore case
  and repeated whitespace; integer values also accept trimmed integer strings.
  Fractions, booleans, number separators and yes/no aliases are rejected.
  Additional keys do not change the score. Buckets are 0–3, 4–7 and 8+.
- An unfinished round invalidates the run and is never scored. Every failed
  turn must be explained from the preserved response and workbench log.
- Spend uses provider-reported usage, and every request must be served by the
  configured model/upstream. No human or model judge contributes to accuracy.

The story laws are in `story::tests` in
`//examples/agent-workbench:agent-workbench__unit_test`: seeded reproducibility,
invalid choice leaves state unchanged, log matches moves, unique entity names
across the path and siblings, no round/turn labels in questions, distinct
questions and capacity refusal, typed scoring including malformed values, and
entity-anchored coin totals. Run each changed/new law by its full test path.

Failed model requests remain in the usage tables. When their trace lacks usage,
the shared live driver reconciles the failed generation through OpenRouter's
read-only generation receipt API and saves `billing.json`. Missing receipts are
listed as `unreported_generations`; their amounts remain unknown. Successful
requests use their original trace usage. This applies to both live drivers.
