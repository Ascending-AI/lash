//! Restate's implementation of root release, resume and bounded park recovery.
//!
//! A paused session drive is never resumed blindly by recovery (ADR 0109
//! §3): it is parked on its session's next root, and the park's redrive,
//! cancel or fork resumes it. Recovery resumes only a drive that stopped
//! behind a redrive that has since settled (D15), and releases one whose
//! session's next work names no root, leaving that work to its ingress
//! obligations.
//!
//! Every verb reaches the engine through the Restate admin API: pausing,
//! resuming and killing an invocation have no ingress form. A deployment
//! therefore always configures the admin endpoint
//! ([`RestateConfig::new`](crate::RestateConfig::new) requires it), so a
//! release is never refused for want of a connection.
use std::sync::Arc;

use lash_core::engine::{
    EngineAck, EngineCursor, EnginePage, EngineParkRecorded, EngineRefusal, ParkReconcileReport,
    ParkRecoveryWriter, ParkTarget, RootRef, SessionControlEngine, StalledExecution,
};
use lash_core::store::EnginePark;

use crate::session_driver::{parse_turn_workflow_key, turn_workflow_key};
use crate::{RestateAdminClient, RestateIngressClient, RestateInvocationId};

pub(crate) struct RestateSessionControl {
    pub(crate) admin: RestateAdminClient,
    pub(crate) ingress: RestateIngressClient,
    /// The namespace whose drives, roots and segments this control reads
    /// and settles (FIG-3898): another deployment's paused work on the same
    /// server is never this one's.
    pub(crate) namespace: crate::RestateNamespace,
    pub(crate) processes: Arc<dyn lash_core::ProcessRegistry>,
    pub(crate) continuations: Arc<dyn lash_core::ProcessContinuationStore>,
    pub(crate) sessions: Arc<dyn lash_core::DeploymentStore>,
}

#[derive(Default)]
pub(crate) struct LostRootPass {
    pub(crate) ended: Vec<RootRef>,
    /// The partials the pass's terminal writes sealed (ADR 0114 §4.4), in
    /// the order their roots ended.
    pub(crate) sealed: Vec<(RootRef, lash_sansio::StoppedPartialSummary)>,
    pub(crate) unchanged: usize,
    pub(crate) failed: Vec<(String, String)>,
}

/// Visit the store's open roots in stable pages and end only those whose
/// authoritative Restate workflow run failed terminally. A live or paused
/// run in any generation lane keeps its root.
///
/// A run that recorded an outcome is skipped: it is not lost. It committed
/// its root, or it met a refusal no retry changes and ended the root in the
/// store before it recorded `Released` (FIG-4018), so the refused run is the
/// one writer of that root's terminal and this pass writes only for a run
/// that recorded nothing.
pub(crate) async fn end_lost_root_runs(
    admin: &RestateAdminClient,
    ingress: &RestateIngressClient,
    namespace: &crate::RestateNamespace,
    sessions: &Arc<dyn lash_core::DeploymentStore>,
    limit: std::num::NonZeroUsize,
) -> Result<LostRootPass, lash_core::StoreError> {
    let mut pass = LostRootPass::default();
    let mut after: Option<RootRef> = None;
    loop {
        let page = sessions
            .non_terminal_roots_page(after.as_ref(), limit)
            .await?;
        if page.is_empty() {
            return Ok(pass);
        }
        let keys: Vec<String> = page
            .iter()
            .map(|target| turn_workflow_key(&target.session, &target.root))
            .collect();
        let runs = admin.root_runs(namespace, &keys).await.map_err(|error| {
            lash_core::StoreError::Backend(format!("read root runs from Restate: {error}"))
        })?;
        for run in runs.iter().filter(|run| {
            run.status == crate::RestateInvocationLifecycle::Completed
                && run.completion_result.as_deref() == Some("failure")
        }) {
            let Some((session, root)) = run
                .target_service_key
                .as_deref()
                .and_then(parse_turn_workflow_key)
            else {
                pass.failed
                    .push((run.id.clone(), "failed root run has no valid key".into()));
                continue;
            };
            let target = RootRef { session, root };
            if !page.contains(&target) {
                pass.failed.push((
                    run.id.clone(),
                    "failed root run is outside its store page".into(),
                ));
                continue;
            }
            if runs.iter().any(|other| {
                other.target_service_key == run.target_service_key
                    && (other.status != crate::RestateInvocationLifecycle::Completed
                        || other.completion_result.as_deref() != Some("failure"))
            }) {
                pass.unchanged += 1;
                continue;
            }
            let outcome: Option<lash_core::engine::RootOutcome> = match ingress
                .call_lash_workflow(
                    &run.target_service_name,
                    &turn_workflow_key(&target.session, &target.root),
                    "outcome",
                    &(),
                )
                .await
            {
                Ok(outcome) => outcome,
                Err(error) => {
                    pass.failed
                        .push((run.id.clone(), format!("read root outcome: {error}")));
                    continue;
                }
            };
            if outcome.is_some() {
                pass.unchanged += 1;
                continue;
            }
            let at_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64;
            match sessions.end_lost_root(&target, at_ms).await {
                Ok(Some(terminal)) => {
                    if let Some(summary) = terminal.stopped_partial {
                        announce_stopped_partial(&target, &summary);
                        pass.sealed.push((target.clone(), summary));
                    }
                    pass.ended.push(target);
                }
                Ok(None) => pass.unchanged += 1,
                Err(error) => pass.failed.push((run.id.clone(), error.to_string())),
            }
        }
        after = page.last().cloned();
        if page.len() < limit.get() {
            return Ok(pass);
        }
    }
}

/// The lost-root write sealed a partial. The pass reports it, and the core
/// that runs the pass announces it to the session's hosts through its Live
/// Replay publisher (ADR 0114 §4.4); a host that misses the announcement
/// reads the partial by its root (§5.2).
fn announce_stopped_partial(target: &RootRef, summary: &lash_sansio::StoppedPartialSummary) {
    tracing::info!(
        event = "root.stopped_partial_available",
        session_id = %target.session,
        root = %target.root,
        turn = %summary.id.turn_id,
        sealed_through = summary.id.sealed_through,
        digest = %summary.digest.to_hex(),
        reason = ?summary.reason,
        eligibility = ?summary.eligibility,
        items = summary.item_count,
        "a lost root's stopped partial was sealed"
    );
}

fn refusal(error: impl std::fmt::Display) -> EngineRefusal {
    EngineRefusal::Retryable(error.to_string())
}

/// One paused invocation, re-read on demand: what the park writer asks when
/// a redrive may have resumed it since the listing.
struct PausedInvocation<'a> {
    admin: &'a RestateAdminClient,
    invocation: RestateInvocationId,
}

#[async_trait::async_trait]
impl StalledExecution for PausedInvocation<'_> {
    async fn still_stopped(&self) -> Result<bool, EngineRefusal> {
        Ok(self
            .admin
            .invocation_status(&self.invocation)
            .await
            .map_err(refusal)?
            .is_some_and(|status| {
                status.status == crate::ingress::RestateInvocationLifecycle::Paused
            }))
    }
}

impl RestateSessionControl {
    async fn invocation(
        &self,
        target: &RootRef,
        handle: Option<&EnginePark>,
    ) -> Result<Option<crate::ingress::RestateInvocationStatus>, EngineRefusal> {
        let status = match handle {
            Some(handle) => {
                self.admin
                    .invocation_status(&RestateInvocationId::new(handle.as_str().to_owned()))
                    .await
            }
            None => {
                self.admin
                    .workflow_invocation_status(
                        &self.namespace.stable(crate::LashService::TurnDriver).name(),
                        &turn_workflow_key(&target.session, &target.root),
                        "run",
                    )
                    .await
            }
        }
        .map_err(refusal)?;
        // A stored handle names the root's execution: a `LashTurn` run of
        // the root's session. A follow-on's recovery runs under an admitted
        // name of its own, so the key's root may differ from the park's.
        if let Some(status) = status.as_ref()
            && (!self
                .namespace
                .parse(&status.target_service_name)
                .is_some_and(|route| route.service() == crate::LashService::TurnDriver)
                || status.target_handler_name != "run"
                || status
                    .target_service_key
                    .as_deref()
                    .and_then(parse_turn_workflow_key)
                    .is_none_or(|(session, _)| session != target.session))
        {
            return Err(refusal(
                "stored engine handle does not name a run of the requested root's session",
            ));
        }
        Ok(status)
    }

    /// Settle one paused invocation of the listing: resume an admission-only
    /// drive, park or release a root's execution, or hand a process to its
    /// registry's reconcile.
    async fn reconcile_invocation(
        &self,
        parks: &dyn ParkRecoveryWriter,
        invocation: crate::ingress::RestatePausedInvocation,
        report: &mut ParkReconcileReport,
    ) -> Result<(), EngineRefusal> {
        let Some(key) = invocation.target_service_key.clone() else {
            report.unchanged += 1;
            return Ok(());
        };
        // Any lane of the service (FIG-3795): a paused invocation keeps the
        // lane it was pinned under.
        let service = self
            .namespace
            .parse(&invocation.target_service_name)
            .map(|route| route.service());
        if service == Some(crate::LashService::SessionDriver) {
            self.reconcile_drive(parks, invocation, key.as_str().into(), report)
                .await?;
        } else if service == Some(crate::LashService::ProcessWorkflow) {
            let pass = crate::process::park_reconcile::reconcile_process_invocations(
                &self.admin,
                &self.processes,
                &self.continuations,
                vec![invocation],
            )
            .await
            .map_err(refusal)?;
            report.parked.extend(
                pass.parked
                    .into_iter()
                    .map(|process| ParkTarget::Process { process }),
            );
            report.unchanged += pass.unchanged;
        } else if let Some((session, root)) = parse_turn_workflow_key(&key) {
            let target = ParkTarget::Root {
                session: session.clone(),
                root: root.clone(),
            };
            let reason = crate::process::park_reconcile::exhausted_reason(&invocation);
            let probe = PausedInvocation {
                admin: &self.admin,
                invocation: invocation.invocation_id(),
            };
            match parks
                .record_engine_park(
                    &target,
                    reason,
                    EnginePark::new(invocation.id.clone()),
                    &probe,
                )
                .await
                .map_err(refusal)?
            {
                EngineParkRecorded::Parked(_) => report.parked.push(target),
                EngineParkRecorded::AttachedToExisting(_) => report.attached += 1,
                EngineParkRecorded::Redriven
                | EngineParkRecorded::NothingToPark
                | EngineParkRecorded::ResumeDrive => {
                    report.unchanged += 1;
                }
                EngineParkRecorded::TargetTerminal | EngineParkRecorded::TargetGone => {
                    self.admin
                        .kill_invocation(&invocation.invocation_id())
                        .await
                        .map_err(refusal)?;
                    report.released.push(RootRef { session, root });
                }
            }
        } else {
            report.unchanged += 1;
        }
        Ok(())
    }

    /// Settle one paused session drive (ADR 0109 §3): never resumed blindly.
    /// Its session is parked on its next root, and only that park's operator
    /// verb resumes it ([`Self::resume_session_drives`]). A drive that
    /// stopped behind a redrive that has since settled is resumed; one whose
    /// session is gone, or whose next work names no root to park on, is
    /// killed, and the session's ingress obligations ask for a fresh drive.
    async fn reconcile_drive(
        &self,
        parks: &dyn ParkRecoveryWriter,
        invocation: crate::ingress::RestatePausedInvocation,
        session: lash_core::SessionId,
        report: &mut ParkReconcileReport,
    ) -> Result<(), EngineRefusal> {
        let target = ParkTarget::Drive {
            session: session.clone(),
        };
        let reason = crate::process::park_reconcile::exhausted_reason(&invocation);
        let probe = PausedInvocation {
            admin: &self.admin,
            invocation: invocation.invocation_id(),
        };
        match parks
            .record_engine_park(
                &target,
                reason,
                EnginePark::new(invocation.id.clone()),
                &probe,
            )
            .await
            .map_err(refusal)?
        {
            EngineParkRecorded::Parked(_) => report.parked.push(target),
            EngineParkRecorded::AttachedToExisting(_) => report.attached += 1,
            EngineParkRecorded::ResumeDrive => {
                self.admin
                    .resume_invocation(&invocation.invocation_id())
                    .await
                    .map_err(refusal)?;
                tracing::info!(
                    session_id = %session,
                    invocation = invocation.id.as_str(),
                    event = "session.drive.resumed",
                    "a session drive paused behind a redrive that has since settled is resumed"
                );
                report.resumed_drives.push(session);
            }
            EngineParkRecorded::NothingToPark => {
                self.admin
                    .kill_invocation(&invocation.invocation_id())
                    .await
                    .map_err(refusal)?;
                tracing::warn!(
                    session_id = %session,
                    invocation = invocation.id.as_str(),
                    event = "session.drive.released",
                    "a paused session drive has no root to park on; it is released, and the \
                     session's ingress obligations ask for a fresh drive"
                );
                report.released_drives.push(session);
            }
            EngineParkRecorded::Redriven | EngineParkRecorded::TargetTerminal => {
                report.unchanged += 1;
            }
            EngineParkRecorded::TargetGone => {
                self.admin
                    .kill_invocation(&invocation.invocation_id())
                    .await
                    .map_err(refusal)?;
                report.released_drives.push(session);
            }
        }
        Ok(())
    }

    /// Resume the session's paused drive, if one is paused: the half of a
    /// park's operator verb that lets the session's admission go on. Whether
    /// any was resumed.
    async fn resume_session_drives(
        &self,
        session: &lash_core::SessionId,
    ) -> Result<bool, EngineRefusal> {
        let paused = self
            .admin
            .paused_session_drives(&self.namespace, session.as_str())
            .await
            .map_err(refusal)?;
        for drive in &paused {
            self.admin
                .resume_invocation(&drive.invocation_id())
                .await
                .map_err(refusal)?;
        }
        Ok(!paused.is_empty())
    }
}

#[async_trait::async_trait]
impl SessionControlEngine for RestateSessionControl {
    async fn resume_root(
        &self,
        target: &RootRef,
        handle: Option<&EnginePark>,
    ) -> Result<EngineAck, EngineRefusal> {
        // A redrive resumes the root's stopped execution and the session's
        // drive stopped behind the park (ADR 0109 §3): the only resume a
        // paused drive gets.
        let status = self.invocation(target, handle).await?;
        let root = match status {
            Some(status) if status.status == crate::ingress::RestateInvocationLifecycle::Paused => {
                self.admin
                    .resume_invocation(&status.invocation_id())
                    .await
                    .map_err(refusal)?;
                true
            }
            _ => false,
        };
        let drive = self.resume_session_drives(&target.session).await?;
        Ok(if root || drive {
            EngineAck::Resumed
        } else {
            EngineAck::NothingHeld
        })
    }

    async fn resume_process(
        &self,
        process: &lash_core::ProcessId,
        park: lash_core::store::ParkId,
    ) -> Result<EngineAck, EngineRefusal> {
        let record = self
            .processes
            .get_process(process)
            .await
            .map_err(refusal)?
            .ok_or_else(|| refusal("process is gone"))?;
        let current = record
            .park
            .as_deref()
            .ok_or_else(|| refusal("process is not parked"))?;
        if current.park_id != park {
            return Err(refusal("process park was superseded"));
        }
        crate::process::resume_parked_process(
            &self.admin,
            &self.namespace,
            &self.processes,
            process,
        )
        .await
        .map_err(refusal)?;
        Ok(EngineAck::Resumed)
    }

    async fn release_root(
        &self,
        target: &RootRef,
        handle: Option<&EnginePark>,
    ) -> Result<EngineAck, EngineRefusal> {
        let status = self.invocation(target, handle).await?;
        let released = match status {
            Some(status) if status.is_still_active() => {
                self.admin
                    .kill_invocation(&status.invocation_id())
                    .await
                    .map_err(refusal)?;
                true
            }
            _ => false,
        };
        // The store already ended the root: the session's drive stopped
        // behind its park admits what follows it once resumed.
        self.resume_session_drives(&target.session).await?;
        Ok(if released {
            EngineAck::Released
        } else {
            EngineAck::NothingHeld
        })
    }

    /// One bounded page of paused invocations. Each invocation is settled on
    /// its own: one that fails is reported in
    /// [`failed`](ParkReconcileReport::failed) and the page goes on, and the
    /// cursor moves past it, so a single stuck invocation never keeps the
    /// ones after it waiting. The next pass that wraps to it retries it.
    async fn reconcile_parks(
        &self,
        parks: &dyn ParkRecoveryWriter,
        page: EnginePage,
    ) -> Result<ParkReconcileReport, EngineRefusal> {
        let invocations = self
            .admin
            .paused_work_page(
                &self.namespace,
                page.after.as_ref().map(|c| c.as_str()),
                page.limit,
            )
            .await
            .map_err(refusal)?;
        let mut report = ParkReconcileReport::default();
        if invocations.len() == page.limit.get() {
            report.next = invocations.last().map(|v| EngineCursor::new(v.id.clone()));
        }
        for invocation in invocations {
            let id = EngineCursor::new(invocation.id.clone());
            if let Err(error) = self
                .reconcile_invocation(parks, invocation, &mut report)
                .await
            {
                tracing::warn!(
                    invocation = id.as_str(),
                    %error,
                    "park reconcile could not settle a paused invocation; the next pass retries it"
                );
                report.failed.push((id, error.to_string()));
            }
        }
        // A process segment's run the engine finished without the process's
        // terminal (an operator's kill) strands the process: Restate never
        // runs that key again. End each one `SubstrateLost` (ADR 0110). A
        // segment whose run Restate no longer holds is resubmitted, and its
        // admission ends a started process `SubstrateLost`.
        match crate::process::park_reconcile::end_lost_process_runs(
            &self.admin,
            &self.ingress,
            &self.namespace,
            &self.processes,
            &self.continuations,
            page.limit,
        )
        .await
        {
            Ok(pass) => {
                for process_id in &pass.resubmitted {
                    tracing::warn!(
                        event = "process.run_missing",
                        process_id = process_id.as_str(),
                        "Restate no longer holds a process's current segment; it was resubmitted"
                    );
                }
                report.ended_processes.extend(pass.ended);
                report.unchanged += pass.unchanged;
                report.failed.extend(
                    pass.failed
                        .into_iter()
                        .map(|(id, error)| (EngineCursor::new(id), error)),
                );
            }
            Err(error) => {
                tracing::warn!(
                    %error,
                    "lost-run reconcile could not read process runs; the next pass retries it"
                );
            }
        }
        match end_lost_root_runs(
            &self.admin,
            &self.ingress,
            &self.namespace,
            &self.sessions,
            page.limit,
        )
        .await
        {
            Ok(pass) => {
                report.ended_roots.extend(pass.ended);
                report
                    .sealed_partials
                    .extend(pass.sealed.into_iter().map(|(_, summary)| summary));
                report.unchanged += pass.unchanged;
                report.failed.extend(
                    pass.failed
                        .into_iter()
                        .map(|(id, error)| (EngineCursor::new(id), error)),
                );
            }
            Err(error) => {
                tracing::warn!(%error, "lost-root reconcile could not read failed root runs");
            }
        }
        Ok(report)
    }
}
