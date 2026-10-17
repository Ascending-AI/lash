//! The kernel process engine (kernel spec §8; ADR 0132 §7, §10).
//!
//! A process's code is an entry of an admitted kernel document: a declared
//! function a host may start, with a typed signature. The process names the
//! document by identity, so a running instance keeps the document it was
//! admitted under. Its body runs as a kernel machine in a worker; every
//! `perform` is one durable step, every `sleep` a durable timer, and a body
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
mod pages;
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
use lash_kernel_doc::{Document, DocumentId, Name, Signature};
use tokio_util::sync::CancellationToken;

pub(crate) use documents::ProcessPreparations;
pub use documents::{
    DocumentStoreError, ExecutionDocument, KernelDocuments, StoredSourceMap, StoredWorkflow,
    Unretained,
};
pub use helpers::{
    AdoptedRun, HelperAdoptionRefusal, HelperDependentProcess, HelperDependentSession,
    RetiredHelpers, plan_helper_adoption, retired_functions_reached, survey_helper_processes,
};
pub use migrate::{
    KernelMigrationRefusal, KernelMigrationSurvey, KernelMigrationSurveyError,
    KernelStateMigration, PlannedMigration, RefusedKernelCell, RefusedKernelProcess,
    UnmigratedKernelSession, migrate_function_value, migrate_saved_function, migration_refusal,
    plan_migration, survey_kernel_processes,
};
pub use replay::{
    ProcessReplayRequest, ReplayFault, ReplayPath, ReplaySegments, SegmentAnswer, SegmentReplay,
    TaskPath, Unreplayable,
};
pub use state::{KERNEL_RUN_STEP, KernelProcessDefinition, KernelProcessInput};
pub use workflow::{
    AdmittedWorkflow, WorkflowAdmissionOutcome, WorkflowAdmissionRefusal, WorkflowAdmissionRequest,
    WorkflowDocument, WorkflowDocumentError, WorkflowEnvironment, WorkflowEnvironmentRequest,
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
    SettingsInvalid,
    BoundaryInvalid,
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
            Self::SettingsInvalid => "process_settings_invalid",
            Self::BoundaryInvalid => "process_boundary_invalid",
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
    let mut failure = lash_core::ToolFailure::runtime(
        lash_core::ToolFailureClass::Execution,
        code.as_str(),
        message,
    );
    failure.raw = raw.map(lash_core::ToolValue::untrusted_json);
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
    pub charge: u64,
    pub memory: u64,
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
            memory: bounds.memory,
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
    let mut value = serde_json::json!({
        "document": definition.document,
        "entry": definition.entry,
    });
    if !definition.functions.is_empty() {
        value["functions"] = serde_json::json!(definition.functions);
    }
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

/// The definition an explicit publication of `function` of `document`
/// would have: a pure id and signature derivation, with no storage or registry read.
pub fn definition_of_entry(
    document: &Document,
    identity: DocumentId,
    function: &Name,
) -> Result<serde_json::Value, String> {
    let signature = document.entries.get(function).ok_or_else(|| {
        format!("`{function}` is not an entry of the document, so no process starts from it")
    })?;
    let draft = definition_draft(&KernelProcessDefinition::of(
        document,
        identity,
        function.clone(),
    ))
    .map_err(|error| error.to_string())?;
    serde_json::to_value(lash_execution_contracts::process::ProcessDefinition::new(
        draft.id(),
        entry_signature(&signature.signature),
    ))
    .map_err(|error| error.to_string())
}

/// The kernel process engine.
#[derive(Clone)]
pub struct KernelProcessEngine {
    pub(crate) documents: KernelDocuments,
    effect_preparations: Arc<std::sync::Mutex<documents::EffectPreparations>>,
    /// The library functions the engine's workers hold, by identity: what a
    /// document is linked and admitted against in the parent.
    pub(crate) functions: Arc<lash_kernel_doc::FunctionRegistry>,
    pub(crate) workers: lash_vm_client::service::Service,
    /// The dialect the host's process documents are written in: a new
    /// process records it, and its runs name it to their worker.
    pub(crate) dialect: String,
    pub(crate) bounds: lash_kernel_vm::Bounds,
    /// The kernel version this engine writes: the newest its build
    /// interprets.
    pub(crate) writes: lash_kernel_doc::KernelVersion,
    pub(crate) policy: KernelRunPolicy,
    pub(crate) random: Arc<dyn Fn() -> u64 + Send + Sync>,
    pub(crate) trace_runtime: Option<lash_core::trace::TraceRuntime>,
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
    /// spends none of `attempts`.
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

fn process_random() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    std::collections::hash_map::RandomState::new()
        .build_hasher()
        .finish()
}

impl KernelProcessEngine {
    /// The engine that runs the documents in `documents`, written in
    /// `dialect`, on `workers`, holding each new process to `bounds`, for
    /// workers that hold the library of the dialects this build was built
    /// with.
    ///
    /// # Errors
    ///
    /// The library's own error: the build's library does not assemble.
    pub fn new(
        documents: KernelDocuments,
        dialect: &str,
        workers: lash_vm_client::service::Service,
        bounds: lash_kernel_vm::Bounds,
    ) -> Result<Self, lash_vm_library::LibraryError> {
        let functions = lash_vm_library::standard_functions()?;
        Ok(Self::with_functions(
            documents, dialect, functions, workers, bounds,
        ))
    }

    /// The engine for workers a host assembled itself: `functions` are the
    /// library functions its worker entry holds, which every document is
    /// linked and admitted against.
    pub fn with_functions(
        documents: KernelDocuments,
        dialect: &str,
        functions: Arc<lash_kernel_doc::FunctionRegistry>,
        workers: lash_vm_client::service::Service,
        bounds: lash_kernel_vm::Bounds,
    ) -> Self {
        Self {
            documents,
            effect_preparations: Arc::new(std::sync::Mutex::new(
                documents::EffectPreparations::default(),
            )),
            functions,
            workers,
            dialect: dialect.to_owned(),
            bounds,
            writes: lash_kernel_doc::KernelVersion::NEWEST,
            policy: KernelRunPolicy::standard(),
            random: Arc::new(process_random),
            trace_runtime: None,
        }
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

    /// Sets the source of the random bits a body reads.
    #[must_use]
    pub fn with_random(mut self, random: Arc<dyn Fn() -> u64 + Send + Sync>) -> Self {
        self.random = random;
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

    /// The entry a definition names, read from its stored document.
    pub(crate) async fn entry(
        &self,
        definition: &KernelProcessDefinition,
    ) -> Result<Result<(lash_kernel_doc::IdentifiedDocument, Signature), String>, DocumentStoreError>
    {
        let Some(document) = self.documents.get_shared(&definition.document).await? else {
            return Ok(Err(format!(
                "document `{}` is not published",
                definition.document
            )));
        };
        if !document
            .manifest
            .functions
            .keys()
            .eq(definition.functions.iter())
        {
            return Ok(Err(format!(
                "the definition does not name the library functions document `{}` lists",
                definition.document
            )));
        }
        Ok(match document.entries.get(&definition.entry).cloned() {
            Some(entry) => Ok((document, entry.signature)),
            None => Err(format!(
                "document `{}` has no entry `{}`",
                definition.document, definition.entry
            )),
        })
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

    /// The effects the entry's code calls or spawns (P2), as the checker
    /// reads them from its published document.
    async fn performed_effects(
        &self,
        payload: &serde_json::Value,
    ) -> Result<lash_execution_contracts::process::PerformedEffects, lash_core::ProcessEnvRefusal>
    {
        let uncertified = |reason: String| lash_core::ProcessEnvRefusal::Uncertified { reason };
        let definition = KernelProcessDefinition::from_payload(payload)
            .map_err(|error| uncertified(format!("invalid kernel process payload: {error}")))?;
        let (document, _) = self
            .entry(&definition)
            .await
            .map_err(|error| uncertified(error.to_string()))?
            .map_err(uncertified)?;
        self.effect_preparations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .effects(
                definition.document,
                &document,
                &self.functions,
                &definition.entry,
            )
    }

    /// A process runs the document its payload names and resumes under the
    /// kernel version that parked it: the generation is both.
    fn program_identity(
        &self,
        payload: &serde_json::Value,
    ) -> Option<lash_core::ExecutableGeneration> {
        Some(
            KernelProcessDefinition::from_payload(payload)
                .ok()?
                .executable_generation(),
        )
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

    /// A `kernel_run`'s counts, which it records at `observe: counts`.
    fn settled_ran(
        &self,
        kind: &lash_execution_contracts::process::EngineStepKind,
        output: &lash_core_store::tool_run::SettledOutput,
    ) -> Vec<(u32, u64)> {
        match output {
            lash_core_store::tool_run::SettledOutput::Completed(material)
                if kind.0 == KERNEL_RUN_STEP =>
            {
                serde_json::from_str::<state::KernelRunOutput>(material.payload())
                    .map(|output| output.observed().ran.iter().copied().collect())
                    .unwrap_or_default()
            }
            _ => Vec::new(),
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
        let (_, signature) = self
            .entry(&definition)
            .await
            .map_err(|error| {
                lash_execution_contracts::process::ProcessDefinitionError::Fault(error.into())
            })?
            .map_err(unresolvable)?;

        Ok(
            lash_execution_contracts::process::ProcessDefinitionResolution::new(entry_signature(
                &signature,
            )),
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
        Some(definition.entry.to_string()),
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
    let registration = lash_core::ProcessEngineRegistration::new(
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
    .with_succession(Arc::new(succession::KernelSuccession {
        engine: Arc::clone(&engine),
    }));
    // A build that interprets the kernel version before its own carries
    // that version's parked processes forward when it claims them.
    match KernelStateMigration::of(engine) {
        Some(migration) => registration.with_state_migration(Arc::new(migration)),
        None => registration,
    }
}
