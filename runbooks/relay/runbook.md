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

## Teardown

```sh
scripts/agent-workbench-dev.sh down --port <port>
```

with the same environment as launch. Keep the trace and extracts as evidence.
