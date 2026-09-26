//! Restate's implementation of root release, resume and bounded park recovery.
use std::sync::Arc;

use lash_core::engine::{
    EngineAck, EnginePage, EngineParkRecorded, EngineRefusal, ParkReconcileReport,
    ParkRecoveryWriter, ParkTarget, RootRef, SessionControlEngine,
};
use lash_core::store::EnginePark;

use crate::session_driver::{parse_turn_workflow_key, turn_workflow_key};
use crate::{RestateAdminClient, RestateInvocationId};

pub(crate) struct RestateSessionControl {
    pub(crate) admin: Option<RestateAdminClient>,
    pub(crate) processes: Arc<dyn lash_core::ProcessRegistry>,
}

fn refusal(error: impl std::fmt::Display) -> EngineRefusal {
    EngineRefusal::Retryable(error.to_string())
}

impl RestateSessionControl {
    fn admin(&self) -> Result<&RestateAdminClient, EngineRefusal> {
        self.admin
            .as_ref()
            .ok_or_else(|| refusal("Restate control requires with_admin_connection"))
    }

    async fn invocation(
        &self,
        target: &RootRef,
        handle: Option<&EnginePark>,
    ) -> Result<Option<crate::ingress::RestateInvocationStatus>, EngineRefusal> {
        let admin = self.admin()?;
        let status = match handle {
            Some(handle) => {
                admin
                    .invocation_status(&RestateInvocationId::new(handle.as_str().to_owned()))
                    .await
            }
            None => {
                admin
                    .workflow_invocation_status(
                        crate::LashService::TurnDriver.name(),
                        &turn_workflow_key(&target.session, &target.root),
                        "run",
                    )
                    .await
            }
        }
        .map_err(refusal)?;
        if let Some(status) = status.as_ref()
            && (status.target_service_name != crate::LashService::TurnDriver.name()
                || status.target_service_key.as_deref()
                    != Some(turn_workflow_key(&target.session, &target.root).as_str())
                || status.target_handler_name != "run")
        {
            return Err(refusal(
                "stored engine handle does not name the requested root",
            ));
        }
        Ok(status)
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
        self.admin()?
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
        crate::process::resume_parked_process(self.admin()?, &self.processes, process)
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
        self.admin()?
            .kill_invocation(&status.invocation_id())
            .await
            .map_err(refusal)?;
        Ok(EngineAck::Released)
    }

    async fn reconcile_parks(
        &self,
        parks: &dyn ParkRecoveryWriter,
        page: EnginePage,
    ) -> Result<ParkReconcileReport, EngineRefusal> {
        let admin = self.admin()?;
        let invocations = admin
            .paused_work_page(page.after.as_ref().map(|c| c.as_str()), page.limit)
            .await
            .map_err(refusal)?;
        let mut report = ParkReconcileReport::default();
        if invocations.len() == page.limit.get() {
            report.next = invocations
                .last()
                .map(|v| lash_core::engine::EngineCursor::new(v.id.clone()));
        }
        for invocation in invocations {
            let Some(key) = invocation.target_service_key.as_deref() else {
                report.unchanged += 1;
                continue;
            };
            if invocation.target_service_name == crate::LashService::SessionDriver.name() {
                admin
                    .resume_invocation(&invocation.invocation_id())
                    .await
                    .map_err(refusal)?;
                report.resumed_drives.push(key.into());
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
            } else if let Some((session, root)) = parse_turn_workflow_key(key) {
                let target = ParkTarget::Root {
                    session: session.clone(),
                    root: root.clone(),
                };
                let reason = crate::process::park_reconcile::exhausted_reason(&invocation);
                match parks
                    .record_engine_park(&target, reason, EnginePark::new(invocation.id.clone()))
                    .await
                    .map_err(refusal)?
                {
                    EngineParkRecorded::Parked(_) => report.parked.push(target),
                    EngineParkRecorded::AttachedToExisting(_) => report.attached += 1,
                    EngineParkRecorded::TargetTerminal | EngineParkRecorded::TargetGone => {
                        admin
                            .kill_invocation(&invocation.invocation_id())
                            .await
                            .map_err(refusal)?;
                        report.released.push(RootRef { session, root });
                    }
                }
            } else {
                report.unchanged += 1;
            }
        }
        Ok(report)
    }
}
