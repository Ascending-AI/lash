//! The session config a logical turn runs under (FIG-3600 S6, D3 §2).
//!
//! A root resolves its config once, as a recorded step at the top of the
//! logical-turn funnel: after the boundary's command drain and the root's
//! claim, before the first physical turn's first effect. Its first execution
//! reads the whole config from the durable head (D3 Q2); every replay decodes
//! that record instead. So a config change that lands after the root started
//! never reaches the root, a redrive of a committed root replays under the
//! model, prompt and options it ran under, and an input sent after a config
//! command runs under the new config. Every physical turn of the root reuses
//! the one record: commands apply only at turn boundaries.
//!
//! The record is data. The route it names is bound to a live provider handle
//! after the step, on every execution: a handle is this worker's capability,
//! not a decision. A route that cannot be bound here was validated when it
//! was set, so the failure is the worker's deployment: the root retries, and
//! its engine's retry budget parks it (D3 Q3). A config command that changes
//! the route is validated when it is sent and when it is applied, and is
//! refused typed if no provider serves it.

use std::sync::Arc;

use crate::provider::{ConfigRefusalCode, RuntimeProviderResolver};
use crate::runtime::LashRuntime;
use crate::runtime::effect::executor::RuntimeEffectLocalRunner;
use crate::{
    EffectAddress, ModelSpec, PersistedSessionConfig, RuntimeAttribution, RuntimeEffectCommand,
    RuntimeEffectControllerError, RuntimeEffectEnvelope, RuntimeEffectInvocation,
    RuntimeEffectLocalExecutor, RuntimeEffectOutcome, RuntimeError, RuntimeErrorCode,
    ScopedEffectController, SessionError, TurnId,
};

/// The replay key of `root`'s config record. Keyed by the root, never by
/// the admission (the `drive-claim:{root}` precedent): a later admission of
/// the same unfinished root replays the config its first execution recorded.
fn turn_config_replay_key(root: &TurnId) -> String {
    format!("turn-config:{root}")
}

impl LashRuntime {
    /// Resolve the config `root`'s logical turn runs under, as one recorded
    /// step on `controller`, and adopt it on resident state.
    pub(in crate::runtime) async fn resolve_turn_config(
        &mut self,
        controller: &ScopedEffectController<'_>,
        root: &TurnId,
    ) -> Result<(), RuntimeError> {
        let invocation = RuntimeEffectInvocation::new(
            EffectAddress::new(
                controller.execution_scope().clone(),
                turn_config_replay_key(root),
            )?,
            RuntimeAttribution::for_turn_admission(self.state.session_id.clone(), root.clone()),
            format!("{root}.turn-config"),
        );
        let runner = ResolveTurnConfigRunner {
            store: self
                .session
                .as_ref()
                .and_then(|session| session.history_store()),
            root: root.clone(),
            head_revision: self.state.head_revision,
            resident: crate::store::persisted_session_config_from_state(&self.state),
        };
        let config = controller
            .execute_effect(
                RuntimeEffectEnvelope::new(
                    invocation,
                    RuntimeEffectCommand::ResolveTurnConfig { root: root.clone() },
                ),
                RuntimeEffectLocalExecutor::owned_runner(Box::new(runner), None),
            )
            .await
            .and_then(RuntimeEffectOutcome::into_resolve_turn_config)
            .map_err(RuntimeEffectControllerError::into_runtime_error)?;
        self.apply_turn_config(root, &config);
        Ok(())
    }

    /// Adopt `root`'s recorded config on resident state: a no-op on the first
    /// execution, which read it from the head the resident state carries, and
    /// the correction a replay needs when the live config moved since.
    fn apply_turn_config(&mut self, root: &TurnId, config: &PersistedSessionConfig) {
        crate::runtime::state::adopt_session_config(&mut self.state, config);
        tracing::info!(
            session_id = %self.state.session_id,
            root = %root,
            provider_id = %config.provider_id,
            model = %config.model.id,
            config_revision = config.config_revision,
            "turn config resolved"
        );
    }

    /// Refuse a config command whose route no provider of this host serves
    /// (D3 §3.1), before anything is enqueued. A command that leaves the
    /// route alone is not judged.
    pub(in crate::runtime) fn refuse_unservable_route(
        &self,
        command: &crate::SessionCommand,
    ) -> Result<(), RuntimeError> {
        let crate::SessionCommand::ApplyConfigPatch { patch } = command else {
            return Ok(());
        };
        if patch.provider_id.is_none() && patch.model.is_none() {
            return Ok(());
        }
        let policy = self.state.effective_policy();
        let provider_id = patch
            .provider_id
            .as_deref()
            .unwrap_or_else(|| policy.recorded_provider_id());
        let model = patch.model.as_ref().unwrap_or(&policy.model);
        validate_route(
            self.host.core.providers.provider_resolver.as_ref(),
            provider_id,
            model,
        )
        .map_err(|code| route_refusal(code, provider_id, model))
    }
}

/// Whether `resolver` serves the route `provider_id` + `model` (D3 §3.3).
pub fn validate_route(
    resolver: &dyn RuntimeProviderResolver,
    provider_id: &str,
    model: &ModelSpec,
) -> Result<(), ConfigRefusalCode> {
    resolver.validate_route(provider_id, model)
}

/// The route validator a config-command planner applies at the drain
/// (D3 §3.3): [`validate_route`] over `resolver`.
pub fn validate_route_with(
    resolver: &dyn RuntimeProviderResolver,
) -> impl Fn(&str, &ModelSpec) -> Result<(), ConfigRefusalCode> + '_ {
    move |provider_id, model| validate_route(resolver, provider_id, model)
}

/// The typed refusal of a config command whose route `code` refused.
pub(crate) fn route_refusal(
    code: ConfigRefusalCode,
    provider_id: &str,
    model: &ModelSpec,
) -> RuntimeError {
    let runtime_code = match code {
        ConfigRefusalCode::ProviderRouteUnknown => RuntimeErrorCode::ProviderRouteUnknown,
        ConfigRefusalCode::ProviderCredentialsMissing => {
            RuntimeErrorCode::ProviderCredentialsMissing
        }
    };
    RuntimeError::new(
        runtime_code,
        format!(
            "config command refused: {code} (provider `{provider_id}`, model `{}`)",
            model.id
        ),
    )
}

/// A turn's recorded route that this worker cannot bind: retried, never the
/// turn's outcome (D3 Q3).
pub(crate) fn provider_binding_unavailable(error: SessionError) -> RuntimeError {
    RuntimeError::new(
        RuntimeErrorCode::ProviderBindingUnavailable,
        format!("the turn's recorded provider route cannot be bound on this worker: {error}"),
    )
}

/// The first execution of one `ResolveTurnConfig` step.
///
/// Everything it needs is captured at the funnel; none of it enters the
/// envelope, which names only the root.
struct ResolveTurnConfigRunner {
    store: Option<Arc<dyn crate::store::RuntimePersistence>>,
    root: TurnId,
    /// The head revision of the resident state the root runs on: its
    /// admitted base for an input root, the post-drain head for a queued one.
    head_revision: u64,
    /// The resident config, recorded when the session has no durable head:
    /// a session that never committed runs under the config it was built
    /// with.
    resident: PersistedSessionConfig,
}

#[async_trait::async_trait]
impl RuntimeEffectLocalRunner for ResolveTurnConfigRunner {
    async fn execute(
        self: Box<Self>,
        envelope: RuntimeEffectEnvelope,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        let RuntimeEffectCommand::ResolveTurnConfig { root } = &envelope.command else {
            return Err(RuntimeEffectControllerError::new(
                RuntimeErrorCode::RuntimeEffectLocalExecutorMismatch,
                format!(
                    "turn-config executor cannot execute {} command",
                    envelope.command.kind().as_str()
                ),
            ));
        };
        if *root != self.root {
            return Err(RuntimeEffectControllerError::new(
                RuntimeErrorCode::RuntimeEffectLocalExecutorMismatch,
                format!(
                    "turn-config executor was bound to root `{}` but asked to resolve `{root}`",
                    self.root
                ),
            ));
        }
        let config = self.resolve().await?;
        Ok(RuntimeEffectOutcome::ResolveTurnConfig {
            config: Box::new(config),
        })
    }
}

impl ResolveTurnConfigRunner {
    /// The durable head's config, when the head is the one the root runs
    /// on. A store that did not answer, or a head that moved under the
    /// root, is this attempt's fault, never the step's record: the engine
    /// runs the step again.
    async fn resolve(self) -> Result<PersistedSessionConfig, RuntimeEffectControllerError> {
        let Some(store) = self.store else {
            return Ok(self.resident);
        };
        let head = store.load_session_head_meta().await.map_err(|error| {
            let mut fault = RuntimeEffectControllerError::from(
                crate::runtime::runtime_error_from_store_commit(error),
            );
            fault.message = format!("turn-config head read: {}", fault.message);
            fault.retryable_uncommitted_derivation()
        })?;
        let Some(head) = head else {
            return Ok(self.resident);
        };
        if head.head_revision != self.head_revision {
            return Err(RuntimeEffectControllerError::new(
                RuntimeErrorCode::SessionHeadRefresh,
                format!(
                    "root `{}` runs on head revision {} but the durable head is at {}; its \
                     config is read again once the resident head is current",
                    self.root, self.head_revision, head.head_revision
                ),
            )
            .retryable_uncommitted_derivation());
        }
        Ok(head.config)
    }
}
