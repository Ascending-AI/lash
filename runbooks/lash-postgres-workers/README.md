# lash-postgres-workers

A scripted failover harness (gate evidence, not a judged runbook; see
[RULES.md](../RULES.md)). It boots lash nodes as separate OS processes over
one PostgreSQL store, kills, partitions, stops and restarts them at chosen
points, and shows for each case that:

- **the work finishes on another node**: the store holds no unfinished turn
  and exactly one `turn.commit` committed (or the process holds its
  terminal), done by the survivor after the fault;
- **no zombie commits**: the store holds only the new owner's outcome,
  written under the new owner's epoch, and nothing the old owner wrote after
  the fault;
- **no `Once` body ran twice**: the witness ledger holds at most one
  `entered` row per `Once` call.

The harness is the internal crate
[`crates/lash-postgres-workers`](../../crates/lash-postgres-workers): it drives
the production runner with scripted services, as `lash-durable-test` does in
process, so it is a substrate harness, not a facade host (the facade-only
rule for `runbooks/*/src` does not apply to it). A facade-host version of this
proof follows once the facade serves durable nodes (L13, FIG-5193).

It is the cross-process proof of ADR 0132 §3 (actors, node liveness, the
epoch fence) and of L8's notifier, liveness lock and failover bound
(FIG-5178). It replaces the deleted engine-workers runbook
(FIG-5190); the [row-disposition table](#row-disposition-table) accounts for
every live row of that runbook.

## Run it

```sh
. ./env.sh
kiln test //crates/lash-postgres-workers:failover__test \
  --test_arg=--exact --test_arg=<case> --test_arg=--nocapture --test_output all
```

Each case boots its own PostgreSQL 18 (`native//:postgres`, handed to the
test as `LASH_WORKERS_POSTGRES`; `LASH_WORKERS_NSS_WRAPPER` lets `initdb`
run as a user the pool image does not list), so a case can stop and restart
the server. Nothing else is needed: no Docker, no live services. The cases run
one at a time (`RUST_TEST_THREADS=1`) and the target is `dev-deferred`: two
cases wait out a 15 s node lease. Each passing case prints one `case=` line
with what it measured. Outside kiln, point `LASH_WORKERS_POSTGRES` at any
PostgreSQL 18 installation directory (one with `bin/initdb`, `bin/postgres`
and `bin/pg_ctl`).

## What runs

| Part | Where | Real or scripted |
|---|---|---|
| Node process | [`src/bin/node.rs`](../../crates/lash-postgres-workers/src/bin/node.rs), [`src/node.rs`](../../crates/lash-postgres-workers/src/node.rs) | real: `lash_core::runtime::durable::node::serve` over `PostgresStoreSet`, the production runner, session activation, process activation, fences, notifier and liveness lock, with the default `DurableSettings` |
| Turn | [`src/turn.rs`](../../crates/lash-postgres-workers/src/turn.rs) | real: the lash facade's session and send, the production turn driver, model pins, and a TypeScript cell on the RLM worker path with its durable snapshots; the model is scripted |
| `ext_write` | [`src/turn.rs`](../../crates/lash-postgres-workers/src/turn.rs) | a `Once` tool called from the cell through the production tool dispatch; its body is the runbook's |
| Process | [`src/process.rs`](../../crates/lash-postgres-workers/src/process.rs) | a host engine (`advance`) with two `Once` steps and a pinned key with a deadline |
| Reports | [`src/recorded.rs`](../../crates/lash-postgres-workers/src/recorded.rs), [`src/events.rs`](../../crates/lash-postgres-workers/src/events.rs) | decorators over the durable store and signals that forward every call and print what the store answered; the partition's heartbeat hold is the one fault they inject |
| Server, nodes, faults | [`tests/support/`](../../crates/lash-postgres-workers/tests/support/) | the test owns the server and the node processes (SIGKILL, stop on stdin, heartbeat hold) |
| Witness ledger | [`witness.sql`](../../crates/lash-postgres-workers/witness.sql) | a separate database the nodes write through an insert-only role; the laws read it and the store, never the report lines, for effects |

A node reads its configuration from its environment (`LASH_WORKERS_NODE`,
`LASH_WORKERS_DATABASE_URL`, `LASH_WORKERS_WITNESS_URL`,
`LASH_WORKERS_NOTIFIER` = `after-commit` or `poll-only`, `LASH_WORKERS_HOLD`,
and `LASH_WORKERS_ADMIT_TURN` for runs by hand), its commands from stdin (`stop`, `block-heartbeat`,
`unblock-heartbeat`; end of input stops it) and writes one JSON report per
line to stdout.

## Cases

Measured on the pool in FIG-5199's final run (all seven in one invocation on the rebased tree, 52.8 s); each row is one test.

| Case (test) | Fault | Laws checked | Result |
|---|---|---|---|
| `a_turn_killed_mid_model_call_finishes_on_another_node` | kill -9 of the node holding the turn's first model call, after the survivor's probe saw it alive | reap by liveness lock; survivor claims and re-sends the pinned call as attempt 2; one `turn.commit` on the survivor; `ext_write` once, `Completed` | detected 257 ms after the kill, claimed 1 ms after the reap, re-sent 278 ms after the kill |
| `a_turn_killed_mid_model_call_without_the_liveness_lock_finishes_within_the_lease_bound` | the same kill with `poll-only` (no listener, no lock) | reap by lease within `ttl` + `reap_every`; the rest as above | detected 15 963 ms after the kill, claimed 169 ms after the reap |
| `a_once_step_killed_mid_body_settles_interrupted_on_another_node` | kill -9 inside `ext_write`'s body, after its witness entry | the started `Once` settles `Interrupted` under the survivor's epoch; its body is never entered again; one `turn.commit` on the survivor | detected 258 ms after the kill; one `entered` row, on the victim; outcome under epoch 3 |
| `a_process_killed_mid_wait_finishes_on_another_node_with_its_start_key_feed_and_state` | kill -9 of the node that ran the process up to its pinned wait | the deadline fires on the survivor, which runs the step after it once; the terminal carries the engine state's full count (7 transitions); the feed holds `before` and `after` once each, in order; the start key still names the process, and a second start under it answers the same process | the victim had already released the waiting process as `waiting` in this run (ADR 0132 §6; in earlier runs it still owned it); either way the survivor took it and ended it 4 199 ms after the kill (deadline 4 s) |
| `a_node_whose_heartbeat_is_held_stops_itself_before_its_lease_lapses` | partition: the node's heartbeats are held while its `ext_write` body is running | the zombie stops `unrenewed`, its activations (the body among them) dropped, within `self_stop_after` + `claim_poll` of its last lease extension (asserted); the survivor reaps it and finishes the turn with the operation `Interrupted`; the zombie commits nothing while partitioned, and the body is never entered again | stopped 10 005 ms after its last lease extension (bound 10 250 ms), reaped through its liveness lock 9 999 ms after the hold (FIG-5178's run); see [findings](#findings), item 1 |
| `a_cleanly_stopped_node_hands_its_turn_over_at_once` | `stop` while the node holds the first model call | the node releases its actors and stops `requested`; no reap; the survivor claims within a claim poll and finishes | released 2 ms after the request, claimed 7 ms after the release (up to 156 ms in earlier runs: the survivor's next claim poll) |
| `a_postgres_restart_strands_no_work_and_reaps_no_node` | fast shutdown of the server for about 3 s while a model call is in flight | no node reaped, no node stopped; the turn finishes with `ext_write` run once and `Completed` | outage 3 141 ms; the turn finished on the node that held it, 2 862 ms after the server was back. In an earlier run the held call was re-sent once by the same owner (attempt 2) after its commit failed during the outage: a pinned model call may be re-sent, only `Once` bodies may not |

A killed node cannot commit: in the kill cases the zombie law holds because
the process is gone, and the store shows the outcome under the survivor's
epoch. A partitioned node stops itself before its lease lapses, so it never
tries either; the epoch fence that refuses a zombie that does is proven in
process by `lash-durable-test`'s fencing laws.

### Failover bound (L8, FIG-5178)

| | Bound | Measured |
|---|---|---|
| Detection with the liveness lock | about one liveness probe (`claim_poll`, 250 ms) | 257–258 ms |
| Detection without it | `ttl` (15 s) + `reap_every` (2 s) | 15 963 ms |
| Resume after the reap | `claim_poll` (250 ms) + ε | 1 ms (lock reap claims at once), 169 ms (lease reap waits for the next claim) |
| Clean handover | `claim_poll` + ε | 7 ms (up to 156 ms) |

The cases assert these with one second of slack for a loaded action.

## Findings

1. **A heartbeat that does not return stopped the runner's loop** (fixed in
   FIG-5178). The runner awaited the heartbeat inline in its loop, so while one
   heartbeat call hung the node neither stopped itself at `self_stop_after`
   (10 s) nor polled its stop future, and its activations kept running past
   its lease: the zombie served 16.8 s past its last lease extension, and the
   epoch fence refused its late write. A real partition has the same shape: an
   established connection to an unreachable server waits for TCP, and a new
   one waits up to the pool's `acquire_timeout` (30 s). Now every store call
   the runner makes races its stop and the self-stop deadline, and the
   partition case asserts the zombie stops within `self_stop_after` +
   `claim_poll`.
2. **A boot no other node saw alive is reaped by its lease, not its lock.** A
   watcher reaps only a boot whose lock it saw held and then free, so that a
   server restart (which drops every lock) reaps nobody. A node killed before
   any other node's first probe (within 250 ms of its boot) costs a lease. The
   kill cases wait for the survivor's probe to see the victim before killing.
3. **An owner commit refused as `Corrupt` is retried by the same activation
   every poll** (owner: L6, FIG-5175). A process whose `advance` emitted an
   undeclared event type retried its `process.advance` commit every claim
   poll for as long as it was owned; it never reached the activation-loop
   budget, which counts claims. Seen while writing the process case, before
   its registration declared its event types.

## The PostgreSQL durability assumption (ADR 0132 §13)

`Once` holds across a database failover only if acknowledged commits survive
promotion. PostgreSQL topology and promotion are the host's: this runbook
restarts one server (a fast shutdown flushes every acknowledged commit) and
does not certify promotion, replicas or synchronous replication. Without that
assumption the guarantee is "at most once, except on loss of acknowledged
commits". Nodes must reach the server directly or through a session-mode
pooler: `LISTEN` and the liveness lock need a session that outlives a
transaction (see [connecting PostgreSQL nodes](../../docs/operations/deploying-and-upgrading.md)).

## By hand

`docker-compose.yml` starts PostgreSQL 18 with `lash` and `lash_witness`
provisioned, for watching nodes by hand; the cases above do not use it.

```sh
docker compose -f runbooks/lash-postgres-workers/docker-compose.yml up -d
port=$(docker compose -f runbooks/lash-postgres-workers/docker-compose.yml port postgres 5432 | cut -d: -f2)
kiln build //crates/lash-postgres-workers:lash-postgres-workers-node__bin
export LASH_WORKERS_DATABASE_URL=postgres://lash@127.0.0.1:$port/lash \
  LASH_WORKERS_WITNESS_URL=postgres://lash_witness_writer@127.0.0.1:$port/lash_witness \
  LASH_WORKERS_HOLD=model
LASH_WORKERS_NODE=a LASH_WORKERS_ADMIT_TURN=1 <node binary> &   # admits the turn and holds its model call
LASH_WORKERS_NODE=b <node binary> &
kill -9 <a's pid>    # b reaps a, re-sends the call and commits the turn
psql "postgres://lash@127.0.0.1:$port/lash_witness" -c 'TABLE witness_model_attempts'
docker compose -f runbooks/lash-postgres-workers/docker-compose.yml down -v
```

## Row-disposition table

Every live row of the deleted engine-workers runbook at its last revision
(`5ad9641f9e^`): its README's gates, faults, laws and ledgers, its runner's
30-row `WORKFLOW_INVENTORY`, and its `tests/`. "Deleted: engine mechanics"
rows tested a mechanism of the deleted journaling engine that lash no longer has; other rows name the case here or
the law elsewhere that now carries the property.

### Failover, crash and restart rows

| Old row | Disposition |
|---|---|
| `e2e-failover` (a worker exits mid-turn; a peer finishes) | `a_turn_killed_mid_model_call_finishes_on_another_node`, `a_once_step_killed_mid_body_settles_interrupted_on_another_node` |
| `e2e-failover-wake` (the failed-over turn's wake delivered once) | `a_turn_killed_mid_model_call_finishes_on_another_node` (one `turn.commit`); wake delivery is mail in the producer's transaction (ADR 0132 §12), `mail_from_another_node_reaches_a_hot_owner_through_its_hint` and `mail_whose_hint_is_lost_reaches_a_hot_owner_within_its_poll` (`lash-postgres-store` durable signals laws) |
| `e2e-tool-batch-failover`, "loss after commit" fault (worker exits after the receiver commits, before the reply) | `a_once_step_killed_mid_body_settles_interrupted_on_another_node`; the in-process cut at every label: `a_once_member_never_starts_twice_across_every_cut` (`lash-durable-test` `round_crash_matrix`) |
| "replay over completed effects" (`crash_once` after `Promise.all`) | deleted: engine mechanics. No code re-runs against a recorded history (ADR 0132 §2); the property it guarded, a completed effect is not run again, is the `Once` law of every case here |
| `e2e-engine-restart-suspended-sleep`, `e2e-engine-restart-cancel`, `e2e-engine-restart-complete`, "cluster restart" fault (SIGKILL both workers, restart the engine server) | the store-restart half: `a_postgres_restart_strands_no_work_and_reaps_no_node`; the node-restart half: the kill cases; a sleeping actor survives its owner: `a_process_killed_mid_wait_finishes_on_another_node_with_its_start_key_feed_and_state`, `a_wait_survives_its_owners_death_with_the_same_key_and_deadline` (wait laws). Restarting the engine server itself: deleted: engine mechanics |
| `e2e-frame-switch-crash` (a crash mid frame switch) | not a failover row: a frame switch is a session phase (L3, FIG-5172; L3s, FIG-5196); its crash cuts belong to the turn's crash matrix (`turn_phases`) |
| `e2e-turn-cancel-crash-recovery` (cancellation replayed by a peer after the original worker exits) | `a_cancel_while_streaming_ends_the_turn_and_a_crash_mid_cancel_finalizes_it` (`lash-durable-test` `turn_phases`: any node finalizes an accepted cancel from its row) |
| `e2e-process-llm-query-replay` (a process's model query replayed after a worker exit) | deleted: engine mechanics (journal replay of a process's query). A process step that survives its node: `a_process_killed_mid_wait_finishes_on_another_node_with_its_start_key_feed_and_state` |
| `tests/process_operations_replacement.rs` (`start_key_feed_and_plugin_state_survive_worker_replacement`) | `a_process_killed_mid_wait_finishes_on_another_node_with_its_start_key_feed_and_state`: start key, event feed and engine state (the host engine's state is what plugin state was) survive a node kill |
| Recovery law 1, durable-result stability and recovery progress | every case: the terminal is in the store, and the unfinished work finishes after the fault |
| Recovery law 2, logical-effect identity (one commit per key, retries send identical bytes) | the `Once` law of every case; identical re-sent bytes: `a_turn_of_two_model_calls_resumes_at_every_label_without_replay` (`turn_phases`) |
| Recovery law 3, causal identity | every witness row names its call and its node; the cases check each against the store's run records |
| Recovery law 4, replay equivalence | deleted: engine mechanics (it compared a replay with the original run); its property is the `Once` law and the zombie law here |
| Law 5, fencing (deferred there for want of an external authority token) | the zombie law: `a_node_whose_heartbeat_is_held_stops_itself_before_its_lease_lapses`, judged from the store's `written_epoch` and the zombie's empty commit record after the partition; the refusal of a zombie that does write is `lash-durable-test`'s fencing laws |
| Witness ledgers (separate database, insert-only role, database clock) | `witness.sql`: `witness_effects`, `witness_model_attempts`, `witness_nemesis`. Submissions, acknowledgements, client terminals and provider receipts were engine ingress and HTTP output: deleted: engine mechanics; the store's own terminal rows replace them |
| `tests.rs` checker fixtures (legal histories, one fixture per rule, property tests) | deleted with the checkers: the laws here are direct assertions over one case's rows, with nothing to fixture |

### Rows that tested the deleted engine itself

| Old row | Disposition |
|---|---|
| Break-glass gate (engine-admin invocation hard-kill stays break-glass) | deleted: engine mechanics (there is no invocation to kill; an operator cancel is a cancel request row) |
| "Deployment upgrades" (deployment pin-and-drain, RT0016 journal mismatches, `drain_status` of a deployment) | deleted: engine mechanics; a node drains by releasing its actors (`a_cleanly_stopped_node_hands_its_turn_over_at_once`), and format drains are L11's (FIG-5187) |
| `fig1126_pending_tool_redrives_after_worker_loss_and_resumes_once` (endpoint-protocol journal splice) | deleted: engine mechanics |
| Stall watchdog (unfinished engine invocations after 240 s) | deleted: engine mechanics; every wait in the cases has its own timeout |
| Coverage scoring (`write_completed_workflow_manifest`, `EXPECTED_WORKFLOW_INVENTORY_LEN` and its coverage-check script) | deleted: engine mechanics (it scored segments of the engine's workflow inventory); each case here is its own test |
| "Local Postgres conformance" (`with-service.sh pg` for the registry conformance) | not a runbook row: the PostgreSQL store's own laws run on its hermetic server (`//crates/lash-postgres-store`) |
| Load witness contracts (`src/load/*`, `just loadtest-ledger`) | not a failover row: load measurement is L12b's (FIG-5188) |
| The session-operator paragraph (withdrawal, running cancel, parked redrive/cancel/fork, lost-reply repeat) | not covered here: owed by the `runbooks/session-operator/runbook.md` ledger row |

### Functional workflow rows

These rows exercised features end to end through the deleted engine's workflows. They are
not failover rows; each names where its property is held now.

| Old row | Disposition |
|---|---|
| `e2e-main`, `e2e-main-wake` | a turn that commits and wakes its session: every turn case here; L9h (FIG-5186) boots example hosts on the durable substrate |
| `e2e-trigger-setup`, `e2e-trigger-emit` | triggers: L6's trigger follow-up (`fig-5175-trigger`, start in the reservation transaction) and L9h |
| `e2e-signal-suspend-setup`, `e2e-signal-first`, `e2e-signal-second` | process signals as mail (`EngineEvent::Signal`): L6 (FIG-5175) and L7b (FIG-5198) |
| `e2e-async-completion`, `e2e-durable-input`, `e2e-parent-durable-input-after-child` | completion keys and waits: `an_unresolved_wait_suspends_and_resumes_on_resolution`, `a_completion_before_the_await_is_already_resolved`, `the_first_resolution_wins` (wait laws, L5) |
| `e2e-process-llm-query` | a process step through the admitted-execution primitive: `a_process_cut_at_every_label_runs_no_step_before_its_state_commits` (`process_crash_proof`) |
| `e2e-tool-batch` | rounds: `round_crash_matrix` (L4, FIG-5174) |
| `e2e-segment-loop` | deleted: engine mechanics (segment cuts kept a journal bounded; ADR 0132 deletes them) |
| `e2e-frame-switch-queued`, `e2e-frame-switch-prepared`, `e2e-frame-switch-cancel` | frame switches are session phases: L3 (FIG-5172) and L3s (FIG-5196) |
| `e2e-suspended-sleep-cancel` | `the_awaiters_cancel_ends_its_wait` (wait laws) |
| `e2e-turn-cancel-late-normal`, `e2e-turn-cancel-before-start`, `e2e-turn-cancel-cross-process`, `e2e-turn-cancel-seal-race` | turn cancel is a first-winner row: `a_turn_cancel_is_a_first_winner_row_with_a_wake` (store law), `a_cancel_while_streaming_ends_the_turn_and_a_crash_mid_cancel_finalizes_it` and `an_after_step_cancel_lets_the_streaming_call_finish_and_stops_at_the_next_boundary` (`turn_phases`, requested from outside the actor) |
| Queued-work tombstone redelivery check | deleted with the wake outbox (L6, FIG-5175): delivery is the append's own transaction |
| `tests/provider_stream_bounds.rs` (real provider parsing through engine turns) | deleted: engine mechanics; provider parsing has its own crate laws |
