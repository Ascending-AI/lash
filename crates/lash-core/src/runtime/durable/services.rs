//! The production turn services (ADR 0132 §4; L3, FIG-5172): a session's
//! turns run through the turn driver in a runtime opened over the session's
//! committed head, with the deployment's plugins, models and stores.

use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use super::commit_publication::CommitBase;
pub use super::commit_publication::{
    PublicationMark, PublicationMarks, PublicationWindow, PublishedHeads,
};
use super::head::SessionHead;
use super::session::{
    AdmittedInputs, OpenTurn, RecordedPreparation, TurnDrive, TurnError, TurnRestore, TurnRow,
    TurnServices,
};
use super::session_mail::{InputAdmission, InputBatching, SessionMailError};
use crate::ActorContext;
use crate::AdmittedScope;
use crate::ExecutionBudgets;
use crate::LiveReplayStore;
use crate::TurnActivity;
use crate::runtime::logical_turn::LogicalTurnAdmissions;
use crate::runtime::turn_driver::{DriveParts, RuntimeDrive};
use crate::runtime::turn_loop::{DurableTurn, TurnStart};
use crate::runtime::{LashRuntime, ObservationSource as _, TurnObserver};
use lash_vocabulary::identity::SessionId;
use lash_vocabulary::identity::TurnId;

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

    /// Construct directly from the claim's acknowledged state. External
    /// implementors may use the cold adapter until they implement retention.
    async fn open_retained(
        &self,
        _cx: &ActorContext,
        head: &SessionHead,
    ) -> Result<LashRuntime, TurnError> {
        self.open(&head.state().session_id).await
    }

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

    /// Read the committed turn named by its durable identity on any node.
    /// Its terminal supplies the commit revision; one existing committed-turn
    /// page reads precisely that receipt and its retained transcript, then the
    /// turn's call transcript supplies its call bodies.
    ///
    /// # Errors
    ///
    /// A typed store, ledger or transcript-decoding refusal.
    pub async fn committed_turn(
        cx: &ActorContext,
        store: &crate::store::SessionStore,
        turn: &TurnId,
        decoders: &crate::transcript::TranscriptDecoders,
    ) -> Result<
        Option<crate::transcript::CommittedTurnWithCalls>,
        crate::plugin::CommittedTurnReadError,
    > {
        let session = store.session_id();
        let Some(terminal) = store.run_terminal(turn).await? else {
            return Ok(None);
        };
        let crate::store::RunTerminalCause::Committed {
            turn: committed, ..
        } = terminal.cause
        else {
            return Ok(None);
        };
        let corrupt = |message: &str| {
            lash_execution_contracts::plugin_error::PluginError::StoredDataCorrupt {
                record_kind: "CommittedTurn".to_owned(),
                message: message.to_owned(),
            }
        };
        if committed != *turn {
            return Err(corrupt("the run terminal names another turn").into());
        }
        let revision = terminal
            .head_revision
            .and_then(|revision| revision.checked_sub(1))
            .ok_or_else(|| corrupt("the committed turn has no head revision"))?;
        let after = crate::store::CommittedTurnCursor::new(session.clone(), revision);
        let page = store
            .load_committed_turns(Some(&after), std::num::NonZeroU32::MIN)
            .await?;
        let mut page =
            crate::transcript::CommittedTurnsPage::decode(page, decoders).map_err(|error| {
                lash_execution_contracts::plugin_error::PluginError::StoredDataCorrupt {
                    record_kind: error.record_kind,
                    message: error.message,
                }
            })?;
        let read = page
            .turns
            .pop()
            .ok_or_else(|| corrupt("the committed turn has no receipt"))?;
        if read.turn_id != *turn {
            return Err(corrupt("the commit revision names another turn").into());
        }
        let tool_calls = Self::settled_tool_calls(cx, session, turn, decoders).await?;
        Ok(Some(crate::transcript::CommittedTurnWithCalls {
            model_calls: cx
                .durable_reads()?
                .model_calls(
                    session,
                    &lash_durable::domain::ModelCallId::turn_owner(turn),
                )
                .await?,
            turn: read,
            tool_calls,
        }))
    }

    /// The calls an open turn answered so far, in the order its runs
    /// answered them: the derivation its terminal's transcript records
    /// ([`TurnTranscript::of`]), over its ledger as it stands. Cell reports
    /// retain references to the existing admission/outcome nodes; the
    /// offline protocol decoder reconstructs their exact typed arguments. A
    /// call with no completed material has no body and is skipped, never
    /// summarized as omitted.
    ///
    /// # Errors
    ///
    /// A durable read, ledger or decoding refusal, preserving its typed
    /// cause.
    ///
    /// [`TurnTranscript::of`]: lash_core_execution::runtime::actor::round::TurnTranscript::of
    pub async fn ledger_tool_calls(
        cx: &ActorContext,
        session: &SessionId,
        run: &TurnId,
        decoders: &crate::transcript::TranscriptDecoders,
    ) -> Result<Vec<crate::ToolCallRecord>, lash_durable::DurableError> {
        use lash_durable::domain::OwnerKey;

        let owner = OwnerKey::Turn(session.clone(), run.clone());
        let facts = cx.durable_reads()?.run_facts(&owner).await?;
        Self::answered_calls(&facts, decoders)
    }

    /// The calls a settled turn answered, read from the transcript its
    /// terminal published (FIG-6601): none when it answered none, or has
    /// no terminal. Its ledger is never read; the terminal retired it.
    ///
    /// # Errors
    ///
    /// A durable read or decoding refusal, preserving its typed cause.
    pub async fn settled_tool_calls(
        cx: &ActorContext,
        session: &SessionId,
        run: &TurnId,
        decoders: &crate::transcript::TranscriptDecoders,
    ) -> Result<Vec<crate::ToolCallRecord>, lash_durable::DurableError> {
        use lash_core_execution::runtime::actor::round::TurnTranscript;
        use lash_core_store::checkpoint_graph::{GraphDecoder, GraphRefusal};
        use lash_durable::domain::RootOwner;

        let graph = match cx
            .durable_reads()?
            .hydrate(
                RootOwner::RunTranscript {
                    session: session.clone(),
                    run: run.clone(),
                },
                None,
                cx.backend().config().settings().graph_reads.owner(),
            )
            .await
        {
            Ok(graph) => graph,
            Err(lash_durable::DurableError::Graph(GraphRefusal::RootRetired)) => {
                return Ok(Vec::new());
            }
            Err(error) => return Err(error),
        };
        let mut decoder = GraphDecoder::new(
            &graph,
            cx.backend().config().settings().graph_reads.execution(),
        );
        TurnTranscript::load(&mut decoder, graph.root)
            .and_then(|transcript| transcript.records(&mut decoder, decoders))
            .map_err(lash_durable::DurableError::Graph)
    }

    /// The calls `facts`, one owner's ledger, answered.
    fn answered_calls(
        facts: &lash_durable::domain::RunFacts,
        decoders: &crate::transcript::TranscriptDecoders,
    ) -> Result<Vec<crate::ToolCallRecord>, lash_durable::DurableError> {
        use lash_core_execution::runtime::actor::round::TurnTranscript;
        use lash_core_store::checkpoint_graph::GraphDecoder;

        let Some(transcript) =
            TurnTranscript::of(facts).map_err(lash_durable::DurableError::Graph)?
        else {
            return Ok(Vec::new());
        };
        transcript
            .records(&mut GraphDecoder::new(facts, facts.read_limits), decoders)
            .map_err(lash_durable::DurableError::Graph)
    }

    /// The tool-call ledger `facts` hold, folded: what a read of an open
    /// turn's pending calls reads. Reporting folds only settled facts and
    /// never consults live policies to run a recovery: the empty policy view
    /// executes nothing.
    fn ledger_fold(
        facts: &lash_durable::domain::RunFacts,
    ) -> Result<lash_core_execution::runtime::actor::round::RunFold, lash_durable::DurableError>
    {
        use lash_core_execution::runtime::actor::round::{self, PolicyView};
        round::fold(facts, &PolicyView::new([]), &round::PinnedWaits::ledger()).map_err(|error| {
            lash_durable::DurableError::Store(lash_durable::StoreFailure {
                kind: lash_durable::StoreFailureKind::Corrupt,
                message: format!("the turn's tool records: {error}"),
            })
        })
    }

    /// The run of `session`'s unfinished turn, if it has one: what a batch
    /// of [`tool_call_bodies`](Self::tool_call_bodies) reads once.
    ///
    /// # Errors
    ///
    /// A durable read's refusal.
    pub async fn unfinished_run(
        cx: &ActorContext,
        session: &SessionId,
    ) -> Result<Option<TurnId>, lash_durable::DurableError> {
        if session.as_str() == cx.actor().id()
            && let Some(projection) = cx.session_projection().await?
        {
            return Ok(projection.turn.map(|turn| turn.run));
        }
        Ok(cx
            .durable_reads()?
            .turn(session)
            .await?
            .map(|turn| turn.run))
    }

    /// Read a named turn's settled bodies, plus its pending identities if it
    /// is `unfinished`, the session's unfinished turn's run
    /// ([`unfinished_run`](Self::unfinished_run)), through its reports,
    /// current round outcomes and saved cell ledgers; an ended turn's through
    /// its transcript. Each owner's facts are read once.
    pub async fn tool_call_bodies(
        cx: &ActorContext,
        session: &SessionId,
        run: &TurnId,
        unfinished: Option<&TurnId>,
        decoders: &crate::transcript::TranscriptDecoders,
    ) -> Result<
        (
            Vec<crate::ToolCallRecord>,
            std::collections::BTreeSet<crate::ToolCallId>,
        ),
        lash_durable::DurableError,
    > {
        use lash_core_store::checkpoint_graph::GraphDecoder;
        use lash_durable::domain::{OwnerKey, RootOwner};
        if unfinished != Some(run) {
            return Self::settled_tool_calls(cx, session, run, decoders)
                .await
                .map(|records| (records, Default::default()));
        }
        let reads = cx.durable_reads()?;
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
            let fold = Self::ledger_fold(&facts)?;
            records.extend(Self::answered_calls(&facts, decoders)?);
            for member in fold.rounds().flat_map(|round| round.members()) {
                if member.draft().tool().as_str().starts_with("cell-host:") {
                    continue;
                }
                if member
                    .outcome()
                    .and_then(lash_core_store::tool_run::SettledOutput::payload)
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
        held: Option<&SessionHead>,
    ) -> Result<(DurableTurn, DriveParts), TurnError> {
        let mut runtime = match held {
            Some(head) => self.runtimes.open_retained(cx, head).await?,
            None => self.runtimes.open(&row.session).await?,
        };
        note_page_reader(&mut runtime, cx);
        let _prepared = crate::runtime::turn_driver::TurnPhaseSpan::begin(
            runtime.turn_phase_probe.clone(),
            crate::runtime::RuntimeTurnPhase::PreparedTurn,
        );
        let admissions = admitted_rows(&runtime, row).await?;
        let controller = cx.scoped(AdmittedScope::turn(row.session.clone(), row.run.clone()))?;
        let live = self.runtimes.live_replay();
        let commit = CommitBase::of(&runtime);
        let revision = crate::runtime::observation::observation_revision(&runtime.state);
        let (observer, publisher) = live_observer(&runtime, &live, &row.run);
        // The run starts from the head the runtime opened at: what it
        // changes from here is what its phases record (FIG-5301).
        runtime.services.plugins.begin_run();
        let start = match recorded {
            Some(recorded) => TurnStart::Resumed(recorded),
            None => TurnStart::Fresh(row.admission.trace().cloned()),
        };
        let turn = Box::pin(runtime.prepare_durable_turn(
            &controller,
            &row.run,
            admissions,
            start,
            held,
            &observer,
        ))
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
    /// on the turn's head commit checks. The turn's prelude takes its base
    /// window from `head` when it names that head.
    async fn start(
        &self,
        cx: &ActorContext,
        row: &TurnRow,
        head: &SessionHead,
    ) -> Result<Box<dyn TurnDrive>, TurnError> {
        let (turn, parts) = Box::pin(self.prepare(cx, row, None, Some(head))).await?;
        Ok(Box::new(RuntimeDrive::start(turn, parts)?))
    }

    async fn resume(
        &self,
        cx: &ActorContext,
        mut restore: TurnRestore<'_>,
    ) -> Result<OpenTurn, TurnError> {
        let recorded = restore.recorded_preparation().await?;
        let (turn, parts) =
            Box::pin(self.prepare(cx, restore.row(), Some(recorded), restore.held_head())).await?;
        RuntimeDrive::resume(cx, turn, parts, restore).await
    }

    async fn apply_commands(
        &self,
        cx: &ActorContext,
        admitted: &AdmittedInputs,
    ) -> Result<(), TurnError> {
        let session = SessionId::parse(cx.actor().id())
            .map_err(|error| TurnError::Exec(format!("session actor id: {error}")))?;
        let mut view = super::head::SessionOwner::default();
        let head = view.head(cx, &session).await?;
        let mut runtime = self.runtimes.open_retained(cx, head).await?;
        note_page_reader(&mut runtime, cx);
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
            let head = view.head(cx, &session).await?;
            commit
                .publish_retained(
                    self.runtimes.live_replay().as_ref(),
                    &self.published,
                    None,
                    head,
                )
                .await;
        }
        Ok(())
    }

    async fn carry_session(&self, cx: &ActorContext, session: &SessionId) -> Result<(), TurnError> {
        let mut view = super::head::SessionOwner::default();
        let head = view.head(cx, session).await?;
        let mut runtime = self.runtimes.open_retained(cx, head).await?;
        note_page_reader(&mut runtime, cx);
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
            let head = view.head(cx, session).await?;
            commit
                .publish_retained(
                    self.runtimes.live_replay().as_ref(),
                    &self.published,
                    None,
                    head,
                )
                .await;
        }
        Ok(())
    }

    fn input_batching(&self) -> &dyn InputBatching {
        self
    }

    fn turn_trace_scope(
        &self,
        cx: &ActorContext,
        run: &TurnId,
        inputs: &[lash_trace::TraceCause],
    ) -> Option<lash_trace::DurableTraceScope> {
        let session = SessionId::parse(cx.actor().id()).ok()?;
        let tracing = self.runtimes.tracing();
        let scope = lash_trace::TraceScopeId::admission(lash_trace::TraceScopeOwner::Turn {
            session_id: session,
            turn_id: run.clone(),
        });
        // A turn joins the trace of the send that caused it: the head
        // input's producer parents it, and the other inputs' producers fan
        // in as links (D1). A turn admitted while nothing observes the
        // runtime retains no cause and no span.
        let (parent, links) = lash_trace::TraceCause::of_admitted(inputs, tracing.limits().links());
        let (parent, links) = if tracing.is_observed() {
            (parent, links)
        } else {
            (None, lash_trace::TraceLinks::new())
        };
        if links.omitted() > 0 {
            let mut counts = lash_trace::TraceOmissionCounts::default();
            counts.add_n(lash_trace::TraceOmission::Links, links.omitted());
            tracing.omissions().record(
                &lash_trace::TraceOmissionSite::Admission {
                    scope: scope.clone(),
                },
                &counts,
            );
        }
        Some(lash_trace::DurableTraceScope::admit(
            scope,
            parent.as_ref(),
            links,
            tracing.clock().timestamp_ms(),
        ))
    }

    fn tracing(&self) -> Option<&crate::trace::TraceRuntime> {
        Some(self.runtimes.tracing())
    }

    async fn announce_head(&self, cx: &ActorContext, session: &SessionId) {
        let mut view = super::head::SessionOwner::default();
        if let Ok(Some(_)) = cx.session_projection().await
            && let Ok(head) = view.head(cx, session).await
        {
            super::commit_publication::announce_retained(
                self.runtimes.live_replay().as_ref(),
                &self.published,
                head,
            )
            .await;
        }
    }

    /// Published at the session's durable head, which the turn has not
    /// moved: the revision its drive would have published at.
    async fn publish_model_call(
        &self,
        cx: &ActorContext,
        row: &TurnRow,
        call_id: &crate::LlmCallId,
    ) -> Result<(), TurnError> {
        let controller = cx.scoped(AdmittedScope::turn(row.session.clone(), row.run.clone()))?;
        let activity =
            crate::runtime::turn_driver::model_call_activity(&controller, &row.run, call_id);
        let Some(projection) = cx.session_projection().await? else {
            return Ok(());
        };
        let revision = crate::runtime::observation::observation_revision(&projection.head);
        LiveActivity {
            live: self.runtimes.live_replay(),
            session: row.session.clone(),
            revision,
        }
        .publish(Some(&row.run), activity)
        .await;
        Ok(())
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
        let mut view = super::head::SessionOwner::default();
        let head = match view.head(_cx, session).await {
            Ok(head) => head,
            // Admit the head input alone so its preparation can settle the
            // typed refusal, even when policy hydration cannot read the head.
            Err(TurnError::Runtime(refusal)) if refusal.is_terminal() => return Ok(None),
            Err(error) => return Err(SessionMailError::Batching(error.to_string())),
        };
        let mut runtime = self
            .runtimes
            .open_retained(_cx, head)
            .await
            .map_err(|error| SessionMailError::Batching(error.to_string()))?;
        note_page_reader(&mut runtime, _cx);
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
        revision: crate::runtime::observation::observation_revision(&runtime.state),
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

/// Sets the session actor's page reader on `runtime` for this activation
/// under `cx`'s claim: the committed-head pages a stored-state check
/// reads (FIG-6418). None where the store publishes no heap pages.
fn note_page_reader(runtime: &mut LashRuntime, cx: &ActorContext) {
    runtime.note_page_reader(
        cx.pages()
            .ok()
            .map(|store| crate::plugin::SessionPageReader {
                store,
                actor: cx.actor().clone(),
                epoch: cx.epoch(),
            }),
    );
}
