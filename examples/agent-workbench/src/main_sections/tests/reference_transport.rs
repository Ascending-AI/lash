//! Reference client transport for the workbench's recoverable-chat
//! observation stream, plus the deterministic corpus proving its invariant:
//! **one stable output identity per turn** across live delivery, observer
//! disconnect and reconnect, trimmed-gap refetch, and recovery redrives
//! (FIG-764).
//!
//! The transport is the client half of `/api/observations`. It consumes
//! [`ObservationStreamItem`]s — decoded either in-process or from the route's
//! NDJSON body — and folds every delivery leg into one [`TurnOutputRow`] per
//! `TurnId`. Provisional output is owned by typed turn provenance. Committed
//! records retain their opaque source row identity across subscription legs,
//! replay-gap replacement and redriven turns.
//!
//! The mapping, in the order the wire presents it:
//!
//! * `cursor` checkpoints are the only cursor a host persists between
//!   connections. The per-event cursor is *delivery* identity, not resume
//!   state; the persisted cursor trails applied events, so a resume
//!   legitimately redelivers them — which is why applied-event dedupe is part
//!   of the contract rather than a nicety.
//! * `observation` events carry `(session_id, replay_incarnation_id, cursor)`
//!   — the remote encoding of `SessionObservationEventId`. A redelivered identity
//!   applies once, never twice.
//! * Turn activity folds into the turn's one output row: prose deltas
//!   accumulate under their activity correlation id (so a
//!   `model_attempt_reset` retracts only the superseded attempt) and the
//!   preview stays provisional until canonical committed rows replace it.
//! * `terminal_replacement`, `resident_replacement`, and `replay_gap` all ask
//!   for one thing — a refetch of the settled read view. The gap additionally
//!   clears the applied-identity window: everything at or before the
//!   replacement snapshot is superseded.
//! * `turn_started` under a turn id the transport already knows is a recovery
//!   redrive: the abandoned shift's provisional copy is superseded in place.
//!   The row — and so the output identity — stays the same.
//! * `replace_from_settled` writes the settled text for each turn the snapshot
//!   carries, keyed by the message's own turn provenance, and retires the
//!   row's provisional copy. Streamed partial text is never authoritative.

use super::*;
use lash::SessionId;
use lash::TurnId;
use lash::direct::{LlmStreamEvent, StreamBlockIdentity};
use lash::provider::{
    GenerationRetryGuarantee, LlmRequest, LlmTransportError, ProviderFailureKind, ProviderOptions,
    ProviderReliability, TransportRetryVerdict,
};
use lash::{StreamBlockEvent, StreamBlockKind};

use lash::TurnEvent;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};

/// Mirrors the bounded dedupe window of `lash::observe::SessionObservationStream`: the dedupe
/// window is bounded because an honest client only needs to absorb redelivery
/// inside a replay suffix, not dedupe history forever.
const MAX_APPLIED_EVENT_IDS: usize = 4096;

/// The wire identity of one delivered observation event — the remote encoding
/// of `lash::observe::SessionObservationEventId`. The incarnation makes
/// the identity safe across replay-store restarts: a rebuilt store may reuse a
/// cursor but cannot reproduce the old identity.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct DeliveredEventId {
    session_id: String,
    replay_incarnation_id: String,
    cursor: String,
}

impl DeliveredEventId {
    fn of(event: &ObservationEvent) -> Self {
        Self {
            session_id: event.session_id.to_string(),
            replay_incarnation_id: event.replay_incarnation_id.clone(),
            cursor: event.cursor.clone(),
        }
    }
}

/// What the transport asks its host to do after an item.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum TransportDirective {
    #[default]
    None,
    /// A checkpoint — a terminal or resident replacement, or a replay gap —
    /// made the settled read view authoritative. Refetch it before trusting
    /// rendered text again.
    RefetchSettled,
}

/// One turn's single output row: the slot `{turn_id}`
/// renders. Live copies are provisional; the settled read view owns the
/// canonical text.
#[derive(Clone, Debug, Default)]
pub(crate) struct TurnOutputRow {
    /// Provisional prose chunks keyed by the activity correlation id that
    /// produced them, so a `model_attempt_reset` retracts only the superseded
    /// attempt.
    provisional_prose: BTreeMap<String, String>,
    /// The turn's reported terminal value — provisional until the settled
    /// read view speaks for the turn.
    /// Text the settled read view last confirmed for this turn.
    settled_text: Option<String>,
}

impl TurnOutputRow {
    /// Provisional text accumulated so far, newest-correlation-id last. Empty
    /// once the settled view has spoken for the turn.
    pub(crate) fn provisional_text(&self) -> String {
        self.provisional_prose.values().cloned().collect()
    }

    /// The settled view's canonical text, when it has spoken for the turn.
    pub(crate) fn settled_text(&self) -> Option<&str> {
        self.settled_text.as_deref()
    }

    pub(crate) fn is_settled(&self) -> bool {
        self.settled_text.is_some()
    }

    /// What a client renders: provisional text while a (re-)shift is live —
    /// it is newer than the last settled read — else the settled text, else
    /// the provisional terminal value.
    pub(crate) fn rendered(&self) -> Option<String> {
        let provisional = self.provisional_text();
        if !provisional.is_empty() {
            return Some(provisional);
        }
        self.settled_text.clone()
    }
}

/// The reference mapping itself: every wire item folds into per-turn output
/// rows and a trailing resume cursor.
#[derive(Default)]
pub(crate) struct ReferenceTransport {
    /// The cursor a host persists between connections. Advanced only at the
    /// stream's own checkpoints, so it trails applied events and a resume
    /// redelivers them for dedupe.
    resume_cursor: Option<String>,
    /// Bounded window of applied delivery identities.
    applied: BTreeSet<DeliveredEventId>,
    applied_order: VecDeque<DeliveredEventId>,
    /// Redeliveries dedupe dropped. The corpus asserts this is non-zero where
    /// redelivery is expected — an invariant proven on a stream that never
    /// repeated anything would be vacuous.
    redeliveries_ignored: usize,
    /// One output row per turn, keyed by the wire's own turn attribution.
    outputs: BTreeMap<TurnId, TurnOutputRow>,
}

impl ReferenceTransport {
    /// Fold one wire item in. Items arrive in-process or decoded from the
    /// route's NDJSON body — the transport does not care which.
    pub(crate) fn apply(&mut self, item: &ObservationStreamItem) -> TransportDirective {
        match item {
            ObservationStreamItem::Cursor { cursor } => {
                self.resume_cursor = Some(cursor.clone());
                TransportDirective::None
            }
            ObservationStreamItem::Observation { event } => {
                if !self.mark_applied(DeliveredEventId::of(&event.body)) {
                    return TransportDirective::None;
                }
                self.fold_event(&event.body);
                TransportDirective::None
            }
            ObservationStreamItem::TerminalReplacement { event, cursor }
            | ObservationStreamItem::ResidentReplacement { event, cursor } => {
                if !self.mark_applied(DeliveredEventId::of(&event.body)) {
                    return TransportDirective::None;
                }
                self.fold_event(&event.body);
                if let Some(turn_id) = event.body.turn_id.clone() {
                    self.outputs.entry(turn_id).or_default();
                }
                // The replacement's snapshot cursor — not the event's own —
                // is the point a resume can safely persist.
                self.resume_cursor = Some(cursor.clone());
                TransportDirective::RefetchSettled
            }
            ObservationStreamItem::ReplayGap { observation, gap } => {
                assert_eq!(
                    gap.body.latest_cursor, observation.body.cursor,
                    "a replay gap's replacement snapshot sits at the gap's latest cursor"
                );
                self.applied.clear();
                self.applied_order.clear();
                self.resume_cursor = Some(gap.body.latest_cursor.clone());
                TransportDirective::RefetchSettled
            }
        }
    }

    /// Replace provisional state with the settled read view — the only
    /// authoritative text the transport ever renders. Rows for turns the
    /// snapshot does not yet carry keep their live provisional state; a
    /// redriven turn that committed twice collapses to the newest copy.
    pub(crate) fn replace_from_settled(&mut self, snapshot: &StateReadSnapshot) {
        self.resume_cursor = Some(snapshot.observation.cursor.clone());
        for record in snapshot
            .transcript
            .iter()
            .filter(|record| record.suppressed.is_none() && record.provenance.is_turn_reply)
        {
            let Some(turn_id) = &record.provenance.turn_id else {
                continue;
            };
            let row = self.outputs.entry(turn_id.clone()).or_default();
            row.settled_text = Some(record.content.text.clone());
            row.provisional_prose.clear();
        }
    }

    /// The cursor to resume from on the next connection — a checkpoint value,
    /// never a per-event one.
    pub(crate) fn resume_cursor(&self) -> Option<&str> {
        self.resume_cursor.as_deref()
    }

    pub(crate) fn redeliveries_ignored(&self) -> usize {
        self.redeliveries_ignored
    }

    /// Every turn's output row, keyed by turn.
    pub(crate) fn outputs(&self) -> &BTreeMap<TurnId, TurnOutputRow> {
        &self.outputs
    }

    /// The rendered row identities — always `{turn_id}`,
    /// one per turn no matter how many delivery legs produced the row.
    pub(crate) fn output_keys(&self) -> Vec<String> {
        self.outputs.keys().map(ToString::to_string).collect()
    }

    fn mark_applied(&mut self, id: DeliveredEventId) -> bool {
        if !self.applied.insert(id.clone()) {
            self.redeliveries_ignored += 1;
            return false;
        }
        self.applied_order.push_back(id);
        while self.applied_order.len() > MAX_APPLIED_EVENT_IDS {
            if let Some(expired) = self.applied_order.pop_front() {
                self.applied.remove(&expired);
            }
        }
        true
    }

    fn fold_event(&mut self, event: &ObservationEvent) {
        if let ObservationPayload::Committed { rows, .. } = &event.event {
            for record in rows
                .iter()
                .filter(|record| record.suppressed.is_none() && record.provenance.is_turn_reply)
            {
                let Some(turn_id) = &record.provenance.turn_id else {
                    continue;
                };
                let row = self.outputs.entry(turn_id.clone()).or_default();
                row.settled_text = Some(record.content.text.clone());
                row.provisional_prose.clear();
            }
            return;
        }
        let Some(turn_id) = event.turn_id.clone() else {
            return;
        };
        let ObservationPayload::TurnActivity { activity } = &event.event else {
            // No other turn-scoped payload carries output text, but the turn's
            // row exists as soon as any of its events do.
            self.outputs.entry(turn_id).or_default();
            return;
        };
        let row = self.outputs.entry(turn_id).or_default();
        match &activity.event {
            TurnEvent::TurnStarted { .. } => {
                // A fresh shift under an existing turn id is a redrive: its
                // provisional copy supersedes whatever the abandoned shift
                // left behind. The settled text stays — it is still the last
                // canonical word until the next refetch.
                row.provisional_prose.clear();
            }
            TurnEvent::StreamBlock(StreamBlockEvent::Delta {
                kind: StreamBlockKind::AssistantText,
                text,
                ..
            }) => {
                row.provisional_prose
                    .entry(activity.correlation_id.clone())
                    .or_default()
                    .push_str(text);
            }
            TurnEvent::ModelAttemptReset {
                assistant_prose_correlation_ids,
                ..
            } => {
                for correlation_id in assistant_prose_correlation_ids {
                    row.provisional_prose.remove(correlation_id.0.as_ref());
                }
            }
            _ => {}
        }
    }
}

/// The client half of `/api/observations`: NDJSON lines in,
/// `ObservationStreamItem`s out — the same type the route serializes.
struct NdjsonObservationStream {
    body: axum::body::BodyDataStream,
    buffered: Vec<u8>,
}

impl NdjsonObservationStream {
    /// The next complete NDJSON line, decoded. Chunks are bytes, not lines —
    /// the reference client splits on `\n` like any NDJSON consumer must.
    async fn next_item(&mut self) -> ObservationStreamItem {
        use futures_util::StreamExt;
        loop {
            if let Some(end) = self.buffered.iter().position(|byte| *byte == b'\n') {
                let line: Vec<u8> = self.buffered.drain(..=end).collect();
                if line.iter().all(|byte| byte.is_ascii_whitespace()) {
                    continue;
                }
                return serde_json::from_slice(&line).expect("decode observation stream line");
            }
            let chunk = tokio::time::timeout(Duration::from_secs(2), self.body.next())
                .await
                .expect("timed out waiting for an observation stream chunk")
                .expect("observation stream closed")
                .expect("observation body chunk");
            self.buffered.extend_from_slice(&chunk);
        }
    }
}

/// Open the production route and read its real NDJSON body. `cursor` is the
/// transport's persisted resume cursor, exactly as a reconnecting client
/// would send it.
async fn connect_observations(
    state: &AppState,
    session_id: &SessionId,
    cursor: Option<&str>,
) -> NdjsonObservationStream {
    let response = session_observations_with_shutdown(
        State(state.clone()),
        Query(EventsQuery {
            cursor: cursor.map(str::to_string),
            session_id: Some(session_id.clone()),
        }),
        None,
    )
    .await
    .expect("open observation stream");
    assert_eq!(
        response
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok()),
        Some("application/x-ndjson; charset=utf-8"),
        "the observation route must answer NDJSON"
    );
    NdjsonObservationStream {
        body: response.into_body().into_data_stream(),
        buffered: Vec::new(),
    }
}

/// Refetch the settled read view through the production `/api/state` handler
/// and install it — the only refetch a checkpoint directive ever asks for.
async fn refetch_settled(
    state: &AppState,
    session_id: &SessionId,
    transport: &mut ReferenceTransport,
) {
    let Json(snapshot) = app_state(
        State(state.clone()),
        Query(SessionQuery {
            session_id: Some(session_id.clone()),
        }),
    )
    .await
    .expect("refetch the settled read view");
    transport.replace_from_settled(&snapshot);
}

/// Apply items, refetching at every checkpoint directive, until the turn's
/// row is settled. The settle condition is the row itself — never a count of
/// wire items — so the loop is correct no matter how many commits a turn
/// makes.
async fn apply_until_settled(
    state: &AppState,
    session_id: &SessionId,
    client: &mut NdjsonObservationStream,
    transport: &mut ReferenceTransport,
    turn_id: &TurnId,
) {
    for _ in 0..256 {
        let item = client.next_item().await;
        if transport.apply(&item) == TransportDirective::RefetchSettled {
            refetch_settled(state, session_id, transport).await;
            if transport
                .outputs()
                .get(turn_id)
                .is_some_and(TurnOutputRow::is_settled)
            {
                return;
            }
        }
    }
    panic!("turn {turn_id} never settled");
}

/// Execute one turn under `turn_id` through the session's engine; the
/// workbench's run follower settles it on the page.
async fn drive_reference_turn(session: &lash::LashSession, turn_id: &TurnId, prompt: &str) {
    session
        .send(lash::TurnInput::text(prompt))
        .id(turn_id.clone())
        .output()
        .await
        .expect("run turn");
}

/// A bare-prose answer needs no `finish` call: the runtime commits the reply
/// itself, so the settled view carries a canonical assistant row for the
/// turn.
fn prose_provider(answers: &[&str]) -> ProviderHandle {
    scripted_cells_provider(answers.iter().map(|answer| answer.to_string()).collect())
}

/// Script `request.stream_events` the way `failure_provider` does: a delta the
/// runtime records as streamed activity for the in-flight attempt.
fn send_delta(request: &LlmRequest, text: &str) {
    if let Some(events) = request.stream_events.as_ref() {
        events.send(LlmStreamEvent::Block(StreamBlockEvent::Delta {
            kind: StreamBlockKind::AssistantText,
            block: StreamBlockIdentity::new("text:0", 0),
            text: text.to_string(),
        }));
    }
}

/// The first call dies after partial output on a retryable stream boundary;
/// the runtime re-buys the generation and calls again. Zero delays — the
/// corpus never waits on wall-clock backoff.
fn retried_attempt_provider(superseded: &'static str, answer: &'static str) -> ProviderHandle {
    let calls = Arc::new(AtomicUsize::new(0));
    lash::testing::TestProvider::builder()
        .kind("workbench-harness")
        .requires_streaming(true)
        .generation_retry_guarantee(GenerationRetryGuarantee::Idempotent)
        .options(ProviderOptions {
            reliability: ProviderReliability::default()
                .max_attempts(Some(2))
                .base_delay_ms(0)
                .max_delay_ms(0),
            ..ProviderOptions::default()
        })
        .complete(move |request: LlmRequest| {
            let calls = Arc::clone(&calls);
            async move {
                if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    send_delta(&request, superseded);
                    return Err(LlmTransportError::new("deterministic retry boundary")
                        .with_kind(ProviderFailureKind::Stream)
                        .with_output_started(true)
                        .with_partial_response(text_response(superseded))
                        .with_retry_verdict(TransportRetryVerdict::RetryableTransient));
                }
                send_delta(&request, answer);
                Ok(text_response(answer))
            }
        })
        .build()
        .into_handle()
}

/// Apply wire items until one turn-scoped event for `turn_id` has landed —
/// the point a mid-stream disconnect provably leaves applied events ahead of
/// the persisted checkpoint.
async fn apply_through_turn_activity(
    client: &mut NdjsonObservationStream,
    transport: &mut ReferenceTransport,
    turn_id: &TurnId,
) {
    for _ in 0..256 {
        let item = client.next_item().await;
        let saw_turn = matches!(
            &item,
            ObservationStreamItem::Observation { event }
                if event.body.turn_id.as_ref() == Some(turn_id)
        );
        transport.apply(&item);
        if saw_turn {
            return;
        }
    }
    panic!("no live activity for {turn_id} arrived");
}

/// Live delivery, a mid-stream disconnect, and a reconnect whose persisted
/// cursor legitimately redelivers applied events: dedupe, not a second row,
/// absorbs the replay.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_output_identity_per_turn_across_disconnect_and_redelivery() {
    const FIRST_ANSWER: &str = "first canonical answer";
    const SECOND_ANSWER: &str = "second canonical answer";
    let workbench = Workbench::builder(prose_provider(&[FIRST_ANSWER, SECOND_ANSWER]))
        .build()
        .await;
    let state = &workbench.state;
    let session_id = state.current_session_id();
    let session = state
        .create_or_open_session(&session_id, "test")
        .await
        .expect("open session");

    let mut transport = ReferenceTransport::default();
    let mut client = connect_observations(state, &session_id, transport.resume_cursor()).await;

    // Turn-one settles; the persisted resume cursor lands on its commit
    // checkpoint — a real replay position, unlike the never-published
    // snapshot cursor a first connect echoes back.
    drive_reference_turn(&session, &TurnId::from("turn-one"), "first question").await;
    apply_until_settled(
        state,
        &session_id,
        &mut client,
        &mut transport,
        &TurnId::from("turn-one"),
    )
    .await;
    let row = transport
        .outputs()
        .get(&TurnId::from("turn-one"))
        .expect("turn-one output row");
    assert_eq!(
        row.provisional_text(),
        "",
        "the settled read view retires streamed partial text"
    );
    assert_eq!(row.settled_text(), Some(FIRST_ANSWER));

    // Turn-two starts while the client watches, then the connection drops
    // with the persisted cursor still trailing the applied turn-two events.
    drive_reference_turn(&session, &TurnId::from("turn-two"), "second question").await;
    apply_through_turn_activity(&mut client, &mut transport, &TurnId::from("turn-two")).await;
    drop(client);

    // Reconnect from the trailing cursor: replay redelivers the applied
    // turn-two events, dedupe absorbs them, and the same row keeps filling
    // in.
    let mut client = connect_observations(state, &session_id, transport.resume_cursor()).await;
    apply_until_settled(
        state,
        &session_id,
        &mut client,
        &mut transport,
        &TurnId::from("turn-two"),
    )
    .await;
    assert!(
        transport.redeliveries_ignored() > 0,
        "a resume from a trailing cursor must redeliver applied events"
    );
    assert_eq!(
        transport.output_keys(),
        vec!["turn-one".to_string(), "turn-two".to_string()]
    );
    assert_eq!(
        transport
            .outputs()
            .get(&TurnId::from("turn-two"))
            .and_then(TurnOutputRow::settled_text),
        Some(SECOND_ANSWER)
    );
    drop(client);
}

/// A persisted cursor trimmed out of the bounded replay window answers with a
/// gap and a replacement snapshot, not a second row: the refetch writes the
/// turn's output onto the same identity it had live.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn trimmed_gap_recovery_replaces_the_same_output_identity() {
    const FIRST_ANSWER: &str = "trimmed first answer";
    const SECOND_ANSWER: &str = "trimmed second answer";
    let workbench = Workbench::builder(prose_provider(&[FIRST_ANSWER, SECOND_ANSWER]))
        .live_replay(Arc::new(lash::observe::InMemoryLiveReplayStore::new(
            lash::observe::InMemoryLiveReplayStoreConfig {
                max_events_per_session: 1,
                ..lash::observe::InMemoryLiveReplayStoreConfig::standard()
            },
        )))
        .build()
        .await;
    let state = &workbench.state;
    let session_id = state.current_session_id();
    let session = state
        .create_or_open_session(&session_id, "test")
        .await
        .expect("open session");

    let mut transport = ReferenceTransport::default();
    let mut client = connect_observations(state, &session_id, transport.resume_cursor()).await;

    // The client watches turn-one live — the row exists provisionally — then
    // goes away before its checkpoints complete.
    drive_reference_turn(&session, &TurnId::from("turn-one"), "first question").await;
    let mut saw_turn_one_activity = false;
    for _ in 0..256 {
        let item = client.next_item().await;
        if let ObservationStreamItem::Observation { event } = &item {
            saw_turn_one_activity |= event.body.turn_id.as_ref() == Some(&TurnId::from("turn-one"));
        }
        transport.apply(&item);
        if saw_turn_one_activity {
            break;
        }
    }
    assert!(saw_turn_one_activity, "turn-one activity must stream live");
    drop(client);

    // Turn-two's commits push turn-one's events out of the one-deep replay
    // window while the client is gone.
    drive_reference_turn(&session, &TurnId::from("turn-two"), "second question").await;

    // The persisted cursor is trimmed: the stream answers with a gap whose
    // replacement snapshot the transport installs from the settled view.
    let mut client = connect_observations(state, &session_id, transport.resume_cursor()).await;
    let mut gap_reason = None;
    for _ in 0..256 {
        let item = client.next_item().await;
        if let ObservationStreamItem::ReplayGap { gap, .. } = &item {
            gap_reason = Some(gap.body.reason);
        }
        if transport.apply(&item) == TransportDirective::RefetchSettled {
            refetch_settled(state, &session_id, &mut transport).await;
        }
        if gap_reason.is_some()
            && transport
                .outputs()
                .values()
                .filter(|row| row.is_settled())
                .count()
                == 2
        {
            break;
        }
    }
    assert_eq!(
        gap_reason,
        Some(lash::observe::LiveReplayGapReason::Trimmed),
        "a trimmed persisted cursor must answer with a trimmed gap"
    );
    assert_eq!(
        transport.output_keys(),
        vec!["turn-one".to_string(), "turn-two".to_string()],
        "gap recovery replaces rows, it never mints new ones"
    );
    assert_eq!(
        transport
            .outputs()
            .get(&TurnId::from("turn-one"))
            .and_then(TurnOutputRow::settled_text),
        Some(FIRST_ANSWER),
        "the trimmed-gap refetch restores turn-one's canonical text"
    );
    assert_eq!(
        transport
            .outputs()
            .get(&TurnId::from("turn-two"))
            .and_then(TurnOutputRow::settled_text),
        Some(SECOND_ANSWER)
    );

    drop(client);
}

/// A provider retry inside one shift: the first attempt dies after partial
/// output, the runtime re-buys the generation, and `model_attempt_reset`
/// retracts the superseded copy on the same row — never a second identity,
/// never stale partial text left standing over the canonical answer.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_retried_attempt_replaces_partial_prose_on_the_same_row() {
    const SUPERSEDED: &str = "superseded partial from the failed attempt";
    const ANSWER: &str = "answer from the retried attempt";
    let workbench = Workbench::builder(retried_attempt_provider(SUPERSEDED, ANSWER))
        .build()
        .await;
    let state = &workbench.state;
    let session_id = state.current_session_id();
    let session = state
        .create_or_open_session(&session_id, "test")
        .await
        .expect("open session");

    let mut transport = ReferenceTransport::default();
    let mut client = connect_observations(state, &session_id, transport.resume_cursor()).await;
    drive_reference_turn(&session, &TurnId::from("turn-one"), "the question").await;

    // While the retried attempt is live, the superseded attempt's partial is
    // already retracted — `model_attempt_reset` removed it in place.
    for _ in 0..256 {
        let item = client.next_item().await;
        transport.apply(&item);
        if transport
            .outputs()
            .get(&TurnId::from("turn-one"))
            .is_some_and(|row| row.provisional_text() == ANSWER)
        {
            break;
        }
    }
    let row = transport
        .outputs()
        .get(&TurnId::from("turn-one"))
        .expect("turn-one output row");
    assert_eq!(
        row.provisional_text(),
        ANSWER,
        "the retried attempt's prose replaced the superseded partial"
    );

    apply_until_settled(
        state,
        &session_id,
        &mut client,
        &mut transport,
        &TurnId::from("turn-one"),
    )
    .await;
    let row = transport
        .outputs()
        .get(&TurnId::from("turn-one"))
        .expect("turn-one output row");
    assert_eq!(
        transport.output_keys(),
        vec!["turn-one".to_string()],
        "a retried attempt must not mint a second output identity"
    );
    assert_eq!(row.settled_text(), Some(ANSWER));
    assert!(
        !row.rendered().unwrap_or_default().contains(SUPERSEDED),
        "the retried attempt's superseded partial must not survive: {:?}",
        row.rendered()
    );
    drop(client);
}

/// A session handed to a new owner mid-turn keeps the turn's output
/// identity: the old build drains the session at its next committed phase,
/// the new build over the same stores runs the turn on under the accepted
/// run id, and the page, reconnecting to the new build, refetches the settled
/// row instead of minting another. A same-id send then observes the settled
/// run without calling the model, and the next turn gets its own row.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_turn_resumed_by_a_new_owner_keeps_its_output_identity() {
    const PARTIAL: &str = "partial prose from the first owner";
    const RESUMED_ANSWER: &str = "answer from the resuming owner";
    const NEXT_ANSWER: &str = "answer from the next turn";
    let (entered_tx, mut entered) = mpsc::unbounded_channel::<usize>();
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let calls = Arc::new(AtomicUsize::new(0));
    let provider = lash::testing::TestProvider::builder()
        .kind("workbench-harness")
        .requires_streaming(true)
        .options(ProviderOptions {
            reliability: ProviderReliability::disabled(),
            ..ProviderOptions::default()
        })
        .complete({
            let release = Arc::clone(&release);
            let calls = Arc::clone(&calls);
            move |request: LlmRequest| {
                let call = calls.fetch_add(1, Ordering::SeqCst);
                let entered_tx = entered_tx.clone();
                let release = Arc::clone(&release);
                async move {
                    let _ = entered_tx.send(call);
                    match call {
                        0 => {
                            send_delta(&request, PARTIAL);
                            release
                                .acquire()
                                .await
                                .expect("the gate stays open")
                                .forget();
                            Ok(text_response(
                                "<typescript>\nconst marker = 1;\n</typescript>",
                            ))
                        }
                        1 => {
                            send_delta(&request, RESUMED_ANSWER);
                            Ok(text_response(RESUMED_ANSWER))
                        }
                        2 => {
                            send_delta(&request, NEXT_ANSWER);
                            Ok(text_response(NEXT_ANSWER))
                        }
                        other => panic!("unexpected provider call {other}"),
                    }
                }
            }
        })
        .build()
        .into_handle();
    let old = Workbench::builder(provider.clone()).build().await;
    let session_id = old.state.current_session_id();
    let old_session = old
        .state
        .create_or_open_session(&session_id, "test")
        .await
        .expect("open session");
    let mut transport = ReferenceTransport::default();
    let mut client = connect_observations(&old.state, &session_id, transport.resume_cursor()).await;
    let turn_one = TurnId::from("turn-one");
    let send = tokio::spawn({
        let turn_one = turn_one.clone();
        async move { drive_reference_turn(&old_session, &turn_one, "the question").await }
    });
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(30), entered.recv())
            .await
            .expect("the first call starts"),
        Some(0)
    );
    apply_through_turn_activity(&mut client, &mut transport, &turn_one).await;
    drop(client);

    let drain = tokio::spawn({
        let core = old.state.core.clone();
        async move { core.drain().await }
    });
    release.add_permits(1);
    tokio::time::timeout(Duration::from_secs(30), drain)
        .await
        .expect("the old build drains")
        .expect("the drain task")
        .expect("the old build releases its sessions");
    let new = Workbench::builder(provider)
        .stores(Arc::clone(&old.stores))
        .build()
        .await;
    let state = &new.state;
    tokio::time::timeout(Duration::from_secs(30), send)
        .await
        .expect("the send answers once the new owner settles the turn")
        .expect("the send task");

    let mut client = connect_observations(state, &session_id, transport.resume_cursor()).await;
    apply_until_settled(state, &session_id, &mut client, &mut transport, &turn_one).await;
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "the committed first call is never bought again"
    );
    assert_eq!(transport.output_keys(), vec!["turn-one".to_string()]);
    let row = transport
        .outputs()
        .get(&turn_one)
        .expect("turn-one output row");
    assert_eq!(row.settled_text(), Some(RESUMED_ANSWER));
    assert_eq!(row.provisional_text(), "", "stale partial text is cleared");
    assert!(
        !row.rendered().unwrap_or_default().contains(PARTIAL),
        "the first owner's partial must not survive: {:?}",
        row.rendered()
    );

    let session = state
        .open_session(&session_id, "test")
        .await
        .expect("open the session on the new owner");
    let retried = session
        .send(lash::TurnInput::text("the question"))
        .id(lash::TurnId::parse("turn-one").expect("nonblank host identity"))
        .output()
        .await
        .expect("same-id retry observes the settled run");
    assert_eq!(retried.assistant_message(), Some(RESUMED_ANSWER));
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(transport.output_keys(), vec!["turn-one".to_string()]);

    drive_reference_turn(&session, &TurnId::from("turn-two"), "next question").await;
    apply_until_settled(
        state,
        &session_id,
        &mut client,
        &mut transport,
        &TurnId::from("turn-two"),
    )
    .await;
    assert_eq!(
        transport.output_keys(),
        vec!["turn-one".to_string(), "turn-two".to_string()],
    );
    assert_eq!(
        transport
            .outputs()
            .get(&TurnId::from("turn-two"))
            .and_then(|row| row.settled_text()),
        Some(NEXT_ANSWER)
    );
    drop(client);
}
