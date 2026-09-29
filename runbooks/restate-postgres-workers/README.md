# Restate Postgres Workers E2E

The full distributed harness runs with:

```sh
just restate-postgres-workers-e2e
```

Budget for a cold build the first time: the harness builds its own binaries with
`cargo build --locked --release -p lash-restate-postgres-workers-e2e --bins` on
plain Cargo, not through kiln, so it does not share the Bazel cache and a release
profile build is paid in full.

That starts Postgres (with the separate witness database described below), S3
(Garage), Restate, a mock OpenAI-compatible provider, two workers, the h2c
proxy, and the runner. Alongside the process, durable-wait,
frame-switch, and storage gates, the runner verifies first-party turn control:

- cancellation from the runner process with no Lash session handle;
- cancellation before the worker starts the addressed turn;
- first-writer-wins cancellation versus completion sealing;
- cancellation replayed by a peer after the original worker exits;
- terminal attachment returning the exact cancellation evidence; and
- Restate Admin invocation hard-kill remaining break-glass rather than
  manufacturing a Lash `Cancelled` terminal.

The final `turn-control gates passed:` line is the deterministic evidence for
the first five. The break-glass gate is **not** in that line: it prints its own
line from `control_scenarios.rs`, so score it separately and do not read a
present `turn-control gates passed:` line as covering it. `LASH_E2E_TURN_CONTROL_ONLY=1`
does not reach the break-glass gate either — `LASH_E2E_WORKFLOW_SEGMENT=2` is the
only selector that runs it. Three different vocabularies name these gates across
the runner, the harness and this README; match them by the assertion they make,
not by name.

Coverage is already scored in the runner, and this README said otherwise until
FIG-3171 checked it: `write_completed_workflow_manifest` compares the workflows
that actually reached a terminal result against the segment's slice of
`WORKFLOW_INVENTORY` and fails naming `missing=` and `unexpected=`, before it
writes anything. So a run that completed one workflow out of the inventory
cannot pass, and the emitted manifest is a record of that check rather than an
input to one — do not re-derive coverage by hand from it. CI then scores the
same thing a second time and across both legs:
`scripts/check_restate_workers_e2e_coverage.py` takes each leg's
`workflow-inventory.tsv` and manifest, refuses legs whose inventories differ,
reports `missing=`/`unexpected=` per segment, and requires the union of the two
manifests to equal the inventory exactly. Two things do still
need reading by eye, because neither is inventory-scored: the index gates and
the break-glass negative produce no terminal-result row and carry their own
assertions, and `WORKFLOW_INVENTORY` is itself pinned by
`EXPECTED_WORKFLOW_INVENTORY_LEN`, so a shrunk inventory fails the pin rather
than silently shrinking what coverage means. Session and turn IDs used by this test are routing identity,
not authorization. Production hosts must authorize callers before exposing the
same driver, and cancellation remains cooperative: detached effects are not
guaranteed to stop.

The runner prints timestamped `submitting`, `submitted`, and `completed`
progress for each workflow. If no workflow progress is reported for 240
seconds, it prints unfinished Restate invocations and recent worker events,
then exits so the shell harness can append per-service logs and process state.
Override that bound with `LASH_E2E_STALL_TIMEOUT_SECS` while debugging.

## Recovery laws with independent evidence (FIG-608)

The runner also checks four recovery laws, and it checks them only against the
witness ledgers: a second database, `lash_witness`, under its own account in the
same Postgres server. `witness.sql` creates it in plain SQL, and the script
applies it before any service starts. No Lash transaction, store adapter,
serializer or schema lifecycle touches these ledgers. The writer account can
only `INSERT` the columns it supplies and `SELECT`, so every ledger is
append-only for its writers. The database stamps every `recorded_at_us` from its
own clock and computes every digest over the bytes it received. The `lash_e2e_*`
tables in the `lash` database remain harness control and diagnostics only; the
laws never read them.

| Ledger | Written by | Records |
|---|---|---|
| `witness_submissions`, `witness_acknowledgements` | runner | the request before it is sent; the ingress acknowledgement after it returns |
| `witness_client_terminals` | runner | the exact bytes of `GET /restate/workflow/E2eTurnWorkflow/<id>/output`: `observed` when the workflow finishes, `reattached` after the restart |
| `witness_provider_receipts` | mock provider | every completion it serves; a failed receipt write fails the request |
| `witness_effect_attempts`, `witness_effect_commits`, `witness_effect_replies` | `batch_side_effect` | every physical attempt with its request digest, the idempotent receiver's one accepted commit per logical key, and the answer each attempt received |
| `witness_nemesis` | script, `crash_once`, `batch_side_effect` | `restart-begin`/`restart-complete`, `worker-exit`, `loss-after-commit` |

A failed witness write fails the tool call, the provider request or the runner
step. It is never logged and dropped.

The faults cover the commit/ack window:

- **Cluster restart (segment 2).** At the engine-restart handshake the script
  SIGKILLs both workers from outside, stops Restate, and starts all three again.
  The provider and the proxy stay up. The runner then reads every terminal it
  had already observed from the identical workflow address.
- **Loss after commit.** In `e2e-tool-batch-failover`, the `slow` call exits its
  worker once, after the receiver commits and before the reply is recorded or
  the tool returns. Restate must re-enter the closure, and the retry must find
  the first commit.
- **Replay over completed effects.** The same workflow's `crash_once` now runs
  after `Promise.all` settles, so the replay runs over effects that already
  completed. The existing one-side-effect-per-key and one-provider-call counts
  still hold.

`process_assertions.rs` holds the checkers as pure functions of the rows in one
read-only repeatable-read snapshot. Each law is a list of named rules:

1. **Durable-result stability and recovery progress.** One restart is
   witnessed. Every terminal observed before it is reattached after it with
   identical bytes. Each invocation that was acknowledged but unfinished at the
   restart reaches a client-observed terminal within 180 s of
   `restart-complete`.
2. **Logical-effect identity.** Each logical key keeps one parent and call id
   across retries, and the retries send identical request bytes. The receiver
   holds at most one commit per key, and every reply carries that commit's
   response. The commit names its accepted attempt. Every expected effect of a
   finished workflow has a receipt. The attempt lost after its commit was
   retried.
3. **Causal identity.** Every acknowledgement, terminal, provider receipt,
   attempt and commit names a submitted workflow. None is recorded before that
   submission, and no workflow was submitted as two different requests.
4. **Replay equivalence.** The worker exit came after the workflow's effects
   committed. No attempt, commit or reply for those effects appears after the
   exit. The client-visible terminal carries each committed response.

A rule reports either a violation or missing evidence. Missing evidence makes a
law **inconclusive**, and an inconclusive law fails the run exactly as a
violation does: an empty or partial snapshot never passes. Segment 2, and a
full run, claims all four laws. Segment 1 on its own, and
`LASH_E2E_TURN_CONTROL_ONLY` and `LASH_E2E_WAKE_RCA_ONLY`, claim law 3 alone,
because the restart and the witnessed batch belong to segment 2. A passing run
prints `recovery-law witness passed:` with the laws and the row counts. Law 5
(fencing and cancellation cut) is deferred: no incident supplies an
externally checkable authority token, and the checker must not read Lash's
store to decide an epoch.

`tests.rs` backs the checkers with fixtures:

- a generated legal history;
- one fixture per rule, each rejected by its law and accepted once that rule
  alone is removed (`check_law_except`), which proves every assertion is the
  one making its rejection test fail;
- one named rejecting fixture per law;
- incomplete-evidence cases that must return inconclusive;
- bounded property tests over 256 seeded shapes. Legal histories pass in any
  row order, every single fault is rejected by its law, and removing any
  required row never passes.

Run them without the distributed services:

```sh
kiln test //runbooks/restate-postgres-workers:test_batch
# or, with plain Cargo after sourcing env.sh:
cargo test -p lash-restate-postgres-workers-e2e --bin lash-e2e-runner
```

The package-level build/unit check is lighter and does not start the distributed
services:

```sh
kiln build //runbooks/restate-postgres-workers:all
kiln test //runbooks/restate-postgres-workers:test_batch
```

The focused parked-tool process-loss replay gate lives at the Restate endpoint
protocol seam, where it can splice the first worker incarnation's exact command
journal into a fresh handler incarnation deterministically:

```sh
kiln test //crates/lash-restate:lash-restate__unit_test \
  --test_arg=fig1126_pending_tool_redrives_after_worker_loss_and_resumes_once \
  --test_arg=--nocapture
```

That test parks a journaled pending tool on its completion key, discards the
first handler incarnation, resolves the captured key in the replay journal, and
asserts one tool launch and one terminal continuation. Keep this focused gate
alongside the distributed harness: the latter covers peer failover and durable
wait ingress, while the endpoint-protocol gate detects structural Restate
command mismatches directly.

## Deployment upgrades

Do not roll out worker code with `docker compose up -d --build worker` followed
by forced re-registration behind the existing deployment URI. That replaces
code while live invocations may replay against it and violates the
[ADR 0043 pin-and-drain contract](../../docs/adr/0043-hosts-register-immutable-deployments.md).
FIG-1126 changes the Restate command-journal shape at the start of every turn,
so an in-place rebuild can RT0016 immediately. Note that a *green* run of this
harness also logs dozens of `RT0016` journal mismatches: they come from the four
deliberate crash and failover workflows and are expected there. Do not treat the
presence of `RT0016` in the logs as evidence of an upgrade violation, and do not
treat its absence as evidence of a clean upgrade — read the deployment status,
not the log grep. Publish the rebuilt worker at a
new deployment URI, register that URI as a new deployment, keep the old worker
available until all of its invocations drain, and only then retire it.

The old deployment is not retired on an empty host-side queue guess. After
admission is closed and its in-flight work has settled, read the Lash-owned
authoritative status while the old deployment is still registered:

```rust
let status = old_core.drain_status(false).await?;
assert!(!status.accepting_new_work);
if status.drained {
    retire_old_deployment();
} else {
    // Keep the old deployment available for status.remaining_invocations.
}
```

`remaining_invocations` counts every retained non-terminal process row,
including suspended/waiting work and retrying work whose status remains
`running`. The read does not route, deadline, or retire the deployment; those
decisions remain with the host.

## Local Postgres Conformance

The Postgres store conformance tests require `LASH_POSTGRES_DATABASE_URL`.
Without it, the Postgres conformance binary reports a skip. To run the process
registry conformance locally without the full E2E stack:

```sh
scripts/ci/with-service.sh pg16 -- \
  cargo test -p lash-internal-postgres-store --locked --test conformance \
  process_registry_
```

`with-service.sh` owns the container: it binds an ephemeral loopback port
instead of a fixed one, exports `LASH_POSTGRES_DATABASE_URL` into the command,
labels the container so leftover-refusal can see it, and removes it on exit. Do
not hand-roll `docker run` with a fixed host port here — a fixed port collides
with a concurrent lane and an unlabelled container is invisible to the leftover
check. Each invocation gets a fresh database, which is what registry persistence
semantics need.
