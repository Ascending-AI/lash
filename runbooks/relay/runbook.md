# E2E Scenario: Relay Execution Policy in the Workbench

> **Read [../RULES.md](../RULES.md) first.** This runbook adds only the relay scenario.

**Automated check.** The model-free laws are
`kiln test //crates/lash:lash__unit_test --test_arg=tests::relay::` (commit,
rollback, budget, receipts, final, between-turns input, cancel, redrive) and
`kiln test //crates/lash-protocol-rlm:lash-protocol-rlm__unit_test --test_arg=relay::`
(baton validation, turn-input view). Run those first; this runbook covers what
they cannot: a real model working under relay.

**Purpose.** Referee an RLM session running the relay execution policy. Each
model call sees exactly the context its previous step committed with
`control.next`, plus one harness message, and nothing else. The scenario
proves that steps commit their batons, that a failed step commits nothing,
that output reaches the user only from a committed step, and that the next
turn starts from the last committed context.

**Real tokens.** This scenario uses OpenRouter with the workbench's default
model. Model prose and step counts are nondeterministic. Gate on the provider
requests in the trace, never on prose.

## Answer key: state this before observing

- Every relay request has at most two messages: one user message of context
  blocks (one block per entry of the last committed `next`, the last block a
  cache breakpoint), then one user message beginning `=== HARNESS · step N ===`.
  The first step of a session has no context message.
- Every harness message of a turn carries that turn's user message under
  `--- User message (this turn) ---`, on every step, committed or not.
- A step whose harness says `--- Last step (committed) ---` has a context equal
  to the `context` argument of the previous step's `next` call, entry for entry.
- A step after a failed one says `NOT committed` and shows the same context
  as the step before it. Calls that ran in the failed step are listed under
  `--- Effects of steps that did not commit ---` and are not repeated.
- The turn ends on a step that calls `send_user_output` and then
  `next({ ..., final: true })`. The assistant row in `/api/state.messages` is
  exactly that output; prose outside a cell never reaches the user.
- The next turn's first request has the context the final `next` committed.

Any mismatch is a **finding and FAIL**.

## Phase 0: launch

In a Kiln fork, with `env.sh` sourced and an OpenRouter key exported:

```sh
export RESTATE_AUTHORITY_ID=<ticket> AGENT_WORKBENCH_RLM_POLICY=relay
export AGENT_WORKBENCH_DATA_DIR=$PWD/.kiln/<ticket>/wb-data
export AGENT_WORKBENCH_RUN_DIR=$PWD/.kiln/<ticket>/wb-run
scripts/agent-workbench-dev.sh up --port <port>
```

`AGENT_WORKBENCH_RLM_POLICY` accepts `chronological` (the default when unset
or empty) and `relay`; anything else refuses to boot. In code, the switch is
`RlmProtocolPluginConfig::with_execution_policy(RlmExecutionPolicy::Relay)`.
The context budget is the RLM `continue_as` soft-warning threshold (100,000
tokens by default) at four characters per token.

## Phase 1: world

Create a session (`POST /api/sessions`), connect the `inbox.work` mock account
and send it two messages whose contents combine into one answer (for example
part A and part B of a code). The mock inbox is in memory and per session:
connect it in the session you will drive, or the agent has nothing to read.

## Phase 2: a multi-step turn

Send a turn that needs at least two steps, for example "Read the work inbox and
tell me the combined vault code." Poll `/api/state` until `active_turns` and
`queued_work` are empty. Extract the per-step prompts:

```sh
python3 runbooks/relay/extract_live.py "$AGENT_WORKBENCH_DATA_DIR/trace.jsonl" \
  .kiln/<ticket>/turnN <turn_id>
```

The script writes `stepN-prompt.json`, `stepN-response.json` and a readable
`transcript.md`. Check every answer-key item against them.

## Phase 3: the baton across turns

In the same session, send a turn that needs the previous turn's result (for
example "remind me of the code from your context and count the words in each
message, one step per message"). Its first request must show the previous
turn's final context.

## Teardown

```sh
scripts/agent-workbench-dev.sh down --port <port>
```

with the same environment as launch. Keep the trace and extracts as evidence.
