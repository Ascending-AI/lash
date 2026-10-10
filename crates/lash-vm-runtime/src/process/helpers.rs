//! Kernel processes that depend on a helper release a build stops
//! retaining, and their adoption onto the build's own helpers (FIG-5799).
//!
//! A helper fix is a new function, and so is every helper that calls it: a
//! process written against an earlier release pins that release's
//! functions, and resumes on exactly them while the build retains the
//! release. Before a build stops retaining it, each process that still
//! pins one of its functions ends, is adopted, or is ended by an operator.
//! Adopting replaces each pinned function with the build's function of the
//! same name, rewrites the document, and carries the parked run onto it
//! (`lash_kernel_migrate::adopt`); a run that stands inside a replaced body
//! no correspondence carries is refused, typed, and goes on as written.
//!
//! [`survey_helper_processes`] lists the dependents for `lashctl
//! kernel-migration list`; a node of a build whose kernel engine adopts
//! ([`KernelProcessEngine::adopting_helpers`]) adopts each it claims, as
//! its first commit, when every live node serving it holds the build's
//! helpers.

use std::collections::{BTreeMap, BTreeSet};

use lash_core::{EngineState, ProcessId};
use lash_kernel_doc::{
    Document, DocumentId, FunctionCatalog, FunctionId, FunctionName, FunctionRegistry,
    KernelVersion, Site, Unit, validate_document,
};
use lash_kernel_migrate::{ParkedRefusal, Rewritten};
use lash_kernel_state::ParkedRun;
use lash_vm_client::OpaqueVmState;

use super::state::{KernelEngineState, KernelProcessInput, Phase, Wait};
use super::{DocumentStoreError, KernelDocuments, KernelMigrationSurveyError, advance};

/// The functions a build stops holding when it stops retaining a helper
/// release, each with the build's function of the same name and kernel
/// version, when it has one (`lash_vm_library::standard_retired_helpers`).
pub type RetiredHelpers = BTreeMap<FunctionId, Option<FunctionId>>;

/// The functions of every helper release before this build's own that the
/// build retains, each with its counterpart: what a node whose engine
/// adopts carries processes off.
///
/// # Errors
///
/// The library's: the shipped library does not assemble.
pub fn retained_earlier_helpers() -> Result<RetiredHelpers, lash_vm_library::LibraryError> {
    let mut retired = RetiredHelpers::new();
    for (_, release) in lash_vm_library::RETAINED_HELPER_RELEASES {
        if *release < lash_vm_library::HELPER_RELEASE {
            retired.extend(lash_vm_library::standard_retired_helpers(*release)?);
        }
    }
    Ok(retired)
}

/// Why a run is not adopted onto a build's own helpers.
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
}

/// A document adopted onto a build's helpers, and the run parked under it
/// carried onto the result.
#[derive(Clone, Debug)]
pub struct AdoptedRun {
    pub from: DocumentId,
    pub rewritten: Rewritten,
    pub parked: Option<OpaqueVmState>,
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

/// `document`, and `parked`, a run of it, adopted onto the counterparts of
/// the `retired` functions it reaches; `None` when it reaches none.
/// `functions` is the build's library, the retired functions and their
/// counterparts among them.
///
/// # Errors
///
/// The typed refusal of a document or run that is not adopted.
pub fn plan_helper_adoption(
    document: &Document,
    parked: Option<&OpaqueVmState>,
    functions: &FunctionRegistry,
    retired: &RetiredHelpers,
) -> Result<Option<AdoptedRun>, HelperAdoptionRefusal> {
    let state = |message: String| HelperAdoptionRefusal::State { message };
    let from = document.identity().map_err(|error| state(error.message))?;
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
                .map(|registered| registered.definition.name.clone())
                .ok_or_else(|| state(format!("helper {function} is not held")))?;
            return Err(HelperAdoptionRefusal::NoCounterpart { function, name });
        };
        replacements.insert(function, counterpart);
    }
    let rewritten =
        lash_kernel_migrate::adopt(document, &replacements, functions, &BTreeMap::new())
            .map_err(|refusal| state(refusal.to_string()))?;
    validate_document(&rewritten.document, functions).map_err(|invalid| {
        HelperAdoptionRefusal::NotAdmitted {
            document: from,
            reason: invalid.to_string(),
        }
    })?;
    let parked = parked
        .map(|parked| adopt_run(parked, document, from, &rewritten, &replacements))
        .transpose()?;
    Ok(Some(AdoptedRun {
        from,
        rewritten,
        parked,
    }))
}

/// `parked`, a run of `base`, carried onto `rewritten` under its own kernel
/// version and owner.
fn adopt_run(
    parked: &OpaqueVmState,
    base: &Document,
    from: DocumentId,
    rewritten: &Rewritten,
    replacements: &BTreeMap<FunctionId, FunctionId>,
) -> Result<OpaqueVmState, HelperAdoptionRefusal> {
    let state = |message: String| HelperAdoptionRefusal::State { message };
    super::check_sealed_kernel(parked).map_err(|refusal| state(refusal.to_string()))?;
    let version = KernelVersion::of(parked.kernel()).ok_or_else(|| {
        state(format!(
            "kernel version {} is not interpreted",
            parked.kernel()
        ))
    })?;
    let run = ParkedRun::from_json(parked.bytes()).map_err(|error| state(error.to_string()))?;
    let carried =
        lash_kernel_migrate::carry(&run, base, rewritten, version).map_err(|refusal| {
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
    let bytes = carried
        .to_json()
        .map_err(|error| state(error.to_string()))?;
    Ok(OpaqueVmState::seal(
        parked.owner().clone(),
        parked.kernel(),
        rewritten.identity().to_string(),
        bytes,
    ))
}

/// A stored state adopted, before its document is published.
pub(crate) struct Adopted {
    pub(crate) state: KernelEngineState,
    pub(crate) input: KernelProcessInput,
    pub(crate) document: Document,
}

pub(crate) enum AdoptError {
    Refused(HelperAdoptionRefusal),
    Store(DocumentStoreError),
}

/// `state`, written by this build's engine in `writes`, adopted onto the
/// counterparts of the `retired` functions its document reaches; `None`
/// when it reaches none or a run is in flight, which is adopted at its
/// next park.
pub(crate) async fn adopt_state(
    documents: &KernelDocuments,
    functions: &FunctionRegistry,
    retired: &RetiredHelpers,
    writes: KernelVersion,
    state: &EngineState,
) -> Result<Option<Adopted>, AdoptError> {
    let undecodable =
        |message: String| AdoptError::Refused(HelperAdoptionRefusal::State { message });
    let (mut state, written) =
        advance::decode(writes, state).map_err(|error| undecodable(error.to_string()))?;
    // A state of an earlier kernel version is carried by its migration
    // first; a run in flight is adopted at its next park.
    if written != writes || matches!(state.phase, Phase::Running { .. } | Phase::Ended) {
        return Ok(None);
    }
    let input = KernelProcessInput::from_payload(&state.payload)
        .map_err(|error| undecodable(error.to_string()))?;
    let base = documents
        .get(&input.document)
        .await
        .map_err(AdoptError::Store)?
        .ok_or(AdoptError::Refused(
            HelperAdoptionRefusal::DocumentMissing {
                document: input.document,
            },
        ))?;
    let Some(adopted) = plan_helper_adoption(&base, state.parked.as_ref(), functions, retired)
        .map_err(AdoptError::Refused)?
    else {
        return Ok(None);
    };
    let identify = |identity: &mut lash_kernel_doc::EffectIdentity| {
        adopted
            .rewritten
            .effect_identity(&base, identity)
            .map(|carried| *identity = carried)
            .map_err(|refusal| {
                AdoptError::Refused(HelperAdoptionRefusal::Parked {
                    document: adopted.from,
                    refusal,
                })
            })
    };
    for wait in state.waits.values_mut() {
        let (Wait::Step { identity, .. } | Wait::Timer { identity, .. }) = wait;
        identify(identity)?;
    }
    for delivery in &mut state.settled {
        identify(&mut delivery.identity)?;
    }
    state.parked = adopted.parked;
    Ok(Some(Adopted {
        state,
        input,
        document: adopted.rewritten.document,
    }))
}

/// A kernel process that pins a function of a helper release a build stops
/// retaining.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct HelperDependentProcess {
    pub process: ProcessId,
    /// The retired functions its document reaches.
    pub functions: BTreeSet<FunctionId>,
    /// Why a node that adopts would not adopt it; `None` when it would.
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
/// document reaches a function of `retired`, with whether it would be
/// adopted onto the build's helpers; `functions` is the build's library. It
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
            let lash_core::ProcessInput::Engine { kind, payload } = record.input.as_ref() else {
                continue;
            };
            if kind != super::LASH_VM_ENGINE_KIND {
                continue;
            }
            let stored = lash_core::runtime::durable::stored_engine_state(
                backend.durable().as_ref(),
                &record.id,
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
            let parked = decoded.as_ref().and_then(|state| state.parked.as_ref());
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
