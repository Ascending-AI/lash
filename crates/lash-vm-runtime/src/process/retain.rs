//! What a node keeps of the package artifacts it holds beside its own set
//! (FIG-6918, RETAIN-SURVEY).
//!
//! Code is pinned by function identity only. An artifact outside the
//! node's own set is retained while any parked root, unfinished process
//! (started or not), saved function or published definition lists one of
//! its identities that no artifact of the own set declares. The unit of
//! retention is the artifact: a set is a gate candidate only while a node
//! holds every member, so an artifact the survey drops takes every set
//! naming it out of the candidates, with no set row edited. A dependent
//! listing an identity no held artifact declares would be stranded on this
//! node, so the node does not start while one remains.

use std::collections::{BTreeMap, BTreeSet};

use lash_core::ProcessId;
use lash_kernel_doc::{DocumentId, FunctionCatalog, FunctionId, KernelVersion};
use lash_kernel_package::{ArtifactKey, HeldPackages};

use super::state::KernelProcessInput;
use super::{KernelDocuments, KernelMigrationSurveyError, advance};

/// One thing that pins code by identity.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "snake_case", tag = "dependent")]
pub enum RetainDependent {
    /// An unfinished kernel process; `started` when it has run a step.
    Process { process: ProcessId, started: bool },
    /// A cell of a session's open turn, parked in its root.
    ParkedCell {
        session: lash_core::SessionId,
        cell: String,
    },
    /// A function a session saved.
    SavedFunction {
        session: lash_core::SessionId,
        name: String,
    },
    /// A published process definition's document.
    PublishedDefinition { document: DocumentId },
}

/// One identity a dependent lists that no artifact the node holds declares.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct StrandedDependent {
    pub dependent: RetainDependent,
    pub function: FunctionId,
}

/// What the node keeps beside its own set, and what it would strand.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct RetainSurvey {
    /// Each retained artifact, with the dependents that list one of the
    /// identities it alone declares.
    pub retained: BTreeMap<ArtifactKey, Vec<RetainDependent>>,
    /// Each dependent listing an identity no held artifact declares.
    pub stranded: Vec<StrandedDependent>,
}

impl RetainSurvey {
    /// What `packages` keeps for `dependents`, each with the identities it
    /// lists: every function its listed ones reach through their bodies is
    /// pinned too.
    #[must_use]
    pub fn of(
        packages: &HeldPackages,
        dependents: impl IntoIterator<Item = (RetainDependent, BTreeSet<FunctionId>)>,
    ) -> Self {
        let own = packages.sets().get(&packages.own());
        let mut declared: BTreeMap<FunctionId, Vec<&ArtifactKey>> = BTreeMap::new();
        let mut own_functions = BTreeSet::new();
        for (key, package) in packages.artifacts() {
            let in_own = own.is_some_and(|set| set.holds(key));
            for function in package.code().ids() {
                if in_own {
                    own_functions.insert(*function);
                } else {
                    declared.entry(*function).or_default().push(key);
                }
            }
        }
        let mut survey = Self::default();
        for (dependent, listed) in dependents {
            for function in reached(listed, packages.functions().as_ref()) {
                if own_functions.contains(&function) {
                    continue;
                }
                match declared.get(&function) {
                    Some(keys) => {
                        for key in keys {
                            let dependents = survey.retained.entry((*key).clone()).or_default();
                            if !dependents.contains(&dependent) {
                                dependents.push(dependent.clone());
                            }
                        }
                    }
                    None => survey.stranded.push(StrandedDependent {
                        dependent: dependent.clone(),
                        function,
                    }),
                }
            }
        }
        survey
    }

    /// The artifacts a node holding `packages` advertises: its own set's,
    /// and each retained one.
    #[must_use]
    pub fn held(&self, packages: &HeldPackages) -> BTreeSet<ArtifactKey> {
        let mut held: BTreeSet<ArtifactKey> = packages
            .sets()
            .get(&packages.own())
            .map(|set| set.keys().collect())
            .unwrap_or_default();
        held.extend(self.retained.keys().cloned());
        held
    }
}

/// `listed` and every function their bodies reach through `functions`.
fn reached(
    listed: impl IntoIterator<Item = FunctionId>,
    functions: &dyn FunctionCatalog,
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
    reached
}

/// Every unfinished kernel process of `backend`, started or not, with the
/// identities its document lists: a parked one's run is pinned by the same
/// document. It reads the store and writes nothing.
///
/// # Errors
///
/// [`KernelMigrationSurveyError`]: a store did not answer.
pub async fn retained_processes(
    backend: &lash_execution_contracts::backend::Backend,
) -> Result<Vec<(RetainDependent, BTreeSet<FunctionId>)>, KernelMigrationSurveyError> {
    const PAGE: std::num::NonZeroUsize = std::num::NonZeroUsize::MIN.saturating_add(99);
    let mut dependents = Vec::new();
    let documents = KernelDocuments::new(backend.module_artifacts());
    let registry = backend.process_registry();
    let filter = lash_execution_contracts::process::ProcessListFilter {
        status: lash_execution_contracts::process::ProcessStatusFilter::any_of([
            lash_execution_contracts::process::ProcessStatus::Running,
            lash_execution_contracts::process::ProcessStatus::Waiting,
        ]),
        ..lash_execution_contracts::process::ProcessListFilter::default()
    };
    let mut continuation = None;
    loop {
        let page = registry
            .list_processes_page(&filter, PAGE, continuation)
            .await?;
        for record in page.records {
            let lash_execution_contracts::process::ProcessInput::Engine { kind, payload, .. } =
                record.input.as_ref()
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
            // A process not yet started is pinned by its document too.
            let input = decoded.as_ref().map_or(payload, |state| &state.payload);
            let Ok(input) = KernelProcessInput::from_payload(input) else {
                continue;
            };
            let Some(document) = documents.get(&input.document).await? else {
                continue;
            };
            dependents.push((
                RetainDependent::Process {
                    process: record.id,
                    started: decoded.is_some(),
                },
                document.manifest.functions.keys().copied().collect(),
            ));
        }
        continuation = page.continuation;
        if continuation.is_none() {
            return Ok(dependents);
        }
    }
}
