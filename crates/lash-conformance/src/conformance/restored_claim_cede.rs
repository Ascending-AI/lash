//! A redrive cedes rows it restored from its journal once another driver
//! took them (FIG-3552).
//!
//! A turn whose worker dies after a checkpoint claim is journaled, but before
//! the turn commits, leaves that claim under a generation that no longer holds
//! the lane. Another worker may then take the rows. The redrive replays the
//! journaled claim and the journaled model reply to it, so it cannot commit
//! that reply without answering the rows a second time.
//!
//! The laws run the follow-on physical turn of a logical run, which carries no
//! journaled initial drive: its first physical turn already settled that. The
//! claim is the follow-on's checkpoint claim, once of turn input and once of
//! queued work. Two recoveries are covered. When a recovery drain answers the
//! rows and commits, the moved session head already refuses the redrive's
//! follow-on commit. When a peer takes the rows and dies holding them, the
//! head is unmoved and only the settlement stops the redrive: it cedes with
//! `accepted_turn_input_ceded`, and the next drain answers the rows once.
//!
//! A resumed queued run is the complement: it retakes the rows its
//! checkpoints were assigned under its own generation first, so a restored
//! claim it settles under that retaken claim is never a peer's supersession
//! (see `queued_run_resume_retakes_its_open_checkpoint_assignments` and the
//! crash matrix's `peer_reclaim` scenario).

use super::direct_turn_acceptance::{
    JournalLayer, acceptance_runtime_for_session, direct_input, text_response,
};
use crate::admit;
use lash_sansio::SessionId;
use lash_sansio::TurnId;
use pretty_assertions::assert_eq;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

/// The session every store in this suite is exercised under.
pub const RESTORED_CLAIM_CEDE_SESSION_ID: &str = "restored-claim-cede";
const SESSION_ID: &str = RESTORED_CLAIM_CEDE_SESSION_ID;

/// The reply the first execution journals for the follow-on turn.
const FIRST_EXECUTION_REPLY: &str = "the first execution answered the checkpoint rows";
/// The reply the recovery drain commits.
const RECOVERY_REPLY: &str = "the recovery drain answered the rows";

/// Two tools: one whose call switches the agent frame, so the logical run
/// continues in a follow-on physical turn, and one plain tool, so the
/// follow-on reaches an `AfterWork` checkpoint before its second model call.
struct CedeTools;

fn tool(name: &str, description: &str) -> crate::ToolDefinition {
    crate::ToolDefinition::raw(
        format!("tool:{name}"),
        name,
        description,
        crate::ToolDefinition::default_input_schema(),
        serde_json::json!({"type": "object", "additionalProperties": true}),
    )
}

fn switch_frame_tool() -> crate::ToolDefinition {
    tool(
        "switch_frame",
        "Continue the run in a follow-on agent frame.",
    )
}

fn step_tool() -> crate::ToolDefinition {
    tool("step", "One unit of work.")
}

#[async_trait::async_trait]
impl crate::ToolProvider for CedeTools {
    fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
        vec![switch_frame_tool().manifest(), step_tool().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<crate::ToolContract>> {
        match name {
            "switch_frame" => Some(Arc::new(switch_frame_tool().contract())),
            "step" => Some(Arc::new(step_tool().contract())),
            _ => None,
        }
    }

    #[expect(
        clippy::expect_used,
        reason = "conformance-law fixture: non-empty frame material always derives"
    )]
    async fn execute(&self, call: crate::ToolCall<'_>) -> crate::ToolAttemptOutcome {
        let output = crate::ToolCallOutput::success(serde_json::json!({"done": true}));
        let output = if call.name() == "switch_frame" {
            output.with_control(crate::ToolControl::SwitchAgentFrame {
                frame_key: crate::FrameKey::from_caller_material("restored-claim-cede-follow-on")
                    .expect("non-empty frame material derives"),
                initial_nodes: Vec::new(),
                task: Some("continue in the follow-on frame".to_string()),
            })
        } else {
            output
        };
        crate::ToolAttemptOutcome::done_without_intents(crate::ToolOutcomeDone::from_output(output))
    }
}

fn tools_plugin() -> Arc<dyn crate::facade_support::PluginFactory> {
    Arc::new(crate::plugin::StaticPluginFactory::new(
        "conformance-restored-claim-cede-tools",
        crate::facade_support::PluginSpec::new().with_tool_provider(Arc::new(CedeTools)),
    ))
}

fn tool_call(name: &str, call_id: &str) -> crate::LlmResponse {
    crate::LlmResponse {
        parts: vec![crate::LlmOutputPart::ToolCall {
            call_id: call_id.to_string(),
            tool_name: name.to_string(),
            input_json: "{}".to_string(),
            replay: None,
        }],
        response_metadata: Default::default(),
        ..crate::LlmResponse::default()
    }
}

fn request_text(request: &crate::LlmRequest) -> String {
    request
        .messages
        .iter()
        .flat_map(|message| message.blocks.iter())
        .filter_map(|block| match block {
            crate::llm::types::LlmContentBlock::Text { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// What the follow-on turn's checkpoint claims in the first execution.
#[derive(Clone, Copy)]
enum CheckpointRow {
    /// A steering input pinned to the follow-on turn.
    TurnInput,
    /// A ready process wake.
    QueuedWork,
}

/// The row a law's checkpoint claims, recorded when it is admitted.
#[derive(Default)]
struct AdmittedRow {
    id: Mutex<Option<String>>,
}

impl AdmittedRow {
    #[expect(clippy::expect_used, reason = "conformance fixture lock")]
    fn get(&self) -> Option<String> {
        self.id.lock().expect("admitted row lock").clone()
    }

    #[expect(clippy::expect_used, reason = "conformance fixture lock")]
    fn set(&self, id: String) {
        *self.id.lock().expect("admitted row lock") = Some(id);
    }
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each write is established by the setup"
)]
async fn admit_checkpoint_row(
    store: &Arc<dyn crate::RuntimePersistence>,
    row: CheckpointRow,
    follow_on: &TurnId,
    words: &str,
) -> String {
    match row {
        CheckpointRow::TurnInput => store
            .enqueue_pending_turn_input(crate::PendingTurnInputDraft::new(
                SESSION_ID,
                crate::TurnInputIngress::active_turn(
                    follow_on.clone(),
                    crate::TurnInputCheckpointBoundary::AfterWork,
                ),
                crate::TurnInput::text(words),
            ))
            .await
            .expect("admit the steering input pinned to the follow-on turn")
            .input_id
            .to_string(),
        CheckpointRow::QueuedWork => {
            let process_id = format!("{SESSION_ID}-producer");
            let wake_id = format!("wake:{SESSION_ID}:1");
            let wake = crate::ProcessWakeDelivery {
                version: crate::PROCESS_WAKE_DELIVERY_FORMAT_VERSION,
                wake_id: wake_id.clone(),
                target_session_id: SessionId::from(SESSION_ID),
                process_id: crate::ProcessId::from(process_id.clone()),
                process_incarnation: crate::ProcessIncarnation::from_registration_sequence(1),
                sequence: 1,
                event_type: "producer.wake".to_string(),
                event_invocation: crate::RuntimeInvocation::effect(
                    crate::EffectAddress::new(
                        crate::ExecutionScope::process(process_id),
                        wake_id.clone(),
                    )
                    .expect("valid process wake address"),
                    crate::RuntimeAttribution::none(),
                    wake_id,
                ),
                process_caused_by: None,
                authority: crate::QueuedWorkAuthority::default(),
                input: words.to_string(),
                created_at_ms: 1,
            };
            store
                .enqueue_queued_work(crate::process_wake_batch_draft(wake))
                .await
                .expect("enqueue the ready process wake")
                .batch_id
                .to_string()
        }
    }
}

/// The first execution's model: the root turn switches frames, and the
/// follow-on turn admits the law's row, runs one tool step, and answers the
/// row its checkpoint delivered.
fn first_execution_provider(
    store: Arc<dyn crate::RuntimePersistence>,
    row: CheckpointRow,
    follow_on: TurnId,
    words: String,
    admitted: Arc<AdmittedRow>,
    requests: Arc<Mutex<Vec<String>>>,
) -> crate::ProviderHandle {
    let calls = Arc::new(AtomicUsize::new(0));
    crate::testing::TestProvider::builder()
        .kind("stub")
        .complete(move |request| {
            let store = Arc::clone(&store);
            let follow_on = follow_on.clone();
            let words = words.clone();
            let admitted = Arc::clone(&admitted);
            let requests = Arc::clone(&requests);
            let call = calls.fetch_add(1, Ordering::SeqCst);
            async move {
                #[expect(clippy::expect_used, reason = "conformance fixture lock")]
                requests
                    .lock()
                    .expect("request lock")
                    .push(request_text(&request));
                Ok(match call {
                    0 => tool_call("switch_frame", "restored-claim-cede-switch"),
                    1 => {
                        admitted.set(admit_checkpoint_row(&store, row, &follow_on, &words).await);
                        tool_call("step", "restored-claim-cede-step")
                    }
                    _ => text_response(FIRST_EXECUTION_REPLY),
                })
            }
        })
        .build()
        .into_handle()
}

/// The recovery drain's model: one tool step, so a checkpoint follows it,
/// then the answer.
fn recovery_provider(requests: Arc<Mutex<Vec<String>>>) -> crate::ProviderHandle {
    let calls = Arc::new(AtomicUsize::new(0));
    crate::testing::TestProvider::builder()
        .kind("stub")
        .complete(move |request| {
            let requests = Arc::clone(&requests);
            let call = calls.fetch_add(1, Ordering::SeqCst);
            async move {
                #[expect(clippy::expect_used, reason = "conformance fixture lock")]
                requests
                    .lock()
                    .expect("request lock")
                    .push(request_text(&request));
                Ok(match call {
                    0 => tool_call("step", "restored-claim-cede-recovery-step"),
                    _ => text_response(RECOVERY_REPLY),
                })
            }
        })
        .build()
        .into_handle()
}

/// A worker that dies at the commit that would settle the law's row: the
/// commit never reaches the store and the turn never returns.
struct CrashAtSettlingCommit {
    inner: Arc<dyn crate::RuntimePersistence>,
    admitted: Arc<AdmittedRow>,
    died: Arc<tokio::sync::Notify>,
}

#[async_trait::async_trait]
impl crate::store::RuntimePersistenceDecorator for CrashAtSettlingCommit {
    fn inner(&self) -> &(dyn crate::RuntimePersistence + '_) {
        self.inner.as_ref()
    }

    async fn commit_runtime_state(
        &self,
        commit: crate::store::RuntimeCommit,
    ) -> Result<crate::store::RuntimeCommitReceipt, crate::StoreError> {
        if let Some(row) = self.admitted.get() {
            let settles_row = commit
                .completed_turn_input_claims
                .iter()
                .any(|claim| claim.input_ids.iter().any(|id| id.as_str() == row))
                || commit
                    .completed_queue_claims
                    .iter()
                    .any(|claim| claim.batch_ids.iter().any(|id| id.as_str() == row));
            if settles_row {
                self.died.notify_one();
                return std::future::pending().await;
            }
        }
        self.inner.commit_runtime_state(commit).await
    }
}

/// A redriven worker resumes from the invocation's resident state before its
/// first commit, while the store already holds that commit and the recovery
/// drain's, so the redrive's runtime sees no persisted session.
struct PreCommitResidentState {
    inner: Arc<dyn crate::RuntimePersistence>,
}

#[async_trait::async_trait]
impl crate::store::RuntimePersistenceDecorator for PreCommitResidentState {
    fn inner(&self) -> &(dyn crate::RuntimePersistence + '_) {
        self.inner.as_ref()
    }

    async fn load_session(
        &self,
    ) -> Result<Option<crate::store::PersistedSessionRead>, crate::StoreError> {
        Ok(None)
    }
}

/// One journal and effect host for the logical turn `turn_id`, shared by its
/// first execution and its redrive the way a durable engine keeps a handler's
/// journal across worker incarnations.
struct JournaledRun {
    effect_host: Arc<dyn crate::EffectHost>,
    turn_id: TurnId,
}

impl JournaledRun {
    fn new(turn_id: TurnId) -> Self {
        let journal: Arc<dyn crate::testing::EffectLayer> = Arc::new(JournalLayer::default());
        Self {
            effect_host: Arc::new(crate::testing::LayeredEffectHost::new(
                Arc::new(crate::NativeEffectHost::default()),
                journal,
            )),
            turn_id,
        }
    }

    #[expect(
        clippy::expect_used,
        reason = "conformance-law fixture: each result is established by the setup above"
    )]
    async fn run(
        &self,
        store: &Arc<dyn crate::RuntimePersistence>,
        provider: crate::ProviderHandle,
        lease_owner: crate::LeaseOwnerIdentity,
    ) -> Result<crate::AssembledTurn, crate::RuntimeError> {
        let mut runtime = acceptance_runtime_for_session(
            SESSION_ID,
            store,
            &self.effect_host,
            provider,
            vec![tools_plugin()],
            lease_owner,
        )
        .await;
        let scope = self
            .effect_host
            .scoped(admit(crate::ExecutionScope::turn(
                SESSION_ID,
                &self.turn_id,
            )))
            .expect("scope the journaled logical turn");
        runtime
            .stream_turn(
                direct_input(&self.turn_id, "start the logical run"),
                crate::TurnOptions::new(CancellationToken::new(), scope),
            )
            .await
    }
}

/// Wait until the dead worker's dropped lease guard has released the lane.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn until_lane_released(store: &Arc<dyn crate::RuntimePersistence>) {
    let session_id = SessionId::from(SESSION_ID);
    for _ in 0..1_000 {
        let observed = store
            .get_session_execution_lease(&session_id)
            .await
            .expect("observe the dead worker's lease");
        if observed.lease.is_none() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    panic!("the dead worker's lease was never released");
}

/// A fresh worker with no journal, over `store`, answering through
/// [`recovery_provider`].
async fn fresh_worker(
    owner: &str,
    store: &Arc<dyn crate::RuntimePersistence>,
    effect_host: &Arc<dyn crate::EffectHost>,
    requests: Arc<Mutex<Vec<String>>>,
) -> crate::LashRuntime {
    acceptance_runtime_for_session(
        SESSION_ID,
        store,
        effect_host,
        recovery_provider(requests),
        vec![tools_plugin()],
        crate::LeaseOwnerIdentity::opaque(format!("{owner}-owner"), format!("{owner}-incarnation")),
    )
    .await
}

/// Run one queued-work drain `drain_id` on a fresh worker.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn drain(
    drain_id: &str,
    store: &Arc<dyn crate::RuntimePersistence>,
    requests: Arc<Mutex<Vec<String>>>,
) {
    let effect_host: Arc<dyn crate::EffectHost> = Arc::new(crate::NativeEffectHost::default());
    let mut drainer = fresh_worker(drain_id, store, &effect_host, requests).await;
    let scope = effect_host
        .scoped(admit(crate::ExecutionScope::queue_drain(
            SESSION_ID, drain_id,
        )))
        .expect("scope the drain");
    match drainer
        .stream_next_queued_work(crate::TurnOptions::new(CancellationToken::new(), scope))
        .await
        .expect("the drain runs")
    {
        crate::QueuedTurnDrain::Ran(_) => {}
        other => panic!("the drain answers the orphaned rows: {other:?}"),
    }
}

/// A direct turn `turn_id` on a fresh worker takes the row and dies at the
/// commit that would settle it.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn peer_dies_holding(
    turn_id: &TurnId,
    store: &Arc<dyn crate::RuntimePersistence>,
    row_id: String,
    requests: Arc<Mutex<Vec<String>>>,
) {
    let admitted = Arc::new(AdmittedRow::default());
    admitted.set(row_id);
    let died = Arc::new(tokio::sync::Notify::new());
    let dying: Arc<dyn crate::RuntimePersistence> = Arc::new(CrashAtSettlingCommit {
        inner: Arc::clone(store),
        admitted,
        died: Arc::clone(&died),
    });
    let effect_host: Arc<dyn crate::EffectHost> = Arc::new(crate::NativeEffectHost::default());
    let mut peer = fresh_worker(turn_id.as_str(), &dying, &effect_host, requests).await;
    let scope = effect_host
        .scoped(admit(crate::ExecutionScope::turn(SESSION_ID, turn_id)))
        .expect("scope the peer turn");
    let turn = peer.stream_turn(
        direct_input(turn_id, "the peer's own input"),
        crate::TurnOptions::new(CancellationToken::new(), scope),
    );
    tokio::select! {
        result = turn => panic!("the peer must take the row and die before settling it: {result:?}"),
        () = died.notified() => {}
    }
    until_lane_released(store).await;
}

/// How many committed message parts of the session's current frame carry
/// `text`.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn committed_mentions(store: &Arc<dyn crate::RuntimePersistence>, text: &str) -> usize {
    crate::load_persisted_session_state(store.as_ref())
        .await
        .expect("read the committed session")
        .expect("the session has committed turns")
        .session_graph
        .read_model(None)
        .expect("the committed frame resolves")
        .messages
        .iter()
        .flat_map(|message| message.parts.iter())
        .filter(|part| part.content().contains(text))
        .count()
}

/// What happens to the row between the first execution's death and the
/// redrive.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Recovery {
    /// A recovery drain answers it and commits. Its commit moves the session
    /// head, so the redrive must commit nothing for the follow-on turn.
    DrainCommits,
    /// A peer takes it under a newer generation and dies before settling it.
    /// The head is unmoved, so only the settlement can stop the redrive: it
    /// must cede, and a later drain answers the row once.
    PeerDiesHolding,
}

/// The steps of FIG-3552 for the law's `row`: the first execution dies after
/// its follow-on turn's checkpoint claim and model reply are journaled;
/// another worker takes the row as `recovery` says; the logical turn is
/// redriven on the same journal; and, when the row is still open, a final
/// drain answers it. Returns the row id and the redrive's result.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn redrive_after_recovery(
    prefix: &str,
    store: &Arc<dyn crate::RuntimePersistence>,
    row: CheckpointRow,
    words: &str,
    recovery: Recovery,
) -> (String, Result<crate::AssembledTurn, crate::RuntimeError>) {
    let run = JournaledRun::new(TurnId::from(format!("{prefix}-logical-run")));
    let follow_on = crate::store::QueuedRunPosition::derive_turn_id(&run.turn_id, 1);
    let admitted = Arc::new(AdmittedRow::default());
    let died = Arc::new(tokio::sync::Notify::new());
    let first_requests = Arc::new(Mutex::new(Vec::new()));
    let dying: Arc<dyn crate::RuntimePersistence> = Arc::new(CrashAtSettlingCommit {
        inner: Arc::clone(store),
        admitted: Arc::clone(&admitted),
        died: Arc::clone(&died),
    });

    // 1. The first execution dies before the follow-on turn commits.
    let first = run.run(
        &dying,
        first_execution_provider(
            Arc::clone(store),
            row,
            follow_on.clone(),
            words.to_string(),
            Arc::clone(&admitted),
            Arc::clone(&first_requests),
        ),
        crate::testing::runtime_lease_owner(),
    );
    tokio::select! {
        result = first => panic!("a dead worker's turn never returns: {result:?}"),
        () = died.notified() => {}
    }
    until_lane_released(store).await;
    let row_id = admitted.get().expect("the follow-on admitted its row");
    #[expect(clippy::expect_used, reason = "conformance fixture lock")]
    let first_requests = first_requests.lock().expect("request lock").clone();
    assert!(
        first_requests
            .last()
            .is_some_and(|request| request.contains(words)),
        "the follow-on's checkpoint delivered the row into its journaled model call: {first_requests:?}"
    );

    // 2. Another worker takes the row under a newer generation.
    let recovery_requests = Arc::new(Mutex::new(Vec::new()));
    match recovery {
        Recovery::DrainCommits => {
            if matches!(row, CheckpointRow::QueuedWork) {
                // The drain's own input, so its checkpoint claims the wake.
                store
                    .enqueue_pending_turn_input(crate::PendingTurnInputDraft::new(
                        SESSION_ID,
                        crate::TurnInputIngress::next_turn(),
                        crate::TurnInput::text("the recovery drain's own input"),
                    ))
                    .await
                    .expect("admit the recovery drain's input");
            }
            drain(
                &format!("{prefix}-recovery-drain"),
                store,
                Arc::clone(&recovery_requests),
            )
            .await;
            assert_eq!(
                committed_mentions(store, RECOVERY_REPLY).await,
                1,
                "the recovery drain committed its answer"
            );
        }
        Recovery::PeerDiesHolding => {
            peer_dies_holding(
                &TurnId::from(format!("{prefix}-peer")),
                store,
                row_id.clone(),
                Arc::clone(&recovery_requests),
            )
            .await;
        }
    }
    #[expect(clippy::expect_used, reason = "conformance fixture lock")]
    let recovery_requests = recovery_requests.lock().expect("request lock").clone();
    assert!(
        recovery_requests
            .iter()
            .any(|request| request.contains(words)),
        "the other worker took the row: {recovery_requests:?}"
    );

    // 3. The logical turn is redriven on its journal.
    let redrive_store: Arc<dyn crate::RuntimePersistence> = Arc::new(PreCommitResidentState {
        inner: Arc::clone(store),
    });
    let redriven = run
        .run(
            &redrive_store,
            recovery_provider(Arc::new(Mutex::new(Vec::new()))),
            crate::LeaseOwnerIdentity::opaque(
                format!("{prefix}-redrive-owner"),
                format!("{prefix}-redrive-incarnation"),
            ),
        )
        .await;

    // 4. A row the peer left open is answered by the next drain.
    if recovery == Recovery::PeerDiesHolding {
        until_lane_released(store).await;
        drain(
            &format!("{prefix}-final-drain"),
            store,
            Arc::new(Mutex::new(Vec::new())),
        )
        .await;
    }
    (row_id, redriven)
}

/// The redrive ceded: it returned `accepted_turn_input_ceded`, or its
/// logical run reports the follow-on turn that ceded and committed nothing.
fn assert_ceded(redriven: &Result<crate::AssembledTurn, crate::RuntimeError>) {
    let ceded = crate::FailureCode::from(&crate::RuntimeErrorCode::AcceptedTurnInputCeded);
    match redriven {
        Err(error) => assert_eq!(
            error.code,
            crate::RuntimeErrorCode::AcceptedTurnInputCeded,
            "{error:?}"
        ),
        Ok(turn) => assert!(
            turn.errors
                .iter()
                .any(|issue| issue.code.as_ref() == Some(&ceded)),
            "the redriven follow-on turn must cede, not commit: {:?} {:?}",
            turn.outcome,
            turn.errors
        ),
    }
}

/// The row was answered exactly once, by `answered_by`, and the first
/// execution's journaled reply to it was never committed.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn assert_answered_once(
    store: &Arc<dyn crate::RuntimePersistence>,
    row: CheckpointRow,
    row_id: &str,
    words: &str,
    answered_by: &str,
) {
    match row {
        CheckpointRow::TurnInput => {
            let applications = store
                .list_turn_input_applications(&SessionId::from(SESSION_ID))
                .await
                .expect("read settled applications")
                .into_iter()
                .filter(|application| application.input_id.as_str() == row_id)
                .collect::<Vec<_>>();
            assert_eq!(
                applications.len(),
                1,
                "the input has exactly one application: {applications:?}"
            );
            assert_eq!(
                applications[0].turn_id.as_str(),
                answered_by,
                "the recovering turn is the one that answered it"
            );
        }
        CheckpointRow::QueuedWork => assert!(
            store
                .list_queued_work(&SessionId::from(SESSION_ID))
                .await
                .expect("list queued work")
                .iter()
                .all(|batch| batch.batch_id.as_str() != row_id),
            "the recovering turn settled the wake"
        ),
    }
    assert_eq!(
        committed_mentions(store, words).await,
        1,
        "the row is committed once, by the recovering turn"
    );
    assert_eq!(
        committed_mentions(store, FIRST_EXECUTION_REPLY).await,
        0,
        "the redrive commits no second answer to the row"
    );
}

/// The ticket's interleaving: a follow-on turn's checkpoint claims a steering
/// input and journals the model's reply to it; the worker dies before the
/// commit. A recovery drain re-defers the input, answers it, and commits. The
/// redrive of the logical run on the same journal commits nothing for the
/// follow-on turn: the input has one application and one answer.
pub async fn a_redrive_commits_nothing_for_input_a_recovery_drain_answered(
    prefix: &str,
    store: Arc<dyn crate::RuntimePersistence>,
) {
    let words = "steer the follow-on turn once";
    let (row_id, _) = Box::pin(redrive_after_recovery(
        prefix,
        &store,
        CheckpointRow::TurnInput,
        words,
        Recovery::DrainCommits,
    ))
    .await;
    assert_answered_once(
        &store,
        CheckpointRow::TurnInput,
        &row_id,
        words,
        &format!("{prefix}-recovery-drain"),
    )
    .await;
}

/// The queued-work form of the ticket's interleaving: the recovery drain's own
/// checkpoint claims the ready wake the dead follow-on turn had claimed,
/// answers it, and commits. The redrive commits nothing for the follow-on.
pub async fn a_redrive_commits_nothing_for_work_a_recovery_checkpoint_answered(
    prefix: &str,
    store: Arc<dyn crate::RuntimePersistence>,
) {
    let words = "the producer woke the session once";
    let (row_id, _) = Box::pin(redrive_after_recovery(
        prefix,
        &store,
        CheckpointRow::QueuedWork,
        words,
        Recovery::DrainCommits,
    ))
    .await;
    assert_answered_once(&store, CheckpointRow::QueuedWork, &row_id, words, "").await;
}

/// A peer takes the steering input the dead follow-on turn had claimed at its
/// checkpoint and dies holding it. The redrive's settlement is superseded, so
/// it cedes and commits nothing; the next drain answers the input once.
pub async fn a_redrive_cedes_checkpoint_input_a_peer_reclaimed(
    prefix: &str,
    store: Arc<dyn crate::RuntimePersistence>,
) {
    let words = "steer the follow-on turn once";
    let (row_id, redriven) = Box::pin(redrive_after_recovery(
        prefix,
        &store,
        CheckpointRow::TurnInput,
        words,
        Recovery::PeerDiesHolding,
    ))
    .await;
    assert_ceded(&redriven);
    assert_answered_once(
        &store,
        CheckpointRow::TurnInput,
        &row_id,
        words,
        &format!("{prefix}-final-drain"),
    )
    .await;
}

/// The queued-work form: a peer's checkpoint takes the ready wake the dead
/// follow-on turn had claimed and dies holding it. The redrive cedes; the next
/// drain answers the wake once.
pub async fn a_redrive_cedes_checkpoint_work_a_peer_reclaimed(
    prefix: &str,
    store: Arc<dyn crate::RuntimePersistence>,
) {
    let words = "the producer woke the session once";
    let (row_id, redriven) = Box::pin(redrive_after_recovery(
        prefix,
        &store,
        CheckpointRow::QueuedWork,
        words,
        Recovery::PeerDiesHolding,
    ))
    .await;
    assert_ceded(&redriven);
    assert_answered_once(&store, CheckpointRow::QueuedWork, &row_id, words, "").await;
}
