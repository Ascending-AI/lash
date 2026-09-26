# lash-restate

`lash-restate` adapts Lash's scoped effect-controller boundary to Restate
handlers. Use it inside a Restate service, object, or workflow handler and pass
the resulting `RestateRuntimeEffectController` (a `RuntimeEffectController`)
into Lash turn execution.

```rust,no_run
use lash_restate::RestateRuntimeEffectController;
use restate_sdk::prelude::*;

#[restate_sdk::workflow]
pub trait AgentTurnWorkflow {
    async fn run(req: Json<TurnRequest>) -> HandlerResult<Json<TurnResponse>>;
}

pub struct AgentTurnWorkflowImpl;

impl AgentTurnWorkflow for AgentTurnWorkflowImpl {
    async fn run(
        &self,
        ctx: WorkflowContext<'_>,
        Json(req): Json<TurnRequest>,
    ) -> HandlerResult<Json<TurnResponse>> {
        let effect_controller = RestateRuntimeEffectController::new(ctx, authority_id());
        let response = run_lash_turn(&effect_controller, req)
            .await
            .map_err(TerminalError::from_error)?;
        Ok(Json(response))
    }
}
```

The application owns `authority_id` (its `RestateEffectHost`'s authority id)
and `run_lash_turn`: open the `LashSession` from stable
request data and call
`session.turn(input).turn_id(turn_id).run_with_effects(&controller)`
for the Restate-backed turn. Restate recovery is handler replay with the same turn id
and request data, not a Lash-owned in-flight checkpoint reload.

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
journaled facts (ADR 0105 §3): durable waits and effect-group rank waits race
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
