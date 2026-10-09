//! The production turn services (ADR 0132 §4; L3, FIG-5172): a session's
//! turns run through the turn driver in a runtime opened over the session's
//! committed head, with the deployment's plugins, models and stores.

use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use super::commit_publication::{CommitBase, announce_head};
pub use super::commit_publication::{
    PublicationMark, PublicationMarks, PublicationWindow, PublishedHeads,
};
use super::head::{HeadCache, SessionHead};
use super::session::{
    AdmittedInputs, CellToolCalls, OpenTurn, RecordedPreparation, TurnDrive, TurnError,
    TurnRestore, TurnRow, TurnServices, UnfinishedPhase,
};
use super::session_mail::{InputAdmission, InputBatching, SessionMailError};
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

    /// The trace runtime the deployment's turns are admitted and traced
    /// under.
    fn tracing(&self) -> &crate::trace::TraceRuntime;
}

/// [`TurnServices`] over a deployment's [`SessionRuntimes`].
pub struct RuntimeTurnServices {
    runtimes: Arc<dyn SessionRuntimes>,
    /// The session heads this node published to the live stream.
    published: Arc<PublishedHeads>,
}

impl std::fmt::Debug for RuntimeTurnServices {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RuntimeTurnServices")
            .finish_non_exhaustive()
    }
}

impl RuntimeTurnServices {
    /// The turn services of the sessions `runtimes` opens, which mark the
    /// commits they are publishing in `published`: the deployment's session
    /// feeds read the same marks.
    #[must_use]
    pub fn new(runtimes: Arc<dyn SessionRuntimes>, published: Arc<PublishedHeads>) -> Self {
        Self {
            runtimes,
            published,
        }
    }

    /// Rebuild a turn's retained tool-call records from its durable rounds,
    /// including protocol-refused calls settled before dispatch, in admission
    /// order. This reads every step under the turn's owner, so
    /// an owner change or a gap in live activity cannot drop earlier calls.
    /// A code cell's tool calls are among them: the turn records them as a
    /// settled round of its own once the cell answers, or when the turn ends
    /// on the cell (FIG-5330). That round is bounded by the code executor:
    /// the calls its bound left out are in the omitted-call summary.
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
        let reads = cx.durable_reads()?;
        let rows = reads.run_records(&owner).await?;
        let waits = round::PinnedWaits::read(reads, &rows).await?;
        // Reporting folds only settled facts and never consults live policies
        // to run a recovery. The empty policy view executes nothing.
        let fold = round::fold(&rows, &PolicyView::new([]), &waits)
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
                if let Some(left_out) =
                    super::tool_round::omitted_cell_calls(member.draft().tool(), &completed)
                {
                    super::session::add_omitted(&mut omitted, left_out);
                    continue;
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
    /// of `cx`, and the trace scope its admission retained. A resumed turn
    /// passes what its checkpoint recorded, which preparation serves: its
    /// before-turn decisions instead of running the callbacks again, and its
    /// trace scope.
    async fn prepare(
        &self,
        cx: &ActorContext,
        row: &TurnRow,
        recorded: Option<RecordedPreparation>,
    ) -> Result<(DurableTurn, DriveParts), TurnError> {
        let mut runtime = self.runtimes.open(&row.session).await?;
        let _prepared = crate::runtime::turn_driver::TurnPhaseSpan::begin(
            runtime.turn_phase_probe.clone(),
            crate::runtime::RuntimeTurnPhase::PreparedTurn,
        );
        let admissions = admitted_rows(&runtime, row).await?;
        let controller = cx.scoped(AdmittedScope::turn(row.session.clone(), row.run.clone()))?;
        let live = self.runtimes.live_replay();
        let commit = CommitBase::of(&runtime);
        let revision = crate::SessionRevision::from_runtime(&runtime);
        let (observer, publisher) = live_observer(&runtime, &live, &row.run);
        // The run starts from the head the runtime opened at: what it
        // changes from here is what its phases record (FIG-5301).
        runtime.services.plugins.begin_run();
        let admitted_trace = row.admission.trace().cloned();
        let turn = runtime
            .prepare_durable_turn(
                &controller,
                &row.run,
                admissions,
                recorded,
                admitted_trace,
                &observer,
            )
            .await?;
        // The turn's commit settles its inputs and queued work from the
        // driver once it finishes: those its run took, the inputs with the
        // application evidence preparation recorded (FIG-5288), and those
        // its checkpoints delivered (FIG-5293, FIG-5294).
        let settlement = crate::store::IngressSettlement::new(row.run.clone());
        Ok((
            turn,
            DriveParts {
                observer,
                settlement,
                live,
                revision,
                publisher,
                commit,
                published: Arc::clone(&self.published),
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
        let (turn, parts) = self.prepare(cx, row, None).await?;
        Ok(Box::new(RuntimeDrive::start(turn, parts)?))
    }

    async fn resume(
        &self,
        cx: &ActorContext,
        restore: TurnRestore<'_>,
    ) -> Result<OpenTurn, TurnError> {
        let recorded = restore.recorded_preparation()?;
        let (turn, parts) = self.prepare(cx, restore.row(), Some(recorded)).await?;
        RuntimeDrive::resume(cx, turn, parts, restore).await
    }

    async fn stopped_cell_calls(
        &self,
        cx: &ActorContext,
        row: &TurnRow,
        heads: &mut HeadCache,
    ) -> Result<Option<(crate::EffectId, CellToolCalls)>, TurnError> {
        use lash_durable::domain::{OwnerKey, RunSeq};

        let UnfinishedPhase::Tools { run, .. } = &row.phase else {
            return Ok(None);
        };
        // A round the turn's rows hold at its phase is a tool round, whose
        // members are recorded already.
        let owner = OwnerKey::Turn(row.session.clone(), row.run.clone());
        let rows = cx.durable_reads()?.run_records(&owner).await?;
        if rows.iter().any(|stored| stored.run == *run) {
            return Ok(None);
        }
        // The restored machine re-delivers the cell, which names its
        // snapshot. A turn its committed state cannot prepare ran no cell
        // this owner can read; its cancel still ends it.
        let turn = match self.resume(cx, TurnRestore::new(cx, row, heads)).await {
            Ok(turn) => turn,
            Err(TurnError::Runtime(refusal)) if refusal.is_terminal() => return Ok(None),
            Err(error) => return Err(error),
        };
        let OpenTurn {
            mut drive, pending, ..
        } = turn;
        let redelivered = pending.or_else(|| drive.machine().poll_effect());
        let Some(crate::Effect::ExecCode { id, .. }) = redelivered else {
            return Ok(None);
        };
        if RunSeq(id.0) != *run {
            return Ok(None);
        }
        let calls = drive.stopped_cell_calls(cx, id).await?;
        Ok((!calls.is_empty()).then_some((id, calls)))
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
        // settles the row, and is published once it is acknowledged.
        let commit = CommitBase::of(&runtime);
        // A command commits once, as its last step: a reader that finds the
        // head past this base waits for the publication below (FIG-5605).
        let _committing = commit
            .as_ref()
            .map(|commit| self.published.committing(commit));
        runtime
            .drain_next_session_command_with_cancellation(CancellationToken::new(), &controller)
            .await?;
        if let Some(commit) = commit {
            commit
                .publish(self.runtimes.live_replay().as_ref(), &self.published, None)
                .await;
        }
        Ok(())
    }

    fn input_batching(&self) -> &dyn InputBatching {
        self
    }

    fn propose_turn_trace(
        &self,
        cx: &ActorContext,
        run: &TurnId,
    ) -> Option<lash_core_execution::runtime::actor::round::TraceProposal> {
        let session = SessionId::parse(cx.actor().id()).ok()?;
        let tracing = self.runtimes.tracing();
        let scope = lash_trace::TraceScopeId::admission(lash_trace::TraceScopeOwner::Turn {
            session_id: session,
            turn_id: run.clone(),
        });
        // A session's turn is its own root: nothing parents its admission.
        let cause = lash_trace::TraceCause::Root;
        let candidate = tracing.scopes().propose(&scope, &cause);
        Some(lash_core_execution::runtime::actor::round::TraceProposal {
            scope: lash_trace::DurableTraceScope {
                scope,
                cause,
                anchor: candidate.anchor(),
                started_at_ms: tracing.clock().timestamp_ms(),
            },
            candidate,
        })
    }

    fn export_turn_admission(&self, scope: &lash_trace::DurableTraceScope) {
        self.runtimes.tracing().scopes().export_admitted(scope);
    }

    fn tracing(&self) -> Option<&crate::trace::TraceRuntime> {
        Some(self.runtimes.tracing())
    }

    async fn announce_head(&self, cx: &ActorContext, session: &SessionId) {
        announce_head(
            cx.backend(),
            self.runtimes.live_replay().as_ref(),
            &self.published,
            session,
        )
        .await;
    }
}

/// The host's batching, as the session's runtime holds it, over the
/// session's model.
#[async_trait::async_trait]
impl InputBatching for RuntimeTurnServices {
    async fn input_admission(
        &self,
        _cx: &ActorContext,
        session: &SessionId,
    ) -> Result<Option<InputAdmission>, SessionMailError> {
        let runtime = self
            .runtimes
            .open(session)
            .await
            .map_err(|error| SessionMailError::Batching(error.to_string()))?;
        Ok(runtime.input_admission())
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
    Ok(LogicalTurnAdmissions::new(turn_inputs))
}

/// The turn's observer, and the task that publishes its activity to `live`
/// at the session's head revision. Every activity the turn publishes is
/// addressed to its run `turn`, so a host following the run adopts it: its
/// tool calls, prose and progress stream live as provisional activity, which
/// the turn's commit settles (ADR 0002), while its terminal waits for that
/// commit (ADR 0122).
fn live_observer(
    runtime: &LashRuntime,
    live: &Arc<dyn LiveReplayStore>,
    turn: &TurnId,
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
    let observer = observer.for_turn(turn);
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
            tracing::warn!(session_id = %self.session, %error, "a turn's live activity was not published");
        }
    }
}
