//! The production turn services (ADR 0132 §4; L3, FIG-5172): a session's
//! turns run through the turn driver in a runtime opened over the session's
//! committed head, with the deployment's plugins, models and stores.

use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use super::commit_publication::{CommitBase, announce_head};
pub use super::commit_publication::{
    PublicationMark, PublicationMarks, PublicationWindow, PublishedHeads,
};
use super::head::SessionHead;
use super::session::{
    AdmittedInputs, OpenTurn, RecordedPreparation, TurnDrive, TurnError, TurnRestore, TurnRow,
    TurnServices,
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

    /// Materialize a turn's original call ledger in admission order. Cell
    /// reports retain references to the existing admission/outcome nodes;
    /// the offline protocol decoder reconstructs their exact typed arguments.
    /// Waits and consumed cell rows are unnecessary. A call with no completed
    /// material has no body and is skipped, never summarized as omitted.
    ///
    /// # Errors
    ///
    /// A durable read, fold or decoding refusal, preserving its typed cause.
    pub async fn recorded_tool_calls(
        cx: &ActorContext,
        session: &SessionId,
        run: &TurnId,
        decoders: &crate::transcript::TranscriptDecoders,
    ) -> Result<Vec<crate::ToolCallRecord>, lash_durable::DurableError> {
        use lash_durable::domain::OwnerKey;

        let owner = OwnerKey::Turn(session.clone(), run.clone());
        let facts = cx.durable_reads()?.run_facts(&owner).await?;
        Self::decode_recorded_tool_calls(&facts, decoders)
    }

    fn decode_recorded_tool_calls(
        facts: &lash_durable::domain::RunFacts,
        decoders: &crate::transcript::TranscriptDecoders,
    ) -> Result<Vec<crate::ToolCallRecord>, lash_durable::DurableError> {
        use lash_core_execution::runtime::actor::round::{self, PolicyView};
        let corrupt = |message| {
            lash_durable::DurableError::Store(lash_durable::StoreFailure {
                kind: lash_durable::StoreFailureKind::Corrupt,
                message,
            })
        };
        // Reporting folds only settled facts and never consults live policies
        // to run a recovery. The empty policy view executes nothing.
        let fold = round::fold(facts, &PolicyView::new([]), &round::PinnedWaits::ledger())
            .map_err(|error| corrupt(format!("the turn's tool records: {error}")))?;
        let mut calls = Vec::new();
        // Rounds and cell reports, in the order of the effects they answer.
        let mut runs: std::collections::BTreeMap<_, Result<_, _>> = fold
            .rounds()
            .map(|round| (round.run(), Ok(round)))
            .collect();
        runs.extend(
            fold.reports()
                .iter()
                .map(|(run, report)| (*run, Err(*report))),
        );
        for entry in runs.into_values() {
            let round = match entry {
                Ok(round) => round,
                Err(report) => {
                    let report = round::cell_report(facts, report, decoders)
                        .map_err(lash_durable::DurableError::Graph)?;
                    calls.extend(report);
                    continue;
                }
            };
            for member in round.members() {
                if member.draft().tool().as_str().starts_with("cell-host:") {
                    continue;
                }
                let Some(payload) = member.outcome().and_then(round::SettledOutput::payload) else {
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
        Ok(calls)
    }

    /// Read a named turn's settled bodies, plus its pending identities if unfinished,
    /// through its reports, current round outcomes and saved cell ledgers.
    pub async fn tool_call_bodies(
        cx: &ActorContext,
        session: &SessionId,
        run: &TurnId,
        decoders: &crate::transcript::TranscriptDecoders,
    ) -> Result<
        (
            Vec<crate::ToolCallRecord>,
            std::collections::BTreeSet<crate::ToolCallId>,
        ),
        lash_durable::DurableError,
    > {
        use lash_core_execution::runtime::actor::round;
        use lash_core_store::checkpoint_graph::GraphDecoder;
        use lash_durable::domain::{OwnerKey, RootOwner};
        let corrupt = |error: String| {
            lash_durable::DurableError::Store(lash_durable::StoreFailure {
                kind: lash_durable::StoreFailureKind::Corrupt,
                message: error,
            })
        };
        let reads = cx.durable_reads()?;
        if !reads
            .turn(session)
            .await?
            .is_some_and(|turn| turn.run == *run)
        {
            return Self::recorded_tool_calls(cx, session, run, decoders)
                .await
                .map(|records| (records, Default::default()));
        }
        let mut records = Vec::new();
        let roots = reads.cell_roots(session, run).await?;
        let mut owners = vec![OwnerKey::Turn(session.clone(), run.clone())];
        let mut pending = std::collections::BTreeSet::new();
        for root in roots {
            owners.push(root.exec.owner());
            let graph = reads
                .hydrate(
                    RootOwner::Exec(root.exec),
                    Some(root.root),
                    cx.backend().config().settings().graph_reads.owner(),
                )
                .await?;
            let mut decoder = GraphDecoder::new(
                &graph,
                cx.backend().config().settings().graph_reads.execution(),
            );
            let (saved, open) = decoders
                .decode_cell_ledger(&mut decoder, root.root)
                .map_err(lash_durable::DurableError::Graph)?;
            records.extend(saved);
            pending.extend(open);
        }
        for owner in owners {
            let facts = reads.run_facts(&owner).await?;
            records.extend(Self::decode_recorded_tool_calls(&facts, decoders)?);
            let fold = round::fold(
                &facts,
                &round::PolicyView::new([]),
                &round::PinnedWaits::ledger(),
            )
            .map_err(|error| corrupt(error.to_string()))?;
            for member in fold.rounds().flat_map(|round| round.members()) {
                if member.draft().tool().as_str().starts_with("cell-host:") {
                    continue;
                }
                if member
                    .outcome()
                    .and_then(round::SettledOutput::payload)
                    .is_none()
                {
                    pending.insert(member.call().clone());
                }
            }
        }
        for record in &records {
            pending.remove(&record.call_id);
        }
        Ok((records, pending))
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
        mut restore: TurnRestore<'_>,
    ) -> Result<OpenTurn, TurnError> {
        let recorded = restore.recorded_preparation().await?;
        let (turn, parts) = self.prepare(cx, restore.row(), Some(recorded)).await?;
        RuntimeDrive::resume(cx, turn, parts, restore).await
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

    async fn carry_session(&self, cx: &ActorContext, session: &SessionId) -> Result<(), TurnError> {
        let mut runtime = self.runtimes.open(session).await?;
        let controller = cx.scoped(AdmittedScope::session_operation(
            session.clone(),
            "session-carry",
        ))?;
        let commit = CommitBase::of(&runtime);
        // The carry commits once, as its last step: a reader that finds the
        // head past this base waits for the publication below (FIG-5605).
        let _committing = commit
            .as_ref()
            .map(|commit| self.published.committing(commit));
        runtime
            .carry_session_state(&controller, cx.backend().formats().session().clone())
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
        let started_at_ms = tracing.clock().timestamp_ms();
        let candidate = tracing.scopes().propose(&scope, &cause, started_at_ms);
        Some(lash_core_execution::runtime::actor::round::TraceProposal {
            scope: lash_trace::DurableTraceScope {
                scope,
                cause,
                anchor: candidate.anchor(),
                started_at_ms,
            },
            candidate,
        })
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
