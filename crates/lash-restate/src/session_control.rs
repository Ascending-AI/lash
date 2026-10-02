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
    EngineAck, EngineCursor, EnginePage, EngineParkRecorded, EngineRefusal, OpenRoot,
    ParkReconcileReport, ParkRecoveryWriter, ParkTarget, RootRef, RootRunLoss,
    SessionControlEngine, StalledExecution,
};
use lash_core::store::{EnginePark, RootExecutor};

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
    /// The generation a lost root segment is resubmitted under.
    pub(crate) generation: lash_core::engine::EngineGeneration,
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
/// - No lane holds a run of the key: the pass judges the root by the
///   execution its recorded admission names ([`RootExecutor`], FIG-4403),
///   never by the root's name.
///   - Its own run ([`RootExecutor::Root`]): the run was purged or its
///     history lost after the root was admitted (FIG-4281). The root
///     started, and a fresh execution would run its effects again under an
///     empty journal, so the store ends it ([`RootRunLoss::NoRun`]).
///   - A process's run ([`RootExecutor::Inline`] under a process scope):
///     the process drives the root inline and no lane ever holds a run of
///     its key. While the process is live the pass leaves the root, and the
///     lost-process pass owns the process's run. A terminal process runs
///     nothing more, so the root ends ([`RootRunLoss::NoRun`]).
///   - Another execution's drive: an in-process drive the engine holds no
///     run of. Its absence from every lane proves nothing, so the pass
///     leaves the root.
///   - No recorded admission: the root started nothing; its ingress
///     obligation still owns its input and drives it, so the pass leaves it.
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
        page.last().map(|open| open.target.clone())
    } else {
        None
    };
    if page.is_empty() {
        return Ok(pass);
    }
    let keys: Vec<String> = page
        .iter()
        .map(|open| turn_workflow_key(&open.target.session, &open.target.root))
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
    for (OpenRoot { target, executor }, key) in page.iter().zip(&keys) {
        let key_runs: Vec<&crate::RestateInvocationStatus> = runs
            .iter()
            .filter(|run| run.target_service_key.as_deref() == Some(key.as_str()))
            .collect();
        let loss = if key_runs.is_empty() {
            let lost = match executor {
                Some(RootExecutor::Root) => Ok(true),
                Some(RootExecutor::Acceptor {
                    scope: lash_core::ExecutionScope::Process { process_id },
                }) => recovery_request(deadline, process_ended(processes, process_id))
                    .await
                    .map_err(|error| error.to_string()),
                Some(RootExecutor::Acceptor { .. } | RootExecutor::Inline { .. }) | None => {
                    Ok(false)
                }
            };
            match lost {
                Ok(true) => RootRunLoss::NoRun,
                Ok(false) => {
                    pass.unchanged += 1;
                    continue;
                }
                Err(error) => {
                    pass.failed.push((key.clone(), error));
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
                // The lost run's open usage runs can never settle: its
                // settlements, if any, precede this on the owner's object.
                if let Err(error) = recovery_request(
                    deadline,
                    crate::usage_accounting::retire_root_usage(
                        ingress,
                        namespace,
                        &target.session,
                        &target.root,
                    ),
                )
                .await
                {
                    pass.failed
                        .push((key.clone(), format!("retire root usage: {error}")));
                }
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

/// Whether process `process_id`, whose run a root's admission recorded as
/// its executor, runs nothing more (FIG-4403): its record is terminal, or
/// the registry holds none.
///
/// A process drives the roots its drive admits inline, in its own
/// `LashProcessWorkflow` run: its child turn's root and every root admitted
/// ahead of it in the session. While the process is live the lost-process
/// pass owns that run, so a root it runs is never judged lost here.
async fn process_ended(
    processes: &Arc<dyn lash_core::ProcessRegistry>,
    process_id: &lash_core::ProcessId,
) -> Result<bool, lash_core::PluginError> {
    Ok(processes
        .get_process(process_id)
        .await?
        .is_none_or(|record| record.is_terminal()))
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

/// An error an engine control verb met, as the refusal it answers: the
/// cause's own retry class and code, never its text alone.
pub(crate) trait ControlFailure {
    fn into_refusal(self) -> EngineRefusal;
}

/// Restate answered, or did not: the client's own classification decides
/// whether asking again can succeed.
impl ControlFailure for crate::RestateHttpError {
    fn into_refusal(self) -> EngineRefusal {
        let code = lash_core::RuntimeErrorCode::EngineControlRequest;
        match self.classification() {
            crate::RestateHttpErrorClass::Transient => {
                EngineRefusal::retryable(code, self.to_string())
            }
            crate::RestateHttpErrorClass::Terminal => {
                EngineRefusal::permanent(code, self.to_string())
            }
        }
    }
}

impl ControlFailure for lash_core::StoreError {
    fn into_refusal(self) -> EngineRefusal {
        self.into()
    }
}

impl ControlFailure for lash_core::PluginError {
    fn into_refusal(self) -> EngineRefusal {
        self.into()
    }
}

/// The recovery page's budget ran out before the request answered: the next
/// page asks again.
impl ControlFailure for tokio::time::error::Elapsed {
    fn into_refusal(self) -> EngineRefusal {
        EngineRefusal::retryable(
            lash_core::RuntimeErrorCode::EngineControlRequest,
            "recovery page time budget exhausted",
        )
    }
}

impl<E: ControlFailure + std::fmt::Display> ControlFailure for RecoveryRequestError<E> {
    fn into_refusal(self) -> EngineRefusal {
        match self {
            Self::Failed(error) => error.into_refusal(),
            Self::BudgetExhausted => EngineRefusal::retryable(
                lash_core::RuntimeErrorCode::EngineControlRequest,
                self.to_string(),
            ),
        }
    }
}

impl ControlFailure for EngineRefusal {
    fn into_refusal(self) -> EngineRefusal {
        self
    }
}

pub(crate) fn refusal(error: impl ControlFailure) -> EngineRefusal {
    error.into_refusal()
}

/// The refusal of a drive ask Restate did not accept: the cause keeps its
/// code and retry class, and the message names the drive it refused.
pub(crate) fn unaccepted_drive(
    session: &lash_core::SessionId,
    request: &lash_core::engine::DriveRequestId,
    mut cause: EngineRefusal,
) -> EngineRefusal {
    cause.message = format!(
        "drive `{}` of session `{session}` was not accepted: {}",
        request.as_str(),
        cause.message
    );
    cause
}

/// No registry row carries the process a control verb named: nothing a
/// retry of the verb can resume.
fn process_gone(process: &lash_core::ProcessId) -> EngineRefusal {
    EngineRefusal::permanent(
        lash_core::RuntimeErrorCode::ProcessNotVisible,
        format!("process `{process}` is gone"),
    )
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
            &self.generation,
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
            return Err(EngineRefusal::permanent(
                lash_core::RuntimeErrorCode::EngineHandleMismatch,
                "stored engine handle does not name a run of the requested root's session",
            ));
        }
        Ok(status)
    }

    /// Settle one paused invocation of the listing: resume an admission-only
    /// drive, park or release a root's execution or the group child it waits
    /// on, or hand a process to its registry's reconcile.
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
            let session = lash_core::SessionId::parse(key.as_str())
                .map_err(|error| EngineRefusal::from(lash_core::StoreError::from(error)))?;
            self.reconcile_drive(parks, invocation, session, report)
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
        } else if service == Some(crate::LashService::EffectGroupDispatch) {
            self.reconcile_group_work(parks, invocation, &key, report)
                .await?;
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
                    crate::usage_accounting::retire_root_usage(
                        &self.ingress,
                        &self.namespace,
                        &session,
                        &root,
                    )
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

    /// What the group still needs of this paused dispatcher invocation, read
    /// from its index.
    async fn group_work_need(
        &self,
        group_key: &str,
        invocation: &crate::ingress::RestatePausedInvocation,
    ) -> Result<crate::effect_group::EffectGroupOpenerResponse, EngineRefusal> {
        self.ingress
            .call_lash_object(
                &self
                    .namespace
                    .stable(crate::LashService::EffectGroupState)
                    .name(),
                group_key,
                "opener",
                &crate::effect_group::EffectGroupOpenerRequest {
                    handler: invocation.target_handler_name.clone(),
                    invocation_id: invocation.id.clone(),
                },
            )
            .await
            .map_err(refusal)
    }

    /// Stopped group preparation, a child or retirement the group still
    /// needs parks the scope it runs for with its typed cause: a process
    /// through its registry, a root with the invocation's handle recorded on
    /// its park, which is what the park's redrive resumes (FIG-4630). A
    /// terminal owner releases stopped work instead of acquiring another
    /// park, and so does a group that needs the work no more: a child whose
    /// position is seated, or work of a retired group, would otherwise stay
    /// paused for good.
    async fn reconcile_group_work(
        &self,
        parks: &dyn ParkRecoveryWriter,
        invocation: crate::ingress::RestatePausedInvocation,
        group_key: &str,
        report: &mut ParkReconcileReport,
    ) -> Result<(), EngineRefusal> {
        let opener = match self.group_work_need(group_key, &invocation).await? {
            crate::effect_group::EffectGroupOpenerResponse::Needed { opener } => {
                opener.scope().clone()
            }
            crate::effect_group::EffectGroupOpenerResponse::Seated => {
                self.admin
                    .kill_invocation(&invocation.invocation_id())
                    .await
                    .map_err(refusal)?;
                tracing::warn!(
                    group_key,
                    handler = invocation.target_handler_name.as_str(),
                    invocation = invocation.id.as_str(),
                    event = "effect_group.work.released",
                    "paused group work nothing waits for is released: its position is seated \
                     or its group is retired"
                );
                report.released_work.push(EnginePark::new(invocation.id));
                return Ok(());
            }
        };
        if let lash_core::ExecutionScope::Process { process_id } = &opener {
            let pass = crate::process::park_reconcile::reconcile_process_group_work(
                &self.admin,
                &self.processes,
                &self.continuations,
                &invocation,
                process_id,
            )
            .await
            .map_err(refusal)?;
            report.parked.extend(
                pass.parked
                    .into_iter()
                    .map(|process| ParkTarget::Process { process }),
            );
            report.unchanged += pass.unchanged;
            return Ok(());
        }
        let lash_core::ExecutionScope::Turn {
            session_id: session,
            turn_id: root,
        } = opener
        else {
            report.unchanged += 1;
            return Ok(());
        };
        let target = ParkTarget::RootChild {
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
                crate::usage_accounting::retire_root_usage(
                    &self.ingress,
                    &self.namespace,
                    &session,
                    &root,
                )
                .await
                .map_err(refusal)?;
                report.released.push(RootRef { session, root });
            }
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

    /// Resume the stopped group work a root's park recorded: exactly the
    /// invocations `children` names, each read on its own (FIG-4630). No
    /// listing is read and no group index is asked, so work of another root,
    /// session or deployment is never touched and can never fail this
    /// redrive. A handle whose invocation is no longer paused moved on since
    /// it was recorded and is skipped. Whether any was resumed.
    async fn resume_recorded_work(&self, children: &[EnginePark]) -> Result<bool, EngineRefusal> {
        let mut resumed = false;
        for child in children {
            let id = RestateInvocationId::new(child.as_str().to_owned());
            let Some(status) = self.admin.invocation_status(&id).await.map_err(refusal)? else {
                continue;
            };
            if status.status != crate::ingress::RestateInvocationLifecycle::Paused {
                continue;
            }
            if !self
                .namespace
                .parse(&status.target_service_name)
                .is_some_and(|route| route.service() == crate::LashService::EffectGroupDispatch)
            {
                return Err(EngineRefusal::permanent(
                    lash_core::RuntimeErrorCode::EngineHandleMismatch,
                    "stored child handle does not name group work of this deployment",
                ));
            }
            self.admin.resume_invocation(&id).await.map_err(refusal)?;
            resumed = true;
        }
        Ok(resumed)
    }

    /// Resume all paused dispatcher work a process's groups still need. A
    /// process's own segment can still be running while it waits for this
    /// work, so the child redrive starts the registry's parked rerun itself.
    /// A group whose index cannot be read is another owner's until it
    /// answers: it is skipped, never this redrive's failure.
    async fn resume_process_group_work(
        &self,
        process_id: &lash_core::ProcessId,
    ) -> Result<bool, EngineRefusal> {
        let scope = lash_core::ExecutionScope::process(process_id.clone());
        let paused = self
            .admin
            .paused_group_work(&self.namespace)
            .await
            .map_err(refusal)?;
        let mut resumed = false;
        for invocation in &paused {
            let Some(group_key) = invocation.target_service_key.as_deref() else {
                continue;
            };
            match self.group_work_need(group_key, invocation).await {
                Ok(crate::effect_group::EffectGroupOpenerResponse::Needed { opener })
                    if *opener.scope() == scope => {}
                Ok(_) => continue,
                Err(error) => {
                    tracing::warn!(
                        group_key,
                        invocation = invocation.id.as_str(),
                        %error,
                        event = "effect_group.work.unread",
                        "a paused group invocation's index could not be read; a process \
                         redrive skips it"
                    );
                    continue;
                }
            }
            if !resumed {
                let record = self
                    .processes
                    .get_process(process_id)
                    .await
                    .map_err(refusal)?
                    .ok_or_else(|| process_gone(process_id))?;
                let authority = crate::process::park_reconcile::execution_authority(&record)
                    .ok_or_else(|| {
                        EngineRefusal::permanent(
                            lash_core::RuntimeErrorCode::MissingProcessExecutionId,
                            format!("process `{process_id}` execution is not started"),
                        )
                    })?;
                self.processes
                    .begin_parked_rerun_with_authority(process_id, &authority)
                    .await
                    .map_err(refusal)?;
            }
            self.admin
                .resume_invocation(&invocation.invocation_id())
                .await
                .map_err(refusal)?;
            resumed = true;
        }
        Ok(resumed)
    }
}

#[async_trait::async_trait]
impl SessionControlEngine for RestateSessionControl {
    /// The session's wait index wakes every turn wait that registered a
    /// hand-over for `generation` (FIG-4739): each woken wait's invocation
    /// journals the wake and ends its turn at a segment boundary.
    async fn hand_over_turns(
        &self,
        session: &lash_core::SessionId,
        generation: &lash_core::engine::BuildGeneration,
    ) -> Result<(), EngineRefusal> {
        let woken: u64 = self
            .ingress
            .call_lash_object(
                &self
                    .namespace
                    .stable(crate::LashService::DurableWaitRegistry)
                    .name(),
                session.as_str(),
                "hand_over_turns",
                &crate::durable_wait::RestateDurableWaitHandOverRequest {
                    generation: generation.clone(),
                },
            )
            .await
            .map_err(refusal)?;
        if woken > 0 {
            tracing::info!(
                target: "lash::restate",
                event = "restate.turn_hand_over_woken",
                session_id = session.as_str(),
                generation = generation.as_str(),
                woken,
                "the drain woke a session's parked turn waits to hand over"
            );
        }
        Ok(())
    }

    async fn resume_root(
        &self,
        target: &RootRef,
        handle: Option<&EnginePark>,
        children: &[EnginePark],
    ) -> Result<EngineAck, EngineRefusal> {
        // A redrive resumes the root's stopped execution, the stopped group
        // work its park recorded, and the session's drive stopped behind the
        // park (ADR 0109 §3): the only resume a paused drive gets.
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
        let children = self.resume_recorded_work(children).await?;
        let drive = self.resume_session_drives(&target.session).await?;
        Ok(if root || children || drive {
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
            .ok_or_else(|| process_gone(process))?;
        let current = record.park().ok_or_else(|| {
            EngineRefusal::permanent(
                lash_core::RuntimeErrorCode::ProcessNotParked,
                format!("process `{process}` is not parked"),
            )
        })?;
        if current.park_id != park {
            return Err(EngineRefusal::permanent(
                lash_core::RuntimeErrorCode::ProcessParkSuperseded,
                format!(
                    "process `{process}` park {park} was superseded by park {}",
                    current.park_id
                ),
            ));
        }
        let work = self.resume_process_group_work(process).await?;
        let segment = crate::process::park_reconcile::resume_process_invocation(
            &self.admin,
            &self.namespace,
            &record,
        )
        .await
        .map_err(refusal)?
        .is_some();
        Ok(if work || segment {
            EngineAck::Resumed
        } else {
            EngineAck::NothingHeld
        })
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
        // A killed execution recalls none of the settlements it sent; what it
        // admitted and never recorded is resolved `unknown(execution_ended)`
        // behind them (ADR 0125). Idempotent, so a release with nothing held
        // retires nothing.
        crate::usage_accounting::retire_root_usage(
            &self.ingress,
            &self.namespace,
            &target.session,
            &target.root,
        )
        .await
        .map_err(refusal)?;
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
