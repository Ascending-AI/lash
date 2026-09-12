//! The one [`AwaitEventResolver`] / [`EffectHost`] / [`RuntimeEffectController`]
//! adapter over a [`StoreEffectReplayDriver`].
//!
//! A SQL store exposes two public types: a deployment-level effect host and a
//! scoped controller, both thin handles on one shared driver. Every method of
//! the three ports those types answer is a forward to the driver plus two
//! backend facts ([`EffectReplayCapabilities`]) — so the forwards live here,
//! once, as blanket impls keyed on three integration traits, and a store
//! implements only those: *which driver* ([`StoreReplayAdapter`]), *I am the
//! host* ([`StoreReplayHost`]), and *I am a controller for this scope*
//! ([`StoreReplayController`]). Before this module each store carried its own
//! copy of all three impls for each of its two types; a change to any port
//! method had to land four times and could drift on each.
//!
//! The integration traits are `#[doc(hidden)]`, like the driver module that
//! owns them: they are the plug-in seam of lash's own SQL tier, not a host
//! API.

use super::*;
use crate::SessionId;
use crate::{AwaitEventResolver, EffectHost, RuntimeEffectController, ScopedEffectController};

/// Names the driver a store-owned host or controller forwards to.
///
/// Implemented by each store's public host and controller types, whose only
/// state is an `Arc` of the driver (plus a scope, for a controller).
#[doc(hidden)]
pub trait StoreReplayAdapter: Send + Sync {
    /// The backend's atomic journal rows.
    type Persistence: EffectReplayRowStore + 'static;
    /// The backend's atomic promise rows.
    type AwaitEvents: AwaitEventBackend + 'static;
    /// The driver this handle shares with every other handle the store minted
    /// over it: one owner id, one lease counter, one replay mode.
    fn replay_driver(&self) -> &Arc<StoreEffectReplayDriver<Self::Persistence, Self::AwaitEvents>>;

    /// Stable await-event authority accepted by this handle, when it
    /// participates in durable turn control.
    fn await_event_authority_binding_id(&self) -> Option<String> {
        None
    }
}

/// Marks a store's deployment-level host: the type that mints scoped
/// controllers. Gets [`EffectHost`] for free.
#[doc(hidden)]
#[async_trait]
pub trait StoreReplayHost: StoreReplayAdapter {
    /// Stable identity of the await-event deployment backing this host.
    fn turn_control_binding_id(&self) -> String;

    /// See [`EffectHost::effect_scope_fence_database`]: the journal file a
    /// session-store factory attaches for the retention sweep, when the
    /// journal lives in a file of its own.
    fn effect_scope_fence_database(&self) -> Option<std::path::PathBuf> {
        None
    }

    /// See [`EffectHost::bind_process_registry`].
    fn bind_process_registry(&self, _binding: crate::ProcessRegistryBinding) {}

    async fn register_turn_cancel_closure_participant(
        &self,
        _participant_id: &str,
        scope: &ExecutionScope,
    ) -> Result<(), RuntimeError> {
        if scope.session_id().is_some() {
            Ok(())
        } else {
            Err(RuntimeError::new(
                RuntimeErrorCode::EffectJournalRetirementUnsupported,
                "store replay host does not implement cancellation-closure lifecycle participation",
            ))
        }
    }

    async fn release_turn_cancel_closure_participant(
        &self,
        _participant_id: &str,
        scope: &ExecutionScope,
    ) -> Result<(), RuntimeError> {
        if scope.session_id().is_some() {
            Ok(())
        } else {
            Err(RuntimeError::new(
                RuntimeErrorCode::EffectJournalRetirementUnsupported,
                "store replay host does not implement cancellation-closure lifecycle participation",
            ))
        }
    }
}

/// Marks a store's scoped controller and names the scope it executes against.
/// Gets [`RuntimeEffectController`] for free.
#[doc(hidden)]
pub trait StoreReplayController: StoreReplayAdapter {
    /// The scope whose journal this controller executes against.
    fn execution_scope(&self) -> &ExecutionScope;
}

/// The controller a [`StoreReplayHost`] mints from [`EffectHost::scoped`]: the
/// host's driver bound to one scope.
struct ScopedStoreReplayController<P, A> {
    driver: Arc<StoreEffectReplayDriver<P, A>>,
    scope: ExecutionScope,
    authority_binding_id: String,
}

impl<P: EffectReplayRowStore + 'static, A: AwaitEventBackend + 'static> StoreReplayAdapter
    for ScopedStoreReplayController<P, A>
{
    type Persistence = P;
    type AwaitEvents = A;
    fn replay_driver(&self) -> &Arc<StoreEffectReplayDriver<P, A>> {
        &self.driver
    }

    fn await_event_authority_binding_id(&self) -> Option<String> {
        Some(self.authority_binding_id.clone())
    }
}

impl<P: EffectReplayRowStore + 'static, A: AwaitEventBackend + 'static> StoreReplayController
    for ScopedStoreReplayController<P, A>
{
    fn execution_scope(&self) -> &ExecutionScope {
        &self.scope
    }
}

fn capabilities<T: StoreReplayAdapter + ?Sized>(adapter: &T) -> EffectReplayCapabilities {
    adapter.replay_driver().row_store.capabilities()
}

#[async_trait]
impl<T: StoreReplayAdapter> AwaitEventResolver for T {
    fn await_event_authority_binding_id(&self) -> Option<String> {
        StoreReplayAdapter::await_event_authority_binding_id(self)
    }

    async fn prepare_completion_key(
        &self,
        scope: &ExecutionScope,
        wait: AwaitEventWaitIdentity,
        may_defer: bool,
    ) -> Result<crate::CompletionKeyPreparation, RuntimeError> {
        if !may_defer {
            return Ok(crate::CompletionKeyPreparation::NotNeeded);
        }
        match capabilities(self).completion_keys {
            CompletionKeys::Unsupported => Ok(crate::CompletionKeyPreparation::Unsupported),
            CompletionKeys::Issued => self
                .await_event_key(scope, wait)
                .await
                .map(crate::CompletionKeyPreparation::Issued),
        }
    }

    async fn await_event_key(
        &self,
        scope: &ExecutionScope,
        wait: AwaitEventWaitIdentity,
    ) -> Result<AwaitEventKey, RuntimeError> {
        self.replay_driver().await_event_key(scope, wait).await
    }

    async fn resolve_await_event(
        &self,
        key: &AwaitEventKey,
        resolution: Resolution,
    ) -> Result<ResolveOutcome, RuntimeError> {
        self.replay_driver()
            .resolve_await_event(key, resolution)
            .await
    }

    async fn peek_await_event(
        &self,
        key: &AwaitEventKey,
    ) -> Result<Option<Resolution>, RuntimeError> {
        self.replay_driver().peek_await_event(key).await
    }

    async fn await_await_event(
        &self,
        key: &AwaitEventKey,
        cancel: CancellationToken,
        deadline: Option<Instant>,
    ) -> Result<Resolution, RuntimeError> {
        self.replay_driver()
            .await_await_event(key, cancel, deadline)
            .await
    }

    async fn revoke_await_events_for_session(
        &self,
        session_id: &SessionId,
    ) -> Result<(), RuntimeError> {
        self.replay_driver()
            .revoke_await_events_for_session(session_id)
            .await
    }

    async fn cancel_await_events_for_session(
        &self,
        session_id: &SessionId,
    ) -> Result<(), RuntimeError> {
        self.replay_driver()
            .cancel_await_events_for_session(session_id)
            .await
    }

    async fn retire_await_events_for_scope(
        &self,
        scope: &ExecutionScope,
    ) -> Result<(), RuntimeError> {
        self.replay_driver()
            .retire_await_events_for_scope(scope)
            .await
    }
}

#[async_trait]
impl<T: StoreReplayHost> EffectHost for T {
    fn turn_control_binding_id(&self) -> String {
        StoreReplayHost::turn_control_binding_id(self)
    }

    async fn list_outstanding_await_event_keys(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<AwaitEventKey>, RuntimeError> {
        self.replay_driver()
            .list_outstanding_await_event_keys(session_id)
            .await
    }

    fn await_event_resolver(&self) -> &dyn AwaitEventResolver {
        self
    }

    fn scoped<'run>(
        &'run self,
        scope: ExecutionScope,
    ) -> Result<ScopedEffectController<'run>, RuntimeError> {
        scope.validate()?;
        let controller = ScopedStoreReplayController {
            driver: Arc::clone(self.replay_driver()),
            scope: scope.clone(),
            authority_binding_id: StoreReplayHost::turn_control_binding_id(self),
        };
        ScopedEffectController::shared(Arc::new(controller), scope)
    }

    fn scoped_static(
        &self,
        scope: ExecutionScope,
    ) -> Result<Option<ScopedEffectController<'static>>, RuntimeError> {
        scope.validate()?;
        let controller = ScopedStoreReplayController {
            driver: Arc::clone(self.replay_driver()),
            scope: scope.clone(),
            authority_binding_id: StoreReplayHost::turn_control_binding_id(self),
        };
        Ok(Some(ScopedEffectController::shared(
            Arc::new(controller),
            scope,
        )?))
    }

    async fn prepare_tool_intent(
        &self,
        _sink: &dyn crate::ToolIntentOutcomeSink,
        _identity: &crate::ToolIntentIdentity,
        _intent: crate::ToolIntent,
    ) -> Result<crate::ToolIntentPreparation, RuntimeError> {
        Ok(crate::ToolIntentPreparation::ControllerOwned)
    }

    async fn record_tool_intent_outcome(
        &self,
        sink: &dyn crate::ToolIntentOutcomeSink,
        identity: &crate::ToolIntentIdentity,
        submitted: crate::ToolIntent,
        outcome: crate::ToolIntentExecutionOutcome,
    ) -> Result<(), RuntimeError> {
        sink.retain_in_journal(identity, submitted, outcome).await
    }

    async fn retire_effect_journal(
        &self,
        retirement: EffectJournalRetirement,
    ) -> Result<usize, RuntimeError> {
        self.replay_driver().retire_effect_journal(retirement).await
    }

    async fn reinstate_effect_scope(&self, scope: &ExecutionScope) -> Result<(), RuntimeError> {
        self.replay_driver().reinstate_effect_scope(scope).await
    }

    fn effect_scope_fence_database(&self) -> Option<std::path::PathBuf> {
        StoreReplayHost::effect_scope_fence_database(self)
    }

    fn bind_process_registry(&self, binding: crate::ProcessRegistryBinding) {
        StoreReplayHost::bind_process_registry(self, binding);
    }

    async fn register_turn_cancel_closure_participant(
        &self,
        participant_id: &str,
        scope: &ExecutionScope,
    ) -> Result<(), RuntimeError> {
        StoreReplayHost::register_turn_cancel_closure_participant(self, participant_id, scope).await
    }

    async fn release_turn_cancel_closure_participant(
        &self,
        participant_id: &str,
        scope: &ExecutionScope,
    ) -> Result<(), RuntimeError> {
        StoreReplayHost::release_turn_cancel_closure_participant(self, participant_id, scope).await
    }
}

#[async_trait]
impl<T: StoreReplayController> RuntimeEffectController for T {
    async fn runtime_effect_failure_disposition(
        &self,
        _code: RuntimeErrorCode,
    ) -> Result<crate::RuntimeEffectFailureDisposition, RuntimeError> {
        Ok(crate::RuntimeEffectFailureDisposition::AbortInvocation)
    }

    async fn turn_control_participation(
        &self,
    ) -> Result<crate::TurnControlParticipation, RuntimeError> {
        Ok(crate::TurnControlParticipation::DurableJournaled)
    }

    async fn execute_effect(
        &self,
        envelope: RuntimeEffectEnvelope,
        local_executor: RuntimeEffectLocalExecutor<'_>,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        let driver = self.replay_driver();
        let scope = self.execution_scope();
        envelope.invocation.validate_execution_scope(scope)?;
        let is_tool_batch = matches!(envelope.command, RuntimeEffectCommand::ToolBatch { .. });
        if is_tool_batch && capabilities(self).tool_batch_redrive == ToolBatchRedrive::ChildrenFirst
        {
            // Re-enter the coordinator on redrive so each child command is
            // reconstructed and crosses its own key-addressed journal row.
            // The aggregate remains durable: after the child drain settles,
            // the ordinary driver records (or validates) the ToolBatch outcome.
            let settled = local_executor.execute(envelope.clone()).await;
            return Box::pin(driver.execute_effect(
                scope,
                envelope,
                RuntimeEffectLocalExecutor::testing(move |_| async move { settled }),
            ))
            .await;
        }
        Box::pin(driver.execute_effect(scope, envelope, local_executor)).await
    }

    /// `true` exactly when this host has a registered
    /// [`GroupExecutors`] resolver.
    ///
    /// The group methods below are implemented against the durable journal, so
    /// the remaining question is where a child's runner comes from: the resolver
    /// is what supplies the `'static` executors the flag's other half requires,
    /// since a child must be able to outlive its caller to honor
    /// [`LoserPolicy::RunToCompletion`](crate::LoserPolicy::RunToCompletion).
    /// A host with no resolver would admit a group and then have nothing to run
    /// it with, which is the drift this answer forecloses.
    fn supports_effect_groups(&self) -> bool {
        self.replay_driver().supports_effect_groups()
    }

    /// Delegated to the shared driver exactly as `execute_effect` is: the group
    /// host is one implementation over [`EffectReplayRowStore`], and a store
    /// contributes the substrate half of it rather than a second copy of the
    /// state machine.
    async fn open_effect_group(
        &self,
        group: crate::RuntimeEffectGroup,
    ) -> Result<crate::EffectGroupHandle, RuntimeEffectControllerError> {
        Box::pin(
            self.replay_driver()
                .open_effect_group(self.execution_scope(), group),
        )
        .await
    }

    async fn await_next_settlement(
        &self,
        handle: &mut crate::EffectGroupHandle,
        cancel: CancellationToken,
    ) -> Result<crate::GroupSettlement, RuntimeEffectControllerError> {
        Box::pin(
            self.replay_driver()
                .await_next_group_settlement(handle, cancel),
        )
        .await
    }

    async fn close_effect_group(
        &self,
        handle: crate::EffectGroupHandle,
        disposition: crate::LoserPolicy,
    ) -> Result<(), RuntimeEffectControllerError> {
        Box::pin(
            self.replay_driver()
                .close_effect_group(&handle, disposition),
        )
        .await
    }
}
