//! The helper releases a build retains, and what still depends on one a
//! build is about to stop retaining (FIG-5799; kernel design §6, ADR 0106
//! §2).
//!
//! A helper fix is a new function, and so is every helper that calls it.
//! Each release holds the functions it ships (`lash-vm-releases`: sealed at
//! the cut that ships it, and defined by the build until then), and a build
//! holds every function of the releases it
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
    releases: Vec<lash_vm_runtime::HelperReleaseIndex>,
    previous: Option<Arc<dyn lash_core::RetirementCheck>>,
}

#[async_trait::async_trait]
impl lash_core::RetirementCheck for HelperRetirement {
    fn retired(&self) -> String {
        let mut names = self
            .releases
            .iter()
            .map(|release| format!("helper release {}", release.release))
            .collect::<Vec<_>>();
        if let Some(previous) = &self.previous {
            names.push(previous.retired());
        }
        names.join(", ")
    }

    fn command(&self) -> String {
        crate::kernel_migration::KERNEL_MIGRATION_SWEEP.to_owned()
    }

    async fn dependents(&self, backend: &lash_core::Backend) -> Result<u64, DurableError> {
        let functions = lash_vm_runtime::helper_survey_functions().map_err(|error| {
            DurableError::Store(StoreFailure {
                kind: StoreFailureKind::Corrupt,
                message: error.to_string(),
            })
        })?;
        let mut count = 0u64;
        for release in &self.releases {
            let surveyed = async {
                let retired = retired_helpers(release.ordinal)?;
                survey_helper_dependents(backend, &functions, release.ordinal, &retired).await
            }
            .await
            .map_err(|error| match error {
                KernelMigrationSurveyError::State(error) => error,
                other => DurableError::Store(StoreFailure {
                    kind: StoreFailureKind::Unavailable,
                    message: other.to_string(),
                }),
            })?;
            count = count.saturating_add(
                u64::try_from(surveyed.0.len() + surveyed.1.len()).unwrap_or(u64::MAX),
            );
        }
        if let Some(previous) = &self.previous {
            count = count.saturating_add(previous.dependents(backend).await?);
        }
        Ok(count)
    }
}

/// Installs every declared retirement on ordinary startup, preserving any kernel check.
pub(crate) fn retiring(
    backend: &crate::Backend,
    releases: Vec<lash_vm_runtime::HelperReleaseIndex>,
) -> crate::Backend {
    backend.with_retirement(Arc::new(HelperRetirement {
        releases,
        previous: backend.retirement().cloned(),
    }))
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

fn fleet_helper_release(
    candidates: &[(u32, FormatSet)],
    carried: &[FormatSet],
    live: &[Vec<FormatSet>],
) -> Option<u32> {
    // A carried turn can reach its first new cell without a snapshot. Nodes
    // serving those sets participate even if they decode no lowering candidate.
    let serving: Vec<_> = live
        .iter()
        .filter(|decodes| {
            candidates.iter().any(|(_, set)| decodes.contains(set))
                || carried.iter().any(|set| decodes.contains(set))
        })
        .collect();
    candidates
        .iter()
        .find(|(_, set)| serving.iter().all(|decodes| decodes.contains(set)))
        .map(|(release, _)| *release)
}

fn helpers_unavailable(message: impl Into<String>) -> lash_core::RuntimeEffectControllerError {
    lash_core::RuntimeEffectControllerError::new(
        lash_core::RuntimeErrorCode::RunDefinitionUnavailable,
        message,
    )
    .retryable_uncommitted_derivation()
}

#[async_trait::async_trait]
impl lash_protocol_rlm::HelperReleaseGate for FleetHelperWrites {
    async fn writable(&self) -> Result<u32, lash_core::RuntimeEffectControllerError> {
        let formats = self.backend.formats();
        let own = crate::formats::helper_release_of(self.backend.surfaces());
        let mut candidates = vec![(own, formats.session().clone())];
        candidates.extend(
            formats
                .decoded_sessions()
                .into_iter()
                .map(|(surfaces, set)| (crate::formats::helper_release_of(surfaces), set)),
        );
        let live = self
            .backend
            .durable()
            .live_decodes()
            .await
            .map_err(|error| {
                tracing::warn!(?candidates, carried = ?formats.carried_sessions(), ?error, "helper fleet survey unavailable; lowering deferred");
                helpers_unavailable(format!("helper fleet survey unavailable: {error}"))
            })?;
        let chosen = fleet_helper_release(&candidates, formats.carried_sessions(), &live);
        tracing::info!(?candidates, carried = ?formats.carried_sessions(), ?live, ?chosen, "helper release fleet gate");
        chosen.ok_or_else(|| {
            helpers_unavailable(
                "no helper release is readable by every live node serving these sessions",
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// V19: an old-only node still co-serves a carried turn, even when it decodes no lowering candidate.
    #[test]
    fn a_carried_turn_defers_helpers_no_co_serving_node_can_read() {
        let new = FormatSet::new("session:kernel-parked-state@2,kernel-helpers@2");
        let old = FormatSet::new("session:kernel-parked-state@1");
        let live = vec![vec![new.clone(), old.clone()], vec![old.clone()]];
        assert_eq!(fleet_helper_release(&[(2, new)], &[old], &live), None);
    }
}
