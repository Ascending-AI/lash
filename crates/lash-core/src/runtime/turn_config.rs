//! The shape a logical turn runs under (FIG-3600 S6, D3 §2; FIG-3838;
//! FIG-6021).
//!
//! A run resolves its shape once, before its first durable effect: after the
//! boundary's command drain and the run's admission, its [`RunSpec`](crate::RunSpec)
//! (the spec its admitted inputs share; the default spec for a run of wakes)
//! is resolved against the run's snapshot, the resident config after the
//! run's plugin transition, which is the durable head's namespaces with this
//! build's initial values for those the head does not record. The result,
//! spec included, is the run's [`ResolvedRun`](crate::ResolvedRun), which
//! the turn's first phase commit records in its checkpoint. A resume and a
//! cancel restore of a committed run read that record and resolve nothing:
//! a config change that lands after the run started never reaches the run, a
//! redeploy of the definition, an owner or a renderer never reshapes it, and
//! an input sent after a config command runs under the new config. Every
//! physical turn of the run reuses the one record. The record is the run's
//! execution view only; commits keep writing the sticky session config, so a
//! spec's overrides and options never become the session's.
//!
//! Resolution is a plain derivation, not an activation: the owners that
//! apply a spec's options come from the host's config registrations, built
//! from its factories alone, and the render from the protocol driver of the
//! session's built contributions, with no readiness callback, catalog
//! refresh or protocol restore run first. A derivation that never commits
//! (a crash before the first phase) is simply derived again.
//!
//! The record is data. The model binding it records is bound to a live
//! provider handle lazily, on every execution: a handle is this worker's
//! capability, not a decision. A recorded model that cannot be bound here
//! is the worker's deployment fault at the call that needs it, so a cancel
//! of a committed run never needs it (D3 Q3). A spec's per-run model key is
//! minted into a binding here, once.
//!
//! Resolution faults that a redeploy repairs end the attempt unrecorded (P3):
//! a spec read the store did not answer, a definition revision this worker
//! does not register, or a per-run model key this worker's models do not
//! serve; the run retries, parks on its engine's budget, and recovers once
//! the worker serves it. Only a deterministic refusal of the spec ends the
//! run, with its cause typed ([`RunShapeRefusal`](crate::RunShapeRefusal),
//! FIG-4652).
//!
//! Which owners judge a resolved run: when the run's spec changed the
//! snapshot, every owner with a recorded namespace judges the complete
//! candidate, changed or not, as a config transaction's candidate is, so a
//! run's change to one namespace cannot break another owner's invariant. A
//! run that changes nothing executes the session's own config, which the
//! transaction that wrote it already judged, and is not judged again: a
//! deploy that tightens one owner refuses runs that state options, not the
//! session's default runs.

use crate::runtime::LashRuntime;
use crate::{RuntimeEffectControllerError, RuntimeError, RuntimeErrorCode, SessionError};

impl LashRuntime {
    /// Resolve the shape a fresh run executes under from `spec`, the
    /// interned spec the run's admitted inputs share (`None` for the default
    /// spec), against the resident config. Nothing is installed or
    /// published: the caller installs the result and its first phase commit
    /// records it.
    pub(in crate::runtime) async fn resolve_run(
        &self,
        spec: Option<&crate::RunSpecHash>,
    ) -> Result<crate::ResolvedRun, RuntimeError> {
        let plugins = &self.services.plugins;
        let registry = plugins.host().config_registry().map_err(|error| {
            RuntimeError::new(
                RuntimeErrorCode::Plugin,
                format!("the installed plugins' config registration is invalid: {error}"),
            )
        })?;
        let snapshot = crate::store::persisted_session_config_from_state(&self.state);
        let mut resolved = match spec {
            None => crate::ResolvedRun::snapshot(snapshot),
            Some(hash) => {
                self.resolve_spec(hash, &snapshot, registry.as_ref())
                    .await?
            }
        };
        // Native execution config is part of the recorded resolution: a
        // resume installs it and never calls a decoder.
        resolved = resolved
            .map_configs(|mut config| {
                config.plugin_config = plugins.host().decode_config(&config.plugin_config)?;
                Ok::<_, RuntimeEffectControllerError>(config)
            })
            .map_err(RuntimeEffectControllerError::into_runtime_error)?;
        if resolved.has_override() {
            registry
                .validate_derived(resolved.base(), resolved.config())
                .map_err(|fault| config_fault(fault).into_runtime_error())?;
            let live = self
                .session
                .as_ref()
                .map(|session| std::sync::Arc::clone(session.plugins().protocol_session()));
            super::config_ops::validate_config_tool_catalog(
                plugins.host(),
                &self.state,
                self.page_reader(),
                resolved.config(),
                live.as_ref(),
            )
            .await
            .map_err(|error| RuntimeEffectControllerError::from(error).into_runtime_error())?;
        }
        // The render comes from the protocol driver of the session's built
        // contributions: building them readies nothing.
        plugins
            .prepare_capabilities()
            .map_err(|error| RuntimeEffectControllerError::from(error).into_runtime_error())?;
        let namespace = resolved.config().plugin_config.protocol_turn_options();
        resolved.render = plugins
            .protocol_driver()
            .resolve_render(&namespace)
            .map_err(|fault| {
                match fault {
                    crate::RenderFault::Refused(refusal) => {
                        RuntimeEffectControllerError::run_shape_refused(
                            crate::RunShapeRefusal::Render { refusal },
                        )
                    }
                    crate::RenderFault::RecordedCorrupt(corrupt) => {
                        corrupt.into_store_error().into()
                    }
                }
                .into_runtime_error()
            })?;
        Ok(resolved)
    }

    /// Resolve interned spec `hash` against `snapshot`; `owners` apply the
    /// run options it states.
    async fn resolve_spec(
        &self,
        hash: &crate::RunSpecHash,
        snapshot: &crate::PersistedSessionConfig,
        owners: &dyn crate::RunOptionsOwner,
    ) -> Result<crate::ResolvedRun, RuntimeError> {
        let store = self.services.store.as_ref().ok_or_else(|| {
            RuntimeError::new(
                RuntimeErrorCode::QueuedWork,
                format!("a run with run spec `{hash}` needs its session store"),
            )
        })?;
        let unread =
            |message: String| RuntimeError::new(RuntimeErrorCode::StoreCommitFailed, message);
        let spec = store
            .load_run_spec(hash)
            .await
            .map_err(|error| unread(format!("run spec `{hash}` could not be read: {error}")))?
            .ok_or_else(|| {
                unread(format!(
                    "run spec `{hash}` is not interned for session `{}`",
                    self.state.session_id
                ))
            })?;
        let definition = match &spec.definition {
            None => None,
            Some(reference) => {
                let definition = self
                    .host
                    .core
                    .providers
                    .run_definitions
                    .get(reference)
                    .ok_or_else(|| {
                        RuntimeError::new(
                            RuntimeErrorCode::RunDefinitionUnavailable,
                            format!(
                                "run definition `{reference}` is not registered on this worker; \
                                 the run retries until a deployment serves that exact revision"
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
                            .into_runtime_error()
                        })?,
                )
            }
        };
        spec.resolve(
            snapshot,
            definition,
            self.host.core.providers.models.as_ref(),
            owners,
        )
        .map_err(|error| run_resolve_fault(hash, error).into_runtime_error())
    }

    /// Install `resolved`, the run's recorded shape, as the execution view,
    /// keeping the sticky config under it. A resumed run installs the record
    /// its checkpoint holds, which names the head the run was admitted at.
    pub(in crate::runtime) fn apply_turn_config(
        &mut self,
        resolved: &crate::ResolvedRun,
    ) -> Result<(), crate::FormatRefusal> {
        self.install_resolved_run(resolved)?;
        debug_assert_eq!(
            self.state.config_revision,
            resolved.base().config_revision,
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

/// Why a run's run spec did not resolve. A model key this worker does not
/// serve is the deployment's fault; a refused shape is the spec's, with its
/// cause typed; and a recorded namespace its owner cannot read is corrupt
/// stored data.
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

/// A config judgment's fault: a refusal is the run's shape refusal, and a
/// corrupt recorded namespace is corruption.
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
                    crate::ExecutionBudgets::recommended(),
                    crate::runtime::DeltaCoalescing::recommended(),
                    crate::DataRetentionConfig::standard(),
                ),
                crate::testing::runtime_lease_owner(),
            )
            .with_session_id(crate::SessionId::fixture(session_id.as_str()))
            .with_plugin_factories(crate::testing::test_standard_protocol_factories())
            .with_creation(crate::SessionPolicy {
                model: Some(crate::testing::test_llm_profile_config(
                    "test-model",
                    crate::LlmProfileMetadata::builder("test-model")
                        .cache_retention(crate::provider::CacheRetention::Short)
                        .context_window_tokens(1024)
                        .build()
                        .expect("model"),
                )),
                ..crate::SessionPolicy::new(
                    crate::TurnBudget::Unbounded,
                    crate::MaxToolCalls::new(1024),
                    crate::NoProgressBudget::bounded(12),
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
        assert!(!fault.is_terminal());
        let park = crate::store::ParkReason::engine_retry_exhausted(
            8,
            None,
            format!("[500] {}", fault.attempt_failure_text()),
        );
        assert_eq!(park.profile_key(), Some(&key));
    }
}
