//! The host's model start policy (D8, D9, D13; final-spec §3.6).
//!
//! A model's `processes.start` names a definition and its arguments; the
//! host's required [`ProcessEnvPolicy`] states the environment and engine
//! settings the child runs under. The policy sees the target's
//! authoritative signature, the starting turn's recorded run, the catalog
//! the declaring attempt was offered and, inside a process, the enclosing
//! process's environment. Its choice is sealed into the declaration's
//! launch and journaled with the tool outcome: once that outcome commits,
//! realization, redrive, takeover and adoption never call it again.

use std::sync::Arc;

use lash_core_store::process_env::{ProcessComponentEntry, ProcessEnv, ProcessEnvRefusal};
use lash_core_store::run_spec::ResolvedRun;

use super::model::{EngineLaunch, EngineSettings, ProcessEnvRef};
use super::{ProcessDefinitionId, ProcessEngineRegistry, ProcessSignature};
use crate::{ExecutionOwner, ProcessId, ToolCatalog};

/// Chooses the environment and engine settings of a model's process start.
///
/// Async because real host mappings read databases and registries; a sync
/// function would force a blocking wait inside a tool body. It must be
/// side-effect free and repeat-safe: before the declaring outcome commits,
/// a crash or an attempt fault may run it again.
#[async_trait::async_trait]
pub trait ProcessEnvPolicy: Send + Sync {
    /// # Errors
    ///
    /// [`ProcessEnvPolicyError::Refused`] is the start's tool error, and
    /// [`ProcessEnvPolicyError::Fault`] a retryable attempt fault.
    async fn choose(
        &self,
        cx: &ProcessEnvCx<'_>,
    ) -> Result<ProcessStartChoice, ProcessEnvPolicyError>;
}

/// The definition a start names, read and verified before the policy runs
/// (D8): its engine's authoritative signature, never the model's claim, and
/// the payload the engine runs.
#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedStartTarget {
    pub definition_id: ProcessDefinitionId,
    pub engine_kind: String,
    pub signature: ProcessSignature,
    pub payload: serde_json::Value,
    pub args: serde_json::Map<String, serde_json::Value>,
}

/// The process a start is made inside: its recorded environment and
/// settings.
#[derive(Clone, Debug, PartialEq)]
pub struct EnclosingProcess {
    pub process: ProcessId,
    pub env_ref: ProcessEnvRef,
    pub env: ProcessEnv,
    pub engine_kind: String,
    pub settings: Option<EngineSettings>,
}

/// What a policy decides with.
#[non_exhaustive]
pub struct ProcessEnvCx<'a> {
    pub starter: &'a ExecutionOwner,
    /// D8: authoritative, read and verified before the policy runs.
    pub target: &'a ResolvedStartTarget,
    pub label: Option<&'a str>,
    /// D13: the starting turn's recorded run, never live config. `None`
    /// inside a process.
    pub run: Option<&'a ResolvedRun>,
    /// D7: the catalog the declaring attempt was dispatched against.
    pub offered: Option<&'a ToolCatalog>,
    pub enclosing: Option<&'a EnclosingProcess>,
    engines: &'a ProcessEngineRegistry,
}

impl<'a> ProcessEnvCx<'a> {
    pub(crate) fn new(
        starter: &'a ExecutionOwner,
        target: &'a ResolvedStartTarget,
        label: Option<&'a str>,
        run: Option<&'a ResolvedRun>,
        offered: Option<&'a ToolCatalog>,
        enclosing: Option<&'a EnclosingProcess>,
        engines: &'a ProcessEngineRegistry,
    ) -> Self {
        Self {
            starter,
            target,
            label,
            run,
            offered,
            enclosing,
            engines,
        }
    }

    /// This declaring build's engine defaults for the target, sealed now:
    /// realization never reads them again. An explicit choice.
    ///
    /// # Errors
    ///
    /// The engine is not configured here, or has no defaults for the
    /// payload: a fault, since another build may serve it.
    pub fn engine_defaults(&self) -> Result<Option<EngineSettings>, ProcessEnvPolicyError> {
        self.engines
            .default_settings(&self.target.engine_kind, &self.target.payload)
            .map_err(|error| ProcessEnvPolicyError::Fault(error.to_string()))
    }

    /// Inherit the enclosing process's environment, and its settings when
    /// the target runs the same engine.
    ///
    /// # Errors
    ///
    /// Refused outside a process.
    pub fn inherit(&self) -> Result<ProcessStartChoice, ProcessEnvPolicyError> {
        let Some(enclosing) = self.enclosing else {
            return Err(ProcessEnvPolicyError::Refused(ProcessEnvRefusal::Policy {
                reason: "a start outside a process has no environment to inherit".into(),
            }));
        };
        Ok(ProcessStartChoice {
            env: ProcessEnvChoice::Inherit,
            engine_settings: (enclosing.engine_kind == self.target.engine_kind)
                .then(|| enclosing.settings.clone())
                .flatten(),
        })
    }
}

/// A policy's decision: the environment and the engine settings.
#[derive(Clone, Debug, PartialEq)]
pub struct ProcessStartChoice {
    pub env: ProcessEnvChoice,
    pub engine_settings: Option<EngineSettings>,
}

/// Which environment a start runs under.
#[derive(Clone, Debug, PartialEq)]
#[expect(
    clippy::large_enum_variant,
    reason = "a choice is made once per start and moved into its launch"
)]
pub enum ProcessEnvChoice {
    /// The enclosing process's own; refused outside a process.
    Inherit,
    /// One already published.
    Published(ProcessEnvRef),
    /// One stated now, admitted and published by the hold.
    New(ProcessEnv),
}

/// Why a policy chose nothing.
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum ProcessEnvPolicyError {
    /// The start's tool error, model-visible; no intent is declared.
    #[error(transparent)]
    Refused(ProcessEnvRefusal),
    /// A retryable attempt fault; never recorded as a refusal.
    #[error("process start policy fault: {0}")]
    Fault(String),
}

/// The explicit policy helpers. None is a default: a host composes the one
/// it means.
pub mod policy {
    use super::*;

    type Mapping = dyn Fn(&ProcessEnvCx<'_>) -> Result<Vec<ProcessComponentEntry>, ProcessEnvRefusal>
        + Send
        + Sync;

    /// State the environment of the starting turn's recorded run with the
    /// components `mapping` names, and this build's engine defaults.
    /// Refused outside a run.
    pub fn from_run(
        mapping: impl Fn(&ProcessEnvCx<'_>) -> Result<Vec<ProcessComponentEntry>, ProcessEnvRefusal>
        + Send
        + Sync
        + 'static,
    ) -> Arc<dyn ProcessEnvPolicy> {
        Arc::new(FromRun(Box::new(mapping)))
    }

    struct FromRun(Box<Mapping>);

    #[async_trait::async_trait]
    impl ProcessEnvPolicy for FromRun {
        async fn choose(
            &self,
            cx: &ProcessEnvCx<'_>,
        ) -> Result<ProcessStartChoice, ProcessEnvPolicyError> {
            let (Some(run), Some(offered)) = (cx.run, cx.offered) else {
                return Err(ProcessEnvPolicyError::Refused(ProcessEnvRefusal::Policy {
                    reason: "this start has no recorded run to state an environment from".into(),
                }));
            };
            let components = (self.0)(cx).map_err(ProcessEnvPolicyError::Refused)?;
            let env = ProcessEnv::from_run(run, offered, components)
                .map_err(ProcessEnvPolicyError::Refused)?;
            Ok(ProcessStartChoice {
                env: ProcessEnvChoice::New(env),
                engine_settings: cx.engine_defaults()?,
            })
        }
    }

    /// Inherit inside a process; ask `inner` outside one.
    pub fn inherit_or(inner: Arc<dyn ProcessEnvPolicy>) -> Arc<dyn ProcessEnvPolicy> {
        Arc::new(InheritOr(inner))
    }

    struct InheritOr(Arc<dyn ProcessEnvPolicy>);

    #[async_trait::async_trait]
    impl ProcessEnvPolicy for InheritOr {
        async fn choose(
            &self,
            cx: &ProcessEnvCx<'_>,
        ) -> Result<ProcessStartChoice, ProcessEnvPolicyError> {
            if cx.enclosing.is_some() {
                return cx.inherit();
            }
            self.0.choose(cx).await
        }
    }

    /// Every start runs under `launch`, published already.
    pub fn published(launch: EngineLaunch) -> Arc<dyn ProcessEnvPolicy> {
        Arc::new(Published(launch))
    }

    struct Published(EngineLaunch);

    #[async_trait::async_trait]
    impl ProcessEnvPolicy for Published {
        async fn choose(
            &self,
            _cx: &ProcessEnvCx<'_>,
        ) -> Result<ProcessStartChoice, ProcessEnvPolicyError> {
            Ok(ProcessStartChoice {
                env: ProcessEnvChoice::Published(self.0.env.clone()),
                engine_settings: self.0.settings.clone(),
            })
        }
    }

    /// Refuse every start with `reason`.
    pub fn refuse(reason: impl Into<String>) -> Arc<dyn ProcessEnvPolicy> {
        Arc::new(Refuse(reason.into()))
    }

    struct Refuse(String);

    #[async_trait::async_trait]
    impl ProcessEnvPolicy for Refuse {
        async fn choose(
            &self,
            _cx: &ProcessEnvCx<'_>,
        ) -> Result<ProcessStartChoice, ProcessEnvPolicyError> {
            Err(ProcessEnvPolicyError::Refused(ProcessEnvRefusal::Policy {
                reason: self.0.clone(),
            }))
        }
    }
}
