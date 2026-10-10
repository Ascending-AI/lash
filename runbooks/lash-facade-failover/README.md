# lash-facade-failover

A scripted failover harness (gate evidence, not a judged runbook; see
[RULES.md](../RULES.md)): the facade-host version of the
[lash-postgres-workers](../lash-postgres-workers/README.md) runbook (FIG-5199).
It runs six turn and node cases, with the same faults and laws, but each node is
a facade host: one OS process holding one `lash::LashCore` whose own node
serves the store's session and process actors, as any lash host's core does.
Everything a node uses is the lash facade's (`scripts/check_facade_only_examples.py`
holds `src/` to it). For each case it shows that:

- **the work finishes on another node**: the store holds no unfinished turn
  and exactly one `turn.commit` committed (or the process holds its
  terminal), done by the survivor after the fault;
- **no zombie commits**: the store holds only the new owner's outcome,
  written under the new owner's epoch, and nothing the old owner wrote after
  the fault;
- **no `Once` body ran twice**: the witness ledger holds at most one
  `entered` row per `Once` call.

## Run it

```sh
. ./env.sh
kiln test //runbooks/lash-facade-failover:failover__test \
  --test_arg=--exact --test_arg=<case> --test_arg=--nocapture --test_output all
```

Each case boots its own PostgreSQL 18 (`native//:postgres`, handed to the
test as `LASH_WORKERS_POSTGRES`, with `LASH_WORKERS_NSS_WRAPPER` for `initdb`
on the pool), exactly as the substrate runbook does, so `with-service.sh pg17`
hands it the PostgreSQL 17 tree the same way. The release gate runs it as the
`facade-failover` leg of `store-tests.sh pg-release`. The cases run one at a
time and the target is `dev-deferred`: two cases wait out a 15 s node lease.

## What runs

| Part | Where | Real or scripted |
|---|---|---|
| Node process | [`src/bin/node.rs`](src/bin/node.rs), [`src/node.rs`](src/node.rs) | real: `DurableBackendBuilder` over `PostgresStoreSet` and one `LashCore` built on it, whose node is the production runner, session activation, process worker, fences, notifier and liveness lock, with the default `DurableSettings`. The host shuts the core down on `stop`, and learns how the node stopped from `LashCore::node_stopped` |
| Turn | [`src/turn.rs`](src/turn.rs) | real: the facade's session and send, the production turn driver, model pins, and a TypeScript cell on the code mode worker path with its durable snapshots; the model is scripted (`lash::testing::TestProvider`) |
| `ext_write` | [`src/turn.rs`](src/turn.rs) | a `Once` host tool (`StaticToolProvider`) called from the cell through the production tool dispatch; its body is the runbook's |
| Reports | [`src/recorded.rs`](src/recorded.rs), [`src/events.rs`](src/events.rs) | decorators over the facade's `StoreSet`, durable store and node wakes that forward every call and print what the store answered; the partition's heartbeat hold is the one fault they inject |
| Server, nodes, faults | [`tests/support/`](tests/support/) | the test owns the server and the node processes (SIGKILL, stop on stdin, heartbeat hold); its operator core admits the turn and serves no node |
| Witness ledger | [`witness.sql`](witness.sql) | the substrate runbook's ledger, unchanged |

A node reads its configuration from its environment (`LASH_FAILOVER_NODE`,
`LASH_FAILOVER_DATABASE_URL`, `LASH_FAILOVER_WITNESS_URL`,
`LASH_FAILOVER_NOTIFIER` = `after-commit` or `poll-only`,
`LASH_FAILOVER_HOLD`, and `LASH_FAILOVER_ADMIT_TURN` for runs by hand), its
commands from stdin (`stop`, `block-heartbeat`, `unblock-heartbeat`; end of
input stops it) and writes one JSON report per line to stdout. Its node is
named by the core's owner id, the node name.

## Cases

Measured on the pool in FIG-5193's runs; each row is one test, and the laws
are the substrate runbook's.

| Case (test) | Fault | Result |
|---|---|---|
| `a_turn_killed_mid_model_call_finishes_on_another_node` | kill -9 of the node holding the turn's first model call | detected 111 ms after the kill by the liveness lock, claimed 2 ms after the reap, re-sent 203 ms after the kill; one `turn.commit` on the survivor |
| `a_turn_killed_mid_model_call_without_the_liveness_lock_finishes_within_the_lease_bound` | the same kill with `poll-only` | detected 15 805 ms after the kill by the lease (`ttl` + `reap_every`), claimed 170 ms after the reap |
| `a_once_step_killed_mid_body_settles_interrupted_on_another_node` | kill -9 inside `ext_write`'s body, after its witness entry | detected 151 ms after the kill; the operation settled `Interrupted` under the survivor's epoch 3; one `entered` row, on the victim |
| `a_node_whose_heartbeat_is_held_stops_itself_before_its_lease_lapses` | partition: the node's heartbeats are held while its `ext_write` body runs | the zombie's node stopped `unrenewed` 9 992 ms after its last lease extension (`self_stop_after` 10 s), and its host learned it from `node_stopped`; the survivor reaped it 9 766 ms after the hold and finished the turn with the operation `Interrupted` |
| `a_cleanly_stopped_node_hands_its_turn_over_at_once` | `stop` (the host shuts its core down) while the node holds the first model call | released 11 ms after the request, claimed 91 ms after the release |
| `a_postgres_restart_strands_no_work_and_reaps_no_node` | fast shutdown of the server for about 3 s while a model call is in flight | outage 3 141 ms; no node reaped or stopped; the turn finished on the node that held it, 2 838 ms after the server was back, `ext_write` once |

## Findings

1. **A host could not learn that its core's node stopped** (fixed in
   FIG-5193). A core's node stops on its own when its lease is lost or goes
   unrenewed, and the core keeps admitting work; nothing told the host, so a
   host whose node stopped could neither exit nor restart as a new boot.
   `LashCore::node_stopped` answers how the node stopped (law:
   `node_drain::a_host_learns_why_its_cores_node_stopped`); this runbook's
   node reports it.
The source mid-wait case asserted engine-emitted events, retired on main.
Its surviving wait, state and start-key guarantees are covered by
`crates/lash-postgres-workers/tests/failover.rs::a_process_killed_mid_wait_finishes_on_another_node_with_its_start_key_feed_and_state`.

## The PostgreSQL durability assumption (ADR 0132 §13)

As for the substrate runbook: one server is restarted with a fast shutdown,
which flushes every acknowledged commit; promotion, replicas and synchronous
replication are the host's and are not certified here.
