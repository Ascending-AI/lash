//! What a breaking kernel version leaves in a deployment, and the sweep
//! that carries it forward before the window closes (kernel spec §6
//! "Upgrades"; ADR 0106 §1, ADR 0115 §3.5; FIG-5787).
//!
//! A build that ships a kernel version carries the processes and sessions
//! the build before it parked forward as a node of it claims them. One that
//! no node claims within the window, a process waiting on a long timer or a
//! session that runs no turn, would be stranded under the build that
//! deletes the previous interpreter. [`survey_kernel_migration`] lists them
//! (`lashctl kernel-migration list`); [`sweep_kernel_migration`] wakes each
//! (`lashctl kernel-migration run`), so a live node of the new build claims
//! it and carries it: a process at its claim, as any pass does, and an idle
//! session outside a turn, restored, recaptured and stamped with the
//! build's format set. A build that retires the previous kernel's formats
//! does not start while one is left ([`KERNEL_MIGRATION_SWEEP`] names the
//! command its refusal points to).

use std::sync::Arc;

use lash_core::durable_port::{ActorKey, CommitLabel, DurableError, MailRefusal, MailTx};
use lash_core::formats::BuildFormats;
use lash_kernel_doc::FunctionRegistry;
use lash_vm_runtime::{
    KernelMigrationSurvey, KernelMigrationSurveyError, LASH_VM_ENGINE_KIND, RefusedKernelCell,
    UnmigratedKernelSession,
};

/// The operator command that carries every process and session still in the
/// previous kernel version forward: what a build that retires that version
/// names when it refuses to start.
pub const KERNEL_MIGRATION_SWEEP: &str = "lashctl kernel-migration run";

/// How many actors one listing page reads.
const PAGE: usize = 100;

/// The format sets a build of the kernel version before this build's wrote:
/// the version this build's window carries forward or, the window closed,
/// retires. `None` when there is none.
fn earlier_kernel_formats() -> Option<BuildFormats> {
    let kernel = lash_vm_runtime::previous_kernel_version()
        .or_else(lash_vm_runtime::retired_kernel_version)?;
    Some(BuildFormats::new(
        &[lash_core::EngineStateFormat {
            kind: LASH_VM_ENGINE_KIND.to_owned(),
            version: kernel,
        }],
        &crate::formats::kernel_actor_state_surfaces(
            kernel,
            crate::formats::previous_helper_release()
                .unwrap_or(crate::formats::KERNEL_HELPER_RELEASE),
        ),
    ))
}

/// Every actor that has not ended whose state is in `set`.
async fn actors_in(
    backend: &crate::Backend,
    set: &lash_core::durable_port::FormatSet,
) -> Result<Vec<ActorKey>, DurableError> {
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

/// Checks every unfinished kernel process and every session of `backend`
/// still in the kernel version before this build's newest against the
/// kernel migration this build ships, with `functions` as its library, and
/// lists what it would refuse: a process with its typed reason, and a
/// session with each cell of its open turn this build would not resume. It
/// reads the store and writes nothing: an operator runs it with the next
/// build's `lashctl` before the upgrade, and before closing the window.
///
/// # Errors
///
/// [`KernelMigrationSurveyError`]: a store did not answer.
pub async fn survey_kernel_migration(
    backend: &crate::Backend,
    functions: &Arc<FunctionRegistry>,
) -> Result<KernelMigrationSurvey, KernelMigrationSurveyError> {
    let mut survey = lash_vm_runtime::survey_kernel_processes(backend, functions).await?;
    let earlier = earlier_kernel_formats();
    let reads = backend.durable();
    let sessions = match &earlier {
        Some(earlier) => actors_in(backend, earlier.session()).await?,
        None => Vec::new(),
    };
    for actor in sessions {
        let Ok(session) = lash_core::SessionId::parse(actor.id()) else {
            continue;
        };
        let mut refused = Vec::new();
        if let Some(turn) = reads.turn(&session).await? {
            for cell in reads.cell_snapshots(&session, &turn.run).await? {
                let lash_core::durable_port::domain::ExecKey::Cell(_, _, id) = &cell.exec else {
                    continue;
                };
                if let Some(refusal) = lash_protocol_rlm::cell_migration_refusal(
                    &cell.snapshot_ref,
                    Arc::clone(functions),
                ) {
                    refused.push(RefusedKernelCell {
                        turn: turn.run.clone(),
                        cell: id.as_str().to_owned(),
                        refusal,
                    });
                }
            }
        }
        survey.unmigrated += 1;
        survey
            .sessions
            .push(UnmigratedKernelSession { session, refused });
    }
    // What still pins a helper release before this build's own, which a
    // later build may stop retaining (FIG-5799).
    for release in surveyed_helper_releases()? {
        let retired = crate::helper_releases::retired_helpers(release)?;
        let functions = lash_vm_runtime::helper_survey_functions().map_err(|error| {
            KernelMigrationSurveyError::State(DurableError::Store(
                lash_core::durable_port::StoreFailure {
                    kind: lash_core::durable_port::StoreFailureKind::Corrupt,
                    message: error.to_string(),
                },
            ))
        })?;
        let (processes, sessions) = crate::helper_releases::survey_helper_dependents(
            backend, &functions, release, &retired,
        )
        .await?;
        survey.helper_processes.extend(processes);
        survey.helper_sessions.extend(sessions);
    }
    Ok(survey)
}

/// What [`sweep_kernel_migration`] woke.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct KernelMigrationSweep {
    /// The processes still in the earlier kernel version it woke.
    pub processes: usize,
    /// The sessions still in it it woke.
    pub sessions: usize,
    /// The processes still pinning a helper release before this build's
    /// own it woke, for a node that adopts to adopt (FIG-5799).
    #[serde(skip_serializing_if = "is_zero")]
    pub helper_processes: usize,
}

fn is_zero(count: &usize) -> bool {
    *count == 0
}

/// Wakes every process and session of `backend` still in the kernel
/// version before this build's newest, so a live node of a build that
/// carries that version forward claims it and carries it: a process at its
/// claim, an idle session outside a turn. It carries nothing itself, so it
/// is idempotent, and resumable by running it again: what is still listed
/// is woken again. A parked actor stays parked (only a redrive readies it),
/// and the survey lists it.
///
/// # Errors
///
/// The store's.
pub async fn sweep_kernel_migration(
    backend: &crate::Backend,
) -> Result<KernelMigrationSweep, DurableError> {
    let mut sweep = KernelMigrationSweep {
        helper_processes: wake_helper_dependents(backend).await?,
        ..KernelMigrationSweep::default()
    };
    let Some(earlier) = earlier_kernel_formats() else {
        return Ok(sweep);
    };
    let sets = earlier
        .process(LASH_VM_ENGINE_KIND)
        .map(|set| (set, CommitLabel::MAIL_PROCESS))
        .into_iter()
        .chain([(earlier.session(), CommitLabel::MAIL_SESSION)]);
    for (set, label) in sets {
        for actor in actors_in(backend, set).await? {
            let mut tx = MailTx::new();
            tx.wake(actor);
            match backend.commit_mail(tx, label).await {
                Ok(_) => {}
                // It ended since the listing read it: nothing to carry.
                Err(DurableError::MailRefused(
                    MailRefusal::ActorTerminal(_) | MailRefusal::UnknownActor(_),
                )) => continue,
                Err(error) => return Err(error),
            }
            if label == CommitLabel::MAIL_PROCESS {
                sweep.processes += 1;
            } else {
                sweep.sessions += 1;
            }
        }
    }
    Ok(sweep)
}

/// Wakes every unfinished kernel process that pins a function of a helper
/// release before this build's own, so a node that adopts claims it and
/// adopts it; answers how many it woke.
async fn wake_helper_dependents(backend: &crate::Backend) -> Result<usize, DurableError> {
    let unavailable = |error: KernelMigrationSurveyError| match error {
        KernelMigrationSurveyError::State(error) => error,
        other => DurableError::Store(lash_core::durable_port::StoreFailure {
            kind: lash_core::durable_port::StoreFailureKind::Unavailable,
            message: other.to_string(),
        }),
    };
    let mut woken = 0;
    for release in surveyed_helper_releases().map_err(unavailable)? {
        let retired = crate::helper_releases::retired_helpers(release).map_err(unavailable)?;
        let functions = lash_vm_runtime::helper_survey_functions().map_err(|error| {
            unavailable(KernelMigrationSurveyError::State(DurableError::Store(
                lash_core::durable_port::StoreFailure {
                    kind: lash_core::durable_port::StoreFailureKind::Corrupt,
                    message: error.to_string(),
                },
            )))
        })?;
        let processes = lash_vm_runtime::survey_helper_processes(backend, &functions, &retired)
            .await
            .map_err(unavailable)?;
        for dependent in processes {
            let Ok(actor) = ActorKey::process(dependent.process.as_str()) else {
                continue;
            };
            let mut tx = MailTx::new();
            tx.wake(actor);
            match backend.commit_mail(tx, CommitLabel::MAIL_PROCESS).await {
                Ok(_) => woken += 1,
                Err(DurableError::MailRefused(
                    MailRefusal::ActorTerminal(_) | MailRefusal::UnknownActor(_),
                )) => {}
                Err(error) => return Err(error),
            }
        }
    }
    Ok(woken)
}

/// The kernel version a backend of this build retires, when the window the
/// build before it opened is closed: what its nodes refuse to start over
/// while a process or session is still in it.
pub(crate) fn retired_kernel_version() -> Option<u32> {
    lash_vm_runtime::retired_kernel_version()
}

/// `backend`, retiring the format sets of kernel version `kernel`.
pub(crate) fn retiring(backend: &crate::Backend, kernel: u32) -> crate::Backend {
    backend.retiring(
        &lash_core::EngineStateFormat {
            kind: LASH_VM_ENGINE_KIND.to_owned(),
            version: kernel,
        },
        &crate::formats::kernel_actor_state_surfaces(
            kernel,
            crate::formats::previous_helper_release()
                .unwrap_or(crate::formats::KERNEL_HELPER_RELEASE),
        ),
        KERNEL_MIGRATION_SWEEP,
    )
}

/// Retained and removed releases participate in the same operator sweep.
fn surveyed_helper_releases() -> Result<Vec<u32>, KernelMigrationSurveyError> {
    let mut releases = crate::formats::earlier_helper_releases().map_err(|error| {
        KernelMigrationSurveyError::State(DurableError::Store(
            lash_core::durable_port::StoreFailure {
                kind: lash_core::durable_port::StoreFailureKind::Corrupt,
                message: error.to_string(),
            },
        ))
    })?;
    let retiring = lash_vm_runtime::retiring_helper_releases().map_err(|error| {
        KernelMigrationSurveyError::State(DurableError::Store(
            lash_core::durable_port::StoreFailure {
                kind: lash_core::durable_port::StoreFailureKind::Corrupt,
                message: error.to_string(),
            },
        ))
    })?;
    releases.extend(retiring.into_iter().map(|release| release.ordinal));
    releases.sort_unstable();
    releases.dedup();
    Ok(releases)
}
