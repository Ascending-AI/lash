//! The helper releases a build retains, and what still depends on one a
//! build is about to stop retaining (FIG-5799; kernel design §6, ADR 0106
//! §2).
//!
//! A helper fix is a new function, and so is every helper that calls it.
//! Each release freezes the functions it ships (`lash-vm-worker`'s
//! `src/generated/`), and a build holds every function of the releases it
//! retains beside its own, so a run written against an earlier release
//! resumes on exactly the functions it pins. Its actors' format sets state
//! the release they were written against (`kernel-helpers`), so a node of a
//! build that holds only an earlier release never claims what a later one
//! wrote; and while such a node is live, a node of the later build writes
//! the newest release every live node holds ([`FleetHelperWrites`]).
//!
//! Before a build that stops retaining a release starts, every process and
//! session that still pins one of its functions ends, is adopted onto the
//! build's own helpers (`RlmProtocolPluginFactory::adopting_helpers`), or is
//! ended by an operator. `lashctl kernel-migration list` names each with
//! why it would not be adopted; the build's node refuses to start while one
//! is left ([`DurableError::RetiredDependents`]). Nothing expires by
//! calendar.
//!
//! [`DurableError::RetiredDependents`]: lash_core::durable_port::DurableError::RetiredDependents

use std::sync::Arc;

use lash_core::durable_port::{DurableError, FormatSet, StoreFailure, StoreFailureKind};
use lash_core::formats::BuildFormats;
use lash_kernel_doc::FunctionRegistry;
use lash_vm_runtime::{
    HelperDependentProcess, HelperDependentSession, KernelMigrationSurveyError, RetiredHelpers,
};

/// How many actors one listing page reads.
const PAGE: usize = 100;

/// The functions a build stops holding when it stops retaining helper
/// release `release`, with their counterparts.
///
/// # Errors
///
/// The embedding's, as a survey error: the shipped library does not
/// assemble.
pub(crate) fn retired_helpers(release: u32) -> Result<RetiredHelpers, KernelMigrationSurveyError> {
    lash_vm_runtime::standard_retired_helpers(release).map_err(|error| {
        KernelMigrationSurveyError::State(DurableError::Store(StoreFailure {
            kind: StoreFailureKind::Corrupt,
            message: format!("the shipped helper releases: {error}"),
        }))
    })
}

/// Every process and session of `backend` that pins a function of
/// `retired`, a helper release about to be dropped, with whether a node
/// that adopts would adopt each process; `functions` is the build's
/// library. It reads the store and writes nothing.
///
/// # Errors
///
/// [`KernelMigrationSurveyError`]: a store did not answer.
pub(crate) async fn survey_helper_dependents(
    backend: &crate::Backend,
    functions: &Arc<FunctionRegistry>,
    release: u32,
    retired: &RetiredHelpers,
) -> Result<(Vec<HelperDependentProcess>, Vec<HelperDependentSession>), KernelMigrationSurveyError>
{
    if retired.is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }
    let processes = lash_vm_runtime::survey_helper_processes(backend, functions, retired).await?;
    let mut sessions = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for set in session_sets(backend, release) {
        for actor in actors_in(backend, &set).await? {
            let Ok(session) = lash_core::SessionId::parse(actor.id()) else {
                continue;
            };
            if !seen.insert(session.clone()) {
                continue;
            }
            if let Some(dependent) = helper_session(backend, functions, retired, session).await? {
                sessions.push(dependent);
            }
        }
    }
    Ok((processes, sessions))
}

/// Every session set a session written against `release` may be in: each
/// this build decodes, and the release's own under each kernel version the
/// build interprets.
fn session_sets(backend: &crate::Backend, release: u32) -> Vec<FormatSet> {
    let mut sets = backend.formats().session_decodes();
    let kernels = [
        lash_vm_runtime::previous_kernel_version(),
        Some(crate::formats::KERNEL_PARKED_STATE_VERSION),
    ];
    for kernel in kernels.into_iter().flatten() {
        let set = BuildFormats::new(
            &[],
            &crate::formats::kernel_actor_state_surfaces(kernel, release),
        )
        .session()
        .clone();
        if !sets.contains(&set) {
            sets.push(set);
        }
    }
    sets
}

/// Every actor that has not ended whose state is in `set`.
async fn actors_in(
    backend: &crate::Backend,
    set: &FormatSet,
) -> Result<Vec<lash_core::durable_port::ActorKey>, DurableError> {
    let mut actors = Vec::new();
    loop {
        let page = backend
            .durable()
            .actors_in(set, actors.last(), PAGE)
            .await?;
        let done = page.len() < PAGE;
        actors.extend(page);
        if done {
            return Ok(actors);
        }
    }
}

/// `session`, when a cell its open turn stopped in or a function it saved
/// reaches a function of `retired`.
async fn helper_session(
    backend: &crate::Backend,
    functions: &Arc<FunctionRegistry>,
    retired: &RetiredHelpers,
    session: lash_core::SessionId,
) -> Result<Option<HelperDependentSession>, KernelMigrationSurveyError> {
    let reaches = |listed: std::collections::BTreeSet<lash_kernel_doc::FunctionId>| {
        !lash_vm_runtime::retired_functions_reached(listed, &**functions, retired).is_empty()
    };
    let reads = backend.durable();
    let mut cells = Vec::new();
    if let Some(turn) = reads.turn(&session).await? {
        for cell in reads.cell_snapshots(&session, &turn.run).await? {
            let lash_core::durable_port::domain::ExecKey::Cell(_, _, id) = &cell.exec else {
                continue;
            };
            if lash_protocol_rlm::cell_snapshot_functions(&cell.snapshot_ref).is_some_and(reaches) {
                cells.push(id.as_str().to_owned());
            }
        }
    }
    let runtime: Arc<dyn lash_core::store::RuntimeStore> = backend.session_store_factory();
    let store = lash_core::store::SessionStore::new(runtime, session.clone())?;
    let mut saved = Vec::new();
    let loaded = lash_core::store::load_session_window_state(
        &store,
        lash_core::store::WindowSelector::Current,
    )
    .await?;
    if let Some(hydrated) = loaded
        .map(|loaded| loaded.state.execution_state_hydration())
        .transpose()?
        .flatten()
        && let Ok(pins) = lash_protocol_rlm::saved_function_pins(&hydrated.root)
    {
        saved.extend(
            pins.into_iter()
                .filter(|(_, pinned)| reaches(pinned.clone()))
                .map(|(name, _)| name),
        );
    }
    Ok(
        (!cells.is_empty() || !saved.is_empty()).then_some(HelperDependentSession {
            session,
            cells,
            saved,
        }),
    )
}

/// A build's node does not start while an unfinished actor still pins a
/// function of the helper release the build no longer retains.
struct HelperRetirement {
    release: u32,
}

#[async_trait::async_trait]
impl lash_core::RetirementCheck for HelperRetirement {
    fn retired(&self) -> String {
        let name = lash_vm_runtime::RETAINED_HELPER_RELEASES
            .iter()
            .find(|(_, ordinal)| *ordinal == self.release)
            .map_or_else(|| self.release.to_string(), |(name, _)| (*name).to_owned());
        format!("helper release {name}")
    }

    fn command(&self) -> String {
        crate::kernel_migration::KERNEL_MIGRATION_SWEEP.to_owned()
    }

    async fn dependents(&self, backend: &lash_core::Backend) -> Result<u64, DurableError> {
        let surveyed = async {
            let functions = lash_vm_runtime::standard_functions().map_err(|error| {
                KernelMigrationSurveyError::State(DurableError::Store(StoreFailure {
                    kind: StoreFailureKind::Corrupt,
                    message: error.to_string(),
                }))
            })?;
            let retired = retired_helpers(self.release)?;
            survey_helper_dependents(backend, &functions, self.release, &retired).await
        };
        let (processes, sessions) = surveyed.await.map_err(|error| match error {
            KernelMigrationSurveyError::State(error) => error,
            other => DurableError::Store(StoreFailure {
                kind: StoreFailureKind::Unavailable,
                message: other.to_string(),
            }),
        })?;
        Ok(u64::try_from(processes.len() + sessions.len()).unwrap_or(u64::MAX))
    }
}

/// `backend`, no longer retaining helper release `release`: its node does
/// not start while an unfinished actor still depends on it.
pub(crate) fn retiring(backend: &crate::Backend, release: u32) -> crate::Backend {
    backend.with_retirement(Arc::new(HelperRetirement { release }))
}

/// The helper release a node of `backend`'s build writes a cell against:
/// its own, once every live node that serves the sessions of a build of an
/// earlier release it retains also decodes its sets; until then, the
/// newest such release they all hold (ADR 0106 §2).
pub(crate) struct FleetHelperWrites {
    backend: crate::Backend,
}

impl FleetHelperWrites {
    pub(crate) fn new(backend: crate::Backend) -> Self {
        Self { backend }
    }
}

#[async_trait::async_trait]
impl lash_protocol_rlm::HelperReleaseGate for FleetHelperWrites {
    async fn writable(&self) -> u32 {
        let formats = self.backend.formats();
        let own = crate::formats::helper_release_of(self.backend.surfaces());
        let decoded = formats.decoded_sessions();
        if decoded.is_empty() {
            return own;
        }
        let mut candidates = vec![(own, formats.session().clone())];
        candidates.extend(
            decoded
                .into_iter()
                .map(|(surfaces, set)| (crate::formats::helper_release_of(surfaces), set)),
        );
        // A fleet that does not answer is held to the oldest release.
        let oldest = candidates.last().map_or(own, |(release, _)| *release);
        let Ok(live) = self.backend.durable().live_decodes().await else {
            return oldest;
        };
        let sets: Vec<FormatSet> = candidates.iter().map(|(_, set)| set.clone()).collect();
        lash_core::durable_port::fleet_writable(&sets, &live)
            .and_then(|chosen| {
                candidates
                    .iter()
                    .find(|(_, set)| set == chosen)
                    .map(|(release, _)| *release)
            })
            .unwrap_or(oldest)
    }
}
