//! Reference client transport for the workbench's recoverable-chat
//! observation stream, plus the deterministic corpus proving its invariant:
//! **one stable output identity per turn** across live delivery, observer
//! disconnect and reconnect, trimmed-gap refetch, and recovery re-drives
//! (FIG-764).
//!
//! The transport is the client half of `/api/observations`. It consumes
//! [`ObservationStreamItem`]s — decoded either in-process or from the route's
//! NDJSON body — and folds every delivery leg into one [`TurnOutputRow`] per
//! `TurnId`. The row renders under the workbench's stable
//! `workbench-assistant:{turn_id}` identity, so a second subscription leg, a
//! replay-gap replacement, or a re-driven turn can never mint a second output
//! row for the same turn.
//!
//! The mapping, in the order the wire presents it:
//!
//! * `cursor` checkpoints are the only cursor a host persists between
//!   connections. The per-event cursor is *delivery* identity, not resume
//!   state; the persisted cursor trails applied events, so a resume
//!   legitimately redelivers them — which is why applied-event dedupe is part
//!   of the contract rather than a nicety.
//! * `observation` events carry `(session_id, replay_incarnation_id, cursor)`
//!   — the remote encoding of `RecoverableChatEventId`. A redelivered identity
//!   applies once, never twice.
//! * Turn activity folds into the turn's one output row: prose deltas
//!   accumulate under their activity correlation id (so a
//!   `model_attempt_reset` retracts only the superseded attempt) and the
//!   reported final value stays provisional until the settled read view
//!   confirms it.
//! * `terminal_replacement`, `resident_replacement`, and `replay_gap` all ask
//!   for one thing — a refetch of the settled read view. The gap additionally
//!   clears the applied-identity window: everything at or before the
//!   replacement snapshot is superseded.
//! * `turn_started` under a turn id the transport already knows is a recovery
//!   re-drive: the abandoned drive's provisional copy is superseded in place.
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
use lash::runtime::{
    RuntimeEffectCommand, RuntimeEffectController, RuntimeEffectControllerError,
    RuntimeEffectEnvelope, RuntimeEffectLocalExecutor, RuntimeEffectOutcome,
};
use lash_remote_protocol::{RemoteSessionObservationEventPayload, RemoteTurnEvent};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};

/// Mirrors `MAX_APPLIED_EVENT_IDS` in `lash::recoverable_chat`: the dedupe
/// window is bounded because an honest client only needs to absorb redelivery
/// inside a replay suffix, not dedupe history forever.
const MAX_APPLIED_EVENT_IDS: usize = 4096;

/// The wire identity of one delivered observation event — the remote encoding
/// of `lash::recoverable_chat::RecoverableChatEventId`. The incarnation makes
/// the identity safe across replay-store restarts: a rebuilt store may reuse a
/// cursor but cannot reproduce the old identity.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct DeliveredEventId {
    session_id: String,
    replay_incarnation_id: String,
    cursor: String,
}

impl DeliveredEventId {
    fn of(event: &RemoteSessionObservationEvent) -> Self {
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

/// One turn's single output row: the slot `workbench-assistant:{turn_id}`
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
    terminal_value: Option<serde_json::Value>,
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

    /// What a client renders: provisional text while a (re-)drive is live —
    /// it is newer than the last settled read — else the settled text, else
    /// the provisional terminal value.
    pub(crate) fn rendered(&self) -> Option<String> {
        let provisional = self.provisional_text();
        if !provisional.is_empty() {
            return Some(provisional);
        }
        self.settled_text
            .clone()
            .or_else(|| self.terminal_value.as_ref().map(ToString::to_string))
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
    /// re-driven turn that committed twice collapses to the newest copy.
    pub(crate) fn replace_from_settled(&mut self, snapshot: &StateReadSnapshot) {
        self.resume_cursor = Some(snapshot.observation.cursor.clone());
        for message in &snapshot.state.messages {
            if message.role != "assistant" {
                continue;
            }
            let Some(turn_id) = message
                .provenance
                .as_ref()
                .map(|ChatMessageProvenance::TurnOutput { turn_id }| turn_id.clone())
                .or_else(|| {
                    workbench_turn_id_from_assistant_message_id(&message.id).map(TurnId::from)
                })
            else {
                continue;
            };
            let row = self.outputs.entry(turn_id).or_default();
            row.settled_text = Some(message.text.clone());
            row.provisional_prose.clear();
            row.terminal_value = None;
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

    /// The rendered row identities — always `workbench-assistant:{turn_id}`,
    /// one per turn no matter how many delivery legs produced the row.
    pub(crate) fn output_keys(&self) -> Vec<String> {
        self.outputs
            .keys()
            .map(workbench_turn_assistant_message_id)
            .collect()
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

    fn fold_event(&mut self, event: &RemoteSessionObservationEvent) {
        let Some(turn_id) = event.turn_id.clone() else {
            return;
        };
        let RemoteSessionObservationEventPayload::TurnActivity { activity } = &event.event else {
            // No other turn-scoped payload carries output text, but the turn's
            // row exists as soon as any of its events do.
            self.outputs.entry(turn_id).or_default();
            return;
        };
        let row = self.outputs.entry(turn_id).or_default();
        match &activity.event {
            RemoteTurnEvent::TurnStarted { .. } => {
                // A fresh drive under an existing turn id is a re-drive: its
                // provisional copy supersedes whatever the abandoned drive
                // left behind. The settled text stays — it is still the last
                // canonical word until the next refetch.
                row.provisional_prose.clear();
                row.terminal_value = None;
            }
            RemoteTurnEvent::AssistantProseDelta { text, .. } => {
                row.provisional_prose
                    .entry(activity.correlation_id.clone())
                    .or_default()
                    .push_str(text);
            }
            RemoteTurnEvent::ModelAttemptReset {
                assistant_prose_correlation_ids,
                ..
            } => {
                for correlation_id in assistant_prose_correlation_ids {
                    row.provisional_prose.remove(correlation_id);
                }
            }
            RemoteTurnEvent::FinalValue { value } => {
                row.terminal_value = Some(value.clone());
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
    let response = session_observations(
        State(state.clone()),
        Query(EventsQuery {
            cursor: cursor.map(str::to_string),
            session_id: Some(session_id.clone()),
        }),
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

/// Drive one turn through the same code path the workbench's own routes use:
/// `stream_to` for activity, `record_turn_output` for the durable and product
/// records, `settle_workbench_turn` for the terminal bookkeeping.
async fn drive_reference_turn(
    state: &AppState,
    session: &lash::LashSession,
    turn_id: &TurnId,
    prompt: &str,
) {
    let turn_state = Arc::new(Mutex::new(TurnStreamState::default()));
    let output = session
        .turn(lash::TurnInput::text(prompt))
        .turn_id(turn_id.clone())
        .stream_to(&ChannelTurnEvents {
            turn_state: Arc::clone(&turn_state),
        })
        .await
        .expect("drive turn");
    crate::restate::record_turn_output(
        state,
        session,
        turn_id,
        output,
        turn_state,
        "test.reference_transport.turn",
    )
    .await
    .expect("record turn output");
    crate::restate::settle_workbench_turn(state, &session.session_id(), turn_id)
        .await
        .expect("settle turn");
}

/// The recoverable-chat test state with its runtime facets overridable: an
/// explicit live-replay store shrinks the replay window so a trimmed-gap
/// recovery is deterministic, and an optional effect layer over the
/// backend's journaling host lets a test crash and re-drive turn effects the
/// way a durable workflow engine does.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn recoverable_chat_test_state_with_replay_store(
    data_dir: &std::path::Path,
    channel_capacity: usize,
    provider: ProviderHandle,
    trigger_store: Arc<dyn lash::triggers::TriggerStore>,
    store_factory: Arc<dyn lash::persistence::SessionStoreFactory>,
    queued_work_driver: Option<Arc<dyn lash::runtime::SessionWorkEngine>>,
    context_window_tokens: usize,
    live_replay_store: Option<Arc<dyn lash::observe::LiveReplayStore>>,
    effect_layer: Option<Arc<dyn lash::testing::EffectLayer>>,
) -> AppState {
    let sqlite = test_file_backend(data_dir);
    let mut decorated = DecoratedBackend::over(sqlite.into())
        .with_catalog(Arc::clone(&store_factory))
        .with_trigger_store(Arc::clone(&trigger_store));
    if let Some(layer) = effect_layer {
        decorated = decorated.with_effect_layer(layer);
    }
    if let Some(driver) = queued_work_driver {
        decorated = decorated.with_queued_work(driver);
    }
    let backend: lash::Backend = decorated.into();
    let model = with_workbench_model_capability(
        lash::ModelSpec::builder("test-model")
            .context_window_tokens(context_window_tokens)
            .build()
            .expect("model spec"),
    );
    let mut core_builder = explicit_durable_test_facets_on(backend)
        .provider(provider)
        .model(model);
    if let Some(live_replay_store) = live_replay_store {
        core_builder = core_builder.live_replay_store(live_replay_store);
    }
    let core = core_builder
        .build(crate::test_core_owner())
        .expect("build test core");
    let process_observer = core
        .processes()
        .observer()
        .expect("process observer configured");
    AppState {
        core,
        attachment_store: test_attachment_store(),
        session_store_factory: Arc::clone(&store_factory),
        trigger_store,
        process_observer,
        // Process work is resolved through the core.
        sessions: WorkbenchSessions::fresh(),
        messages: Arc::new(Mutex::new(Vec::new())),
        selected_model: Arc::new(Mutex::new(ModelSelection {
            model: "test-model".to_string(),
            model_variant: Default::default(),
        })),
        trace_sink: None,
        lashlang_execution: Arc::new(TraceLashlangGraphStore::default()),
        event_tx: SessionEventRegistry::new(channel_capacity),
        queued_work_driver: inert_queued_work(),
        restate_ingress_url: "http://127.0.0.1:8080".to_string(),
        restate_admin_url: "http://127.0.0.1:9070".to_string(),
        restate_http: reqwest::Client::new(),
        restate_cron_job_keys: Arc::new(Mutex::new(BTreeMap::new())),
        mail_world: mail::MailWorld::new(),
        active_turns: ActiveTurns::default(),
        unknown_turn_terminals: UnknownTurnTerminals::default(),
        authorization: WorkbenchAuthorization::allow_all(),
        approvals: approvals::WorkbenchApprovals::in_memory().unwrap(),
    }
}

/// A bare-prose answer needs no `finish` call: the runtime commits the reply
/// itself, so the settled view carries a canonical assistant row for the
/// turn.
fn prose_provider(kind: &'static str, answers: &[&str]) -> ProviderHandle {
    scripted_cells_provider(
        kind,
        answers.iter().map(|answer| answer.to_string()).collect(),
    )
}

/// Script `request.stream_events` the way `failure_provider` does: a delta the
/// runtime records as streamed activity for the in-flight attempt.
fn send_delta(request: &LlmRequest, text: &str) {
    if let Some(events) = request.stream_events.as_ref() {
        events.send(LlmStreamEvent::Delta {
            block: StreamBlockIdentity::new("text:0", 0),
            text: text.to_string(),
        });
    }
}

/// The first call dies after partial output on a retryable stream boundary;
/// the runtime re-buys the generation and calls again. Zero delays — the
/// corpus never waits on wall-clock backoff.
fn retried_attempt_provider(superseded: &'static str, answer: &'static str) -> ProviderHandle {
    let calls = Arc::new(AtomicUsize::new(0));
    lash::testing::TestProvider::builder()
        .kind("reference-transport-retry")
        .requires_streaming(true)
        .generation_retry_guarantee(GenerationRetryGuarantee::Idempotent)
        .options(ProviderOptions {
            reliability: ProviderReliability::default()
                .max_attempts(2)
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

/// The provider behind the re-drive leg. Call 0 streams a partial and returns
/// a cell that keeps the turn open — the journaled call the crashed drive
/// bought and the re-drive must replay rather than re-buy. Every later call is
/// a plain answer: the re-drive's own completion, then the queued drain's.
fn redrive_provider(
    partial: &'static str,
    answers: &[&'static str],
) -> (ProviderHandle, Arc<AtomicUsize>) {
    let answers: Vec<String> = answers.iter().map(|answer| answer.to_string()).collect();
    let calls = Arc::new(AtomicUsize::new(0));
    let provider = lash::testing::TestProvider::builder()
        .kind("reference-transport-redrive")
        .requires_streaming(true)
        .options(ProviderOptions {
            reliability: ProviderReliability::disabled(),
            ..ProviderOptions::default()
        })
        .complete({
            let calls = Arc::clone(&calls);
            move |request: LlmRequest| {
                let answers = answers.clone();
                let calls = Arc::clone(&calls);
                async move {
                    match calls.fetch_add(1, Ordering::SeqCst) {
                        0 => {
                            send_delta(&request, partial);
                            Ok(text_response(
                                "<typescript>\nconst marker = 1;\n</typescript>",
                            ))
                        }
                        call => {
                            let answer = answers
                                .get(call - 1)
                                .expect("the re-drive provider ran out of answers");
                            send_delta(&request, answer);
                            Ok(text_response(answer))
                        }
                    }
                }
            }
        })
        .build()
        .into_handle();
    (provider, calls)
}

/// A crash layer over a SQLite effect host: the
/// `fail_on_llm_call`-th LLM effect dies before it reaches the journal, the
/// way a crashed workflow invocation leaves a turn mid-flight with its journal
/// intact. The recovery drive under the same turn id replays each journaled
/// effect instead of re-executing it — the provider is never re-bought and the
/// already-admitted input is never re-applied — then resumes executing where
/// the journal ends; the deployment's journal refuses a replayed envelope that
/// diverges from the recorded one.
struct RedriveCrashLayer {
    llm_effects: AtomicUsize,
    answered_llm_effects: AtomicUsize,
    fail_on_llm_call: usize,
}

impl RedriveCrashLayer {
    fn failing_on_llm_call(ordinal: usize) -> Self {
        Self {
            llm_effects: AtomicUsize::new(0),
            answered_llm_effects: AtomicUsize::new(0),
            fail_on_llm_call: ordinal,
        }
    }

    /// LLM effects the journal answered, live or replayed.
    fn answered_llm_effects(&self) -> usize {
        self.answered_llm_effects.load(Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl lash::testing::EffectLayer for RedriveCrashLayer {
    async fn execute_effect(
        &self,
        inner: &dyn RuntimeEffectController,
        envelope: RuntimeEffectEnvelope,
        local_executor: RuntimeEffectLocalExecutor<'_>,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        if !matches!(&envelope.command, RuntimeEffectCommand::LlmCall { .. }) {
            return inner.execute_effect(envelope, local_executor).await;
        }
        if self.llm_effects.fetch_add(1, Ordering::SeqCst) + 1 == self.fail_on_llm_call {
            return Err(RuntimeEffectControllerError::foreign(
                "reference_transport_drive_crashed",
                // A crash is a live fault: the drive aborts and the redrive
                // replays the journal.
                lash::runtime::TurnFailureCause::LiveFault,
                "injected crash: the drive dies mid-turn with its journal intact",
            ));
        }
        let outcome = inner.execute_effect(envelope, local_executor).await?;
        self.answered_llm_effects.fetch_add(1, Ordering::SeqCst);
        Ok(outcome)
    }
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
#[tokio::test]
async fn one_output_identity_per_turn_across_disconnect_and_redelivery() {
    const FIRST_ANSWER: &str = "first canonical answer";
    const SECOND_ANSWER: &str = "second canonical answer";
    let data_dir = tempfile::tempdir().expect("reference transport tempdir");
    let state = recoverable_chat_test_state_with_provider(
        data_dir.path(),
        16,
        prose_provider(
            "reference-transport-disconnect",
            &[FIRST_ANSWER, SECOND_ANSWER],
        ),
    )
    .await;
    let session_id = state.current_session_id();
    let session = state
        .core
        .session(session_id.clone())
        .open()
        .await
        .expect("open session");

    let mut transport = ReferenceTransport::default();
    let mut client = connect_observations(&state, &session_id, transport.resume_cursor()).await;

    // Turn-one settles; the persisted resume cursor lands on its commit
    // checkpoint — a real replay position, unlike the never-published
    // snapshot cursor a first connect echoes back.
    drive_reference_turn(
        &state,
        &session,
        &TurnId::from("turn-one"),
        "first question",
    )
    .await;
    apply_until_settled(
        &state,
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
    drive_reference_turn(
        &state,
        &session,
        &TurnId::from("turn-two"),
        "second question",
    )
    .await;
    apply_through_turn_activity(&mut client, &mut transport, &TurnId::from("turn-two")).await;
    drop(client);

    // Reconnect from the trailing cursor: replay redelivers the applied
    // turn-two events, dedupe absorbs them, and the same row keeps filling
    // in.
    let mut client = connect_observations(&state, &session_id, transport.resume_cursor()).await;
    apply_until_settled(
        &state,
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
        vec![
            "workbench-assistant:turn-one".to_string(),
            "workbench-assistant:turn-two".to_string()
        ]
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
#[tokio::test]
async fn trimmed_gap_recovery_replaces_the_same_output_identity() {
    const FIRST_ANSWER: &str = "trimmed first answer";
    const SECOND_ANSWER: &str = "trimmed second answer";
    let data_dir = tempfile::tempdir().expect("reference transport tempdir");
    let state = recoverable_chat_test_state_with_replay_store(
        data_dir.path(),
        16,
        prose_provider("reference-transport-trim", &[FIRST_ANSWER, SECOND_ANSWER]),
        detached_trigger_store(),
        Arc::new(lash_sqlite_store::SqliteSessionStoreFactory::new(
            data_dir.path().join("lash-sessions"),
        )),
        None,
        4096,
        Some(Arc::new(lash::observe::InMemoryLiveReplayStore::new(
            lash::observe::InMemoryLiveReplayStoreConfig {
                max_events_per_session: 1,
                ..lash::observe::InMemoryLiveReplayStoreConfig::default()
            },
        ))),
        None,
    )
    .await;
    let session_id = state.current_session_id();
    let session = state
        .core
        .session(session_id.clone())
        .open()
        .await
        .expect("open session");

    let mut transport = ReferenceTransport::default();
    let mut client = connect_observations(&state, &session_id, transport.resume_cursor()).await;

    // The client watches turn-one live — the row exists provisionally — then
    // goes away before its checkpoints complete.
    drive_reference_turn(
        &state,
        &session,
        &TurnId::from("turn-one"),
        "first question",
    )
    .await;
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
    drive_reference_turn(
        &state,
        &session,
        &TurnId::from("turn-two"),
        "second question",
    )
    .await;

    // The persisted cursor is trimmed: the stream answers with a gap whose
    // replacement snapshot the transport installs from the settled view.
    let mut client = connect_observations(&state, &session_id, transport.resume_cursor()).await;
    let mut gap_reason = None;
    for _ in 0..256 {
        let item = client.next_item().await;
        if let ObservationStreamItem::ReplayGap { gap, .. } = &item {
            gap_reason = Some(gap.body.reason);
        }
        if transport.apply(&item) == TransportDirective::RefetchSettled {
            refetch_settled(&state, &session_id, &mut transport).await;
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
        Some(lash_remote_protocol::RemoteLiveReplayGapReason::Trimmed),
        "a trimmed persisted cursor must answer with a trimmed gap"
    );
    assert_eq!(
        transport.output_keys(),
        vec![
            "workbench-assistant:turn-one".to_string(),
            "workbench-assistant:turn-two".to_string()
        ],
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

/// A provider retry inside one drive: the first attempt dies after partial
/// output, the runtime re-buys the generation, and `model_attempt_reset`
/// retracts the superseded copy on the same row — never a second identity,
/// never stale partial text left standing over the canonical answer.
#[tokio::test]
async fn a_retried_attempt_replaces_partial_prose_on_the_same_row() {
    const SUPERSEDED: &str = "superseded partial from the failed attempt";
    const ANSWER: &str = "answer from the retried attempt";
    let data_dir = tempfile::tempdir().expect("reference transport tempdir");
    let state = recoverable_chat_test_state_with_provider(
        data_dir.path(),
        16,
        retried_attempt_provider(SUPERSEDED, ANSWER),
    )
    .await;
    let session_id = state.current_session_id();
    let session = state
        .core
        .session(session_id.clone())
        .open()
        .await
        .expect("open session");

    let mut transport = ReferenceTransport::default();
    let mut client = connect_observations(&state, &session_id, transport.resume_cursor()).await;
    drive_reference_turn(&state, &session, &TurnId::from("turn-one"), "the question").await;

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
        &state,
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
        vec!["workbench-assistant:turn-one".to_string()],
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

/// A recovery re-drive reuses the request's turn identity: the first drive
/// dies mid-turn with its journaled provider call intact — the crashed
/// workflow invocation — and the recovery drive under the same `turn_id`
/// replays the journal and commits once. Fresh delivery identities arrive
/// under the same turn id and the transport keeps one output row whose
/// canonical text the settled refetch writes.
#[tokio::test]
async fn a_redriven_turn_keeps_its_output_identity() {
    const CRASHED_PARTIAL: &str = "partial prose from the crashed drive";
    const REDRIVEN_ANSWER: &str = "answer from the recovery re-drive";
    const DRAINED_ANSWER: &str = "answer from the queued drain";
    let data_dir = tempfile::tempdir().expect("reference transport tempdir");
    // The fixture is a file backend under `data_dir`: the crash layer sits
    // over its journaling host, so the re-drive replays the journal the first
    // drive wrote.
    let layer = Arc::new(RedriveCrashLayer::failing_on_llm_call(2));
    let (provider, provider_calls) =
        redrive_provider(CRASHED_PARTIAL, &[REDRIVEN_ANSWER, DRAINED_ANSWER]);
    let state = recoverable_chat_test_state_with_replay_store(
        data_dir.path(),
        16,
        provider,
        detached_trigger_store(),
        Arc::new(lash_sqlite_store::SqliteSessionStoreFactory::new(
            data_dir.path().join("lash-sessions"),
        )),
        None,
        4096,
        None,
        Some(Arc::clone(&layer) as Arc<dyn lash::testing::EffectLayer>),
    )
    .await;
    let session_id = state.current_session_id();
    let session = state
        .core
        .session(session_id.clone())
        .open()
        .await
        .expect("open session");

    let mut transport = ReferenceTransport::default();
    let mut client = connect_observations(&state, &session_id, transport.resume_cursor()).await;

    // The first drive journals one provider call — its streamed partial is
    // live on the row — then dies on the call the journal does not yet hold,
    // the way a crashed runner leaves a turn mid-flight.
    let crashed = session
        .turn(lash::TurnInput::text("the question"))
        .turn_id(TurnId::from("turn-one"))
        .stream_to(&ChannelTurnEvents {
            turn_state: Arc::new(Mutex::new(TurnStreamState::default())),
        })
        .await;
    assert!(
        crashed.is_err(),
        "the first drive aborts mid-invocation: {crashed:?}"
    );
    for _ in 0..256 {
        let item = client.next_item().await;
        transport.apply(&item);
        if transport
            .outputs()
            .get(&TurnId::from("turn-one"))
            .is_some_and(|row| row.provisional_text() == CRASHED_PARTIAL)
        {
            break;
        }
    }
    let row = transport
        .outputs()
        .get(&TurnId::from("turn-one"))
        .expect("the crashed drive still has one output row");
    assert_eq!(
        row.provisional_text(),
        CRASHED_PARTIAL,
        "the crashed drive's streamed partial is provisional state on the one row"
    );
    assert!(!row.is_settled(), "nothing committed, so nothing settled");
    drop(session);

    // Recovery re-drives the request's own turn id on a reopened session —
    // the journaled call replays instead of re-buying the provider, the call
    // past the journal executes, and the turn commits once.
    let session = state
        .core
        .session(session_id.clone())
        .open()
        .await
        .expect("reopen session for the recovery drive");
    drive_reference_turn(&state, &session, &TurnId::from("turn-one"), "the question").await;
    apply_until_settled(
        &state,
        &session_id,
        &mut client,
        &mut transport,
        &TurnId::from("turn-one"),
    )
    .await;
    assert_eq!(
        provider_calls.load(Ordering::SeqCst),
        2,
        "the re-drive replays the journaled call instead of re-buying it"
    );
    assert_eq!(
        layer.answered_llm_effects() - provider_calls.load(Ordering::SeqCst),
        1,
        "the journaled call must replay on the recovery drive"
    );
    assert_eq!(
        transport.output_keys(),
        vec!["workbench-assistant:turn-one".to_string()],
        "a re-drive must not mint a second output identity"
    );
    let row = transport
        .outputs()
        .get(&TurnId::from("turn-one"))
        .expect("turn-one output row");
    assert_eq!(
        row.settled_text(),
        Some(REDRIVEN_ANSWER),
        "the re-drive's committed output replaces the first copy in place"
    );
    assert_eq!(
        row.provisional_text(),
        "",
        "the re-drive superseded the crashed drive's stale partial"
    );

    // The durable drain a workflow journal re-drives carries its own
    // idempotency key: a repeated drain answers empty instead of running the
    // turn a second time — the durable half of the same one-identity story.
    session
        .durable()
        .enqueue(lash::TurnInput::text("queued question"))
        .id("turn-two-input")
        .send()
        .await
        .expect("enqueue queued input");
    session
        .queued_turn()
        .drain_id("turn-two")
        .run()
        .await
        .expect("drain queued work")
        .expect("the queued input should run");
    apply_until_settled(
        &state,
        &session_id,
        &mut client,
        &mut transport,
        &TurnId::from("turn-two"),
    )
    .await;
    let redriven = session
        .queued_turn()
        .drain_id("turn-two")
        .run()
        .await
        .expect("re-drive the satisfied drain");
    assert!(
        redriven.ran().is_none(),
        "re-driving a satisfied drain id must be a durable no-op"
    );
    assert_eq!(
        transport.output_keys(),
        vec![
            "workbench-assistant:turn-one".to_string(),
            "workbench-assistant:turn-two".to_string()
        ],
        "a satisfied drain's re-drive emits nothing and mints nothing"
    );
    drop(client);
}
