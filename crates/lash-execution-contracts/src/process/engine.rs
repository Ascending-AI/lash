use super::events::ProcessAwaitOutput;
use super::model::ProcessIdentity;

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
    pub caught: Vec<lash_vocabulary::sites::EffectId>,
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

/// A linked execution reference; origin remains retained by the host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProcessDocumentRefRead {
    Named(LinkedStart),
    ArtifactMissing { artifact: ArtifactName },
}
use crate::ArtifactStoreError;
use crate::PluginError;
use crate::process::{
    ArgsMismatch, ArgsMode, EngineAction, EngineEvent, EngineSettings, EngineState,
    EngineStateFormat, EngineStepKind, ProcessDefinitionDraft, ProcessDefinitionError,
    ProcessDefinitionRef, ProcessDefinitionResolution, ProcessSignature,
};
use lash_core_store::artifact_referrer::{ArtifactName, ReferrerClaim, ResolvedArtifactCleanup};
use lash_core_store::executable_generation::ExecutableGeneration;
use lash_core_store::tool_run::SettledOutput;
use lash_vocabulary::types::*;
use lash_vocabulary::{claims::*, names::*, package::*};
use serde::{Deserialize, Serialize};
#[async_trait::async_trait]
/// Neutral retained lifecycle interface. Inspecting a Link never fulfils it against current advertisements.
pub trait ProcessEngine: Send + Sync {
    fn kind(&self) -> &'static str;
    fn trace_class(&self) -> ProcessTraceClass;
    fn state_format(&self) -> EngineStateFormat;
    fn cancel_grace(&self) -> std::time::Duration;
    fn program_identity(&self, payload: &serde_json::Value) -> Option<ExecutableGeneration>;
    fn check_settings(
        &self,
        payload: &serde_json::Value,
        settings: Option<&EngineSettings>,
    ) -> Result<(), PluginError>;
    fn default_settings(
        &self,
        payload: &serde_json::Value,
    ) -> Result<Option<EngineSettings>, PluginError>;
    fn settled_ran(&self, kind: &EngineStepKind, output: &SettledOutput) -> SettledRan;
    fn advance(
        &self,
        state: EngineState,
        event: EngineEvent,
    ) -> Result<(EngineState, EngineAction), ProcessInfraError>;
    fn start_artifacts(
        &self,
        payload: &serde_json::Value,
    ) -> Result<Vec<ArtifactName>, PluginError>;
    async fn end_artifact_referrer(
        &self,
        cleanup: &ResolvedArtifactCleanup,
    ) -> Result<(), ArtifactStoreError>;
    async fn acquire_engine_artifact(
        &self,
        claim: &ReferrerClaim,
        artifact_ref: &str,
    ) -> Result<(), PluginError>;
    async fn inspect_link(&self, start: &LinkedStart) -> Result<LinkInspection, EnginePortError>;
    fn capabilities(&self) -> NodeRequirements;
    async fn check_args(
        &self,
        signature: &ProcessSignature,
        args: &serde_json::Map<String, serde_json::Value>,
        mode: ArgsMode,
    ) -> Result<(), ArgsMismatch>;
    async fn resolve(
        &self,
        reference: &ProcessDefinitionRef,
    ) -> Result<ProcessDefinitionResolution, ProcessDefinitionError>;
}

#[async_trait::async_trait]
/// Trusted runtime linking, inspection, retention and opaque document ports; fulfil does no durable publication.
pub trait ProcessDocumentProvider: Send + Sync {
    async fn fulfil(&self, request: &FulfilRequest) -> Result<Fulfilled, EnginePortError>;
    async fn inspect_link(
        &self,
        link: LinkId,
        selector: &ReachSelector,
    ) -> Result<LinkInspection, EnginePortError>;
    async fn hold(
        &self,
        claim: &ReferrerClaim,
        reference: &ContentRef,
    ) -> Result<(), EnginePortError>;
    async fn load(&self, reference: &ContentRef) -> Result<LoadedContent, EnginePortError>;
    fn capabilities(&self) -> NodeRequirements;
    async fn document(
        &self,
        payload: &serde_json::Value,
    ) -> Result<ProcessDocumentRead, PluginError>;
    async fn document_ref(
        &self,
        payload: &serde_json::Value,
    ) -> Result<ProcessDocumentRefRead, PluginError>;
    async fn execution_document(
        &self,
        reference: &LinkedStart,
    ) -> Result<ProcessExecutionDocumentRead, PluginError>;
    async fn admit(
        &self,
        claim: &ReferrerClaim,
        request: ProcessDocument,
    ) -> Result<ProcessDocument, PluginError>;
    async fn environment(&self, request: ProcessDocument) -> Result<ProcessDocument, PluginError>;
    async fn replay(&self, request: ProcessDocument) -> Result<ProcessDocument, PluginError>;
    fn counts_statements(&self, settings: Option<&EngineSettings>) -> bool;
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
/// Unlinked guest documents fulfil anew; admitted linked inputs retain their defining origin.
pub enum InlineCode {
    Unlinked {
        document: DocumentId,
        selector: StartSelector,
    },
    Linked {
        start: LinkedStart,
    },
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// Chosen code and explicit host namespace, publication context and attachment.
pub struct InlineRequest {
    pub code: InlineCode,
    pub snapshot: NamespaceSnapshot,
    pub context: PublicationContext,
    pub attachment: Attachment,
}
#[derive(Debug)]
/// Prepared process draft, authoritative signature and linked inspection. Realize holds before publication.
pub struct InlineDefinition {
    pub draft: ProcessDefinitionDraft,
    pub signature: ProcessSignature,
    pub linked: LinkInspection,
}
#[async_trait::async_trait]
/// Inspect prepares without publication or resolver calls; realize holds full reach under the supplied claim.
pub trait InlineStarts: Send + Sync {
    async fn inspect(&self, request: &InlineRequest) -> Result<InlineDefinition, EnginePortError>;
    async fn realize(
        &self,
        claim: &ReferrerClaim,
        request: &InlineRequest,
    ) -> Result<InlineDefinition, EnginePortError>;
}
