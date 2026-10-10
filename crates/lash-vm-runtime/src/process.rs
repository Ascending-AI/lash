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
mod migrate;
mod run;
mod state;
mod trace;
mod workflow;

#[cfg(test)]
mod tests;

use std::sync::Arc;

use lash_core::{EngineStepKind, EngineStepRun, SettledOutput};
use lash_kernel_doc::{Document, DocumentId, Name, Signature};
use tokio_util::sync::CancellationToken;

pub use documents::{DocumentStoreError, KernelDocuments};
pub use helpers::{
    AdoptedRun, HelperAdoptionRefusal, HelperDependentProcess, HelperDependentSession,
    RetiredHelpers, plan_helper_adoption, retained_earlier_helpers, retired_functions_reached,
    survey_helper_processes,
};
pub use migrate::{
    KernelMigrationRefusal, KernelMigrationSurvey, KernelMigrationSurveyError,
    KernelStateMigration, PlannedMigration, RefusedKernelCell, RefusedKernelProcess,
    SealedKernelRefusal, UnmigratedKernelSession, check_sealed_kernel, migrate_run,
    migrate_saved_function, migration_refusal, plan_migration, survey_kernel_processes,
};
pub use run::with_definitions;
pub use state::{KERNEL_RUN_STEP, KernelProcessDefinition, KernelProcessInput};
pub use workflow::{
    AdmittedWorkflow, WorkflowAdmissionOutcome, WorkflowAdmissionRefusal, WorkflowAdmissionRequest,
    WorkflowDocument, WorkflowDocumentError, WorkflowEnvironment, WorkflowEnvironmentRequest,
};

/// The engine kind every kernel process is registered under.
pub use lash_sansio::LASH_VM_ENGINE_KIND;

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
) -> lash_core::ProcessAwaitOutput {
    let mut failure = lash_core::ToolFailure::runtime(
        lash_core::ToolFailureClass::Execution,
        code.as_str(),
        message,
    );
    failure.raw = raw.map(lash_core::ToolValue::untrusted_json);
    lash_core::ProcessAwaitOutput::from_tool_output(lash_core::ToolCallOutput::failure(failure))
}

/// The bounds a process records at creation and every run of it is held
/// to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KernelRecordedSettings {
    pub charge: u64,
    pub memory: u64,
    pub call_depth: u32,
    pub live_tasks: u32,
    pub requests_per_park: u32,
    pub join_members: u32,
}

impl From<lash_kernel_vm::Bounds> for KernelRecordedSettings {
    fn from(bounds: lash_kernel_vm::Bounds) -> Self {
        Self {
            charge: bounds.charge,
            memory: bounds.memory,
            call_depth: bounds.call_depth,
            live_tasks: bounds.live_tasks,
            requests_per_park: bounds.requests_per_park,
            join_members: bounds.join_members,
        }
    }
}

/// The descriptor of the definition `entry` of `document` is: the engine,
/// the definition value and the one artifact it holds.
///
/// # Errors
///
/// The draft's refusal, which a well-formed identity never meets.
pub fn definition_draft(
    definition: &KernelProcessDefinition,
) -> Result<lash_core::ProcessDefinitionDraft, lash_core::ProcessDefinitionDraftError> {
    lash_core::ProcessDefinitionDraft::new(
        lash_core::ProcessEngineKind::new(LASH_VM_ENGINE_KIND),
        lash_core::ProcessDefinitionValue::new(serde_json::json!({
            "document": definition.document,
            "entry": definition.entry,
        })),
        vec![lash_core::ArtifactName {
            store: lash_core::ArtifactStoreId::KernelDocument,
            artifact_ref: definition.artifact_ref(),
        }],
    )
}

/// The signature a definition of an entry states: the entry's kernel
/// signature, as data.
pub fn entry_signature(signature: &Signature) -> lash_core::ProcessSignature {
    match serde_json::to_value(signature) {
        Ok(encoding) => lash_core::ProcessSignature::known(encoding),
        Err(_) => lash_core::ProcessSignature::Unknown,
    }
}

/// The definition a function reference to `function` crosses the effect
/// boundary as: the id and signature of that entry of `document`.
pub fn definition_of_entry(
    document: &Document,
    identity: DocumentId,
    function: &Name,
) -> Result<serde_json::Value, String> {
    let signature = document.entries.get(function).ok_or_else(|| {
        format!("`{function}` is not an entry of the document, so no process starts from it")
    })?;
    let draft = definition_draft(&KernelProcessDefinition {
        document: identity,
        entry: function.clone(),
    })
    .map_err(|error| error.to_string())?;
    serde_json::to_value(lash_core::ProcessDefinition::new(
        draft.id(),
        entry_signature(signature),
    ))
    .map_err(|error| error.to_string())
}

/// The kernel process engine.
#[derive(Clone)]
pub struct KernelProcessEngine {
    pub(crate) documents: KernelDocuments,
    /// The library functions the engine's workers hold, by identity: what a
    /// document is linked and admitted against in the parent.
    pub(crate) functions: Arc<lash_kernel_doc::FunctionRegistry>,
    pub(crate) workers: lash_vm_client::service::Service,
    pub(crate) bounds: lash_kernel_vm::Bounds,
    /// The kernel version this engine writes: the newest its build
    /// interprets.
    pub(crate) writes: lash_kernel_doc::KernelVersion,
    pub(crate) policy: KernelRunPolicy,
    pub(crate) random: Arc<dyn Fn() -> u64 + Send + Sync>,
    pub(crate) trace_runtime: Option<lash_core::trace::TraceRuntime>,
    /// The functions of the helper releases a node of this engine adopts
    /// processes off, with their counterparts (FIG-5799).
    pub(crate) adopts: Option<Arc<RetiredHelpers>>,
}

/// Host-selected policy for one `kernel_run`, separate from a process's
/// recorded bounds.
#[derive(Clone, Copy, Debug)]
pub struct KernelRunPolicy {
    pub execution: std::time::Duration,
    pub attempts: std::num::NonZeroU32,
    pub retry_initial_ms: u64,
    pub retry_max_ms: u64,
}

impl KernelRunPolicy {
    /// 120 seconds, three attempts, immediate retries. A run recomputes
    /// only effect-free work from the last saved state.
    pub const fn standard() -> Self {
        Self {
            execution: std::time::Duration::from_secs(120),
            attempts: std::num::NonZeroU32::MIN.saturating_add(2),
            retry_initial_ms: 0,
            retry_max_ms: 0,
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
    /// The engine that runs the documents in `documents` on `workers`,
    /// holding each new process to `bounds`, for workers assembled as lash
    /// ships them.
    ///
    /// # Errors
    ///
    /// The library's own error: the shipped library does not assemble.
    pub fn new(
        documents: KernelDocuments,
        workers: lash_vm_client::service::Service,
        bounds: lash_kernel_vm::Bounds,
    ) -> Result<Self, lash_vm_library::LibraryError> {
        let functions = lash_vm_library::standard_functions()?;
        Ok(Self::with_functions(documents, functions, workers, bounds))
    }

    /// The engine for workers a host assembled itself: `functions` are the
    /// library functions its worker entry holds, which every document is
    /// linked and admitted against.
    pub fn with_functions(
        documents: KernelDocuments,
        functions: Arc<lash_kernel_doc::FunctionRegistry>,
        workers: lash_vm_client::service::Service,
        bounds: lash_kernel_vm::Bounds,
    ) -> Self {
        Self {
            documents,
            functions,
            workers,
            bounds,
            writes: lash_kernel_doc::KernelVersion::NEWEST,
            policy: KernelRunPolicy::standard(),
            random: Arc::new(process_random),
            trace_runtime: None,
            adopts: None,
        }
    }

    /// This engine adopting each process it claims whose document reaches
    /// a function of `retired` onto that function's counterpart, the
    /// build's own helper of its name, when the run is not parked inside a
    /// helper the adoption changes (FIG-5799). An operator chooses it to
    /// carry what the build before it wrote off a helper release before a
    /// build that no longer retains that release starts. A process it
    /// refuses goes on as written, and `lashctl kernel-migration list`
    /// names it with the typed reason.
    #[must_use]
    pub fn adopting_helpers(mut self, retired: RetiredHelpers) -> Self {
        self.adopts = (!retired.is_empty()).then(|| Arc::new(retired));
        self
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
    ) -> Result<Result<(Document, Signature), String>, DocumentStoreError> {
        let Some(document) = self.documents.get(&definition.document).await? else {
            return Ok(Err(format!(
                "document `{}` is not published",
                definition.document
            )));
        };
        Ok(match document.entries.get(&definition.entry).cloned() {
            Some(signature) => Ok((document, signature)),
            None => Err(format!(
                "document `{}` has no entry `{}`",
                definition.document, definition.entry
            )),
        })
    }
}

fn payload_definition(
    payload: &serde_json::Value,
) -> Result<KernelProcessDefinition, lash_core::PluginError> {
    KernelProcessDefinition::from_payload(payload).map_err(|error| {
        lash_core::PluginError::Session(format!("invalid kernel process payload: {error}"))
    })
}

#[async_trait::async_trait]
impl lash_core::ProcessEngine for KernelProcessEngine {
    fn kind(&self) -> &'static str {
        LASH_VM_ENGINE_KIND
    }

    fn state_format(&self) -> lash_core::EngineStateFormat {
        advance::state_format(self.writes)
    }

    fn cancel_grace(&self) -> std::time::Duration {
        std::time::Duration::ZERO
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

    fn creation_config(
        &self,
        _env_spec: &lash_core::ProcessExecutionEnvSpec,
    ) -> Result<Option<serde_json::Value>, lash_core::PluginError> {
        serde_json::to_value(KernelRecordedSettings::from(self.bounds))
            .map(Some)
            .map_err(|error| lash_core::PluginError::Registration(error.to_string()))
    }

    fn advance(
        &self,
        state: lash_core::EngineState,
        event: lash_core::EngineEvent,
    ) -> Result<(lash_core::EngineState, lash_core::EngineAction), lash_core::ProcessInfraError>
    {
        advance::advance(self.writes, state, event)
    }

    /// A start names one artifact: its document, in the store set's module
    /// port. The payload is a start input or a definition value.
    fn start_artifacts(
        &self,
        payload: &serde_json::Value,
    ) -> Result<Vec<lash_core::ArtifactName>, lash_core::PluginError> {
        Ok(vec![lash_core::ArtifactName {
            store: lash_core::ArtifactStoreId::KernelDocument,
            artifact_ref: payload_definition(payload)?.artifact_ref(),
        }])
    }

    /// The engine keeps no store of its own: documents live in the module
    /// port, which ends its own referrers.
    async fn end_artifact_referrer(
        &self,
        _cleanup: &lash_core::ResolvedArtifactCleanup,
    ) -> Result<(), lash_core::ArtifactStoreError> {
        Ok(())
    }

    /// Never called: [`Self::start_artifacts`] names nothing under this
    /// engine's own store.
    async fn acquire_engine_artifact(
        &self,
        _claim: &lash_core::ReferrerClaim,
        artifact_ref: &str,
    ) -> Result<(), lash_core::PluginError> {
        Err(lash_core::PluginError::Invoke(format!(
            "the kernel engine keeps no engine artifact store; `{artifact_ref}` belongs to the \
             module port"
        )))
    }

    async fn check_args(
        &self,
        signature: &lash_core::ProcessSignature,
        supplied: &serde_json::Map<String, serde_json::Value>,
        mode: lash_core::ArgsMode,
    ) -> Result<(), lash_core::ArgsMismatch> {
        let unsupported = || lash_core::ArgsMismatch::UnsupportedSignature {
            engine_kind: LASH_VM_ENGINE_KIND.into(),
        };
        let encoding = signature.encoding().ok_or_else(unsupported)?;
        let signature: Signature =
            serde_json::from_value(encoding.clone()).map_err(|_| unsupported())?;
        let argument = |path: &str, message: String| lash_core::ArgsMismatch::Argument {
            path: path.to_owned(),
            message,
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
                None if param.optional || mode != lash_core::ArgsMode::Complete => {}
                None => {
                    return Err(argument(
                        param.name.as_str(),
                        "the entry requires this argument".to_owned(),
                    ));
                }
            }
        }
        Ok(())
    }

    async fn resolve(
        &self,
        reference: &lash_core::ProcessDefinitionRef,
    ) -> Result<lash_core::ProcessDefinitionResolution, lash_core::ProcessDefinitionRefusal> {
        let unresolvable =
            |message: String| lash_core::ProcessDefinitionRefusal::UnresolvableDefinition {
                engine_kind: reference.engine_kind.clone(),
                message,
            };
        let definition = KernelProcessDefinition::from_payload(reference.definition.as_json())
            .map_err(|error| unresolvable(error.to_string()))?;
        let (document, signature) = self
            .entry(&definition)
            .await
            .map_err(|error| unresolvable(error.to_string()))?
            .map_err(unresolvable)?;
        // Every entry of the document: a process of this definition starts
        // the others by function reference.
        let siblings = document
            .entries
            .keys()
            .filter(|entry| **entry != definition.entry)
            .map(|entry| {
                definition_draft(&KernelProcessDefinition {
                    document: definition.document,
                    entry: entry.clone(),
                })
                .map_err(|error| unresolvable(error.to_string()))
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(
            lash_core::ProcessDefinitionResolution::new(entry_signature(&signature))
                .with_siblings(siblings),
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
impl lash_core::EngineSteps for KernelEngineSteps {
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

    async fn run(&self, run: EngineStepRun, cancel: CancellationToken) -> SettledOutput {
        run::run_kernel_step(&self.engine, run, cancel).await
    }
}

/// Admits a start payload: the process pins the definition it names.
pub fn admit_kernel_process(
    _kind: &'static str,
    payload: &serde_json::Value,
    _env_spec: Option<&lash_core::ProcessExecutionEnvSpec>,
) -> Result<lash_core::ProcessIdentity, lash_core::PluginError> {
    let definition = payload_definition(payload)?;
    let mut identity = lash_core::ProcessIdentity::labelled(
        LASH_VM_ENGINE_KIND,
        Some(definition.entry.to_string()),
    );
    identity.definition_id = Some(
        definition_draft(&definition)
            .map_err(|error| lash_core::PluginError::Session(error.to_string()))?
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
        lash_core::ProcessEngineAdmission::new(LASH_VM_ENGINE_KIND, admit_kernel_process),
    )
    .expect("the kernel engine and its admission share a fixed kind")
    .with_document_provider(Arc::new(workflow::KernelDocumentProvider {
        engine: Arc::clone(&engine),
    }))
    .with_engine_steps(Arc::new(KernelEngineSteps::new(Arc::clone(&engine))));
    // A build that interprets the kernel version before its own carries
    // that version's parked processes forward when it claims them.
    match KernelStateMigration::of(engine) {
        Some(migration) => registration.with_state_migration(Arc::new(migration)),
        None => registration,
    }
}
