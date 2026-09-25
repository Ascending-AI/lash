//! The reference turn as a [`ConformanceTurnAttempt`](crate::ConformanceTurnAttempt):
//! the shape every crash-matrix law hands the tier's turn runner, so one law
//! body runs the scripted drain in process and inside a Restate handler.

use super::*;

/// How a reference drain ended, as its attempt reports it.
pub(super) type DrainReport =
    Result<crate::facade_support::QueuedTurnDrain<crate::AssembledTurn>, crate::RuntimeError>;

/// Arms a scenario's seam once the runtime is built, before the turn drives:
/// the runtime's own construction crosses seams the scenario must not see.
pub(super) type BeforeDrive = Arc<dyn Fn(&SeamControl) + Send + Sync>;

/// One reference drain run on the tier's runner: every execution of it
/// builds the reference runtime over the tier's host behind `seam`, arms the
/// seam, drains the reference ingress on the controller the runner lent, and
/// reports what the drain returned.
#[derive(Clone)]
pub(super) struct ReferenceTurn {
    pub(super) stores: Arc<dyn crate::StoreSet>,
    /// The scenario's store, undecorated: each execution wraps it in its
    /// seam.
    pub(super) store: Arc<dyn RuntimePersistence>,
    /// The law's one layered host (see [`LawSeamHost`]); each execution
    /// routes its layer to this turn's seam.
    pub(super) host: LawSeamHost,
    pub(super) identity: ReferenceIdentity,
    pub(super) seam: SeamLayer,
    pub(super) trace_tool: TraceTool,
    pub(super) lease_timings: crate::LeaseTimings,
    pub(super) before_drive: BeforeDrive,
    /// Where an execution that ends reports its drain. A crashing turn has
    /// none: it must never end.
    pub(super) reports: Option<tokio::sync::mpsc::UnboundedSender<DrainReport>>,
}

impl ReferenceTurn {
    /// The reference drain of `identity` over the scenario `store`, with a
    /// fresh seam counting into `executions`.
    pub(super) fn new(
        stores: &Arc<dyn crate::StoreSet>,
        store: Arc<dyn RuntimePersistence>,
        host: &LawSeamHost,
        identity: &ReferenceIdentity,
        control: SeamControl,
        executions: &Arc<std::sync::atomic::AtomicUsize>,
        lease_timings: crate::LeaseTimings,
    ) -> Self {
        Self {
            stores: Arc::clone(stores),
            store,
            host: host.clone(),
            identity: identity.clone(),
            seam: SeamLayer {
                control,
                executions: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                journal_faults: None,
            },
            trace_tool: TraceTool {
                executed: Arc::clone(executions),
                ..TraceTool::default()
            },
            lease_timings,
            before_drive: Arc::new(|_| {}),
            reports: None,
        }
    }

    pub(super) fn before_drive(
        mut self,
        before_drive: impl Fn(&SeamControl) + Send + Sync + 'static,
    ) -> Self {
        self.before_drive = Arc::new(before_drive);
        self
    }

    /// Arms the seam's journal faults, and the trace tool's, with `faults`.
    pub(super) fn journal_faults(
        mut self,
        faults: Option<lash_core::facade_support::effect_replay_driver::EffectJournalFaults>,
    ) -> Self {
        self.seam.journal_faults = faults.clone();
        self.trace_tool.journal_faults = faults;
        self
    }

    /// The attempt, and the channel its ending execution reports on.
    pub(super) fn reporting(
        mut self,
    ) -> (
        crate::ConformanceTurnAttempt,
        tokio::sync::mpsc::UnboundedReceiver<DrainReport>,
    ) {
        let (reports, reported) = tokio::sync::mpsc::unbounded_channel();
        self.reports = Some(reports);
        (self.attempt(), reported)
    }

    /// The attempt of a turn the law crashes: it never reports.
    #[expect(
        clippy::expect_used,
        reason = "conformance-law fixture: each result is established by the setup above"
    )]
    pub(super) fn attempt(self) -> crate::ConformanceTurnAttempt {
        let turn = Arc::new(self);
        Arc::new(move |scoped| {
            let turn = Arc::clone(&turn);
            Box::pin(async move {
                let store = SeamStore::wrap(Arc::clone(&turn.store), turn.seam.control.clone());
                turn.host.route_to(&turn.seam);
                let mut runtime = Box::pin(try_build_runtime_over_host(
                    turn.stores.as_ref(),
                    store,
                    turn.seam.control.clone(),
                    turn.host.host(),
                    &turn.identity,
                    turn.trace_tool.clone(),
                    turn.lease_timings,
                ))
                .await
                .expect("build the reference runtime");
                (turn.before_drive)(&turn.seam.control);
                let drain = Box::pin(runtime.stream_next_queued_work(crate::TurnOptions::new(
                    tokio_util::sync::CancellationToken::new(),
                    turn.seam.clone().over_scoped(scoped),
                )))
                .await;
                let end = crate::ConformanceTurnEnd::of(&drain);
                match &turn.reports {
                    Some(reports) => {
                        let _ = reports.send(drain);
                    }
                    None => panic!(
                        "the crashing reference turn ended before its crash: {:?}",
                        drain.map(crate::facade_support::QueuedTurnDrain::ran)
                    ),
                }
                end
            })
        })
    }
}

/// The report of the one execution of `reported` that ended.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub(super) async fn reported(
    mut reported: tokio::sync::mpsc::UnboundedReceiver<DrainReport>,
) -> DrainReport {
    reported
        .recv()
        .await
        .expect("the reference turn reported its drain")
}
