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
//! overrides never become the session's. The record also carries the host's
//! termination policy from the root's first execution, which terminal
//! assembly reads, so a worker with another policy assembles the same
//! terminal for the same recorded work (FIG-4389).
//!
//! The record is data. The model binding it records is bound to a live
//! provider handle after the step, on every execution: a handle is this
//! worker's capability, not a decision. A recorded model that cannot be bound
//! here was adopted when it was set, so the failure is the worker's
//! deployment: the root retries, and its engine's retry budget parks it
//! (D3 Q3). A spec's per-run model key is resolved here, once, on the step's
//! first execution; a replay reads the binding the step recorded. A config
//! transaction that changes the model mints its binding when it resolves,
//! and is refused typed if the host serves no such key (FIG-4379).
//!
//! Resolution faults that a redeploy repairs stay out of the record (P3): a
//! spec read the store did not answer, a definition revision this worker
//! does not register, or a per-run model key this worker's models do not
//! serve, ends the attempt unrecorded, so the root retries, parks on its
//! engine's budget, and recovers once the worker serves it. Only a
//! deterministic refusal of the spec is recorded, with its cause typed
//! ([`RunShapeRefusal`](crate::RunShapeRefusal), FIG-4652).

use crate::runtime::LashRuntime;
use crate::runtime::effect::executor::RuntimeEffectLocalRunner;
use crate::{
    EffectAddress, PersistedSessionConfig, RuntimeAttribution, RuntimeEffectCommand,
    RuntimeEffectControllerError, RuntimeEffectEnvelope, RuntimeEffectInvocation,
    RuntimeEffectOutcome, RuntimeError, RuntimeErrorCode, ScopedEffectController, SessionError,
    TurnId,
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
                models: std::sync::Arc::clone(&self.host.core.providers.models),
            }),
        };
        let config_registry = self
            .session
            .as_ref()
            .and_then(|session| session.plugins().host().config_registry().ok());
        let runner = ResolveTurnConfigRunner {
            root: root.clone(),
            snapshot: crate::store::persisted_session_config_from_state(&self.state),
            spec,
            inherited,
            termination: self.host.core.control.termination.clone(),
            follow_on_recoveries: self
                .host
                .core
                .durability
                .queued_work_batching
                .max_follow_on_recoveries(),
            protocol_driver: self
                .session
                .as_ref()
                .map(|session| session.plugins().protocol_driver()),
            config_registry,
        };
        let resolved = controller
            .execute_effect(
                RuntimeEffectEnvelope::new(
                    invocation,
                    RuntimeEffectCommand::ResolveTurnConfig { root: root.clone() },
                ),
                lash_core_execution::core_internal::owned_runner_executor(Box::new(runner), None),
            )
            .await
            .and_then(RuntimeEffectOutcome::into_resolve_turn_config)
            .map_err(RuntimeEffectControllerError::into_runtime_error)?;
        self.apply_turn_config(&resolved)?;
        Ok(())
    }

    /// Preserve the sticky config before installing the recorded execution
    /// view. A replay may name a config that differs from the current head.
    fn apply_turn_config(
        &mut self,
        resolved: &crate::ResolvedRun,
    ) -> Result<(), crate::FormatRefusal> {
        self.install_resolved_run(resolved)?;
        debug_assert_eq!(
            self.state.config_revision, resolved.base.config_revision,
            "a root's resident config revision moved inside the root"
        );
        Ok(())
    }
}

/// A recorded config that selects no model has nothing to run a model call
/// with. The absence is recorded, so no deployment repairs it and no retry
/// changes it: the terminal `ModelUnconfigured`, as a session's open refuses
/// the same head (FIG-4531).
pub(crate) fn model_unconfigured(error: SessionError) -> RuntimeError {
    RuntimeError::new(
        RuntimeErrorCode::ModelUnconfigured,
        format!("the recorded config selects no model: {error}"),
    )
}

/// Why a root's run spec did not resolve, as its `ResolveTurnConfig` step
/// answers it. A model key this worker does not serve is the deployment's
/// fault; a refused shape is the spec's, with its cause typed; and a recorded
/// namespace its owner cannot read is corrupt stored data.
fn run_resolve_fault(
    hash: &impl std::fmt::Display,
    error: crate::RunResolveError,
) -> RuntimeEffectControllerError {
    match error {
        crate::RunResolveError::Model(error) => RuntimeEffectControllerError::model_unavailable(
            &error.key,
            format!(
                "run spec `{hash}` names a model this worker does not serve; the root \
                 retries until a deployment serves it: {error}"
            ),
        ),
        crate::RunResolveError::Refused(refusal) => {
            RuntimeEffectControllerError::run_shape_refused(refusal)
        }
        crate::RunResolveError::RecordedCorrupt(corrupt) => corrupt.into_store_error().into(),
        crate::RunResolveError::Encode(error) => RuntimeEffectControllerError::new(
            RuntimeErrorCode::RecordEncodingFailed,
            format!("run spec `{hash}` could not be encoded: {error}"),
        ),
    }
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
    /// This worker's host termination policy, which a root records on its
    /// first resolution (FIG-4389); a replay decodes the record instead.
    termination: crate::runtime::TerminationPolicy,
    /// This worker's host follow-on recovery bound, recorded with the root
    /// the same way (FIG-4646).
    follow_on_recoveries: u32,
    protocol_driver: Option<std::sync::Arc<dyn crate::plugin::ProtocolDriverPlugin>>,
    /// The session's config owners, which judge every namespace a spec's
    /// overrides changed (FIG-4379).
    config_registry: Option<std::sync::Arc<crate::ConfigRegistry>>,
}

/// A root's non-default spec, read and resolved only on the step's first
/// execution.
struct RootSpec {
    hash: crate::RunSpecHash,
    session_id: crate::SessionId,
    store: crate::store::SessionStore,
    definitions: crate::RunDefinitions,
    models: std::sync::Arc<dyn crate::RuntimeModels>,
}

impl RootSpec {
    /// Resolve this spec against `snapshot` under `termination` and
    /// `follow_on_recoveries`; `owners` apply the protocol options it states.
    /// A fault a redeploy or a retry repairs is marked so it never becomes
    /// the step's recorded outcome.
    async fn resolve(
        self,
        snapshot: &PersistedSessionConfig,
        termination: crate::runtime::TerminationPolicy,
        follow_on_recoveries: u32,
        owners: &dyn crate::RunOptionsOwner,
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
                            RuntimeEffectControllerError::run_shape_refused(
                                crate::RunShapeRefusal::Definition { refusal },
                            )
                        })?,
                )
            }
        };
        spec.resolve(
            snapshot,
            definition,
            termination,
            follow_on_recoveries,
            self.models.as_ref(),
            owners,
        )
        .map_err(|error| run_resolve_fault(&self.hash, error))
    }
}

/// A config judgment's fault as the step's error: a refusal is the root's
/// recorded shape refusal, and a corrupt recorded namespace is corruption.
fn config_fault(fault: crate::ConfigFault) -> RuntimeEffectControllerError {
    match fault {
        crate::ConfigFault::Refused(refusal) => {
            RuntimeEffectControllerError::run_shape_refused(crate::RunShapeRefusal::Owner {
                refusal,
            })
        }
        crate::ConfigFault::RecordedCorrupt(corrupt) => corrupt.into_store_error().into(),
    }
}

#[async_trait::async_trait]
impl RuntimeEffectLocalRunner for ResolveTurnConfigRunner {
    async fn execute(
        self: Box<Self>,
        envelope: RuntimeEffectEnvelope,
        _usage_run: Option<crate::UsageRun>,
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
            (None, None) => crate::ResolvedRun::snapshot(
                self.snapshot,
                self.termination,
                self.follow_on_recoveries,
            ),
            (None, Some(spec)) => {
                let owners: &dyn crate::RunOptionsOwner = match self.config_registry.as_deref() {
                    Some(registry) => registry,
                    None => &crate::NoRunOptionsOwner,
                };
                spec.resolve(
                    &self.snapshot,
                    self.termination,
                    self.follow_on_recoveries,
                    owners,
                )
                .await?
            }
        };
        // An override is judged by the owner of every namespace it changed,
        // as a config command's candidate is: an overlay cannot set what the
        // owner does not admit. The refusal is the root's recorded shape.
        if !inherited
            && resolved.resolved.is_some()
            && let Some(registry) = self.config_registry.as_ref()
        {
            registry
                .validate_derived(&resolved.base, resolved.config())
                .map_err(config_fault)?;
        }
        if !inherited && let Some(driver) = self.protocol_driver {
            let namespace = resolved.config().plugin_config.protocol_turn_options();
            resolved.render = driver
                .resolve_render(&namespace)
                .map_err(|fault| match fault {
                    crate::RenderFault::Refused(refusal) => {
                        RuntimeEffectControllerError::run_shape_refused(
                            crate::RunShapeRefusal::Render { refusal },
                        )
                    }
                    crate::RenderFault::RecordedCorrupt(corrupt) => {
                        corrupt.into_store_error().into()
                    }
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
                    crate::testing::sqlite_memory_store_backend().await,
                    crate::CommitBudget::bounded(1024 * 1024, 512),
                    crate::QueuedWorkBatchingConfig::new(1),
                ),
                crate::testing::runtime_lease_owner(),
            )
            .with_session_id("root-view-authority")
            .with_plugin_factories(crate::testing::test_standard_protocol_factories())
            .with_policy(crate::SessionPolicy {
                model: Some(crate::testing::test_model_config(
                    "test-model",
                    crate::ModelMetadata::builder("test-model")
                        .context_window_tokens(1024)
                        .build()
                        .expect("model"),
                )),
                ..crate::SessionPolicy::new(
                    crate::TurnBudget::Unbounded,
                    crate::MaxToolCalls::new(1024),
                )
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
            capability: "root-view-capability".to_string(),
            depth: 1,
        };
        let mut view = crate::store::persisted_session_config_from_state(&runtime.state);
        assert_ne!(view.tool_access, tool_access);
        assert_ne!(view.subagent.as_ref(), Some(&subagent));
        view.tool_access = tool_access.clone();
        view.subagent = Some(subagent.clone());

        runtime
            .apply_turn_config(&crate::ResolvedRun::snapshot(
                view,
                crate::runtime::TerminationPolicy::default(),
                crate::store::DEFAULT_MAX_FOLLOW_ON_RECOVERIES,
            ))
            .expect("root view formats decode");

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

    /// FIG-4631: a recorded config that selects no model is a recorded
    /// absence no deployment repairs. It is the terminal `ModelUnconfigured`
    /// wherever the runtime meets it, as a session's open refuses the same
    /// head (FIG-4531), and never the retried `ModelUnavailable`.
    #[tokio::test]
    async fn a_recorded_config_that_selects_no_model_is_terminal_model_unconfigured() {
        let session_id = crate::SessionId::from("recorded-without-model");
        let at_the_turn = super::model_unconfigured(crate::SessionError::ModelUnconfigured {
            session_id: session_id.clone(),
        });

        let mut runtime = Box::pin(
            LashRuntime::builder(
                crate::RuntimeHostConfig::new(
                    crate::testing::sqlite_memory_store_backend().await,
                    crate::CommitBudget::bounded(1024 * 1024, 512),
                    crate::QueuedWorkBatchingConfig::new(1),
                ),
                crate::testing::runtime_lease_owner(),
            )
            .with_session_id(session_id.as_str())
            .with_plugin_factories(crate::testing::test_standard_protocol_factories())
            .with_policy(crate::SessionPolicy {
                model: Some(crate::testing::test_model_config(
                    "test-model",
                    crate::ModelMetadata::builder("test-model")
                        .context_window_tokens(1024)
                        .build()
                        .expect("model"),
                )),
                ..crate::SessionPolicy::new(
                    crate::TurnBudget::Unbounded,
                    crate::MaxToolCalls::new(1024),
                )
            })
            .build(),
        )
        .await
        .expect("runtime");
        runtime.state.policy.model = None;
        let at_root_admission = runtime
            .max_context_tokens()
            .expect_err("a config with no model has no prompt budget");

        for refusal in [at_the_turn, at_root_admission] {
            assert_eq!(
                refusal.code,
                crate::RuntimeErrorCode::ModelUnconfigured,
                "{refusal:?}"
            );
            assert!(
                refusal.is_terminal() && !refusal.is_retryable(),
                "no retry changes a recorded absence: {refusal:?}"
            );
            assert_eq!(
                refusal.turn_failure_cause(),
                crate::TurnFailureCause::Outcome,
                "the refusal is the work's outcome: {refusal:?}"
            );
            assert_eq!(refusal.model_key(), None);
            assert!(
                refusal.message.contains(session_id.as_str()),
                "the refusal names the session: {refusal:?}"
            );
        }
    }

    /// FIG-4631: a run spec whose model key this worker does not serve ends
    /// its resolution with the one `ModelUnavailable` fault: the key typed,
    /// the attempt's fault, and the key in the text the engine keeps, so the
    /// park of the exhausted retries names it.
    #[test]
    fn an_unserved_run_spec_key_is_the_typed_attempt_fault() {
        let key = crate::ModelKey::new("kimi-k3@tensorx");
        let fault = super::run_resolve_fault(
            &"spec-hash",
            crate::RunResolveError::Model(crate::ModelUnavailable::new(
                key.clone(),
                crate::ModelUnavailableReason::UnknownKey,
            )),
        );
        assert_eq!(fault.code, crate::RuntimeErrorCode::ModelUnavailable);
        assert_eq!(fault.model_key(), Some(&key), "{fault:?}");
        assert!(
            fault
                .journal_disposition(crate::RuntimeEffectKind::ResolveTurnConfig)
                .is_retryable_derivation(),
            "the fault is never the resolution's recorded result"
        );
        assert!(!fault.is_terminal());
        let park = crate::store::ParkReason::engine_retry_exhausted(
            8,
            None,
            format!("[500] {}", fault.attempt_failure_text()),
        );
        assert_eq!(park.model_key(), Some(&key));
    }
}
