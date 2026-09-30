//! The shape a logical turn runs under (FIG-3600 S6, D3 §2; FIG-3838).
//!
//! A root resolves its shape once, as a recorded step at the top of the
//! logical-turn funnel: after the boundary's command drain and the root's
//! admission, before the first physical turn's first effect. Its first execution
//! resolves the root's [`RunSpec`](crate::RunSpec) (the spec its admitted inputs
//! share; the default spec for a root of wakes or a follow-on) against the
//! root's snapshot, the resident config, which is the durable head's, adopted
//! head-authoritatively under the root's lease one step earlier (the admission's
//! refresh for an input root, the drain's commit for a queued one), and
//! records the result as the root's [`ResolvedRun`](crate::ResolvedRun). Every
//! replay decodes that record instead of reading any live config or spec. So a
//! config change that lands after the root started never reaches the root, a
//! redrive of a committed root replays under the model, prompt and options it
//! ran under, and an input sent after a config command runs under the new
//! config. Every physical turn of the root reuses the one record: commands
//! apply only at turn boundaries. The record is the root's execution view
//! only; commits keep writing the sticky session config, so a spec's
//! overrides never become the session's.
//!
//! The record is data. The route it names is bound to a live provider handle
//! after the step, on every execution: a handle is this worker's capability,
//! not a decision. A route that cannot be bound here was validated when it
//! was set, so the failure is the worker's deployment: the root retries, and
//! its engine's retry budget parks it (D3 Q3). A config command that changes
//! the route is validated when it is sent and when it is applied, and is
//! refused typed if no provider serves it.
//!
//! Resolution faults that a redeploy repairs stay out of the record (P3): a
//! spec read the store did not answer, or a definition revision this worker
//! does not register, ends the attempt unrecorded, so the root retries, parks
//! on its engine's budget, and recovers once the worker serves it. Only a
//! definition's deterministic refusal of the spec's context is recorded.

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
/// the admission (as with `drive-admit:{root}`): a later admission of
/// the same unfinished root replays the config its first execution recorded.
fn turn_config_replay_key(root: &TurnId) -> String {
    format!("turn-config:{root}")
}

impl LashRuntime {
    /// Resolve the shape `root`'s logical turn runs under, as one recorded
    /// step on `controller`, and adopt it as the execution view. `spec` is
    /// the interned spec the root's admitted inputs share, `None` for the
    /// default spec. `inherited` is the shape a recovered follow-on's parent
    /// root recorded at the switch (FIG-3877): the first execution re-records
    /// it under this admission's root verbatim, so the follow-on runs under
    /// the shape its logical run resolved rather than the session's current
    /// defaults.
    pub(in crate::runtime) async fn resolve_turn_config(
        &mut self,
        controller: &ScopedEffectController<'_>,
        root: &TurnId,
        spec: Option<&crate::RunSpecHash>,
        inherited: Option<crate::ResolvedRun>,
    ) -> Result<(), RuntimeError> {
        let invocation = RuntimeEffectInvocation::new(
            EffectAddress::new(
                controller.execution_scope().clone(),
                turn_config_replay_key(root),
            )?,
            RuntimeAttribution::for_turn_admission(self.state.session_id.clone(), root.clone()),
            format!("{root}.turn-config"),
        );
        let spec = match spec {
            None => None,
            Some(hash) => Some(RootSpec {
                hash: hash.clone(),
                session_id: self.state.session_id.clone(),
                store: self
                    .session
                    .as_ref()
                    .and_then(|session| session.history_store())
                    .ok_or_else(|| {
                        RuntimeError::new(
                            RuntimeErrorCode::QueuedWork,
                            format!("a root with run spec `{hash}` needs its session store"),
                        )
                    })?,
                definitions: self.host.core.providers.run_definitions.clone(),
            }),
        };
        let runner = ResolveTurnConfigRunner {
            root: root.clone(),
            snapshot: crate::store::persisted_session_config_from_state(&self.state),
            spec,
            inherited,
            protocol_driver: self
                .session
                .as_ref()
                .map(|session| session.plugins().protocol_driver()),
        };
        let resolved = controller
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
        self.apply_turn_config(&resolved);
        Ok(())
    }

    /// Preserve the sticky config before installing the recorded execution
    /// view. A replay may name a config that differs from the current head.
    fn apply_turn_config(&mut self, resolved: &crate::ResolvedRun) {
        self.install_resolved_run(resolved);
        debug_assert_eq!(
            self.state.config_revision, resolved.base.config_revision,
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

/// The first execution of one `ResolveTurnConfig` step: it resolves the
/// root's spec against the snapshot captured at the funnel and records the
/// result. None of it enters the envelope, which names only the root.
struct ResolveTurnConfigRunner {
    root: TurnId,
    snapshot: PersistedSessionConfig,
    spec: Option<RootSpec>,
    /// The shape a recovered follow-on inherits from its parent root
    /// (FIG-3877); present only on the follow-on's own admission.
    inherited: Option<crate::ResolvedRun>,
    protocol_driver: Option<std::sync::Arc<dyn crate::plugin::ProtocolDriverPlugin>>,
}

/// A root's non-default spec, read and resolved only on the step's first
/// execution.
struct RootSpec {
    hash: crate::RunSpecHash,
    session_id: crate::SessionId,
    store: crate::store::SessionStore,
    definitions: crate::RunDefinitions,
}

impl RootSpec {
    /// Resolve this spec against `snapshot`. A fault a redeploy or a retry
    /// repairs is marked so it never becomes the step's recorded outcome.
    async fn resolve(
        self,
        snapshot: &PersistedSessionConfig,
    ) -> Result<crate::ResolvedRun, RuntimeEffectControllerError> {
        let repairable = |code: RuntimeErrorCode, message: String| {
            RuntimeEffectControllerError::new(code, message).retryable_uncommitted_derivation()
        };
        let spec = self
            .store
            .load_run_spec(&self.hash)
            .await
            .map_err(|error| {
                repairable(
                    RuntimeErrorCode::StoreCommitFailed,
                    format!("run spec `{}` could not be read: {error}", self.hash),
                )
            })?
            .ok_or_else(|| {
                repairable(
                    RuntimeErrorCode::StoreCommitFailed,
                    format!(
                        "run spec `{}` is not interned for session `{}`",
                        self.hash, self.session_id
                    ),
                )
            })?;
        let definition = match &spec.definition {
            None => None,
            Some(reference) => {
                let definition = self.definitions.get(reference).ok_or_else(|| {
                    repairable(
                        RuntimeErrorCode::RunDefinitionUnavailable,
                        format!(
                            "run definition `{reference}` is not registered on this worker; the \
                             root retries until a deployment serves that exact revision"
                        ),
                    )
                })?;
                Some(
                    definition
                        .resolve(snapshot, &spec.context)
                        .map_err(|refusal| {
                            RuntimeEffectControllerError::new(
                                RuntimeErrorCode::RunShapeRefused,
                                refusal.to_string(),
                            )
                        })?,
                )
            }
        };
        spec.resolve(snapshot, definition).map_err(|error| {
            RuntimeEffectControllerError::new(
                RuntimeErrorCode::RunShapeRefused,
                format!("run spec `{}` could not be resolved: {error}", self.hash),
            )
        })
    }
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
        let inherited = self.inherited.is_some();
        let mut resolved = match (self.inherited, self.spec) {
            // A recovered follow-on re-records the shape its parent root
            // resolved, verbatim: it does not re-resolve.
            (Some(inherited), _) => inherited,
            (None, None) => crate::ResolvedRun::snapshot(self.snapshot),
            (None, Some(spec)) => spec.resolve(&self.snapshot).await?,
        };
        if !inherited && let Some(driver) = self.protocol_driver {
            let options = resolved
                .config()
                .protocol_turn_options
                .as_ref()
                .cloned()
                .unwrap_or_default();
            resolved.render = driver.resolve_render(&options).map_err(|message| {
                RuntimeEffectControllerError::new(RuntimeErrorCode::RunShapeRefused, message)
            })?;
        }
        Ok(RuntimeEffectOutcome::ResolveTurnConfig {
            resolved: Box::new(resolved),
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::runtime::LashRuntime;

    /// FIG-4022: a root's recorded execution view can carry an authority
    /// other than the resident one (a replay after a config change, a
    /// recovered follow-on's inherited shape). Installing it must publish
    /// that authority to the live plugin session at once, not at the next
    /// commit's whole-state swap.
    #[tokio::test]
    async fn installing_a_root_execution_view_publishes_its_authority_to_live_plugins() {
        let mut runtime = Box::pin(
            LashRuntime::builder(
                crate::RuntimeHostConfig::new(
                    crate::testing::memory_store_backend().await,
                    crate::CommitBudget::bounded(1024 * 1024, 512),
                    crate::QueuedWorkBatchingConfig::new(1),
                ),
                crate::testing::runtime_lease_owner(),
            )
            .with_session_id("root-view-authority")
            .with_plugin_factories(crate::testing::test_standard_protocol_factories())
            .with_policy(crate::SessionPolicy {
                model: crate::ModelSpec::builder("test-model")
                    .context_window_tokens(1024)
                    .build()
                    .expect("model"),
                ..crate::SessionPolicy::new(crate::TurnBudget::Unbounded)
            })
            .build(),
        )
        .await
        .expect("runtime");
        let head_revision = runtime.state.head_revision;
        let tool_access = crate::SessionToolAccess::ambient()
            .with_hidden_tools(["hidden-by-root-view"])
            .expect("valid hidden tool");
        let subagent = crate::SubagentSessionContext {
            parent_session_id: crate::SessionId::from("root-view-parent"),
            capability: "root-view-capability".to_string(),
            depth: 1,
            max_depth: 3,
        };
        let mut view = crate::store::persisted_session_config_from_state(&runtime.state);
        assert_ne!(view.tool_access, tool_access);
        assert_ne!(view.subagent.as_ref(), Some(&subagent));
        view.tool_access = tool_access.clone();
        view.subagent = Some(subagent.clone());

        runtime.apply_turn_config(&crate::ResolvedRun::snapshot(view));

        let plugins = runtime.plugin_session().expect("live plugin session");
        assert_eq!(
            runtime.state.head_revision, head_revision,
            "nothing committed"
        );
        assert_eq!(runtime.state.authority.tool_access, tool_access);
        assert_eq!(
            plugins.tool_access(),
            tool_access,
            "the live plugin session must see the root view's tool access"
        );
        assert_eq!(
            plugins.subagent_context(),
            Some(subagent),
            "the live plugin session must see the root view's subagent context"
        );
    }
}
