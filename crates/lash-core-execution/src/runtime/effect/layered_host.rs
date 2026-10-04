//! [`LayeredEffectHost`]: a recording or fault layer over any effect host
//! (FIG-3580).
//!
//! A test that has to observe or perturb the effect boundary wraps a real
//! deployment's host instead of hand-writing a controller that re-implements
//! half of one. The host lends exactly the controllers its inner host lends —
//! plain, static and group-child-bound — each wrapped with one
//! [`EffectLayer`]. Every group operation, await-event registry read and
//! journal lever lands on the inner host's substrate, so a group opened through
//! a layered controller is arbitrated by the state the inner host wrote, and a
//! bound child's admissions are fenced exactly where the inner host fences
//! them. A layer sees the seam operations and may observe, delay, fail or crash
//! them; it owns no state the substrate answers from.
//!
//! A controller the host did not lend can be layered too:
//! [`LayeredEffectHost::layer_scoped`] layers any scoped controller, including
//! one borrowed from an engine handler (a Restate turn's `ctx`-bound
//! controller), for as long as that borrow lives, and
//! [`LayeredEffectHost::layer_controller`] layers a shared controller. So a law
//! that runs its turn wherever the tier runs turns (a
//! `lash_conformance::ConformanceTurnRunner`) layers the controller that runner
//! lends, and one layer observes the same seam on every tier.

use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use super::{
    AdmittedScope, AwaitEventKey, AwaitEventResolver, AwaitEventWaitIdentity, BoundaryReason,
    CompletionKeyPreparation, EffectHost, EffectJournalRetirement, ExecutionScope, Resolution,
    ResolveOutcome, RuntimeEffectController, RuntimeEffectControllerError, RuntimeEffectEnvelope,
    RuntimeEffectLocalExecutor, RuntimeEffectOutcome, ScopedEffectController, SegmentProgress,
};
use crate::{RuntimeError, RuntimeErrorCode, SessionId};

/// The seam operations a [`LayeredEffectHost`] routes through its layer.
///
/// Each hook receives the inner host's controller (or resolver) for the call
/// and defaults to forwarding to it untouched, so a layer implements only what
/// it observes or perturbs. A hook may run code before or after the inner
/// call, replace the local executor it hands down, return an error in place of
/// the inner call, or park forever to model a crash; it must not answer a
/// group operation from state of its own, because the inner substrate is the
/// only arbiter of a group.
#[async_trait::async_trait]
pub trait EffectLayer: Send + Sync + 'static {
    /// Observe the acknowledged native record without changing its body.
    async fn record_run_record(
        &self,
        inner: &dyn RuntimeEffectController,
        name: String,
        step: crate::RunRecordStep<'_>,
    ) -> Result<crate::tool_run::RunJournalEntry, RuntimeEffectControllerError> {
        inner.record_run_record(name, step).await
    }

    async fn record_run_schedule(
        &self,
        inner: &dyn RuntimeEffectController,
        name: String,
        step: crate::RunRecordStep<'_>,
    ) -> Result<crate::tool_run::RunJournalEntry, RuntimeEffectControllerError> {
        inner.record_run_schedule(name, step).await
    }

    fn start_run_attempt<'run>(
        &'run self,
        inner: &'run dyn RuntimeEffectController,
        name: String,
        step: crate::tool_dispatch::RunAttemptStep<'run>,
    ) -> crate::tool_dispatch::RunAttemptHandle<'run> {
        inner.start_run_attempt(name, step)
    }

    /// Whether the layered controller owns commit backpressure, as an
    /// engine-backed controller does.
    fn owns_commit_backpressure(&self, inner: &dyn RuntimeEffectController) -> bool {
        inner.owns_commit_backpressure()
    }

    /// Whether the layered controller hands foreground turns over
    /// ([`RuntimeEffectController::hands_over_turns`]).
    fn hands_over_turns(&self, inner: &dyn RuntimeEffectController) -> bool {
        inner.hands_over_turns()
    }

    async fn revoke_await_events_for_session(
        &self,
        inner: &dyn AwaitEventResolver,
        session_id: &SessionId,
    ) -> Result<(), RuntimeError> {
        inner.revoke_await_events_for_session(session_id).await
    }

    async fn retire_effect_journal(
        &self,
        inner: &dyn EffectHost,
        retirement: EffectJournalRetirement,
    ) -> Result<usize, RuntimeError> {
        inner.retire_effect_journal(retirement).await
    }

    async fn execute_effect(
        &self,
        inner: &dyn RuntimeEffectController,
        envelope: RuntimeEffectEnvelope,
        local_executor: RuntimeEffectLocalExecutor<'_>,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        inner.execute_effect(envelope, local_executor).await
    }

    async fn resolve_await_event(
        &self,
        inner: &dyn AwaitEventResolver,
        key: &AwaitEventKey,
        resolution: Resolution,
    ) -> Result<ResolveOutcome, RuntimeError> {
        inner.resolve_await_event(key, resolution).await
    }

    async fn publish_await_event(
        &self,
        inner: &dyn AwaitEventResolver,
        key: &AwaitEventKey,
        resolution: Resolution,
    ) -> Result<Option<ResolveOutcome>, RuntimeError> {
        inner.publish_await_event(key, resolution).await
    }

    async fn peek_await_event(
        &self,
        inner: &dyn AwaitEventResolver,
        key: &AwaitEventKey,
    ) -> Result<Option<Resolution>, RuntimeError> {
        inner.peek_await_event(key).await
    }

    async fn await_await_event(
        &self,
        inner: &dyn AwaitEventResolver,
        key: &AwaitEventKey,
        cancel: CancellationToken,
    ) -> Result<Resolution, RuntimeError> {
        inner.await_await_event(key, cancel).await
    }
}

/// An [`EffectHost`] that delegates to `inner` and wraps each scoped
/// controller `inner` lends with `layer`.
///
/// Two methods are this host's own so promises cross the layer:
/// `await_event_resolver` answers with this host, and `turn_control_binding`
/// is the trait's composition over this host's resolver and scoped
/// controllers. Everything else is `inner`'s.
///
/// Testing only. A controller `inner` lends for one call is layered for as
/// long as that call's borrow lives; the static and group-child controllers
/// it lends are layered only when they are owned, and the host refuses to
/// lend an unlayered one.
pub struct LayeredEffectHost {
    inner: Arc<dyn EffectHost>,
    layer: Arc<dyn EffectLayer>,
}

impl LayeredEffectHost {
    pub fn new(inner: Arc<dyn EffectHost>, layer: Arc<dyn EffectLayer>) -> Self {
        Self { inner, layer }
    }

    /// The host this one layers.
    pub fn inner(&self) -> &Arc<dyn EffectHost> {
        &self.inner
    }

    fn layered(
        layer: &Arc<dyn EffectLayer>,
        scoped: &ScopedEffectController<'_>,
    ) -> Result<ScopedEffectController<'static>, RuntimeError> {
        let inner = scoped.owned_controller().ok_or_else(|| {
            RuntimeError::new(
                RuntimeErrorCode::RuntimeEffectLocalExecutorUnavailable,
                "a static layered controller needs an owned scoped controller to layer",
            )
        })?;
        let layered = ScopedEffectController::shared(
            Self::layer_controller(inner, Arc::clone(layer)),
            scoped.admitted_scope().clone(),
        )?;
        Ok(match scoped.journal_guard() {
            Some(guard) => layered.with_journal_guard(guard),
            None => layered,
        })
    }

    /// `controller` with `layer` in front of its seam operations: the shared
    /// twin of [`Self::layer_scoped`], for a controller a law holds by value.
    pub fn layer_controller(
        controller: Arc<dyn RuntimeEffectController>,
        layer: Arc<dyn EffectLayer>,
    ) -> Arc<dyn RuntimeEffectController> {
        Arc::new(LayeredController {
            inner: LayeredInner::Shared(controller),
            layer,
        })
    }

    /// `scoped` with `layer` in front of its seam operations, for as long as
    /// `scoped` lives: the controller a tier lends a turn, whatever it is — a
    /// host's owned controller, or one borrowed from an engine handler that
    /// cannot outlive the handler's execution. A rescope of the layered
    /// controller rescopes the one it layers, so a child scope the runtime
    /// narrows to is layered as well, and the command guard `scoped` serves
    /// under stays in front of every journal write.
    pub fn layer_scoped<'run>(
        scoped: ScopedEffectController<'run>,
        layer: Arc<dyn EffectLayer>,
    ) -> Result<ScopedEffectController<'run>, RuntimeError> {
        if scoped.owned_controller().is_some() {
            return Self::layered(&layer, &scoped);
        }
        let admitted = scoped.admitted_scope().clone();
        let guard = scoped.journal_guard();
        let layered = ScopedEffectController::owned(
            Arc::new(LayeredController {
                inner: LayeredInner::Scoped(scoped),
                layer,
            }),
            admitted,
        )?;
        Ok(match guard {
            Some(guard) => layered.with_journal_guard(guard),
            None => layered,
        })
    }

    fn layered_option(
        &self,
        scoped: Option<ScopedEffectController<'static>>,
    ) -> Result<Option<ScopedEffectController<'static>>, RuntimeError> {
        scoped
            .map(|scoped| Self::layer_scoped(scoped, Arc::clone(&self.layer)))
            .transpose()
    }
}

#[async_trait::async_trait]
impl AwaitEventResolver for LayeredEffectHost {
    fn await_event_authority_binding_id(&self) -> Option<String> {
        self.inner.await_event_authority_binding_id()
    }

    async fn prepare_completion_key(
        &self,
        scope: &ExecutionScope,
        wait: AwaitEventWaitIdentity,
        may_defer: bool,
    ) -> Result<CompletionKeyPreparation, RuntimeError> {
        self.inner
            .prepare_completion_key(scope, wait, may_defer)
            .await
    }

    async fn await_event_key(
        &self,
        scope: &ExecutionScope,
        wait: AwaitEventWaitIdentity,
    ) -> Result<AwaitEventKey, RuntimeError> {
        self.inner.await_event_key(scope, wait).await
    }

    async fn resolve_await_event(
        &self,
        key: &AwaitEventKey,
        resolution: Resolution,
    ) -> Result<ResolveOutcome, RuntimeError> {
        self.layer
            .resolve_await_event(self.inner.await_event_resolver(), key, resolution)
            .await
    }

    async fn publish_await_event(
        &self,
        key: &AwaitEventKey,
        resolution: Resolution,
    ) -> Result<Option<ResolveOutcome>, RuntimeError> {
        self.layer
            .publish_await_event(self.inner.await_event_resolver(), key, resolution)
            .await
    }

    async fn peek_await_event(
        &self,
        key: &AwaitEventKey,
    ) -> Result<Option<Resolution>, RuntimeError> {
        self.layer
            .peek_await_event(self.inner.await_event_resolver(), key)
            .await
    }

    async fn await_await_event(
        &self,
        key: &AwaitEventKey,
        cancel: CancellationToken,
    ) -> Result<Resolution, RuntimeError> {
        self.layer
            .await_await_event(self.inner.await_event_resolver(), key, cancel)
            .await
    }

    async fn revoke_await_events_for_session(
        &self,
        session_id: &SessionId,
    ) -> Result<(), RuntimeError> {
        self.layer
            .revoke_await_events_for_session(self.inner.await_event_resolver(), session_id)
            .await
    }

    async fn cancel_await_events_for_session(
        &self,
        session_id: &SessionId,
    ) -> Result<(), RuntimeError> {
        self.inner.cancel_await_events_for_session(session_id).await
    }

    async fn retire_await_events_for_scope(
        &self,
        scope: &ExecutionScope,
    ) -> Result<(), RuntimeError> {
        self.inner.retire_await_events_for_scope(scope).await
    }

    async fn retire_await_events_for_scope_if_quiescent(
        &self,
        scope: &ExecutionScope,
    ) -> Result<bool, RuntimeError> {
        self.inner
            .retire_await_events_for_scope_if_quiescent(scope)
            .await
    }

    async fn reinstate_await_event_scope(
        &self,
        scope: &ExecutionScope,
    ) -> Result<(), RuntimeError> {
        self.inner.reinstate_await_event_scope(scope).await
    }

    async fn await_event_scope_is_retired(
        &self,
        scope: &ExecutionScope,
    ) -> Result<bool, RuntimeError> {
        self.inner.await_event_scope_is_retired(scope).await
    }
}

#[async_trait::async_trait]
impl EffectHost for LayeredEffectHost {
    fn turn_control_binding_id(&self) -> String {
        self.inner.turn_control_binding_id()
    }

    async fn retire_closed_run_waits(
        &self,
        session_id: &SessionId,
        run: &crate::TurnId,
        committed_turn: Option<&crate::TurnId>,
    ) -> Result<(), RuntimeError> {
        self.inner
            .retire_closed_run_waits(session_id, run, committed_turn)
            .await
    }

    async fn list_outstanding_await_event_keys(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<AwaitEventKey>, RuntimeError> {
        self.inner
            .list_outstanding_await_event_keys(session_id)
            .await
    }

    fn turn_attach(&self) -> Option<Arc<dyn crate::TurnAttach>> {
        self.inner.turn_attach()
    }

    fn scoped<'run>(
        &'run self,
        admitted: AdmittedScope,
    ) -> Result<ScopedEffectController<'run>, RuntimeError> {
        Self::layer_scoped(self.inner.scoped(admitted)?, Arc::clone(&self.layer))
    }

    fn scoped_static(
        &self,
        admitted: AdmittedScope,
    ) -> Result<Option<ScopedEffectController<'static>>, RuntimeError> {
        self.layered_option(self.inner.scoped_static(admitted)?)
    }

    /// The inner host's routing first, then this host's layer: a child an
    /// engine handler executes crosses every layer of the stack, innermost
    /// first, as the controllers this host lends do.
    fn route_handler_child_controller<'run>(
        &self,
        controller: ScopedEffectController<'run>,
    ) -> Result<ScopedEffectController<'run>, RuntimeError> {
        Self::layer_scoped(
            self.inner.route_handler_child_controller(controller)?,
            Arc::clone(&self.layer),
        )
    }

    /// The layered host itself, so a turn-control binding composed from this
    /// host resolves and peeks its promises through the layer. The trait's
    /// own `turn_control_binding` is kept for the same reason: it builds the
    /// binding from this host's resolver and scoped controllers.
    fn await_event_resolver(&self) -> &dyn AwaitEventResolver {
        self
    }

    async fn retire_effect_journal(
        &self,
        retirement: EffectJournalRetirement,
    ) -> Result<usize, RuntimeError> {
        self.layer
            .retire_effect_journal(self.inner.as_ref(), retirement)
            .await
    }

    async fn journal_replay(
        &self,
        journal: &lash_sansio::EffectJournalIdentity,
    ) -> Result<crate::JournalReplay, RuntimeError> {
        self.inner.journal_replay(journal).await
    }

    async fn reinstate_effect_scope(&self, scope: &ExecutionScope) -> Result<(), RuntimeError> {
        self.inner.reinstate_effect_scope(scope).await
    }

    fn bind_process_registry(&self, binding: crate::ProcessRegistryBinding) {
        self.inner.bind_process_registry(binding);
    }

    async fn register_turn_cancel_closure_participant(
        &self,
        participant_id: &str,
        scope: &ExecutionScope,
    ) -> Result<(), RuntimeError> {
        self.inner
            .register_turn_cancel_closure_participant(participant_id, scope)
            .await
    }

    async fn release_turn_cancel_closure_participant(
        &self,
        participant_id: &str,
        scope: &ExecutionScope,
    ) -> Result<(), RuntimeError> {
        self.inner
            .release_turn_cancel_closure_participant(participant_id, scope)
            .await
    }
}

/// One scoped controller of the inner host, with the layer in front of its
/// seam operations and everything else forwarded.
struct LayeredController<'run> {
    inner: LayeredInner<'run>,
    layer: Arc<dyn EffectLayer>,
}

/// The controller a [`LayeredController`] layers: a shared one, or a scoped
/// one that may be borrowed and that a rescope rescopes.
enum LayeredInner<'run> {
    Shared(Arc<dyn RuntimeEffectController>),
    Scoped(ScopedEffectController<'run>),
}

impl LayeredInner<'_> {
    fn as_ref(&self) -> &dyn RuntimeEffectController {
        match self {
            Self::Shared(controller) => controller.as_ref(),
            Self::Scoped(scoped) => scoped.controller(),
        }
    }
}

impl<'run> super::ScopeBoundController for LayeredController<'run> {
    #[expect(
        clippy::expect_used,
        reason = "the outer controller's rescope already refused a process \
                  repin, the only refusal the layered one's rescope can make"
    )]
    fn for_scope<'a>(&self, admitted: AdmittedScope) -> Arc<dyn super::ScopeBoundController + 'a>
    where
        Self: 'a,
    {
        let inner = match &self.inner {
            LayeredInner::Shared(controller) => LayeredInner::Shared(Arc::clone(controller)),
            LayeredInner::Scoped(scoped) => LayeredInner::Scoped(
                scoped
                    .rescope(admitted)
                    .expect("the layered controller rescopes with its outer controller"),
            ),
        };
        Arc::new(LayeredController {
            inner,
            layer: Arc::clone(&self.layer),
        })
    }
}

#[async_trait::async_trait]
impl AwaitEventResolver for LayeredController<'_> {
    fn await_event_authority_binding_id(&self) -> Option<String> {
        self.inner.as_ref().await_event_authority_binding_id()
    }

    async fn prepare_completion_key(
        &self,
        scope: &ExecutionScope,
        wait: AwaitEventWaitIdentity,
        may_defer: bool,
    ) -> Result<CompletionKeyPreparation, RuntimeError> {
        self.inner
            .as_ref()
            .prepare_completion_key(scope, wait, may_defer)
            .await
    }

    async fn await_event_key(
        &self,
        scope: &ExecutionScope,
        wait: AwaitEventWaitIdentity,
    ) -> Result<AwaitEventKey, RuntimeError> {
        self.inner.as_ref().await_event_key(scope, wait).await
    }

    async fn resolve_await_event(
        &self,
        key: &AwaitEventKey,
        resolution: Resolution,
    ) -> Result<ResolveOutcome, RuntimeError> {
        self.layer
            .resolve_await_event(self.inner.as_ref(), key, resolution)
            .await
    }

    async fn publish_await_event(
        &self,
        key: &AwaitEventKey,
        resolution: Resolution,
    ) -> Result<Option<ResolveOutcome>, RuntimeError> {
        self.layer
            .publish_await_event(self.inner.as_ref(), key, resolution)
            .await
    }

    async fn peek_await_event(
        &self,
        key: &AwaitEventKey,
    ) -> Result<Option<Resolution>, RuntimeError> {
        self.layer.peek_await_event(self.inner.as_ref(), key).await
    }

    async fn await_await_event(
        &self,
        key: &AwaitEventKey,
        cancel: CancellationToken,
    ) -> Result<Resolution, RuntimeError> {
        self.layer
            .await_await_event(self.inner.as_ref(), key, cancel)
            .await
    }

    async fn revoke_await_events_for_session(
        &self,
        session_id: &SessionId,
    ) -> Result<(), RuntimeError> {
        self.layer
            .revoke_await_events_for_session(self.inner.as_ref(), session_id)
            .await
    }

    async fn cancel_await_events_for_session(
        &self,
        session_id: &SessionId,
    ) -> Result<(), RuntimeError> {
        self.inner
            .as_ref()
            .cancel_await_events_for_session(session_id)
            .await
    }

    async fn retire_await_events_for_scope(
        &self,
        scope: &ExecutionScope,
    ) -> Result<(), RuntimeError> {
        self.inner
            .as_ref()
            .retire_await_events_for_scope(scope)
            .await
    }

    async fn retire_await_events_for_scope_if_quiescent(
        &self,
        scope: &ExecutionScope,
    ) -> Result<bool, RuntimeError> {
        self.inner
            .as_ref()
            .retire_await_events_for_scope_if_quiescent(scope)
            .await
    }

    async fn reinstate_await_event_scope(
        &self,
        scope: &ExecutionScope,
    ) -> Result<(), RuntimeError> {
        self.inner.as_ref().reinstate_await_event_scope(scope).await
    }

    async fn await_event_scope_is_retired(
        &self,
        scope: &ExecutionScope,
    ) -> Result<bool, RuntimeError> {
        self.inner
            .as_ref()
            .await_event_scope_is_retired(scope)
            .await
    }
}

#[async_trait::async_trait]
impl RuntimeEffectController for LayeredController<'_> {
    fn owns_commit_backpressure(&self) -> bool {
        self.layer.owns_commit_backpressure(self.inner.as_ref())
    }

    fn attempt_observation(&self) -> Option<lash_trace::AttemptObservation> {
        self.inner.as_ref().attempt_observation()
    }

    fn hands_over_turns(&self) -> bool {
        self.layer.hands_over_turns(self.inner.as_ref())
    }

    fn wants_segment_boundary(&self, progress: &SegmentProgress) -> Option<BoundaryReason> {
        self.inner.as_ref().wants_segment_boundary(progress)
    }

    async fn observe_process_cancel(
        &self,
        lent_stop: &tokio_util::sync::CancellationToken,
    ) -> Result<bool, RuntimeEffectControllerError> {
        self.inner.as_ref().observe_process_cancel(lent_stop).await
    }

    async fn record_process_drive_step(
        &self,
        name: String,
        step: crate::ProcessDriveStep<'_>,
    ) -> Result<(), RuntimeEffectControllerError> {
        self.inner
            .as_ref()
            .record_process_drive_step(name, step)
            .await
    }

    fn run_record_observer(&self) -> Option<&crate::trace::RunRecordObserver> {
        self.inner.as_ref().run_record_observer()
    }

    async fn record_run_schedule(
        &self,
        name: String,
        step: crate::RunRecordStep<'_>,
    ) -> Result<crate::tool_run::RunJournalEntry, RuntimeEffectControllerError> {
        self.layer
            .record_run_schedule(self.inner.as_ref(), name, step)
            .await
    }

    fn start_run_attempt<'run>(
        &'run self,
        name: String,
        step: crate::tool_dispatch::RunAttemptStep<'run>,
    ) -> crate::tool_dispatch::RunAttemptHandle<'run> {
        self.layer
            .start_run_attempt(self.inner.as_ref(), name, step)
    }

    fn start_run_retry(&self, backoff_ms: u64) -> crate::tool_dispatch::RunRetryTimer<'_> {
        self.inner.as_ref().start_run_retry(backoff_ms)
    }

    async fn arm_run_source(
        &self,
        descriptor: crate::tool_run::SourceDescriptor,
    ) -> Result<(), RuntimeEffectControllerError> {
        self.inner.as_ref().arm_run_source(descriptor).await
    }
    async fn attach_run_process_terminal(
        &self,
        descriptor: crate::tool_run::SourceDescriptor,
    ) -> Result<(), RuntimeEffectControllerError> {
        self.inner
            .as_ref()
            .attach_run_process_terminal(descriptor)
            .await
    }
    async fn cancel_run_source(
        &self,
        descriptor: crate::tool_run::SourceDescriptor,
    ) -> Result<crate::tool_run::SourceSeal, RuntimeEffectControllerError> {
        self.inner.as_ref().cancel_run_source(descriptor).await
    }
    async fn await_run_sources(
        &self,
        subscriptions: Vec<crate::tool_run::SourceSubscription>,
        cancel: crate::TurnCancelWait,
    ) -> Result<(usize, crate::tool_run::SourceSeal), RuntimeEffectControllerError> {
        self.inner
            .as_ref()
            .await_run_sources(subscriptions, cancel)
            .await
    }

    async fn record_run_record(
        &self,
        name: String,
        step: crate::RunRecordStep<'_>,
    ) -> Result<crate::tool_run::RunJournalEntry, RuntimeEffectControllerError> {
        self.layer
            .record_run_record(self.inner.as_ref(), name, step)
            .await
    }

    async fn execute_effect(
        &self,
        envelope: RuntimeEffectEnvelope,
        local_executor: RuntimeEffectLocalExecutor<'_>,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        self.layer
            .execute_effect(self.inner.as_ref(), envelope, local_executor)
            .await
    }

    async fn read_recorded_journal(
        &self,
        range: &crate::RecordedKeyRange,
    ) -> Result<crate::RecordedJournal, crate::RuntimeEffectControllerError> {
        self.inner.as_ref().read_recorded_journal(range).await
    }
}
