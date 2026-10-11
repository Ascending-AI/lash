# E2E Scenario: Example Core Shutdown

> **Read [../RULES.md](../RULES.md) first.** Its ownership, bounded polling,
> evidence, and teardown rules apply. This is a deterministic host-lifecycle
> runbook with a token-free Standard composer turn and no provider network call.

**Purpose.** Prove that each example host which successfully constructs a
`LashCore` awaits its installed plugin factories' shutdown before returning.
Process disappearance is insufficient: require a marker written and synced by
an installed `PluginFactory::shutdown` hook, plus trace and process-reap evidence.

## Deterministic companion

Run from your Lash Kiln fork through its integration gate:

```sh
. ./env.sh
LASH_HOST_SHUTDOWN_ARTIFACT_DIR=<artifact-dir> \
kiln gate lash <fork> -- bash scripts/example-core-shutdown-e2e.sh
```

The script resolves the `provider-wire-fixtures` feature target and launches
shipping-geometry hosts with `kiln run --config=judged`. The first host uses
`AGENT_WORKBENCH_PROTOCOL=standard` and
`AGENT_WORKBENCH_DEV_PROVIDER_SCENARIO=valid-empty-completion`. It requests provider-default reasoning with an empty `model_variant` in the
send. It submits one composer-equivalent `POST /api/turn`, captures the accepted turn's live `done` event,
and checks the committed empty reply and one completed terminal-observed attempt
with native `stop` and 7 input / 0 output tokens. Both HTTP streams are attached before the send, with successful response
headers recorded, and stay active until SIGTERM is sent to the owned host.
The second host uses `auth-failure-once` without sending a turn and encounters an
owned port conflict after constructing its core. Neither host calls OpenRouter.

The required scorecard rows are:

| Row | Required evidence |
| --- | --- |
| `workbench-signal-active-streams` | Active `/api/events` and `/api/observations` responses close on shutdown, Axum completes, and exactly one Workbench marker is durable. |
| `workbench-valid-empty-composer` | The accepted Standard turn completed with an empty committed reply and one successful 7/0 attempt before SIGTERM; the host produced exactly one marker. |
| `workbench-bind-error` | The post-build bind failure remains the primary error and still produces exactly one factory marker. |

Inspect `valid-empty-response.json`, `valid-empty-state.json`, the stream headers,
trace, host logs and `scorecard.tsv` in the artifact bundle. This turn belongs to
the installed host core, so completion itself produces no separate shutdown
marker. The Workbench signal closes its HTTP producer streams so Axum can finish
draining; the turn has already settled before that signal.

## Focused contract gate

Run the finite-owner and cleanup-result laws on their owning target once and
require **three executed tests and three passes** in Kiln's receipt:

```sh
. ./env.sh
kiln test --test_output=all //examples/toolbench:toolbench__unit_test \
  --test_arg=runtime::tests::wall_limit_retains_core_and_awaits_installed_factory_shutdown \
  --test_arg=runtime::tests::cleanup_failure_preserves_primary_failed_turn_evidence \
  --test_arg=runtime::tests::cleanup_failure_marks_successful_turn_failed
```

The zero-wall-limit law observes an installed factory's awaited shutdown. The
other two laws preserve a primary failure and mark a successful turn failed if
cleanup fails. Their provider remains local.

## Mutation control

When changing the shutdown contract itself, record the source checksum,
deliberately remove the `core.shutdown().await` call from the Workbench drain,
and rerun the bind-error case. Require failure because its marker is absent.
Restore the exact source, verify the checksum and retain the red log and patch.
Never publish the mutated tree.

## Abort and score

Abort on a failed empty-completion assertion, missing or duplicate marker, live
owned process after the bounded reap window, active NDJSON client after host
exit, provider network request, or mutation-restoration checksum mismatch.
Preserve the bundle and fill the three-row scorecard. Record tier `deterministic`,
model `scripted-provider`, cost $0 and the script's exit status. Forced kill and
panic are outside the awaited-shutdown guarantee.
