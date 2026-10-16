//! Kernel processes that depend on a helper release a build stops
//! retaining (FIG-5799), and the plan that would carry one onto the
//! build's own helpers.
//!
//! A helper fix is a new function, and so is every helper that calls it: a
//! process written against an earlier release pins that release's
//! functions, and resumes on exactly them while the build retains the
//! release. Nothing moves it off them on its own (FIG-6074, D2): before a
//! build stops retaining the release, each process that still pins one of
//! its functions ends, or its host carries it onto a successor pair whose
//! document names the build's helpers (`Processes::succeed`, FIG-6121), and
//! the build's node refuses to start while one is left.
//!
//! [`plan_carry`] is that carry's correspondence: it replaces each library
//! function a document lists with the function the successor names under
//! the same name and rewrites the document (`lash_kernel_migrate::adopt`).
//! A parked run is never carried: its heap pages are carried by no
//! migrator this build ships, so it is refused, typed
//! ([`ParkedRefusal::StateNotCarried`]), stays parked with its rows
//! intact, and resumes on a build that retains its release (FIG-6193).
//! [`plan_helper_adoption`] plans it onto the build's own helpers, and
//! [`survey_helper_processes`] lists the dependents for `lashctl
//! kernel-migration list`, each with the typed reason its run would not
//! carry onto them.

use std::collections::{BTreeMap, BTreeSet};

use lash_core::ProcessId;
use lash_kernel_doc::{
    Document, DocumentId, FunctionCatalog, FunctionId, FunctionName, FunctionRegistry,
    KernelVersion, validate_document,
};
use lash_kernel_migrate::{ParkedRefusal, Rewritten};

use super::state::KernelProcessInput;
use super::{KernelDocuments, KernelMigrationSurveyError, advance};

/// The functions a build stops holding when it stops retaining a helper
/// release, each with the build's function of the same name and kernel
/// version, when it has one (`lash_vm_library::standard_retired_helpers`).
pub type RetiredHelpers = BTreeMap<FunctionId, Option<FunctionId>>;

/// Why a run is not carried onto other library functions: the build's own
/// helpers, or the document a successor pair names.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, thiserror::Error)]
#[serde(rename_all = "snake_case", tag = "refused")]
#[non_exhaustive]
pub enum HelperAdoptionRefusal {
    /// The document reaches `function`, and the build has no function of
    /// its name to adopt.
    #[error("helper `{name}` ({function}) has no counterpart in this build")]
    NoCounterpart {
        function: FunctionId,
        name: FunctionName,
    },
    #[error("document {document} is not retained")]
    DocumentMissing { document: DocumentId },
    /// The adopted document is not one this build runs.
    #[error("document {document}, adopted, is not admitted: {reason}")]
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
    /// The successor's document is not the process's document with each
    /// library function it lists replaced by the successor's function of
    /// the same name: no correspondence carries the run onto it.
    #[error("no correspondence carries document {document} onto {successor}")]
    NoCorrespondence {
        document: DocumentId,
        successor: DocumentId,
    },
    /// The successor's payload starts another entry, or with other
    /// arguments: it is no successor of this run.
    #[error("the successor enters `{entry}` with other arguments than the run it would carry")]
    OtherEntry { entry: lash_kernel_doc::Name },
}

/// A document adopted onto a build's helpers.
#[derive(Clone, Debug)]
pub struct AdoptedRun {
    pub from: DocumentId,
    pub rewritten: Rewritten,
}

/// The functions of `retired` that `listed`, the library functions a
/// document lists, reach through `functions`.
pub fn retired_functions_reached(
    listed: impl IntoIterator<Item = FunctionId>,
    functions: &dyn FunctionCatalog,
    retired: &RetiredHelpers,
) -> BTreeSet<FunctionId> {
    let mut reached = BTreeSet::new();
    let mut pending: Vec<FunctionId> = listed.into_iter().collect();
    while let Some(function) = pending.pop() {
        if !reached.insert(function) {
            continue;
        }
        if let Some(body) = functions
            .definition(&function)
            .and_then(|definition| definition.body())
        {
            pending.extend(body.functions.keys().copied());
        }
    }
    reached.retain(|function| retired.contains_key(function));
    reached
}

/// `document` adopted onto the counterparts of the `retired` functions it
/// reaches; `None` when it reaches none. With a run `parked` under it, the
/// adoption is refused: no migrator carries a parked run's pages.
/// `functions` is the build's library, the retired functions and their
/// counterparts among them.
///
/// # Errors
///
/// The typed refusal of a document or run that is not adopted.
pub fn plan_helper_adoption(
    document: &Document,
    parked: bool,
    functions: &FunctionRegistry,
    retired: &RetiredHelpers,
) -> Result<Option<AdoptedRun>, HelperAdoptionRefusal> {
    let state = |message: String| HelperAdoptionRefusal::State { message };
    let reached = retired_functions_reached(
        document.manifest.functions.keys().copied(),
        functions,
        retired,
    );
    if reached.is_empty() {
        return Ok(None);
    }
    let mut replacements = BTreeMap::new();
    for function in reached {
        let counterpart = retired.get(&function).copied().flatten();
        let Some(counterpart) = counterpart else {
            let name = functions
                .get(&function)
                .map(|registered| registered.definition().name.clone())
                .ok_or_else(|| state(format!("helper {function} is not held")))?;
            return Err(HelperAdoptionRefusal::NoCounterpart { function, name });
        };
        replacements.insert(function, counterpart);
    }
    plan_carry(document, parked, functions, &replacements).map(Some)
}

/// `document` carried onto `replacements`: each library function it names
/// replaced by its counterpart. `functions` holds every function the
/// document reaches and every replacement. A run `parked` under the
/// document is refused, typed: its pages stay as they are, and it resumes
/// only on a build that retains what it pins.
///
/// # Errors
///
/// The typed refusal of a document or run that is not carried.
pub fn plan_carry(
    document: &Document,
    parked: bool,
    functions: &FunctionRegistry,
    replacements: &BTreeMap<FunctionId, FunctionId>,
) -> Result<AdoptedRun, HelperAdoptionRefusal> {
    let state = |message: String| HelperAdoptionRefusal::State { message };
    let from = document.identity();
    if parked {
        return Err(HelperAdoptionRefusal::Parked {
            document: from,
            refusal: parked_not_carried(),
        });
    }
    let rewritten = lash_kernel_migrate::adopt(document, replacements, functions, &BTreeMap::new())
        .map_err(|refusal| state(refusal.to_string()))?;
    validate_document(&rewritten.document, functions).map_err(|invalid| {
        HelperAdoptionRefusal::NotAdmitted {
            document: from,
            reason: invalid.to_string(),
        }
    })?;
    Ok(AdoptedRun { from, rewritten })
}

/// Why a parked run is not carried onto another document: no migrator
/// this build ships carries its heap pages (FIG-6193).
pub(crate) fn parked_not_carried() -> ParkedRefusal {
    ParkedRefusal::StateNotCarried {
        detail: "a parked run's heap pages are carried by no migrator this build ships; \
                 it stays parked and resumes on a build that retains what it pins"
            .to_owned(),
    }
}

/// A kernel process that pins a function of a helper release a build stops
/// retaining.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct HelperDependentProcess {
    pub process: ProcessId,
    /// The retired functions its document reaches.
    pub functions: BTreeSet<FunctionId>,
    /// Why its run would not carry onto the build's helpers, were its host
    /// to name a successor pair over them; `None` when it would.
    pub refused: Option<HelperAdoptionRefusal>,
}

/// A session that pins a function of a helper release a build stops
/// retaining: in a cell its open turn stopped in, or in a function it
/// saved. It ends, or is ended by an operator, before the build starts.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct HelperDependentSession {
    pub session: lash_core::SessionId,
    /// The cells of its open turn that reach one.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub cells: Vec<String>,
    /// Whether a function it saved reaches one.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub saved: bool,
}

/// Every unfinished kernel process of `backend`, started or not, whose
/// document reaches a function of `retired`, with whether its run would
/// carry onto the build's helpers; `functions` is the build's library. It
/// reads the store and writes nothing.
///
/// # Errors
///
/// [`KernelMigrationSurveyError`]: a store did not answer.
pub async fn survey_helper_processes(
    backend: &lash_core::Backend,
    functions: &FunctionRegistry,
    retired: &RetiredHelpers,
) -> Result<Vec<HelperDependentProcess>, KernelMigrationSurveyError> {
    const PAGE: std::num::NonZeroUsize = std::num::NonZeroUsize::MIN.saturating_add(99);
    let mut dependents = Vec::new();
    if retired.is_empty() {
        return Ok(dependents);
    }
    let documents = KernelDocuments::new(backend.module_artifacts());
    let registry = backend.process_registry();
    let filter = lash_core::ProcessListFilter {
        status: lash_core::ProcessStatusFilter::any_of([
            lash_core::ProcessStatus::Running,
            lash_core::ProcessStatus::Waiting,
        ]),
        ..lash_core::ProcessListFilter::default()
    };
    let mut continuation = None;
    loop {
        let page = registry
            .list_processes_page(&filter, PAGE, continuation)
            .await?;
        for record in page.records {
            let lash_core::ProcessInput::Engine { kind, payload, .. } = record.input.as_ref()
            else {
                continue;
            };
            if kind != super::LASH_VM_ENGINE_KIND {
                continue;
            }
            let stored = lash_core::runtime::durable::stored_engine_state(
                backend.durable().as_ref(),
                &record.id,
                backend.config().settings().graph_reads.engine(),
            )
            .await?;
            let decoded = stored.as_ref().and_then(|stored| {
                advance::decode(KernelVersion::NEWEST, stored)
                    .ok()
                    .map(|(state, _)| state)
            });
            // A process not yet started is admitted on its document too.
            let input = decoded.as_ref().map_or(payload, |state| &state.payload);
            let Ok(input) = KernelProcessInput::from_payload(input) else {
                continue;
            };
            let Some(document) = documents.get(&input.document).await? else {
                continue;
            };
            let reached = retired_functions_reached(
                document.manifest.functions.keys().copied(),
                functions,
                retired,
            );
            if reached.is_empty() {
                continue;
            }
            let parked = decoded.as_ref().is_some_and(|state| state.parked.is_some());
            let refused = plan_helper_adoption(&document, parked, functions, retired).err();
            dependents.push(HelperDependentProcess {
                process: record.id,
                functions: reached,
                refused,
            });
        }
        continuation = page.continuation;
        if continuation.is_none() {
            return Ok(dependents);
        }
    }
}
