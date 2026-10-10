//! A kernel process carried to the next kernel version (kernel spec §6
//! "Upgrades"; ADR 0106 §1).
//!
//! A breaking kernel version ships the migration from the version before
//! it (`lash-kernel-migrate`). A node of the build that ships it decodes
//! the engine state the previous build wrote, and the first time it holds a
//! process parked in that format, with no node of the previous build left
//! to read it, it carries the process forward: the document is rewritten
//! and published under the process's own referrer, the parked run is
//! carried onto it, every wait the state names is identified in the
//! rewritten document, and the claimer commits the result as the engine
//! state in this build's format, before any transition.
//!
//! The parked run is read here as data, in the parent, as
//! `lash-kernel-state` lets any host read one: under the state's byte bound
//! and the schema's nesting bound. Nothing is compiled or run.
//!
//! [`plan_migration`] and [`migrate_run`] are that carrying without a
//! store, which `lashctl` runs over a deployment before an upgrade to list
//! what would be refused.

use std::sync::Arc;

use lash_core::{EngineState, EngineStateFormat, EngineStateRefusal, ProcessId};
use lash_kernel_doc::{Document, DocumentId, FunctionRegistry, KernelVersion, validate_document};
use lash_kernel_migrate::{DocumentRefusal, ParkedRefusal, Rewritten, migration_from};
use lash_kernel_state::ParkedRun;
use lash_vm_client::OpaqueVmState;

use super::state::{KernelEngineState, KernelProcessInput, Phase, Wait};
use super::{DocumentStoreError, KernelDocuments, KernelProcessEngine, advance};

/// Why a kernel process is not carried to the next kernel version.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, thiserror::Error)]
#[serde(rename_all = "snake_case", tag = "refused")]
#[non_exhaustive]
pub enum KernelMigrationRefusal {
    #[error("document {document} is not retained")]
    DocumentMissing { document: DocumentId },
    #[error("document {document} is not rewritten: {refusal}")]
    Document {
        document: DocumentId,
        refusal: DocumentRefusal,
    },
    /// The rewritten document is not one this build runs: a function it
    /// lists, redeclared for the next version, is not in this build's
    /// library.
    #[error("document {document}, rewritten, is not admitted: {reason}")]
    NotAdmitted {
        document: DocumentId,
        reason: String,
    },
    #[error("the run parked under document {document} is not carried: {refusal}")]
    Parked {
        document: DocumentId,
        refusal: ParkedRefusal,
    },
    #[error("the process's stored state does not decode: {message}")]
    State { message: String },
}

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
    let state = |message: String| KernelMigrationRefusal::State { message };
    let from = document.identity().map_err(|error| state(error.message))?;
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

/// A parked run whose bytes do not state the kernel version it is sealed
/// under: the seal is what a holder reads the version from, so such a run
/// would be resumed, or carried, as a version it was not parked under.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SealedKernelRefusal {
    #[error(
        "the parked run is sealed under kernel version {sealed}, and its bytes state kernel version {parked}"
    )]
    Mismatch { sealed: u32, parked: u32 },
    #[error("the parked run's bytes state no kernel version: {message}")]
    Unreadable { message: String },
}

/// Checks that `parked`'s bytes state the kernel version of its seal.
///
/// # Errors
///
/// [`SealedKernelRefusal`].
pub fn check_sealed_kernel(parked: &OpaqueVmState) -> Result<(), SealedKernelRefusal> {
    let stated =
        ParkedRun::kernel_of(parked.bytes()).map_err(|error| SealedKernelRefusal::Unreadable {
            message: error.to_string(),
        })?;
    if stated != parked.kernel() {
        return Err(SealedKernelRefusal::Mismatch {
            sealed: parked.kernel(),
            parked: stated,
        });
    }
    Ok(())
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
    let state = |message: String| KernelMigrationRefusal::State { message };
    let mut carried: Option<lash_kernel_dialect::SavedFunction> = None;
    loop {
        let current = carried.as_ref().unwrap_or(function);
        let found = current.document.manifest.kernel;
        if found == writes.number() {
            return Ok(carried);
        }
        let from = current
            .document
            .identity()
            .map_err(|error| state(error.message))?;
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
        carried = Some(
            lash_kernel_migrate::saved_function(migration, current, functions).map_err(refused)?,
        );
    }
}

/// The run `parked` under `base`, carried onto `plan`'s rewrite of it and
/// sealed under the same owner.
///
/// # Errors
///
/// The typed refusal of a run that is not carried.
pub fn migrate_run(
    parked: &OpaqueVmState,
    base: &Document,
    plan: &PlannedMigration,
) -> Result<OpaqueVmState, KernelMigrationRefusal> {
    let state = |message: String| KernelMigrationRefusal::State { message };
    let refused = |refusal| KernelMigrationRefusal::Parked {
        document: plan.from,
        refusal,
    };
    let Some(migration) = KernelVersion::of(parked.kernel()).and_then(migration_from) else {
        return Err(refused(ParkedRefusal::Version {
            found: parked.kernel(),
            from: base.manifest.kernel,
        }));
    };
    check_sealed_kernel(parked).map_err(|refusal| state(refusal.to_string()))?;
    let run = ParkedRun::from_json(parked.bytes()).map_err(|error| state(error.to_string()))?;
    let carried = (migration.parked)(&run, base, &plan.rewritten).map_err(refused)?;
    let bytes = carried
        .to_json()
        .map_err(|error| state(error.to_string()))?;
    Ok(OpaqueVmState::seal(
        parked.owner().clone(),
        plan.to.number(),
        plan.rewritten.identity().to_string(),
        bytes,
    ))
}

/// How the kernel process engine carries a process the previous build
/// parked into this build's format.
#[derive(Clone)]
pub struct KernelStateMigration {
    engine: Arc<KernelProcessEngine>,
}

impl KernelStateMigration {
    /// The migration of `engine`'s processes, or `None` when its build
    /// ships none: it interprets one kernel version.
    pub fn of(engine: Arc<KernelProcessEngine>) -> Option<Self> {
        let previous = engine.writes.previous()?;
        migration_from(previous)
            .is_some_and(|migration| migration.to == engine.writes)
            .then_some(Self { engine })
    }
}

fn fatal(refusal: &KernelMigrationRefusal) -> EngineStateRefusal {
    EngineStateRefusal {
        refusal: serde_json::to_value(refusal).unwrap_or(serde_json::Value::Null),
        message: refusal.to_string(),
        fatal: true,
    }
}

fn retried(message: impl std::fmt::Display) -> EngineStateRefusal {
    EngineStateRefusal {
        refusal: serde_json::Value::Null,
        message: message.to_string(),
        fatal: false,
    }
}

#[async_trait::async_trait]
impl lash_core::EngineStateMigration for KernelStateMigration {
    fn carries(&self) -> Vec<EngineStateFormat> {
        self.engine
            .writes
            .previous()
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
            at_park: true,
        }) = carried
        else {
            return Ok(None);
        };
        // The process holds the rewritten document as it held the one it
        // started from: under its own record, until it ends.
        let claim = lash_core::ReferrerClaim::unguarded(
            lash_core::ArtifactReferrer::ProcessRecord(process.clone()),
        )
        .map_err(retried)?;
        input.document = engine
            .documents
            .publish(&claim, &document)
            .await
            .map_err(retried)?;
        state.payload = serde_json::to_value(&input).map_err(retried)?;
        advance::encode(&state, engine.writes)
            .map(Some)
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
    /// The state, its waits identified in the rewritten document and its
    /// run, when it stands at a park, carried onto it.
    state: KernelEngineState,
    input: KernelProcessInput,
    /// The rewritten document.
    document: Document,
    /// Whether the run stands at a park and was carried; a run in flight
    /// is checked and left as written.
    at_park: bool,
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
/// written in; `None` when it is written in `writes` already or its
/// document is in the newest version.
async fn carry(
    documents: &KernelDocuments,
    functions: &FunctionRegistry,
    writes: KernelVersion,
    state: &EngineState,
) -> Result<Option<Carried>, CarryError> {
    let undecodable = |message: String| KernelMigrationRefusal::State { message };
    let (mut state, written) =
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
    let Some(plan) = plan_migration(&base, functions)? else {
        return Ok(None);
    };
    let identify = |identity: &mut lash_kernel_doc::EffectIdentity| {
        plan.rewritten
            .effect_identity(&base, identity)
            .map(|carried| *identity = carried)
            .map_err(|refusal| KernelMigrationRefusal::Parked {
                document: plan.from,
                refusal,
            })
    };
    for wait in state.waits.values_mut() {
        let (Wait::Step { identity, .. } | Wait::Timer { identity, .. }) = wait;
        identify(identity)?;
    }
    for delivery in &mut state.settled {
        identify(&mut delivery.identity)?;
    }
    let at_park = matches!(state.phase, Phase::Parked);
    if let Some(parked) = state.parked.take() {
        // A run in flight is checked, so that a survey names one this build
        // will refuse once it parks, and kept as written.
        let carried = migrate_run(&parked, &base, &plan)?;
        state.parked = Some(if at_park { carried } else { parked });
    }
    Ok(Some(Carried {
        at_park: at_park && state.parked.is_some(),
        state,
        input,
        document: plan.rewritten.document,
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

/// What a kernel migration would refuse among a deployment's processes.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct KernelMigrationSurvey {
    /// How many unfinished kernel processes were checked.
    pub checked: usize,
    /// How many of them are still in a kernel version before this build's
    /// newest: carried when a node of this build next claims them.
    pub unmigrated: usize,
    /// Each one this build would park instead of carrying, with why.
    pub refused: Vec<RefusedKernelProcess>,
    /// Each library function with no counterpart in the next version, by
    /// identity, with the processes whose documents list it.
    pub retired_functions: std::collections::BTreeMap<lash_kernel_doc::FunctionId, Vec<ProcessId>>,
}

/// Why a survey did not finish.
#[derive(Debug, thiserror::Error)]
pub enum KernelMigrationSurveyError {
    #[error(transparent)]
    Processes(#[from] lash_core::PluginError),
    #[error(transparent)]
    State(#[from] lash_core::durable_port::DurableError),
    #[error(transparent)]
    Documents(#[from] DocumentStoreError),
}

/// Checks every unfinished kernel process of `backend` against the kernel
/// migration this build ships, with `functions` as its library, and lists
/// those it would refuse. It reads the store and writes nothing: an
/// operator runs it with the next build's `lashctl` before the upgrade.
///
/// # Errors
///
/// [`KernelMigrationSurveyError`]: a store did not answer.
pub async fn survey_kernel_migration(
    backend: &lash_core::Backend,
    functions: &FunctionRegistry,
) -> Result<KernelMigrationSurvey, KernelMigrationSurveyError> {
    const PAGE: std::num::NonZeroUsize = std::num::NonZeroUsize::MIN.saturating_add(99);
    let documents = KernelDocuments::new(backend.module_artifacts());
    let registry = backend.process_registry();
    let filter = lash_core::ProcessListFilter {
        status: lash_core::ProcessStatusFilter::any_of([
            lash_core::ProcessStatus::Running,
            lash_core::ProcessStatus::Waiting,
        ]),
        ..lash_core::ProcessListFilter::default()
    };
    let mut survey = KernelMigrationSurvey::default();
    let mut continuation = None;
    loop {
        let page = registry
            .list_processes_page(&filter, PAGE, continuation)
            .await?;
        for record in page.records {
            let lash_core::ProcessInput::Engine { kind, .. } = record.input.as_ref() else {
                continue;
            };
            if kind != super::LASH_VM_ENGINE_KIND {
                continue;
            }
            let Some(state) = lash_core::runtime::durable::stored_engine_state(
                backend.durable().as_ref(),
                &record.id,
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
