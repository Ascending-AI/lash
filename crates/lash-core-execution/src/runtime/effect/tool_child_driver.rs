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
//! It holds no in-process drain slot. §5 orders sibling drains by a durable
//! per-group final-commit order: §4 commits the child's final at the child's
//! terminal — its final attempt's boundary or its resolved completion — and
//! §5 admits its drain by the recorded `commit_seq`, which orders it against
//! every sibling without a process-local gate.
//!
//! It projects the child's result exactly once, at its own presentation
//! boundary: the session's ordered presentation steps run once through the
//! journaled `PresentToolResult` effect under the child's bound controller,
//! and the driver journals the resolved `ModelToolReturn` on the settlement
//! rather than leaving a `CompletedToolCall` for the opener to derive.
//! Incorporation consumes the record; it never re-presents, so a changed
//! presentation environment on replay cannot change what the child settled.

use std::sync::Arc;

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
    /// The store a child's recorded `execution_env` resolves against
    /// (`run_tool_child`). Mutable for the same reason the clock is: the host
    /// is installed — get-or-init — inside `RuntimeHostConfig::new`, before
    /// `with_process_env_store` can name the store the rest of the runtime
    /// publishes to, and a second install cannot replace it.
    process_env_store: Arc<std::sync::Mutex<Arc<dyn ProcessExecutionEnvStore>>>,
    /// The clock a `Sleep`/`AwaitEvent` group child waits on. Mutable because
    /// the host is installed — get-or-init — before the runtime's configured
    /// clock is known (`RuntimeHostConfig::with_clock` follows `new`), and a
    /// second install cannot replace it.
    clock: Arc<std::sync::Mutex<Arc<dyn crate::Clock>>>,
    /// The deployment's builder of a child's context when its opener is not
    /// live here (FIG-3712). **Weak** for the same reason `host` is: the
    /// source is the embedder's session wiring, which owns the backend this
    /// host belongs to. A source that is gone builds nothing, and the child
    /// is then a routing miss as it would be with none installed. More than
    /// one live source makes the host ambiguous (see
    /// [`ContextSourceInstall`]).
    context_source: Arc<std::sync::Mutex<Vec<std::sync::Weak<dyn ToolChildContextSource>>>>,
    /// Testing only: the resolver a law installs *behind* this host, asked
    /// for a command no group child can be (a law's synthetic children). A
    /// conformance world whose laws open synthetic groups and whose runtime
    /// also forms product tool groups needs both answers on one controller,
    /// and a controller has one registered resolver.
    #[cfg(any(test, feature = "testing"))]
    law_fallback: Arc<std::sync::OnceLock<Arc<dyn super::group_drain::GroupExecutors>>>,
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
            process_env_store: Arc::new(std::sync::Mutex::new(process_env_store)),
            clock: Arc::new(std::sync::Mutex::new(Arc::new(crate::SystemClock))),
            context_source: Arc::new(std::sync::Mutex::new(Vec::new())),
            #[cfg(any(test, feature = "testing"))]
            law_fallback: Arc::new(std::sync::OnceLock::new()),
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

    /// Rebinds the store children resolve their recorded execution
    /// environment against, in place, and returns the same host.
    ///
    /// In place rather than rebuilding for the same reason as
    /// [`with_clock`](Self::with_clock): a `RuntimeHostConfig` that swaps its
    /// durability store after `new` must not leave the installed host reading
    /// the store the runtime no longer publishes to.
    pub fn with_process_env_store(
        self: &Arc<Self>,
        store: Arc<dyn ProcessExecutionEnvStore>,
    ) -> Arc<Self> {
        *self.process_env_store.lock_recover() = store;
        Arc::clone(self)
    }

    /// The registry the turn and process sites register their openers in.
    #[must_use]
    pub fn openers(&self) -> &Arc<LiveOpenerRegistry> {
        &self.openers
    }

    /// Installs the deployment's builder of a child's context for when its
    /// opener is not live here (FIG-3712).
    ///
    /// One host has one answer, as it has one resolver: while two distinct
    /// sources are alive, which wiring a child ran under would depend on
    /// which embedder was built last, so neither is used. The host is
    /// ambiguous, and a child with no live opener here is refused, typed,
    /// until only one source is left. A source that has been dropped no longer
    /// counts. Installing the same source again changes nothing.
    pub fn install_context_source(
        &self,
        source: &Arc<dyn ToolChildContextSource>,
    ) -> ContextSourceInstall {
        let mut installed = self.context_source.lock_recover();
        installed.retain(|existing| existing.strong_count() > 0);
        if !installed.iter().any(|existing| {
            existing
                .upgrade()
                .is_some_and(|existing| Arc::ptr_eq(&existing, source))
        }) {
            installed.push(Arc::downgrade(source));
        }
        match installed.len() {
            1 => ContextSourceInstall::Sole,
            live => ContextSourceInstall::Ambiguous { live },
        }
    }

    /// The installed context source, while exactly one is alive.
    fn context_source(&self) -> InstalledContextSource {
        let mut installed = self.context_source.lock_recover();
        installed.retain(|existing| existing.strong_count() > 0);
        match installed.as_slice() {
            [] => InstalledContextSource::None,
            [only] => only
                .upgrade()
                .map_or(InstalledContextSource::None, InstalledContextSource::Sole),
            _ => InstalledContextSource::Ambiguous,
        }
    }

    /// Where a tool child finds the context it runs under: its opener's, lent
    /// by the registry, when the opener is live here; otherwise the
    /// deployment's, when a source is installed; otherwise nowhere, the
    /// routing fact "not mine".
    fn child_opener(&self, opener: &crate::EffectOpener) -> Option<ChildOpenerContext> {
        match self.openers.context_for(opener) {
            Some(live) => Some(ChildOpenerContext::Live(live)),
            None => (!matches!(self.context_source(), InstalledContextSource::None))
                .then_some(ChildOpenerContext::Deployment),
        }
    }

    /// The controller a deployment-built context's controller slots are lent,
    /// as a live opener lends its admitted scope's: the rebind replaces both.
    fn lent_controller(
        &self,
        request: &ToolChildRequest,
    ) -> Result<ScopedEffectController<'static>, RuntimeEffectControllerError> {
        self.effect_host()?
            .scoped_static(request.scope.admitted_scope.clone())
            .map_err(RuntimeEffectControllerError::from)?
            .ok_or_else(|| {
                RuntimeEffectControllerError::new(
                    crate::RuntimeErrorCode::RuntimeEffectLocalExecutorUnavailable,
                    "this effect host hands out no owned scoped controller to lend a \
                     deployment-built tool-child context",
                )
            })
    }

    /// The context a child runs under, resolved at execution: the live
    /// opener's lent context, or one the deployment builds now for the child's
    /// recorded session and environment, with its stream recorded.
    async fn resolve_child_context(
        &self,
        opener: &ChildOpenerContext,
        request: &ToolChildRequest,
        execution_env: &crate::ProcessExecutionEnvSpec,
    ) -> Result<ResolvedChildContext, RuntimeEffectControllerError> {
        match opener {
            ChildOpenerContext::Live(live) => Ok(ResolvedChildContext {
                context: live.clone(),
                recorder: None,
                refusal: None,
                _keepalive: None,
            }),
            ChildOpenerContext::Deployment => {
                // What the opener's context had and no deployment can
                // rebuild: the child waits for its opener rather than run
                // without it (FIG-3712).
                if let Some(refusal) = request.session.unrecorded.rebuild_refusal() {
                    tracing::warn!(
                        call_id = %request.call.call_id,
                        %refusal,
                        "a tool child with no live opener here waits for its opener"
                    );
                    return Err(refusal.into_error(&request.call.call_id));
                }
                let source = match self.context_source() {
                    InstalledContextSource::Sole(source) => source,
                    InstalledContextSource::Ambiguous => {
                        let refusal = super::ToolChildRebuildRefusal::AmbiguousDeployment;
                        tracing::warn!(
                            call_id = %request.call.call_id,
                            %refusal,
                            "a tool child with no live opener here waits for its opener"
                        );
                        return Err(refusal.into_error(&request.call.call_id));
                    }
                    InstalledContextSource::None => {
                        return Err(RuntimeEffectControllerError::new(
                            crate::RuntimeErrorCode::RuntimeEffectLocalExecutorUnavailable,
                            format!(
                                "tool child `{}` has no live opener here and the deployment's \
                                 context source is gone",
                                request.call.call_id
                            ),
                        ));
                    }
                };
                let built = source
                    .tool_child_context(request, execution_env, self.lent_controller(request)?)
                    .await
                    .map_err(|error| {
                        RuntimeEffectControllerError::from(
                            error.into_turn_failure(crate::RuntimeErrorCode::Plugin),
                        )
                    })?;
                let (mut dispatch, keepalive) = built.into_parts();
                let recorder = ChildStreamRecorder::start();
                recorder.attach(&mut dispatch);
                let refusal = SessionServicesRefusal::default();
                refusal.attach(&mut dispatch);
                Ok(ResolvedChildContext {
                    context: LiveOpenerContext::deployment_built(dispatch),
                    recorder: Some(recorder),
                    refusal: Some(refusal),
                    _keepalive: Some(keepalive),
                })
            }
        }
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
    pub(crate) fn child_controller(
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
    /// Every tool child is this resolver's, whether or not its opener is live
    /// in this process: the process whose opener is live runs it. Every other
    /// command routes exactly when [`executor_for`](Self::executor_for) answers.
    fn routes(&self, envelope: &RuntimeEffectEnvelope) -> bool {
        matches!(
            envelope.command,
            RuntimeEffectCommand::ToolInvocation { .. }
        ) || super::group_drain::GroupExecutors::executor_for(self, envelope).is_some()
    }

    /// Routes through the host that installed this resolver: a layer over
    /// that host wraps every child an engine handler runs for it, tool,
    /// timer and durable wait alike.
    fn route_handler_child_controller<'run>(
        &self,
        controller: crate::ScopedEffectController<'run>,
    ) -> Result<crate::ScopedEffectController<'run>, crate::RuntimeError> {
        self.effect_host()
            .map_err(RuntimeEffectControllerError::into_runtime_error)?
            .route_handler_child_controller(controller)
    }

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
    /// (ADR 0099 §4). A timer child already carries the deadline its
    /// aggregate recorded at admission (`SleepSpec::Until`, §11 clause 4), so
    /// the executor waits on an instant rather than restarting a duration.
    fn executor_for(
        &self,
        envelope: &RuntimeEffectEnvelope,
    ) -> Option<RuntimeEffectLocalExecutor<'static>> {
        match &envelope.command {
            RuntimeEffectCommand::ToolInvocation { request } => {
                let opener = self.child_opener(&request.scope.opener)?;
                Some(RuntimeEffectLocalExecutor::owned_runner(
                    Box::new(ToolChildRunner {
                        host: self.clone(),
                        opener,
                    }),
                    None,
                ))
            }
            // A group child's cancellation is the group's (ADR 0099 §4: the
            // group dispatch passes its own token into the child's
            // `execute_effect_cancellable`), so turn-cancel observation is off
            // on both arms. A timer child's deadline was recorded when its
            // aggregate admitted it (§11 clause 4); a losing durable wait is
            // released by its opener's close, never by selection (§12).
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
            #[cfg(any(test, feature = "testing"))]
            _ => self
                .law_fallback
                .get()
                .and_then(|fallback| fallback.executor_for(envelope)),
            #[cfg(not(any(test, feature = "testing")))]
            _ => None,
        }
    }

    fn live_generation(&self, opener: &crate::EffectOpener) -> Option<u64> {
        self.openers.generation_of(opener)
    }
}

#[cfg(any(test, feature = "testing"))]
impl ToolChildHost {
    /// Installs `fallback` behind this host: asked only for a command no
    /// group child can be. Set once; a second call keeps the first.
    pub fn with_law_fallback(
        self: &Arc<Self>,
        fallback: Arc<dyn super::group_drain::GroupExecutors>,
    ) -> Arc<Self> {
        let _ = self.law_fallback.set(fallback);
        Arc::clone(self)
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
            &ChildOpenerContext::Live(self.context.clone()),
            &request,
            envelope.invocation.address.clone(),
            self.host
                .child_controller(&request.scope.admitted_scope, binding)?,
        ))
        .await
    }
}

/// Where a routed child's context comes from.
enum ChildOpenerContext {
    /// Its opener is live here and lends this context.
    Live(LiveOpenerContext),
    /// Its opener is not live here; the deployment's context source builds
    /// one at execution (FIG-3712).
    Deployment,
}

/// A child's context for one execution, with the recorder of its stream when
/// the deployment built it, and whatever the built context must keep alive.
/// What installing a tool-child context source left the host with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[must_use]
pub enum ContextSourceInstall {
    /// This source is the only live one: children with no live opener here
    /// are built from it.
    Sole,
    /// Other distinct sources are live too, `live` in all: no child is built
    /// from any of them, and one with no live opener here is refused
    /// ([`ToolChildRebuildRefusal::AmbiguousDeployment`](super::ToolChildRebuildRefusal::AmbiguousDeployment))
    /// and waits for its opener.
    Ambiguous { live: usize },
}

enum InstalledContextSource {
    None,
    Sole(Arc<dyn ToolChildContextSource>),
    Ambiguous,
}

struct ResolvedChildContext {
    context: LiveOpenerContext,
    recorder: Option<ChildStreamRecorder>,
    /// Set on a built context: fires when the child reached a session service
    /// only its opener's turn can serve.
    refusal: Option<SessionServicesRefusal>,
    _keepalive: Option<Arc<dyn std::any::Any + Send + Sync>>,
}

/// One routed child, waiting to be handed its envelope.
///
/// The runner owns the exact opener context `executor_for` resolved — the
/// journaled request was admitted against that context, so execution uses it
/// rather than asking the registry again: an opener guard dropped between
/// resolution and execution must not stall the child on a re-registration
/// nothing guarantees, nor rebind it to a successor opener's context. A child
/// routed with no live opener builds its context from the deployment when it
/// runs.
struct ToolChildRunner {
    host: ToolChildHost,
    opener: ChildOpenerContext,
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
            &self.opener,
            &request,
            envelope.invocation.address.clone(),
            self.host
                .child_controller(&request.scope.admitted_scope, binding)?,
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
    /// outcome. The tier routes `controller` through the resolver's host
    /// ([`GroupExecutors::route_handler_child_controller`](super::group_drain::GroupExecutors::route_handler_child_controller))
    /// before handing it here, as it routes every other child kind. `child` is the child's own `ToolInvocation`
    /// envelope address — the replay row its §4 final commits against (ADR
    /// 0099 §4). A live opener's token is the parent of the child's body
    /// token, as it is for the in-process `execute`; a deployment-built
    /// context has none, and the opener's turn cancel reaches the child
    /// through its durable gate.
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
            &self.opener,
            request,
            child,
            controller,
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
/// Tool access binds through the recorded surface: the opener's access is
/// folded into the surface it recorded, and every id the child's calls name
/// resolves there. The subagent context cannot be rebound, since the plugins
/// were built under it, so it is checked against the recorded one instead.
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
    // The subagent context the serving plugins were built under decides how
    // deep a nested spawn may recurse, and plugins cannot be rebound. A lent
    // context has its opener's own, which is what the request recorded; a
    // built one is built from the request. A context that disagrees serves
    // nothing (FIG-3712).
    if lent.plugins.subagent_context() != request.session.subagent.as_ref() {
        return Err(
            super::ToolChildRebuildRefusal::SubagentContext.into_error(&request.call.call_id)
        );
    }
    let mut child = lent.clone();
    // A child may be attributed to a session the lending opener is not: a
    // process opener has no session of its own (ADR 0094) and still does tool
    // work that belongs to one.
    child.session_id = request.scope.session_id.clone();
    // One session holds many frames (ADR 0092), so a child that inherited the
    // opener's current frame would attribute its work to the wrong one.
    child.agent_frame_id = request.scope.agent_frame_id.clone();
    // Ruling 1: a reopen may not consult the live Tool Catalog, and neither
    // may the calls the child issues (FIG-3712). See `admitted_catalog`.
    child.tool_catalog = Arc::new(admitted_catalog(request));
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

/// The catalog a child is dispatched against: the tool surface its opener
/// recorded at group open, with the child's admitted manifest pinned at its
/// own id.
///
/// ADR 0099 §3 amendment 1: "An ungranted call pins its admitted manifest. A
/// reopen may not consult the live Tool Catalog" *for it* — a tool whose
/// retry policy or argument projection changed between admission and
/// recovery would otherwise make a recovered child behave unlike the child
/// that was admitted. At this call's id the catalog answers with the recorded
/// manifest, whatever the recorded surface or the deployment now says.
///
/// Every other id answers from the recorded surface too (FIG-3712), never
/// from the context that happens to serve the child. A call the child's
/// orchestrating body issues is admitted against what its opener could call
/// at group open: the opener's session tool access and subagent depth are
/// already folded into that surface (a subagent at its maximum depth has no
/// `spawn_agent` in it), so a child run by a context the deployment built
/// cannot reach a tool its opener could not — nor lose one it could.
///
/// The recorded manifest's contract is the recorded surface's entry for that
/// id when there is one, and a default otherwise: a contract is read during
/// preparation, which has already happened, since a child carries a
/// `PreparedToolCall`, so neither can change what the child does.
fn admitted_catalog(request: &ToolChildRequest) -> ToolCatalog {
    let manifest = request.admission.manifest().clone();
    let surface = &request.session.tool_surface;
    let recorded_contract = surface
        .iter()
        .find(|definition| definition.manifest.id == manifest.id)
        .map(|definition| definition.contract.clone())
        .unwrap_or_default();
    let definitions = std::iter::once(crate::ToolDefinition {
        manifest: manifest.clone(),
        contract: recorded_contract,
    })
    .chain(
        surface
            .iter()
            .filter(|definition| definition.manifest.id != manifest.id)
            .cloned(),
    )
    .collect();
    ToolCatalog::from_tool_definitions(definitions)
}

/// How the tool a catalog-admitted child calls drifted from the definition
/// its opener recorded (FIG-3725): its admitted manifest, with the contract
/// the recorded surface holds for it, judged against the catalog the serving
/// registry resolves to under the child's recorded tool access and subagent
/// context — the rule a turn's recorded surface is judged by (FIG-3587). A
/// granted call carries its own definition and is not judged.
fn admitted_tool_drift(
    lent: &ToolDispatchContext<'_>,
    request: &ToolChildRequest,
) -> Result<Option<crate::ToolSurfaceDrift>, RuntimeEffectControllerError> {
    let super::tool_child::ToolChildAdmission::Catalog { manifest } = &request.admission else {
        return Ok(None);
    };
    // The opener records its whole surface, the admitted tool included. A
    // request whose surface holds no definition for its tool recorded
    // nothing to judge the live tool by, so it fails closed: served only
    // from its journal.
    let Some(contract) = request
        .session
        .tool_surface
        .iter()
        .find(|definition| definition.manifest.id == manifest.id)
        .map(|definition| definition.contract.clone())
    else {
        return Ok(Some(crate::ToolSurfaceDrift {
            kind: crate::ToolSurfaceDriftKind::Unrecorded,
            recorded: crate::ToolDefinition {
                manifest: manifest.as_ref().clone(),
                contract: crate::ToolContract::default(),
            },
        }));
    };
    let recorded = crate::ToolDefinition {
        manifest: manifest.as_ref().clone(),
        contract,
    };
    // The registry the serving context dispatches through, as a turn's
    // recorded surface is judged against it; a context with no registry
    // serves its provider alone.
    let live_tools = lent.tool_registry.as_ref().map_or_else(
        || Arc::clone(&lent.tools),
        |registry| Arc::clone(registry) as Arc<dyn crate::ToolProvider>,
    );
    let live = lent
        .plugins
        .resolve_live_tool_catalog(
            &request.scope.session_id,
            live_tools,
            request.session.tool_access.clone(),
            request.session.subagent.clone(),
        )
        .map_err(|error| {
            RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::ToolCatalogResolutionFailed,
                format!(
                    "tool child `{}` could not resolve the live catalog its tool is judged \
                     against: {error}",
                    request.call.call_id
                ),
            )
        })?;
    Ok(crate::ToolSurfaceDrift::judge(&recorded, &live))
}

/// Runs one tool child to a terminal and reports what it produced.
///
/// The ordering this provides, stated rather than assumed (§4/§5, FIG-3409):
/// a child takes no in-process slot because the durable group owns the order —
/// its final commits at the child's terminal — its final attempt's boundary
/// or its resolved completion — against its own replay row (`child`), and
/// its drain is admitted by the recorded `commit_seq` barrier before the
/// first declared intent runs.
async fn run_tool_child<'run>(
    host: &ToolChildHost,
    opener: &ChildOpenerContext,
    request: &ToolChildRequest,
    child: crate::EffectAddress,
    controller: ScopedEffectController<'run>,
) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
    // Refused here as well as at decode: a request this build cannot
    // reconstruct completely is a child it would run under partial authority.
    request.validate()?;

    let execution_env_spec = load_execution_env(host, request, &controller).await?;

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

    let resolved = host
        .resolve_child_context(opener, request, &execution_env_spec)
        .await?;
    let live = &resolved.context;
    // The child judges its own tool against the registry serving it
    // (FIG-3725): a tool that drifted since its opener recorded it is served
    // only from the child's journal. Every dispatching effect the child
    // issues carries the drift refusal to its engine, which serves a recorded
    // outcome and refuses — running and recording nothing — one it would run
    // live (FIG-3719).
    let served_only =
        admitted_tool_drift(live.dispatch().as_ref(), request)?.map(|drift| {
            Arc::new(crate::CommandJournalGuard::open().served_only(
                crate::ServedOnlyRange::every_key(drift.refusal(&request.call.call_id)),
            ))
        });
    let controller = match &served_only {
        Some(guard) => controller.with_journal_guard(Arc::clone(guard)),
        None => controller,
    };
    let cancel = live.cancellation().child_token();
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
    let orchestrating_sinks = crate::tool_dispatch::OrchestratingChildSinks::default();
    // The cancellation trio is computed once, here, from the *recorded*
    // authority the validator just authenticated — and carried whole into the
    // child's context so every retry sleep and deferred wait inside it,
    // including a nested batch's, waits under exactly this shape (§3).
    let turn_cancel_wait = child_turn_cancel_wait(&dispatch, request, &cancel);
    // A built context has no running opener to fire its stop: its turn's
    // durable gate does (see `watch_turn_stop`).
    let _turn_stop_watch = match &resolved.recorder {
        Some(_) => deployment_context::watch_turn_stop(
            host.effect_host()?,
            child_turn_cancel_scope(&dispatch, request),
            live.cancellation().clone(),
        ),
        None => None,
    };
    // Boxed for the same reason the runner's call is: `drive` holds the
    // coordinator and its attempt machinery live across every await.
    let driven = Box::pin(drive(
        &dispatch,
        request,
        child,
        turn_cancel_wait,
        orchestrating_sinks.clone(),
    ));
    // On a built context, a session service call the child made abandons the
    // drive where it stands, as a crash would: nothing the refused call led
    // to is recorded (see `SessionServicesRefusal`).
    let mut outcome = match &resolved.refusal {
        None => driven.await?,
        Some(refusal) => match refusal.abandoning(driven).await {
            Ok(outcome) => outcome?,
            Err(refusal) => {
                tracing::warn!(
                    call_id = %request.call.call_id,
                    %refusal,
                    "a tool child with no live opener here waits for its opener"
                );
                return Err(refusal.into_error(&request.call.call_id));
            }
        },
    };
    // An engine that refused a served-only effect tripped the guard, however
    // the attempt's caller shaped the error: the child refuses with the
    // drift, settles nothing and presents nothing (FIG-3725).
    if let Some(refusal) = served_only.as_ref().and_then(|guard| guard.tripped()) {
        return Err(refusal);
    }
    // The settlement is aggregated from the journaled outcome by the one
    // constructor every terminal owns (FIG-3411): per-attempt facts ride
    // `outcome.captures` and its `triggers`; what the orchestrating lane wrote
    // outside any attempt frame still drains from the child-local buffers.
    let model_return =
        resolve_model_return(&dispatch, request, &outcome, &outcome.intent_outcomes).await?;
    let mut settlement = ToolSettlement::from_dispatch(&outcome, model_return);
    settlement
        .checkpoint_messages
        .extend(dispatch.checkpoint_messages.drain());
    settlement
        .triggers
        .extend(dispatch.trigger_outcomes.drain());
    settlement.usage.extend(usage_ledger.take());
    if let Some(recorder) = resolved.recorder {
        let mut stream = recorder.finish().await;
        // What the child's own journaled record holds is referenced, not
        // recorded twice (see `RecordedChildStream::settle_against`).
        if let Ok(record) = serde_json::to_value(&outcome.record) {
            stream.settle_against(&record);
        }
        settlement.stream = stream;
    }
    for process_id in orchestrating_sinks.drain() {
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

/// The environment the child was admitted under, read through a recorded
/// step on its own controller (FIG-3683): the store is read once, and every
/// replay of the child executes under the recorded spec instead of reading it
/// again. A store that did not answer is not recorded; the engine runs the
/// step again (see `ExecutionEnvLoadExecution`).
async fn load_execution_env(
    host: &ToolChildHost,
    request: &ToolChildRequest,
    controller: &ScopedEffectController<'_>,
) -> Result<crate::ProcessExecutionEnvSpec, RuntimeEffectControllerError> {
    let store = host.process_env_store.lock_recover().clone();
    let scope = controller.execution_scope();
    let replay_key = format!("{}:env", request.call.call_id);
    let address = crate::EffectAddress::new(scope.clone(), replay_key.clone())?;
    let attribution = scope
        .session_id()
        .map(crate::RuntimeAttribution::for_session)
        .unwrap_or_else(crate::RuntimeAttribution::none);
    controller
        .execute_effect(
            RuntimeEffectEnvelope::new(
                crate::RuntimeEffectInvocation::new(address, attribution, replay_key),
                RuntimeEffectCommand::LoadExecutionEnv {
                    env: request.execution_env.clone(),
                },
            ),
            RuntimeEffectLocalExecutor::execution_env_load(
                store,
                format!("tool child `{}`", request.call.call_id),
            ),
        )
        .await
        .and_then(RuntimeEffectOutcome::into_execution_env)
}

/// Authenticates the recorded authority set against this host before any key
/// is prepared or any attempt dispatched.
///
/// Two recorded facts are checked, both the way the journal means them:
///
/// * **Cancellation authority** (ADR 0099 §3): the recorded
///   [`TurnControlBindingId`](crate::TurnControlBindingId) is what the
///   opener's cooperative signal is fenced on, so the child must record
///   *exactly* the binding this host derives for its admitted scope. A
///   foreign binding means the child would observe a cancellation channel
///   this opener never signals.
/// * **Completion routing** (ADR 0099 §14): `Durable` requires a durable
///   await-event resolver behind the child's controller — a controller that
///   does not identify a durable authority would prepare keys no resolution
///   can reach.
pub(crate) async fn validate_recorded_authorities(
    host: &ToolChildHost,
    controller: &ScopedEffectController<'_>,
    request: &ToolChildRequest,
) -> Result<(), RuntimeEffectControllerError> {
    // Routing first: a controller that names no durable authority cannot
    // serve a durable child at all, and that, not the turn-control binding
    // it also cannot derive, is why the child is refused.
    match &request.completion_routing {
        crate::runtime::effect::ToolChildCompletionRouting::Inline => {}
        crate::runtime::effect::ToolChildCompletionRouting::Durable => {
            if controller
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
    }
    let recorded = &request.cancellation_authority;
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
    orchestrating_sinks: crate::tool_dispatch::OrchestratingChildSinks,
) -> Result<ToolDispatchOutcome, RuntimeEffectControllerError> {
    let tool_context = child_tool_context(
        dispatch,
        request,
        turn_cancel_wait.clone(),
        orchestrating_sinks.clone(),
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
        let outcome = Box::pin(crate::tool_dispatch::execute_orchestrating_tool(
            dispatch.as_ref(),
            request.call.clone(),
            tool_context,
        ))
        .await;
        // A nested call the body issued was refused: the child is refused with
        // it rather than settling what the body made of the failure.
        return match orchestrating_sinks.take_refusal() {
            Some(refusal) => Err(refusal),
            None => Ok(outcome),
        };
    }

    let executor_context = tool_context.clone();
    let executor_dispatch = Arc::clone(dispatch);
    let group_child = crate::tool_dispatch::GroupChildCoordination {
        completion_routing: request.completion_routing.clone(),
        child,
    };
    let coordinated = Box::pin(crate::tool_dispatch::coordinate_tool_invocation(
        dispatch.as_ref(),
        request.call.clone(),
        request.admission.grant().cloned().map(Box::new),
        request.admission.retry_policy(),
        Some(group_child.clone()),
        request.attempt_identity.clone(),
        &turn_cancel_wait,
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
        //
        // The resolved completion is this child's terminal, so its final
        // record crosses the §4 boundary here — before presentation, exactly
        // where an inline terminal crosses it. Left to the child's finalize,
        // the commit would land only after the presentation boundary, and a
        // sibling that settled later but committed at its own attempt boundary
        // would take the earlier commit position and lead the settlement order
        // (FIG-3609). A parked attempt declares no intents, so there is no
        // drain to admit behind the barrier: the discharge seats the rank.
        ToolCallLaunch::Pending(pending) => {
            let mut outcome =
                await_child_completion(dispatch, request, *pending, &turn_cancel_wait).await?;
            let mut recorded_call_id = outcome.record.call_id.clone();
            crate::tool_dispatch::commit_group_child_boundary(
                dispatch.as_ref(),
                Some(&group_child),
                &mut outcome.record,
                &mut outcome.intents,
                &mut recorded_call_id,
            )
            .await?;
            Ok(outcome)
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
    orchestrating_sinks: crate::tool_dispatch::OrchestratingChildSinks,
) -> crate::ToolContext<'run> {
    let mut builder = crate::ToolContext::from_dispatch(Arc::clone(dispatch))
        .prepared_call(&request.call)
        .cancellation_token(Some(turn_cancel_wait.cancellation().clone()))
        .parent_invocation(request.attempt_identity.parent_invocation().cloned())
        .orchestrating_sinks(orchestrating_sinks)
        .turn_cancel_wait(turn_cancel_wait);
    if let Some(process_ref) = request.enclosing_process.as_ref() {
        builder = builder.enclosing_process(Some(process_ref.process_id.clone()));
    }
    builder.build()
}

/// The cancellation trio a child waits under: observing its recorded
/// cancellation authority's gate for the child's physical turn.
fn child_turn_cancel_wait(
    dispatch: &Arc<ToolDispatchContext<'_>>,
    request: &ToolChildRequest,
    cancel: &CancellationToken,
) -> crate::runtime::TurnCancelWait {
    crate::runtime::TurnCancelWait::observing(
        cancel.clone(),
        child_turn_cancel_scope(dispatch, request),
    )
}

/// The scope a child's turn-cancel gate registers under: the *physical* turn
/// its call was issued in, exactly as the opener's own waits register it.
///
/// A turn's admitted scope stays the root turn's across agent frames, while a
/// follow-on frame is a distinct physical turn whose cancel gate is its own.
/// The child's attempts and retry sleeps are attributed to the call's
/// physical turn (the parent invocation's), so the gate must name the same
/// turn — a durable journal refuses a wait whose cancel scope and attribution
/// disagree. A non-turn opener (a process) keeps its admitted scope.
fn child_turn_cancel_scope(
    dispatch: &Arc<ToolDispatchContext<'_>>,
    request: &ToolChildRequest,
) -> crate::ExecutionScope {
    let scoped = dispatch.effect_controller.scoped();
    let admitted = scoped.execution_scope();
    let physical_turn = request
        .attempt_identity
        .parent_invocation()
        .and_then(|parent| parent.attribution.turn_id.as_ref());
    match (admitted, physical_turn) {
        (crate::ExecutionScope::Turn { .. }, Some(turn_id)) => match admitted.session_id() {
            Some(session_id) => crate::ExecutionScope::turn(session_id.clone(), turn_id.clone()),
            None => admitted.clone(),
        },
        _ => admitted.clone(),
    }
}

/// Arms the resolver the parked call named, then parks on the child's own
/// journaled await.
async fn await_child_completion(
    dispatch: &Arc<ToolDispatchContext<'_>>,
    request: &ToolChildRequest,
    pending: crate::tool_dispatch::PendingToolDispatchOutcome,
    turn_cancel_wait: &crate::runtime::TurnCancelWait,
) -> Result<ToolDispatchOutcome, RuntimeEffectControllerError> {
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
) -> Result<ToolDispatchOutcome, RuntimeEffectControllerError> {
    if let Err(error) = crate::tool_dispatch::arm_pending_resolver(
        dispatch.processes.as_ref(),
        &pending.pending,
        &pending.key,
        dispatch.process_scope(),
    )
    .await
    {
        return Ok(unarmed_child_outcome(pending, &error.to_string()));
    }
    let Some(invocation) =
        parent_invocation.map(|parent| journaled_await_invocation(dispatch, parent, call_id))
    else {
        return Ok(unarmed_child_outcome(
            pending,
            "the caller's lineage names no invocation to hang an await on",
        ));
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
        // The await's recorded `Failed` terminal replaying is the call's
        // result. Anything else — a replay divergence against its record, a
        // live journal fault — is a refusal, returned so the caller refuses
        // the call rather than settling the error as its result (FIG-3679).
        Err(error) if error.journaled => {
            return Ok(failed_child_outcome(pending, &error.to_string()));
        }
        Err(error) => return Err(error),
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
    Ok(outcome)
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
/// step settles a refusal rather than aborting the settlement. A failure of the
/// presentation *effect* — a replay divergence against its recorded envelope, a
/// journal fault — is no presentation at all: it refuses the child, exactly as
/// a failed attempt effect does, and never settles as the model-facing return.
async fn resolve_model_return(
    dispatch: &ToolDispatchContext<'_>,
    request: &ToolChildRequest,
    outcome: &ToolDispatchOutcome,
    intent_outcomes: &[crate::ToolIntentExecutionOutcome],
) -> Result<crate::ModelToolReturn, RuntimeEffectControllerError> {
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
                        outcome.record.duration_ms,
                    ),
                )
                .await
                .and_then(crate::RuntimeEffectOutcome::into_tool_presentation),
            Err(error) => Err(error.into()),
        };
    let mut model_return = presented?.model_return;
    // The same addenda the session path appends in `complete_tool_call`: the
    // realized intents are part of the presentation the model sees, so the
    // recorded return carries them rather than leaving incorporation to
    // recompute them.
    for intent_outcome in intent_outcomes {
        model_return.parts.push(crate::ModelToolReturnPart::text(
            intent_outcome.model_addendum(),
        ));
    }
    Ok(model_return)
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

mod deployment_context;
use deployment_context::{ChildStreamRecorder, SessionServicesRefusal};
pub use deployment_context::{DeploymentToolChildContext, ToolChildContextSource};

#[cfg(test)]
#[path = "tool_child_driver/tests.rs"]
mod tests;

#[cfg(test)]
mod rebuild_tests;
