//! The host's model start policy (D8, D9, D13; final-spec §3.6).
//!
//! A model's `processes.start` names a definition and its arguments; the
//! host's required [`ProcessEnvPolicy`] states the environment and engine
//! settings the child runs under. The policy sees the target's
//! authoritative signature, the starting turn's recorded run, the surface
//! the declaring attempt was admitted under and, inside a process, the
//! enclosing process's environment. Its choice is sealed into the declaration's
//! launch and journaled with the tool outcome: once that outcome commits,
//! realization, redrive, takeover and adoption never call it again.

use std::sync::Arc;

use lash_core_store::process_env::{ProcessComponentEntry, ProcessEnv, ProcessEnvRefusal};
use lash_core_store::run_spec::ResolvedRun;

use super::model::{EngineLaunch, EngineSettings, ProcessEnvRef};
use super::{ProcessDefinitionId, ProcessEngineRegistry, ProcessSignature};
use crate::{ExecutionOwner, ProcessId, SurfaceTools};

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
    /// The recorded surface the declaring attempt was admitted under: what
    /// capture links each performed effect from (R8). `None` inside a
    /// process.
    pub surface: Option<&'a SurfaceTools>,
    pub enclosing: Option<&'a EnclosingProcess>,
    engines: &'a ProcessEngineRegistry,
}

impl<'a> ProcessEnvCx<'a> {
    pub(crate) fn new(
        starter: &'a ExecutionOwner,
        target: &'a ResolvedStartTarget,
        label: Option<&'a str>,
        run: Option<&'a ResolvedRun>,
        surface: Option<&'a SurfaceTools>,
        enclosing: Option<&'a EnclosingProcess>,
        engines: &'a ProcessEngineRegistry,
    ) -> Self {
        Self {
            starter,
            target,
            label,
            run,
            surface,
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

    /// Capture (R8): the env of the starting turn's recorded `run` with
    /// `components`, linking each effect the target performs to the tool the
    /// run's recorded surface matched it to, served by one of `components`.
    /// Nothing is re-resolved: an effect the surface does not offer (one the
    /// run met only through a deferred match, say) is refused, as is one no
    /// component serves.
    ///
    /// # Errors
    ///
    /// The target's effects cannot be certified, an effect is not on the
    /// surface, or no component serves its tool.
    pub async fn capture(
        &self,
        run: &ResolvedRun,
        surface: &SurfaceTools,
        components: Vec<ProcessComponentEntry>,
    ) -> Result<ProcessEnv, ProcessEnvRefusal> {
        let performed = self
            .engines
            .performed_effects(&self.target.engine_kind, &self.target.payload)
            .await?;
        let base = ProcessEnv::from_run(run, Default::default(), components.clone())?;
        let composition = self.engines.components().compose(&base, None)?;
        let config = crate::plugin::OwnerConfigView::components(Arc::new(base));
        let mut effects = std::collections::BTreeMap::new();
        for effect in performed.keys() {
            let entry = surface
                .entries()
                .find(|entry| {
                    crate::effect_path_of(&entry.definition.manifest).as_deref()
                        == Some(effect.as_str())
                })
                .ok_or_else(|| ProcessEnvRefusal::EffectNotLinked {
                    effect: effect.to_string(),
                })?;
            let tool = entry.tool();
            let (provider, served) = composition
                .registry
                .keys()
                .find_map(|key| {
                    composition
                        .registry
                        .served(key, tool, &config)
                        .map(|served| (key.clone(), served))
                })
                .ok_or_else(|| ProcessEnvRefusal::CaptureMissing {
                    effect: effect.to_string(),
                    tool: tool.to_string(),
                })?;
            effects.insert(
                effect.clone(),
                crate::EffectLink {
                    provider,
                    surface: crate::ProcessSurface::of(&served),
                    kind: crate::LinkKind::Tool,
                },
            );
        }
        ProcessEnv::from_run(run, effects, components)
    }

    /// Restrict the enclosing process's environment to the effects the
    /// target performs (P5), and keep its settings when the target runs the
    /// same engine.
    ///
    /// # Errors
    ///
    /// Refused outside a process.
    pub fn restrict(&self) -> Result<ProcessStartChoice, ProcessEnvPolicyError> {
        let Some(enclosing) = self.enclosing else {
            return Err(ProcessEnvPolicyError::Refused(ProcessEnvRefusal::Policy {
                reason: "a start outside a process has no environment to restrict".into(),
            }));
        };
        Ok(ProcessStartChoice {
            env: ProcessEnvChoice::Restrict,
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
    /// The enclosing process's own, restricted to the effects the target
    /// performs from the links the enclosing env recorded, never re-resolved:
    /// its reference itself when the target performs exactly what it links,
    /// and otherwise a derived env at a new content address. Refused outside
    /// a process, and for an effect the enclosing env does not link.
    Restrict,
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
            let (Some(run), Some(surface)) = (cx.run, cx.surface) else {
                return Err(ProcessEnvPolicyError::Refused(ProcessEnvRefusal::Policy {
                    reason: "this start has no recorded run to state an environment from".into(),
                }));
            };
            let components = (self.0)(cx).map_err(ProcessEnvPolicyError::Refused)?;
            let env = cx
                .capture(run, surface, components)
                .await
                .map_err(ProcessEnvPolicyError::Refused)?;
            Ok(ProcessStartChoice {
                env: ProcessEnvChoice::New(env),
                engine_settings: cx.engine_defaults()?,
            })
        }
    }

    /// Restrict the enclosing env inside a process; ask `inner` outside one.
    pub fn restrict_or(inner: Arc<dyn ProcessEnvPolicy>) -> Arc<dyn ProcessEnvPolicy> {
        Arc::new(RestrictOr(inner))
    }

    struct RestrictOr(Arc<dyn ProcessEnvPolicy>);

    #[async_trait::async_trait]
    impl ProcessEnvPolicy for RestrictOr {
        async fn choose(
            &self,
            cx: &ProcessEnvCx<'_>,
        ) -> Result<ProcessStartChoice, ProcessEnvPolicyError> {
            if cx.enclosing.is_some() {
                return cx.restrict();
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
