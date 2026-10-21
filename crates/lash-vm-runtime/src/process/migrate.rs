//! A kernel process carried to the next kernel version (kernel spec §6
//! "Upgrades"; ADR 0106 §1).
//!
//! A breaking kernel version ships the migration from the version before
//! it (`lash-kernel-migrate`). A node of the build that ships it decodes
//! the engine state the previous build wrote, and the first time it holds a
//! process in that format, with no node of the previous build left to read
//! it, it carries the process forward: the document is rewritten and
//! published under the process's own referrer, every wait the state names
//! is identified in the rewritten document, and the claimer commits the
//! result as the engine state in this build's format, before any
//! transition.
//!
//! Parked records carry copy-on-write under their exact pinned snapshot.
//! The owner commits their new pages, requirements and closure roots together;
//! an unavailable definition or unmapped coordinate leaves the old run intact.
//!
//! [`plan_migration`] is that carrying without a store, which `lashctl`
//! runs over a deployment before an upgrade to list what would be
//! refused.

use std::sync::Arc;

use lash_core::ProcessId;
use lash_execution_contracts::process::EngineState;
use lash_execution_contracts::process::EngineStateFormat;
use lash_execution_contracts::process::EngineStateRefusal;
use lash_kernel_doc::{Document, DocumentId, FunctionRegistry, KernelVersion, validate_document};
use lash_kernel_migrate::{DocumentRefusal, Rewritten, migration_from};

use super::state::{KernelEngineState, KernelProcessInput, Phase};
use super::{DocumentStoreError, KernelDocuments, KernelProcessEngine, advance};

pub use lash_kernel_migrate::KernelMigrationRefusal;

/// A document's rewrite for the next kernel version, checked against the
/// library that will run it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlannedMigration {
    /// The identity of the document as written.
    pub from: DocumentId,
    pub to: KernelVersion,
    pub rewritten: Rewritten,
}

/// The rewrite of `document` for the kernel version that replaced the one
/// it is written in, or `None` when it is written in the newest.
/// `functions` is the build's library: the old version's functions and
/// each redeclared for the new one.
///
/// # Errors
///
/// The typed refusal of a document that is not carried.
pub fn plan_migration(
    document: &Document,
    functions: &FunctionRegistry,
) -> Result<Option<PlannedMigration>, KernelMigrationRefusal> {
    let from = document.identity();
    let Some(version) = KernelVersion::of(document.manifest.kernel) else {
        return Err(KernelMigrationRefusal::Document {
            document: from,
            refusal: DocumentRefusal::Version {
                found: document.manifest.kernel,
                from: KernelVersion::NEWEST.number(),
            },
        });
    };
    let Some(migration) = migration_from(version) else {
        return Ok(None);
    };
    let rewritten = (migration.document)(document, functions).map_err(|refusal| {
        KernelMigrationRefusal::Document {
            document: from,
            refusal,
        }
    })?;
    validate_document(&rewritten.document, functions).map_err(|invalid| {
        KernelMigrationRefusal::NotAdmitted {
            document: from,
            reason: invalid.to_string(),
        }
    })?;
    Ok(Some(PlannedMigration {
        from,
        to: migration.to,
        rewritten,
    }))
}

/// `function`, a saved function, carried to kernel version `writes`: its
/// code rewritten by each migration between the version it is stored in
/// and that one, or `None` when it is stored in `writes` already. A saved
/// function is self-contained, so it is carried alone; `functions` is the
/// build's library.
///
/// # Errors
///
/// The typed refusal of a function that is not carried.
pub fn migrate_saved_function(
    function: &lash_kernel_dialect::SavedFunction,
    functions: &FunctionRegistry,
    writes: KernelVersion,
) -> Result<Option<lash_kernel_dialect::SavedFunction>, KernelMigrationRefusal> {
    carry_code(
        function,
        |function| &function.document,
        |migration, function| lash_kernel_migrate::saved_function(migration, function, functions),
        writes,
    )
}

/// `value`, a function value a host holds, carried to kernel version
/// `writes` as its saved counterpart is ([`migrate_saved_function`]), or
/// `None` when it is written in `writes` already and runs on `functions`,
/// the build's library, as it is. A carried value's identity follows its
/// new document. A request that recorded the value keeps what it recorded.
///
/// # Errors
///
/// The typed refusal of a value that is not carried, and
/// [`KernelMigrationRefusal::NotAdmitted`] for one whose code calls a
/// library function the build does not hold, such as a helper of a release
/// it retired: no helper is rebound by name.
pub fn migrate_function_value(
    value: &lash_kernel_doc::FunctionValue,
    functions: &FunctionRegistry,
    writes: KernelVersion,
) -> Result<Option<lash_kernel_doc::FunctionValue>, KernelMigrationRefusal> {
    let carried = carry_code(
        value,
        |value| &value.document,
        |migration, value| lash_kernel_migrate::function_value(migration, value, functions),
        writes,
    )?;
    let checked = carried.as_ref().unwrap_or(value);
    let document = checked.document.identity();
    checked
        .check(
            functions,
            lash_kernel_doc::FunctionValueLimits::standard(),
            lash_kernel_doc::KernelDataLimits::standard(),
        )
        .map_err(|refusal| KernelMigrationRefusal::NotAdmitted {
            document,
            reason: refusal.to_string(),
        })?;
    Ok(carried)
}

/// `code` carried to kernel version `writes` by `step`, each migration
/// between the version its `document` is written in and that one, or
/// `None` when it is written in `writes` already.
fn carry_code<T>(
    code: &T,
    document: impl Fn(&T) -> &Document,
    step: impl Fn(&lash_kernel_migrate::Migration, &T) -> Result<T, DocumentRefusal>,
    writes: KernelVersion,
) -> Result<Option<T>, KernelMigrationRefusal> {
    let mut carried: Option<T> = None;
    loop {
        let current = carried.as_ref().unwrap_or(code);
        let found = document(current).manifest.kernel;
        if found == writes.number() {
            return Ok(carried);
        }
        let from = document(current).identity();
        let refused = |refusal| KernelMigrationRefusal::Document {
            document: from,
            refusal,
        };
        let migration = KernelVersion::of(found)
            .filter(|version| *version < writes)
            .and_then(migration_from)
            .ok_or_else(|| {
                refused(DocumentRefusal::Version {
                    found,
                    from: writes.number(),
                })
            })?;
        carried = Some(step(migration, current).map_err(refused)?);
    }
}

/// How the kernel process engine carries a process the previous build
/// parked into this build's format.
#[derive(Clone)]
pub struct KernelStateMigration {
    engine: Arc<KernelProcessEngine>,
}

impl KernelStateMigration {
    /// The migration of `engine`'s processes, or `None` when its build
    /// ships none: it interprets one kernel version. A process pinned to a
    /// helper release keeps that exact manifest through a format carry
    /// (FIG-6074, D2); incompatible retained helpers refuse the carry.
    pub fn of(engine: Arc<KernelProcessEngine>) -> Option<Self> {
        let carries = engine.writes.previous().is_some_and(|previous| {
            migration_from(previous).is_some_and(|migration| migration.to == engine.writes)
        });
        carries.then_some(Self { engine })
    }
}

fn fatal(refusal: &KernelMigrationRefusal) -> EngineStateRefusal {
    EngineStateRefusal {
        refusal: refusal.clone(),
        message: refusal.to_string(),
        fatal: true,
    }
}

fn retried(message: impl std::fmt::Display) -> EngineStateRefusal {
    let message = message.to_string();
    EngineStateRefusal {
        refusal: KernelMigrationRefusal::State {
            message: message.clone(),
        },
        message,
        fatal: false,
    }
}

#[async_trait::async_trait]
impl lash_execution_contracts::process::EngineStateMigration for KernelStateMigration {
    fn carries(&self) -> Vec<EngineStateFormat> {
        self.engine
            .writes
            .previous()
            .filter(|previous| {
                migration_from(*previous)
                    .is_some_and(|migration| migration.to == self.engine.writes)
            })
            .map(advance::state_format)
            .into_iter()
            .collect()
    }

    async fn migrate(
        &self,
        process: &ProcessId,
        state: &EngineState,
    ) -> Result<Option<EngineState>, EngineStateRefusal> {
        let engine = &self.engine;
        if state.format == advance::state_format(engine.writes) {
            return Ok(None);
        }
        let carried = carry(&engine.documents, &engine.functions, engine.writes, state)
            .await
            .map_err(|error| match error {
                CarryError::Refused(refusal) => fatal(&refusal),
                CarryError::Store(error) => retried(error),
            })?;
        // A run in flight settles in the format it started in; the process
        // is carried from its next park.
        let Some(Carried {
            mut state,
            mut input,
            document,
            correspondence,
            functions,
            at_park: true,
        }) = carried
        else {
            return Ok(None);
        };
        // The process holds the rewritten document as it held the one it
        // started from: under its own record, until it ends, with the
        // declarations of the functions it was redeclared over and the
        // record of the rewrite a host rekeys its maps by.
        let claim = lash_core::ReferrerClaim::unguarded(
            lash_core::ArtifactReferrer::ProcessRecord(process.clone()),
        )
        .map_err(retried)?;
        let document = lash_kernel_doc::IdentifiedDocument::new(document);
        let declarations =
            lash_kernel_doc::CheckedDeclarations::required(&document, functions.as_ref()).map_err(
                |refusal| {
                    fatal(&KernelMigrationRefusal::NotAdmitted {
                        document: correspondence.base,
                        reason: refusal.to_string(),
                    })
                },
            )?;
        let rewritten = Rewritten {
            document: document.document().clone(),
            correspondence: correspondence.clone(),
            functions: Default::default(),
        };
        let staged = engine
            .carry_pages(process, &mut state, &rewritten, None, Some(&functions))
            .await
            .map_err(|error| match error {
                lash_kernel_migrate::CarryFault::Refused(refusal) => {
                    fatal(&KernelMigrationRefusal::Parked {
                        document: correspondence.base,
                        refusal,
                    })
                }
                lash_kernel_migrate::CarryFault::Heap(error) => retried(error),
            })?;
        input.document = engine
            .documents
            .publish(&claim, &document, &declarations)
            .await
            .map_err(retried)?;
        input.functions = document.manifest.functions.keys().copied().collect();
        engine
            .documents
            .record_rewrite(&claim, &correspondence)
            .await
            .map_err(retried)?;
        state.payload = serde_json::to_value(&input).map_err(retried)?;
        advance::encode(&state, engine.writes)
            .map(|mut state| {
                state.staged = staged;
                Some(state)
            })
            .map_err(|error| {
                fatal(&KernelMigrationRefusal::State {
                    message: error.to_string(),
                })
            })
    }
}

/// A stored state carried to the next kernel version, before its document
/// is published.
struct Carried {
    /// The state, its waits identified in the rewritten document.
    state: KernelEngineState,
    input: KernelProcessInput,
    /// The rewritten document.
    document: Document,
    /// Where each node of the document it was rewritten from went.
    correspondence: lash_kernel_doc::Correspondence,
    /// Whether the process stands at a park; one in flight is carried
    /// from its next.
    at_park: bool,
    functions: Arc<FunctionRegistry>,
}

enum CarryError {
    Refused(KernelMigrationRefusal),
    Store(DocumentStoreError),
}

impl From<KernelMigrationRefusal> for CarryError {
    fn from(refusal: KernelMigrationRefusal) -> Self {
        Self::Refused(refusal)
    }
}

/// `state` carried to the kernel version that replaced the one it is
/// written in; `None` when it is written in `writes` already, its
/// document is in the newest version, or it stands on a run this build
/// resumes in place. A process with an uncarryable coordinate or unavailable pinned code is refused.
async fn carry(
    documents: &KernelDocuments,
    functions: &FunctionRegistry,
    writes: KernelVersion,
    state: &EngineState,
) -> Result<Option<Carried>, CarryError> {
    let undecodable = |message: String| KernelMigrationRefusal::State { message };
    let (state, written) =
        advance::decode(writes, state).map_err(|error| undecodable(error.to_string()))?;
    if written == writes {
        return Ok(None);
    }
    let input = KernelProcessInput::from_payload(&state.payload)
        .map_err(|error| undecodable(error.to_string()))?;
    let base = documents
        .get(&input.document)
        .await
        .map_err(CarryError::Store)?
        .ok_or(KernelMigrationRefusal::DocumentMissing {
            document: input.document,
        })?;
    let stored = documents
        .stored_definitions(&base, functions)
        .await
        .map_err(CarryError::Store)?
        .map_err(|error| KernelMigrationRefusal::Parked {
            document: base.identity(),
            refusal: lash_kernel_migrate::ParkedRefusal::StateNotCarried {
                detail: format!("pinned definitions: {error:?}"),
            },
        })?;
    let mut declared = lash_vm_broker::kernel::loader::declare(
        base.manifest.functions.keys().copied(),
        base.manifest.kernel,
        functions,
        &stored,
    )
    .map_err(|error| KernelMigrationRefusal::NotAdmitted {
        document: base.identity(),
        reason: error.to_string(),
    })?;
    if let Some(migration) = KernelVersion::of(base.manifest.kernel).and_then(migration_from) {
        lash_kernel_migrate::migrate_registry(&mut declared, migration).map_err(|error| {
            KernelMigrationRefusal::NotAdmitted {
                document: base.identity(),
                reason: error.to_string(),
            }
        })?;
    }
    let Some(plan) = plan_migration(&base, &declared)? else {
        return Ok(None);
    };
    let at_park = matches!(state.phase, Phase::Parked);
    // A state that stands on no run holds no wait: every wait is a park's.
    Ok(Some(Carried {
        at_park,
        functions: Arc::new(declared),
        state,
        input,
        document: plan.rewritten.document,
        correspondence: plan.rewritten.correspondence,
    }))
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
    functions: &FunctionRegistry,
    state: &EngineState,
) -> Result<Option<KernelMigrationRefusal>, DocumentStoreError> {
    match carry(documents, functions, KernelVersion::NEWEST, state).await {
        Ok(_) => Ok(None),
        Err(CarryError::Refused(refusal)) => Ok(Some(refusal)),
        Err(CarryError::Store(error)) => Err(error),
    }
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
    pub turn: lash_core::TurnId,
    /// The cell.
    pub cell: String,
    pub refusal: KernelMigrationRefusal,
}

/// A session still holding state in the kernel version before this build's
/// newest: its bindings and saved functions, and the cell its open turn
/// stopped in, if any.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct UnmigratedKernelSession {
    pub session: lash_core::SessionId,
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
    /// this build's newest: each is carried when a node of this build next
    /// claims it, or when `lashctl kernel-migration run` wakes it.
    pub unmigrated: usize,
    /// Each process this build would park instead of carrying, with why.
    pub refused: Vec<RefusedKernelProcess>,
    /// Each session still in the earlier kernel version, with the cells of
    /// its open turn this build would not resume.
    pub sessions: Vec<UnmigratedKernelSession>,
    /// Each library function with no counterpart in the next version, by
    /// identity, with the processes whose documents list it.
    pub retired_functions: std::collections::BTreeMap<lash_kernel_doc::FunctionId, Vec<ProcessId>>,
}

/// Why a survey did not finish.
#[derive(Debug, thiserror::Error)]
pub enum KernelMigrationSurveyError {
    #[error(transparent)]
    Processes(#[from] lash_execution_contracts::plugin_error::PluginError),
    #[error(transparent)]
    State(#[from] lash_core::durable_port::DurableError),
    #[error(transparent)]
    Documents(#[from] DocumentStoreError),
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
    let documents = KernelDocuments::new(backend.module_artifacts());
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
