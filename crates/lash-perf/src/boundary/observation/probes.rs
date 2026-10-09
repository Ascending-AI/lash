//! Counting decorators over the replay stores a core is built with. They
//! delegate every call, so a workload measures the product path and reads
//! what crossed the store boundary.
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use lash_core::{
    LiveReplayEventDraft, LiveReplayOutcome, LiveReplayStore, LiveReplayStoreError,
    LiveReplaySubscribeOutcome, ProcessObservationCursor, ProcessObservationEvent,
    ProcessObservationEventPayload, ProcessReplayEventDraft, ProcessReplayOutcome,
    ProcessReplayStore, ProcessReplayStoreError, ProcessReplaySubscribeOutcome, ProcessSequence,
    SessionCursor, SessionObservationEvent, SessionRevision,
};
use lash_sansio::sync::MutexExt;
use lash_sansio::{ProcessId, SessionId};
use serde::Serialize;

use super::super::Meter;

#[derive(Default)]
struct Count(AtomicU64);
impl Count {
    fn add(&self, n: usize) {
        self.0.fetch_add(n as u64, Ordering::Relaxed);
    }
    fn get(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}

/// What crossed one process replay store's boundary.
#[derive(Clone, Debug, Default, Serialize)]
pub(super) struct ProcessReplayCounts {
    /// Store round trips that carried drafts.
    pub(super) publish_calls: u64,
    pub(super) drafts: u64,
    /// Events the store answered: drafts less redeliveries.
    pub(super) published: u64,
    pub(super) redelivered: u64,
    pub(super) committed_drafts: u64,
    pub(super) language_drafts: u64,
    pub(super) step_body_drafts: u64,
    pub(super) largest_batch: u64,
    pub(super) publish_errors: u64,
    pub(super) invalidate_all: u64,
    pub(super) invalidate_process: u64,
    pub(super) subscribe_calls: u64,
    pub(super) subscribe_gaps: u64,
    pub(super) replay_calls: u64,
    pub(super) earliest_cursor_calls: u64,
    /// The deepest backlog of provisional drafts between their observation
    /// and the store's answer, at millisecond resolution. Drafts an ingress
    /// overflow dropped never reach the store and are not in it.
    pub(super) deepest_backlog: u64,
}

#[derive(Default)]
struct ProcessCells {
    publish_calls: Count,
    drafts: Count,
    published: Count,
    committed_drafts: Count,
    language_drafts: Count,
    step_body_drafts: Count,
    largest_batch: AtomicU64,
    publish_errors: Count,
    invalidate_all: Count,
    invalidate_process: Count,
    subscribe_calls: Count,
    subscribe_gaps: Count,
    replay_calls: Count,
    earliest_cursor_calls: Count,
    /// `(observed_at_ms, answered_at_ms)` of each provisional draft.
    dwell: Mutex<Vec<(u64, u64)>>,
}

pub(in crate::boundary) struct ProcessReplayProbe {
    inner: Arc<dyn ProcessReplayStore>,
    cells: ProcessCells,
    meter: Meter,
}

fn now_ms() -> u64 {
    u64::try_from(chrono::Utc::now().timestamp_millis()).unwrap_or(0)
}

impl ProcessReplayProbe {
    pub(super) fn new(inner: Arc<dyn ProcessReplayStore>, meter: &Meter) -> Arc<Self> {
        Arc::new(Self {
            inner,
            cells: ProcessCells::default(),
            meter: meter.clone(),
        })
    }

    pub(super) fn counts(&self) -> ProcessReplayCounts {
        let cells = &self.cells;
        let drafts = cells.drafts.get();
        let published = cells.published.get();
        ProcessReplayCounts {
            publish_calls: cells.publish_calls.get(),
            drafts,
            published,
            redelivered: drafts.saturating_sub(published),
            committed_drafts: cells.committed_drafts.get(),
            language_drafts: cells.language_drafts.get(),
            step_body_drafts: cells.step_body_drafts.get(),
            largest_batch: cells.largest_batch.load(Ordering::Relaxed),
            publish_errors: cells.publish_errors.get(),
            invalidate_all: cells.invalidate_all.get(),
            invalidate_process: cells.invalidate_process.get(),
            subscribe_calls: cells.subscribe_calls.get(),
            subscribe_gaps: cells.subscribe_gaps.get(),
            replay_calls: cells.replay_calls.get(),
            earliest_cursor_calls: cells.earliest_cursor_calls.get(),
            deepest_backlog: deepest_backlog(&cells.dwell.lock_recover()),
        }
    }
}

/// The most drafts observed and not yet answered at any one answer.
fn deepest_backlog(dwell: &[(u64, u64)]) -> u64 {
    let mut edges = Vec::with_capacity(dwell.len() * 2);
    for (observed, answered) in dwell {
        edges.push((*observed, 1_i64));
        // A draft answered within its own millisecond still counts once.
        edges.push((answered.max(observed) + 1, -1_i64));
    }
    edges.sort_unstable();
    let (mut depth, mut deepest) = (0_i64, 0_i64);
    for (_, delta) in edges {
        depth += delta;
        deepest = deepest.max(depth);
    }
    u64::try_from(deepest).unwrap_or(0)
}

#[async_trait::async_trait]
impl ProcessReplayStore for ProcessReplayProbe {
    async fn publish(
        &self,
        process_id: &ProcessId,
        events: Vec<ProcessReplayEventDraft>,
    ) -> Result<Vec<Arc<ProcessObservationEvent>>, ProcessReplayStoreError> {
        let cells = &self.cells;
        cells.publish_calls.add(1);
        cells.drafts.add(events.len());
        cells
            .largest_batch
            .fetch_max(events.len() as u64, Ordering::Relaxed);
        let mut observed = Vec::new();
        for event in &events {
            match event.payload() {
                ProcessObservationEventPayload::Committed { .. } => cells.committed_drafts.add(1),
                ProcessObservationEventPayload::LanguageExecution(observation) => {
                    cells.language_drafts.add(1);
                    observed.push(observation.observed_at_ms);
                }
                ProcessObservationEventPayload::StepBodyStarted(observation) => {
                    cells.step_body_drafts.add(1);
                    observed.push(observation.observed_at_ms);
                }
            }
        }
        let start = Instant::now();
        let result = self.inner.publish(process_id, events).await;
        self.meter.operation(
            "process.replay.publish",
            process_id,
            if result.is_ok() { "ok" } else { "error" },
            start,
        );
        match &result {
            Ok(published) => cells.published.add(published.len()),
            Err(_) => cells.publish_errors.add(1),
        }
        let answered = now_ms();
        let mut dwell = cells.dwell.lock_recover();
        for at in observed {
            dwell.push((at, answered));
        }
        result
    }

    async fn replay_after_cursor(
        &self,
        cursor: &ProcessObservationCursor,
    ) -> Result<ProcessReplayOutcome, ProcessReplayStoreError> {
        self.cells.replay_calls.add(1);
        self.inner.replay_after_cursor(cursor).await
    }

    async fn subscribe_after_cursor(
        &self,
        cursor: &ProcessObservationCursor,
    ) -> Result<ProcessReplaySubscribeOutcome, ProcessReplayStoreError> {
        self.cells.subscribe_calls.add(1);
        let start = Instant::now();
        let outcome = self.inner.subscribe_after_cursor(cursor).await;
        self.meter.operation(
            "process.replay.subscribe",
            cursor.as_str(),
            if outcome.is_ok() { "ok" } else { "error" },
            start,
        );
        if matches!(outcome, Ok(ProcessReplaySubscribeOutcome::Gap(_))) {
            self.cells.subscribe_gaps.add(1);
        }
        outcome
    }

    fn publish_limits(&self) -> lash_core::ProcessReplayPublishLimits {
        self.inner.publish_limits()
    }

    async fn earliest_cursor(
        &self,
        process_id: &ProcessId,
        sequence: ProcessSequence,
    ) -> Result<ProcessObservationCursor, ProcessReplayStoreError> {
        self.cells.earliest_cursor_calls.add(1);
        self.inner.earliest_cursor(process_id, sequence).await
    }

    async fn invalidate_process(
        &self,
        process_id: &ProcessId,
    ) -> Result<(), ProcessReplayStoreError> {
        self.cells.invalidate_process.add(1);
        self.inner.invalidate_process(process_id).await
    }

    async fn invalidate_all(&self) -> Result<(), ProcessReplayStoreError> {
        self.cells.invalidate_all.add(1);
        let start = Instant::now();
        let result = self.inner.invalidate_all().await;
        self.meter.operation(
            "process.replay.invalidate_all",
            "all-processes",
            if result.is_ok() { "ok" } else { "error" },
            start,
        );
        result
    }
}

/// What crossed one live replay store's boundary.
#[derive(Clone, Debug, Default, Serialize)]
pub(super) struct LiveReplayCounts {
    pub(super) publish_calls: u64,
    pub(super) drafts: u64,
    pub(super) published: u64,
    pub(super) largest_batch: u64,
    pub(super) publish_errors: u64,
    pub(super) invalidate_all: u64,
    pub(super) invalidate_session: u64,
    pub(super) subscribe_calls: u64,
    pub(super) subscribe_gaps: u64,
    pub(super) replay_calls: u64,
    pub(super) replay_gaps: u64,
    pub(super) replayed_events: u64,
}

#[derive(Default)]
struct LiveCells {
    publish_calls: Count,
    drafts: Count,
    published: Count,
    largest_batch: AtomicU64,
    publish_errors: Count,
    invalidate_all: Count,
    invalidate_session: Count,
    subscribe_calls: Count,
    subscribe_gaps: Count,
    replay_calls: Count,
    replay_gaps: Count,
    replayed_events: Count,
}

pub(in crate::boundary) struct LiveReplayProbe {
    inner: Arc<dyn LiveReplayStore>,
    cells: LiveCells,
    meter: Meter,
}

impl LiveReplayProbe {
    pub(super) fn new(inner: Arc<dyn LiveReplayStore>, meter: &Meter) -> Arc<Self> {
        Arc::new(Self {
            inner,
            cells: LiveCells::default(),
            meter: meter.clone(),
        })
    }

    pub(super) fn counts(&self) -> LiveReplayCounts {
        let cells = &self.cells;
        LiveReplayCounts {
            publish_calls: cells.publish_calls.get(),
            drafts: cells.drafts.get(),
            published: cells.published.get(),
            largest_batch: cells.largest_batch.load(Ordering::Relaxed),
            publish_errors: cells.publish_errors.get(),
            invalidate_all: cells.invalidate_all.get(),
            invalidate_session: cells.invalidate_session.get(),
            subscribe_calls: cells.subscribe_calls.get(),
            subscribe_gaps: cells.subscribe_gaps.get(),
            replay_calls: cells.replay_calls.get(),
            replay_gaps: cells.replay_gaps.get(),
            replayed_events: cells.replayed_events.get(),
        }
    }
}

#[async_trait::async_trait]
impl LiveReplayStore for LiveReplayProbe {
    async fn publish(
        &self,
        session_id: &SessionId,
        revision: SessionRevision,
        events: Vec<LiveReplayEventDraft>,
    ) -> Result<Vec<Arc<SessionObservationEvent>>, LiveReplayStoreError> {
        let cells = &self.cells;
        cells.publish_calls.add(1);
        cells.drafts.add(events.len());
        cells
            .largest_batch
            .fetch_max(events.len() as u64, Ordering::Relaxed);
        let start = Instant::now();
        let result = self.inner.publish(session_id, revision, events).await;
        self.meter.operation(
            "session.replay.publish",
            format!("{session_id}/revision:{revision:?}"),
            if result.is_ok() { "ok" } else { "error" },
            start,
        );
        match &result {
            Ok(published) => cells.published.add(published.len()),
            Err(_) => cells.publish_errors.add(1),
        }
        result
    }

    async fn replay_after_cursor(
        &self,
        cursor: &SessionCursor,
    ) -> Result<LiveReplayOutcome, LiveReplayStoreError> {
        self.cells.replay_calls.add(1);
        let start = Instant::now();
        let outcome = self.inner.replay_after_cursor(cursor).await;
        self.meter.operation(
            "session.replay.replay",
            cursor.as_str(),
            if outcome.is_ok() { "ok" } else { "error" },
            start,
        );
        match &outcome {
            Ok(LiveReplayOutcome::Replayed(events)) => self.cells.replayed_events.add(events.len()),
            Ok(LiveReplayOutcome::Gap(_)) => self.cells.replay_gaps.add(1),
            Err(_) => {}
        }
        outcome
    }

    async fn subscribe_after_cursor(
        &self,
        cursor: &SessionCursor,
    ) -> Result<LiveReplaySubscribeOutcome, LiveReplayStoreError> {
        self.cells.subscribe_calls.add(1);
        let start = Instant::now();
        let outcome = self.inner.subscribe_after_cursor(cursor).await;
        self.meter.operation(
            "session.replay.subscribe",
            cursor.as_str(),
            if outcome.is_ok() { "ok" } else { "error" },
            start,
        );
        if matches!(outcome, Ok(LiveReplaySubscribeOutcome::Gap(_))) {
            self.cells.subscribe_gaps.add(1);
        }
        outcome
    }

    fn current_cursor(&self, session_id: &SessionId, revision: SessionRevision) -> SessionCursor {
        self.inner.current_cursor(session_id, revision)
    }

    fn earliest_cursor(&self, session_id: &SessionId, revision: SessionRevision) -> SessionCursor {
        self.inner.earliest_cursor(session_id, revision)
    }

    async fn invalidate_session(&self, session_id: &SessionId) -> Result<(), LiveReplayStoreError> {
        self.cells.invalidate_session.add(1);
        self.inner.invalidate_session(session_id).await
    }

    async fn invalidate_all(&self) -> Result<(), LiveReplayStoreError> {
        self.cells.invalidate_all.add(1);
        let start = Instant::now();
        let result = self.inner.invalidate_all().await;
        self.meter.operation(
            "session.replay.invalidate_all",
            "all-sessions",
            if result.is_ok() { "ok" } else { "error" },
            start,
        );
        result
    }

    async fn trim_session(&self, session_id: &SessionId) -> Result<(), LiveReplayStoreError> {
        self.inner.trim_session(session_id).await
    }
}
