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
//! the same name, rewrites the document (`lash_kernel_migrate::adopt`), and
//! carries the parked run onto it; a run that stands inside a replaced body
//! no correspondence carries is refused, typed. [`plan_helper_adoption`]
//! plans it onto the build's own helpers, and [`survey_helper_processes`]
//! lists the dependents for `lashctl kernel-migration list`, each with the
//! typed reason its run would not carry onto them.

use std::collections::{BTreeMap, BTreeSet};

use lash_core::ProcessId;
use lash_kernel_doc::graph_tokens::ChildRef;
use lash_kernel_doc::{
    Document, DocumentId, FunctionCatalog, FunctionId, FunctionName, FunctionRegistry,
    KernelVersion, Site, Unit, validate_document,
};
use lash_kernel_migrate::{ParkedRefusal, Rewritten};
use lash_kernel_state::AdmittedWait;
use lash_vm_broker::kernel::state::{OpenedRun, WholeState, capture_whole};

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
    /// The run stands inside the body of `function`, a helper the adoption
    /// replaces with a changed one, and no declared correspondence says
    /// where `site` is in its replacement.
    #[error("the run stands at {site}, inside helper {function}, which the adoption changes")]
    ParkedInsideChanged { function: FunctionId, site: Site },
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

/// A document adopted onto a build's helpers, and the run parked under it
/// carried onto the result.
#[derive(Clone, Debug)]
pub struct AdoptedRun {
    pub from: DocumentId,
    pub rewritten: Rewritten,
    pub parked: Option<WholeState>,
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

/// `document`, and `parked`, a run of it with the admission of each wait it
/// holds, adopted onto the counterparts of the `retired` functions it
/// reaches and pinning the build's helper release; `None` when it reaches
/// none.
/// `functions` is the build's library, the retired functions and their
/// counterparts among them.
///
/// # Errors
///
/// The typed refusal of a document or run that is not adopted.
pub fn plan_helper_adoption(
    document: &Document,
    parked: Option<(&WholeState, &[AdmittedWait])>,
    functions: &FunctionRegistry,
    retired: &RetiredHelpers,
    limits: lash_core_store::checkpoint_graph::DecodeLimits,
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
    plan_carry(document, parked, functions, &replacements, limits).map(Some)
}

/// `document`, and `parked`, a run of it with the admission of each wait it
/// holds, carried onto `replacements`: each library function it names
/// replaced by its counterpart, and the run pinning the build's helper
/// release. `functions` holds every function the document reaches and
/// every replacement.
///
/// # Errors
///
/// The typed refusal of a document or run that is not carried.
pub fn plan_carry(
    document: &Document,
    parked: Option<(&WholeState, &[AdmittedWait])>,
    functions: &FunctionRegistry,
    replacements: &BTreeMap<FunctionId, FunctionId>,
    limits: lash_core_store::checkpoint_graph::DecodeLimits,
) -> Result<AdoptedRun, HelperAdoptionRefusal> {
    let state = |message: String| HelperAdoptionRefusal::State { message };
    let from = document.identity().map_err(|error| state(error.message))?;
    let rewritten = lash_kernel_migrate::adopt(document, replacements, functions, &BTreeMap::new())
        .map_err(|refusal| state(refusal.to_string()))?;
    validate_document(&rewritten.document, functions).map_err(|invalid| {
        HelperAdoptionRefusal::NotAdmitted {
            document: from,
            reason: invalid.to_string(),
        }
    })?;
    let parked = parked
        .map(|(parked, admitted)| {
            adopt_run(
                parked,
                admitted,
                document,
                from,
                &rewritten,
                replacements,
                limits,
            )
        })
        .transpose()?;
    Ok(AdoptedRun {
        from,
        rewritten,
        parked,
    })
}

/// The release an explicit successor pair selects within the pinned
/// dialect's line. A kernel-format carry never calls this selector.
fn current_release(pinned: &ChildRef) -> Result<ChildRef, String> {
    let releases = lash_vm_broker::kernel::state::release_manifests()
        .map_err(|failure| failure.to_string())?;
    releases
        .manifests
        .dialect_of(pinned)
        .and_then(|dialect| releases.manifests.current(dialect))
        .ok_or_else(|| "the run pins a helper release this build does not hold".to_owned())
}

/// `parked`, a run of `base`, carried onto `rewritten` under its own kernel
/// version and owner, pinning the build's helper release.
fn adopt_run(
    parked: &WholeState,
    admitted: &[AdmittedWait],
    base: &Document,
    from: DocumentId,
    rewritten: &Rewritten,
    replacements: &BTreeMap<FunctionId, FunctionId>,
    limits: lash_core_store::checkpoint_graph::DecodeLimits,
) -> Result<WholeState, HelperAdoptionRefusal> {
    let state = |message: String| HelperAdoptionRefusal::State { message };
    let version = KernelVersion::of(parked.kernel).ok_or_else(|| {
        state(format!(
            "kernel version {} is not interpreted",
            parked.kernel
        ))
    })?;
    let opened = super::migrate::open_run(parked, admitted, base, limits).map_err(state)?;
    let carried =
        lash_kernel_migrate::carry(&opened.run, base, rewritten, version).map_err(|refusal| {
            let inside = match &refusal {
                ParkedRefusal::SiteNotCarried { site, .. } => match &site.unit {
                    Unit::Library(function) if replacements.contains_key(function) => {
                        Some((*function, site.clone()))
                    }
                    _ => None,
                },
                _ => None,
            };
            match inside {
                Some((function, site)) => {
                    HelperAdoptionRefusal::ParkedInsideChanged { function, site }
                }
                None => HelperAdoptionRefusal::Parked {
                    document: from,
                    refusal,
                },
            }
        })?;
    capture_whole(
        &OpenedRun {
            run: carried,
            release: current_release(&opened.release).map_err(state)?,
            binding: opened.binding,
        },
        u64::MAX,
    )
    .map_err(|failure| state(failure.to_string()))
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
    /// The functions it saved that reach one, by the binding each is
    /// called through.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub saved: Vec<String>,
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
            let parked = decoded.as_ref().and_then(|state| {
                let admitted: Vec<AdmittedWait> = state
                    .admissions
                    .values()
                    .map(|admission| admission.0.clone())
                    .collect();
                Some((super::migrate::unsealed(state.parked.as_ref()?), admitted))
            });
            let refused = match &parked {
                Some((Err(message), _)) => Some(HelperAdoptionRefusal::State {
                    message: message.clone(),
                }),
                Some((Ok(whole), admitted)) => plan_helper_adoption(
                    &document,
                    Some((whole, admitted)),
                    functions,
                    retired,
                    backend.config().settings().graph_reads.execution(),
                )
                .err(),
                None => plan_helper_adoption(
                    &document,
                    None,
                    functions,
                    retired,
                    backend.config().settings().graph_reads.execution(),
                )
                .err(),
            };
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
