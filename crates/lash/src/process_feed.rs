//! The process feed: observation whose snapshot is the durable process and
//! whose tail is the configured process replay store (D-PROCOBS).
//!
//! It keeps the session feed's contract (`observation_feed`, ADR 0002) with
//! the process's durable event sequence as the revision. A snapshot is the
//! process's durable read view at one sequence, paired with the earliest
//! cursor the replay still retains, so an observer that attaches late folds
//! the whole retained window of provisional node evidence. The durable
//! process is the authority a cursor is judged against: a cursor behind it
//! continues only when the replay holds the committed fact of every sequence
//! in between, and every gap's replacement is the durable read view.
//!
//! A `Committed` event carries one committed fact. The feed delivers it only
//! to a consumer holding the sequence before it, skips it for one that
//! already holds it, and answers any other with a gap. A provisional
//! language observation never advances the sequence a consumer holds, and
//! what a gap loses of it is not restored: node history is not durable.
//!
//! An open feed converges on the durable process whoever committed to it
//! and whatever became of the commit's publication. It holds a subscription
//! of the registry's emitted hub, which ticks once this node's observation
//! dispatcher was handed a commit made here and, through node wakes, at a
//! commit on another node; at a tick it compares the durable sequence with
//! the one its consumer holds. The window between a commit and its
//! publication is closed by the session feed's rule: while this node is
//! still publishing a fact behind the sequence it read, the feed waits for
//! that publication and takes the fact from the replay. A tick follows the
//! dispatcher's admission of the commit, so a commit made here is always
//! found in flight or already published. Only with none in flight (another
//! node's commit, or a lost publication) does the feed publish the retained
//! facts between to the replay store itself, which drops those it already
//! holds, and deliver them from the store like any other. It bridges only a
//! small distance that way: a consumer further behind than the bridge
//! bound, facts that were released, and a fact whose publication was
//! dropped or refused are each a gap, and the feed goes on from the durable
//! process. Only a failed read ends a feed with an error. An idle feed
//! reads nothing: a node that may have missed a wake ticks every followed
//! process when its listener resubscribes.
//!
//! Delivery is at least once. A stream drops an event identity
//! ([`ProcessObservationEventId`]) it already delivered within a bounded
//! window, which a host seeds with the identities it applied before a
//! reconnect; a gap clears the window. A host that persists a position
//! persists [`ProcessObservationStream::cursor`], which carries the sequence
//! the consumer holds.

use std::collections::{BTreeSet, VecDeque};
use std::num::NonZeroUsize;
use std::pin::Pin;
use std::task::{Context, Poll};

use futures_util::future::BoxFuture;
use futures_util::{FutureExt as _, Stream, StreamExt as _};
use lash_core::ProcessEffectCoverage;
use lash_core::ProcessEffectEvidence;
use lash_core::ProcessEffectGapReason;
use lash_core::ProcessObservation;
use lash_core::ProcessObservationCursor;
use lash_core::ProcessObservationEnd;
use lash_core::ProcessObservationEvent;
use lash_core::ProcessObservationEventPayload;
use lash_core::ProcessObservationGapCause;
use lash_core::ProcessObservationReplacement;
use lash_core::ProcessReadView;
use lash_core::ProcessReplayStore;
use lash_core::ProcessReplayStoreError;
use lash_core::ProcessReplaySubscribeOutcome;
use lash_core::ProcessReplaySubscription;
use lash_core::ProcessSequence;
use lash_core::RetainedProcessView;
use lash_core::runtime::{ProcessChangeHub, ProcessChangeSubscription};
use lash_execution_contracts::plugin_error::PluginError;
use lash_execution_contracts::process::ProcessEffectReport;
use lash_execution_contracts::process::ProcessEventHistoryRetention;
use lash_execution_contracts::process::ProcessEventPageEvents;
use lash_execution_contracts::process::ProcessEventPageMore;
use lash_execution_contracts::process::ProcessEventQueryMode;
use lash_execution_contracts::process::ProcessEventReadOutcome;
use lash_execution_contracts::process::ProcessRegistry;
use lash_execution_contracts::process::RetiredProcessStatus;
use lash_protocol_types::ProcessId;

use crate::language_observation::CommittedPublication;
use crate::support::{Arc, EmbedError, Result, RuntimeErrorCode};

/// How much of a process's durable history one snapshot may read to fold
/// its effect evidence: the core's
/// [`ObservationWorkLimits`](lash_trace::ObservationWorkLimits).
#[derive(Clone, Copy, Debug)]
pub(crate) struct EffectFoldBudget {
    pub(crate) pages: usize,
    pub(crate) page_size: NonZeroUsize,
    occurrence_cap: u64,
}

impl EffectFoldBudget {
    pub(crate) fn of(limits: lash_trace::ObservationWorkLimits) -> Self {
        Self {
            pages: limits.process_effect_fold_pages,
            page_size: limits.process_effect_fold_page_size,
            occurrence_cap: lash_core_store::execution_limits::EvidenceLimits::standard()
                .process_effect_occurrences(),
        }
    }

    fn pages_for(self, recorded_cap: u64) -> usize {
        // Preserve the standard work per node when the host retains more
        // occurrences; a recorded terminal cut also survives host reconfiguration.
        let base = lash_core_store::execution_limits::EvidenceLimits::standard()
            .process_effect_occurrences();
        let scale = self
            .occurrence_cap
            .max(recorded_cap)
            .max(base)
            .div_ceil(base);
        self.pages
            .saturating_mul(usize::try_from(scale).unwrap_or(usize::MAX))
    }
}

/// A process's history was pruned while it was read.
pub(crate) struct Pruned {
    pub(crate) terminal_label: RetiredProcessStatus,
    pub(crate) pruned_at_ms: u64,
}

/// Fold the effect evidence of one process through `through`, the sequence
/// read with its row, with Full payloads and within `budget`.
///
/// Events are append-only, so an append past `through` never changes the
/// fold. A released prefix is reported and folded on from; a prune is the
/// typed absence, never an empty history.
pub(crate) async fn fold_effects(
    registry: &dyn ProcessRegistry,
    process_id: &ProcessId,
    through: u64,
    budget: EffectFoldBudget,
    mut workflow: Option<&mut lash_core::ProcessWorkflowOverlayAccumulator>,
) -> std::result::Result<std::result::Result<ProcessEffectEvidence, Pruned>, PluginError> {
    let fleet_format = registry.fleet_format();
    let mut report = ProcessEffectReport::default();
    let mut coverage = ProcessEffectCoverage::Complete;
    let mut incomplete = |reason| coverage = ProcessEffectCoverage::Incomplete { reason };
    // An omission record is one indexed read regardless of the occurrence
    // budget. Its upper bound is the same row revision as the forward fold,
    // even if a terminal commits after the snapshot's row was read.
    let mut recorded_cap = 0;
    match registry
        .event_by_replay_key(
            process_id,
            &format!("process:{process_id}:effect-omissions"),
            through,
        )
        .await?
    {
        ProcessEventReadOutcome::Retained(Some(event)) => {
            if let lash_execution_contracts::process::ProcessLifecycleFact::EffectOmissions(
                omissions,
            ) = &event.fact
            {
                if report.fold_event(&event.fact, fleet_format).is_ok() {
                    recorded_cap = omissions.occurrence_cap;
                } else {
                    incomplete(ProcessEffectGapReason::Undecodable);
                }
            } else {
                incomplete(ProcessEffectGapReason::Undecodable);
            }
        }
        ProcessEventReadOutcome::Retained(None) => {}
        ProcessEventReadOutcome::NoLongerRetained(ProcessEventHistoryRetention::Released {
            ..
        }) => {
            incomplete(ProcessEffectGapReason::HistoryReleased);
        }
        ProcessEventReadOutcome::NoLongerRetained(ProcessEventHistoryRetention::Pruned {
            terminal_label,
            pruned_at_ms,
        }) => {
            return Ok(Err(Pruned {
                terminal_label,
                pruned_at_ms,
            }));
        }
    }
    let page_budget = budget.pages_for(recorded_cap);
    let mut after = 0;
    let mut pages = 0;
    'pages: while after < through {
        if pages >= page_budget {
            incomplete(ProcessEffectGapReason::AcquisitionBudgetExhausted);
            return Ok(Ok(ProcessEffectEvidence {
                report,
                observed_through: ProcessSequence::new(after),
                coverage,
            }));
        }
        pages += 1;
        let page = match registry
            .event_page_after(
                process_id,
                after,
                budget.page_size,
                ProcessEventQueryMode::Full,
            )
            .await?
        {
            ProcessEventReadOutcome::Retained(page) => page,
            ProcessEventReadOutcome::NoLongerRetained(ProcessEventHistoryRetention::Pruned {
                terminal_label,
                pruned_at_ms,
            }) => {
                return Ok(Err(Pruned {
                    terminal_label,
                    pruned_at_ms,
                }));
            }
            ProcessEventReadOutcome::NoLongerRetained(ProcessEventHistoryRetention::Released {
                released_through,
            }) => {
                incomplete(ProcessEffectGapReason::HistoryReleased);
                after = released_through;
                continue;
            }
        };
        let ProcessEventPageEvents::Full(events) = page.events else {
            return Err(PluginError::Session(
                "a Full process event page returned Lite events".to_string(),
            ));
        };
        for event in events {
            if event.sequence > through {
                break 'pages;
            }
            if let Some(workflow) = workflow.as_deref_mut() {
                workflow.committed(&event.clone().into());
            }
            if report.fold_event(&event.fact, fleet_format).is_err() {
                incomplete(ProcessEffectGapReason::Undecodable);
            }
            after = event.sequence;
        }
        if matches!(page.more, ProcessEventPageMore::Complete) {
            break;
        }
    }
    Ok(Ok(ProcessEffectEvidence {
        report,
        observed_through: ProcessSequence::new(through),
        coverage,
    }))
}

/// What wakes an open feed to compare the durable process with what its
/// consumer holds.
#[derive(Clone)]
pub(crate) struct FeedReconcile {
    /// The core's ordered publication barrier, shared with its language sink.
    pub(crate) publisher: Arc<crate::language_observation::LanguageObservationPublisher>,
    /// Ticks once this node's sinks were handed a commit that grew a
    /// process's log, and when another node grew it: the watched registry's
    /// emitted hub.
    pub(crate) changes: ProcessChangeHub,
}

/// What a process feed reads: the durable process and the replay store.
#[derive(Clone)]
pub(crate) struct ProcessFeedSource {
    work_limits: lash_trace::ObservationWorkLimits,
    process_id: ProcessId,
    registry: Arc<dyn ProcessRegistry>,
    observer: lash_execution_contracts::process::ProcessWorkObserver,
    /// The engines whose document providers name a process's document.
    engines: lash_core::ProcessEngineRegistry,
    replay: Arc<dyn ProcessReplayStore>,
    effect_budget: EffectFoldBudget,
    reconcile: FeedReconcile,
}

impl ProcessFeedSource {
    pub(crate) fn new(
        process_id: ProcessId,
        registry: Arc<dyn ProcessRegistry>,
        observer: lash_execution_contracts::process::ProcessWorkObserver,
        engines: lash_core::ProcessEngineRegistry,
        replay: Arc<dyn ProcessReplayStore>,
        work_limits: lash_trace::ObservationWorkLimits,
        reconcile: FeedReconcile,
    ) -> Self {
        Self {
            work_limits,
            process_id,
            registry,
            observer,
            engines,
            replay,
            effect_budget: EffectFoldBudget::of(work_limits),
            reconcile,
        }
    }

    /// Size occurrence acquisition for the host's configured retention. The
    /// terminal omission record may name a larger cut pinned by an older host.
    pub(crate) fn with_effect_retention(mut self, occurrence_cap: u64) -> Self {
        self.effect_budget.occurrence_cap = occurrence_cap;
        self
    }

    /// The durable sequence of the process, or `None` when no process is
    /// retained under its id.
    async fn durable_sequence(&self) -> Result<Option<ProcessSequence>> {
        match self.registry.get_process(&self.process_id).await {
            Ok(Some(record)) => Ok(Some(ProcessSequence::new(record.last_event_sequence))),
            Ok(None)
            | Err(
                PluginError::ProcessNoLongerRetained { .. } | PluginError::ProcessUnknown { .. },
            ) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    /// The durable read view: the row at one sequence and the effect
    /// evidence folded through that sequence.
    async fn read_view(
        &self,
    ) -> Result<(
        ProcessReadView,
        Option<lash_core::ProcessWorkflowOverlayAccumulator>,
    )> {
        let process = match self.observer.process(&self.process_id).await {
            Ok(Some(process)) => process,
            Ok(None) | Err(PluginError::ProcessUnknown { .. }) => {
                return Ok((ProcessReadView::Unknown, None));
            }
            Err(PluginError::ProcessNoLongerRetained {
                terminal_label,
                pruned_at_ms,
            }) => {
                return Ok((
                    ProcessReadView::Retired {
                        terminal_label,
                        pruned_at_ms,
                    },
                    None,
                ));
            }
            Err(error) => return Err(error.into()),
        };
        let document = self.document(&process).await?;
        let mut workflow = match &document {
            lash_core::ProcessDocumentIdentity::Available(_) => Some(
                lash_core::ProcessWorkflowOverlayAccumulator::new(self.process_id.clone())
                    .observing(self.observation(&process)),
            ),
            _ => None,
        };
        match fold_effects(
            self.registry.as_ref(),
            &self.process_id,
            process.last_event_sequence,
            self.effect_budget,
            workflow.as_mut(),
        )
        .await?
        {
            Ok(effects) => {
                if process.lifecycle.terminal().is_some()
                    && let Some(workflow) = &mut workflow
                {
                    workflow.finish();
                }
                let view = RetainedProcessView {
                    process,
                    effects,
                    document,
                    workflow: None,
                };
                Ok((ProcessReadView::Retained(Box::new(view)), workflow))
            }
            Err(Pruned {
                terminal_label,
                pruned_at_ms,
            }) => Ok((
                ProcessReadView::Retired {
                    terminal_label,
                    pruned_at_ms,
                },
                None,
            )),
        }
    }

    /// Whether `process` counts its statements, as its engine's document
    /// provider reads its recorded settings: `off` for any other process.
    fn observation(
        &self,
        process: &lash_execution_contracts::process::ObservedProcess,
    ) -> lash_core::ProcessWorkflowObservation {
        let lash_execution_contracts::process::ProcessInput::Engine { kind, launch, .. } =
            &process.input
        else {
            return lash_core::ProcessWorkflowObservation::Off;
        };
        match self.engines.document_provider(kind) {
            Some(provider) if provider.counts_statements(launch.settings.as_ref()) => {
                lash_core::ProcessWorkflowObservation::Counts
            }
            _ => lash_core::ProcessWorkflowObservation::Off,
        }
    }

    /// Which document `process` runs, named by its engine's document
    /// provider from the process's recorded input.
    async fn document(
        &self,
        process: &lash_execution_contracts::process::ObservedProcess,
    ) -> Result<lash_core::ProcessDocumentIdentity> {
        use lash_core::ProcessDocumentIdentity;
        let lash_execution_contracts::process::ProcessInput::Engine { kind, payload, .. } =
            &process.input
        else {
            return Ok(ProcessDocumentIdentity::Unsupported);
        };
        let Some(provider) = self.engines.document_provider(kind) else {
            return Ok(ProcessDocumentIdentity::Unsupported);
        };
        Ok(match provider.document_ref(payload).await? {
            lash_execution_contracts::process::ProcessDocumentRefRead::Named { link, selector } => {
                match selector {
                    lash_execution_contracts::process::linking::ReachSelector::Start(selector) => {
                        ProcessDocumentIdentity::Available(lash_vocabulary::names::LinkedStart {
                            link,
                            selector,
                        })
                    }
                    // The workflow view supports Main and top-level Exports.
                    // A captured callable must not be presented as another Export.
                    _ => ProcessDocumentIdentity::Unsupported,
                }
            }
            lash_execution_contracts::process::ProcessDocumentRefRead::ArtifactMissing {
                artifact,
            } => ProcessDocumentIdentity::ArtifactUnavailable { artifact },
        })
    }

    /// The durable read view with the earliest cursor the replay retains,
    /// bound to the view's sequence.
    ///
    /// The view is read first: a commit that races it is stamped past the
    /// cursor's sequence, so the feed replays it or answers a gap. A view
    /// that retains no process asks the replay store for nothing: its
    /// cursor names no store's continuity, so no id costs a replay window
    /// by being read.
    pub(crate) async fn snapshot(&self) -> Result<ProcessObservation> {
        let (mut read_view, mut workflow) = self.read_view().await?;
        let cursor = match read_view.sequence() {
            Some(sequence) => self
                .replay
                .earliest_cursor(&self.process_id, sequence)
                .await
                .map_err(process_replay_error)?,
            None => ProcessObservationCursor::new(
                UNRETAINED_INCARNATION,
                &self.process_id,
                ProcessSequence::new(0),
                0,
            ),
        };
        if let (Some(workflow), ProcessReadView::Retained(view)) = (&mut workflow, &mut read_view) {
            match self
                .replay
                .replay_after_cursor(&cursor)
                .await
                .map_err(process_replay_error)?
            {
                lash_core::ProcessReplayOutcome::Replayed(events) => {
                    // read_view already includes committed counts. The publisher
                    // admits language frames and their subsequent commit in FIFO
                    // order, so Ran frames through the last retained count commit
                    // belong to that durable prefix. Replaying those deltas would
                    // count it twice; retain the node evidence and later frames.
                    let counts_through = events.iter().rposition(|event| {
                        matches!(
                            &event.payload,
                            ProcessObservationEventPayload::Committed { event }
                                if event.sequence <= view.process.last_event_sequence
                                    && matches!(event.fact, lash_execution_contracts::process::ProcessLifecycleFact::Ran(_))
                        )
                    });
                    for (position, event) in events.into_iter().enumerate() {
                        if counts_through.is_some_and(|through| position <= through)
                            && matches!(&event.payload,
                                ProcessObservationEventPayload::LanguageExecution(observation)
                                    if matches!(observation.execution.payload, crate::tracing::TraceLanguageExecutionPayload::Counts { .. }))
                        {
                            continue;
                        }
                        // A commit after the row revision belongs to the feed,
                        // never to this snapshot's durable projection.
                        if matches!(&event.payload, ProcessObservationEventPayload::Committed { event }
                            if event.sequence > view.process.last_event_sequence)
                        {
                            continue;
                        }
                        if let lash_core::ProcessDocumentIdentity::Available(document) =
                            &view.document
                        {
                            workflow.observe(document, &event.payload)?;
                        }
                    }
                }
                lash_core::ProcessReplayOutcome::Gap(reason) => workflow.live_gap(reason),
            }
            view.workflow = Some(workflow.snapshot());
        }
        Ok(ProcessObservation { read_view, cursor })
    }

    /// The publication window of the durable sequence `durable`, read just
    /// now: whether this node is still publishing a fact at or before it.
    fn publication_window(
        &self,
        durable: ProcessSequence,
    ) -> lash_core::runtime::durable::services::PublicationWindow<ProcessId, ProcessSequence> {
        self.reconcile
            .publisher
            .publication_window(&self.process_id, durable)
    }

    /// The sequence `cursor` names, refused when it is malformed or names
    /// another process.
    fn requested_sequence(&self, cursor: &ProcessObservationCursor) -> Result<ProcessSequence> {
        Ok(cursor
            .parse_for_process(&self.process_id)
            .map_err(|error| process_replay_error(error.into()))?
            .sequence)
    }

    /// The authoritative replacement at a gap: retained state and its
    /// continuation, or typed absence that ends observation.
    async fn gap(
        &self,
        cause: ProcessObservationGapCause,
    ) -> Result<ProcessObservationReplacement> {
        let observation = self.snapshot().await?;
        Ok(match observation.read_view {
            ProcessReadView::Retained(view) => ProcessObservationReplacement::Replaced {
                view,
                cursor: observation.cursor,
                cause,
            },
            ProcessReadView::Retired {
                terminal_label,
                pruned_at_ms,
            } => ProcessObservationReplacement::Ended(ProcessObservationEnd::Retired {
                terminal_label,
                pruned_at_ms,
            }),
            ProcessReadView::Unknown => {
                ProcessObservationReplacement::Ended(ProcessObservationEnd::Unknown)
            }
        })
    }
}

/// The incarnation a cursor names when its snapshot retains no process. No
/// replay store holds it, so a feed from the cursor is judged against the
/// durable process alone.
const UNRETAINED_INCARNATION: &str = "unretained";

/// One item of a process feed.
#[derive(Clone, Debug)]
pub enum ProcessObservationStreamItem {
    Event(Arc<ProcessObservationEvent>),
    /// The feed could not continue from its cursor. `replacement` replaces
    /// the consumer's durable state; the consumer discards its provisional
    /// state and folds what the feed replays next.
    Gap {
        requested: ProcessObservationCursor,
        replacement: ProcessObservationReplacement,
    },
}

/// At-least-once delivery identity of one process observation event: the
/// process, the replay-store incarnation and the live position. It is safe
/// to persist across a restart, because a new store cannot reproduce an old
/// incarnation. It is not the identity of the fact the event carries: a
/// committed fact is its process and sequence in every incarnation.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProcessObservationEventId {
    pub process_id: ProcessId,
    pub replay_incarnation_id: String,
    pub live_position: u64,
}

impl ProcessObservationEventId {
    /// The identity of `event`.
    pub fn of(event: &ProcessObservationEvent) -> Self {
        Self {
            process_id: event.process_id(),
            replay_incarnation_id: event.replay_incarnation_id().to_string(),
            live_position: event.live_position(),
        }
    }
}

/// The bounded window of identities a stream delivered or its host applied.
#[derive(Default)]
struct AppliedEventIds {
    limit: usize,
    ids: BTreeSet<ProcessObservationEventId>,
    order: VecDeque<ProcessObservationEventId>,
}

impl AppliedEventIds {
    fn insert(&mut self, id: ProcessObservationEventId) -> bool {
        if !self.ids.insert(id.clone()) {
            return false;
        }
        self.order.push_back(id);
        self.shrink();
        true
    }

    fn shrink(&mut self) {
        while self.order.len() > self.limit {
            if let Some(expired) = self.order.pop_front() {
                self.ids.remove(&expired);
            }
        }
    }

    /// Whether the stream delivers `item`: a gap always, clearing the
    /// window, since its snapshot is authoritative; an event only when its
    /// identity is new.
    fn admit(&mut self, item: &ProcessObservationStreamItem) -> bool {
        match item {
            ProcessObservationStreamItem::Gap { .. } => {
                self.ids.clear();
                self.order.clear();
                true
            }
            ProcessObservationStreamItem::Event(event) => {
                self.insert(ProcessObservationEventId::of(event))
            }
        }
    }
}

/// Stream returned by
/// [`ObservableProcess::subscribe_and_recover`](crate::ObservableProcess::subscribe_and_recover):
/// the process feed from a cursor.
///
/// It yields the replay's events after the cursor, each committed fact once
/// and in sequence, and [`ProcessObservationStreamItem::Gap`] with the
/// durable read view when the cursor cannot be continued. It keeps going
/// after a gap from the gap's cursor, and ends after a gap whose read view
/// says no process is retained.
///
/// Dropping the stream only disconnects observation; it never cancels work.
pub struct ProcessObservationStream {
    cursor: ProcessObservationCursor,
    state: Option<Box<FeedState>>,
    step: Option<FeedStep>,
    applied: AppliedEventIds,
}

/// One feed step in flight: it owns the feed's state and hands it back with
/// the item it produced.
type FeedStep = BoxFuture<'static, (Box<FeedState>, Option<Result<ProcessObservationStreamItem>>)>;

impl ProcessObservationStream {
    pub(crate) fn new(source: ProcessFeedSource, cursor: ProcessObservationCursor) -> Self {
        let limit = source.work_limits.process_dedup_ids;
        // Watched before the feed's first read of the durable process, so a
        // commit after that read ticks.
        let changes = Some(source.reconcile.changes.subscribe(&source.process_id));
        Self {
            cursor: cursor.clone(),
            state: Some(Box::new(FeedState {
                source,
                cursor,
                done: false,
                held: None,
                live: None,
                changes,
            })),
            step: None,
            applied: AppliedEventIds {
                limit,
                ..Default::default()
            },
        }
    }

    /// Seed identities the host already applied, so a reconnect's
    /// redelivery is idempotent. The stream keeps a bounded recent window
    /// and clears it at a gap, whose snapshot is authoritative.
    pub fn with_applied_event_ids(
        mut self,
        ids: impl IntoIterator<Item = ProcessObservationEventId>,
    ) -> Self {
        for id in ids {
            self.applied.insert(id);
        }
        self
    }

    /// Where the stream stands: the last delivered or skipped event's live
    /// position, at the newest sequence delivered. A host resumes from it.
    pub fn cursor(&self) -> &ProcessObservationCursor {
        &self.cursor
    }
}

impl Stream for ProcessObservationStream {
    type Item = Result<ProcessObservationStreamItem>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            if self.step.is_none() {
                let Some(state) = self.state.take() else {
                    return Poll::Ready(None);
                };
                self.step = Some(state.next().boxed());
            }
            let Some(step) = self.step.as_mut() else {
                return Poll::Ready(None);
            };
            let (state, item) = std::task::ready!(step.as_mut().poll(cx));
            self.step = None;
            self.cursor = state.cursor.clone();
            if !state.done {
                self.state = Some(state);
            }
            if let Some(Ok(delivered)) = &item
                && !self.applied.admit(delivered)
            {
                continue;
            }
            return Poll::Ready(item);
        }
    }
}

/// What the feed does with one replayed or live event.
enum Delivery {
    Item(ProcessObservationStreamItem),
    /// A committed fact the consumer already holds.
    Skipped,
    /// A committed fact past the one that extends what the consumer holds:
    /// the consumer rebuilds from the durable process.
    Unbridged,
}

struct FeedState {
    source: ProcessFeedSource,
    /// Where the feed stands: the last delivered or skipped event's live
    /// position, at the newest sequence delivered.
    cursor: ProcessObservationCursor,
    done: bool,
    /// The durable sequence the consumer holds, once a subscription or a
    /// gap established it.
    held: Option<ProcessSequence>,
    live: Option<ProcessReplaySubscription>,
    /// Ticks when a commit grew the process's log; `None` once the
    /// registry's hub is gone, when only the replay is left to follow.
    changes: Option<ProcessChangeSubscription>,
}

impl FeedState {
    async fn next(
        mut self: Box<Self>,
    ) -> (Box<Self>, Option<Result<ProcessObservationStreamItem>>) {
        let item = self.step().await;
        if matches!(item, None | Some(Err(_))) {
            self.done = true;
        }
        (self, item)
    }

    async fn step(&mut self) -> Option<Result<ProcessObservationStreamItem>> {
        loop {
            if self.done {
                return None;
            }
            let Some(live) = self.live.as_mut() else {
                match self.subscribe().await {
                    Ok(Some(item)) => return Some(Ok(item)),
                    Ok(None) => continue,
                    Err(error) => return Some(Err(error)),
                }
            };
            // Whatever the replay already holds goes first; a tick only
            // makes the feed look at the durable process.
            let changes = &mut self.changes;
            let next = tokio::select! {
                biased;
                next = live.next() => Some(next),
                changed = async {
                    match changes {
                        Some(changes) => changes.changed().await,
                        None => std::future::pending().await,
                    }
                } => {
                    if changed.is_err() {
                        self.changes = None;
                        continue;
                    }
                    None
                }
            };
            let Some(next) = next else {
                match self.reconcile().await {
                    Ok(Some(item)) => return Some(Ok(item)),
                    Ok(None) => continue,
                    Err(error) => return Some(Err(error)),
                }
            };
            match next {
                None => return None,
                Some(Ok(event)) => match self.deliver(event) {
                    Delivery::Item(item) => return Some(Ok(item)),
                    Delivery::Skipped => {}
                    Delivery::Unbridged => {
                        return Some(
                            self.rebuild(ProcessObservationGapCause::CommitUnbridged)
                                .await,
                        );
                    }
                },
                Some(Err(
                    ProcessReplayStoreError::SubscriberLagged(_) | ProcessReplayStoreError::Closed,
                )) => self.live = None,
                Some(Err(error)) => return Some(Err(process_replay_error(error))),
            }
        }
    }

    /// Subscribe from the feed's cursor, judged against the durable
    /// process: a gap replaces the consumer's state, and the feed continues
    /// from the gap's cursor.
    ///
    /// A replay that lacks facts between the cursor and the durable process
    /// is judged by the session feed's rule (`PublicationWindow`): while
    /// this node is still publishing a fact at or before the sequence read,
    /// the feed waits that out and judges again from a new read. With none
    /// in flight the publication was lost, or another node committed, and
    /// the feed brings the replay up to that sequence itself, within the
    /// bridge bound and once for each sequence it reads, before it calls
    /// the missing facts a gap.
    async fn subscribe(&mut self) -> Result<Option<ProcessObservationStreamItem>> {
        let requested = self.source.requested_sequence(&self.cursor)?;
        let mut reconciled = None;
        let cause = loop {
            let Some(durable) = self.source.durable_sequence().await? else {
                break ProcessObservationGapCause::CommitUnbridged;
            };
            if requested > durable {
                break ProcessObservationGapCause::AheadOfDurableProcess;
            }
            let window = self.source.publication_window(durable);
            match self
                .source
                .replay
                .subscribe_after_cursor(&self.cursor)
                .await
                .map_err(process_replay_error)?
            {
                ProcessReplaySubscribeOutcome::Subscribed(subscription)
                    if subscription.bridges(requested, durable) =>
                {
                    self.held = Some(self.held.map_or(requested, |held| held.max(requested)));
                    self.live = Some(subscription);
                    return Ok(None);
                }
                ProcessReplaySubscribeOutcome::Subscribed(_) => {
                    if window.closed().await {
                        continue;
                    }
                    if reconciled.is_some_and(|through| through >= durable) {
                        break ProcessObservationGapCause::CommitUnbridged;
                    }
                    reconciled = Some(durable);
                    let held = self.held.map_or(requested, |held| held.max(requested));
                    self.held = Some(held);
                    if let Some(gap) = self.publish_missing(held, durable).await? {
                        return Ok(Some(gap));
                    }
                }
                ProcessReplaySubscribeOutcome::Gap(reason) => {
                    break ProcessObservationGapCause::Replay { reason };
                }
            }
        };
        self.rebuild(cause).await.map(Some)
    }

    /// Look at the durable process from an open subscription. A sequence
    /// past the one the consumer holds is the live tail's to deliver while
    /// this node is still publishing a fact at or before it: the feed waits
    /// that publication out and reads its tail. With none in flight it
    /// brings the replay up to the durable process itself.
    async fn reconcile(&mut self) -> Result<Option<ProcessObservationStreamItem>> {
        let Some(held) = self.held else {
            return Ok(None);
        };
        let Some(durable) = self.source.durable_sequence().await? else {
            return self
                .rebuild(ProcessObservationGapCause::CommitUnbridged)
                .await
                .map(Some);
        };
        if durable <= held || self.source.publication_window(durable).closed().await {
            return Ok(None);
        }
        self.publish_missing(held, durable).await
    }

    /// Bring the replay store up to the durable process: publish the retained
    /// facts after `held` through `durable`, which the store drops where it
    /// already holds them and the feed then delivers in order.
    ///
    /// The feed bridges at most the host's bridge bound that way. A consumer
    /// further behind gaps at once and the feed publishes nothing: the
    /// durable read view is cheaper than the bridge, and facts pushed into a
    /// window that cannot hold them evict other observers' evidence. A
    /// released or pruned history is a gap too, and so is a fact the
    /// dispatcher dropped or the store refused. Only a registry read that
    /// fails is an error.
    async fn publish_missing(
        &mut self,
        held: ProcessSequence,
        durable: ProcessSequence,
    ) -> Result<Option<ProcessObservationStreamItem>> {
        let bridge = self.source.work_limits.process_reconcile_bridge_events as u64;
        if durable.as_u64().saturating_sub(held.as_u64()) > bridge {
            return self.unbridged().await;
        }
        let mut after = held.as_u64();
        while let Some(missing) = usize::try_from(durable.as_u64().saturating_sub(after))
            .ok()
            .and_then(NonZeroUsize::new)
        {
            let page = match self
                .source
                .registry
                .event_page_after(
                    &self.source.process_id,
                    after,
                    missing,
                    ProcessEventQueryMode::Full,
                )
                .await?
            {
                ProcessEventReadOutcome::Retained(page) => page,
                ProcessEventReadOutcome::NoLongerRetained(_) => return self.unbridged().await,
            };
            let ProcessEventPageEvents::Full(events) = page.events else {
                return Err(PluginError::Session(
                    "a Full process event page returned Lite events".to_string(),
                )
                .into());
            };
            let facts: Vec<_> = events
                .into_iter()
                .filter(|event| event.sequence > after && event.sequence <= durable.as_u64())
                .collect();
            let Some(last) = facts.last().map(|event| event.sequence) else {
                break;
            };
            for event in facts {
                match self
                    .source
                    .reconcile
                    .publisher
                    .publish_committed(&self.source.process_id, event.into())
                    .await
                {
                    CommittedPublication::Published => {}
                    CommittedPublication::ContinuityLost | CommittedPublication::Stopped => {
                        return self.unbridged().await;
                    }
                }
            }
            after = last;
        }
        Ok(None)
    }

    /// The gap of a consumer whose missing facts the feed does not bridge.
    async fn unbridged(&mut self) -> Result<Option<ProcessObservationStreamItem>> {
        self.rebuild(ProcessObservationGapCause::CommitUnbridged)
            .await
            .map(Some)
    }

    /// Replace the consumer's state with the durable read view: a gap item
    /// whose cursor the feed continues from. A read view that retains no
    /// process ends the feed.
    async fn rebuild(
        &mut self,
        cause: ProcessObservationGapCause,
    ) -> Result<ProcessObservationStreamItem> {
        let requested = self.cursor.clone();
        let replacement = self.source.gap(cause).await?;
        match &replacement {
            ProcessObservationReplacement::Replaced { view, cursor, .. } => {
                self.held = Some(ProcessSequence::new(view.process.last_event_sequence));
                self.cursor = cursor.clone();
            }
            ProcessObservationReplacement::Ended(_) => {
                self.held = None;
                self.done = true;
            }
        }
        self.live = None;
        Ok(ProcessObservationStreamItem::Gap {
            requested,
            replacement,
        })
    }

    /// Deliver one event. A committed fact at or below the sequence the
    /// consumer holds is a redelivery; the next one extends it; any later
    /// one arrived without the facts between.
    fn deliver(&mut self, event: Arc<ProcessObservationEvent>) -> Delivery {
        let held = self.held.unwrap_or(ProcessSequence::new(0));
        if let ProcessObservationEventPayload::Committed { event: fact } = &event.payload {
            if fact.sequence <= held.as_u64() {
                self.advance_past(&event, held);
                return Delivery::Skipped;
            }
            if fact.sequence != held.as_u64() + 1 {
                return Delivery::Unbridged;
            }
            let held = ProcessSequence::new(fact.sequence);
            self.held = Some(held);
            self.advance_past(&event, held);
            return Delivery::Item(ProcessObservationStreamItem::Event(event));
        }
        self.advance_past(&event, held);
        Delivery::Item(ProcessObservationStreamItem::Event(event))
    }

    /// Move the feed's cursor to a delivered or skipped event's position,
    /// at the sequence the consumer holds: a provisional event stamped at
    /// an older sequence never moves it back.
    fn advance_past(&mut self, event: &ProcessObservationEvent, held: ProcessSequence) {
        self.cursor = ProcessObservationCursor::new(
            event.replay_incarnation_id(),
            &self.source.process_id,
            held,
            event.live_position(),
        );
    }
}

pub(crate) fn process_replay_error(err: ProcessReplayStoreError) -> EmbedError {
    EmbedError::Runtime(lash_core::RuntimeError::new(
        RuntimeErrorCode::LiveReplay,
        err.to_string(),
    ))
}

/// One process, observed: its durable snapshot and the feed that continues
/// it.
///
/// ```rust,ignore
/// let observed = core.processes().observe(&process_id);
/// let snapshot = observed.snapshot().await?;
/// let mut feed = observed.subscribe_and_recover(snapshot.cursor);
/// ```
#[derive(Clone)]
pub struct ObservableProcess {
    pub(crate) source: ProcessFeedSource,
}

impl ObservableProcess {
    /// The process's durable read view and the cursor a feed from
    /// [`subscribe_and_recover`](Self::subscribe_and_recover) continues.
    ///
    /// The cursor is the earliest position the replay store retains for the
    /// process, so the feed replays the retained provisional evidence;
    /// retained commits the view already reflects are skipped.
    pub async fn snapshot(&self) -> Result<ProcessObservation> {
        self.source.snapshot().await
    }

    /// The process feed from `cursor`: the replay's events after it, each
    /// committed fact once, and a gap with the durable read view when the
    /// cursor cannot be continued.
    ///
    /// A cursor that names another process is refused by the stream's first
    /// item, never retargeted.
    pub fn subscribe_and_recover(
        &self,
        cursor: ProcessObservationCursor,
    ) -> ProcessObservationStream {
        ProcessObservationStream::new(self.source.clone(), cursor)
    }
}

#[cfg(test)]
#[path = "process_feed/tests.rs"]
mod tests;
