use super::*;
use std::collections::BTreeMap;
use std::sync::Arc;
#[derive(Clone)]
pub struct ProcessEngineRegistration {
    installed: InstalledEngine,
    /// The host's retry policies for engine step kinds, over the engine's.
    engine_step_retries: BTreeMap<
        lash_execution_contracts::process::EngineStepKind,
        lash_protocol_types::ExecutionPolicy,
    >,
}

/// One installed engine kind: the engine, its checked admission and the
/// optional capabilities its registration declared. A registry stores it
/// whole, so replacing a kind replaces every capability with it.
#[derive(Clone)]
struct InstalledEngine {
    engine: Arc<dyn ProcessEngine>,
    admission: ProcessEngineAdmission,
    /// The engine's steps; once installed, under the host's retry overrides.
    engine_steps: Option<Arc<dyn lash_execution_contracts::process::EngineSteps>>,
    document_provider: Option<Arc<dyn ProcessDocumentProvider>>,
    state_migration: Option<Arc<dyn lash_execution_contracts::process::EngineStateMigration>>,
    succession: Option<Arc<dyn lash_execution_contracts::process::EngineSuccession>>,
    status_projection: Option<Arc<dyn lash_execution_contracts::process::EngineStatusProjection>>,
    inline_starts: Option<Arc<dyn InlineStarts>>,
}

impl ProcessEngineRegistration {
    fn checked(engine: Arc<dyn ProcessEngine>, admission: ProcessEngineAdmission) -> Self {
        Self {
            installed: InstalledEngine {
                engine,
                admission,
                engine_steps: None,
                document_provider: None,
                state_migration: None,
                succession: None,
                status_projection: None,
                inline_starts: None,
            },
            engine_step_retries: BTreeMap::new(),
        }
    }

    /// The entry a registry installs: the declared steps under the host's
    /// retry overrides.
    fn into_installed(self) -> InstalledEngine {
        let Self {
            mut installed,
            engine_step_retries: retries,
        } = self;
        if !retries.is_empty() {
            installed.engine_steps = installed.engine_steps.map(|steps| {
                Arc::new(RetriedEngineSteps { steps, retries })
                    as Arc<dyn lash_execution_contracts::process::EngineSteps>
            });
        }
        installed
    }

    pub fn kind(&self) -> &str {
        self.installed.engine.kind()
    }
    pub fn new(
        engine: Arc<dyn ProcessEngine>,
        admission: ProcessEngineAdmission,
    ) -> Result<Self, lash_execution_contracts::plugin_error::PluginError> {
        if engine.kind() != admission.kind() {
            return Err(
                lash_execution_contracts::plugin_error::PluginError::Registration(format!(
                    "process engine kind `{}` does not match admission kind `{}`",
                    engine.kind(),
                    admission.kind()
                )),
            );
        }
        Ok(Self::checked(engine, admission))
    }

    /// Pair an engine with the default recorded-input admission policy.
    pub fn accepting(engine: Arc<dyn ProcessEngine>) -> Self {
        let admission = ProcessEngineAdmission::accepting(engine.kind());
        Self::checked(engine, admission)
    }

    /// Declare the engine's own step bodies: what runs a
    /// [`StepRequest::Engine`](lash_execution_contracts::process::StepRequest::Engine) its `advance`
    /// asks for.
    #[must_use]
    pub fn with_engine_steps(
        mut self,
        steps: Arc<dyn lash_execution_contracts::process::EngineSteps>,
    ) -> Self {
        self.installed.engine_steps = Some(steps);
        self
    }

    /// Declare how this engine's definitions read as a language document.
    #[must_use]
    pub fn with_document_provider(mut self, provider: Arc<dyn ProcessDocumentProvider>) -> Self {
        self.installed.document_provider = Some(provider);
        self
    }

    /// Declare how this engine carries the state an earlier build wrote
    /// into its own format: a node serving it also claims a process left
    /// in a format the migration carries (ADR 0106 §1).
    #[must_use]
    pub fn with_state_migration(
        mut self,
        migration: Arc<dyn lash_execution_contracts::process::EngineStateMigration>,
    ) -> Self {
        self.installed.state_migration = Some(migration);
        self
    }

    /// Declare how this engine carries a process onto a successor pair its
    /// host names (D2, FIG-6121); an engine registered without one refuses
    /// every successor request.
    #[must_use]
    pub fn with_succession(
        mut self,
        succession: Arc<dyn lash_execution_contracts::process::EngineSuccession>,
    ) -> Self {
        self.installed.succession = Some(succession);
        self
    }

    /// Declare the status this engine projects from its committed state:
    /// lash records it with each transition's commit, and a host reads the
    /// last committed one back.
    #[must_use]
    pub fn with_status_projection(
        mut self,
        projection: Arc<dyn lash_execution_contracts::process::EngineStatusProjection>,
    ) -> Self {
        self.installed.status_projection = Some(projection);
        self
    }

    /// Declare how this engine starts a function carried inline
    /// (FIG-6382); an engine registered without one refuses every inline
    /// start.
    #[must_use]
    pub fn with_inline_starts(mut self, starts: Arc<dyn InlineStarts>) -> Self {
        self.installed.inline_starts = Some(starts);
        self
    }

    /// Retry the engine's `kind` steps under `policy` instead of the policy
    /// its [`EngineSteps`](lash_execution_contracts::process::EngineSteps) declare: the
    /// host's override of the engine author's default, for this deployment.
    #[must_use]
    pub fn with_engine_step_retry(
        mut self,
        kind: lash_execution_contracts::process::EngineStepKind,
        policy: lash_protocol_types::ExecutionPolicy,
    ) -> Self {
        self.engine_step_retries.insert(kind, policy);
        self
    }
}

#[derive(Clone, Default)]
pub struct ProcessEngineRegistry {
    /// Each installed engine kind, whole, by kind.
    installed: Arc<BTreeMap<String, InstalledEngine>>,
    artifact_ports: Option<Arc<super::ArtifactReferrerPorts>>,
    /// The process components a new environment is admitted against and a
    /// process is composed from (FIG-6034).
    components: crate::plugin::ProcessComponentRegistry,
}

/// A [`ProcessEngineRegistry`] held weakly: it keeps no engine alive.
#[derive(Clone)]
pub struct WeakProcessEngineRegistry {
    installed: std::sync::Weak<BTreeMap<String, InstalledEngine>>,
    artifact_ports: Option<Arc<super::ArtifactReferrerPorts>>,
    components: crate::plugin::ProcessComponentRegistry,
}

impl WeakProcessEngineRegistry {
    /// The registry, while some holder keeps it alive.
    #[must_use]
    pub fn upgrade(&self) -> Option<ProcessEngineRegistry> {
        Some(ProcessEngineRegistry {
            installed: self.installed.upgrade()?,
            artifact_ports: self.artifact_ports.clone(),
            components: self.components.clone(),
        })
    }
}

/// An engine's steps under the host's per-kind retry overrides.
struct RetriedEngineSteps {
    steps: Arc<dyn lash_execution_contracts::process::EngineSteps>,
    retries: BTreeMap<
        lash_execution_contracts::process::EngineStepKind,
        lash_protocol_types::ExecutionPolicy,
    >,
}

#[async_trait::async_trait]
impl lash_execution_contracts::process::EngineSteps for RetriedEngineSteps {
    fn kinds(&self) -> Vec<lash_execution_contracts::process::EngineStepKind> {
        self.steps.kinds()
    }

    fn execution(
        &self,
        kind: &lash_execution_contracts::process::EngineStepKind,
    ) -> std::time::Duration {
        self.steps.execution(kind)
    }

    fn retry(
        &self,
        kind: &lash_execution_contracts::process::EngineStepKind,
    ) -> lash_protocol_types::ExecutionPolicy {
        self.retries
            .get(kind)
            .copied()
            .unwrap_or_else(|| self.steps.retry(kind))
    }

    async fn run(
        &self,
        run: lash_execution_contracts::process::EngineStepRun,
        cancel: tokio_util::sync::CancellationToken,
    ) -> lash_execution_contracts::process::EngineStepAnswer {
        self.steps.run(run, cancel).await
    }
}

impl ProcessEngineRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// This registry, held weakly.
    #[must_use]
    pub fn downgrade(&self) -> WeakProcessEngineRegistry {
        WeakProcessEngineRegistry {
            installed: Arc::downgrade(&self.installed),
            artifact_ports: self.artifact_ports.clone(),
            components: self.components.clone(),
        }
    }

    /// This registry with `components` as the process components it admits
    /// environments against and composes processes from.
    #[must_use]
    pub fn with_components(mut self, components: crate::plugin::ProcessComponentRegistry) -> Self {
        self.components = components;
        self
    }

    pub fn components(&self) -> &crate::plugin::ProcessComponentRegistry {
        &self.components
    }

    /// The ports a start holds its environment through: the artifact
    /// stores' environment store, admitting new environments against this
    /// registry's components and provisioning their writers in that store's
    /// fleet record.
    pub fn env_ports(&self) -> Option<super::ProcessEnvPorts<'_>> {
        self.artifact_ports
            .as_deref()
            .map(|ports| super::ProcessEnvPorts::admitting(ports.env().as_ref(), &self.components))
    }

    #[must_use]
    pub fn with_artifact_ports(mut self, ports: super::ArtifactReferrerPorts) -> Self {
        self.artifact_ports = Some(Arc::new(ports));
        self
    }

    pub fn artifact_ports(&self) -> Option<&super::ArtifactReferrerPorts> {
        self.artifact_ports.as_deref()
    }

    pub fn with_registration(self, registration: ProcessEngineRegistration) -> Self {
        let mut installed = (*self.installed).clone();
        installed.insert(
            registration.kind().to_owned(),
            registration.into_installed(),
        );
        Self {
            installed: Arc::new(installed),
            ..self
        }
    }

    /// The body that runs `kind` for processes of engine `engine`: the
    /// typed refusal, before admission, of a
    /// [`StepRequest::Engine`](lash_execution_contracts::process::StepRequest::Engine) no registration
    /// declares.
    ///
    /// # Errors
    ///
    /// [`EngineStepRefusal`](lash_execution_contracts::process::EngineStepRefusal) when
    /// the engine is unknown, declares no engine steps, or not `kind`.
    pub fn engine_steps(
        &self,
        engine: &str,
        kind: &lash_execution_contracts::process::EngineStepKind,
    ) -> Result<
        Arc<dyn lash_execution_contracts::process::EngineSteps>,
        lash_execution_contracts::process::EngineStepRefusal,
    > {
        use lash_execution_contracts::process::EngineStepRefusal;
        let installed =
            self.installed
                .get(engine)
                .ok_or_else(|| EngineStepRefusal::UnknownEngine {
                    engine: engine.to_owned(),
                })?;
        let steps =
            installed
                .engine_steps
                .as_ref()
                .ok_or_else(|| EngineStepRefusal::NoEngineSteps {
                    engine: engine.to_owned(),
                })?;
        if !steps.kinds().contains(kind) {
            return Err(EngineStepRefusal::UndeclaredStep {
                engine: engine.to_owned(),
                kind: kind.clone(),
            });
        }
        Ok(Arc::clone(steps))
    }

    /// The document provider the `kind` engine was registered with, if any.
    pub fn document_provider(&self, kind: &str) -> Option<Arc<dyn ProcessDocumentProvider>> {
        self.installed.get(kind)?.document_provider.clone()
    }

    /// Apply one resolved cleanup to every installed engine's own store
    /// (ADR 0113 §2.2): each engine receives the referrer and only the
    /// carries under `ArtifactStoreId::Engine` of its own kind, and ends the
    /// referrer in its store even when it has none to carry.
    pub async fn end_artifact_referrer(
        &self,
        cleanup: &crate::ResolvedArtifactCleanup,
    ) -> Result<(), lash_execution_contracts::module_artifacts::ArtifactStoreError> {
        for (kind, installed) in self.installed.iter() {
            let own = lash_core_store::artifact_referrer::ResolvedArtifactCleanup::for_store(
                &cleanup.referrer,
                &cleanup.carries,
                &lash_core_store::artifact_referrer::ArtifactStoreId::Engine(kind.clone()),
            );
            installed.engine.end_artifact_referrer(&own).await?;
        }
        Ok(())
    }

    /// This is the single enforcement point for unique engine kinds across everything
    /// registered on a runtime host, whether the engine was wired directly or contributed
    /// through the plugin contract.
    pub(crate) fn try_with_engine(
        self,
        registration: ProcessEngineRegistration,
    ) -> Result<Self, lash_execution_contracts::plugin_error::PluginError> {
        if self.installed.contains_key(registration.kind()) {
            return Err(
                lash_execution_contracts::plugin_error::PluginError::Registration(format!(
                    "duplicate process engine kind `{}`; each engine kind may be registered once",
                    registration.kind()
                )),
            );
        }
        Ok(self.with_registration(registration))
    }

    /// Every registered engine's carrying of an earlier build's state, by
    /// engine kind.
    pub fn state_migrations(
        &self,
    ) -> impl Iterator<
        Item = (
            &str,
            &Arc<dyn lash_execution_contracts::process::EngineStateMigration>,
        ),
    > {
        self.installed.iter().filter_map(|(kind, installed)| {
            Some((kind.as_str(), installed.state_migration.as_ref()?))
        })
    }

    /// Every registered engine's status projection, by engine kind.
    pub fn status_projections(
        &self,
    ) -> impl Iterator<
        Item = (
            &str,
            &Arc<dyn lash_execution_contracts::process::EngineStatusProjection>,
        ),
    > {
        self.installed.iter().filter_map(|(kind, installed)| {
            Some((kind.as_str(), installed.status_projection.as_ref()?))
        })
    }

    /// How the `kind` engine carries a process onto a successor pair, when
    /// its registration declares it.
    pub fn succession(
        &self,
        kind: &str,
    ) -> Option<Arc<dyn lash_execution_contracts::process::EngineSuccession>> {
        self.installed.get(kind)?.succession.clone()
    }

    /// How the `kind` engine starts a function carried inline.
    ///
    /// # Errors
    ///
    /// A `DefinitionRefused` runtime error when the engine is not
    /// configured or starts no inline function.
    pub fn inline_starts(
        &self,
        kind: &str,
    ) -> Result<Arc<dyn InlineStarts>, lash_execution_contracts::plugin_error::PluginError> {
        self.installed
            .get(kind)
            .and_then(|installed| installed.inline_starts.clone())
            .ok_or_else(|| {
                lash_execution_contracts::plugin_error::PluginError::Runtime(
                    lash_core_store::runtime_error::RuntimeError::new(
                        lash_core_store::runtime_error::RuntimeErrorCode::DefinitionRefused,
                        format!("process engine `{kind}` starts no function carried inline"),
                    ),
                )
            })
    }

    /// Every registered engine, by kind.
    pub fn engines(&self) -> impl Iterator<Item = &Arc<dyn ProcessEngine>> {
        self.installed.values().map(|installed| &installed.engine)
    }

    pub(crate) fn get(&self, kind: &str) -> Option<Arc<dyn ProcessEngine>> {
        self.installed
            .get(kind)
            .map(|installed| Arc::clone(&installed.engine))
    }

    /// Have the `kind` engine check the settings a start of `payload`
    /// states (D4).
    ///
    /// # Errors
    ///
    /// The engine is not configured, or it refuses the settings.
    pub fn check_settings(
        &self,
        kind: &str,
        payload: &serde_json::Value,
        settings: Option<&EngineSettings>,
    ) -> Result<(), lash_execution_contracts::plugin_error::PluginError> {
        self.require(kind)?.check_settings(payload, settings)
    }

    /// The `kind` engine's default settings for a start of `payload`: an
    /// explicit starter helper, never called by lash to fill a start.
    ///
    /// # Errors
    ///
    /// The engine is not configured, or it has no defaults for `payload`.
    pub fn default_settings(
        &self,
        kind: &str,
        payload: &serde_json::Value,
    ) -> Result<Option<EngineSettings>, lash_execution_contracts::plugin_error::PluginError> {
        self.require(kind)?.default_settings(payload)
    }

    /// Inspect retained linked reach without re-fulfilling it against current advertisements.
    pub async fn inspect_link(
        &self,
        kind: &str,
        start: &lash_vocabulary::names::LinkedStart,
    ) -> Result<
        lash_execution_contracts::process::linking::LinkInspection,
        lash_execution_contracts::process::linking::EnginePortError,
    > {
        self.require(kind)
            .map_err(|error| {
                lash_execution_contracts::process::linking::EnginePortError::Unavailable {
                    message: error.to_string(),
                }
            })?
            .inspect_link(start)
            .await
    }

    /// Ordinary start admission checks every reached operation use and its known debt.
    pub fn check_links(
        &self,
        env: &Arc<crate::ProcessEnv>,
        inspection: &lash_execution_contracts::process::linking::LinkInspection,
    ) -> Result<
        lash_vocabulary::claims::ClaimRequirements,
        lash_execution_contracts::process::linking::EnginePortError,
    > {
        self.components().check_links(env, inspection)
    }
    pub fn check_links_with_seeds(
        &self,
        env: &Arc<crate::ProcessEnv>,
        inspection: &lash_execution_contracts::process::linking::LinkInspection,
        seeds: &[lash_core_store::resolutions::ResolutionSeed],
    ) -> Result<
        lash_vocabulary::claims::ClaimRequirements,
        lash_execution_contracts::process::linking::EnginePortError,
    > {
        self.components()
            .check_links_with_seeds(env, inspection, seeds)
    }

    /// Resolve the engine a `ProcessInput::Engine` start names, refusing a kind
    /// this host never registered.
    ///
    /// The kinds of the engines registered here.
    pub fn engine_kinds(&self) -> std::collections::BTreeSet<String> {
        self.installed.keys().cloned().collect()
    }

    pub fn require(
        &self,
        kind: &str,
    ) -> Result<Arc<dyn ProcessEngine>, lash_execution_contracts::plugin_error::PluginError> {
        self.get(kind).ok_or_else(|| {
            lash_execution_contracts::plugin_error::PluginError::Session(format!(
                "process engine `{kind}` is not configured"
            ))
        })
    }

    /// Admit a start using only immutable recorded inputs, then verify every
    /// definition reference the admission derived against the owning engine.
    ///
    /// This is the single boundary at which a signature claim is checked. The
    /// admission policy is pure and cannot read an artifact, so it derives the
    /// reference the recorded payload names; the engine then says what that
    /// definition's signature actually is, and a disagreement refuses the start
    /// before any durable row exists.
    pub async fn admit(
        &self,
        kind: &str,
        payload: &serde_json::Value,
    ) -> Result<AdmittedProcessIdentity, lash_execution_contracts::plugin_error::PluginError> {
        let identity = self
            .installed
            .get(kind)
            .map(|installed| installed.admission)
            .ok_or_else(|| {
                lash_execution_contracts::plugin_error::PluginError::Session(format!(
                    "process engine `{kind}` is not configured"
                ))
            })?
            .admit(payload)?;
        let Some(reference) = identity.definition.clone() else {
            return Ok(
                lash_execution_contracts::process::engine::admitted_process_identity(identity),
            );
        };
        let resolution = match self.resolve(&reference).await {
            Ok(resolution) => resolution,
            // An engine that cannot read its definition right now has said
            // nothing about an unclaimed reference's signature. Admit the
            // reference unresolved; acquisition later checks its availability.
            // Operational faults are separate and propagate for retry, never
            // as a recorded definition refusal.
            Err(lash_execution_contracts::process::ProcessDefinitionError::Refused(
                ProcessDefinitionRefusal::UnresolvableDefinition { .. },
            )) if reference.signature.is_unknown() => {
                return Ok(
                    lash_execution_contracts::process::engine::admitted_process_identity(identity),
                );
            }
            Err(error) => return Err(error.into()),
        };
        // The durable row pins the engine's authority, not the claim that
        // arrived: they are equal by the check above when a claim was made, and
        // an unclaimed reference adopts the artifact's signature here rather
        // than recording "unknown" forever.
        let identity = ProcessIdentity {
            definition: Some(reference.with_resolved_signature(resolution.signature)),
            ..identity
        };
        Ok(lash_execution_contracts::process::engine::admitted_process_identity(identity))
    }

    pub async fn resolve(
        &self,
        reference: &ProcessDefinitionRef,
    ) -> Result<ProcessDefinitionResolution, super::ProcessDefinitionError> {
        let engine = self.get(reference.engine_kind.as_str()).ok_or_else(|| {
            ProcessDefinitionRefusal::UnknownEngine {
                engine_kind: reference.engine_kind.clone(),
            }
        })?;
        let resolution = engine.resolve(reference).await?;
        // An unknown claim asserts nothing and adopts the authority. A known
        // claim must be the authority exactly: this is the single point where a
        // fabricated signature is refused, and it runs before the row exists.
        if !reference.signature.is_unknown() && resolution.signature != reference.signature {
            return Err(ProcessDefinitionRefusal::SignatureMismatch {
                engine_kind: reference.engine_kind.clone(),
                claimed: reference.signature.clone(),
                authoritative: resolution.signature,
            }
            .into());
        }
        Ok(resolution)
    }
}

#[cfg(test)]
#[path = "engine_resolve_tests.rs"]
mod resolve_tests;
