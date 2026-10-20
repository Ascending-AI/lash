use super::definition_ref::ProcessDefinitionRef;
use super::definition_ref::ProcessDefinitionResolution;
use super::engine_state::{EngineAction, EngineEvent, EngineState, EngineStateFormat};
use super::events::ProcessAwaitOutput;
use super::model::{EngineSettings, ProcessIdentity};

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
/// The effects a process performs, each with the requirement its code
/// states: the kernel signature it was checked against and the contract it
/// pins (P3, OP-CONTRACT-PIN).
pub type PerformedEffects =
    std::collections::BTreeMap<lash_kernel_doc::EffectName, lash_kernel_doc::OperationRequirement>;

/// What the package that owns an operation's path declares of it: the
/// requirement and intents every runner of it must serve (PKG-RUNNER).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OwnedOperation {
    /// The owning package's path, dotted.
    pub package: String,
    /// Its declared signature and contract id.
    pub requirement: lash_kernel_doc::OperationRequirement,
    /// The intents it declares, by wire name.
    pub intents: std::collections::BTreeSet<String>,
}

/// The performed operations whose paths a package owns, by effect.
pub type OwnedOperations = std::collections::BTreeMap<lash_kernel_doc::EffectName, OwnedOperation>;

/// Whether an engine is an internal process or a user-facing workflow.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessTraceClass {
    #[default]
    Internal,
    Workflow,
}

/// What one settled engine step recorded for its process's
/// `process.ran` (FIG-6370, FIG-6817).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SettledRan {
    /// The document statement starts and empty-block entries the step
    /// counted, by document site ordinal.
    pub ran: Vec<(u32, u64)>,
    /// The effect occurrences whose failed outcome a `catch` in the step
    /// recovered from, in order.
    pub caught: Vec<lash_protocol_types::EffectId>,
}

/// Every engine answers every execution question
/// (law S1, no silent defaults). Core built-ins (`SessionTurn` and
/// `External`) are not registered here.
#[async_trait::async_trait]
pub trait ProcessEngine: Send + Sync {
    /// The kind every process of this engine is registered under.
    fn kind(&self) -> &'static str;

    /// The span taxonomy this engine explicitly declares for its processes.
    fn trace_class(&self) -> ProcessTraceClass;

    /// The encoding of this engine's state. L11's claim filter reads it.
    fn state_format(&self) -> EngineStateFormat;

    /// How long a cancelled process may run best-effort steps before lash
    /// forces its terminal. A live engine control, read when the cancel's
    /// `Started` is handled; it is not recorded with the process.
    fn cancel_grace(&self) -> std::time::Duration;

    /// The executable generation a run of `payload` would run as (FIG-3571),
    /// or `None` for an engine whose runs carry no generation, or for a
    /// payload the engine refuses anyway.
    fn program_identity(&self, payload: &serde_json::Value) -> Option<crate::ExecutableGeneration>;

    /// Check the settings a starter states for a start of `payload` (D4):
    /// pure, called before the start is durable. It refuses an engine that
    /// requires settings and gets none, and settings it cannot decode or does
    /// not serve. Every run of the process reads the checked settings back
    /// from its row, never a live default.
    fn check_settings(
        &self,
        payload: &serde_json::Value,
        settings: Option<&EngineSettings>,
    ) -> Result<(), crate::PluginError>;

    /// This build's default settings for a start of `payload`, or `None` for
    /// an engine with no settings. Only a starter calls it, explicitly; lash
    /// never calls it to fill a start's settings.
    fn default_settings(
        &self,
        payload: &serde_json::Value,
    ) -> Result<Option<EngineSettings>, crate::PluginError>;

    /// What a settled engine step of `kind` recorded for its process's
    /// `process.ran`, as its committed `output` records it: the document
    /// statement starts and empty-block entries it counted, by document
    /// site ordinal (FIG-6370), and the effect occurrences whose failed
    /// outcome a `catch` in it recovered from (FIG-6817). Pure, like
    /// `advance`. Lash appends them as one `process.ran` in the
    /// transaction that hands the step to `advance`. An engine whose steps
    /// count nothing, and a step that counted nothing, answer none.
    fn settled_ran(
        &self,
        kind: &super::engine_state::EngineStepKind,
        output: &lash_core_store::tool_run::SettledOutput,
    ) -> SettledRan;

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

    /// The effects a run of `payload` performs (P3): each path its code
    /// calls or spawns, with the kernel signature it was checked against.
    /// An env links exactly these. An engine that performs no effect of its
    /// own answers an empty map: its runs link nothing.
    ///
    /// # Errors
    ///
    /// The payload names effects the engine cannot certify, typed.
    async fn performed_effects(
        &self,
        payload: &serde_json::Value,
    ) -> Result<PerformedEffects, crate::ProcessEnvRefusal>;

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
    /// Store and transport faults return `ProcessDefinitionError::Fault`;
    /// only a definition judgment returns `ProcessDefinitionError::Refused`.
    ///
    /// This is an **integrator class 3: process-engine implementor** seam.
    async fn resolve(
        &self,
        reference: &ProcessDefinitionRef,
    ) -> Result<ProcessDefinitionResolution, super::ProcessDefinitionError>;
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
    /// An artifact the definition reads is retained with bytes that are not
    /// the artifact its name says.
    ArtifactCorrupt {
        artifact: crate::ArtifactName,
        message: String,
    },
}

/// What a [`ProcessDocumentProvider`] reads for a document reference.
#[derive(Debug)]
pub enum ProcessExecutionDocumentRead {
    Read(ProcessDocument),
    /// Nothing retains the module the reference names, or an artifact it
    /// reads.
    ArtifactMissing {
        artifact: crate::ArtifactName,
    },
    /// An artifact the document reads is retained with bytes that are not
    /// the artifact its name says.
    ArtifactCorrupt {
        artifact: crate::ArtifactName,
        message: String,
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
/// (FIG-5563), and its admission of such a document as a definition
/// (FIG-5574). An engine registered without one has no document: a host's
/// inspection of its processes and definitions, and its publication of a
/// document, answer `Unsupported`.
#[async_trait::async_trait]
pub trait ProcessDocumentProvider: Send + Sync {
    /// The operations among `performed` whose paths a package owns, in the
    /// package view the engine's documents run under, each with what its
    /// package declares (PKG-RUNNER).
    fn owned_operations(&self, performed: &PerformedEffects) -> OwnedOperations;

    /// The producer's package set for a host start with no parent cell.
    fn definition_packages(
        &self,
        payload: &serde_json::Value,
    ) -> Result<Option<lash_kernel_doc::PackageSetDigest>, crate::PluginError>;
    fn initial_packages(&self) -> Result<lash_kernel_doc::PackageSetDigest, crate::PluginError>;
    /// Resolve the definition's selected surface snapshot.
    fn package_view(
        &self,
        packages: lash_kernel_doc::PackageSetDigest,
    ) -> Result<std::sync::Arc<dyn lash_kernel_dialect::ExportView + Send + Sync>, crate::PluginError>;

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

    /// The document `reference` names, with the entry it selects: what an
    /// execution that named it runs, whether that is a definition's process
    /// or a module's main body. The answer is the provider's own type, or
    /// `ArtifactMissing` when nothing retains the module any more.
    async fn execution_document(
        &self,
        reference: &lash_trace::WorkflowDocumentRef,
    ) -> Result<ProcessExecutionDocumentRead, crate::PluginError>;

    /// Admit a language document as a definition. The engine validates the
    /// document itself, derives everything a definition states from it, and
    /// only then holds what the definition executes under `claim` in its own
    /// stores. A document it refuses holds nothing.
    ///
    /// Core names no language here either: `request` and the answer are the
    /// provider's own types, which the caller that knows them builds and
    /// takes back with [`ProcessDocument::downcast`].
    async fn admit(
        &self,
        claim: &crate::ReferrerClaim,
        request: ProcessDocument,
    ) -> Result<ProcessDocument, crate::PluginError>;

    /// What a document of this engine is written, edited and admitted
    /// against, for the processes `request` describes. A host edits a
    /// document against the same environment its publication is admitted
    /// under, so an edit the engine would refuse is refused where it is
    /// made. Nothing is stored. `request` and the answer are the
    /// provider's own types.
    async fn environment(
        &self,
        request: ProcessDocument,
    ) -> Result<ProcessDocument, crate::PluginError>;

    /// Replays recorded run segments of one process from what its journal
    /// retains, on demand. It is a read: nothing runs that the process
    /// performs, and nothing is recorded. `request` and the answer are the
    /// provider's own types.
    async fn replay(&self, request: ProcessDocument)
    -> Result<ProcessDocument, crate::PluginError>;

    /// Whether a process recorded with `settings` counts the statements its
    /// run segments start: what its workflow overlay reports it observed.
    fn counts_statements(&self, settings: Option<&EngineSettings>) -> bool;
}

/// What an engine reads of a function a start carries inline: the
/// definition a start of it runs, its signature and the effects it performs
/// (P3). Derived from the payload alone: nothing stored is read.
#[derive(Clone, Debug, PartialEq)]
pub struct InlineDefinition {
    pub draft: super::ProcessDefinitionDraft,
    pub signature: super::ProcessSignature,
    pub performed: PerformedEffects,
}

/// An engine's start of a function carried inline
/// ([`ProcessStartTarget::Inline`](super::ProcessStartTarget::Inline),
/// FIG-6382). An engine registered without one refuses every inline start.
///
/// Core names no language here: the payload is the engine's own value of a
/// function, which it checks itself.
#[async_trait::async_trait]
pub trait InlineStarts: Send + Sync {
    /// Check `payload` and derive what a start of it runs. Stores nothing.
    ///
    /// # Errors
    ///
    /// A `DefinitionRefused` runtime error for a payload the engine refuses.
    async fn inspect(
        &self,
        payload: &serde_json::Value,
    ) -> Result<InlineDefinition, crate::PluginError>;

    /// [`Self::inspect`], then hold what the definition executes under
    /// `claim` in the engine's own stores, written the way the engine writes
    /// every document it admits. The descriptor is not published: the
    /// caller publishes [`InlineDefinition::draft`] under the same claim.
    ///
    /// # Errors
    ///
    /// [`Self::inspect`]'s refusal, the claim's fence (`ReferrerEnded`), and
    /// every store failure.
    async fn realize(
        &self,
        claim: &crate::ReferrerClaim,
        payload: &serde_json::Value,
    ) -> Result<InlineDefinition, crate::PluginError>;
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
    /// Replay an identity that was admitted once and then durably pinned: a
    /// process row a remote peer already created and is now reporting.
    ///
    /// This is a **replay**, never an admission. Do not reach for it on a path
    /// that creates a durable row from caller-supplied input: there the
    /// registry's `ProcessEngineRegistry::admit` is the only route,
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
    admit: fn(&'static str, &serde_json::Value) -> Result<ProcessIdentity, crate::PluginError>,
}

impl ProcessEngineAdmission {
    pub const fn new(
        kind: &'static str,
        admit: fn(&'static str, &serde_json::Value) -> Result<ProcessIdentity, crate::PluginError>,
    ) -> Self {
        Self { kind, admit }
    }

    pub const fn accepting(kind: &'static str) -> Self {
        Self::new(kind, |kind, _| Ok(ProcessIdentity::new(kind)))
    }

    pub fn kind(self) -> &'static str {
        self.kind
    }

    pub fn admit(self, payload: &serde_json::Value) -> Result<ProcessIdentity, crate::PluginError> {
        (self.admit)(self.kind, payload)
    }
}

/// Runtime construction after the engine admitted the process identity.
#[doc(hidden)]
pub fn admitted_process_identity(identity: ProcessIdentity) -> AdmittedProcessIdentity {
    AdmittedProcessIdentity { identity }
}
