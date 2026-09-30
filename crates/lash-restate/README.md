# lash-restate

`lash-restate` is Lash's Restate engine. A deployment starts its endpoint from
`RestateEngine::endpoint_builder`, which binds Lash's own services beside the
host's: the `LashSession` virtual object drives each session and the `LashTurn`
workflow runs each root it admits, through a `RestateRuntimeEffectController`
(a `RuntimeEffectController`). A host handler never runs a turn (FIG-3600). It
sends and, where it waits, waits durably:

```rust,ignore
// Inside a service, workflow, or shared handler: `ctx` holds no exclusive lock.
let handle = session
    .send(lash::TurnInput::text(request.text))
    .id(request.turn_id)
    .accept_restate(&ctx)
    .await?;
let outcome = handle
    .outcome_restate(&ctx, lash::restate::RestateWait::new())
    .await?;
```

Serve the built endpoint with host-selected incoming message limits:

```rust,ignore
let limits = lash_restate::RestateEndpointLimits::new(
    32 * 1024 * 1024,
    32 * 1024 * 1024 + 8,
);
lash_restate::serve_endpoint(listener, endpoint, limits, shutdown).await;
```

The first limit counts one service-protocol message's payload. The second
counts its pending framed bytes, including the eight-byte header. The endpoint
checks each declared length before forwarding payload bytes to the SDK and
stops a refused input stream. It retains only a fixed header and counters beside
the SDK's bounded incomplete-message buffer. HTTP/2 flow control also limits
queued input. A replay may contain arbitrarily many legal messages; there is no
aggregate request limit. These limits belong to the deployment, as ADR 0025
requires, and do not impose a core-wide tool-result ceiling. The supplied hosts
choose 32 MiB per payload and eight additional bytes for framing.

Several deployments share one `restate-server` by namespace (ADR 0111):
`RestateConfig::with_namespace` prefixes every service name the engine binds or
calls (`alpha.LashSession`), and `RestateEngine::register_deployment` registers
the endpoint only when no other deployment holds those names. The default
namespace keeps the bare names.

`accept_restate` journals the input id before it accepts, so every replay of the
handler submits under the same id; `outcome_restate` follows the root in bounded,
journaled probes, so the wait survives suspension, replay, and a turn longer than
the invocation's timers. An exclusive object handler cannot wait for a root its
own object may serve: it accepts, returns the receipt, and a shared handler or
the caller waits.

The adapter records atomic Lash LLM calls, tool attempts, independent direct
completions, checkpoints, and execution-surface syncs with Restate
`ctx.run(...).name(lash:<replay_key>)`. A direct completion made by opaque tool
code runs inside the open atomic tool attempt. Composite tool-batch and
exec-code interpreters are rebuilt for every handler attempt; their nested
atomic effects retain stable replay keys. Runtime sleeps outside tool attempts
use Restate durable timers.
Upgrade note: invocations that journaled `ExecCode` under the pre-fix wrapping
will diverge on replay after upgrade; they were already panic-looping and need
an admin `KILL`.
Substrate-native Restate turns do not use store-side in-flight replay rows; Lash
only commits final session state through its turn-commit idempotency contract.
Replaying a handler with the same turn id returns Restate-recorded effect
outcomes, validates the current Lash envelope hash, and retries the final commit
without exposing partial session state.

For host-tier HTTP integration, use `RestateIngressClient` to submit `/send`
requests and capture the returned invocation id. The client accepts Restate's
`Accepted` and `PreviouslyAccepted` send statuses and returns the
`RestateInvocationId` from the response body, so the host can track a durable
turn invocation instead of modeling it as local in-process work.
`RestateAdminClient` cancels those active invocations through the Admin API,
queries invocation status, and exposes unfinished-invocation introspection for
tests and cleanup. `kill_invocation` stops an invocation for good; it is the
release half of an operator's cancel or fork of a parked root, run only after
the store recorded the root's end. The Restate CLI remains a
useful operator tool, but Lash tests and examples use these HTTP APIs directly.

Deterministic contract failures are terminal handler errors, not retry loops.
If a replayed recorded effect no longer matches the current Lash envelope hash,
or a previously recorded Restate run completed with a terminal failure, the
adapter returns an explicit terminal error code to the handler. Hosts should
surface that failure and clear their running state rather than leaving the
invocation to back off forever.

Lash's own Restate services — the durable-wait workflow and index, the
`LashProcessWorkflow` background tasks run on, process attach, and the
effect-group index, payload and dispatcher — are bound by lash, never by the
host. A deployment that serves lash work starts its endpoint from
`RestateEngine::endpoint_builder`, which binds every one of them, and binds
only its own services beside them:

```rust,no_run
use lash_restate::{RestateEngine, RestateProcessServing};
use restate_sdk::prelude::*;

fn endpoint(
    engine: &RestateEngine,
    // The process worker of the core built over `engine`.
    worker: lash_core::DurableProcessWorker,
) -> restate_sdk::endpoint::Endpoint {
    engine
        // A bare worker serves processes under the default segment policy;
        // `RestateProcessServing` sets an effect budget.
        .endpoint_builder(RestateProcessServing::new(worker))
        .bind(lash_restate::turn_service(
            AgentTurnWorkflowImpl.serve(),
            "run",
        ))
        .build()
}
```

Effect-group children route through the resolver registered on the backend's
effect host — the runtime's `ToolChildHost` once a core over the backend
installs it — so there is one resolver and one authority by construction. A
process that only submits work to Restate and serves no handlers does not call
`endpoint_builder`.

A missing binding is itself a deterministic contract failure, so it is treated
as one: Restate answers an invocation of a service no deployment binds with
`404`, and the adapter classifies that (`RestateHttpError::is_service_unregistered`)
and raises the engine's own `404`-class terminal naming the address nothing
binds. A retryable 404 would turn a forgotten `bind` into an invocation backing
off forever with no operator told what is wrong.

`RestateHttpError::classification` distinguishes transient ingress failures
from definitive answers. A process await reattaches after connection failures,
EOF, timeouts, overload, and ingress-generated 5xx responses, preserving the
durable process and its wait address. An invocation's terminal error stays
terminal even with a 5xx code. See ADR 0016.

An await of an already-terminal child journals the registry's full outcome in
one step, after acquiring the receiver's references to its stored attachments.
Replay reads that outcome even after retention prunes the child. The controller
also journals its cancellation and revocation observations. Terminal attachment
commands return the same observed value, so pending tool calls settle without
opening a wait. Non-terminal or cancelled children and closed control gates use the attach and durable-wait path.

## Stuck effect-group dispatcher retirement

Effect-group retirement tombstones the index before it cancels and durably
joins the adopted `EffectGroupDispatch` invocation. If that dispatcher's
endpoint is gone, the join deliberately remains pending: the tombstone prevents
new child execution, while the saga waits for a terminal it can prove.

Inspect the retired index cleanup and copy its exact `dispatcher.id`. Confirm
that the invocation is the `EffectGroupDispatch/run` execution for the affected
group key. Then terminate only that recorded invocation:

```text
restate invocation kill <dispatcher-invocation-id>
```

Do not kill by service name, wildcard, or process match. Once the exact
invocation is terminal, the saga's durable attach completes and engine redrive
continues child cancellation, all `3N+1` retained wait fences, payload-byte
deletion, and the final tombstone-only reduction. Verify that the index reports
`Retired`, a late READY or RANK registration resolves as `Retired`, and a late
payload put returns `Retired`.

The wait workflow owns Restate promises and durable deadline timers for every
Lash execution scope. The virtual-object index serializes wait registration,
session-wide cancellation, and permanent revocation during session deletion.
Deadline-bearing waits journal their absolute deadline once in the invoking
handler, then send deadline wire version 2 to the wait workflow. This preserves
one total time budget across worker replacement and keeps the replay-compared
nested call payload stable. The former unversioned `timeout_ms` request is
refused; drain deadline-bearing waits before upgrading. Requests without a
deadline retain their prior wire bytes and journal shape.
At turn start, Lash reads the cancellation gate through the handler-scoped
controller, so Restate journals the observation before any turn effect. A
pre-registered cancellation is therefore still observed before execution, and
handler replay reuses the original observation instead of branching on a later
out-of-band ingress result. After that, cancellation reaches a turn only as
journaled facts (ADR 0105 §3): durable waits and effect-group notices race
the turn's gate in the journal, the turn peeks the gate at its step
boundaries, and a model call's `ctx.run` body watches the gate itself and
records whether it was stopped. That watch, and a host-local stop forwarded to
the gate, are the only users of the deployment-level ingress controller;
nothing live races the handler.

The controller submits workflow `run` with workflow key
`ProcessRegistration.id` and sends cancellation to the workflow's shared
`cancel` handler. The workflow runner should be built from the host's
deployment config: plugin factories, runtime host config, session-store
factory, process registry, attachment store, and provider policy.
Process rows carry the process input plus `ProcessProvenance`: originator
and optional causal parent. Tool and Lashlang rows also carry a
captured execution-environment reference, so workers do not parse grant keys or
rebuild origin sessions to recover execution context.

`Call<T>` also checks JSON structure before the SDK constructs `T`. Its default
allowances are 32 MiB of encoded bytes, 1,000,000 nodes, depth 64, and 128 MiB of
estimated allocation bytes. Object keys and values each count as nodes; the
root has depth one. The estimate charges 64 bytes per node plus each string's
encoded content length. It estimates JSON storage, not arbitrary allocations a
custom deserializer may perform. The iterative preflight holds only counters,
then checks syntax without constructing a value tree. The wire probe skips the
body, so an unsupported range never materializes it. Direct callers can choose
all four allowances with `Call::decode_json_with_limits`; SDK ingress uses the
default allowances. Remote envelopes and turn inputs use the same preflight
through their `decode_json_with_limits` methods.
