# E2E Scenario: Example Core Shutdown

> **Read [../RULES.md](../RULES.md) first.** Its ownership, bounded polling,
> evidence, and teardown rules apply. This is a deterministic host-lifecycle
> runbook: it makes no provider request and opens no model turn.

**Purpose.** Prove that each example host which successfully constructs a
`LashCore` awaits its installed plugin factories' shutdown before returning.
Process disappearance is insufficient: the harness requires a marker written
and synced by an actual installed `PluginFactory::shutdown` hook, plus trace and
process-reap evidence.

## Deterministic companion

Run from a warm Lash orb through its integration gate:

```sh
. ./env.sh
orb gate lash mcp-host-shutdown-1165 -- \
  bash scripts/example-core-shutdown-e2e.sh
```

Set `LASH_HOST_SHUTDOWN_ARTIFACT_DIR` to retain the bundle at a chosen path.
The script boots shipping-geometry `--profile judged` hosts via `cargo run` and
uses only dummy or checked-in development-provider configuration. It never
sends a turn to OpenRouter or another provider.

The required scorecard rows are:

| Row | Required evidence |
| --- | --- |
| `agent-service-signal` | SIGTERM stops Axum intake, the process exits after its empty trace flush returns, and exactly one `agent-service` factory marker is durable. |
| `agent-service-bind-error` | A post-build bind failure remains the primary error and still produces exactly one factory marker. |
| `workbench-signal-active-streams` | Active `/api/events` and `/api/observations` responses close on host shutdown, Axum completes, and exactly one Workbench marker is durable. |
| `workbench-valid-empty-nested` | The token-free fixture returns only after its nested core produced exactly one marker. |
| `workbench-bind-error` | A post-build bind failure remains the primary error and still produces exactly one factory marker. |

The Workbench signal closes its host-owned HTTP producer streams so Axum can
finish draining. It does not cancel an active turn or introduce a durable-turn
policy.

Agent-service emits no trace record until a turn runs. These token-free rows
therefore require that its trace path remains absent and that the final
`shutdown complete` line is reached after the source-ordered flush succeeds;
they do not mislabel an empty sink as a persisted trace artifact. Workbench
emits a startup trace, so its rows require the trace file itself.

The owned Restate listener uses Restate SDK 0.11 `serve_with_cancel`. That API
stops listener intake and applies its fixed ten-second connection grace before
the listener task returns. It does not expose the accepted-connection task set,
so the listener join is not proof that every active handler completed. This
runbook claims stopped intake and completed SDK grace only.

## Focused and compile gates

Run one workspace feature-graph check covering the optional Restate and
valid-empty branches:

```sh
. ./env.sh
cargo check --workspace --all-targets --locked \
  --features agent-service/restate,agent-workbench/provider-wire-fixtures
```

Run the focused finite-owner and cleanup-result tests and require four executed
tests and four passes:

```sh
. ./env.sh
cargo nextest run --workspace --all-targets --locked \
  --features slack-clone/live-e2e \
  -E 'test(smoke_stream_timeout_drains_full_channel_before_factory_shutdown) | test(wall_limit_retains_core_and_awaits_installed_factory_shutdown) | test(cleanup_failure_preserves_primary_failed_turn_evidence) | test(cleanup_failure_marks_successful_turn_failed)'
```

The Slack test fills the real bounded activity channel, triggers the smoke
timeout, drains after existing process-local cancellation, joins the real
`TurnStream`, and then lets an installed shutdown factory run. The toolbench
test takes its real zero-wall-limit owner path and observes an installed
factory's awaited shutdown. Both providers remain local; do not execute the
paid live-model harness for this deterministic runbook.

## Mutation control

Once per contract change, record the source checksum, deliberately remove the
`core.shutdown().await` call from the agent-service drain, and rerun the
agent-service bind-error case. Require the command to fail because the marker
is absent. Restore the exact source, verify the checksum matches, and retain
the red log and patch. Never publish the mutated tree.

## Abort and score

Abort on a missing or duplicate marker, a live owned process after the bounded
reap window, an active Workbench NDJSON client after host exit, a provider
request, or a checksum mismatch after mutation restoration. Preserve the
artifact root and scorecard. Forced kill and panic are outside the awaited
shutdown guarantee.
