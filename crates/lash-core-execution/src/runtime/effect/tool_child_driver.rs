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
//! child: it runs against the child's own admitted controller — bound to the
//! child's recorded identity through
//! [`GroupChildBinding`](crate::GroupChildBinding) so every admission it
//! serves arbitrates under the child's own §4 decision — and emits that
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
//! "wrong if copied into durable recovery". The child therefore passes
//! `None`: §4 commits its final at the attempt boundary and §5 admits its
//! drain by the recorded `commit_seq`, which orders it against every sibling
//! without an in-process slot.
//!
//! It projects the child's result exactly once, at its own presentation
//! boundary: the session's ordered presentation steps run once through the
//! journaled `PresentToolResult` effect under the child's bound controller,
//! and the driver journals the resolved `ModelToolReturn` on the settlement
//! rather than leaving a `CompletedToolCall` for the opener to derive.
//! Incorporation consumes the record; it never re-presents, so a changed
//! presentation environment on replay cannot change what the child settled.

use std::sync::Arc;

use lash_sansio::core_support::ModelToolReturnCoreSupport as _;
use lash_sansio::sync::MutexExt;
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
    AdmittedScope, EffectOpener, ProcessExecutionEnvStore, ToolCatalog, ToolChildExecutionTraceHook,
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
    /// The clock a `Sleep`/`AwaitEvent` group child waits on. Mutable because
    /// the host is installed — get-or-init — before the runtime's configured
    /// clock is known (`RuntimeHostConfig::with_clock` follows `new`), and a
    /// second install cannot replace it.
    clock: Arc<std::sync::Mutex<Arc<dyn crate::Clock>>>,
}

impl ToolChildHost {
    /// Wires tool-child routing for one effect host, with a fresh live-opener
    /// registry and the system clock.
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
            clock: Arc::new(std::sync::Mutex::new(Arc::new(crate::SystemClock))),
        })
    }

    /// Sets the clock the host's `Sleep`/`AwaitEvent` child executors wait on,
    /// in place, and returns the same host.
    ///
    /// In place rather than rebuilding: the installed host is shared through
    /// the controller's registered resolver, so a rebuilt host with a fresh
    /// registry would strand the openers already registered on this one.
    pub fn with_clock(self: &Arc<Self>, clock: Arc<dyn crate::Clock>) -> Arc<Self> {
        *self.clock.lock_recover() = clock;
        Arc::clone(self)
    }

    /// The registry the turn and process sites register their openers in.
    #[must_use]
    pub fn openers(&self) -> &Arc<LiveOpenerRegistry> {
        &self.openers
    }

    /// The host this resolver routes for, or the routing fact "gone".
    fn effect_host(&self) -> Result<Arc<dyn EffectHost>, RuntimeEffectControllerError> {
        self.host.upgrade().ok_or_else(|| {
            RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::RuntimeEffectLocalExecutorUnavailable,
                "the effect host that routed this tool child is gone",
            )
        })
    }

    /// The child's own admitted controller, bound to the child's recorded
    /// identity (ADR 0099 §2, §4; FIG-3470).
    ///
    /// Built by the host for the child's *own* claim, never re-scoped from
    /// the opener's: an opener's controller carries the opener's retirement
    /// fence, and a child claimed under a different scope that inherited that
    /// fence would be refused — or admitted — for reasons that have nothing
    /// to do with it. `admitted` is the request's recorded
    /// [`AdmittedScope`]: the claim *and* its pin, one checked pair, so a
    /// process claim reaches the controller with the incarnation it was
    /// admitted under rather than whatever process carries the name now.
    ///
    /// `binding` carries the child's envelope address and retained
    /// membership — one journaled fact — and the returned controller mints
    /// every nested semantic admission *under* it, fenced by the substrate's
    /// own arbitration for that child rather than by any `caused_by` lineage
    /// an envelope happens to carry.
    fn child_controller(
        &self,
        admitted: &AdmittedScope,
        binding: crate::GroupChildBinding,
    ) -> Result<ScopedEffectController<'static>, RuntimeEffectControllerError> {
        self.effect_host()?
            .scoped_for_group_child(admitted.clone(), binding)
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

/// The [`GroupChildBinding`](crate::GroupChildBinding) a tool-child envelope
/// carries: the child's own `ToolInvocation` address plus its retained
/// membership, one journaled pair (ADR 0099 §4, FIG-3470). A `ToolInvocation`
/// without `envelope.group` is a shape error — such an envelope was never
/// recorded as a group child, and a child without retained membership is
/// refused rather than run unbound.
fn envelope_group_child_binding(
    envelope: &RuntimeEffectEnvelope,
) -> Result<crate::GroupChildBinding, RuntimeEffectControllerError> {
    let Some(membership) = envelope.group.as_deref().cloned() else {
        return Err(RuntimeEffectControllerError::new(
            crate::RuntimeErrorCode::RuntimeEffectGroupShape,
            "a tool child without retained group membership has no child \
             identity to bind a controller to and is never run unbound",
        ));
    };
    Ok(crate::GroupChildBinding {
        child: envelope.invocation.address.clone(),
        membership,
    })
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
    /// * a command that is not a group child this resolver runs — tool
    ///   children route to the driver, `Sleep`/`AwaitEvent` children (FIG-3397)
    ///   route to the in-process wait executors, and anything else is honestly
    ///   "not mine";
    /// * a tool child whose **opener is not live in this process** — the
    ///   routing fact the registry exists to report. The child is neither run
    ///   nor failed: it stays accepted, and the process whose opener is live —
    ///   or a later incarnation of this one — runs it. Failing instead would
    ///   turn "this worker cannot reach that opener" into a terminal the
    ///   journal keeps forever.
    ///
    /// The `Sleep`/`AwaitEvent` executors are built with turn-cancel
    /// observation off: a group child's cancellation is the group's — the
    /// group dispatch passes its own token into `execute_effect_cancellable`
    /// (ADR 0099 §4) — and the wait deadline the group stamps at admission is
    /// PR C's, when a product producer of such children exists.
    fn executor_for(
        &self,
        envelope: &RuntimeEffectEnvelope,
    ) -> Option<RuntimeEffectLocalExecutor<'static>> {
        match &envelope.command {
            RuntimeEffectCommand::ToolInvocation { request } => {
                let live = self.openers.context_for(&request.scope.opener)?;
                Some(RuntimeEffectLocalExecutor::owned_runner(
                    Box::new(ToolChildRunner {
                        host: self.clone(),
                        live,
                    }),
                    None,
                ))
            }
            // A group child's cancellation is the group's (ADR 0099 §4: the
            // group dispatch passes its own token into the child's
            // `execute_effect_cancellable`), so turn-cancel observation is off
            // on both arms. The §11 deadline-at-admission rule and the
            // durable-wait deadline are PR C's, when a product producer of
            // such children exists.
            RuntimeEffectCommand::Sleep { .. } => Some(
                RuntimeEffectLocalExecutor::sleep_with_clock(
                    CancellationToken::new(),
                    self.clock.lock_recover().clone(),
                )
                .with_turn_cancel_observation(false),
            ),
            // Wait *options*, not a self-running leaf: the tier's own dispatch
            // — the store driver's `execute_effect_cancellable` arm, the native
            // controller's `execute_effect` arm — is what reads them and waits
            // on its await-event registry.
            RuntimeEffectCommand::AwaitEvent { .. } => Some(
                RuntimeEffectLocalExecutor::await_event_with_clock(
                    CancellationToken::new(),
                    None,
                    self.clock.lock_recover().clone(),
                )
                .with_turn_cancel_observation(false),
            ),
            _ => None,
        }
    }

    fn live_generation(&self, opener: &crate::EffectOpener) -> Option<u64> {
        self.openers.generation_of(opener)
    }
}

#[cfg(feature = "testing")]
impl ToolChildHost {
    /// A testing seam (FIG-3429): a runner bound to `context` at *resolution*
    /// time — the shape an authority leak takes when the group-open selector
    /// hands a retained child whatever the reoffering successor staged under
    /// its replay key.
    ///
    /// [`ToolChildRunner`] deliberately does not do this: it re-derives the
    /// live context from the envelope's recorded opener at the execution
    /// boundary, which is the binding the differential exists to prove.
    /// Staging this runner for an offered child and letting the `KeyOnly`
    /// offered-child selection reuse it is how the oracle demonstrates what
    /// the retained-envelope check prevents: the retained request's work
    /// executing under the *stager's* plugins, provider, completions,
    /// processes and cancellation token.
    pub fn executor_bound_to(
        &self,
        context: LiveOpenerContext,
    ) -> RuntimeEffectLocalExecutor<'static> {
        RuntimeEffectLocalExecutor::owned_runner(
            Box::new(BoundToolChildRunner {
                host: self.clone(),
                context,
            }),
            None,
        )
    }
}

/// The resolution-bound twin of [`ToolChildRunner`], testing-only: the live
/// context is captured when the executor is minted rather than re-derived
/// from the envelope's recorded opener at execution. That is precisely the
/// leak — "which context" answered at resolution instead of "may this runner
/// serve this request" answered at execution — so nothing outside
/// `#[cfg(feature = "testing")]` may build one.
#[cfg(feature = "testing")]
struct BoundToolChildRunner {
    host: ToolChildHost,
    context: LiveOpenerContext,
}

#[cfg(feature = "testing")]
#[async_trait::async_trait]
impl RuntimeEffectLocalRunner for BoundToolChildRunner {
    async fn execute(
        self: Box<Self>,
        envelope: RuntimeEffectEnvelope,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        let binding = envelope_group_child_binding(&envelope)?;
        let RuntimeEffectCommand::ToolInvocation { request } = envelope.command else {
            return Err(RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::RuntimeEffectWrongOutcome,
                "the tool-child driver was handed an envelope that is not a tool invocation",
            ));
        };
        Box::pin(run_tool_child(
            &self.host,
            &self.context,
            &request,
            envelope.invocation.address.clone(),
            self.host
                .child_controller(&request.scope.admitted_scope, binding)?,
            self.context.cancellation().child_token(),
        ))
        .await
    }
}

/// One routed child, waiting to be handed its envelope.
///
/// The runner owns the exact [`LiveOpenerContext`] `executor_for` resolved —
/// the journaled request was admitted against that context, so execution uses
/// it rather than asking the registry again: an opener guard dropped between
/// resolution and execution must not stall the child on a re-registration
/// nothing guarantees, nor rebind it to a successor opener's context.
struct ToolChildRunner {
    host: ToolChildHost,
    live: LiveOpenerContext,
}

#[async_trait::async_trait]
impl RuntimeEffectLocalRunner for ToolChildRunner {
    fn tool_child_driver(&self) -> Option<&dyn ToolChildDriver> {
        Some(self)
    }

    async fn execute(
        self: Box<Self>,
        envelope: RuntimeEffectEnvelope,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        // Boxed: the driver future carries the whole dispatch, and a group
        // child is spawned per member — 21 kB of stack per pending child is a
        // real cost, not a lint's taste.
        let binding = envelope_group_child_binding(&envelope)?;
        let RuntimeEffectCommand::ToolInvocation { request } = envelope.command else {
            return Err(RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::RuntimeEffectWrongOutcome,
                "the tool-child driver was handed an envelope that is not a tool invocation",
            ));
        };
        Box::pin(run_tool_child(
            &self.host,
            &self.live,
            &request,
            envelope.invocation.address.clone(),
            self.host
                .child_controller(&request.scope.admitted_scope, binding)?,
            self.live.cancellation().child_token(),
        ))
        .await
    }
}

/// The handler-level driving seam a tool child's resolver hands to a tier that
/// supplies the admitted controller itself (ADR 0099 §2, FIG-2266).
///
/// On the in-process tiers the runner is self-contained: `execute` builds the
/// child's controller from the host and runs to a terminal in one call. A tier
/// whose admitted controller is bound to a live handler context — Restate,
/// where only a `ctx`-bound controller journals steps in the child's own
/// invocation — cannot use that shape, so it resolves the same runner through
/// `executor_for` and calls [`drive`](Self::drive) with the controller *it*
/// built. The driver inside is identical either way: the controller is the
/// only tier-specific input.
///
/// `controller` is deliberately a borrow-bounded [`ScopedEffectController`]
/// rather than the `'static` one the in-process path builds: a handler-bound
/// controller is valid exactly as long as the handler drives it.
#[async_trait::async_trait]
pub trait ToolChildDriver: Send {
    /// Runs the child to a terminal on `controller`, returning its settlement
    /// outcome. `child` is the child's own `ToolInvocation` envelope address —
    /// the replay row its §4 final commits against (ADR 0099 §4). Cancellation
    /// comes from the captured live opener's token, the same parent the
    /// in-process `execute` mints the child token from.
    async fn drive<'run>(
        &self,
        request: &ToolChildRequest,
        child: crate::EffectAddress,
        controller: ScopedEffectController<'run>,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError>;
}

#[async_trait::async_trait]
impl ToolChildDriver for ToolChildRunner {
    async fn drive<'run>(
        &self,
        request: &ToolChildRequest,
        child: crate::EffectAddress,
        controller: ScopedEffectController<'run>,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        Box::pin(run_tool_child(
            &self.host,
            &self.live,
            request,
            child,
            controller,
            self.live.cancellation().child_token(),
        ))
        .await
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
pub(crate) fn rebind_child_dispatch<'run>(
    lent: &ToolDispatchContext<'static>,
    request: &ToolChildRequest,
    controller: ScopedEffectController<'run>,
    execution_env_spec: crate::ProcessExecutionEnvSpec,
    usage_ledger: &ToolUsageLedger,
) -> Result<ToolDispatchContext<'run>, RuntimeEffectControllerError> {
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
    child.execution_env_spec = execution_env_spec.clone();
    // §2: the child's own admitted controller, never the lent one — bound to
    // the child's recorded identity (§4), so a nested admission minted after
    // the child's cancel decision commits refuses at the substrate. This is
    // the authority boundary; everything else on this list is attribution.
    child.effect_controller =
        crate::runtime::RuntimeEffectControllerHandle::borrowed(controller.clone());
    // Child-local buffers. Their contents ride the child's outcome (§6, §13),
    // so a child that wrote into the opener's buffers would put its facts
    // somewhere its settlement cannot carry them from.
    child.checkpoint_messages = crate::tool_dispatch::CheckpointMessageBuffer::default();
    child.trigger_outcomes = crate::tool_dispatch::ToolTriggerOutcomeBuffer::default();
    // The lent direct-completion client, rebound to the child's recorded
    // authority. What is lent is the live completion *transport*; what is
    // rebound is everything that decides whose call it is — the recorded
    // session, environment, lineage, admitted controller and usage ledger —
    // so a managed-LLM call the child makes is journaled under the child's
    // facts, never the opener's (ADR 0099 §3, §13).
    child.direct_completions = lent.direct_completions.bind_tool_child(
        &request.scope.session_id,
        &execution_env_spec,
        crate::runtime::RuntimeEffectControllerHandle::borrowed(controller),
        request
            .attempt_identity
            .parent_invocation()
            .and_then(|parent| parent.attribution.turn_id.clone()),
        request.attempt_identity.parent_invocation().cloned(),
        usage_ledger.clone(),
    )?;
    Ok(child)
}

/// The catalog a child is dispatched against: its admitted manifest pinned at
/// its own id, plus the lent live entries for every other id.
///
/// ADR 0099 §3 amendment 1: "An ungranted call pins its admitted manifest. A
/// reopen may not consult the live Tool Catalog" *for it* — a tool whose
/// retry policy or argument projection changed between admission and
/// recovery would otherwise make a recovered child behave unlike the child
/// that was admitted. The ruling binds the *recorded call*: at this call's
/// id the catalog answers with the recorded manifest, whatever the live
/// deployment now says — a changed or removed live entry cannot alter what
/// the child was admitted to do.
///
/// The other ids are the live catalog, lent unchanged, because a call the
/// child's orchestrating body issues is a *fresh admission*, not a retained
/// fact: §3 has nothing recorded to prefer for it, and what admits a new
/// call at body runtime is what admits any live call — the deployment's
/// catalog at that moment.
///
/// For the recorded manifest the **contract** beside it is not recorded
/// either: a contract is schemas and documentation for the tool's code, which
/// §3 amendment 3 puts on the deployment-wiring side along with the code
/// itself, and it is read during *preparation* — which has already happened,
/// since a child carries a `PreparedToolCall`. So the live entry's contract
/// is reused when this deployment still has one, and a default stands in when
/// it does not; neither can change what the child does.
fn admitted_catalog(
    lent: &ToolDispatchContext<'static>,
    request: &ToolChildRequest,
) -> ToolCatalog {
    let manifest = request.admission.manifest().clone();
    let recorded_contract = lent
        .tool_catalog
        .tools
        .iter()
        .find(|entry| entry.manifest.id == manifest.id)
        .map(|entry| entry.contract.as_ref().clone())
        .unwrap_or_default();
    let definitions = std::iter::once(crate::ToolDefinition {
        manifest: manifest.clone(),
        contract: recorded_contract,
    })
    .chain(
        lent.tool_catalog
            .tools
            .iter()
            .filter(|entry| entry.manifest.id != manifest.id)
            .map(|entry| crate::ToolDefinition {
                manifest: entry.manifest.clone(),
                contract: entry.contract.as_ref().clone(),
            }),
    )
    .collect();
    ToolCatalog::from_tool_definitions(definitions)
}

/// Runs one tool child to a terminal and reports what it produced.
///
/// The ordering this provides, stated rather than assumed (§4/§5, FIG-3409):
/// a child takes no in-process slot because the durable group owns the order —
/// its final commits at the attempt's terminal boundary against its own
/// replay row (`child`), and its drain is admitted by the recorded
/// `commit_seq` barrier before the first declared intent runs.
pub(crate) async fn run_tool_child<'run>(
    host: &ToolChildHost,
    live: &LiveOpenerContext,
    request: &ToolChildRequest,
    child: crate::EffectAddress,
    controller: ScopedEffectController<'run>,
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

    // The controller arrives bound to the request's recorded admitted pair:
    // the claim scope *and* the incarnation it was admitted under, one checked
    // value. Who builds it is the tier's business — the in-process runner asks
    // this host for a `'static` one, a handler-bound tier scopes its own — and
    // there is no post-construction pin step either way: the pair was checked
    // when the request was decoded (`AdmittedScope::new` is the only
    // construction), and `enclosing_process` is never it: that field is tool
    // execution context, which `ToolChildRequest::validate` has already
    // reconciled with the opener.
    validate_recorded_authorities(host, &controller, request).await?;

    let usage_ledger = ToolUsageLedger::new();
    let dispatch = Arc::new(rebind_child_dispatch(
        live.dispatch().as_ref(),
        request,
        controller,
        execution_env_spec,
        &usage_ledger,
    )?);

    // The orchestrating-start sink the context carries: an orchestrating body
    // runs outside an attempt frame, so its realized starts have no intent
    // outcome to ride and are captured here instead (ADR 0099 §6).
    let orchestrating_starts = crate::tool_dispatch::OrchestratingStartsBuffer::default();
    // The cancellation trio is computed once, here, from the *recorded*
    // authority the validator just authenticated — and carried whole into the
    // child's context so every retry sleep and deferred wait inside it,
    // including a nested batch's, waits under exactly this shape (§3).
    let turn_cancel_wait = child_turn_cancel_wait(&dispatch, request, &cancel);
    // Boxed for the same reason the runner's call is: `drive` holds the
    // coordinator and its attempt machinery live across every await.
    let mut outcome = Box::pin(drive(
        &dispatch,
        request,
        child,
        turn_cancel_wait,
        orchestrating_starts.clone(),
    ))
    .await?;
    // The settlement is aggregated from the journaled outcome by the one
    // constructor every terminal owns (FIG-3411): per-attempt facts ride
    // `outcome.captures` and its `triggers`; what the orchestrating lane wrote
    // outside any attempt frame still drains from the child-local buffers.
    let model_return =
        resolve_model_return(&dispatch, request, &outcome, &outcome.intent_outcomes).await;
    let mut settlement = ToolSettlement::from_dispatch(&outcome, model_return);
    settlement
        .checkpoint_messages
        .extend(dispatch.checkpoint_messages.drain());
    settlement
        .triggers
        .extend(dispatch.trigger_outcomes.drain());
    settlement.usage.extend(usage_ledger.take());
    for process_id in orchestrating_starts.drain() {
        if !settlement.possession.contains(&process_id) {
            settlement.possession.push(process_id);
        }
    }
    // Realized intent evidence moved into the settlement, where the opener
    // incorporates it as evidence. The journaled terminal keeps the record and
    // the declarations; the outcomes belong to the settlement channel.
    let _ = std::mem::take(&mut outcome.intent_outcomes);
    Ok(RuntimeEffectOutcome::ToolInvocation {
        outcome: Box::new(outcome),
        settlement: Box::new(settlement),
    })
}

/// Authenticates the recorded authority set against this host before any key
/// is prepared or any attempt dispatched.
///
/// Two recorded facts are checked, both the way the journal means them:
///
/// * **Cancellation authority** (ADR 0099 §3): the recorded
///   [`TurnControlBindingId`](crate::TurnControlBindingId) is what the
///   opener's cooperative signal is fenced on, and the matrix is exact —
///   a durable-journaled participant must record *exactly* the binding this
///   host derives for the child's admitted scope, and a locally
///   participating one must record `None`, because a local participant has
///   no durable address to signal. A `Some` under local participation or a
///   `None` under durable journaling is a refused inconsistency: the child
///   would observe a cancellation channel nothing signals, or none.
/// * **Completion routing** (ADR 0099 §14): `Durable` requires a durable
///   await-event resolver behind the child's controller — a host whose
///   controller does not identify a durable authority would prepare keys no
///   resolution can reach. `ProcessLifetime` is a local-participation
///   admission only — a durable journal would resolve the key after the
///   issuing process is gone — and its recorded issuer must be *this*
///   host's registry identity: a process-lifetime key minted by another
///   registry is unresolvable here and a fresh one would double dispatch.
async fn validate_recorded_authorities(
    host: &ToolChildHost,
    controller: &ScopedEffectController<'_>,
    request: &ToolChildRequest,
) -> Result<(), RuntimeEffectControllerError> {
    use crate::runtime::effect::EffectJournaling;
    let journaling = controller.controller().effect_journaling();
    match (request.cancellation_authority.as_ref(), journaling) {
        (Some(recorded), EffectJournaling::Journaled) => {
            let effect_host = host.effect_host()?;
            let binding = effect_host
                .turn_control_binding(controller)
                .await
                .map_err(RuntimeEffectControllerError::from)?;
            if binding.binding_id() != recorded.as_str() {
                return Err(RuntimeEffectControllerError::new(
                    crate::RuntimeErrorCode::RuntimeEffectToolChildCancellationAuthority,
                    format!(
                        "tool child `{}` records cancellation authority `{}` and this host \
                         derives `{}` for its admitted scope; a foreign binding means the \
                         cooperative signal it would honour is not the one this opener sends",
                        request.call.call_id,
                        recorded.as_str(),
                        binding.binding_id()
                    ),
                ));
            }
        }
        (Some(recorded), EffectJournaling::Local) => {
            return Err(RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::RuntimeEffectToolChildCancellationAuthority,
                format!(
                    "tool child `{}` records cancellation authority `{recorded}`, but its \
                     admitted controller participates in turn control locally; a local \
                     participant has no durable binding to signal, so the record names an \
                     authority nothing can honour",
                    request.call.call_id,
                    recorded = recorded.as_str(),
                ),
            ));
        }
        (None, EffectJournaling::Journaled) => {
            return Err(RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::RuntimeEffectToolChildCancellationAuthority,
                format!(
                    "tool child `{}` records no cancellation authority, but its admitted \
                     controller participates through a durable journaled binding; the \
                     cooperative signal exists and the record that omits it is inconsistent",
                    request.call.call_id
                ),
            ));
        }
        // `None` records that no cooperative authority existed at admission;
        // there is nothing to re-derive and the child simply is not wired to
        // the cooperative signal.
        (None, EffectJournaling::Local) => {}
    }
    match &request.completion_routing {
        crate::runtime::effect::ToolChildCompletionRouting::Inline => {}
        crate::runtime::effect::ToolChildCompletionRouting::Durable => {
            if journaling != EffectJournaling::Journaled
                || controller
                    .controller()
                    .await_event_authority_binding_id()
                    .is_none()
            {
                return Err(RuntimeEffectControllerError::new(
                    crate::RuntimeErrorCode::RuntimeEffectToolChildCompletionRouting,
                    format!(
                        "tool child `{}` was admitted under durable completion routing and this \
                         controller names no durable await-event authority; the child is \
                         refused rather than parked on a key nothing resolves",
                        request.call.call_id
                    ),
                ));
            }
        }
        crate::runtime::effect::ToolChildCompletionRouting::ProcessLifetime { issuer } => {
            let current = host.effect_host()?.turn_control_binding_id();
            if journaling != EffectJournaling::Local || current != issuer.as_str() {
                return Err(RuntimeEffectControllerError::new(
                    crate::RuntimeErrorCode::RuntimeEffectToolChildCompletionRouting,
                    format!(
                        "tool child `{}` was admitted under a process-lifetime key issued by \
                         registry `{issuer}`; this host is registry `{current}` with \
                         {journaling:?} effect journaling — a process-lifetime \
                         key resolves only while its issuing local registry lives, so a \
                         durable journal or a foreign issuer makes it unresolvable here and \
                         a fresh one would double dispatch",
                        request.call.call_id,
                        issuer = issuer.as_str(),
                    ),
                ));
            }
        }
    }
    Ok(())
}

/// Dispatches the child down the lane its admitted manifest names.
///
/// The orchestrating lane is a lane of coordination, not an attempt: ADR 0042
/// says `batch` and `spawn_agent` "have no `ToolAttempt` frame of their own",
/// so an orchestrating child runs its body directly and is classified by the
/// commands it issued, never by an invented outer attempt (§4).
async fn drive(
    dispatch: &Arc<ToolDispatchContext<'_>>,
    request: &ToolChildRequest,
    child: crate::EffectAddress,
    turn_cancel_wait: crate::runtime::TurnCancelWait,
    orchestrating_starts: crate::tool_dispatch::OrchestratingStartsBuffer,
) -> Result<ToolDispatchOutcome, RuntimeEffectControllerError> {
    let tool_context = child_tool_context(
        dispatch,
        request,
        turn_cancel_wait.clone(),
        orchestrating_starts,
    );
    // The orchestrating lane is a catalog lane: only a child the Tool Catalog
    // itself admitted may run a handler-level body with no attempt frame. A
    // granted call names its own authority, and running a grant's call under
    // an orchestrating registration would let a registered orchestrator stand
    // in for a call the grant never described — the admission arm and the
    // lane are one fact, checked together.
    if matches!(
        request.admission,
        super::tool_child::ToolChildAdmission::Catalog { .. }
    ) && dispatch.is_orchestrating_tool(&request.call.tool_id)
    {
        return Ok(Box::pin(crate::tool_dispatch::execute_orchestrating_tool(
            dispatch.as_ref(),
            request.call.clone(),
            tool_context,
        ))
        .await);
    }

    let executor_context = tool_context.clone();
    let executor_dispatch = Arc::clone(dispatch);
    let coordinated = Box::pin(crate::tool_dispatch::coordinate_tool_invocation(
        dispatch.as_ref(),
        request.call.clone(),
        request.admission.grant().cloned().map(Box::new),
        request.admission.retry_policy(),
        Some(crate::tool_dispatch::GroupChildCoordination {
            completion_routing: request.completion_routing.clone(),
            child,
        }),
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
fn child_tool_context<'run>(
    dispatch: &Arc<ToolDispatchContext<'run>>,
    request: &ToolChildRequest,
    turn_cancel_wait: crate::runtime::TurnCancelWait,
    orchestrating_starts: crate::tool_dispatch::OrchestratingStartsBuffer,
) -> crate::ToolContext<'run> {
    let mut builder = crate::ToolContext::from_dispatch(Arc::clone(dispatch))
        .prepared_call(&request.call)
        .cancellation_token(Some(turn_cancel_wait.cancellation().clone()))
        .parent_invocation(request.attempt_identity.parent_invocation().cloned())
        .orchestrating_starts(orchestrating_starts)
        .turn_cancel_wait(turn_cancel_wait);
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
    dispatch: &Arc<ToolDispatchContext<'_>>,
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
async fn await_child_completion(
    dispatch: &Arc<ToolDispatchContext<'_>>,
    request: &ToolChildRequest,
    pending: crate::tool_dispatch::PendingToolDispatchOutcome,
    turn_cancel_wait: &crate::runtime::TurnCancelWait,
) -> ToolDispatchOutcome {
    await_journaled_tool_completion(
        dispatch,
        request.attempt_identity.parent_invocation(),
        &request.call.call_id,
        pending,
        turn_cancel_wait,
    )
    .await
}

/// Arms the resolver a deferred tool call named, then parks on a journaled
/// await derived from the lineage the caller supplies.
///
/// Two callers park through this one body. The driver itself parks the child's
/// own deferred call under the request's *recorded* parent invocation; an
/// orchestrating body on the group-child path parks a nested deferred call
/// under the parent it derived for that call. Both derivations name recorded
/// lineage, so a redrive re-derives the same replay key rather than a fresh
/// one — the same reconstruction property this module's documentation claims
/// for the attempts themselves.
///
/// The arming runs before the park and on every redrive, for the reason the
/// session path states: the recorded attempt body that named the resolver does
/// not re-run, so nothing else would arm it.
pub(crate) async fn await_journaled_tool_completion(
    dispatch: &ToolDispatchContext<'_>,
    parent_invocation: Option<&crate::RuntimeInvocation>,
    call_id: &str,
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
    let Some(invocation) =
        parent_invocation.map(|parent| journaled_await_invocation(dispatch, parent, call_id))
    else {
        return unarmed_child_outcome(
            pending,
            "the caller's lineage names no invocation to hang an await on",
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
    let mut outcome = crate::tool_dispatch::settle_completed_pending_tool_call(
        dispatch,
        call_id,
        pending.tool_name,
        pending.args,
        resolution,
        resolver.as_ref(),
        pending.duration_ms,
        pending.attempts,
    )
    .await;
    // The captures and trigger receipts the pre-park attempts journaled ride
    // the pending row into the settled outcome (FIG-3411); facts the resume
    // itself produced landed in the child-local buffers and reach the
    // settlement through the driver's drains.
    let mut captures = pending.captures;
    captures.extend(outcome.captures);
    outcome.captures = captures;
    let mut triggers = pending.triggers;
    triggers.extend(outcome.triggers);
    outcome.triggers = triggers;
    outcome
}

/// The invocation a journaled await is recorded under: a child of the
/// supplied parent, keyed by the deferred call's id.
fn journaled_await_invocation(
    dispatch: &ToolDispatchContext<'_>,
    parent: &crate::RuntimeInvocation,
    call_id: &str,
) -> crate::RuntimeEffectInvocation {
    let suffix = format!("{call_id}:await");
    let parent_effect_id = parent.effect_id().unwrap_or("tool").to_string();
    crate::runtime::causal::child_effect_invocation(
        dispatch.effect_controller.scoped().execution_scope(),
        parent,
        format!("{parent_effect_id}:{suffix}"),
        crate::RuntimeEffectKind::AwaitEvent,
        suffix,
    )
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
        captures: pending.captures,
        triggers: pending.triggers,
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
        captures: pending.captures,
        triggers: pending.triggers,
    }
}

/// The child's presentation boundary: the registered presentation steps, run
/// once over the settled outcome, with attachment notices computed under the
/// child's *recorded* environment (ADR 0099 §3 — a reopen uses the recorded
/// facts).
///
/// The resolved return is journaled on the settlement so incorporation consumes
/// a record instead of re-running the steps: a step that changed between
/// execution and replay cannot change what the child settled. A step *error*
/// resolves to the same recorded fallback the session path uses, so a broken
/// step settles a refusal rather than aborting the settlement.
async fn resolve_model_return(
    dispatch: &ToolDispatchContext<'_>,
    request: &ToolChildRequest,
    outcome: &ToolDispatchOutcome,
    intent_outcomes: &[crate::ToolIntentExecutionOutcome],
) -> crate::ModelToolReturn {
    let baseline = crate::ModelToolReturn::from_output(
        request.call.call_id.clone(),
        outcome.record.tool.clone(),
        &outcome.record.output,
    );
    let settlement = Arc::new(ToolSettlement::from_dispatch(outcome, baseline));
    // The child's presentation boundary is a journaled `PresentToolResult`
    // effect under the child's own bound controller, so the folded return is
    // the settlement's recorded `model_return` — replay serves the record and
    // never re-runs a step (ADR 0099 §6, FIG-3420).
    let replay_key = format!("{}:present", request.call.call_id);
    let scoped = dispatch.effect_controller.scoped();
    let presented =
        match crate::EffectAddress::new(scoped.execution_scope().clone(), replay_key.clone()) {
            Ok(address) => scoped
                .execute_effect(
                    crate::RuntimeEffectEnvelope::new(
                        crate::RuntimeEffectInvocation::new(
                            address,
                            dispatch.parentless_attribution(),
                            replay_key,
                        ),
                        crate::RuntimeEffectCommand::PresentToolResult {
                            call_id: request.call.call_id.clone(),
                            tool_name: outcome.record.tool.clone(),
                            args: outcome.record.args.clone(),
                            output: Box::new(outcome.record.output.clone()),
                            duration_ms: outcome.record.duration_ms,
                        },
                    ),
                    crate::RuntimeEffectLocalExecutor::presentation(
                        Arc::clone(&dispatch.plugins),
                        settlement,
                        Arc::clone(&dispatch.attachment_store),
                        (*dispatch
                            .execution_env_spec
                            .policy
                            .model
                            .capability
                            .attachment_acceptance)
                            .clone(),
                    ),
                )
                .await
                .and_then(crate::RuntimeEffectOutcome::into_tool_presentation),
            Err(error) => Err(error.into()),
        };
    let mut model_return = match presented {
        Ok(presentation) => presentation.model_return,
        Err(error) => crate::ModelToolReturn::text(
            request.call.call_id.clone(),
            outcome.record.tool.clone(),
            error.to_string(),
        ),
    };
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

/// The opener an admitted execution scope names, or `None` when the scope is
/// not one an opener can be derived from.
///
/// The derivation is the one owner derivation every durable-work surface uses
/// ([`EffectOpener::for_scope`], FIG-3417): the scope supplies the identity
/// for a turn and for a queued drain, while a process scope's opener is the
/// **pinned** incarnation inside the [`AdmittedScope`] — never a name
/// resolved afresh, because a same-name successor must not rebind work its
/// predecessor still owns. `for_scope`'s refusals — an administrative scope
/// names no opener — are `None` here, because a registration site that cannot
/// name an opener must register nothing rather than mint one.
///
/// Everything that registers a live opener goes through here, so a new scope
/// arm is a compile error in `for_scope` rather than a silently unregistered
/// opener.
#[must_use]
pub fn opener_for_execution_scope(admitted: &AdmittedScope) -> Option<EffectOpener> {
    EffectOpener::for_scope(admitted).ok()
}

#[cfg(test)]
#[path = "tool_child_driver/tests.rs"]
mod tests;
