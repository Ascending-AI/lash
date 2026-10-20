//! The host's model start policy (D8, D9, D13; final-spec §3.6).
//!
//! A model's `processes.start` names a definition and its arguments; the
//! host's required [`ProcessEnvPolicy`] states the environment and engine
//! settings the child runs under. The policy sees the target's
//! authoritative signature, the starting turn's recorded run, the surface
//! the declaring attempt was admitted under, the grants the declaring
//! cell's committed deferred matches recorded and, inside a process, the
//! enclosing process's environment. Its choice is sealed into the declaration's
//! launch and journaled with the tool outcome: once that outcome commits,
//! realization, redrive, takeover and adoption never call it again.

use std::collections::BTreeMap;
use std::sync::Arc;

use lash_core_store::process_env::{ProcessComponentEntry, ProcessEnv, ProcessEnvRefusal};
use lash_core_store::run_spec::ResolvedRun;

use super::ProcessDefinitionId;
use super::ProcessEngineRegistry;
use super::model::ProcessEnvRef;
use crate::{EffectName, ExecutionOwner, ProcessId, SurfaceTools, ToolExecutionGrant};
use lash_execution_contracts::process::EngineLaunch;
use lash_execution_contracts::process::EngineSettings;
use lash_execution_contracts::process::ProcessSignature;

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
    /// The effects a run of `payload` performs (P3), as its engine says.
    pub performed: super::PerformedEffects,
}

/// A function a start carries inline, read and checked by its engine before
/// the policy runs (D8, FIG-6382): the definition a start of it runs, and
/// the function itself, which nothing stores until the start is realized.
#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedInlineStart {
    pub target: ResolvedStartTarget,
    /// The function, as its engine's payload.
    pub payload: serde_json::Value,
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
    recorded_grants: &'a BTreeMap<EffectName, ToolExecutionGrant>,
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
        static NONE: BTreeMap<EffectName, ToolExecutionGrant> = BTreeMap::new();
        Self {
            starter,
            target,
            label,
            run,
            surface,
            enclosing,
            recorded_grants: &NONE,
            engines,
        }
    }

    /// This context for a start the cell whose committed deferred matches
    /// recorded `grants` declares.
    pub(crate) fn with_recorded_grants(
        mut self,
        grants: &'a BTreeMap<EffectName, ToolExecutionGrant>,
    ) -> Self {
        self.recorded_grants = grants;
        self
    }

    /// The grants the declaring cell's committed deferred matches recorded,
    /// by the effect path each matched tool's binding names: what capture
    /// links an effect the surface does not offer from. Only the declaring
    /// cell's own: empty for any other start, and never another cell's.
    #[must_use]
    pub fn recorded_grants(&self) -> &BTreeMap<EffectName, ToolExecutionGrant> {
        self.recorded_grants
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
    /// run's recorded surface matched it to or, for one the surface does not
    /// offer, the tool the declaring cell's committed deferred match resolved
    /// it to ([`recorded_grants`](Self::recorded_grants)), with that match's
    /// definition and execution binding. Each is served by the declared
    /// process twin of its recorded provider among `components`: the same
    /// owner and local key in the process registry. A binding is routed to
    /// its codec's twin there, its format and payload kept as recorded.
    /// Nothing is re-resolved: an effect neither offers is refused, as is
    /// one no component serves or whose binding no component reads.
    ///
    /// # Errors
    ///
    /// The target's effects cannot be certified, an effect is not linked, no
    /// component serves its tool, or none reads its binding.
    pub async fn capture(
        &self,
        run: &ResolvedRun,
        surface: &SurfaceTools,
        components: Vec<ProcessComponentEntry>,
    ) -> Result<ProcessEnv, ProcessEnvRefusal> {
        let performed = &self.target.performed;
        let base = ProcessEnv::from_run(run, Default::default(), components.clone())?;
        let composition = self.engines.components().compose(&base, None)?;
        let config = crate::plugin::OwnerConfigView::components(Arc::new(base));
        let twin = |provider: &crate::ProviderKey| {
            composition
                .registry
                .keys()
                .find(|twin| {
                    twin.registry() == crate::RegistryKind::Process
                        && twin.owner() == provider.owner()
                        && twin.key() == provider.key()
                })
                .cloned()
        };
        let mut effects = BTreeMap::new();
        for effect in performed.keys() {
            let offered = surface
                .entries()
                .find(|entry| entry.definition.manifest.operation.effect_name() == *effect);
            let link = match (offered, self.recorded_grants.get(effect)) {
                (Some(entry), _) => {
                    let tool = entry.tool();
                    let missing = || ProcessEnvRefusal::CaptureMissing {
                        effect: effect.to_string(),
                        tool: tool.to_string(),
                    };
                    let provider = twin(&entry.provider).ok_or_else(missing)?;
                    let served = composition
                        .registry
                        .served(&provider, tool, &config)
                        .ok_or_else(missing)?;
                    let retained = crate::ProcessSurface::of(&entry.definition);
                    if crate::ProcessSurface::of(&served) != retained {
                        return Err(ProcessEnvRefusal::LinkedSurfaceChanged {
                            effect: effect.to_string(),
                            tool: tool.to_string(),
                        });
                    }
                    crate::EffectLink {
                        provider,
                        surface: retained,
                        kind: crate::LinkKind::Tool,
                    }
                }
                (None, Some(grant)) => {
                    let tool = &grant.manifest().id;
                    let provider =
                        twin(&grant.provider).ok_or_else(|| ProcessEnvRefusal::CaptureMissing {
                            effect: effect.to_string(),
                            tool: tool.to_string(),
                        })?;
                    let binding = grant
                        .binding
                        .clone()
                        .map(|binding| {
                            let codec = twin(&binding.codec).ok_or_else(|| {
                                ProcessEnvRefusal::BindingUnreadable {
                                    effect: effect.to_string(),
                                    tool: tool.to_string(),
                                    codec: binding.codec.to_string(),
                                    reason: "it has no process twin: no component the env names \
                                             declares it"
                                        .to_owned(),
                                }
                            })?;
                            Ok(crate::ExecutionBinding { codec, ..binding })
                        })
                        .transpose()?;
                    crate::EffectLink::resolved(
                        provider,
                        crate::ToolDefinition {
                            manifest: grant.manifest().clone(),
                            contract: grant.contract().clone(),
                        },
                        binding,
                    )
                }
                (None, None) => {
                    return Err(ProcessEnvRefusal::EffectNotLinked {
                        effect: effect.to_string(),
                    });
                }
            };
            effects.insert(effect.clone(), link);
        }
        let candidate = ProcessEnv::from_run(run, effects, components)?;
        self.engines.capture_env(performed, &candidate)
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
            fits: Default::default(),
            env: ProcessEnvChoice::Restrict,
            engine_settings: (enclosing.engine_kind == self.target.engine_kind)
                .then(|| enclosing.settings.clone())
                .flatten(),
        })
    }
}

impl ProcessEngineRegistry {
    /// Capture exactly the certified `performed` effects from `candidate`,
    /// preserving their recorded links and all other environment settings.
    /// Host and model starts use this same capture judgment.
    ///
    /// # Errors
    ///
    /// A missing effect, an unfit link, or a component that cannot serve it.
    pub fn capture_env(
        &self,
        performed: &lash_execution_contracts::process::PerformedEffects,
        candidate: &ProcessEnv,
    ) -> Result<ProcessEnv, ProcessEnvRefusal> {
        let env = candidate
            .restrict(performed.keys())
            .map_err(|refusal| match refusal {
                ProcessEnvRefusal::RestrictMissing { effect } => {
                    ProcessEnvRefusal::EffectNotLinked { effect }
                }
                refusal => refusal,
            })?;
        crate::check_links(&env, performed, &Default::default())?;
        self.components().admit(&env)?;
        self.components()
            .check_served(&Arc::new(env.clone()))
            .map_err(|refusal| match refusal {
                ProcessEnvRefusal::ToolNotServed { effect, tool, .. } => {
                    ProcessEnvRefusal::CaptureMissing { effect, tool }
                }
                refusal => refusal,
            })?;
        Ok(env)
    }
}

/// A policy's decision: the environment and the engine settings.
#[derive(Clone, Debug, PartialEq)]
pub struct ProcessStartChoice {
    /// Per-effect fit checked before holding the launch; never stored.
    pub fits: BTreeMap<crate::EffectName, lash_execution_contracts::process::FitPolicy>,
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
                fits: Default::default(),
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
                fits: Default::default(),
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

#[cfg(test)]
#[path = "env_policy_tests.rs"]
mod tests;
