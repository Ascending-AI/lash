# Restate Postgres Workers E2E

The full distributed harness runs with:

```sh
just restate-postgres-workers-e2e
```

That starts Postgres, MinIO, Restate, a mock OpenAI-compatible provider, two
workers, the h2c proxy, and the runner. Alongside the process, durable-wait,
frame-switch, and storage gates, the runner verifies first-party turn control:

- cancellation from the runner process with no Lash session handle;
- cancellation before the worker starts the addressed turn;
- first-writer-wins cancellation versus completion sealing;
- cancellation replayed by a peer after the original worker exits;
- terminal attachment returning the exact cancellation evidence; and
- Restate Admin invocation hard-kill remaining break-glass rather than
  manufacturing a Lash `Cancelled` terminal.

The final `turn-control gates passed:` line is the deterministic evidence for
those assertions. Session and turn IDs used by this test are routing identity,
not authorization. Production hosts must authorize callers before exposing the
same driver, and cancellation remains cooperative: detached effects are not
guaranteed to stop.

The runner prints timestamped `submitting`, `submitted`, and `completed`
progress for each workflow. If no workflow progress is reported for 240
seconds, it prints unfinished Restate invocations and recent worker events,
then exits so the shell harness can append per-service logs and process state.
Override that bound with `LASH_E2E_STALL_TIMEOUT_SECS` while debugging.

The package-level build/unit check is lighter and does not start the distributed
services:

```sh
cargo test -p lash-restate-postgres-workers-e2e --all-targets
```

The focused parked-tool process-loss replay gate lives at the Restate endpoint
protocol seam, where it can splice the first worker incarnation's exact command
journal into a fresh handler incarnation deterministically:

```sh
cargo test -p lash-restate \
  fig1126_pending_tool_redrives_after_worker_loss_and_resumes_once -- --nocapture
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
so an in-place rebuild can RT0016 immediately. Publish the rebuilt worker at a
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
docker run --rm --name lash-postgres-conformance \
  -e POSTGRES_PASSWORD=postgres \
  -e POSTGRES_DB=lash_conformance \
  -p 55432:5432 \
  -d postgres:16

LASH_POSTGRES_DATABASE_URL=postgres://postgres:postgres@localhost:55432/lash_conformance \
  cargo test -p lash-postgres-store --locked \
  postgres_process_registry_satisfies_conformance_when_configured

docker rm -f lash-postgres-conformance
```

Use a fresh database for each run when debugging registry persistence semantics.
