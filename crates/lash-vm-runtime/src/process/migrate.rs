//! General kernel-format carry preserves pinned operation fills. Package adoption is separate.
use super::state::{KernelEngineState, KernelProcessInput, Phase};
use super::{KernelDocuments, KernelProcessEngine, advance};
use lash_core::ProcessId;
use lash_execution_contracts::process::linking::{ContentRef, EnginePortError};
use lash_execution_contracts::process::{
    EngineState, EngineStateFormat, EngineStateRefusal, MigrationError,
};
use lash_kernel_doc::{Document, FunctionRegistry, KernelVersion, validate_document};

pub use lash_kernel_migrate::KernelMigrationRefusal;
use lash_kernel_migrate::{DocumentRefusal, Rewritten, migration_from};
use lash_vocabulary::legacy_names::DocumentId;
use std::sync::Arc;
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlannedMigration {
    pub from: DocumentId,
    pub to: KernelVersion,
    pub rewritten: Rewritten,
}
pub fn plan_migration(
    document: &Document,
    _functions: &FunctionRegistry,
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
    let rewritten = (migration.document)(document, &document.functions).map_err(|refusal| {
        KernelMigrationRefusal::Document {
            document: from,
            refusal,
        }
    })?;
    validate_document(&rewritten.document, &rewritten.document.functions).map_err(|error| {
        KernelMigrationRefusal::NotAdmitted {
            document: from,
            reason: error.to_string(),
        }
    })?;
    Ok(Some(PlannedMigration {
        from,
        to: migration.to,
        rewritten,
    }))
}
/// A saved callable needs its full retained origin to rewrite Link identities.
/// Re-linking alone is not a format migration and never substitutes for that proof.
pub fn migrate_saved_function(
    function: &lash_kernel_dialect::SavedFunction,
    _functions: &FunctionRegistry,
    writes: KernelVersion,
) -> Result<Option<lash_kernel_dialect::SavedFunction>, KernelMigrationRefusal> {
    if function.document.kernel() == writes.number() {
        return Ok(None);
    }
    Err(KernelMigrationRefusal::NotAdmitted {document:function.document.identity(),reason:"saved callable format carry requires an explicit successor defining Link and capture correspondence".into()})
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
#[derive(Clone)]
pub struct KernelStateMigration {
    engine: Arc<KernelProcessEngine>,
}
impl KernelStateMigration {
    pub fn of(engine: Arc<KernelProcessEngine>) -> Option<Self> {
        engine
            .writes
            .previous()
            .is_some_and(|previous| migration_from(previous).is_some_and(|m| m.to == engine.writes))
            .then_some(Self { engine })
    }
}
fn fatal(refusal: &KernelMigrationRefusal) -> MigrationError {
    MigrationError::Refused(EngineStateRefusal {
        code: "kernel_migration".into(),
        detail: serde_json::to_value(refusal).unwrap_or(serde_json::Value::Null),
        message: refusal.to_string(),
    })
}
fn retried(message: impl std::fmt::Display) -> MigrationError {
    MigrationError::Unavailable {
        message: message.to_string(),
    }
}
#[async_trait::async_trait]
impl lash_execution_contracts::process::EngineStateMigration for KernelStateMigration {
    fn carries(&self) -> Vec<EngineStateFormat> {
        self.engine
            .writes
            .previous()
            .filter(|v| migration_from(*v).is_some_and(|m| m.to == self.engine.writes))
            .map(advance::state_format)
            .into_iter()
            .collect()
    }
    async fn migrate(
        &self,
        process: &ProcessId,
        state: &EngineState,
    ) -> Result<Option<EngineState>, MigrationError> {
        let engine = &self.engine;
        if state.format == advance::state_format(engine.writes) {
            return Ok(None);
        }
        let Some(mut carried) = carry(&engine.documents, &engine.functions, engine.writes, state)
            .await
            .map_err(|error| match error {
                CarryError::Refused(r) => fatal(&r),
                CarryError::Store(e) => retried(e),
            })?
        else {
            return Ok(None);
        };
        if !matches!(carried.state.phase, Phase::Parked) {
            return Ok(None);
        }
        let claim = lash_core::ReferrerClaim::unguarded(
            lash_core::ArtifactReferrer::ProcessRecord(process.clone()),
        )
        .map_err(retried)?;
        // A new document is admitted first; the same fills remain pinned in its successor Link.
        engine
            .documents
            .admit_document(
                &claim,
                &lash_kernel_doc::DocumentBytes::from(carried.rewritten.document.clone()),
            )
            .await
            .map_err(retried)?;
        engine
            .documents
            .prepare_link(carried.link.clone())
            .await
            .map_err(retried)?;
        engine
            .documents
            .hold(&claim, &ContentRef::Link(carried.link.identity()))
            .await
            .map_err(retried)?;
        let next = lash_vocabulary::names::LinkedStart {
            link: carried.link.identity(),
            selector: match &carried.input.code {
                super::InlineRequest::Linked { start } => start.selector.clone(),
                _ => {
                    return Err(fatal(&KernelMigrationRefusal::State {
                        message: "format carry requires a linked source start".into(),
                    }));
                }
            },
        };
        let staged = engine
            .carry_pages(
                process,
                &mut carried.state,
                &carried.rewritten,
                Some(&next),
                None,
            )
            .await
            .map_err(|e| {
                fatal(&KernelMigrationRefusal::NotAdmitted {
                    document: carried.rewritten.document.identity(),
                    reason: e.to_string(),
                })
            })?;
        carried.input.code = super::InlineRequest::Linked { start: next };
        carried.state.payload = serde_json::to_value(carried.input).map_err(retried)?;
        let mut encoded = advance::encode(&carried.state, engine.writes).map_err(retried)?;
        encoded.staged = staged;
        Ok(Some(encoded))
    }
}
struct Carried {
    state: KernelEngineState,
    input: KernelProcessInput,
    rewritten: Rewritten,
    link: lash_vocabulary::package::Link,
}
enum CarryError {
    Refused(KernelMigrationRefusal),
    Store(EnginePortError),
}
impl From<KernelMigrationRefusal> for CarryError {
    fn from(r: KernelMigrationRefusal) -> Self {
        Self::Refused(r)
    }
}
async fn carry(
    documents: &KernelDocuments,
    functions: &FunctionRegistry,
    writes: KernelVersion,
    state: &EngineState,
) -> Result<Option<Carried>, CarryError> {
    let (state, written) =
        advance::decode(writes, state).map_err(|e| KernelMigrationRefusal::State {
            message: e.to_string(),
        })?;
    if written == writes {
        return Ok(None);
    }
    let input = KernelProcessInput::from_payload(&state.payload).map_err(|e| {
        KernelMigrationRefusal::State {
            message: e.to_string(),
        }
    })?;
    let super::InlineRequest::Linked { start } = &input.code else {
        return Err(KernelMigrationRefusal::State {
            message: "captured callable format carry requires an explicit capture correspondence"
                .into(),
        }
        .into());
    };
    let content = documents
        .load_linked(start.link)
        .await
        .map_err(CarryError::Store)?;
    let old = content
        .link(start.link)
        .map_err(|e| KernelMigrationRefusal::State {
            message: e.to_string(),
        })?;
    let base = content
        .document(old.document)
        .map_err(|e| KernelMigrationRefusal::State {
            message: e.to_string(),
        })?;
    if content.links.len() != 1 {
        return Err(KernelMigrationRefusal::NotAdmitted {
            document: base.identity(),
            reason:
                "multi-document format carry requires a Link-qualified coordinate correspondence"
                    .into(),
        }
        .into());
    }
    let Some(plan) = plan_migration(base, functions)? else {
        return Ok(None);
    };
    let link = lash_vocabulary::package::Link {
        document: plan.rewritten.document.identity(),
        fills: old.fills.clone(),
    };
    Ok(Some(Carried {
        state,
        input,
        rewritten: plan.rewritten,
        link,
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
) -> Result<
    Option<KernelMigrationRefusal>,
    lash_execution_contracts::process::linking::EnginePortError,
> {
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
    pub retired_functions:
        std::collections::BTreeMap<lash_vocabulary::legacy_names::FunctionId, Vec<ProcessId>>,
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
    let documents =
        KernelDocuments::new(backend.module_artifacts(), backend.stores().package_store());
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
