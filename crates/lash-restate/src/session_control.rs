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
    ParkRecoveryWriter, ParkTarget, RootRef, RootRunLoss, SessionControlEngine, StalledExecution,
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
    pub(crate) lost_processes: tokio::sync::Mutex<Option<lash_core::ProcessRegistryCursor>>,
    pub(crate) lost_roots: tokio::sync::Mutex<Option<RootRef>>,
}

#[derive(Default)]
pub(crate) struct LostRootPass {
    pub(crate) ended: Vec<RootRef>,
    pub(crate) unchanged: usize,
    /// Roots this pass could not settle, each by its workflow key, with why.
    pub(crate) failed: Vec<(String, String)>,
}

pub(crate) struct RecoveryScan<'a, C> {
    pub(crate) limit: std::num::NonZeroUsize,
    pub(crate) after: &'a mut Option<C>,
    pub(crate) deadline: tokio::time::Instant,
}

/// Visit one stable page of the store's open roots and end each one whose
/// execution Restate lost, on the evidence its runs give
/// ([`RootRunLoss`]). The store supplies the keys, so the engine's retained
/// history never decides which roots are read, and every generation lane of
/// a key is read together: a live, paused or completed run on any lane
/// keeps its root.
///
/// - Every run of the key failed: the key never runs again. A run that
///   recorded an outcome is not lost: it committed its root, or it met a
///   refusal no retry changes and ended the root in the store before it
///   recorded `Released` (FIG-4018), so the refused run is the one writer of
///   that root's terminal. A root whose failed runs recorded nothing ends
///   ([`RootRunLoss::FailedRun`]).
/// - No lane holds a run of the key: its run was purged or its history lost
///   after the root was admitted (FIG-4281). The store ends a root that
///   recorded its admission, and so started: a fresh execution would run
///   its effects again under an empty journal. A root that never recorded
///   its admission started nothing; its ingress obligation still owns its
///   input and drives it, so the store leaves it ([`RootRunLoss::NoRun`]).
///   A root a live process runs is not a root run at all, so no lane ever
///   holds one of its key, and the pass leaves it ([`run_by_live_process`]).
///
/// An admin read that fails proves nothing about any run: the pass stops
/// before it ends anything on that page. Every row spends the inspected-record
/// budget. The cursor advances before engine requests, including failures and
/// timeouts, and an exhausted catalog wraps and retries them. Store reads,
/// queries, outcomes and terminal writes share the page deadline.
pub(crate) async fn end_lost_root_runs(
    admin: &RestateAdminClient,
    ingress: &RestateIngressClient,
    namespace: &crate::RestateNamespace,
    sessions: &Arc<dyn lash_core::DeploymentStore>,
    processes: &Arc<dyn lash_core::ProcessRegistry>,
    scan: RecoveryScan<'_, RootRef>,
) -> Result<LostRootPass, lash_core::StoreError> {
    let RecoveryScan {
        limit,
        after,
        deadline,
    } = scan;
    let mut pass = LostRootPass::default();
    let page = match recovery_request(
        deadline,
        sessions.non_terminal_roots_page(after.as_ref(), limit),
    )
    .await
    {
        Ok(page) => page,
        Err(RecoveryRequestError::Failed(error)) => return Err(error),
        Err(error) => {
            pass.failed
                .push(("lost-root-page".into(), error.to_string()));
            return Ok(pass);
        }
    };
    *after = if page.len() == limit.get() {
        page.last().cloned()
    } else {
        None
    };
    if page.is_empty() {
        return Ok(pass);
    }
    let keys: Vec<String> = page
        .iter()
        .map(|target| turn_workflow_key(&target.session, &target.root))
        .collect();
    let runs = match recovery_request(deadline, admin.root_runs(namespace, &keys)).await {
        Ok(runs) => runs,
        Err(RecoveryRequestError::Failed(error)) => {
            return Err(lash_core::StoreError::Backend(format!(
                "read root runs from Restate: {error}"
            )));
        }
        Err(error) => {
            pass.failed
                .push(("lost-root-page".into(), error.to_string()));
            return Ok(pass);
        }
    };
    for (target, key) in page.iter().zip(&keys) {
        let key_runs: Vec<&crate::RestateInvocationStatus> = runs
            .iter()
            .filter(|run| run.target_service_key.as_deref() == Some(key.as_str()))
            .collect();
        let loss = if key_runs.is_empty() {
            match recovery_request(deadline, run_by_live_process(processes, target)).await {
                Ok(false) => RootRunLoss::NoRun,
                Ok(true) => {
                    pass.unchanged += 1;
                    continue;
                }
                Err(error) => {
                    pass.failed.push((key.clone(), error.to_string()));
                    continue;
                }
            }
        } else if key_runs.iter().all(|run| run.completed_with_failure()) {
            match recorded_outcome(ingress, &key_runs, key, deadline).await {
                Ok(false) => RootRunLoss::FailedRun,
                Ok(true) => {
                    pass.unchanged += 1;
                    continue;
                }
                Err(error) => {
                    pass.failed.push((key.clone(), error));
                    continue;
                }
            }
        } else {
            pass.unchanged += 1;
            continue;
        };
        let at_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        match recovery_request(deadline, sessions.end_lost_root(target, loss, at_ms)).await {
            Ok(Some(_)) => {
                tracing::warn!(
                    event = "root.run_lost",
                    session_id = target.session.as_str(),
                    root = target.root.as_str(),
                    ?loss,
                    "Restate lost a root's execution; the root ends substrate-lost"
                );
                pass.ended.push(target.clone());
            }
            Ok(None) => pass.unchanged += 1,
            Err(error) => pass.failed.push((key.clone(), error.to_string())),
        }
    }
    Ok(pass)
}

/// Every operation in a page spends the same deadline. Dropping a timed-out
/// request does not undo its durable write; all recovery writes and sends
/// are idempotent when a later scan reaches the item again.
pub(crate) async fn recovery_request<T, E: std::fmt::Display>(
    deadline: tokio::time::Instant,
    request: impl std::future::Future<Output = Result<T, E>>,
) -> Result<T, RecoveryRequestError<E>> {
    if tokio::time::Instant::now() >= deadline {
        return Err(RecoveryRequestError::BudgetExhausted);
    }
    match tokio::time::timeout_at(deadline, request).await {
        Ok(result) => result.map_err(RecoveryRequestError::Failed),
        Err(_) => Err(RecoveryRequestError::BudgetExhausted),
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum RecoveryRequestError<E: std::fmt::Display> {
    #[error("{0}")]
    Failed(E),
    #[error("recovery page time budget exhausted")]
    BudgetExhausted,
}

/// Whether a live process runs `target` in its own execution (FIG-4378).
///
/// A `SessionTurn` process drives its child turn's root inline, in its own
/// `LashProcessWorkflow` run: the root is admitted, sealed and committed
/// there, and never runs as a `LashTurn` run. No lane of the root's key ever
/// holds a run of it, so that absence proves nothing while the process is
/// live, and the lost-process pass owns the process's run. A terminal
/// process runs nothing more, so its root is judged like any other.
///
/// The child root is named by its process's id, and the process's recorded
/// input names the session it runs as its own
/// ([`ProcessRecord::session_turn_root`](lash_core::ProcessRecord::session_turn_root)).
/// A root whose id is not a process id is no process's child root.
async fn run_by_live_process(
    processes: &Arc<dyn lash_core::ProcessRegistry>,
    target: &RootRef,
) -> Result<bool, lash_core::PluginError> {
    let Ok(process_id) = lash_core::ProcessId::parse(target.root.as_str()) else {
        return Ok(false);
    };
    Ok(processes
        .get_process(&process_id)
        .await?
        .is_some_and(|record| {
            !record.is_terminal() && record.session_turn_root().as_ref() == Some(target)
        }))
}

/// Whether any of `key`'s failed runs recorded the root's outcome, read on
/// the lane each ran under.
async fn recorded_outcome(
    ingress: &RestateIngressClient,
    failed: &[&crate::RestateInvocationStatus],
    key: &str,
    deadline: tokio::time::Instant,
) -> Result<bool, String> {
    for run in failed {
        let outcome: Option<lash_core::engine::RootOutcome> = recovery_request(
            deadline,
            ingress.call_lash_workflow(&run.target_service_name, key, "outcome", &()),
        )
        .await
        .map_err(|error| format!("read root outcome of run `{}`: {error}", run.id))?;
        if outcome.is_some() {
            return Ok(true);
        }
    }
    Ok(false)
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
    async fn paused_page(
        &self,
        parks: &dyn ParkRecoveryWriter,
        page: &EnginePage,
        deadline: tokio::time::Instant,
    ) -> Result<ParkReconcileReport, EngineRefusal> {
        let invocations = recovery_request(
            deadline,
            self.admin.paused_work_page(
                &self.namespace,
                page.after.as_ref().map(|cursor| cursor.as_str()),
                page.limit,
            ),
        )
        .await
        .map_err(refusal)?;
        let mut report = ParkReconcileReport::default();
        if invocations.len() == page.limit.get() {
            report.next = invocations
                .last()
                .map(|invocation| EngineCursor::new(invocation.id.clone()));
        }
        for invocation in invocations {
            let id = EngineCursor::new(invocation.id.clone());
            if let Err(error) = recovery_request(
                deadline,
                self.reconcile_invocation(parks, invocation, &mut report),
            )
            .await
            {
                report.failed.push((id, error.to_string()));
            }
        }
        Ok(report)
    }

    async fn lost_process_page(
        &self,
        page: &EnginePage,
        deadline: tokio::time::Instant,
    ) -> Result<crate::process::park_reconcile::LostRunPass, EngineRefusal> {
        let mut cursor = tokio::time::timeout_at(deadline, self.lost_processes.lock())
            .await
            .map_err(refusal)?;
        crate::process::park_reconcile::end_lost_process_runs(
            &self.admin,
            &self.ingress,
            &self.namespace,
            &self.processes,
            &self.continuations,
            RecoveryScan {
                limit: page.limit,
                after: &mut cursor,
                deadline,
            },
        )
        .await
        .map_err(refusal)
    }

    async fn lost_root_page(
        &self,
        page: &EnginePage,
        deadline: tokio::time::Instant,
    ) -> Result<LostRootPass, EngineRefusal> {
        let mut cursor = tokio::time::timeout_at(deadline, self.lost_roots.lock())
            .await
            .map_err(refusal)?;
        end_lost_root_runs(
            &self.admin,
            &self.ingress,
            &self.namespace,
            &self.sessions,
            &self.processes,
            RecoveryScan {
                limit: page.limit,
                after: &mut cursor,
                deadline,
            },
        )
        .await
        .map_err(refusal)
    }

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
        // Independent pages: a slow paused invocation, lost process or root
        // never serializes either of the other catalogs. Cursor locks belong
        // to this installed control, so concurrent callers cannot repeat a page.
        let deadline = tokio::time::Instant::now() + page.budget;
        let (paused, processes, roots) = tokio::join!(
            self.paused_page(parks, &page, deadline),
            self.lost_process_page(&page, deadline),
            self.lost_root_page(&page, deadline),
        );
        let mut report = match paused {
            Ok(report) => report,
            Err(error) => {
                let mut report = ParkReconcileReport {
                    next: page.after.clone(),
                    ..ParkReconcileReport::default()
                };
                report
                    .failed
                    .push((EngineCursor::new("paused-page"), error.to_string()));
                report
            }
        };
        match processes {
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
            Err(error) => report
                .failed
                .push((EngineCursor::new("lost-process-page"), error.to_string())),
        }
        match roots {
            Ok(pass) => {
                report.ended_roots.extend(pass.ended);
                report.unchanged += pass.unchanged;
                report.failed.extend(
                    pass.failed
                        .into_iter()
                        .map(|(id, error)| (EngineCursor::new(id), error)),
                );
            }
            Err(error) => report
                .failed
                .push((EngineCursor::new("lost-root-page"), error.to_string())),
        }
        Ok(report)
    }
}
