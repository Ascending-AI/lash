//! FIG-3619: a session-scope effect-group child checks its owning session's
//! state generation at invocation entry.
//!
//! A child is its own Restate invocation, and ADR 0043 routes cross-invocation
//! work to the latest deployment. A turn the pre-cutover build started, still
//! pinned to the old deployment, can open a tool batch after the new
//! deployment registers; its children then run on the new build. These tests
//! drive the deployed `EffectGroupDispatch/child` handler through the endpoint
//! protocol against a session store whose marker names the previous
//! generation, and pin that the child settles with the typed refusal before it
//! admits, records membership, runs anything, or dispatches its tool.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use lash_core::store::StoreTestSupport as _;
use lash_core::store::{SessionCommitStore as _, TurnInputStore as _};
use lash_core::{
    EffectAddress, ExecutionScope, GroupExecutors, GroupWakePolicy, LoserPolicy,
    RuntimeAttribution, RuntimeEffectCommand, RuntimeEffectEnvelope, RuntimeEffectInvocation,
    RuntimeEffectLocalExecutor, RuntimeErrorCode, RuntimePersistence, SessionStoreFactory,
    StoreError,
};
use lash_sansio::SessionId;
use restate_sdk::prelude::Endpoint;

use super::conformance_and_poison::prepared_tool_call;
use super::endpoint_protocol::{
    invoke_endpoint_with_named_call_responses, restate_call_parameters, restate_command_frame_types,
};
use crate::effect_group::{
    EffectGroupChildRequest, EffectGroupRecordSettlementRequest, EffectGroupSettlementTerminal,
    EffectGroupShape,
};

const SESSION: &str = "pre-cutover-session";
const GROUP: &str = "pre-cutover-turn:tool-batch";
/// `RunCommandMessage`: the journal entry of an atomic `ctx.run` body.
const RESTATE_RUN_COMMAND_MESSAGE_TYPE: u16 = 0x0411;

/// The catalog a deployment hands its effect-group services: the one session
/// the child names.
struct OneSessionCatalog {
    session_id: SessionId,
    store: Arc<dyn RuntimePersistence>,
}

#[async_trait::async_trait]
impl lash_core::AttachmentRootSet for OneSessionCatalog {
    async fn live_attachment_refs(
        &self,
        _intent_grace_cutoff_epoch_ms: u64,
    ) -> Result<BTreeSet<lash_core::AttachmentId>, StoreError> {
        Ok(BTreeSet::new())
    }

    async fn has_live_attachment_ref(
        &self,
        _id: &lash_core::AttachmentId,
        _intent_grace_cutoff_epoch_ms: u64,
    ) -> Result<bool, StoreError> {
        Ok(false)
    }
}

#[async_trait::async_trait]
impl SessionStoreFactory for OneSessionCatalog {
    async fn create_store(
        &self,
        _request: &lash_core::SessionStoreCreateRequest,
    ) -> Result<Arc<dyn RuntimePersistence>, StoreError> {
        Err(StoreError::UnsupportedStoreOperation {
            operation: "create_store",
        })
    }

    async fn open_existing_store_by_id(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<Arc<dyn RuntimePersistence>>, StoreError> {
        Ok((*session_id == self.session_id).then(|| Arc::clone(&self.store)))
    }

    async fn session_was_deleted(&self, _session_id: &SessionId) -> Result<bool, String> {
        Ok(false)
    }

    async fn delete_session(
        &self,
        _session_id: &SessionId,
    ) -> lash_core::store::MaintenanceResult<lash_core::SessionBlobReclaimReport> {
        Ok(lash_core::SessionBlobReclaimReport::default())
    }

    // This fixture keeps no countable catalog, so it refuses rather than report zero turns.
    async fn count_unsettled_turns(
        &self,
    ) -> Result<lash_core::store::UnsettledTurnCounts, lash_core::StoreError> {
        Err(lash_core::StoreError::UnsupportedStoreOperation {
            operation: "SessionStoreFactory::count_unsettled_turns",
        })
    }

    async fn list_turn_parks(
        &self,
        _query: &lash_core::store::TurnParkQuery,
    ) -> Result<Vec<lash_core::store::TurnPark>, lash_core::StoreError> {
        Err(lash_core::StoreError::UnsupportedStoreOperation {
            operation: "SessionStoreFactory::list_turn_parks",
        })
    }

    async fn turn_park_feed(
        &self,
        _after: lash_core::store::ParkFeedCursor,
        _limit: std::num::NonZeroUsize,
    ) -> Result<
        lash_core::store::ParkFeedPage<lash_core::store::TurnParkTarget>,
        lash_core::StoreError,
    > {
        Err(lash_core::StoreError::UnsupportedStoreOperation {
            operation: "SessionStoreFactory::turn_park_feed",
        })
    }

    async fn root_terminal(
        &self,
        _session_id: &lash_core::SessionId,
        _root: &lash_core::TurnId,
    ) -> std::result::Result<Option<lash_core::store::RootTerminal>, lash_core::StoreError> {
        Err(lash_core::StoreError::UnsupportedStoreOperation {
            operation: "SessionStoreFactory::root_terminal",
        })
    }

    async fn list_open_control_intents(
        &self,
        _after: Option<lash_core::store::ControlIntentId>,
        _limit: std::num::NonZeroUsize,
    ) -> std::result::Result<Vec<lash_core::store::ControlIntent>, lash_core::StoreError> {
        Err(lash_core::StoreError::UnsupportedStoreOperation {
            operation: "SessionStoreFactory::list_open_control_intents",
        })
    }

    async fn compact_turn_park_feed(
        &self,
        _through: lash_core::store::ParkFeedCursor,
    ) -> Result<(), lash_core::StoreError> {
        Err(lash_core::StoreError::UnsupportedStoreOperation {
            operation: "SessionStoreFactory::compact_turn_park_feed",
        })
    }
}

/// Counts every time the deployment asks how to run a child — the step before
/// the child's tool could be dispatched — and answers that it cannot.
#[derive(Default)]
struct CountingExecutors {
    consulted: AtomicUsize,
}

impl GroupExecutors for CountingExecutors {
    fn executor_for(
        &self,
        _envelope: &RuntimeEffectEnvelope,
    ) -> Option<RuntimeEffectLocalExecutor<'static>> {
        self.consulted.fetch_add(1, Ordering::SeqCst);
        None
    }
}

/// A session store on the current generation, or stamped to `generation`.
async fn session_catalog(
    dir: &std::path::Path,
    generation: Option<u32>,
) -> Arc<dyn SessionStoreFactory> {
    let store = Arc::new(
        lash_sqlite_store::Store::open(&dir.join("session.db"))
            .await
            .expect("open session store"),
    );
    let session_id = SessionId::from(SESSION);
    lash_core::testing::store_fixtures::bind_conformance_session(
        &(Arc::clone(&store) as Arc<dyn RuntimePersistence>),
        &session_id,
    )
    .await;
    if let Some(generation) = generation {
        store
            .stamp_session_state_version_for_testing(generation)
            .await
            .expect("stamp the session's generation marker");
    }
    Arc::new(OneSessionCatalog {
        session_id,
        store: store as Arc<dyn RuntimePersistence>,
    })
}

/// The admitted request of one tool in the batch.
fn tool_request(scope: &ExecutionScope) -> lash_core::runtime::effect::ToolChildRequest {
    let admitted = lash_core::AdmittedScope::new(scope.clone());
    let definition = lash_core::ToolDefinition::raw(
        "tool:tool",
        "tool",
        "the batch's tool",
        lash_core::ToolDefinition::default_input_schema(),
        serde_json::json!({ "type": "object" }),
    );
    lash_core::runtime::effect::ToolChildRequest::new(
        prepared_tool_call(),
        lash_core::runtime::effect::ToolChildAdmission::Catalog {
            manifest: Box::new(definition.manifest()),
        },
        lash_core::tool_dispatch::ToolAttemptEffectIdentity::Scalar { parent: None },
        lash_core::runtime::effect::ToolChildScope {
            opener: lash_core::EffectOpener::for_scope(&admitted)
                .expect("a turn scope derives an opener"),
            admitted_scope: admitted,
            session_id: SessionId::from(SESSION),
            agent_frame_id: lash_core::FrameNodeId::new("frame").expect("a valid frame id"),
        },
        // The child is refused at the session gate, before any authority is
        // consulted.
        lash_core::TurnControlBindingId::new("restate-session-gate-law")
            .expect("a valid binding id"),
        lash_core::ProcessExecutionEnvRef::new("env"),
        lash_core::runtime::effect::ToolChildCompletionRouting::Durable,
        lash_core::runtime::effect::ToolChildSessionFacts::default(),
    )
}

/// The tool child of a batch a turn of `SESSION` opened.
fn tool_child() -> EffectGroupChildRequest {
    let scope = ExecutionScope::turn(SessionId::from(SESSION), "pre-cutover-turn");
    let envelope = RuntimeEffectEnvelope::new(
        RuntimeEffectInvocation::new(
            EffectAddress::new(scope.clone(), format!("{GROUP}:child:0"))
                .expect("valid child address"),
            RuntimeAttribution::for_session(SESSION),
            "tool",
        ),
        RuntimeEffectCommand::ToolInvocation {
            request: Box::new(tool_request(&scope)),
        },
    );
    EffectGroupChildRequest {
        group_key: GROUP.to_owned(),
        shape: EffectGroupShape {
            wake: GroupWakePolicy::All,
            loser_disposition: LoserPolicy::RunToCompletion,
            replay_keys: vec![envelope.invocation.replay_key().to_string()],
            wait_scope: scope,
            membership: vec![serde_json::to_string(&envelope).expect("encode the member")],
            opener: lash_core::AdmittedScope::turn("session", "turn"),
        },
        position: 0,
        envelope,
    }
}

fn endpoint(sessions: Arc<dyn SessionStoreFactory>, executors: Arc<CountingExecutors>) -> Endpoint {
    let host = crate::RestateEffectHost::new_for_test("http://127.0.0.1:9");
    host.register_group_executors(executors as Arc<dyn GroupExecutors>)
        .expect("register the counting resolver");
    Endpoint::builder()
        .bind(crate::EffectGroupDispatch::new(
            &host,
            crate::RestateIngressClient::new("http://127.0.0.1:9".to_string()),
            restate_sdk::context::RunRetryPolicy::new(),
            sessions,
        ))
        .build()
}

/// The scenario from FIG-3619: the pre-cutover turn's tool child lands on
/// this build and is refused, typed, before anything of it runs.
#[tokio::test]
async fn a_pre_cutover_sessions_group_child_is_refused_before_its_tool_is_dispatched() {
    let dir = tempfile::tempdir().expect("tempdir");
    let previous = lash_core::store::CURRENT_SESSION_STATE_VERSION - 1;
    let sessions = session_catalog(dir.path(), Some(previous)).await;
    // Precondition: the store really holds a generation this build refuses.
    assert!(
        matches!(
            lash_core::admit_session_state_generation(
                sessions.as_ref(),
                &SessionId::from(SESSION)
            )
            .await,
            Err(StoreError::SessionStateVersionUnsupported { found, .. }) if found == previous
        ),
        "the fixture session must be on the pre-cutover generation"
    );
    let executors = Arc::new(CountingExecutors::default());
    let endpoint = endpoint(sessions, Arc::clone(&executors));

    let output = invoke_endpoint_with_named_call_responses(
        &endpoint,
        "EffectGroupDispatch",
        "child",
        GROUP,
        &tool_child(),
        vec![
            (
                "commit_child".to_string(),
                serde_json::json!({
                    "type": "committed",
                    "commit_seq": 0,
                    "blocking_positions": [],
                }),
            ),
            (
                "record_settlement".to_string(),
                serde_json::json!({ "type": "recorded", "rank": 1 }),
            ),
        ],
    )
    .await
    .expect("drive the child invocation");

    let calls = restate_call_parameters(&output).expect("decode the child's calls");
    assert_eq!(
        calls
            .iter()
            .map(|(handler, _)| handler.as_str())
            .collect::<Vec<_>>(),
        vec!["commit_child", "record_settlement"],
        "the child's only calls settle its refusal: no admission, no membership record, \
         no durable wait"
    );
    let settlement: EffectGroupRecordSettlementRequest =
        serde_json::from_value(calls[1].1.clone()).expect("decode the settlement");
    let EffectGroupSettlementTerminal::Failed { error } = settlement.terminal else {
        panic!(
            "the refused child settles failed, got {:?}",
            settlement.terminal
        );
    };
    assert_eq!(error.code, RuntimeErrorCode::SessionStateVersionUnsupported);
    assert!(
        error
            .message
            .contains(&format!("session state version {previous}")),
        "the found generation stays readable on the settlement: {}",
        error.message
    );
    assert!(
        !restate_command_frame_types(&output).contains(&RESTATE_RUN_COMMAND_MESSAGE_TYPE),
        "the refused child journals no run"
    );
    assert_eq!(
        executors.consulted.load(Ordering::SeqCst),
        0,
        "the refused child never resolved its tool driver, so its tool was never dispatched"
    );
}

/// The control: on the current generation the same child passes the gate and
/// its first call is its admission, exactly as before the gate existed.
#[tokio::test]
async fn a_current_sessions_group_child_passes_the_gate_to_admission() {
    let dir = tempfile::tempdir().expect("tempdir");
    let sessions = session_catalog(dir.path(), None).await;
    let executors = Arc::new(CountingExecutors::default());
    let endpoint = endpoint(sessions, executors);

    let output = invoke_endpoint_with_named_call_responses(
        &endpoint,
        "EffectGroupDispatch",
        "child",
        GROUP,
        &tool_child(),
        Vec::new(),
    )
    .await
    .expect("drive the child invocation");

    let calls = restate_call_parameters(&output).expect("decode the child's calls");
    assert_eq!(
        calls.first().map(|(handler, _)| handler.as_str()),
        Some("admit_child"),
        "a current-generation child proceeds to admission"
    );
}

/// FIG-3735: a turn handler whose redrive the generation gate refused parks
/// the turn when one is in flight for its scope — here a direct turn whose
/// journaled acceptance wrote its input row — and answers `None` for a scope
/// with nothing in flight, whose refusal stays terminal.
#[tokio::test]
async fn a_refused_redrive_parks_only_a_turn_in_flight() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Arc::new(
        lash_sqlite_store::Store::open(&dir.path().join("session.db"))
            .await
            .expect("open session store"),
    );
    let session_id = SessionId::from(SESSION);
    lash_core::testing::store_fixtures::bind_conformance_session(
        &(Arc::clone(&store) as Arc<dyn RuntimePersistence>),
        &session_id,
    )
    .await;
    // What the crashed execution's journaled acceptance wrote: the turn's
    // input row, under the id its acceptance address provisions.
    let in_flight = ExecutionScope::turn(SESSION, "turn-in-flight");
    let acceptance = lash_core::runtime::causal::turn_acceptance_effect_invocation(
        &in_flight,
        &session_id,
        &lash_core::TurnId::from("turn-in-flight"),
    );
    store
        .enqueue_pending_turn_input(
            lash_core::PendingTurnInputDraft::new(
                session_id.clone(),
                lash_core::TurnInputIngress::next_turn(),
                lash_core::TurnInput::text("the in-flight turn's input"),
            )
            .with_input_id(lash_core::runtime::provisioned_turn_input_id(
                acceptance.address(),
            )),
        )
        .await
        .expect("record the accepted input");
    let previous = lash_core::store::CURRENT_SESSION_STATE_VERSION - 1;
    store
        .stamp_session_state_version_for_testing(previous)
        .await
        .expect("stamp the pre-cutover generation");
    let sessions: Arc<dyn SessionStoreFactory> = Arc::new(OneSessionCatalog {
        session_id: session_id.clone(),
        store: Arc::clone(&store) as Arc<dyn RuntimePersistence>,
    });
    let refusal = lash_core::SessionStateVersionRefusal::of_store_error(
        &lash_core::admit_session_state_generation(sessions.as_ref(), &session_id)
            .await
            .expect_err("the gate refuses the pre-cutover session"),
    )
    .expect("the refusal is the generation gate's");

    let idle = ExecutionScope::turn(SESSION, "turn-never-accepted");
    assert_eq!(
        crate::park_generation_refused_turn(sessions.as_ref(), &idle, refusal, 1_000)
            .await
            .expect("read the idle scope"),
        None,
        "nothing ran for a scope with no accepted input: its refusal stays terminal"
    );
    assert_eq!(
        store
            .load_turn_park(&session_id)
            .await
            .expect("read the park"),
        None
    );

    for (run, at_ms) in [(1_u32, 2_000_u64), (2, 3_000)] {
        let park =
            crate::park_generation_refused_turn(sessions.as_ref(), &in_flight, refusal, at_ms)
                .await
                .expect("record the park")
                .expect("the in-flight turn parks");
        assert_eq!(park.turn_id, lash_core::TurnId::from("turn-in-flight"));
        assert_eq!(park.attempts, run, "each refused run re-parks the turn");
        assert!(
            matches!(
                park.reason,
                lash_core::store::ParkReason::SessionStateGenerationRefused { found, current, .. }
                    if found == previous
                        && current == lash_core::store::CURRENT_SESSION_STATE_VERSION
            ),
            "the park names both generations: {park:?}"
        );
    }
}
