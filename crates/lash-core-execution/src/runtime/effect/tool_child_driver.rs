//! The handler-level invocation driver for one tool child of a durable effect
//! group (ADR 0099 §2, FIG-2266).
//!
//! # What "handler level" means, and why it is not a detail
//!
//! ADR 0099 §2: "A tool child is a replayable invocation driver. Retry,
//! completion-key derivation, deferred await and the orchestrating lane are
//! *coordination* and run at handler level on the child's own admitted
//! controller. Only atomic attempts run inside recorded bodies." The structural
//! reason is ADR 0042's — "A recorded body must not emit commands into an
//! ordinal-addressed journal" — so a driver that lived inside a `ctx.run`
//! closure could not journal its second attempt, its retry sleep or its
//! deferred await, and a child expressed as a single
//! [`ToolAttempt`](super::envelope::RuntimeEffectCommand::ToolAttempt) could
//! not retry at all, because a second attempt is a second envelope with a
//! second hash.
//!
//! This module is therefore the body of a
//! [`ToolInvocation`](super::envelope::RuntimeEffectCommand::ToolInvocation)
//! child: it runs against the child's own admitted controller and emits that
//! child's attempts, retry sleeps and awaits as its own journal entries.
//!
//! # The one rebind site
//!
//! A child needs a [`ToolDispatchContext`], and that context is 24 fields of
//! three different provenances: what the deployment supplies, what the journal
//! recorded, and what only a live opener has. The live half cannot be
//! journaled — §3: "`RuntimeExecutionContext` is never serialized and there is
//! no second environment store … Semantic completion facts travel; live
//! channels do not" — so on the in-process tiers the child borrows its opener's
//! context through the [`LiveOpenerRegistry`](super::LiveOpenerRegistry) and
//! **rebinds** exactly the fields the request records.
//!
//! [`rebind_child_dispatch`] is that site, and it is deliberately the only one.
//! A child that ran under its opener's `session_id`, its opener's
//! `agent_frame_id`, its opener's catalog or — worst — its opener's effect
//! controller would be doing work under an authority it was never admitted
//! under, and every one of those is a field a caller could forget. One
//! function, one list, one test per line of that list.
//!
//! # What the driver does not do
//!
//! It does not take an [`IntentDrainGuard`](crate::tool_dispatch::IntentDrainGuard).
//! §5 replaces the in-process source-order gate with a durable per-group
//! final-commit order and is explicit that the gate's `Drop` discharge is
//! "wrong if copied into durable recovery". FIG-3409 owns that order; until it
//! lands the driver passes `None`, which means each child drains its own
//! declared intents in ADR 0042's intra-attempt source order and waits for no
//! sibling. See the module's `intent_drain_slot` note in
//! [`tool_child`](super::tool_child).
//!
//! It projects the child's result exactly once, at its own presentation
//! boundary: the session's plugin projector is a singleton lent through the
//! dispatch context, and the driver journals the resolved `ModelToolReturn` on
//! the settlement rather than leaving a `CompletedToolCall` for the opener to
//! derive. Incorporation consumes the record; it never re-projects, so a
//! changed projector environment on replay cannot change what the child
//! settled.

use std::sync::Arc;

use lash_sansio::core_support::ModelToolReturnCoreSupport;
use tokio_util::sync::CancellationToken;

use super::envelope::{RuntimeEffectCommand, RuntimeEffectEnvelope, RuntimeEffectOutcome};
use super::executor::EffectHost;
use super::executor::{
    RuntimeEffectControllerError, RuntimeEffectLocalExecutor, RuntimeEffectLocalRunner,
    ScopedEffectController,
};
use super::live_openers::{LiveOpenerContext, LiveOpenerRegistry};
use super::tool_child::ToolChildRequest;
use super::tool_settlement::{ToolSettlement, ToolUsageLedger};
use crate::tool_dispatch::{ToolCallLaunch, ToolDispatchContext, ToolDispatchOutcome};
use crate::{
    EffectOpener, ExecutionScope, ProcessExecutionEnvStore, ToolCatalog,
    ToolChildExecutionTraceHook,
};

/// The deployment wiring a tool child needs and its request deliberately does
/// not record (ADR 0099 §3, amendment 3).
///
/// One value per host, held by the resolver and handed to every child it
/// routes. Three things, and each is here because the request could not carry
/// it:
///
/// * the **live-opener registry**, which is how a child finds the live half of
///   its context, and whose `None` is the routing fact "not mine";
/// * the **controller**, which is what gives the child *its own* admitted
///   controller rather than its opener's lent one (§2);
/// * the **process-execution-environment store**, which is what turns the
///   recorded [`ProcessExecutionEnvRef`](crate::ProcessExecutionEnvRef) back
///   into the spec the child executes under.
#[derive(Clone)]
pub struct ToolChildHost {
    openers: Arc<LiveOpenerRegistry>,
    /// **Weak**, because this value is installed *on* the host: the host owns
    /// its controller, the controller owns this resolver, and a strong
    /// reference back would make the three a cycle no drop ever breaks. A
    /// resolver whose host is gone routes nothing, which is the honest answer.
    host: std::sync::Weak<dyn EffectHost>,
    process_env_store: Arc<dyn ProcessExecutionEnvStore>,
}

impl ToolChildHost {
    /// Wires tool-child routing for one effect host, with a fresh live-opener
    /// registry.
    ///
    /// The registry is per host and never static: two hosts in one process —
    /// which the conformance suites build routinely — must not see each other's
    /// openers, or a child would run against a deployment that never admitted
    /// it. Conversely there is exactly *one* per host, which is why
    /// [`EffectHost::install_tool_child_host`] is a get-or-init rather than a
    /// plain register: one host can back several runtimes, and two registries
    /// on one host would mean a turn registering its opener in one while the
    /// resolver consulted the other.
    #[must_use]
    pub fn new(
        host: &Arc<dyn EffectHost>,
        process_env_store: Arc<dyn ProcessExecutionEnvStore>,
    ) -> Arc<Self> {
        Arc::new(Self {
            openers: Arc::new(LiveOpenerRegistry::new()),
            host: Arc::downgrade(host),
            process_env_store,
        })
    }

    /// The registry the turn and process sites register their openers in.
    #[must_use]
    pub fn openers(&self) -> &Arc<LiveOpenerRegistry> {
        &self.openers
    }

    /// The child's own admitted controller (ADR 0099 §2).
    ///
    /// Built by the host for the child's *own* claim scope, never re-scoped
    /// from the opener's: an opener's controller carries the opener's
    /// retirement fence, and a child claimed under a different scope that
    /// inherited that fence would be refused — or admitted — for reasons that
    /// have nothing to do with it.
    fn child_controller(
        &self,
        scope: &ExecutionScope,
    ) -> Result<ScopedEffectController<'static>, RuntimeEffectControllerError> {
        let host = self.host.upgrade().ok_or_else(|| {
            RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::RuntimeEffectLocalExecutorUnavailable,
                "the effect host that routed this tool child is gone",
            )
        })?;
        host.scoped_static(scope.clone())
            .map_err(RuntimeEffectControllerError::from)?
            .ok_or_else(|| {
                RuntimeEffectControllerError::new(
                    crate::RuntimeErrorCode::RuntimeEffectLocalExecutorUnavailable,
                    "this effect host hands out no owned scoped controller, so it cannot give a \
                     group child the admitted controller that outlives its caller",
                )
            })
    }
}

impl std::fmt::Debug for ToolChildHost {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ToolChildHost")
            .field("openers", &self.openers)
            .finish_non_exhaustive()
    }
}

impl super::group_drain::GroupExecutors for ToolChildHost {
    /// Routes a tool child to this host's driver, and answers `None` for
    /// everything else.
    ///
    /// Two distinct `None`s, and the distinction is the point:
    ///
    /// * a command that is not a tool child — this resolver answers for tool
    ///   children and FIG-3397 extends it, so anything else is honestly "not
    ///   mine";
    /// * a tool child whose **opener is not live in this process** — the
    ///   routing fact the registry exists to report. The child is neither run
    ///   nor failed: it stays accepted, and the process whose opener is live —
    ///   or a later incarnation of this one — runs it. Failing instead would
    ///   turn "this worker cannot reach that opener" into a terminal the
    ///   journal keeps forever.
    fn executor_for(
        &self,
        envelope: &RuntimeEffectEnvelope,
    ) -> Option<RuntimeEffectLocalExecutor<'static>> {
        let RuntimeEffectCommand::ToolInvocation { request } = &envelope.command else {
            return None;
        };
        let live = self.openers.context_for(&request.scope.opener)?;
        Some(RuntimeEffectLocalExecutor::owned_runner(
            Box::new(ToolChildRunner {
                host: self.clone(),
                live,
            }),
            None,
        ))
    }
}

/// One routed child, waiting to be handed its envelope.
struct ToolChildRunner {
    host: ToolChildHost,
    live: LiveOpenerContext,
}

#[async_trait::async_trait]
impl RuntimeEffectLocalRunner for ToolChildRunner {
    async fn execute(
        self: Box<Self>,
        envelope: RuntimeEffectEnvelope,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        let RuntimeEffectCommand::ToolInvocation { request } = envelope.command else {
            return Err(RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::RuntimeEffectWrongOutcome,
                "the tool-child driver was handed an envelope that is not a tool invocation",
            ));
        };
        run_tool_child(&self.host, &self.live, &request, CancellationToken::new()).await
    }
}

/// The one place a lent opener context becomes a child's context.
///
/// Every override below is a recorded fact overriding a lent one, and every one
/// of them has a negative test that makes the lent value deliberately wrong and
/// asserts the child followed the request. **A child running under its opener's
/// authority instead of its own recorded authority is the failure this function
/// exists to make impossible.**
///
/// What is *not* overridden is everything else: the plugin session, the tool
/// provider and registries, the session services, the process service, the
/// trigger router, the process definitions and engines, the attachment store
/// and its source policy, the event sender, the turn context and the clock.
/// Those are deployment wiring and live channels, and §3 puts both on the lent
/// side of the split.
pub(crate) fn rebind_child_dispatch(
    lent: &ToolDispatchContext<'static>,
    request: &ToolChildRequest,
    controller: ScopedEffectController<'static>,
    execution_env_spec: crate::ProcessExecutionEnvSpec,
    usage_ledger: &ToolUsageLedger,
) -> ToolDispatchContext<'static> {
    let mut child = lent.clone();
    // A child may be attributed to a session the lending opener is not: a
    // process opener has no session of its own (ADR 0094) and still does tool
    // work that belongs to one.
    child.session_id = request.scope.session_id.clone();
    // One session holds many frames (ADR 0092), so a child that inherited the
    // opener's current frame would attribute its work to the wrong one.
    child.agent_frame_id = request.scope.agent_frame_id.clone();
    // Ruling 1: a reopen may not consult the live Tool Catalog. See
    // `admitted_catalog`.
    child.tool_catalog = Arc::new(admitted_catalog(lent, request));
    // Lineage is recorded, not the opener's current one.
    child.parent_invocation = request.attempt_identity.parent_invocation().cloned();
    // The environment the child was admitted under, resolved from its recorded
    // reference rather than inherited from whatever the opener is running now.
    child.execution_env_spec = execution_env_spec;
    // §2: the child's own admitted controller, never the lent one. This is the
    // authority boundary; everything else on this list is attribution.
    child.effect_controller = crate::runtime::RuntimeEffectControllerHandle::borrowed(controller);
    // Child-local buffers. Their contents ride the child's outcome (§6, §13),
    // so a child that wrote into the opener's buffers would put its facts
    // somewhere its settlement cannot carry them from.
    child.checkpoint_messages = crate::tool_dispatch::CheckpointMessageBuffer::default();
    child.trigger_outcomes = crate::tool_dispatch::ToolTriggerOutcomeBuffer::default();
    // The lent direct-completion client, with this child's usage ledger
    // installed. The opener's own ledger is untouched; this only *also* names
    // the spend as the child's, which §13 needs and an address space that is
    // not the opener's has no other way to report. Each attempt's runner
    // overlays a per-attempt sink on this, so the journaled attempt capture
    // attributes every spend to the attempt that made it and the coordinator
    // restores it here.
    child.direct_completions = lent
        .direct_completions
        .clone()
        .with_usage_ledger(usage_ledger.clone());
    child
}

/// The catalog a child is dispatched against: exactly its admitted manifest.
///
/// ADR 0099 §3 amendment 1: "An ungranted call pins its admitted manifest. A
/// reopen may not consult the live Tool Catalog … a tool whose retry policy or
/// argument projection changed between admission and recovery would otherwise
/// make a recovered child behave unlike the child that was admitted."
///
/// The manifest is always the recorded one. The **contract** beside it is not:
/// a contract is schemas and documentation for the tool's code, which §3
/// amendment 3 puts on the deployment-wiring side along with the code itself,
/// and it is read during *preparation* — which has already happened, since a
/// child carries a `PreparedToolCall`. So the live entry's contract is reused
/// when this deployment still has one, and a default stands in when it does
/// not; neither can change what the child does.
fn admitted_catalog(
    lent: &ToolDispatchContext<'static>,
    request: &ToolChildRequest,
) -> ToolCatalog {
    let manifest = request.admission.manifest().clone();
    let contract = lent
        .tool_catalog
        .tools
        .iter()
        .find(|entry| entry.manifest.id == manifest.id)
        .map(|entry| Arc::clone(&entry.contract))
        .unwrap_or_else(|| Arc::new(crate::ToolContract::default()));
    ToolCatalog::from_tool_definitions(vec![crate::ToolDefinition {
        manifest,
        contract: contract.as_ref().clone(),
    }])
}

/// Runs one tool child to a terminal and reports what it produced.
///
/// The ordering this provides, stated rather than assumed (§5, FIG-3409): a
/// child drains **its own** declared intents, in ADR 0042's intra-attempt
/// source order, and waits on no sibling. There is no cross-child order here
/// and none is invented — the group's rank order is the group's, and the
/// durable per-group commit order is FIG-3409's.
pub(crate) async fn run_tool_child(
    host: &ToolChildHost,
    live: &LiveOpenerContext,
    request: &ToolChildRequest,
    cancel: CancellationToken,
) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
    // Refused here as well as at decode: a request this build cannot
    // reconstruct completely is a child it would run under partial authority.
    request.validate()?;

    let execution_env_spec = crate::runtime::load_process_execution_env(
        host.process_env_store.as_ref(),
        &request.execution_env,
    )
    .await
    .map_err(|err| {
        RuntimeEffectControllerError::new(
            crate::RuntimeErrorCode::RuntimeEffectToolChildRequestVersion,
            format!(
                "tool child `{}` could not resolve its recorded execution environment `{}`: \
                 {err}; a child never invents an environment (ADR 0099 §3)",
                request.call.call_id, request.execution_env
            ),
        )
    })?;

    let controller = host.child_controller(&request.scope.admitted_scope)?;

    let usage_ledger = ToolUsageLedger::new();
    let dispatch = Arc::new(rebind_child_dispatch(
        live.dispatch().as_ref(),
        request,
        controller,
        execution_env_spec,
        &usage_ledger,
    ));

    let mut outcome = drive(&dispatch, request, cancel).await?;
    // Realized intent evidence moves into the settlement, where the opener
    // incorporates it as evidence. The journaled terminal keeps the record and
    // the declarations; the outcomes belong to the settlement channel.
    let intent_outcomes = std::mem::take(&mut outcome.intent_outcomes);
    let settlement = ToolSettlement {
        version: super::tool_settlement::TOOL_SETTLEMENT_VERSION,
        possession: started_processes(&intent_outcomes),
        model_return: resolve_model_return(&dispatch, request, &outcome, &intent_outcomes).await,
        intent_outcomes,
        triggers: dispatch.trigger_outcomes.drain(),
        checkpoint_messages: dispatch.checkpoint_messages.drain(),
        usage: usage_ledger.take(),
    };
    Ok(RuntimeEffectOutcome::ToolInvocation {
        outcome: Box::new(outcome),
        settlement: Box::new(settlement),
    })
}

/// Dispatches the child down the lane its admitted manifest names.
///
/// The orchestrating lane is a lane of coordination, not an attempt: ADR 0042
/// says `batch` and `spawn_agent` "have no `ToolAttempt` frame of their own",
/// so an orchestrating child runs its body directly and is classified by the
/// commands it issued, never by an invented outer attempt (§4).
async fn drive(
    dispatch: &Arc<ToolDispatchContext<'static>>,
    request: &ToolChildRequest,
    cancel: CancellationToken,
) -> Result<ToolDispatchOutcome, RuntimeEffectControllerError> {
    let tool_context = child_tool_context(dispatch, request, &cancel);
    if dispatch.is_orchestrating_tool(&request.call.tool_id) {
        return Ok(crate::tool_dispatch::execute_orchestrating_tool(
            dispatch.as_ref(),
            request.call.clone(),
            tool_context,
        )
        .await);
    }

    let turn_cancel_wait = child_turn_cancel_wait(dispatch, request, &cancel);
    let executor_context = tool_context.clone();
    let executor_dispatch = Arc::clone(dispatch);
    let coordinated = Box::pin(crate::tool_dispatch::coordinate_tool_invocation(
        dispatch.as_ref(),
        request.call.clone(),
        request.admission.grant().cloned().map(Box::new),
        request.admission.retry_policy(),
        Some(request.completion_routing),
        request.attempt_identity.clone(),
        &turn_cancel_wait,
        // §5: no cross-child gate here. See `run_tool_child`.
        None,
        None::<ToolChildExecutionTraceHook>,
        move |completion_key| {
            RuntimeEffectLocalExecutor::prepared_tool_attempt(
                Arc::clone(&executor_dispatch),
                executor_context.clone(),
                completion_key,
            )
        },
    ))
    .await;
    for trigger in coordinated.triggers {
        dispatch.trigger_outcomes.enqueue(trigger);
    }
    match coordinated.launch {
        ToolCallLaunch::Done(outcome) => Ok(*outcome),
        // Deferred completion is coordination and belongs at handler level
        // (§2), so the driver arms the resolver the call named and parks on its
        // own journaled await rather than handing a parked child back to a
        // group that can only read settlements.
        ToolCallLaunch::Pending(pending) => {
            Ok(await_child_completion(dispatch, request, *pending, &turn_cancel_wait).await)
        }
        // A refusal, not a settlement: a fabricated terminal here would journal
        // an outcome no effect ever produced.
        ToolCallLaunch::ControllerAborted(error) => Err(error),
    }
}

/// The tool context one child executes under.
///
/// `runtime_execution_context` is left unset, which is ADR 0099 §3's ruling and
/// the situation §6 quotes from `process_handles.rs`: a child holds no runtime
/// execution context, so the facts a context would have recorded are captured
/// into the child's outcome instead.
fn child_tool_context(
    dispatch: &Arc<ToolDispatchContext<'static>>,
    request: &ToolChildRequest,
    cancel: &CancellationToken,
) -> crate::ToolContext<'static> {
    let mut builder = crate::ToolContext::from_dispatch(Arc::clone(dispatch))
        .prepared_call(&request.call)
        .cancellation_token(Some(cancel.clone()))
        .parent_invocation(request.attempt_identity.parent_invocation().cloned());
    if let Some(process_ref) = request.enclosing_process.as_ref() {
        builder = builder.enclosing_process(Some(process_ref.process_id.clone()));
    }
    builder.build()
}

/// The cancellation trio a child waits under.
///
/// Observing when the child records a durable cancellation authority, and
/// unobserved when it does not — which is the one legal `None` on the request:
/// an opener whose controller participates in turn control locally has no
/// durable address to signal, so there is nothing for a child of it to observe.
fn child_turn_cancel_wait(
    dispatch: &Arc<ToolDispatchContext<'static>>,
    request: &ToolChildRequest,
    cancel: &CancellationToken,
) -> crate::runtime::TurnCancelWait {
    match request.cancellation_authority.as_ref() {
        Some(_) => crate::runtime::TurnCancelWait::observing(
            cancel.clone(),
            dispatch
                .effect_controller
                .scoped()
                .execution_scope()
                .clone(),
        ),
        None => crate::runtime::TurnCancelWait::unobserved(cancel.clone()),
    }
}

/// Arms the resolver the parked call named, then parks on the child's own
/// journaled await.
///
/// The arming runs before the park and on every redrive, for the reason the
/// session path states: the recorded attempt body that named the resolver does
/// not re-run, so nothing else would arm it.
async fn await_child_completion(
    dispatch: &Arc<ToolDispatchContext<'static>>,
    request: &ToolChildRequest,
    pending: crate::tool_dispatch::PendingToolDispatchOutcome,
    turn_cancel_wait: &crate::runtime::TurnCancelWait,
) -> ToolDispatchOutcome {
    if let Err(error) = crate::tool_dispatch::arm_pending_resolver(
        dispatch.processes.as_ref(),
        &pending.pending,
        &pending.key,
        dispatch.process_scope(),
    )
    .await
    {
        return unarmed_child_outcome(pending, &error.to_string());
    }
    let Some(invocation) = child_await_invocation(dispatch, request) else {
        return unarmed_child_outcome(
            pending,
            "the child's recorded attempt identity names no invocation to hang an await on",
        );
    };
    let resolver = pending.pending.resolved_by.clone();
    let deadline = pending
        .pending
        .deadline
        .map(|duration| dispatch.clock.now() + duration);
    let outcome = dispatch
        .effect_controller
        .scoped()
        .execute_effect(
            RuntimeEffectEnvelope::new(
                invocation,
                RuntimeEffectCommand::AwaitEvent {
                    key: pending.key.clone(),
                },
            ),
            RuntimeEffectLocalExecutor::await_event_under(
                turn_cancel_wait,
                deadline,
                Arc::clone(&dispatch.clock),
            ),
        )
        .await;
    let resolution = match outcome.and_then(RuntimeEffectOutcome::into_await_event) {
        Ok(resolution) => resolution,
        Err(error) => return failed_child_outcome(pending, &error.to_string()),
    };
    crate::tool_dispatch::settle_completed_pending_tool_call(
        dispatch.as_ref(),
        pending.tool_name,
        pending.args,
        resolution,
        resolver.as_ref(),
        pending.duration_ms,
        pending.attempts,
    )
    .await
}

/// The invocation the child's await is journaled under.
///
/// Derived from the child's *recorded* attempt identity, so a redrive re-derives
/// the same replay key rather than a fresh one — the same reconstruction
/// property the request's module documentation claims for the attempts
/// themselves.
fn child_await_invocation(
    dispatch: &Arc<ToolDispatchContext<'static>>,
    request: &ToolChildRequest,
) -> Option<crate::RuntimeEffectInvocation> {
    let parent = request.attempt_identity.parent_invocation()?;
    let suffix = format!("{}:await", request.call.call_id);
    let parent_effect_id = parent.effect_id().unwrap_or("tool").to_string();
    Some(crate::runtime::causal::child_effect_invocation(
        dispatch.effect_controller.scoped().execution_scope(),
        parent,
        format!("{parent_effect_id}:{suffix}"),
        crate::RuntimeEffectKind::AwaitEvent,
        suffix,
    ))
}

/// A wait nobody will resolve is a failure, never a park.
fn unarmed_child_outcome(
    pending: crate::tool_dispatch::PendingToolDispatchOutcome,
    reason: &str,
) -> ToolDispatchOutcome {
    ToolDispatchOutcome {
        record: crate::ToolCallRecord {
            call_id: None,
            tool: pending.tool_name,
            args: pending.args,
            output: crate::ToolCallOutput::failure(crate::ToolFailure::runtime(
                crate::ToolFailureClass::Internal,
                "pending_tool_resolver_unarmed",
                format!("the declared resolver for this group child could not be armed: {reason}"),
            )),
            duration_ms: pending.duration_ms,
        },
        attempts: pending.attempts,
        intents: crate::ToolIntents::default(),
        intent_outcomes: Vec::new(),
    }
}

/// The child's await itself failed, which is the child's terminal.
fn failed_child_outcome(
    pending: crate::tool_dispatch::PendingToolDispatchOutcome,
    reason: &str,
) -> ToolDispatchOutcome {
    ToolDispatchOutcome {
        record: crate::ToolCallRecord {
            call_id: None,
            tool: pending.tool_name,
            args: pending.args,
            output: crate::ToolCallOutput::failure(crate::ToolFailure::runtime(
                crate::ToolFailureClass::Internal,
                "pending_tool_completion_failed",
                reason.to_string(),
            )),
            duration_ms: pending.duration_ms,
        },
        attempts: pending.attempts,
        intents: crate::ToolIntents::default(),
        intent_outcomes: Vec::new(),
    }
}

/// The child's presentation boundary: the singleton plugin projector, run once
/// over the settled outcome, with attachment notices computed under the child's
/// *recorded* environment (ADR 0099 §3 — a reopen uses the recorded facts).
///
/// The resolved return is journaled on the settlement so incorporation consumes
/// a record instead of re-running the projector: a projector that changed
/// between execution and replay cannot change what the child settled. A
/// projector *error* resolves to the same recorded fallback the session path
/// uses, so a broken projector settles a refusal rather than aborting the
/// settlement.
async fn resolve_model_return(
    dispatch: &ToolDispatchContext<'_>,
    request: &ToolChildRequest,
    outcome: &ToolDispatchOutcome,
    intent_outcomes: &[crate::ToolIntentExecutionOutcome],
) -> crate::ModelToolReturn {
    let mut model_return = match dispatch
        .plugins
        .project_tool_result(crate::plugin::ToolResultProjectionContext {
            session_id: dispatch.session_id.clone(),
            call_id: request.call.call_id.clone(),
            tool_name: outcome.record.tool.clone(),
            args: outcome.record.args.clone(),
            output: outcome.record.output.clone(),
            duration_ms: outcome.record.duration_ms,
        })
        .await
    {
        Ok(projected) => projected,
        Err(error) => crate::ModelToolReturn::text(
            request.call.call_id.clone(),
            outcome.record.tool.clone(),
            error.to_string(),
        ),
    };
    crate::session::tool_execution::surface_attachment_materialization_notices(
        &dispatch
            .execution_env_spec
            .policy
            .model
            .capability
            .attachment_acceptance,
        &outcome.record.output,
        &mut model_return,
    );
    // The same addenda the session path appends in `complete_tool_call`: the
    // realized intents are part of the presentation the model sees, so the
    // recorded return carries them rather than leaving incorporation to
    // recompute them.
    for intent_outcome in intent_outcomes {
        model_return.parts.push(crate::ModelToolReturnPart::text(
            intent_outcome.model_addendum(),
        ));
    }
    model_return
}

/// Possession, read out of the same realized outcome the bound value's
/// projection is taken from (ADR 0099 §6).
///
/// Deliberately the realized *intent outcome*, not a side channel: that is what
/// `record_processes_started_by_intents` reads for an in-turn call, and a
/// possession set assembled from anywhere else could name a process the child's
/// own result never bound.
fn started_processes(
    intent_outcomes: &[crate::ToolIntentExecutionOutcome],
) -> Vec<crate::ProcessId> {
    intent_outcomes
        .iter()
        .filter_map(|intent| match intent {
            crate::ToolIntentExecutionOutcome::Executed { kind, result, .. }
                if *kind == crate::ToolIntentKind::StartProcess =>
            {
                crate::ProcessRef::from_handle_json(result)
                    .ok()
                    .map(|process_ref| process_ref.process_id)
            }
            _ => None,
        })
        .collect()
}

/// The opener a turn's execution scope names, or `None` when the scope is not
/// one an opener can be derived from.
///
/// **One function**, because a scope grows arms: a queued drain runs its whole
/// effect tree under [`ExecutionScope::QueueDrain`], and a registration site
/// that constructed `EffectOpener::turn(..)` by hand would silently register
/// nothing for it. Everything that registers a turn-side opener goes through
/// here, and the match is exhaustive so a new scope arm is a compile error
/// rather than a silently unregistered opener.
#[must_use]
pub fn opener_for_execution_scope(scope: &ExecutionScope) -> Option<EffectOpener> {
    match scope {
        ExecutionScope::Turn {
            session_id,
            turn_id,
        } => Some(EffectOpener::turn(session_id.clone(), turn_id.clone())),
        // A process opener is `Process(ProcessRef)` and carries a store-minted
        // incarnation that `ExecutionScope::Process` does not have (§1), so a
        // process opener is registered at the process site from its admitted
        // incarnation and never derived from a scope.
        ExecutionScope::Process { .. }
        // Not openers: a queued drain has no turn identity of its own today, a
        // session delete opens no tool work, and a runtime operation is not a
        // session's turn.
        | ExecutionScope::QueueDrain { .. }
        | ExecutionScope::SessionDelete { .. }
        | ExecutionScope::RuntimeOperation { .. } => None,
    }
}

#[cfg(test)]
#[path = "tool_child_driver/tests.rs"]
mod tests;
