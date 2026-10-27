//! General kernel-format carry preserves pinned operation fills. Package adoption is separate.
use super::state::KernelProcessInput;
use super::{KernelDocuments, advance};
use lash_core::ProcessId;
use lash_execution_contracts::process::EngineState;
use lash_execution_contracts::process::linking::EnginePortError;
use lash_kernel_doc::{Document, FunctionRegistry, KernelVersion, validate_document};

pub use lash_kernel_migrate::KernelMigrationRefusal;
use lash_kernel_migrate::{DocumentRefusal, Rewritten, migration_from};
use lash_kernel_value::names::DocumentId;
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlannedMigration {
    pub from: DocumentId,
    pub to: KernelVersion,
    pub rewritten: Rewritten,
}
pub fn plan_migration(
    document: &Document,
    functions: &FunctionRegistry,
) -> Result<Option<PlannedMigration>, KernelMigrationRefusal> {
    let from = document.identity();
    let found = document.kernel();
    let version = KernelVersion::of(found).ok_or(KernelMigrationRefusal::Document {
        document: from,
        refusal: DocumentRefusal::Version {
            found,
            from: KernelVersion::NEWEST.number(),
        },
    })?;
    let Some(migration) = migration_from(version) else {
        return Ok(None);
    };
    struct MigrationCatalog<'a>(&'a Document, &'a FunctionRegistry);
    impl lash_kernel_doc::FunctionCatalog for MigrationCatalog<'_> {
        fn definition(
            &self,
            id: &lash_kernel_value::FunctionId,
        ) -> Option<&lash_kernel_doc::FunctionDefinition> {
            self.0
                .functions
                .get(id)
                .or_else(|| lash_kernel_doc::FunctionCatalog::definition(self.1, id))
        }
    }
    let catalog = MigrationCatalog(document, functions);
    let rewritten = (migration.document)(document, &catalog).map_err(|refusal| {
        KernelMigrationRefusal::Document {
            document: from,
            refusal,
        }
    })?;
    validate_document(&rewritten.document, functions).map_err(|error| {
        KernelMigrationRefusal::NotAdmitted {
            document: from,
            refusal: error,
        }
    })?;
    Ok(Some(PlannedMigration {
        from,
        to: migration.to,
        rewritten,
    }))
}
/// An escaped callable is pinned to its defining Link. Its consumer supplies a validated successor explicitly.
pub fn migrate_function_value(
    value: &lash_kernel_doc::FunctionValue,
    _functions: &FunctionRegistry,
    _writes: KernelVersion,
) -> Result<Option<lash_kernel_doc::FunctionValue>, KernelMigrationRefusal> {
    // Transport holds identities and immutable captures, without an embedded program to rewrite.
    if _writes.previous().is_none() {
        return Ok(None);
    }
    Err(KernelMigrationRefusal::CallableOriginRequired {
        callable: value.callable,
    })
}
/// Why the process holding `state` would not be carried to the next kernel
/// version by a build whose library is `functions`, or `None` when it would
/// be, or needs no carrying. What `lashctl kernel-migration list` asks of
/// every kernel process before an upgrade; it writes nothing.
///
/// # Errors
///
/// The document store's.
pub async fn migration_refusal(
    documents: &KernelDocuments,
    _functions: &FunctionRegistry,
    state: &EngineState,
) -> Result<
    Option<KernelMigrationRefusal>,
    lash_execution_contracts::process::linking::EnginePortError,
> {
    let (decoded, written) = advance::decode(KernelVersion::NEWEST, state).map_err(|error| {
        EnginePortError::Unavailable {
            message: error.to_string(),
        }
    })?;
    if written == KernelVersion::NEWEST {
        return Ok(None);
    }
    let input = KernelProcessInput::from_payload(&decoded.payload).map_err(|error| {
        EnginePortError::Unavailable {
            message: error.to_string(),
        }
    })?;
    let Some((link, _)) = input.code.linked() else {
        return Ok(Some(KernelMigrationRefusal::State {
            message: "unrealized process input".into(),
        }));
    };
    let content = documents.load_linked(link).await?;
    Ok(Some(KernelMigrationRefusal::ExplicitSuccessorRequired {
        document: content.links[&link].document,
    }))
}

/// A kernel process the next kernel version's build would not carry.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct RefusedKernelProcess {
    pub process: ProcessId,
    pub refusal: KernelMigrationRefusal,
}

/// The cell a session's open turn stopped in, which the next kernel
/// version's build would not carry.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct RefusedKernelCell {
    /// The turn it stopped.
    pub turn: lash_vocabulary::identity::TurnId,
    /// The cell.
    pub cell: String,
    pub refusal: KernelMigrationRefusal,
}

/// A session still holding state in the kernel version before this build's
/// newest: its bindings and saved functions, and the cell its open turn
/// stopped in, if any.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct UnmigratedKernelSession {
    pub session: lash_kernel_value::identity::SessionId,
    /// Each cell of its open turn this build would not resume, with why.
    pub refused: Vec<RefusedKernelCell>,
}

/// What a kernel migration would refuse among a deployment's processes and
/// sessions.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct KernelMigrationSurvey {
    /// How many unfinished kernel processes were checked.
    pub checked: usize,
    /// How many processes and sessions are still in a kernel version before
    /// this build's newest: hosts supply explicit successors to carry them.
    /// Waking a process alone continues its supported format as written.
    pub unmigrated: usize,
    /// Each process this build would park instead of carrying, with why.
    pub refused: Vec<RefusedKernelProcess>,
    /// Each session still in the earlier kernel version, with the cells of
    /// its open turn this build would not resume.
    pub sessions: Vec<UnmigratedKernelSession>,
    /// Each library function with no counterpart in the next version, by
    /// identity, with the processes whose documents list it.
    pub retired_functions:
        std::collections::BTreeMap<lash_kernel_value::names::FunctionId, Vec<ProcessId>>,
}

/// Why a survey did not finish.
#[derive(Debug, thiserror::Error)]
pub enum KernelMigrationSurveyError {
    #[error(transparent)]
    Processes(#[from] lash_execution_contracts::plugin_error::PluginError),
    #[error(transparent)]
    State(#[from] lash_core::durable_port::DurableError),
    #[error(transparent)]
    Documents(#[from] lash_execution_contracts::process::linking::EnginePortError),
    /// A session's stored state did not load.
    #[error(transparent)]
    Sessions(#[from] lash_core::StoreError),
}

/// Checks every unfinished kernel process of `backend` against the kernel
/// migration this build ships, with `functions` as its library, and lists
/// those it would refuse. It reads the store and writes nothing: the
/// process half of the survey `lashctl kernel-migration list` prints, whose
/// session half needs the code executor's cell checkpoints (`lash::vm`).
///
/// # Errors
///
/// [`KernelMigrationSurveyError`]: a store did not answer.
pub async fn survey_kernel_processes(
    backend: &lash_execution_contracts::backend::Backend,
    functions: &FunctionRegistry,
) -> Result<KernelMigrationSurvey, KernelMigrationSurveyError> {
    const PAGE: std::num::NonZeroUsize = std::num::NonZeroUsize::MIN.saturating_add(99);
    let documents = KernelDocuments::new(
        backend.module_artifacts(),
        backend.stores().package_store(),
        lash_kernel_doc::BuildHelpers::empty(),
    );
    let registry = backend.process_registry();
    let filter = lash_execution_contracts::process::ProcessListFilter {
        status: lash_execution_contracts::process::ProcessStatusFilter::any_of([
            lash_execution_contracts::process::ProcessStatus::Running,
            lash_execution_contracts::process::ProcessStatus::Waiting,
        ]),
        ..lash_execution_contracts::process::ProcessListFilter::default()
    };
    let mut survey = KernelMigrationSurvey::default();
    let mut continuation = None;
    loop {
        let page = registry
            .list_processes_page(&filter, PAGE, continuation)
            .await?;
        for record in page.records {
            let lash_execution_contracts::process::ProcessInput::Engine { kind, .. } =
                record.input.as_ref()
            else {
                continue;
            };
            if kind != super::LASH_VM_ENGINE_KIND {
                continue;
            }
            let Some(state) = lash_core::runtime::durable::stored_engine_state(
                backend.durable().as_ref(),
                &record.id,
                backend.config().settings().graph_reads.engine(),
            )
            .await?
            else {
                continue;
            };
            survey.checked += 1;
            if state.format != advance::state_format(KernelVersion::NEWEST) {
                survey.unmigrated += 1;
            }
            let Some(refusal) = migration_refusal(&documents, functions, &state).await? else {
                continue;
            };
            if let KernelMigrationRefusal::Document {
                refusal: DocumentRefusal::FunctionRetired { function, .. },
                ..
            } = &refusal
            {
                survey
                    .retired_functions
                    .entry(*function)
                    .or_default()
                    .push(record.id.clone());
            }
            survey.refused.push(RefusedKernelProcess {
                process: record.id,
                refusal,
            });
        }
        continuation = page.continuation;
        if continuation.is_none() {
            return Ok(survey);
        }
    }
}
