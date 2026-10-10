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
        &crate::formats::kernel_actor_state_surfaces(kernel),
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
    let Some(earlier) = earlier_kernel_formats() else {
        return Ok(survey);
    };
    let reads = backend.durable();
    for actor in actors_in(backend, earlier.session()).await? {
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
    Ok(survey)
}

/// What [`sweep_kernel_migration`] woke.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct KernelMigrationSweep {
    /// The processes still in the earlier kernel version it woke.
    pub processes: usize,
    /// The sessions still in it it woke.
    pub sessions: usize,
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
    let mut sweep = KernelMigrationSweep::default();
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
        &crate::formats::kernel_actor_state_surfaces(kernel),
        KERNEL_MIGRATION_SWEEP,
    )
}
