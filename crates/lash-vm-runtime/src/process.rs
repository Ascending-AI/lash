//! The kernel process engine (kernel spec §8; ADR 0132 §7, §10).
//!
//! A process's code is an entry of an admitted kernel document: a declared
//! function a host may start, with a typed signature. The process names the
//! document by identity, so a running instance keeps the document it was
//! admitted under. Its body runs as a kernel machine in a worker; every
//! recorded `perform` is one durable step, while a read is answered inline; every `sleep` a durable timer, and a body
//! that fans out waits on all of them at once ([`advance`]).
//!
//! Starting and awaiting a process are effects like any other: the host
//! offers them as tools. A function reference passed to an effect crosses
//! the boundary as the definition of the entry it names, which is what the
//! start effect starts.

mod advance;
mod documents;
mod helpers;
mod inline;
mod migrate;
mod page_carry;
mod pages;
pub use page_carry::{carry_parked_checkpoint, carry_snapshot};
#[doc(hidden)]
pub use pages::check_parked_helpers;
mod reads;
mod replay;
mod run;
mod state;
mod succession;
mod trace;
mod workflow;

#[cfg(test)]
mod tests;

use std::sync::Arc;

use lash_execution_contracts::process::EngineStepAnswer;
use lash_execution_contracts::process::EngineStepKind;
use lash_execution_contracts::process::EngineStepRun;
use lash_kernel_doc::IdentifiedDocument;

use lash_vocabulary::names::DocumentId;

use lash_vocabulary::types::Signature;
use tokio_util::sync::CancellationToken;

/// Kernel start input: linked source selection or a callable with its frozen environment.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum InlineRequest {
    Linked {
        start: lash_vocabulary::names::LinkedStart,
    },
    Callable {
        link: lash_vocabulary::package::LinkId,
        function: Box<lash_kernel_doc::FunctionValue>,
        origin: lash_vocabulary::package::CallableOrigin,
    },
    Unlinked {
        document: DocumentId,
        selector: lash_vocabulary::names::StartSelector,
    },
}
impl InlineRequest {
    pub fn linked(
        &self,
    ) -> Option<(
        lash_vocabulary::package::LinkId,
        lash_execution_contracts::process::linking::ReachSelector,
    )> {
        use lash_execution_contracts::process::linking::ReachSelector;
        match self {
            Self::Linked { start } => {
                Some((start.link, ReachSelector::Start(start.selector.clone())))
            }
            Self::Callable { link, function, .. } => Some((
                *link,
                ReachSelector::Callable {
                    function: function.callable.function,
                },
            )),
            Self::Unlinked { .. } => None,
        }
    }
    pub(crate) fn links(&self) -> std::collections::BTreeSet<lash_vocabulary::package::LinkId> {
        let mut links = std::collections::BTreeSet::new();
        match self {
            Self::Linked { start } => {
                links.insert(start.link);
            }
            Self::Callable {
                link,
                function,
                origin,
            } => {
                links.extend(&origin.authored);
                links.insert(*link);
                links.extend(function.nodes().iter().map(|node| node.callable.link));
            }
            Self::Unlinked { .. } => {}
        }
        links
    }
    pub fn artifacts(&self) -> Vec<lash_core::ArtifactName> {
        let mut artifacts: Vec<_> = self
            .links()
            .into_iter()
            .map(documents::link_artifact)
            .collect();
        if let Self::Callable { origin, .. } = self {
            artifacts.extend(
                origin
                    .publications
                    .iter()
                    .map(|key| lash_core::ArtifactName {
                        store: lash_core::ArtifactStoreId::KernelDocument,
                        artifact_ref: lash_durable::packages::publication_reference(key),
                    }),
            );
        }
        artifacts
    }
}
pub(crate) use documents::ProcessPreparations;
pub use documents::{KernelDocuments, StoredSourceMap, Unretained, map_link_refusal};
pub use helpers::{HelperAdoptionRefusal, plan_adoption};
pub use migrate::{
    KernelMigrationRefusal, KernelMigrationSurvey, KernelMigrationSurveyError, PlannedMigration,
    RefusedKernelCell, RefusedKernelProcess, UnmigratedKernelSession, migrate_function_value,
    migrate_saved_function, migration_refusal, plan_migration, survey_kernel_processes,
};
pub use replay::{
    ProcessReplayRequest, ReplayFault, ReplayPath, ReplaySegments, SegmentAnswer, SegmentReplay,
    TaskPath, Unreplayable,
};
pub use state::{KERNEL_RUN_STEP, KernelProcessDefinition, KernelProcessInput};
pub use workflow::{
    WorkflowEnvironment, WorkflowEnvironmentRequest, WorkflowPublishRequest,
    WorkflowSourceServiceRequest,
};

/// The engine kind every kernel process is registered under.
pub use lash_protocol_types::LASH_VM_ENGINE_KIND;

/// The error kind raised at a `perform` of an effect the host does not
/// offer.
pub const EFFECT_UNKNOWN: &str = "unknown_effect";
/// The error kind raised at a `perform` whose arguments the effect cannot
/// take.
pub const EFFECT_ARGUMENTS: &str = "tool_arguments";

/// The error kind of a `perform` whose tool reported a failure. The tool's
/// failure is the error's data.
pub const TOOL_FAILED: &str = "tool_failed";

/// Why a kernel process failed, as its terminal's failure code states it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum KernelProcessFailureCode {
    /// The body ended with `fail`.
    Failed,
    /// The body ended in an uncaught error or a deadlock.
    RuntimeError,
    /// The body ended with tasks unfinished or failed unobserved
    /// (`K-TASK-016`).
    TasksOutstanding,
    /// The run passed one of its recorded bounds.
    BoundExceeded,
    /// The body finished with a value that is not effect data.
    ResultNotData,
    PayloadInvalid,
    ArgumentsInvalid,
    DocumentMissing,
    /// A definition the document reaches, which the node does not hold, is
    /// not stored as it should be (FIG-6985).
    DefinitionUnretained,
    /// A stored definition the process reaches is refused at admission:
    /// it performs an internal operation of a package it is not a member
    /// of (R5, FIG-6985).
    DefinitionRefused,
    SettingsInvalid,
    /// The machine refuses the run: its document or its saved state.
    RunRefused,
    /// The run reached no park after every attempt.
    RunUnsettled,
}

impl KernelProcessFailureCode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Failed => "process_failed",
            Self::RuntimeError => "process_runtime_error",
            Self::TasksOutstanding => "process_tasks_outstanding",
            Self::BoundExceeded => "process_bound_exceeded",
            Self::ResultNotData => "process_result_not_data",
            Self::PayloadInvalid => "process_payload_invalid",
            Self::ArgumentsInvalid => "process_arguments_invalid",
            Self::DocumentMissing => "process_document_missing",
            Self::DefinitionUnretained => "process_definition_unretained",
            Self::DefinitionRefused => "process_definition_refused",
            Self::SettingsInvalid => "process_settings_invalid",
            Self::RunRefused => "process_run_refused",
            Self::RunUnsettled => "process_run_unsettled",
        }
    }
}

pub(crate) fn failure(
    code: KernelProcessFailureCode,
    message: impl Into<String>,
    raw: Option<serde_json::Value>,
) -> lash_execution_contracts::process::ProcessAwaitOutput {
    caused_failure(code, message, raw, None)
}

/// [`failure`] with its typed cause.
pub(crate) fn caused_failure(
    code: KernelProcessFailureCode,
    message: impl Into<String>,
    raw: Option<serde_json::Value>,
    cause: Option<lash_core::ToolFailureCause>,
) -> lash_execution_contracts::process::ProcessAwaitOutput {
    let mut failure = lash_core::ToolFailure::runtime(
        lash_core::ToolFailureClass::Execution,
        code.as_str(),
        message,
    );
    failure.raw = raw.map(lash_core::ToolValue::untrusted_json);
    failure.cause = cause.map(Box::new);
    lash_execution_contracts::process::ProcessAwaitOutput::from_tool_output(
        lash_core::ToolCallOutput::failure(failure),
    )
}

/// The kernel engine's settings, which a start states and its row records
/// (FIG-6034 D4): the dialect its documents are written in, which a worker
/// must install to run it, the bounds every run of it is held to, and what
/// its runs record of the statements they run. Every run reads them back
/// from the record (FIG-5990), and a successor carry keeps them.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KernelRecordedSettings {
    pub dialect: String,
    pub charge: lash_kernel_vm::Charge,
    pub charge_limit: u64,
    pub memory: u64,
    pub memory_meter: lash_kernel_vm::MemoryMeter,
    pub call_depth: u32,
    pub live_tasks: u32,
    pub requests_per_park: u32,
    pub join_members: u32,
    /// `counts` commits each run segment's statement starts as a
    /// `process.ran` event, with the host reads and start that replay the
    /// segment (FIG-6370). The host chooses it per start; it is `off`
    /// unless the start states otherwise, and lash has no default of its
    /// own for it.
    #[serde(
        default,
        with = "observe_level",
        skip_serializing_if = "observe_level::is_off"
    )]
    pub observe: lash_kernel_vm::ObservationMode,
}

/// Engine settings that cannot decode as kernel recorded settings.
#[derive(Debug, thiserror::Error)]
#[error("{source}")]
pub struct KernelSettingsError {
    #[source]
    source: serde_json::Error,
}

impl KernelRecordedSettings {
    /// The settings of a process written in `dialect` and held to `bounds`,
    /// observed `off`.
    #[must_use]
    pub fn new(dialect: &str, bounds: lash_kernel_vm::Bounds) -> Self {
        Self {
            dialect: dialect.to_owned(),
            charge: bounds.charge,
            charge_limit: bounds.charge_limit,
            memory: bounds.memory,
            memory_meter: bounds.memory_meter,
            call_depth: bounds.call_depth,
            live_tasks: bounds.live_tasks,
            requests_per_park: bounds.requests_per_park,
            join_members: bounds.join_members,
            observe: lash_kernel_vm::ObservationMode::Off,
        }
    }

    /// These settings observed at `observe`.
    #[must_use]
    pub fn observing(mut self, observe: lash_kernel_vm::ObservationMode) -> Self {
        self.observe = observe;
        self
    }

    /// Decode settings received from a host transport into their typed form.
    ///
    /// # Errors
    /// Unknown fields, missing fields and invalid values return the typed
    /// JSON decoding error; no default settings replace a refused value.
    pub fn from_engine_settings(
        settings: &lash_execution_contracts::process::EngineSettings,
    ) -> Result<Self, KernelSettingsError> {
        serde_json::from_value(settings.value().clone())
            .map_err(|source| KernelSettingsError { source })
    }

    /// The engine settings a start records, including its typed observation choice.
    #[must_use]
    pub fn into_engine_settings(self) -> lash_execution_contracts::process::EngineSettings {
        lash_execution_contracts::process::EngineSettings::new(serde_json::json!(self))
    }

    /// The level a process recorded with `settings` is observed at: `off`
    /// when it recorded none, or settings that do not decode.
    #[must_use]
    pub fn observed(
        settings: Option<&lash_execution_contracts::process::EngineSettings>,
    ) -> lash_kernel_vm::ObservationMode {
        settings
            .and_then(|settings| serde_json::from_value::<Self>(settings.value().clone()).ok())
            .map_or(lash_kernel_vm::ObservationMode::Off, |settings| {
                settings.observe
            })
    }
}

/// The recorded spelling of an observation level: `"off"` or `"counts"`.
mod observe_level {
    use lash_kernel_vm::ObservationMode;
    use serde::{Deserialize, Deserializer, Serializer};

    #[expect(
        clippy::trivially_copy_pass_by_ref,
        reason = "serde's skip_serializing_if passes a reference"
    )]
    pub(super) fn is_off(observe: &ObservationMode) -> bool {
        *observe == ObservationMode::Off
    }

    #[expect(
        clippy::trivially_copy_pass_by_ref,
        reason = "serde's serialize_with passes a reference"
    )]
    pub(super) fn serialize<S: Serializer>(
        observe: &ObservationMode,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(match observe {
            ObservationMode::Off => "off",
            ObservationMode::Counts => "counts",
        })
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<ObservationMode, D::Error> {
        match String::deserialize(deserializer)?.as_str() {
            "off" => Ok(ObservationMode::Off),
            "counts" => Ok(ObservationMode::Counts),
            other => Err(serde::de::Error::unknown_variant(other, &["off", "counts"])),
        }
    }
}

/// The descriptor of the definition `entry` of `document` is: the engine,
/// the definition value, and the artifacts it holds: the document and the
/// declaration of every library function the document lists, each by its
/// own identity (FIG-6379).
///
/// # Errors
///
/// The draft's refusal, which a well-formed identity never meets.
pub fn definition_draft(
    definition: &KernelProcessDefinition,
) -> Result<
    lash_execution_contracts::process::ProcessDefinitionDraft,
    lash_execution_contracts::process::ProcessDefinitionDraftError,
> {
    if definition.code.linked().is_none() {
        return Err(lash_execution_contracts::process::ProcessDefinitionDraftError::UnlinkedCode);
    }
    let value = serde_json::json!({"code":definition.code});
    lash_execution_contracts::process::ProcessDefinitionDraft::new(
        lash_execution_contracts::process::ProcessEngineKind::new(LASH_VM_ENGINE_KIND),
        lash_execution_contracts::process::ProcessDefinitionValue::new(value),
        definition.artifacts(),
    )
}

/// The signature a definition of an entry states: the entry's kernel
/// signature, as data.
pub fn entry_signature(
    signature: &Signature,
) -> lash_execution_contracts::process::ProcessSignature {
    match serde_json::to_value(signature) {
        Ok(encoding) => lash_execution_contracts::process::ProcessSignature::known(encoding),
        Err(_) => lash_execution_contracts::process::ProcessSignature::Unknown,
    }
}

/// The definition an explicit publication of `function` of `document`,
/// linked under `packages`, would have: a pure id and signature derivation,
/// with no storage or registry read.
pub fn definition_of_start(
    start: &lash_vocabulary::names::LinkedStart,
    inspection: &lash_execution_contracts::process::linking::LinkInspection,
) -> Result<serde_json::Value, String> {
    let draft = definition_draft(&KernelProcessDefinition {
        code: InlineRequest::Linked {
            start: start.clone(),
        },
    })
    .map_err(|e| e.to_string())?;
    serde_json::to_value(lash_execution_contracts::process::ProcessDefinition::new(
        draft.id(),
        inspection.signature.as_ref().map_or(
            lash_execution_contracts::process::ProcessSignature::Unknown,
            entry_signature,
        ),
    ))
    .map_err(|e| e.to_string())
}

/// The kernel process engine.
#[derive(Clone)]
pub struct KernelProcessEngine {
    pub(crate) documents: KernelDocuments,
    /// The library functions the engine's workers hold, by identity: what a
    /// document is linked and admitted against in the parent.
    pub(crate) functions: Arc<lash_kernel_doc::FunctionRegistry>,
    pub(crate) helpers: Arc<lash_kernel_doc::BuildHelpers>,
    pub(crate) workers: lash_vm_client::service::Service,
    /// The dialect the host's process documents are written in: a new
    /// process records it, and its runs name it to their worker.
    pub(crate) dialect: String,
    pub(crate) bounds: lash_kernel_vm::Bounds,
    /// The kernel version this engine writes: the newest its build
    /// interprets.
    pub(crate) writes: lash_kernel_doc::KernelVersion,
    pub(crate) policy: KernelRunPolicy,
    pub(crate) trace_runtime: Option<lash_core::trace::TraceRuntime>,
    pub(crate) carry_stores: Option<Arc<dyn lash_execution_contracts::backend::StoreSet>>,
}

/// Host-selected policy for one `kernel_run`, separate from a process's
/// recorded bounds.
#[derive(Clone, Copy, Debug)]
pub struct KernelRunPolicy {
    pub execution: std::time::Duration,
    pub attempts: std::num::NonZeroU32,
    pub retry_initial_ms: u64,
    pub retry_max_ms: u64,
    /// How long a process whose run the worker pool lacked capacity for
    /// backs off before its next claim, doubled for each claim in a row
    /// that made no progress ([`lash_execution_contracts::process::CapacityVerdict`]). Such a run
    /// spends none of `attempts`. A worker that died running the run is no
    /// capacity verdict: it spends one of `attempts` (FIG-6786).
    pub capacity_backoff: std::time::Duration,
    /// How long a replay's reader lease on a retained run record lives
    /// between renewals (FIG-6370): the longest a replay keeps the pages it
    /// reads from retiring past its own end. Within
    /// (0, [`Self::MAX_REPLAY_LEASE`]]; a replay refuses any other.
    pub replay_lease: std::time::Duration,
}

impl KernelRunPolicy {
    /// The longest reader lease a replay takes.
    pub const MAX_REPLAY_LEASE: std::time::Duration = std::time::Duration::from_secs(600);

    /// 120 seconds, three attempts, immediate retries, a 50 ms capacity
    /// backoff and a 60 second replay lease, the lease a run's own reads
    /// take. A run recomputes only effect-free work from the last saved
    /// state. The backoff has no workload measurement behind it: with the
    /// standard activation-loop budget of 8, a pool that stays saturated
    /// parks the process after about 13 seconds of doubling backoff.
    pub const fn standard() -> Self {
        Self {
            execution: std::time::Duration::from_secs(120),
            attempts: std::num::NonZeroU32::MIN.saturating_add(2),
            retry_initial_ms: 0,
            retry_max_ms: 0,
            capacity_backoff: std::time::Duration::from_millis(50),
            replay_lease: std::time::Duration::from_secs(60),
        }
    }
}

impl Default for KernelRunPolicy {
    fn default() -> Self {
        Self::standard()
    }
}

impl KernelProcessEngine {
    /// The engine that runs the documents in `documents`, written in
    /// `dialect`, on `workers`, holding each new process to `bounds`. The
    /// engine links and admits against the packages `workers` bind, and
    /// shares their pool: a host selects packages on the service before it
    /// shares it.
    ///
    /// # Errors
    ///
    /// The library's own error: the build's library does not assemble.
    pub fn new(
        documents: KernelDocuments,
        dialect: &str,
        workers: lash_vm_client::service::Service,
        bounds: lash_kernel_vm::Bounds,
    ) -> Result<Self, lash_vm_releases::LibraryError> {
        let library = workers.config().worker_library()?;
        if !Arc::ptr_eq(documents.helpers(), library.helpers()) {
            return Err(lash_vm_releases::LibraryError::new(
                "the document owner must share the worker image helper handle",
            ));
        }
        let functions = Arc::new(library.registry().clone());
        let helpers = Arc::clone(library.helpers());
        Ok(Self {
            documents,
            functions,
            helpers,
            workers,
            dialect: dialect.to_owned(),
            bounds,
            writes: lash_kernel_doc::KernelVersion::NEWEST,
            policy: KernelRunPolicy::standard(),
            trace_runtime: None,
            carry_stores: None,
        })
    }

    /// This engine as the previous build's: it writes kernel version
    /// `writes`, reads nothing newer and carries nothing forward. The
    /// two-build laws run a node of each build from one binary with it.
    #[cfg(feature = "synthetic-next")]
    #[must_use]
    pub fn writing(mut self, writes: lash_kernel_doc::KernelVersion) -> Self {
        self.writes = writes;
        self
    }

    /// Sets the policy of the engine's `kernel_run` step.
    #[must_use]
    pub fn with_run_policy(mut self, policy: KernelRunPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Observes process language execution through the deployment's trace
    /// sinks.
    #[must_use]
    pub fn with_trace_runtime(mut self, runtime: lash_core::trace::TraceRuntime) -> Self {
        self.trace_runtime = Some(runtime);
        self
    }

    pub fn documents(&self) -> &KernelDocuments {
        &self.documents
    }

    pub fn worker_service(&self) -> &lash_vm_client::service::Service {
        &self.workers
    }

    /// The library functions documents are linked against.
    pub fn functions(&self) -> &Arc<lash_kernel_doc::FunctionRegistry> {
        &self.functions
    }

    pub(crate) async fn entry(
        &self,
        definition: &KernelProcessDefinition,
    ) -> Result<
        (IdentifiedDocument, Option<Signature>),
        lash_execution_contracts::process::linking::EnginePortError,
    > {
        let (link, selector) = definition.code.linked().ok_or_else(|| {
            lash_execution_contracts::process::linking::EnginePortError::Refused(
                lash_execution_contracts::process::linking::AdmissionRefusal::InvalidDocument {
                    message: "unlinked process definition".into(),
                },
            )
        })?;
        let content = self.documents.load_linked(link).await?;
        let reach = content
            .inspect(
                link,
                &documents::selection(&selector),
                self.helpers.as_ref(),
            )
            .map_err(documents::refused)?;
        let document = content
            .document(content.link(link).map_err(documents::refused)?.document)
            .map_err(documents::refused)?
            .clone();
        let signature = match &definition.code {
            InlineRequest::Callable { link, function, .. } => {
                if *link != function.callable.link {
                    return Err(documents::refused(
                        lash_kernel_package::LinkRefusal::InvalidDocument {
                            message: "callable defining Link mismatch".into(),
                        },
                    ));
                }
                let internal_signature = document
                    .functions
                    .get(&function.callable.function)
                    .map(|definition| &definition.signature)
                    .or_else(|| {
                        self.helpers
                            .admits(&document.library, &function.callable.function)
                            .then(|| self.helpers.registered(&function.callable.function))
                            .flatten()
                            .map(|registered| &registered.header().signature)
                    })
                    .ok_or_else(|| {
                        documents::refused(lash_kernel_package::LinkRefusal::InvalidDocument {
                            message: "missing callable definition".into(),
                        })
                    })?;
                lash_kernel_doc::validate_callable_signature(
                    &function.signature,
                    &function.convention,
                    internal_signature,
                )
                .map_err(|e| {
                    documents::refused(lash_kernel_package::LinkRefusal::InvalidDocument {
                        message: e.to_string(),
                    })
                })?;
                let exported = function
                    .entry_export(
                        &document,
                        self.helpers.as_ref(),
                        &lash_kernel_doc::Selection::Fresh,
                    )
                    .map_err(|error| {
                        use lash_execution_contracts::process::linking::{
                            AdmissionRefusal, EnginePortError,
                        };
                        EnginePortError::Refused(match error {
                            lash_kernel_doc::FunctionValueRefusal::EntryAnnotation { source } => {
                                AdmissionRefusal::EntryAnnotation { source }
                            }
                            lash_kernel_doc::FunctionValueRefusal::SignatureRequired { source } => {
                                AdmissionRefusal::SignatureRequired { source }
                            }
                            _ => AdmissionRefusal::CallableIdentity {
                                callable: function.callable,
                            },
                        })
                    })?;
                Some(exported.signature)
            }
            _ => reach.signature,
        };
        Ok((IdentifiedDocument::new(document), signature))
    }
    pub(crate) fn machine_requirements(&self) -> lash_vocabulary::claims::NodeRequirements {
        lash_vocabulary::claims::NodeRequirements {
            natives: self
                .functions
                .iter()
                .filter(|(_, f)| f.states_native())
                .map(|(id, _)| *id)
                .collect(),
            // A successor must claim a supported predecessor before its
            // owner can apply explicit succession to the new format.
            kernels: lash_kernel_doc::KernelVersion::ALL
                .iter()
                .filter(|kernel| **kernel <= self.writes)
                .map(|kernel| kernel.number())
                .collect(),
        }
    }
}

fn payload_definition(
    payload: &serde_json::Value,
) -> Result<KernelProcessDefinition, lash_execution_contracts::plugin_error::PluginError> {
    KernelProcessDefinition::from_payload(payload).map_err(|error| {
        lash_execution_contracts::plugin_error::PluginError::Session(format!(
            "invalid kernel process payload: {error}"
        ))
    })
}

#[async_trait::async_trait]
impl lash_execution_contracts::process::ProcessEngine for KernelProcessEngine {
    fn kind(&self) -> &'static str {
        LASH_VM_ENGINE_KIND
    }

    fn trace_class(&self) -> lash_execution_contracts::process::ProcessTraceClass {
        lash_execution_contracts::process::ProcessTraceClass::Workflow
    }

    fn state_format(&self) -> lash_execution_contracts::process::EngineStateFormat {
        advance::state_format(self.writes)
    }

    fn cancel_grace(&self) -> std::time::Duration {
        std::time::Duration::ZERO
    }

    async fn inspect_start(
        &self,
        payload: &serde_json::Value,
    ) -> Result<
        lash_execution_contracts::process::linking::LinkInspection,
        lash_execution_contracts::process::linking::EnginePortError,
    > {
        let definition = KernelProcessDefinition::from_payload(payload).map_err(|error| {
            documents::refused(lash_kernel_package::LinkRefusal::InvalidDocument {
                message: error.to_string(),
            })
        })?;
        Ok(self
            .documents
            .load_execution(&definition.code, &Default::default())
            .await?
            .inspection)
    }
    async fn inspect_link(
        &self,
        link: lash_vocabulary::package::LinkId,
        selector: &lash_execution_contracts::process::linking::ReachSelector,
    ) -> Result<
        lash_execution_contracts::process::linking::LinkInspection,
        lash_execution_contracts::process::linking::EnginePortError,
    > {
        self.documents.inspect_link(link, selector).await
    }
    fn capabilities(&self) -> lash_vocabulary::claims::NodeRequirements {
        self.machine_requirements()
    }

    /// A process runs the document its payload names and resumes under the
    /// kernel version that parked it: the generation is both.
    fn program_identity(
        &self,
        payload: &serde_json::Value,
    ) -> Option<lash_core::ExecutableGeneration> {
        KernelProcessDefinition::from_payload(payload)
            .ok()?
            .executable_generation()
    }

    /// A kernel start states its settings: they decode strictly, and name
    /// the dialect this engine's workers serve. A stored definition records
    /// no dialect, so nothing else is compared.
    fn check_settings(
        &self,
        _payload: &serde_json::Value,
        settings: Option<&lash_execution_contracts::process::EngineSettings>,
    ) -> Result<(), lash_execution_contracts::plugin_error::PluginError> {
        let Some(settings) = settings else {
            return Err(
                lash_execution_contracts::plugin_error::PluginError::Registration(
                    "a kernel process start states its settings".to_owned(),
                ),
            );
        };
        let settings = KernelRecordedSettings::from_engine_settings(settings).map_err(|error| {
            lash_execution_contracts::plugin_error::PluginError::Registration(format!(
                "the kernel settings do not decode: {error}"
            ))
        })?;
        if settings.dialect != self.dialect {
            return Err(
                lash_execution_contracts::plugin_error::PluginError::Registration(format!(
                    "the kernel settings name dialect `{}`, and this engine's workers serve `{}`",
                    settings.dialect, self.dialect
                )),
            );
        }
        Ok(())
    }

    /// This build's dialect and bounds. Only a starter asks for them.
    fn default_settings(
        &self,
        _payload: &serde_json::Value,
    ) -> Result<
        Option<lash_execution_contracts::process::EngineSettings>,
        lash_execution_contracts::plugin_error::PluginError,
    > {
        serde_json::to_value(KernelRecordedSettings::new(&self.dialect, self.bounds))
            .map(|value| {
                Some(lash_execution_contracts::process::EngineSettings::new(
                    value,
                ))
            })
            .map_err(|error| {
                lash_execution_contracts::plugin_error::PluginError::Registration(error.to_string())
            })
    }

    /// A `kernel_run`'s counts and the occurrences it recovered from,
    /// which it records at `observe: counts`.
    fn settled_ran(
        &self,
        kind: &lash_execution_contracts::process::EngineStepKind,
        output: &lash_core_store::tool_run::SettledOutput,
    ) -> lash_execution_contracts::process::SettledRan {
        match output {
            lash_core_store::tool_run::SettledOutput::Completed(material)
                if kind.0 == KERNEL_RUN_STEP =>
            {
                serde_json::from_str::<state::KernelRunOutput>(material.payload())
                    .map(|output| {
                        let observed = output.observed();
                        lash_execution_contracts::process::SettledRan {
                            ran: observed.ran.iter().copied().collect(),
                            caught: observed.caught.to_vec(),
                        }
                    })
                    .unwrap_or_default()
            }
            _ => Default::default(),
        }
    }

    fn advance(
        &self,
        state: lash_execution_contracts::process::EngineState,
        event: lash_execution_contracts::process::EngineEvent,
    ) -> Result<
        (
            lash_execution_contracts::process::EngineState,
            lash_execution_contracts::process::EngineAction,
        ),
        lash_execution_contracts::process::ProcessInfraError,
    > {
        advance::advance(self.writes, state, event)
    }

    /// A start holds its document and the declaration of every library
    /// function the document lists, in the store set's module port. The
    /// payload is a start input or a definition value.
    fn start_artifacts(
        &self,
        payload: &serde_json::Value,
    ) -> Result<Vec<lash_core::ArtifactName>, lash_execution_contracts::plugin_error::PluginError>
    {
        Ok(KernelProcessDefinition::from_payload(payload)
            .map_err(|error| {
                lash_execution_contracts::plugin_error::PluginError::Runtime(
                    lash_core::RuntimeError::new(
                        lash_core::RuntimeErrorCode::DefinitionRefused,
                        format!("invalid kernel process payload: {error}"),
                    ),
                )
            })?
            .artifacts())
    }

    /// The engine keeps no store of its own: documents live in the module
    /// port, which ends its own referrers.
    async fn end_artifact_referrer(
        &self,
        _cleanup: &lash_core::ResolvedArtifactCleanup,
    ) -> Result<(), lash_execution_contracts::module_artifacts::ArtifactStoreError> {
        Ok(())
    }

    /// Never called: [`Self::start_artifacts`] names nothing under this
    /// engine's own store.
    async fn acquire_engine_artifact(
        &self,
        _claim: &lash_core::ReferrerClaim,
        artifact_ref: &str,
    ) -> Result<(), lash_execution_contracts::plugin_error::PluginError> {
        Err(lash_execution_contracts::plugin_error::PluginError::Invoke(
            format!(
                "the kernel engine keeps no engine artifact store; `{artifact_ref}` belongs to the \
             module port"
            ),
        ))
    }

    async fn check_args(
        &self,
        signature: &lash_execution_contracts::process::ProcessSignature,
        supplied: &serde_json::Map<String, serde_json::Value>,
        mode: lash_execution_contracts::process::ArgsMode,
    ) -> Result<(), lash_execution_contracts::process::ArgsMismatch> {
        let unsupported =
            || lash_execution_contracts::process::ArgsMismatch::UnsupportedSignature {
                engine_kind: LASH_VM_ENGINE_KIND.into(),
            };
        let encoding = signature.encoding().ok_or_else(unsupported)?;
        let signature: Signature =
            serde_json::from_value(encoding.clone()).map_err(|_| unsupported())?;
        let argument = |path: &str, message: String| {
            lash_execution_contracts::process::ArgsMismatch::Argument {
                path: path.to_owned(),
                message,
            }
        };
        for name in supplied.keys() {
            if !signature
                .params
                .iter()
                .any(|param| param.name.as_str() == name)
            {
                return Err(argument(name, "the entry has no such parameter".to_owned()));
            }
        }
        for param in &signature.params {
            match supplied.get(param.name.as_str()) {
                Some(value) => {
                    crate::boundary::json_serves(&param.ty, value)
                        .map_err(|message| argument(param.name.as_str(), message))?;
                }
                None if param.optional
                    || mode != lash_execution_contracts::process::ArgsMode::Complete => {}
                None => {
                    return Err(
                        lash_execution_contracts::process::ArgsMismatch::ArgumentMissing {
                            path: param.name.to_string(),
                        },
                    );
                }
            }
        }
        Ok(())
    }

    async fn resolve(
        &self,
        reference: &lash_execution_contracts::process::ProcessDefinitionRef,
    ) -> Result<
        lash_execution_contracts::process::ProcessDefinitionResolution,
        lash_execution_contracts::process::ProcessDefinitionError,
    > {
        let unresolvable = |message: String| {
            lash_execution_contracts::process::ProcessDefinitionRefusal::UnresolvableDefinition {
                engine_kind: reference.engine_kind.clone(),
                message,
            }
        };
        let definition = KernelProcessDefinition::from_payload(reference.definition.as_json())
            .map_err(|error| unresolvable(error.to_string()))?;
        let (_, signature) = self.entry(&definition).await.map_err(|error| {
            lash_execution_contracts::process::ProcessDefinitionError::Fault(error.into())
        })?;

        Ok(
            lash_execution_contracts::process::ProcessDefinitionResolution::new(
                signature.as_ref().map_or(
                    lash_execution_contracts::process::ProcessSignature::Unknown,
                    entry_signature,
                ),
            ),
        )
    }
}

/// The engine's own step body: `kernel_run`.
#[derive(Clone)]
pub struct KernelEngineSteps {
    engine: Arc<KernelProcessEngine>,
}

impl KernelEngineSteps {
    /// The step bodies of `engine`.
    #[must_use]
    pub fn new(engine: Arc<KernelProcessEngine>) -> Self {
        Self { engine }
    }
}

#[async_trait::async_trait]
impl lash_execution_contracts::process::EngineSteps for KernelEngineSteps {
    fn kinds(&self) -> Vec<EngineStepKind> {
        vec![EngineStepKind::new(KERNEL_RUN_STEP)]
    }

    fn execution(&self, _kind: &EngineStepKind) -> std::time::Duration {
        self.engine.policy.execution
    }

    fn retry(&self, _kind: &EngineStepKind) -> lash_core::ExecutionPolicy {
        lash_core::ExecutionPolicy::repeatable(
            self.engine.policy.attempts,
            self.engine.policy.retry_initial_ms,
            self.engine.policy.retry_max_ms,
        )
    }

    async fn run(&self, run: EngineStepRun, cancel: CancellationToken) -> EngineStepAnswer {
        run::run_kernel_step(&self.engine, run, cancel).await
    }
}

/// Admits a start payload: the process pins the definition it names.
pub fn admit_kernel_process(
    _kind: &'static str,
    payload: &serde_json::Value,
) -> Result<
    lash_execution_contracts::process::ProcessIdentity,
    lash_execution_contracts::plugin_error::PluginError,
> {
    let definition = payload_definition(payload)?;
    let mut identity = lash_execution_contracts::process::ProcessIdentity::labelled(
        LASH_VM_ENGINE_KIND,
        Some(match &definition.code {
            InlineRequest::Linked { start } => format!("{:?}", start.selector),
            InlineRequest::Callable { function, .. } => function.callable.function.to_string(),
            InlineRequest::Unlinked { selector, .. } => format!("{selector:?}"),
        }),
    );
    identity.definition_id = Some(
        definition_draft(&definition)
            .map_err(|error| {
                lash_execution_contracts::plugin_error::PluginError::Session(error.to_string())
            })?
            .id(),
    );
    Ok(identity)
}

/// The registration of `engine`: its admission, its step bodies and its
/// reading of definitions as workflow documents.
#[expect(
    clippy::expect_used,
    reason = "the engine and the admission descriptor are constructed from the same LASH_VM_ENGINE_KIND constant, so registration cannot refuse"
)]
pub fn kernel_process_engine_registration(
    engine: KernelProcessEngine,
) -> lash_core::ProcessEngineRegistration {
    let engine = Arc::new(engine);
    lash_core::ProcessEngineRegistration::new(
        engine.clone(),
        lash_execution_contracts::process::ProcessEngineAdmission::new(
            LASH_VM_ENGINE_KIND,
            admit_kernel_process,
        ),
    )
    .expect("the kernel engine and its admission share a fixed kind")
    .with_document_provider(Arc::new(workflow::KernelDocumentProvider {
        engine: Arc::clone(&engine),
    }))
    .with_inline_starts(Arc::new(inline::KernelInlineStarts {
        engine: Arc::clone(&engine),
    }))
    .with_engine_steps(Arc::new(KernelEngineSteps::new(Arc::clone(&engine))))
    .with_readable_state_formats(
        lash_kernel_doc::KernelVersion::ALL
            .iter()
            .copied()
            .filter(|version| *version < engine.writes)
            .map(advance::state_format),
    )
    .with_succession(Arc::new(succession::KernelSuccession {
        engine: Arc::clone(&engine),
    }))
}
