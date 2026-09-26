//! Restate's implementation of root release, resume and bounded park recovery.
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
use crate::{RestateAdminClient, RestateInvocationId};

pub(crate) struct RestateSessionControl {
    pub(crate) admin: RestateAdminClient,
    pub(crate) processes: Arc<dyn lash_core::ProcessRegistry>,
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
                        crate::LashService::TurnDriver.name(),
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
            && (status.target_service_name != crate::LashService::TurnDriver.name()
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
        if invocation.target_service_name == crate::LashService::SessionDriver.name() {
            self.admin
                .resume_invocation(&invocation.invocation_id())
                .await
                .map_err(refusal)?;
            report.resumed_drives.push(key.as_str().into());
        } else if invocation.target_service_name == crate::LashService::ProcessWorkflow.name() {
            let pass = crate::process::park_reconcile::reconcile_process_invocations(
                &self.processes,
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
                EngineParkRecorded::Redriven => report.unchanged += 1,
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
}

#[async_trait::async_trait]
impl SessionControlEngine for RestateSessionControl {
    async fn resume_root(
        &self,
        target: &RootRef,
        handle: Option<&EnginePark>,
    ) -> Result<EngineAck, EngineRefusal> {
        let Some(status) = self.invocation(target, handle).await? else {
            return Ok(EngineAck::NothingHeld);
        };
        if status.status != crate::ingress::RestateInvocationLifecycle::Paused {
            return Ok(EngineAck::NothingHeld);
        }
        self.admin
            .resume_invocation(&status.invocation_id())
            .await
            .map_err(refusal)?;
        Ok(EngineAck::Resumed)
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
        crate::process::resume_parked_process(&self.admin, &self.processes, process)
            .await
            .map_err(refusal)?;
        Ok(EngineAck::Resumed)
    }

    async fn release_root(
        &self,
        target: &RootRef,
        handle: Option<&EnginePark>,
    ) -> Result<EngineAck, EngineRefusal> {
        let Some(status) = self.invocation(target, handle).await? else {
            return Ok(EngineAck::NothingHeld);
        };
        if !status.is_still_active() {
            return Ok(EngineAck::NothingHeld);
        }
        self.admin
            .kill_invocation(&status.invocation_id())
            .await
            .map_err(refusal)?;
        Ok(EngineAck::Released)
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
            .paused_work_page(page.after.as_ref().map(|c| c.as_str()), page.limit)
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
        Ok(report)
    }
}
