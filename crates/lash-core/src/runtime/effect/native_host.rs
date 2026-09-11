use crate::SessionId;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use tokio_util::sync::CancellationToken;

use super::{
    AwaitEventKey, AwaitEventResolver, AwaitEventWaitIdentity, BoundaryReason,
    CompletionKeyPreparation, EffectGroupHandle, EffectHost, EffectJournalRetirement,
    ExecutionScope, GroupSettlement, LoserPolicy, NativeRuntimeEffectController, Resolution,
    ResolveOutcome, RuntimeEffectController, RuntimeEffectControllerError, RuntimeEffectEnvelope,
    RuntimeEffectFailureDisposition, RuntimeEffectGroup, RuntimeEffectLocalExecutor,
    RuntimeEffectOutcome, ScopedEffectController, SegmentProgress, TurnControlParticipation,
};
use crate::RuntimeError;

/// In-process deployment effect host.
#[derive(Clone)]
pub struct NativeEffectHost {
    turn_control_binding_id: Arc<str>,
    controller: Arc<dyn RuntimeEffectController>,
    allow_process_lifetime_completion_keys: Arc<std::sync::atomic::AtomicBool>,
    /// Effects executing and groups open under each non-session scope, by
    /// journal key: the in-process twin of a journal's `in_progress` rows and
    /// open group rows, which a quiescent-gated retirement must not cut under.
    live: Arc<ScopeLiveness>,
}

/// Admission bookkeeping shared by a host and every scoped controller it
/// hands out. `admission` orders "check the fence, then count as live"
/// against "prove nothing is live, then fence", so a quiescent-gated
/// retirement and an effect starting under the same scope cannot interleave.
#[derive(Default)]
struct ScopeLiveness {
    admission: tokio::sync::Mutex<()>,
    counts: std::sync::Mutex<HashMap<String, usize>>,
}

impl ScopeLiveness {
    fn is_live(&self, scope_key: &str) -> bool {
        self.counts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(scope_key)
            .is_some_and(|count| *count > 0)
    }

    fn enter(self: &Arc<Self>, scope_key: String) -> LiveScopeGuard {
        *self
            .counts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .entry(scope_key.clone())
            .or_insert(0) += 1;
        LiveScopeGuard {
            live: Some(Arc::clone(self)),
            scope_key,
        }
    }

    fn leave(&self, scope_key: &str) {
        let mut counts = self
            .counts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(count) = counts.get_mut(scope_key) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                counts.remove(scope_key);
            }
        }
    }
}

struct LiveScopeGuard {
    live: Option<Arc<ScopeLiveness>>,
    scope_key: String,
}

impl LiveScopeGuard {
    /// Keep the scope counted as live past this guard; the holder releases it
    /// with [`ScopeLiveness::leave`].
    fn keep(mut self) {
        self.live = None;
    }
}

impl Drop for LiveScopeGuard {
    fn drop(&mut self) {
        if let Some(live) = self.live.take() {
            live.leave(&self.scope_key);
        }
    }
}

impl NativeEffectHost {
    pub fn new(controller: Arc<dyn RuntimeEffectController>) -> Self {
        Self {
            turn_control_binding_id: Arc::from(format!("native-process:{}", uuid::Uuid::new_v4())),
            controller,
            allow_process_lifetime_completion_keys: Arc::new(std::sync::atomic::AtomicBool::new(
                false,
            )),
            live: Arc::new(ScopeLiveness::default()),
        }
    }

    /// Explicitly accept that externally routed completion keys die with this
    /// process. Intended only for deliberately single-process embeddings.
    pub fn allow_process_lifetime_completion_keys(self) -> Self {
        self.allow_process_lifetime_completion_keys
            .store(true, std::sync::atomic::Ordering::Relaxed);
        self
    }
}

impl Default for NativeEffectHost {
    fn default() -> Self {
        Self::new(Arc::new(NativeRuntimeEffectController::default()))
    }
}

#[async_trait::async_trait]
impl AwaitEventResolver for NativeEffectHost {
    async fn prepare_completion_key(
        &self,
        scope: &ExecutionScope,
        wait: AwaitEventWaitIdentity,
        may_defer: bool,
    ) -> Result<CompletionKeyPreparation, RuntimeError> {
        if !may_defer {
            return Ok(CompletionKeyPreparation::NotNeeded);
        }
        if self
            .allow_process_lifetime_completion_keys
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            return self
                .await_event_key(scope, wait)
                .await
                .map(CompletionKeyPreparation::Issued);
        }
        self.controller
            .prepare_completion_key(scope, wait, may_defer)
            .await
    }

    async fn await_event_key(
        &self,
        scope: &ExecutionScope,
        wait: AwaitEventWaitIdentity,
    ) -> Result<AwaitEventKey, RuntimeError> {
        self.controller.await_event_key(scope, wait).await
    }

    async fn resolve_await_event(
        &self,
        key: &AwaitEventKey,
        resolution: Resolution,
    ) -> Result<ResolveOutcome, RuntimeError> {
        self.controller.resolve_await_event(key, resolution).await
    }

    async fn peek_await_event(
        &self,
        key: &AwaitEventKey,
    ) -> Result<Option<Resolution>, RuntimeError> {
        self.controller.peek_await_event(key).await
    }

    async fn await_await_event(
        &self,
        key: &AwaitEventKey,
        cancel: CancellationToken,
        deadline: Option<Instant>,
    ) -> Result<Resolution, RuntimeError> {
        self.controller
            .await_await_event(key, cancel, deadline)
            .await
    }

    async fn revoke_await_events_for_session(
        &self,
        session_id: &SessionId,
    ) -> Result<(), RuntimeError> {
        self.controller
            .revoke_await_events_for_session(session_id)
            .await
    }

    async fn cancel_await_events_for_session(
        &self,
        session_id: &SessionId,
    ) -> Result<(), RuntimeError> {
        self.controller
            .cancel_await_events_for_session(session_id)
            .await
    }

    async fn retire_await_events_for_scope(
        &self,
        scope: &ExecutionScope,
    ) -> Result<(), RuntimeError> {
        self.controller.retire_await_events_for_scope(scope).await
    }

    async fn retire_await_events_for_scope_if_quiescent(
        &self,
        scope: &ExecutionScope,
    ) -> Result<bool, RuntimeError> {
        self.controller
            .retire_await_events_for_scope_if_quiescent(scope)
            .await
    }

    async fn reinstate_await_event_scope(
        &self,
        scope: &ExecutionScope,
    ) -> Result<(), RuntimeError> {
        self.controller.reinstate_await_event_scope(scope).await
    }

    async fn await_event_scope_is_retired(
        &self,
        scope: &ExecutionScope,
    ) -> Result<bool, RuntimeError> {
        self.controller.await_event_scope_is_retired(scope).await
    }
}

#[async_trait::async_trait]
impl EffectHost for NativeEffectHost {
    fn turn_control_binding_id(&self) -> String {
        self.turn_control_binding_id.to_string()
    }
    fn turn_control_authority_owner(&self) -> super::TurnControlAuthorityOwner {
        super::TurnControlAuthorityOwner::SessionStore
    }

    fn await_event_resolver(&self) -> &dyn crate::AwaitEventResolver {
        self
    }

    /// The in-memory host keeps no journal, so there is no claim for the
    /// fence to refuse at: the scoped controller consults the fence itself
    /// before every effect and group it runs under the scope, exactly where
    /// the durable hosts read the fence at claim time.
    fn scoped<'run>(
        &'run self,
        scope: ExecutionScope,
    ) -> Result<ScopedEffectController<'run>, RuntimeError> {
        ScopedEffectController::shared(self.fenced_controller(scope.clone()), scope)
    }

    fn scoped_static(
        &self,
        scope: ExecutionScope,
    ) -> Result<Option<ScopedEffectController<'static>>, RuntimeError> {
        Ok(Some(ScopedEffectController::shared(
            self.fenced_controller(scope.clone()),
            scope,
        )?))
    }

    /// The in-memory host keeps no effect journal, so no journal rows are ever
    /// deleted here (the count is always 0). A scope-exact retirement still
    /// performs the promise half: the scope's in-process promises are dropped
    /// and the scope is fenced, mirroring what the durable hosts do in one
    /// transaction. Session retirements stay a no-op: session promises are
    /// revoked through the session lever the host already calls.
    ///
    /// A `WhenQuiescent` retirement is refused with `effect_scope_not_quiescent`
    /// while an effect is executing or a group is open under the scope through
    /// a controller this host handed out, or while a waiter is parked on one
    /// of the scope's promises; the proof and the fence are taken under the
    /// admission lock every scoped controller enters through.
    async fn retire_effect_journal(
        &self,
        retirement: EffectJournalRetirement,
    ) -> Result<usize, RuntimeError> {
        let Some(scope) = retirement.retired_scope() else {
            return Ok(0);
        };
        if retirement.gate() == Some(super::EffectRetirementGate::WhenQuiescent) {
            let key = scope.journal_identity()?.key().to_string();
            let _admission = self.live.admission.lock().await;
            if self.live.is_live(&key)
                || !self
                    .controller
                    .retire_await_events_for_scope_if_quiescent(&scope)
                    .await?
            {
                return Err(super::effect_replay_driver::scope_not_quiescent(&key));
            }
            return Ok(0);
        }
        self.controller
            .retire_await_events_for_scope(&scope)
            .await?;
        Ok(0)
    }

    async fn reinstate_effect_scope(&self, scope: &ExecutionScope) -> Result<(), RuntimeError> {
        self.controller.reinstate_await_event_scope(scope).await
    }
}

impl NativeEffectHost {
    fn fenced_controller(&self, scope: ExecutionScope) -> Arc<dyn RuntimeEffectController> {
        Arc::new(FencedNativeController {
            host: self.clone(),
            scope,
        })
    }
}

/// The controller a [`NativeEffectHost`] hands out for one scope: every
/// effect and group it runs is refused with `effect_scope_retired` once the
/// scope has been retired, so a late redrive under a retired process or
/// runtime operation fails closed in-process exactly as it does against a
/// durable journal. Everything else forwards to the host.
struct FencedNativeController {
    host: NativeEffectHost,
    scope: ExecutionScope,
}

impl FencedNativeController {
    /// Refuse a retired scope, else count the caller as live under it for as
    /// long as the returned guard lives. Both happen under the admission lock
    /// a quiescent-gated retirement takes, so the fence and the liveness
    /// count cannot cross.
    async fn admit(&self) -> Result<Option<LiveScopeGuard>, RuntimeEffectControllerError> {
        let _admission = self.host.live.admission.lock().await;
        if self
            .host
            .controller
            .await_event_scope_is_retired(&self.scope)
            .await?
        {
            let identity = self.scope.journal_identity()?;
            return Err(super::effect_replay_driver::scope_retired(identity.key()));
        }
        if self.scope.session_id().is_some() {
            return Ok(None);
        }
        let key = self.scope.journal_identity()?.key().to_string();
        Ok(Some(self.host.live.enter(key)))
    }
}

#[async_trait::async_trait]
impl AwaitEventResolver for FencedNativeController {
    async fn prepare_completion_key(
        &self,
        scope: &ExecutionScope,
        wait: AwaitEventWaitIdentity,
        may_defer: bool,
    ) -> Result<CompletionKeyPreparation, RuntimeError> {
        self.host
            .prepare_completion_key(scope, wait, may_defer)
            .await
    }

    async fn await_event_key(
        &self,
        scope: &ExecutionScope,
        wait: AwaitEventWaitIdentity,
    ) -> Result<AwaitEventKey, RuntimeError> {
        self.host.await_event_key(scope, wait).await
    }

    async fn resolve_await_event(
        &self,
        key: &AwaitEventKey,
        resolution: Resolution,
    ) -> Result<ResolveOutcome, RuntimeError> {
        self.host.resolve_await_event(key, resolution).await
    }

    async fn peek_await_event(
        &self,
        key: &AwaitEventKey,
    ) -> Result<Option<Resolution>, RuntimeError> {
        self.host.peek_await_event(key).await
    }

    async fn await_await_event(
        &self,
        key: &AwaitEventKey,
        cancel: CancellationToken,
        deadline: Option<Instant>,
    ) -> Result<Resolution, RuntimeError> {
        self.host.await_await_event(key, cancel, deadline).await
    }

    async fn revoke_await_events_for_session(
        &self,
        session_id: &SessionId,
    ) -> Result<(), RuntimeError> {
        self.host.revoke_await_events_for_session(session_id).await
    }

    async fn cancel_await_events_for_session(
        &self,
        session_id: &SessionId,
    ) -> Result<(), RuntimeError> {
        self.host.cancel_await_events_for_session(session_id).await
    }

    async fn retire_await_events_for_scope(
        &self,
        scope: &ExecutionScope,
    ) -> Result<(), RuntimeError> {
        self.host.retire_await_events_for_scope(scope).await
    }

    async fn retire_await_events_for_scope_if_quiescent(
        &self,
        scope: &ExecutionScope,
    ) -> Result<bool, RuntimeError> {
        self.host
            .retire_await_events_for_scope_if_quiescent(scope)
            .await
    }

    async fn reinstate_await_event_scope(
        &self,
        scope: &ExecutionScope,
    ) -> Result<(), RuntimeError> {
        self.host
            .controller
            .reinstate_await_event_scope(scope)
            .await
    }

    async fn await_event_scope_is_retired(
        &self,
        scope: &ExecutionScope,
    ) -> Result<bool, RuntimeError> {
        self.host
            .controller
            .await_event_scope_is_retired(scope)
            .await
    }
}

#[async_trait::async_trait]
impl RuntimeEffectController for FencedNativeController {
    fn owns_commit_backpressure(&self) -> bool {
        self.host.owns_commit_backpressure()
    }

    fn wants_segment_boundary(&self, progress: &SegmentProgress) -> Option<BoundaryReason> {
        self.host.wants_segment_boundary(progress)
    }

    fn supports_concurrent_effects(&self) -> bool {
        self.host.supports_concurrent_effects()
    }

    async fn runtime_effect_failure_disposition(
        &self,
        code: crate::RuntimeErrorCode,
    ) -> Result<RuntimeEffectFailureDisposition, RuntimeError> {
        self.host.runtime_effect_failure_disposition(code).await
    }

    async fn turn_control_participation(&self) -> Result<TurnControlParticipation, RuntimeError> {
        self.host.turn_control_participation().await
    }

    async fn execute_effect(
        &self,
        envelope: RuntimeEffectEnvelope,
        local_executor: RuntimeEffectLocalExecutor<'_>,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        let _live = self.admit().await?;
        self.host.execute_effect(envelope, local_executor).await
    }

    fn supports_effect_groups(&self) -> bool {
        self.host.supports_effect_groups()
    }

    async fn open_effect_group(
        &self,
        group: RuntimeEffectGroup,
    ) -> Result<EffectGroupHandle, RuntimeEffectControllerError> {
        let live = self.admit().await?;
        let handle = self.host.open_effect_group(group).await?;
        // An open group stays live until it is closed through this
        // controller; the guard is released in `close_effect_group`.
        if let Some(live) = live {
            live.keep();
        }
        Ok(handle)
    }

    async fn await_next_settlement(
        &self,
        handle: &mut EffectGroupHandle,
        cancel: CancellationToken,
    ) -> Result<GroupSettlement, RuntimeEffectControllerError> {
        self.host.await_next_settlement(handle, cancel).await
    }

    async fn close_effect_group(
        &self,
        handle: EffectGroupHandle,
        disposition: LoserPolicy,
    ) -> Result<(), RuntimeEffectControllerError> {
        let result = self.host.close_effect_group(handle, disposition).await;
        if self.scope.session_id().is_none()
            && let Ok(identity) = self.scope.journal_identity()
        {
            self.host.live.leave(identity.key());
        }
        result
    }
}

#[async_trait::async_trait]
impl RuntimeEffectController for NativeEffectHost {
    fn owns_commit_backpressure(&self) -> bool {
        self.controller.owns_commit_backpressure()
    }

    fn wants_segment_boundary(&self, progress: &SegmentProgress) -> Option<BoundaryReason> {
        self.controller.wants_segment_boundary(progress)
    }

    fn supports_concurrent_effects(&self) -> bool {
        self.controller.supports_concurrent_effects()
    }

    async fn runtime_effect_failure_disposition(
        &self,
        code: crate::RuntimeErrorCode,
    ) -> Result<RuntimeEffectFailureDisposition, RuntimeError> {
        self.controller
            .runtime_effect_failure_disposition(code)
            .await
    }

    async fn turn_control_participation(&self) -> Result<TurnControlParticipation, RuntimeError> {
        self.controller.turn_control_participation().await
    }

    async fn execute_effect(
        &self,
        envelope: RuntimeEffectEnvelope,
        local_executor: RuntimeEffectLocalExecutor<'_>,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        self.controller
            .execute_effect(envelope, local_executor)
            .await
    }

    // The group methods below must stay forwarded, never trait-defaulted:
    // supports_effect_groups() would otherwise advertise a capability the
    // defaults refuse.
    fn supports_effect_groups(&self) -> bool {
        self.controller.supports_effect_groups()
    }

    async fn open_effect_group(
        &self,
        group: RuntimeEffectGroup,
    ) -> Result<EffectGroupHandle, RuntimeEffectControllerError> {
        self.controller.open_effect_group(group).await
    }

    async fn await_next_settlement(
        &self,
        handle: &mut EffectGroupHandle,
        cancel: CancellationToken,
    ) -> Result<GroupSettlement, RuntimeEffectControllerError> {
        self.controller.await_next_settlement(handle, cancel).await
    }

    async fn close_effect_group(
        &self,
        handle: EffectGroupHandle,
        disposition: LoserPolicy,
    ) -> Result<(), RuntimeEffectControllerError> {
        self.controller
            .close_effect_group(handle, disposition)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        AwaitEventWaitIdentity, EffectJournalRetirement, RuntimeEffectCommand, RuntimeEffectKind,
        RuntimeInvocation, RuntimeScope,
    };

    fn envelope(effect_id: &str) -> RuntimeEffectEnvelope {
        RuntimeEffectEnvelope::new(
            RuntimeInvocation::effect(
                RuntimeScope::for_turn("native-fence-session", "native-fence-turn", 1, 0),
                effect_id,
                RuntimeEffectKind::LanguageRuntimeValue,
                effect_id,
            ),
            RuntimeEffectCommand::LanguageRuntimeValue {
                operation: effect_id.to_string(),
            },
        )
    }

    fn executor() -> RuntimeEffectLocalExecutor<'static> {
        RuntimeEffectLocalExecutor::testing(|_| async {
            Ok(RuntimeEffectOutcome::LanguageRuntimeValue {
                value: serde_json::json!({ "ran": true }),
            })
        })
    }

    /// The in-process fence is a set, not a ring: the oldest retired scope
    /// stays fenced after thousands of later retirements (FIG-2499 review round 1).
    #[tokio::test]
    async fn retired_scope_stays_fenced_after_many_later_retirements() {
        let host = NativeEffectHost::default();
        let oldest = ExecutionScope::runtime_operation("oldest");
        host.retire_effect_journal(
            EffectJournalRetirement::for_scope(&oldest).expect("runtime operations retire"),
        )
        .await
        .expect("retire the oldest scope");
        for i in 0..4_097 {
            host.retire_effect_journal(EffectJournalRetirement::runtime_operation(format!(
                "later-{i}"
            )))
            .await
            .expect("retire a later scope");
        }
        let err = host
            .await_event_key(&oldest, AwaitEventWaitIdentity::tool_completion("late"))
            .await
            .expect_err("the oldest scope is still fenced");
        assert_eq!(err.code.as_str(), "await_event_unknown_or_revoked");
    }

    /// The scoped controller the native host hands out refuses effects and
    /// groups under a retired scope, exactly where a durable host refuses
    /// the claim (FIG-2499 review round 1).
    #[tokio::test]
    async fn scoped_controller_refuses_effects_under_a_retired_scope() {
        let host = NativeEffectHost::default();
        let scope = ExecutionScope::runtime_operation("retired");
        host.scoped(scope.clone())
            .expect("scope binds")
            .controller()
            .execute_effect(envelope("before"), executor())
            .await
            .expect("an unretired scope runs effects");
        host.retire_effect_journal(
            EffectJournalRetirement::for_scope(&scope).expect("runtime operations retire"),
        )
        .await
        .expect("retire the scope");
        let refusal = host
            .scoped(scope.clone())
            .expect("a retired scope still binds a controller")
            .controller()
            .execute_effect(envelope("after"), executor())
            .await
            .expect_err("a retired scope runs nothing");
        assert_eq!(refusal.code, crate::RuntimeErrorCode::EffectScopeRetired);
        let refusal = host
            .scoped_static(scope.clone())
            .expect("scope binds")
            .expect("the native host hands out owned controllers")
            .controller()
            .execute_effect(envelope("after-static"), executor())
            .await
            .expect_err("the owned controller is fenced too");
        assert_eq!(refusal.code, crate::RuntimeErrorCode::EffectScopeRetired);
        let session_scope = ExecutionScope::turn("native-fence-session", "turn-1");
        host.scoped(session_scope)
            .expect("session scope binds")
            .controller()
            .execute_effect(envelope("session"), executor())
            .await
            .expect("session scopes are never fenced by the scope lever");
    }

    /// Reinstating a pruned process id lifts its fence and nothing else: the
    /// new incarnation runs effects and mints promises again (ADR 0049).
    #[tokio::test]
    async fn reinstate_lifts_a_process_scope_fence() {
        let host = NativeEffectHost::default();
        let scope = ExecutionScope::process("reused-process");
        host.retire_effect_journal(EffectJournalRetirement::process("reused-process"))
            .await
            .expect("prune retires the process scope");
        host.scoped(scope.clone())
            .expect("scope binds")
            .controller()
            .execute_effect(envelope("fenced"), executor())
            .await
            .expect_err("a pruned process id runs nothing");
        host.reinstate_effect_scope(&scope)
            .await
            .expect("re-registration lifts the fence");
        host.scoped(scope.clone())
            .expect("scope binds")
            .controller()
            .execute_effect(envelope("reinstated"), executor())
            .await
            .expect("the re-registered incarnation runs effects");
        host.await_event_key(
            &scope,
            AwaitEventWaitIdentity::tool_completion("reinstated"),
        )
        .await
        .expect("the re-registered incarnation mints");
        let refused = host
            .reinstate_effect_scope(&ExecutionScope::turn("native-fence-session", "turn-2"))
            .await
            .expect_err("session scopes are not reinstated through the scope lever");
        assert_eq!(refused.code.as_str(), "await_event_scope_not_retirable");
    }
}
