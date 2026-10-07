//! The production turn services (ADR 0132 §4; L3, FIG-5172): a session's
//! turns run through the turn driver in a runtime opened over the session's
//! committed head, with the deployment's plugins, models and stores.

use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use super::head::SessionHead;
use super::session::{
    AdmittedInputs, OpenTurn, TurnDrive, TurnError, TurnRestore, TurnRow, TurnServices,
};
use crate::runtime::logical_turn::LogicalTurnAdmissions;
use crate::runtime::turn_driver::{DriveParts, RuntimeDrive};
use crate::runtime::turn_loop::DurableTurn;
use crate::runtime::{LashRuntime, ObservationSource as _, TurnObserver};
use crate::{
    ActorContext, AdmittedScope, ExecutionBudgets, LiveReplayStore, SessionId, TurnActivity, TurnId,
};

/// The runtimes a node's sessions run in. A deployment implements it over
/// its own plugins, models and stores; each runtime it opens is at the
/// session's committed head and is used by one activation pass.
#[async_trait::async_trait]
pub trait SessionRuntimes: Send + Sync {
    /// A runtime of `session` at its committed head.
    ///
    /// # Errors
    ///
    /// [`TurnError`] when the session cannot be opened.
    async fn open(&self, session: &SessionId) -> Result<LashRuntime, TurnError>;

    /// Where the turns publish their live activity.
    fn live_replay(&self) -> Arc<dyn LiveReplayStore>;

    /// The budgets the deployment's turns run under.
    fn execution_budgets(&self) -> ExecutionBudgets;
}

/// [`TurnServices`] over a deployment's [`SessionRuntimes`].
pub struct RuntimeTurnServices {
    runtimes: Arc<dyn SessionRuntimes>,
}

impl std::fmt::Debug for RuntimeTurnServices {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RuntimeTurnServices")
            .finish_non_exhaustive()
    }
}

impl RuntimeTurnServices {
    /// The turn services of the sessions `runtimes` opens.
    #[must_use]
    pub fn new(runtimes: Arc<dyn SessionRuntimes>) -> Self {
        Self { runtimes }
    }

    /// Rebuild a turn's retained tool-call records from its durable rounds,
    /// in admission order. This reads every step under the turn's owner, so
    /// an owner change or a gap in live activity cannot drop earlier calls.
    /// Cell host operations are not catalog tool calls. Calls that settled
    /// without material have no retained request/output record and remain
    /// accounted for in the omitted-call summary.
    ///
    /// # Errors
    ///
    /// [`lash_durable::DurableError`] when the records do not read, fold or decode.
    pub async fn recorded_tool_calls(
        cx: &ActorContext,
        session: &SessionId,
        run: &TurnId,
    ) -> Result<
        (Vec<crate::ToolCallRecord>, Option<crate::OmittedToolCalls>),
        lash_durable::DurableError,
    > {
        use lash_core_execution::runtime::actor::round::{self, PolicyView};
        use lash_durable::domain::OwnerKey;

        let corrupt = |message| {
            lash_durable::DurableError::Store(lash_durable::StoreFailure {
                kind: lash_durable::StoreFailureKind::Corrupt,
                message,
            })
        };
        let owner = OwnerKey::Turn(session.clone(), run.clone());
        let rows = cx.durable_reads()?.run_records(&owner).await?;
        // Reporting folds only settled facts and never consults live policies
        // to run a recovery. The empty policy view executes nothing.
        let fold = round::fold(&rows, &PolicyView::new([]))
            .map_err(|error| corrupt(format!("the turn's tool records: {error}")))?;
        let mut calls = Vec::new();
        let mut omitted = None;
        for round in fold.rounds() {
            for member in round.members() {
                if member.draft().tool().as_str().starts_with("cell-host:") {
                    continue;
                }
                let Some(payload) = member.outcome().and_then(round::SettledOutput::payload) else {
                    let summary = omitted.get_or_insert_with(|| crate::OmittedToolCalls {
                        count: 0,
                        failures: 0,
                        attachments: Vec::new(),
                    });
                    summary.count += 1;
                    summary.failures += 1;
                    continue;
                };
                let completed = round::decode_completed(payload).ok_or_else(|| {
                    corrupt(format!(
                        "call {} has no readable tool answer",
                        member.call()
                    ))
                })?;
                if completed.call_id != *member.call() {
                    return Err(corrupt(format!(
                        "call {} has another call's recorded answer",
                        member.call(),
                    )));
                }
                calls.push(crate::ToolCallRecord {
                    call_id: completed.call_id,
                    provider_call_id: completed.provider_call_id,
                    tool: completed.tool_name,
                    args: completed.args,
                    output: completed.output,
                });
            }
        }
        Ok((calls, omitted))
    }

    /// Open `row`'s session and prepare its turn under the turn's own scope
    /// of `cx`.
    async fn prepare(
        &self,
        cx: &ActorContext,
        row: &TurnRow,
    ) -> Result<(DurableTurn, DriveParts), TurnError> {
        let mut runtime = self.runtimes.open(&row.session).await?;
        let admissions = admitted_rows(&runtime, row).await?;
        let settlement = crate::store::IngressSettlement {
            run: row.run.clone(),
            completed_inputs: admissions
                .turn_inputs
                .iter()
                .map(crate::AdmittedTurnInputs::completion)
                .collect(),
            completed_batches: admissions
                .queued
                .iter()
                .map(crate::AdmittedQueuedWork::completion)
                .collect(),
            released: Vec::new(),
            dropped: Vec::new(),
        };
        let controller = cx.scoped(AdmittedScope::turn(row.session.clone(), row.run.clone()))?;
        let live = self.runtimes.live_replay();
        let (observer, publisher) = live_observer(&runtime, &live);
        let turn = runtime
            .prepare_durable_turn(&controller, &row.run, admissions, &observer)
            .await?;
        Ok((
            turn,
            DriveParts {
                observer,
                settlement,
                live,
                publisher,
            },
        ))
    }
}

#[async_trait::async_trait]
impl TurnServices for RuntimeTurnServices {
    fn execution_budgets(&self, _session: &SessionId) -> ExecutionBudgets {
        self.runtimes.execution_budgets()
    }

    /// The runtime opens at the session's committed head itself; the owner's
    /// cached `head` is the same revision, which the store's compare-and-set
    /// on the turn's head commit checks.
    async fn start(
        &self,
        cx: &ActorContext,
        row: &TurnRow,
        _head: &SessionHead,
    ) -> Result<Box<dyn TurnDrive>, TurnError> {
        let (turn, parts) = self.prepare(cx, row).await?;
        Ok(Box::new(RuntimeDrive::start(turn, parts)?))
    }

    async fn resume(
        &self,
        cx: &ActorContext,
        restore: TurnRestore<'_>,
    ) -> Result<OpenTurn, TurnError> {
        let (turn, parts) = self.prepare(cx, restore.row()).await?;
        RuntimeDrive::resume(turn, parts, restore).await
    }

    async fn apply_commands(
        &self,
        cx: &ActorContext,
        admitted: &AdmittedInputs,
    ) -> Result<(), TurnError> {
        let session = SessionId::parse(cx.actor().id())
            .map_err(|error| TurnError::Exec(format!("session actor id: {error}")))?;
        let mut runtime = self.runtimes.open(&session).await?;
        let controller = cx.scoped(AdmittedScope::session_operation(
            session.clone(),
            "session-command",
        ))?;
        if admitted.admission.is_turn() {
            return Err(TurnError::Exec(format!(
                "run {} admits a turn, not a command",
                admitted.run
            )));
        }
        // The command drain reads the session's leading open command run,
        // the one the mail drain handed out, and applies it: its commit
        // settles the row.
        runtime
            .drain_next_session_command_with_cancellation(CancellationToken::new(), &controller)
            .await?;
        Ok(())
    }
}

/// The rows `row`'s run took, as its admission bound them.
async fn admitted_rows(
    runtime: &LashRuntime,
    row: &TurnRow,
) -> Result<LogicalTurnAdmissions, TurnError> {
    let admission = &row.admission;
    let store = runtime.services.store.clone().ok_or_else(|| {
        TurnError::Exec(format!(
            "session {} runs its turns over its store",
            row.session
        ))
    })?;
    let store_error = |error: crate::StoreError| TurnError::Runtime(error.runtime_error());
    let mut inputs = Vec::with_capacity(admission.input_ids().len());
    for input in admission.input_ids() {
        let read = store
            .pending_turn_input(input)
            .await
            .map_err(store_error)?
            .ok_or_else(|| {
                TurnError::Exec(format!(
                    "run {} took input {input}, which is no longer pending",
                    row.run
                ))
            })?;
        inputs.push(read.input);
    }
    let turn_inputs = if inputs.is_empty() {
        Vec::new()
    } else {
        vec![crate::AdmittedTurnInputs {
            session_id: row.session.clone(),
            mode: crate::TurnInputAdmissionMode::NextTurn,
            inputs,
            applications: Vec::new(),
        }]
    };
    let taken = admission.batch_ids();
    let queued = if taken.is_empty() {
        Vec::new()
    } else {
        let batches = store
            .list_queued_work()
            .await
            .map_err(store_error)?
            .into_iter()
            .filter(|batch| taken.contains(&batch.batch_id))
            .collect::<Vec<_>>();
        if batches.len() != taken.len() {
            return Err(TurnError::Exec(format!(
                "run {} took batches {taken:?}, not all of which are queued",
                row.run
            )));
        }
        vec![crate::AdmittedQueuedWork {
            session_id: row.session.clone(),
            batches,
        }]
    };
    Ok(LogicalTurnAdmissions::new(queued, turn_inputs))
}

/// The turn's observer, and the task that publishes its activity to `live`
/// at the session's head revision.
fn live_observer(
    runtime: &LashRuntime,
    live: &Arc<dyn LiveReplayStore>,
) -> (TurnObserver, tokio::task::JoinHandle<()>) {
    let sink = Arc::new(LiveActivity {
        live: Arc::clone(live),
        session: runtime.session_id().clone(),
        revision: crate::SessionRevision::from_runtime(runtime),
    });
    let (observer, mut observations) = TurnObserver::open(
        &crate::runtime::NoopEventSink,
        sink.as_ref(),
        runtime.delta_framing(),
    );
    let publisher = crate::task::spawn(async move {
        while let Some(observation) =
            std::future::poll_fn(|context| observations.poll_next(context)).await
        {
            crate::runtime::turn_loop::publish_observation(
                &crate::runtime::NoopEventSink,
                sink.as_ref(),
                observation,
            )
            .await;
            observations.published_one();
        }
    });
    (observer, publisher)
}

/// Publishes a turn's activity to the session's live stream.
struct LiveActivity {
    live: Arc<dyn LiveReplayStore>,
    session: SessionId,
    revision: crate::SessionRevision,
}

#[async_trait::async_trait]
impl crate::TurnActivitySink for LiveActivity {
    async fn emit(&self, activity: TurnActivity) {
        self.publish(None, activity).await;
    }

    async fn emit_for_turn(&self, turn_id: &TurnId, activity: TurnActivity) {
        self.publish(Some(turn_id), activity).await;
    }
}

impl LiveActivity {
    async fn publish(&self, turn: Option<&TurnId>, activity: TurnActivity) {
        if let Err(error) = self
            .live
            .publish(
                &self.session,
                self.revision,
                vec![crate::LiveReplayEventDraft::new(
                    turn,
                    crate::SessionObservationEventPayload::TurnActivity(activity),
                )],
            )
            .await
        {
            // The live stream is best effort: a reader that misses it sees a
            // gap and reads the store.
            tracing::warn!(session = %self.session, %error, "a turn's live activity was not published");
        }
    }
}
