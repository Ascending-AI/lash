//! The session config a logical turn runs under (FIG-3600 S6, D3 §2).
//!
//! A root resolves its config once, as a recorded step at the top of the
//! logical-turn funnel: after the boundary's command drain and the root's
//! claim, before the first physical turn's first effect. Its first execution
//! records the whole config (D3 Q2) the root is about to run under: the
//! resident config, which is the durable head's, adopted head-authoritatively
//! under the root's lease one step earlier (the claim's refresh for an input
//! root, the drain's commit for a queued one). Every replay decodes that
//! record instead of reading any live config. So a config change that lands after the root started
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
            root: root.clone(),
            config: crate::store::persisted_session_config_from_state(&self.state),
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
        self.apply_turn_config(&config);
        Ok(())
    }

    /// Adopt the root's recorded config on resident state: a no-op on the
    /// first execution, which recorded the resident config, and the
    /// correction a replay needs when the live config moved since. Adoption
    /// must leave the resident revision equal to the recorded one — a root's
    /// resident revision never moves inside the root (D3 Q11).
    fn apply_turn_config(&mut self, config: &PersistedSessionConfig) {
        crate::runtime::state::adopt_session_config(&mut self.state, config);
        debug_assert_eq!(
            self.state.config_revision, config.config_revision,
            "a root's resident config revision moved inside the root"
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
        match self.patch_route_refusal(patch, self.state.effective_policy()) {
            Some(refusal) => Err(refusal),
            None => Ok(()),
        }
    }

    /// Whether the drain refuses `patch` at apply (D3 §3.3): its route,
    /// judged over the running `policy`, is one no provider of this host
    /// serves any more. A refused patch changes nothing. The typed `Refused`
    /// settlement and its refused window arrive with the ingress drain
    /// (FIG-3541, S8); the command lane before it settles the command
    /// completed.
    pub(in crate::runtime) fn refuses_route_at_apply(
        &self,
        patch: &crate::runtime::ApplyConfigPatch,
        policy: &crate::SessionPolicy,
    ) -> bool {
        let Some(refusal) = self.patch_route_refusal(patch, policy) else {
            return false;
        };
        tracing::warn!(
            session_id = %self.state.session_id,
            code = %refusal.code.as_str(),
            error = %refusal.message,
            "config command refused at apply"
        );
        true
    }

    /// The refusal of `patch`'s route over `policy`, when it changes the
    /// route and no provider of this host serves the result.
    fn patch_route_refusal(
        &self,
        patch: &crate::runtime::ApplyConfigPatch,
        policy: &crate::SessionPolicy,
    ) -> Option<RuntimeError> {
        if patch.provider_id.is_none() && patch.model.is_none() {
            return None;
        }
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
        .err()
        .map(|code| route_refusal(code, provider_id, model))
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

/// The first execution of one `ResolveTurnConfig` step: it records the
/// config captured at the funnel. None of it enters the envelope, which names
/// only the root.
struct ResolveTurnConfigRunner {
    root: TurnId,
    config: PersistedSessionConfig,
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
        Ok(RuntimeEffectOutcome::ResolveTurnConfig {
            config: Box::new(self.config),
        })
    }
}
