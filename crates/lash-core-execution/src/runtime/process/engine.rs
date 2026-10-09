use std::collections::BTreeMap;
use std::sync::Arc;

use super::definition_ref::{
    ProcessDefinitionRef, ProcessDefinitionRefusal, ProcessDefinitionResolution,
};
use super::engine_state::{EngineAction, EngineEvent, EngineState, EngineStateFormat};
use super::events::ProcessAwaitOutput;
use super::model::{ProcessExecutionEnvSpec, ProcessIdentity};

/// Result of one process invocation.
#[derive(Clone, Debug, PartialEq)]
pub enum ProcessRunOutcome {
    /// The run ended. `prelude` is its terminal batch (FIG-3571): the
    /// execution-owned events the run still owes the log (its pending
    /// effect-summary occurrences, then its `process.effect_omissions`
    /// record), which the runner commits ahead of the terminal event in the
    /// completion's own transaction
    /// ([`ProcessLifecycle::complete_process_with_prelude`](super::registry_concerns::ProcessLifecycle::complete_process_with_prelude)).
    Terminal {
        output: Box<ProcessAwaitOutput>,
        prelude: Vec<super::events::ProcessEventAppendRequest>,
    },
}

impl ProcessRunOutcome {
    /// This is an **integrator class 3: process-engine implementor** seam.
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Terminal { .. })
    }

    /// Borrow the terminal output.
    ///
    /// This is an **integrator class 3: process-engine implementor** seam.
    pub fn terminal_output(&self) -> Option<&ProcessAwaitOutput> {
        match self {
            Self::Terminal { output, .. } => Some(output),
        }
    }
}

/// Failure of the host/runtime infrastructure needed to execute a process.
///
/// Infrastructure failures are not producer outcomes and must not be persisted
/// as terminal process failures. The worker leaves the row claimable so its
/// configured retry pacing and attempt budget can decide what happens next.
#[derive(Debug)]
pub struct ProcessInfraError {
    source: crate::PluginError,
}

impl ProcessInfraError {
    /// Constructs a `ProcessInfraError` for protocol and process-engine implementors while running
    /// a durable process.
    ///
    /// An opaque session error names no cause. Met while running a process it
    /// is the infrastructure's, so it is carried as the attempt's fault
    /// ([`PluginError::attempt_fault`](crate::PluginError::attempt_fault)):
    /// never the process's terminal. A typed error keeps its own class.
    pub fn new(source: crate::PluginError) -> Self {
        let source = match source {
            crate::PluginError::Session(message) => crate::PluginError::attempt_fault(message),
            typed => typed,
        };
        Self { source }
    }

    pub fn into_plugin_error(self) -> crate::PluginError {
        self.source
    }
}

impl std::fmt::Display for ProcessInfraError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.source.fmt(formatter)
    }
}

impl std::error::Error for ProcessInfraError {}

impl From<crate::PluginError> for ProcessInfraError {
    fn from(source: crate::PluginError) -> Self {
        Self::new(source)
    }
}

impl From<ProcessAwaitOutput> for ProcessRunOutcome {
    fn from(output: ProcessAwaitOutput) -> Self {
        Self::Terminal {
            output: Box::new(output),
            prelude: Vec::new(),
        }
    }
}

/// A host process engine: a state machine lash advances (ADR 0132 §7; S6
/// of I0, FIG-5194). See [`super::engine_state`] for the advance contract
/// and cancel.
///
/// No method has a default body: every engine answers every question
/// (law S1, no silent defaults). Core built-ins (`SessionTurn` and
/// `External`) are not registered here.
#[async_trait::async_trait]
pub trait ProcessEngine: Send + Sync {
    /// The kind every process of this engine is registered under.
    fn kind(&self) -> &'static str;

    /// The encoding of this engine's state. L11's claim filter reads it.
    fn state_format(&self) -> EngineStateFormat;

    /// How long a cancelled process may run best-effort steps before lash
    /// forces its terminal. Recorded in `engine_config` at creation.
    fn cancel_grace(&self) -> std::time::Duration;

    /// The executable generation a run of `payload` would run as (FIG-3571),
    /// or `None` for an engine whose runs carry no generation, or for a
    /// payload the engine refuses anyway.
    fn program_identity(&self, payload: &serde_json::Value) -> Option<crate::ExecutableGeneration>;

    /// What a process created now under `env_spec` records with its row
    /// (FIG-4527), or `None` for an engine with no such configuration. Asked
    /// once, at registration; every activation reads it back from the row.
    fn creation_config(
        &self,
        env_spec: &ProcessExecutionEnvSpec,
    ) -> Result<Option<serde_json::Value>, crate::PluginError>;

    /// The next state and action for `event` over `state`: synchronous and
    /// effect-free. The same state and event always give the same answer.
    fn advance(
        &self,
        state: EngineState,
        event: EngineEvent,
    ) -> Result<(EngineState, EngineAction), ProcessInfraError>;

    /// Every artifact a start payload names, with the store that holds it
    /// (ADR 0113 §2.2).
    fn start_artifacts(
        &self,
        payload: &serde_json::Value,
    ) -> Result<Vec<crate::ArtifactName>, crate::PluginError>;

    /// Apply a resolved cleanup to the engine's own artifact store. An engine
    /// whose artifacts all live in a store-set port answers `Ok(())`. A carry
    /// whose bytes are missing returns
    /// `ArtifactStoreError::CarryArtifactMissing` so the cleanup row stalls
    /// instead of retrying indefinitely.
    async fn end_artifact_referrer(
        &self,
        cleanup: &crate::ResolvedArtifactCleanup,
    ) -> Result<(), crate::ArtifactStoreError>;

    /// Add the claim's edge to one artifact this engine's store holds,
    /// refusing a fenced referrer with `ReferrerEnded`. Called only for names
    /// `start_artifacts` reported under `ArtifactStoreId::Engine`, after the
    /// caller armed the claim's guard.
    async fn acquire_engine_artifact(
        &self,
        claim: &crate::ReferrerClaim,
        artifact_ref: &str,
    ) -> Result<(), crate::PluginError>;

    /// Check supplied start arguments against an authoritative signature.
    /// Engines with no checkable signature return a typed refusal.
    async fn check_args(
        &self,
        signature: &super::ProcessSignature,
        args: &serde_json::Map<String, serde_json::Value>,
        mode: super::ArgsMode,
    ) -> Result<(), super::ArgsMismatch>;

    /// What this engine's stored artifact says about a definition reference:
    /// its authoritative signature.
    /// The signature on the reference is a claim this method never reads; an
    /// engine that stores no artifacts answers `ProcessSignature::Unknown`.
    ///
    /// This is an **integrator class 3: process-engine implementor** seam.
    async fn resolve(
        &self,
        reference: &ProcessDefinitionRef,
    ) -> Result<ProcessDefinitionResolution, ProcessDefinitionRefusal>;
}

/// A definition's language document, as its engine's
/// [`ProcessDocumentProvider`] answers it.
///
/// Core names no language: the document crosses it as the provider's own
/// type, and the reader that knows the type takes it back with
/// [`Self::downcast`].
pub struct ProcessDocument(Box<dyn std::any::Any + Send + Sync>);

impl ProcessDocument {
    pub fn new<T: std::any::Any + Send + Sync>(document: T) -> Self {
        Self(Box::new(document))
    }

    /// The document as `T`, or the document unchanged when it is another
    /// type.
    pub fn downcast<T: std::any::Any>(self) -> Result<T, Self> {
        self.0
            .downcast::<T>()
            .map(|document| *document)
            .map_err(Self)
    }
}

impl std::fmt::Debug for ProcessDocument {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("ProcessDocument(..)")
    }
}

/// What a document provider reads of one definition: its engine-neutral
/// identity, derived from the same artifact read as its language document.
#[derive(Debug)]
pub struct InspectedProcessDefinition {
    /// The content-derived id and the signature the stored artifact states.
    pub definition: super::ProcessDefinition,
    pub document: ProcessDocument,
}

/// The answer of a [`ProcessDocumentProvider`].
#[derive(Debug)]
pub enum ProcessDocumentRead {
    Inspected(Box<InspectedProcessDefinition>),
    /// Nothing retains an artifact the definition reads.
    ArtifactMissing {
        artifact: crate::ArtifactName,
    },
}

/// What a [`ProcessDocumentProvider`] names a definition's document by.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProcessDocumentRefRead {
    Named(lash_trace::WorkflowDocumentRef),
    /// Nothing retains an artifact the definition reads.
    ArtifactMissing {
        artifact: crate::ArtifactName,
    },
}

/// An engine's optional reading of its definitions as a language document
/// (FIG-5563). An engine registered without one has no document: a host's
/// inspection of its processes and definitions answers `Unsupported`.
#[async_trait::async_trait]
pub trait ProcessDocumentProvider: Send + Sync {
    /// The document of the definition `payload` names: a start payload or a
    /// definition value, as [`ProcessEngine::start_artifacts`] reads it.
    async fn document(
        &self,
        payload: &serde_json::Value,
    ) -> Result<ProcessDocumentRead, crate::PluginError>;

    /// The reference of that document, without the document: what a process
    /// observation snapshot carries.
    async fn document_ref(
        &self,
        payload: &serde_json::Value,
    ) -> Result<ProcessDocumentRefRead, crate::PluginError>;
}

/// A process identity the engine registry produced.
///
/// This is the only way an identity reaches a
/// [`ProcessRegistration`](super::model::ProcessRegistration). A
/// definition reference can therefore only appear on a durable row if the
/// engine that owns the definition resolved it and agreed with the signature
/// the reference claimed: a fabricated claim is refused before the row exists,
/// and no caller can construct this value carrying one.
#[derive(Clone, Debug, PartialEq)]
pub struct AdmittedProcessIdentity {
    identity: ProcessIdentity,
}

impl AdmittedProcessIdentity {
    pub(crate) fn admitted(identity: ProcessIdentity) -> Self {
        Self { identity }
    }

    /// Replay an identity that was admitted once and then durably pinned: a
    /// process row a remote peer already created and is now reporting.
    ///
    /// This is a **replay**, never an admission. Do not reach for it on a path
    /// that creates a durable row from caller-supplied input: there the
    /// registry's [`admit`](ProcessEngineRegistry::admit) is the only route,
    /// because it is what checks a signature claim.
    pub fn pinned(identity: ProcessIdentity) -> Self {
        Self { identity }
    }

    #[cfg(any(test, feature = "testing"))]
    pub fn for_testing(identity: ProcessIdentity) -> Self {
        Self { identity }
    }

    /// Borrow the admitted identity.
    pub fn identity(&self) -> &ProcessIdentity {
        &self.identity
    }

    pub fn into_identity(self) -> ProcessIdentity {
        self.identity
    }
}

/// Pure admission policy for immutable, recorded engine-start inputs.
///
/// The function pointer cannot capture an engine, store, catalog, or session.
#[derive(Clone, Copy)]
pub struct ProcessEngineAdmission {
    kind: &'static str,
    admit: fn(
        &'static str,
        &serde_json::Value,
        Option<&ProcessExecutionEnvSpec>,
    ) -> Result<ProcessIdentity, crate::PluginError>,
}

impl ProcessEngineAdmission {
    pub const fn new(
        kind: &'static str,
        admit: fn(
            &'static str,
            &serde_json::Value,
            Option<&ProcessExecutionEnvSpec>,
        ) -> Result<ProcessIdentity, crate::PluginError>,
    ) -> Self {
        Self { kind, admit }
    }

    pub const fn accepting(kind: &'static str) -> Self {
        Self::new(kind, |kind, _, _| Ok(ProcessIdentity::new(kind)))
    }

    pub fn kind(self) -> &'static str {
        self.kind
    }

    pub fn admit(
        self,
        payload: &serde_json::Value,
        env_spec: Option<&ProcessExecutionEnvSpec>,
    ) -> Result<ProcessIdentity, crate::PluginError> {
        (self.admit)(self.kind, payload, env_spec)
    }
}

#[derive(Clone)]
pub struct ProcessEngineRegistration {
    engine: Arc<dyn ProcessEngine>,
    admission: ProcessEngineAdmission,
    engine_steps: Option<Arc<dyn super::engine_state::EngineSteps>>,
    document_provider: Option<Arc<dyn ProcessDocumentProvider>>,
    /// The host's retry policies for engine step kinds, over the engine's.
    engine_step_retries:
        BTreeMap<super::engine_state::EngineStepKind, lash_sansio::ExecutionPolicy>,
}

impl ProcessEngineRegistration {
    pub fn new(
        engine: Arc<dyn ProcessEngine>,
        admission: ProcessEngineAdmission,
    ) -> Result<Self, crate::PluginError> {
        if engine.kind() != admission.kind() {
            return Err(crate::PluginError::Registration(format!(
                "process engine kind `{}` does not match admission kind `{}`",
                engine.kind(),
                admission.kind()
            )));
        }
        Ok(Self {
            engine,
            admission,
            engine_steps: None,
            document_provider: None,
            engine_step_retries: BTreeMap::new(),
        })
    }

    /// Pair an engine with the default recorded-input admission policy.
    pub fn accepting(engine: Arc<dyn ProcessEngine>) -> Self {
        let admission = ProcessEngineAdmission::accepting(engine.kind());
        Self {
            engine,
            admission,
            engine_steps: None,
            document_provider: None,
            engine_step_retries: BTreeMap::new(),
        }
    }

    /// Declare the engine's own step bodies: what runs a
    /// [`StepRequest::Engine`](super::StepRequest::Engine) its `advance`
    /// asks for.
    #[must_use]
    pub fn with_engine_steps(mut self, steps: Arc<dyn super::engine_state::EngineSteps>) -> Self {
        self.engine_steps = Some(steps);
        self
    }

    /// Declare how this engine's definitions read as a language document.
    #[must_use]
    pub fn with_document_provider(mut self, provider: Arc<dyn ProcessDocumentProvider>) -> Self {
        self.document_provider = Some(provider);
        self
    }

    /// Retry the engine's `kind` steps under `policy` instead of the policy
    /// its [`EngineSteps`](super::engine_state::EngineSteps) declare: the
    /// host's override of the engine author's default, for this deployment.
    #[must_use]
    pub fn with_engine_step_retry(
        mut self,
        kind: super::engine_state::EngineStepKind,
        policy: lash_sansio::ExecutionPolicy,
    ) -> Self {
        self.engine_step_retries.insert(kind, policy);
        self
    }
}

#[derive(Clone, Default)]
pub struct ProcessEngineRegistry {
    engines: Arc<BTreeMap<String, Arc<dyn ProcessEngine>>>,
    admissions: Arc<BTreeMap<String, ProcessEngineAdmission>>,
    engine_steps: Arc<BTreeMap<String, Arc<dyn super::engine_state::EngineSteps>>>,
    document_providers: Arc<BTreeMap<String, Arc<dyn ProcessDocumentProvider>>>,
    artifact_ports: Option<Arc<super::ArtifactReferrerPorts>>,
}

/// A [`ProcessEngineRegistry`] held weakly: it keeps no engine alive.
#[derive(Clone)]
pub struct WeakProcessEngineRegistry {
    engines: std::sync::Weak<BTreeMap<String, Arc<dyn ProcessEngine>>>,
    admissions: std::sync::Weak<BTreeMap<String, ProcessEngineAdmission>>,
    engine_steps: std::sync::Weak<BTreeMap<String, Arc<dyn super::engine_state::EngineSteps>>>,
    document_providers: std::sync::Weak<BTreeMap<String, Arc<dyn ProcessDocumentProvider>>>,
    artifact_ports: Option<Arc<super::ArtifactReferrerPorts>>,
}

impl WeakProcessEngineRegistry {
    /// The registry, while some holder keeps it alive.
    #[must_use]
    pub fn upgrade(&self) -> Option<ProcessEngineRegistry> {
        Some(ProcessEngineRegistry {
            engines: self.engines.upgrade()?,
            admissions: self.admissions.upgrade()?,
            engine_steps: self.engine_steps.upgrade()?,
            document_providers: self.document_providers.upgrade()?,
            artifact_ports: self.artifact_ports.clone(),
        })
    }
}

/// An engine's steps under the host's per-kind retry overrides.
struct RetriedEngineSteps {
    steps: Arc<dyn super::engine_state::EngineSteps>,
    retries: BTreeMap<super::engine_state::EngineStepKind, lash_sansio::ExecutionPolicy>,
}

#[async_trait::async_trait]
impl super::engine_state::EngineSteps for RetriedEngineSteps {
    fn kinds(&self) -> Vec<super::engine_state::EngineStepKind> {
        self.steps.kinds()
    }

    fn execution(&self, kind: &super::engine_state::EngineStepKind) -> std::time::Duration {
        self.steps.execution(kind)
    }

    fn retry(&self, kind: &super::engine_state::EngineStepKind) -> lash_sansio::ExecutionPolicy {
        self.retries
            .get(kind)
            .copied()
            .unwrap_or_else(|| self.steps.retry(kind))
    }

    async fn run(
        &self,
        run: super::engine_state::EngineStepRun,
        cancel: tokio_util::sync::CancellationToken,
    ) -> crate::runtime::actor::round::SettledOutput {
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
            engines: Arc::downgrade(&self.engines),
            admissions: Arc::downgrade(&self.admissions),
            engine_steps: Arc::downgrade(&self.engine_steps),
            document_providers: Arc::downgrade(&self.document_providers),
            artifact_ports: self.artifact_ports.clone(),
        }
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
        let mut engines = (*self.engines).clone();
        let mut admissions = (*self.admissions).clone();
        let mut engine_steps = (*self.engine_steps).clone();
        let mut document_providers = (*self.document_providers).clone();
        let ProcessEngineRegistration {
            engine,
            admission,
            engine_steps: steps,
            document_provider,
            engine_step_retries: retries,
        } = registration;
        match steps {
            Some(steps) if retries.is_empty() => {
                engine_steps.insert(engine.kind().to_string(), steps)
            }
            Some(steps) => engine_steps.insert(
                engine.kind().to_string(),
                Arc::new(RetriedEngineSteps { steps, retries }),
            ),
            None => engine_steps.remove(engine.kind()),
        };
        match document_provider {
            Some(provider) => document_providers.insert(engine.kind().to_string(), provider),
            None => document_providers.remove(engine.kind()),
        };
        engines.insert(engine.kind().to_string(), engine);
        admissions.insert(admission.kind().to_string(), admission);
        Self {
            engines: Arc::new(engines),
            admissions: Arc::new(admissions),
            engine_steps: Arc::new(engine_steps),
            document_providers: Arc::new(document_providers),
            artifact_ports: self.artifact_ports,
        }
    }

    /// The body that runs `kind` for processes of engine `engine`: the
    /// typed refusal, before admission, of a
    /// [`StepRequest::Engine`](super::StepRequest::Engine) no registration
    /// declares.
    ///
    /// # Errors
    ///
    /// [`EngineStepRefusal`](super::engine_state::EngineStepRefusal) when
    /// the engine is unknown, declares no engine steps, or not `kind`.
    pub fn engine_steps(
        &self,
        engine: &str,
        kind: &super::engine_state::EngineStepKind,
    ) -> Result<Arc<dyn super::engine_state::EngineSteps>, super::engine_state::EngineStepRefusal>
    {
        use super::engine_state::EngineStepRefusal;
        if !self.engines.contains_key(engine) {
            return Err(EngineStepRefusal::UnknownEngine {
                engine: engine.to_owned(),
            });
        }
        let steps =
            self.engine_steps
                .get(engine)
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
        self.document_providers.get(kind).cloned()
    }

    /// Apply one resolved cleanup to every installed engine's own store
    /// (ADR 0113 §2.2): each engine receives the referrer and only the
    /// carries under `ArtifactStoreId::Engine` of its own kind, and ends the
    /// referrer in its store even when it has none to carry.
    pub async fn end_artifact_referrer(
        &self,
        cleanup: &crate::ResolvedArtifactCleanup,
    ) -> Result<(), crate::ArtifactStoreError> {
        for (kind, engine) in self.engines.iter() {
            let own = crate::ResolvedArtifactCleanup::for_store(
                &cleanup.referrer,
                &cleanup.carries,
                &crate::ArtifactStoreId::Engine(kind.clone()),
            );
            engine.end_artifact_referrer(&own).await?;
        }
        Ok(())
    }

    /// This is the single enforcement point for unique engine kinds across everything
    /// registered on a runtime host, whether the engine was wired directly or contributed
    /// through the plugin contract.
    pub(crate) fn try_with_engine(
        self,
        registration: ProcessEngineRegistration,
    ) -> Result<Self, crate::PluginError> {
        if self.engines.contains_key(registration.engine.kind()) {
            return Err(crate::PluginError::Registration(format!(
                "duplicate process engine kind `{}`; each engine kind may be registered once",
                registration.engine.kind()
            )));
        }
        Ok(self.with_registration(registration))
    }

    /// Every registered engine, by kind.
    pub fn engines(&self) -> impl Iterator<Item = &Arc<dyn ProcessEngine>> {
        self.engines.values()
    }

    pub(crate) fn get(&self, kind: &str) -> Option<Arc<dyn ProcessEngine>> {
        self.engines.get(kind).cloned()
    }

    /// Resolve the engine a `ProcessInput::Engine` start names, refusing a kind
    /// this host never registered.
    ///
    pub fn require(&self, kind: &str) -> Result<Arc<dyn ProcessEngine>, crate::PluginError> {
        self.get(kind).ok_or_else(|| {
            crate::PluginError::Session(format!("process engine `{kind}` is not configured"))
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
        env_spec: Option<&ProcessExecutionEnvSpec>,
    ) -> Result<AdmittedProcessIdentity, crate::PluginError> {
        let identity = self
            .admissions
            .get(kind)
            .copied()
            .ok_or_else(|| {
                crate::PluginError::Session(format!("process engine `{kind}` is not configured"))
            })?
            .admit(payload, env_spec)?;
        let Some(reference) = identity.definition.clone() else {
            return Ok(AdmittedProcessIdentity::admitted(identity));
        };
        let resolution = match self.resolve(&reference).await {
            Ok(resolution) => resolution,
            // An engine that cannot read its definition right now has said
            // nothing about the claim. Start admission is a recorded-input
            // decision (FIG-1838/FIG-1521): a live artifact-store outage belongs
            // to retryable execution after the row is durable, so an *unclaimed*
            // reference is admitted unresolved rather than refused for the
            // lifetime of the intent. A reference that does claim a signature is
            // still refused — an unverifiable claim never creates a row.
            Err(ProcessDefinitionRefusal::UnresolvableDefinition { .. })
                if reference.signature.is_unknown() =>
            {
                return Ok(AdmittedProcessIdentity::admitted(identity));
            }
            Err(refusal) => return Err(refusal.into()),
        };
        // The durable row pins the engine's authority, not the claim that
        // arrived: they are equal by the check above when a claim was made, and
        // an unclaimed reference adopts the artifact's signature here rather
        // than recording "unknown" forever.
        let identity = ProcessIdentity {
            definition: Some(reference.with_resolved_signature(resolution.signature)),
            ..identity
        };
        Ok(AdmittedProcessIdentity::admitted(identity))
    }

    pub async fn resolve(
        &self,
        reference: &ProcessDefinitionRef,
    ) -> Result<ProcessDefinitionResolution, ProcessDefinitionRefusal> {
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
            });
        }
        Ok(resolution)
    }
}

#[cfg(test)]
#[path = "engine_resolve_tests.rs"]
mod resolve_tests;
