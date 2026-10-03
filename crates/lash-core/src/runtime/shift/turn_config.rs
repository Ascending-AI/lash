//! The shape a logical turn runs under (FIG-3600 S6, D3 §2; FIG-3838).
//!
//! A run resolves its shape once, as a recorded step at the top of the
//! logical-turn funnel: after the boundary's command drain and the run's
//! admission, before the first physical turn's first effect. Its first execution
//! resolves the run's [`RunSpec`](crate::RunSpec) (the spec its admitted inputs
//! share; the default spec for a run of wakes or a follow-on) against the
//! run's snapshot, the resident config, which is the durable head's, adopted
//! head-authoritatively under the run's lease one step earlier (the admission's
//! refresh for an input run, the drain's commit for a queued one), and
//! records the result as the run's [`ResolvedRun`](crate::ResolvedRun). Every
//! replay decodes that record instead of reading any live config or spec. So a
//! config change that lands after the run started never reaches the run, a
//! redrive of a committed run replays under the model, prompt and options it
//! ran under, and an input sent after a config command runs under the new
//! config. Every physical turn of the run reuses the one record: commands
//! apply only at turn boundaries. The record is the run's execution view
//! only; commits keep writing the sticky session config, so a spec's
//! overrides never become the session's. The record also carries the host's
//! termination policy from the run's first execution, which terminal
//! assembly reads, so a worker with another policy assembles the same
//! terminal for the same recorded work (FIG-4389).
//!
//! The record is data. The model binding it records is bound to a live
//! provider handle after the step, on every execution: a handle is this
//! worker's capability, not a decision. A recorded model that cannot be bound
//! here was adopted when it was set, so the failure is the worker's
//! deployment: the run retries, and its engine's retry budget parks it
//! (D3 Q3). A spec's per-run model key is resolved here, once, on the step's
//! first execution; a replay reads the binding the step recorded. A config
//! transaction that changes the model mints its binding when it resolves,
//! and is refused typed if the host serves no such key (FIG-4379).
//!
//! Resolution faults that a redeploy repairs stay out of the record (P3): a
//! spec read the store did not answer, a definition revision this worker
//! does not register, or a per-run model key this worker's models do not
//! serve, ends the attempt unrecorded, so the run retries, parks on its
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

/// The replay key of `run`'s config record. Keyed by the run, never by
/// the admission (as with `shift-admit:{run}`): a later admission of
/// the same unfinished run replays the config its first execution recorded.
fn turn_config_replay_key(run: &TurnId) -> String {
    format!("turn-config:{run}")
}

impl LashRuntime {
    /// Resolve the shape `run`'s logical turn runs under, as one recorded
    /// step on `controller`, and adopt it as the execution view. `spec` is
    /// the interned spec the run's admitted inputs share, `None` for the
    /// default spec. `inherited` is the shape a recovered follow-on's parent
    /// run recorded at the switch (FIG-3877): the first execution re-records
    /// it under this admission's run verbatim, so the follow-on runs under
    /// the shape its logical run resolved rather than the session's current
    /// defaults.
    pub(in crate::runtime) async fn resolve_turn_config(
        &mut self,
        controller: &ScopedEffectController<'_>,
        run: &TurnId,
        spec: Option<&crate::RunSpecHash>,
        inherited: Option<crate::ResolvedRun>,
    ) -> Result<(), RuntimeError> {
        let invocation = RuntimeEffectInvocation::new(
            EffectAddress::new(
                controller.execution_scope().clone(),
                turn_config_replay_key(run),
            )?,
            RuntimeAttribution::for_turn_admission(self.state.session_id.clone(), run.clone()),
            format!("{run}.turn-config"),
        );
        let spec = match spec {
            None => None,
            Some(hash) => Some(RecordedRunSpec {
                hash: hash.clone(),
                session_id: self.state.session_id.clone(),
                store: self
                    .session
                    .as_ref()
                    .and_then(|session| session.history_store())
                    .ok_or_else(|| {
                        RuntimeError::new(
                            RuntimeErrorCode::QueuedWork,
                            format!("a run with run spec `{hash}` needs its session store"),
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
            run: run.clone(),
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
                    RuntimeEffectCommand::ResolveTurnConfig { run: run.clone() },
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
            "a run's resident config revision moved inside the run"
        );
        Ok(())
    }
}

/// A recorded config that selects no model has nothing to run a model call
/// with. The absence is recorded, so no deployment repairs it and no retry
/// changes it: the terminal `LlmProfileUnconfigured`, as a session's open refuses
/// the same head (FIG-4531).
pub(crate) fn llm_profile_unconfigured(error: SessionError) -> RuntimeError {
    RuntimeError::new(
        RuntimeErrorCode::LlmProfileUnconfigured,
        format!("the recorded config selects no model: {error}"),
    )
}

/// Why a run's run spec did not resolve, as its `ResolveTurnConfig` step
/// answers it. A model key this worker does not serve is the deployment's
/// fault; a refused shape is the spec's, with its cause typed; and a recorded
/// namespace its owner cannot read is corrupt stored data.
fn run_resolve_fault(
    hash: &impl std::fmt::Display,
    error: crate::RunResolveError,
) -> RuntimeEffectControllerError {
    match error {
        crate::RunResolveError::Model(error) => {
            RuntimeEffectControllerError::llm_profile_unavailable(
                &error.key,
                format!(
                    "run spec `{hash}` names a model this worker does not serve; the run \
                 retries until a deployment serves it: {error}"
                ),
            )
        }
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
/// run's spec against the snapshot captured at the funnel and records the
/// result. None of it enters the envelope, which names only the run.
struct ResolveTurnConfigRunner {
    run: TurnId,
    snapshot: PersistedSessionConfig,
    spec: Option<RecordedRunSpec>,
    /// The shape a recovered follow-on inherits from its parent run
    /// (FIG-3877); present only on the follow-on's own admission.
    inherited: Option<crate::ResolvedRun>,
    /// This worker's host termination policy, which a run records on its
    /// first resolution (FIG-4389); a replay decodes the record instead.
    termination: crate::runtime::TerminationPolicy,
    /// This worker's host follow-on recovery bound, recorded with the run
    /// the same way (FIG-4646).
    follow_on_recoveries: u32,
    protocol_driver: Option<std::sync::Arc<dyn crate::plugin::ProtocolDriverPlugin>>,
    /// The session's config owners, which judge every namespace a spec's
    /// overrides changed (FIG-4379).
    config_registry: Option<std::sync::Arc<crate::ConfigRegistry>>,
}

/// A run's non-default spec, read and resolved only on the step's first
/// execution.
struct RecordedRunSpec {
    hash: crate::RunSpecHash,
    session_id: crate::SessionId,
    store: crate::store::SessionStore,
    definitions: crate::RunDefinitions,
    models: std::sync::Arc<dyn crate::LlmProfiles>,
}

impl RecordedRunSpec {
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
                             run retries until a deployment serves that exact revision"
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

/// A config judgment's fault as the step's error: a refusal is the run's
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
        _effect_attempt: Option<crate::EffectAttempt>,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        let RuntimeEffectCommand::ResolveTurnConfig { run } = &envelope.command else {
            return Err(RuntimeEffectControllerError::new(
                RuntimeErrorCode::RuntimeEffectLocalExecutorMismatch,
                format!(
                    "turn-config executor cannot execute {} command",
                    envelope.command.kind().as_str()
                ),
            ));
        };
        if *run != self.run {
            return Err(RuntimeEffectControllerError::new(
                RuntimeErrorCode::RuntimeEffectLocalExecutorMismatch,
                format!(
                    "turn-config executor was bound to run `{}` but asked to resolve `{run}`",
                    self.run
                ),
            ));
        }
        let inherited = self.inherited.is_some();
        let mut resolved = match (self.inherited, self.spec) {
            // A recovered follow-on re-records the shape its parent run
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
        // owner does not admit. The refusal is the run's recorded shape.
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

    /// FIG-4631: a recorded config that selects no model is a recorded
    /// absence no deployment repairs. It is the terminal `LlmProfileUnconfigured`
    /// wherever the runtime meets it, as a session's open refuses the same
    /// head (FIG-4531), and never the retried `LlmProfileUnavailable`.
    #[tokio::test]
    async fn a_recorded_config_that_selects_no_model_is_terminal_model_unconfigured() {
        let session_id = crate::SessionId::from("recorded-without-model");
        let at_the_turn =
            super::llm_profile_unconfigured(crate::SessionError::LlmProfileUnconfigured {
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
            .with_session_id(crate::SessionId::fixture(session_id.as_str()))
            .with_plugin_factories(crate::testing::test_standard_protocol_factories())
            .with_policy(crate::SessionPolicy {
                model: Some(crate::testing::test_llm_profile_config(
                    "test-model",
                    crate::LlmProfileMetadata::builder("test-model")
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
        let at_run_admission = runtime
            .max_context_tokens()
            .expect_err("a config with no model has no prompt budget");

        for refusal in [at_the_turn, at_run_admission] {
            assert_eq!(
                refusal.code,
                crate::RuntimeErrorCode::LlmProfileUnconfigured,
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
            assert_eq!(refusal.profile_key(), None);
            assert!(
                refusal.message.contains(session_id.as_str()),
                "the refusal names the session: {refusal:?}"
            );
        }
    }

    /// FIG-4631: a run spec whose model key this worker does not serve ends
    /// its resolution with the one `LlmProfileUnavailable` fault: the key typed,
    /// the attempt's fault, and the key in the text the engine keeps, so the
    /// park of the exhausted retries names it.
    #[test]
    fn an_unserved_run_spec_key_is_the_typed_attempt_fault() {
        let key = crate::LlmProfileKey::new("kimi-k3@tensorx");
        let fault = super::run_resolve_fault(
            &"spec-hash",
            crate::RunResolveError::Model(crate::LlmProfileUnavailable::new(
                key.clone(),
                crate::LlmProfileUnavailableReason::UnknownKey,
            )),
        );
        assert_eq!(fault.code, crate::RuntimeErrorCode::LlmProfileUnavailable);
        assert_eq!(fault.profile_key(), Some(&key), "{fault:?}");
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
        assert_eq!(park.profile_key(), Some(&key));
    }
}
