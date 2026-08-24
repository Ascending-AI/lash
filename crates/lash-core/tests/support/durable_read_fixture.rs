use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use lash_core::runtime::{
    QueuedWorkBatchDraft, QueuedWorkClaimBoundary, QueuedWorkPayload, load_process_execution_env,
    persist_process_execution_env, process_wake_batch_draft,
};
use lash_core::{
    AttachmentId, AttachmentIntent, AttachmentManifest, AwaitEventKey, AwaitEventWaitIdentity,
    BoundaryReason, Clock, DeliveryPolicy, EffectHost, ExecResponse, ExecutionScope, LashSchema,
    LeaseClaimNonce, LeaseOwnerIdentity, MessageOrigin, MessageRole, OperationId, PartKind,
    PendingTurnInputDraft, PersistedSegmentHandover, PluginSessionSnapshot, PluginSnapshotArtifact,
    PluginSnapshotEntry, PluginSnapshotMeta, ProcessAwaitOutput, ProcessChange,
    ProcessChangeCursor, ProcessCompletionAuthority, ProcessContinuationStore,
    ProcessEventAppendRequest, ProcessEventSemanticsSpec, ProcessEventType, ProcessExecutionEnvRef,
    ProcessExecutionEnvSpec, ProcessExecutionEnvStore, ProcessExecutionWriteAuthority,
    ProcessIdentity, ProcessInput, ProcessOriginator, ProcessProvenance, ProcessRegistration,
    ProcessRegistry, ProcessStatus, ProcessValueSelector, ProcessWakeDelivery, ProcessWakeSpec,
    ProjectionWatermark, ProtocolTurnOptions, RecoveryContract, Resolution, ResolveOutcome,
    RuntimeCommit, RuntimeEffectCommand, RuntimeEffectEnvelope, RuntimeEffectKind,
    RuntimeEffectLocalExecutor, RuntimeEffectOutcome, RuntimeInvocation, RuntimePersistence,
    RuntimeScope, RuntimeSessionState, SegmentHandover, SessionAppendNode, SessionNodePayload,
    SessionPolicy, SessionRelation, SessionScope, SessionStoreCreateRequest, SessionStoreFactory,
    StoreError, TokenLedgerEntry, TokenUsage, TriggerCommand, TriggerCommandOutcome,
    TriggerDeliveryReservationOutcome, TriggerInputBinding, TriggerMutationOutcome,
    TriggerOccurrenceFilter, TriggerOccurrenceRequest, TriggerOwnerScope, TriggerStore,
    TriggerSubscriptionDraft, TriggerSubscriptionFilter, TurnInput, TurnInputIngress, WaitKind,
    WaitState,
};
use serde::{Deserialize, Serialize};

pub const SESSION_ID: &str = "durable-read-fixture";
pub const DURABLE_READ_FIXTURE_SCHEMA_VERSION: u32 = 27;
pub const FIXTURE_WRITE_MS: u64 = 1_700_000_000_000;
pub const FIXTURE_READ_MS: u64 = FIXTURE_WRITE_MS + 1_000;
const PROCESS_ID: &str = "durable-read-waiting-process";
const WAKE_PROCESS_ID: &str = "durable-read-wake-process";
const TOMBSTONE_PROCESS_ID: &str = "durable-read-retired-process";
const DELETED_SESSION_ID: &str = "durable-read-deleted-session";
const REVOKED_SESSION_ID: &str = "durable-read-revoked-session";
const TRIGGER_KEY: &str = "durable-read-trigger";
const TRIGGER_REGISTER_OPERATION: &str = "durable-read-trigger-register";
const QUEUE_SOURCE_KEY: &str = "durable-read-queue-source";
const INPUT_SOURCE_KEY: &str = "durable-read-input-source";

#[allow(dead_code)]
pub async fn assert_prior_component_encoding_is_refused(store: &dyn RuntimePersistence) {
    let error = store
        .load_session()
        .await
        .expect_err("component encoding version 1 must be refused during hydration");
    assert_eq!(
        error.to_string(),
        "checkpoint component `execution_state` uses encoding version 1, but this build requires version 2; remedy: drain affected sessions and recreate the store with this Lash version"
    );
}

pub struct FixtureHandles {
    pub clock: Arc<dyn Clock>,
    pub runtime: Arc<dyn RuntimePersistence>,
    pub session_factory: Arc<dyn SessionStoreFactory>,
    pub processes: Arc<dyn ProcessRegistry>,
    pub continuations: Arc<dyn ProcessContinuationStore>,
    pub process_envs: Arc<dyn ProcessExecutionEnvStore>,
    pub triggers: Arc<dyn TriggerStore>,
    pub effects: Arc<dyn EffectHost>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ExpectedFixture {
    pub fixture_schema_version: u32,
    pub head_revision: u64,
    pub node_ids_in_read_order: Vec<String>,
    pub current_append_retry: RuntimeCommit,
    pub legacy_commit_retry: RuntimeCommit,
    pub queue_batch_id: String,
    pub pending_input_id: String,
    pub process_env_ref: ProcessExecutionEnvRef,
    pub await_event_key: AwaitEventKey,
    pub revoked_await_event_key: AwaitEventKey,
    pub wake_delivery: ProcessWakeDelivery,
}

pub async fn seed(handles: &FixtureHandles) -> ExpectedFixture {
    let mut state = fixture_state();
    let append_nodes = fixture_append_nodes();
    let current_append_retry = lash_core::store::append_request_commit_with_clock_for_testing(
        &mut state,
        "durable-read-current-append",
        &append_nodes,
        None,
        handles.clock.as_ref(),
    )
    .expect("build identity-bearing fixture append");
    handles
        .runtime
        .commit_runtime_state(current_append_retry.clone())
        .await
        .expect("commit identity-bearing fixture append");

    let attachment_id =
        AttachmentId::parse("durable-read-attachment").expect("valid attachment id");
    handles
        .runtime
        .record_intent(AttachmentIntent {
            attachment_id: attachment_id.clone(),
            session_id: SESSION_ID.to_string(),
            canonical_uri: "session:durable-read-fixture:sha256:durable-read-attachment"
                .to_string(),
            intent_at_epoch_ms: 100,
            owner_kind: None,
            owner_id: None,
        })
        .expect("record fixture attachment intent");

    let mut loaded = lash_core::store::load_persisted_session_state(handles.runtime.as_ref())
        .await
        .expect("load fixture state before legacy commit")
        .expect("fixture session exists before legacy commit");
    loaded.turn_index = 7;
    loaded.token_usage = TokenUsage {
        input_tokens: 13,
        output_tokens: 8,
        cache_read_input_tokens: 5,
        cache_write_input_tokens: 3,
        reasoning_output_tokens: 2,
    };
    loaded.set_tool_state_snapshot(Some(
        serde_json::from_value(serde_json::json!({"generation": 887, "tools": {}}))
            .expect("build distinctive fixture tool state"),
    ));
    loaded.plugin_snapshot_revision = Some(4);
    loaded.set_plugin_snapshot(Some(fixture_plugin_snapshot()));
    loaded.set_execution_state_snapshot(Some(vec![0x46, 0x49, 0x47, 0x38, 0x38, 0x37]));
    let usage = TokenLedgerEntry {
        source: "durable-read-turn".to_string(),
        model: "durable-read-model".to_string(),
        usage: TokenUsage {
            input_tokens: 21,
            output_tokens: 12,
            cache_read_input_tokens: 5,
            cache_write_input_tokens: 3,
            reasoning_output_tokens: 2,
        },
    };
    let legacy_operation = OperationId::new(
        ExecutionScope::runtime_operation("durable-read-legacy-commit"),
        "commit",
    );
    let mut legacy_commit_retry = RuntimeCommit::persisted_state_with_operation_for_testing(
        &loaded,
        &[usage],
        legacy_operation,
    );
    legacy_commit_retry = legacy_commit_retry.with_committed_attachments([attachment_id.clone()]);
    handles
        .runtime
        .commit_runtime_state(legacy_commit_retry.clone())
        .await
        .expect("commit supported NULL-identity legacy-shaped receipt");

    let committed = handles
        .runtime
        .load_session()
        .await
        .expect("load fixture before pin")
        .expect("fixture exists before pin");
    handles
        .session_factory
        .pin(
            committed
                .graph
                .leaf_node_id
                .as_deref()
                .expect("fixture graph has a leaf"),
        )
        .await
        .expect("pin fixture leaf through session factory");

    let deleted_request = fixture_session_request(DELETED_SESSION_ID);
    handles
        .session_factory
        .create_store(&deleted_request)
        .await
        .expect("create fixture session that will be retired");
    handles
        .session_factory
        .delete_session(DELETED_SESSION_ID)
        .await
        .expect("retire fixture session through session factory");

    let queued = handles
        .runtime
        .enqueue_queued_work(
            QueuedWorkBatchDraft::new(
                SESSION_ID,
                DeliveryPolicy::EarliestSafeBoundary,
                vec![QueuedWorkPayload::agent_frame_task(
                    "durable-read-frame",
                    "durable read queued task",
                    None,
                )],
            )
            .with_source_key(QUEUE_SOURCE_KEY),
        )
        .await
        .expect("enqueue fixture queued work");
    let pending = handles
        .runtime
        .enqueue_pending_turn_input(
            PendingTurnInputDraft::new(
                SESSION_ID,
                TurnInputIngress::NextTurn,
                TurnInput::text("durable read pending input"),
            )
            .with_input_id("durable-read-pending-input")
            .with_source_key(INPUT_SOURCE_KEY),
        )
        .await
        .expect("enqueue fixture pending turn input");

    let process_env = fixture_process_env();
    let process_env_ref =
        persist_process_execution_env(handles.process_envs.as_ref(), &process_env)
            .await
            .expect("persist fixture process execution environment");
    let registration = waiting_process_registration(process_env_ref.clone());
    handles
        .processes
        .register_process_with_observers(registration, &[SESSION_ID.to_string()])
        .await
        .expect("register waiting fixture process");
    let lease = handles
        .processes
        .claim_process_lease(
            PROCESS_ID,
            &LeaseOwnerIdentity::opaque("durable-read-owner", "durable-read-incarnation"),
            100,
        )
        .await
        .expect("claim fixture process lease")
        .acquired()
        .expect("fixture process lease acquired");
    handles
        .processes
        .set_process_wait_with_authority(
            PROCESS_ID,
            fixture_wait_state(),
            &ProcessExecutionWriteAuthority::lease(lease),
        )
        .await
        .expect("persist fixture process wait state");
    handles
        .continuations
        .put_segment_handover(PROCESS_ID, fixture_handover())
        .await
        .expect("persist fixture continuation");

    handles
        .processes
        .register_process(
            ProcessRegistration::new(
                WAKE_PROCESS_ID,
                ProcessInput::External {
                    metadata: serde_json::json!({"fixture": "wake"}),
                },
                RecoveryContract::ExternallyOwned,
                ProcessProvenance::host(),
            )
            .with_extra_event_types([ProcessEventType {
                name: "fixture.wake".to_string(),
                payload_schema: LashSchema::any(),
                semantics: ProcessEventSemanticsSpec {
                    wake: Some(ProcessWakeSpec {
                        when: Some(ProcessValueSelector::Present("/wake_input".to_string())),
                        input: ProcessValueSelector::Pointer("/wake_input".to_string()),
                    }),
                    ..ProcessEventSemanticsSpec::default()
                },
            }])
            .with_wake_session_id(Some(SESSION_ID.to_string())),
        )
        .await
        .expect("register fixture wake process");
    let wake_append = handles
        .processes
        .append_event(
            WAKE_PROCESS_ID,
            ProcessEventAppendRequest::new(
                "fixture.wake",
                serde_json::json!({"wake_input": "durable read wake"}),
            ),
        )
        .await
        .expect("append fixture wake event");
    let wake_delivery = wake_append
        .wake_delivery
        .expect("wake-semantic fixture event emits a delivery");

    handles
        .processes
        .register_process(ProcessRegistration::new(
            TOMBSTONE_PROCESS_ID,
            ProcessInput::External {
                metadata: serde_json::json!({"fixture": "tombstone"}),
            },
            RecoveryContract::ExternallyOwned,
            ProcessProvenance::host(),
        ))
        .await
        .expect("register fixture process to prune");
    handles
        .processes
        .complete_process(
            TOMBSTONE_PROCESS_ID,
            ProcessAwaitOutput::Success {
                value: serde_json::json!({"fixture": "retired"}),
                control: None,
            },
            ProcessCompletionAuthority::external_owner(),
        )
        .await
        .expect("complete fixture process to prune");
    let (_, terminal_cursor) = handles
        .processes
        .processes_changed_since(ProcessChangeCursor::initial(), 100)
        .await
        .expect("project fixture terminal process before prune");
    let prune = handles
        .processes
        .prune_terminal_processes(u64::MAX, None, ProjectionWatermark::UpTo(terminal_cursor))
        .await
        .expect("prune fixture terminal process to a tombstone");
    assert_eq!(prune.pruned_processes, 1);

    let register_command = fixture_register_command(process_env_ref.clone());
    let receipt = trigger_receipt(
        handles.triggers.as_ref(),
        TRIGGER_REGISTER_OPERATION,
        register_command,
    )
    .await;
    assert_eq!(receipt.disposition, TriggerMutationOutcome::Created);
    handles
        .triggers
        .ingest_occurrence(TriggerOccurrenceRequest::new(
            "fixture.event",
            "fixture-source",
            serde_json::json!({"value": 42}),
            "durable-read-occurrence",
        ))
        .await
        .expect("ingest fixture occurrence");

    let scope = ExecutionScope::turn(SESSION_ID, "durable-read-turn");
    let await_event_key = handles
        .effects
        .await_event_key(
            &scope,
            AwaitEventWaitIdentity::tool_completion("durable-read-tool-call"),
        )
        .await
        .expect("mint fixture await-event key");
    assert_eq!(
        handles
            .effects
            .resolve_await_event(
                &await_event_key,
                Resolution::Ok(serde_json::json!({"fixture": "resolved"})),
            )
            .await
            .expect("resolve fixture await-event"),
        ResolveOutcome::Accepted
    );

    let revoked_await_event_key = handles
        .effects
        .await_event_key(
            &ExecutionScope::turn(REVOKED_SESSION_ID, "durable-read-revoked-turn"),
            AwaitEventWaitIdentity::tool_completion("durable-read-revoked-tool-call"),
        )
        .await
        .expect("mint fixture await-event key before session revocation");
    handles
        .effects
        .revoke_await_events_for_session(REVOKED_SESSION_ID)
        .await
        .expect("persist fixture await-event session revocation");

    let effect_envelope = fixture_effect_envelope();
    handles
        .effects
        .scoped(ExecutionScope::turn(SESSION_ID, "durable-read-effect-turn"))
        .expect("scope fixture effect journal")
        .controller()
        .execute_effect(
            effect_envelope,
            RuntimeEffectLocalExecutor::testing(|envelope| async move {
                assert!(matches!(
                    envelope.command,
                    RuntimeEffectCommand::ExecCode { ref language, ref code }
                        if language == "fixture" && code == "return 887"
                ));
                Ok(RuntimeEffectOutcome::ExecCode {
                    result: Box::new(Ok(ExecResponse {
                        observations: vec!["durable read effect".to_string()],
                        observation_truncation: Vec::new(),
                        tool_calls: Vec::new(),
                        executed_calls: Vec::new(),
                        printed_images: Vec::new(),
                        error: None,
                        duration_ms: 887,
                        degraded_bindings: Vec::new(),
                        terminal_finish: Some(serde_json::json!({"fixture": 887})),
                    })),
                })
            }),
        )
        .await
        .expect("persist fixture runtime-effect replay row");

    let wake_batch = handles
        .runtime
        .enqueue_queued_work(process_wake_batch_draft(wake_delivery.clone()))
        .await
        .expect("enqueue fixture process wake at receiver");
    let queue_owner = LeaseOwnerIdentity::opaque(
        "durable-read-session-owner",
        "durable-read-session-incarnation",
    );
    let queue_lease = handles
        .runtime
        .try_claim_session_execution_lease_with_token(
            SESSION_ID,
            &queue_owner,
            "durable-read-queue-executor",
            &LeaseClaimNonce::for_testing("durable-read-queue-claim-nonce"),
            100,
        )
        .await
        .expect("claim fixture session lane for wake consumption")
        .acquired()
        .expect("fixture session lane is available");
    let wake_claim = handles
        .runtime
        .claim_ready_queued_work_by_batch_ids(
            SESSION_ID,
            &queue_lease.fence(),
            &queue_owner,
            QueuedWorkClaimBoundary::Idle,
            std::slice::from_ref(&wake_batch.batch_id),
            lash_core::testing::queued_work_claim_policy(1),
        )
        .await
        .expect("claim fixture receiver wake")
        .expect("fixture receiver wake is claimable");
    let wake_state = lash_core::store::load_persisted_session_state(handles.runtime.as_ref())
        .await
        .expect("load fixture state before wake settlement")
        .expect("fixture exists before wake settlement");
    let wake_operation = OperationId::new(
        ExecutionScope::runtime_operation("durable-read-wake-settlement"),
        "commit",
    );
    let wake_commit =
        RuntimeCommit::persisted_state_with_operation_for_testing(&wake_state, &[], wake_operation)
            .completing_queue_claim(wake_claim.completion())
            .releasing_session_execution_lease(queue_lease.completion());
    handles
        .runtime
        .commit_runtime_state(wake_commit)
        .await
        .expect("settle fixture receiver wake and persist redelivery fence");
    handles
        .runtime
        .try_claim_session_execution_lease_with_token(
            SESSION_ID,
            &queue_owner,
            "durable-read-retained-executor",
            &LeaseClaimNonce::for_testing("durable-read-retained-session-lease"),
            100,
        )
        .await
        .expect("persist fixture retained session lease")
        .acquired()
        .expect("fixture retained session lease is available");

    let read = handles
        .runtime
        .load_session()
        .await
        .expect("load seeded fixture session")
        .expect("seeded fixture session exists");
    ExpectedFixture {
        fixture_schema_version: DURABLE_READ_FIXTURE_SCHEMA_VERSION,
        head_revision: read.head_revision,
        node_ids_in_read_order: read
            .graph
            .nodes
            .iter()
            .map(|node| node.node_id.clone())
            .collect(),
        current_append_retry,
        legacy_commit_retry,
        queue_batch_id: queued.batch_id,
        pending_input_id: pending.input_id,
        process_env_ref,
        await_event_key,
        revoked_await_event_key,
        wake_delivery,
    }
}

pub async fn assert_semantics(handles: &FixtureHandles, expected: &ExpectedFixture) {
    assert_eq!(
        expected.fixture_schema_version, DURABLE_READ_FIXTURE_SCHEMA_VERSION,
        "durable fixture schema version changed without regeneration"
    );
    let read = handles
        .runtime
        .load_session()
        .await
        .expect("durable fixture drift: public session read failed")
        .expect("durable fixture drift: session disappeared");
    assert_eq!(
        read.head_revision, expected.head_revision,
        "durable fixture semantic drift: head revision changed"
    );
    let node_ids = read
        .graph
        .nodes
        .iter()
        .map(|node| node.node_id.clone())
        .collect::<Vec<_>>();
    assert_eq!(
        node_ids, expected.node_ids_in_read_order,
        "durable fixture semantic drift: graph node ids or order changed"
    );
    assert_graph_payloads(&read.graph.nodes);
    let checkpoint = read
        .checkpoint
        .expect("durable fixture semantic drift: checkpoint disappeared");
    assert_eq!(
        checkpoint.turn_state.turn_index, 7,
        "durable fixture semantic drift: checkpoint turn_index changed"
    );
    assert_eq!(
        checkpoint.turn_state.token_usage,
        TokenUsage {
            input_tokens: 13,
            output_tokens: 8,
            cache_read_input_tokens: 5,
            cache_write_input_tokens: 3,
            reasoning_output_tokens: 2,
        },
        "durable fixture semantic drift: checkpoint token usage changed"
    );
    assert_eq!(
        serde_json::to_value(
            checkpoint
                .decode_component::<lash_core::ToolState>(
                    lash_core::store::TOOL_STATE_CHECKPOINT_COMPONENT,
                )
                .expect("decode durable fixture tool state")
                .as_ref()
                .expect("durable fixture semantic drift: tool-state component disappeared")
        )
        .expect("encode fixture tool state"),
        serde_json::json!({"generation": 887, "tools": {}}),
        "durable fixture semantic drift: tool-state content changed"
    );
    assert_eq!(
        serde_json::to_value(
            checkpoint
                .decode_component::<PluginSessionSnapshot>(
                    lash_core::store::PLUGIN_SNAPSHOT_CHECKPOINT_COMPONENT,
                )
                .expect("decode durable fixture plugin snapshot")
                .as_ref()
                .expect("durable fixture semantic drift: plugin snapshot disappeared")
        )
        .expect("encode fixture plugin snapshot"),
        serde_json::to_value(fixture_plugin_snapshot()).expect("encode expected plugin snapshot"),
        "durable fixture semantic drift: plugin snapshot content changed"
    );
    assert_eq!(
        checkpoint.plugin_snapshot_revision,
        Some(4),
        "durable fixture semantic drift: plugin snapshot revision changed"
    );
    assert_eq!(
        checkpoint.component_body(lash_core::store::EXECUTION_STATE_CHECKPOINT_COMPONENT),
        Some(&[0x46, 0x49, 0x47, 0x38, 0x38, 0x37][..]),
        "durable fixture semantic drift: execution-state component changed"
    );
    assert_eq!(read.token_ledger.len(), 1);
    assert_eq!(
        read.token_ledger[0].usage,
        TokenUsage {
            input_tokens: 21,
            output_tokens: 12,
            cache_read_input_tokens: 5,
            cache_write_input_tokens: 3,
            reasoning_output_tokens: 2,
        },
        "durable fixture semantic drift: usage ledger totals changed"
    );
    assert!(
        AttachmentManifest::list_all_refs(handles.runtime.as_ref())
            .expect("read fixture attachment manifest")
            .contains(
                &AttachmentId::parse("durable-read-attachment").expect("valid attachment id")
            ),
        "durable fixture semantic drift: committed attachment disappeared"
    );

    let pinned = handles
        .session_factory
        .fork_points()
        .await
        .expect("durable fixture drift: node-anchor read failed");
    assert_eq!(pinned.len(), 1);
    assert_eq!(
        pinned[0].node_id,
        *expected
            .node_ids_in_read_order
            .last()
            .expect("fixture expected graph has a leaf")
    );
    assert_eq!(pinned[0].source_session_id, SESSION_ID);
    assert!(pinned[0].pinned);
    assert!(
        handles
            .session_factory
            .session_was_deleted(DELETED_SESSION_ID)
            .await
            .expect("durable fixture drift: deleted-session probe failed"),
        "durable fixture semantic drift: session tombstone disappeared"
    );
    match handles
        .session_factory
        .create_store(&fixture_session_request(DELETED_SESSION_ID))
        .await
    {
        Err(StoreError::SessionDeleted { session_id }) => {
            assert_eq!(session_id, DELETED_SESSION_ID)
        }
        Ok(_) => panic!("durable fixture drift: retired session id was reopened"),
        Err(error) => panic!(
            "durable fixture drift: retired session open returned wrong typed error: {error}"
        ),
    }

    let session_lease = handles
        .runtime
        .get_session_execution_lease(SESSION_ID)
        .await
        .expect("durable fixture drift: session lease read failed")
        .expect("durable fixture drift: retained session lease disappeared");
    assert_eq!(
        session_lease.owner,
        LeaseOwnerIdentity::opaque(
            "durable-read-session-owner",
            "durable-read-session-incarnation"
        )
    );
    assert_eq!(
        session_lease.lease_token,
        "durable-read-retained-session-lease"
    );
    assert_eq!(session_lease.fencing_token, 2);
    assert_eq!(session_lease.claimed_at_epoch_ms, FIXTURE_WRITE_MS);
    assert_eq!(session_lease.lease_term_ms, 100);
    assert_eq!(session_lease.expires_at_epoch_ms, FIXTURE_WRITE_MS + 100);
    assert!(
        session_lease.expires_at_epoch_ms <= FIXTURE_READ_MS,
        "fixture session lease must deliberately read as an expired raw generation fact"
    );

    let current_replay = handles
        .runtime
        .commit_runtime_state(expected.current_append_retry.clone())
        .await
        .expect("durable fixture identity drift: current append receipt no longer replays");
    assert!(
        current_replay.receipt_replayed,
        "durable fixture identity drift: current append receipt was applied instead of replayed"
    );
    let legacy_replay = handles
        .runtime
        .commit_runtime_state(expected.legacy_commit_retry.clone())
        .await
        .expect("durable fixture identity drift: NULL-identity legacy receipt no longer replays");
    assert!(
        legacy_replay.receipt_replayed,
        "durable fixture identity drift: legacy receipt was applied instead of replayed"
    );
    assert_eq!(
        legacy_replay.committed_usage_delta_identities,
        vec![
            expected.legacy_commit_retry.usage_deltas[0]
                .identity
                .clone()
        ],
        "durable fixture identity drift: usage receipt identity changed"
    );

    let queued = handles
        .runtime
        .list_queued_work(SESSION_ID)
        .await
        .expect("durable fixture drift: queued-work read failed");
    assert_eq!(queued.len(), 1);
    assert_eq!(queued[0].batch_id, expected.queue_batch_id);
    assert_eq!(queued[0].source_key.as_deref(), Some(QUEUE_SOURCE_KEY));
    assert_eq!(queued[0].items.len(), 1);
    assert!(
        matches!(
            &queued[0].items[0].payload,
            QueuedWorkPayload::AgentFrameTask { frame_id, task, .. }
                if frame_id == "durable-read-frame" && task == "durable read queued task"
        ),
        "durable fixture semantic drift: queued-work payload changed"
    );
    let pending = handles
        .runtime
        .list_pending_turn_inputs(SESSION_ID)
        .await
        .expect("durable fixture drift: pending-input read failed");
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].input_id, expected.pending_input_id);
    assert_eq!(pending[0].source_key.as_deref(), Some(INPUT_SOURCE_KEY));
    assert!(pending[0].state.is_next_turn_pending());
    assert_eq!(
        serde_json::to_value(&pending[0].input).expect("encode fixture pending input"),
        serde_json::to_value(TurnInput::text("durable read pending input"))
            .expect("encode expected pending input"),
        "durable fixture semantic drift: pending-input payload changed"
    );

    let process = handles
        .processes
        .get_process(PROCESS_ID)
        .await
        .expect("durable fixture drift: process read failed")
        .expect("durable fixture drift: process disappeared");
    assert_eq!(process.status, ProcessStatus::Waiting);
    assert_eq!(process.wait.as_ref(), Some(&fixture_wait_state()));
    assert_eq!(process.env_ref.as_ref(), Some(&expected.process_env_ref));
    let process_events = handles
        .processes
        .events_after(PROCESS_ID, 0)
        .await
        .expect("durable fixture drift: waiting-process event read failed");
    assert_eq!(process_events.len(), 2);
    assert_eq!(process_events[0].sequence, 1);
    assert_eq!(process_events[0].event_type, "process.observer_added");
    assert_eq!(
        process_events[0].payload,
        serde_json::json!({
            "by": {"kind": "host", "operation_id": "registration"},
            "session": SESSION_ID,
        }),
        "durable fixture semantic drift: observer-added event payload changed"
    );
    assert_eq!(process_events[1].sequence, 2);
    assert_eq!(process_events[1].event_type, "process.waiting");
    assert_eq!(
        process_events[1].payload,
        serde_json::json!({"wait": fixture_wait_state()}),
        "durable fixture semantic drift: waiting-process event payload changed"
    );
    assert_eq!(
        handles
            .processes
            .observers_for_process(PROCESS_ID)
            .await
            .expect("durable fixture drift: process-observer read failed"),
        vec![SESSION_ID.to_string()],
        "durable fixture semantic drift: process-observer edge changed"
    );
    let process_lease = handles
        .processes
        .get_process_lease(PROCESS_ID)
        .await
        .expect("durable fixture drift: process-lease read failed")
        .expect("durable fixture drift: process lease disappeared");
    let expected_process_lease = expected_process_lease();
    assert_eq!(
        process_lease.schema_version,
        expected_process_lease.schema_version
    );
    assert_eq!(process_lease.process_id, expected_process_lease.process_id);
    assert_eq!(process_lease.owner, expected_process_lease.owner);
    assert_eq!(
        process_lease.lease_token,
        expected_process_lease.lease_token
    );
    assert_eq!(
        process_lease.fencing_token,
        expected_process_lease.fencing_token
    );
    assert_eq!(
        process_lease.claimed_at_epoch_ms,
        expected_process_lease.claimed_at_epoch_ms
    );
    assert_eq!(
        process_lease.expires_at_epoch_ms,
        expected_process_lease.expires_at_epoch_ms
    );
    assert!(
        process_lease.expires_at_epoch_ms <= FIXTURE_READ_MS,
        "fixture process lease is intentionally expired; get_process_lease must expose the raw row without treating it as live authority"
    );
    assert_eq!(
        handles
            .continuations
            .latest_segment_handover(PROCESS_ID)
            .await
            .expect("durable fixture drift: continuation read failed"),
        Some(fixture_handover())
    );
    let loaded_env =
        load_process_execution_env(handles.process_envs.as_ref(), &expected.process_env_ref)
            .await
            .expect("durable fixture identity drift: process env ref no longer resolves");
    assert_eq!(
        serde_json::to_value(loaded_env).expect("encode loaded fixture env"),
        serde_json::to_value(fixture_process_env()).expect("encode expected fixture env")
    );
    let reregistered = handles
        .processes
        .register_process_with_observers(
            waiting_process_registration(expected.process_env_ref.clone()),
            &[SESSION_ID.to_string()],
        )
        .await
        .expect("durable fixture identity drift: identical process re-registration conflicted");
    assert_eq!(
        reregistered.registration_fingerprint,
        process.registration_fingerprint
    );
    assert_eq!(
        handles
            .processes
            .get_process(WAKE_PROCESS_ID)
            .await
            .expect("durable fixture drift: wake process read failed")
            .expect("durable fixture drift: wake process disappeared")
            .status,
        ProcessStatus::Running
    );
    let wake_events = handles
        .processes
        .events_after(WAKE_PROCESS_ID, 0)
        .await
        .expect("durable fixture drift: wake-process event read failed");
    assert_eq!(wake_events.len(), 1);
    assert_eq!(wake_events[0].sequence, 1);
    assert_eq!(wake_events[0].event_type, "fixture.wake");
    assert_eq!(
        wake_events[0].payload,
        serde_json::json!({"wake_input": "durable read wake"}),
        "durable fixture semantic drift: wake-process event payload changed"
    );
    assert!(
        handles
            .processes
            .list_wake_deliveries(None)
            .await
            .expect("durable fixture drift: wake-delivery read failed")
            .iter()
            .any(|delivery| delivery.wake.process_id == WAKE_PROCESS_ID),
        "durable fixture semantic drift: process wake delivery disappeared"
    );
    assert_eq!(
        handles
            .processes
            .wake_allocation_floor_for_testing(SESSION_ID, WAKE_PROCESS_ID)
            .await
            .expect("durable fixture drift: wake-allocation-floor read failed"),
        Some(1),
        "durable fixture semantic drift: sender wake allocation floor changed"
    );
    let redelivery = handles
        .runtime
        .enqueue_queued_work(process_wake_batch_draft(expected.wake_delivery.clone()))
        .await
        .expect_err("durable fixture drift: settled process wake was redelivered");
    assert!(
        matches!(
            redelivery,
            StoreError::ProcessWakeSequenceRewound {
                sequence: 1,
                allocation_floor: 1,
                ..
            }
        ),
        "durable fixture drift: receiver wake-redelivery fence returned {redelivery}"
    );

    match handles.processes.get_process(TOMBSTONE_PROCESS_ID).await {
        Err(lash_core::PluginError::ProcessNoLongerRetained {
            terminal_label,
            pruned_at_ms,
        }) => {
            assert_eq!(terminal_label, "completed");
            assert_eq!(pruned_at_ms, FIXTURE_WRITE_MS);
        }
        other => panic!(
            "durable fixture drift: process tombstone did not return ProcessNoLongerRetained: {other:?}"
        ),
    }
    assert_process_change_feed(handles.processes.as_ref()).await;

    let subscriptions = handles
        .triggers
        .list_subscriptions(TriggerSubscriptionFilter::for_session(SESSION_ID))
        .await
        .expect("durable fixture drift: trigger subscription read failed");
    assert_eq!(subscriptions.len(), 1);
    assert_eq!(subscriptions[0].subscription_key, TRIGGER_KEY);
    assert!(subscriptions[0].enabled);
    let occurrences = handles
        .triggers
        .list_occurrences(TriggerOccurrenceFilter::default())
        .await
        .expect("durable fixture drift: trigger occurrence read failed");
    assert_eq!(occurrences.len(), 1);
    let deliveries = handles
        .triggers
        .list_deliveries_by_occurrence_id(&occurrences[0].occurrence_id)
        .await
        .expect("durable fixture drift: trigger delivery read failed");
    assert_eq!(deliveries.len(), 1);
    assert_eq!(deliveries[0].subscription.subscription_key, TRIGGER_KEY);
    assert!(deliveries[0].subscription.enabled);
    assert_eq!(
        deliveries[0].reservation_status,
        TriggerDeliveryReservationOutcome::AlreadyReserved
    );
    assert_eq!(
        deliveries[0].occurrence.payload,
        serde_json::json!({"value": 42})
    );
    let replayed_receipt = trigger_receipt(
        handles.triggers.as_ref(),
        TRIGGER_REGISTER_OPERATION,
        fixture_register_command(expected.process_env_ref.clone()),
    )
    .await;
    assert_eq!(
        replayed_receipt.subscription_id,
        subscriptions[0].subscription_id
    );
    let unchanged = trigger_receipt(
        handles.triggers.as_ref(),
        "durable-read-trigger-reregister",
        fixture_register_command(expected.process_env_ref.clone()),
    )
    .await;
    assert_eq!(
        unchanged.disposition,
        TriggerMutationOutcome::Unchanged,
        "durable fixture identity drift: identical trigger re-registration changed meaning"
    );

    let reminted = handles
        .effects
        .await_event_key(
            &ExecutionScope::turn(SESSION_ID, "durable-read-turn"),
            AwaitEventWaitIdentity::tool_completion("durable-read-tool-call"),
        )
        .await
        .expect("durable fixture identity drift: promise key cannot be reminted");
    assert_eq!(
        reminted, expected.await_event_key,
        "durable fixture identity drift: promise key bytes changed"
    );
    assert_eq!(
        reminted.promise_key(),
        expected.await_event_key.promise_key()
    );
    assert_eq!(
        handles
            .effects
            .peek_await_event(&reminted)
            .await
            .expect("durable fixture drift: await-event peek failed"),
        Some(Resolution::Ok(serde_json::json!({"fixture": "resolved"}))),
        "durable fixture semantic drift: await-event resolution changed"
    );

    assert_eq!(
        handles
            .effects
            .resolve_await_event(
                &expected.revoked_await_event_key,
                Resolution::Ok(serde_json::json!({"fixture": "late"})),
            )
            .await
            .expect("durable fixture drift: revoked await-event resolve failed"),
        ResolveOutcome::UnknownOrRevoked,
        "durable fixture semantic drift: await-event session revocation disappeared"
    );
    let revoked_error = handles
        .effects
        .await_await_event(
            &expected.revoked_await_event_key,
            tokio_util::sync::CancellationToken::new(),
            None,
        )
        .await
        .expect_err("durable fixture drift: revoked await-event unexpectedly remained open");
    assert_eq!(
        revoked_error.code.as_str(),
        "await_event_unknown_or_revoked"
    );

    let replayed_effect = handles
        .effects
        .scoped(ExecutionScope::turn(SESSION_ID, "durable-read-effect-turn"))
        .expect("durable fixture drift: scope replayed effect journal")
        .controller()
        .execute_effect(
            fixture_effect_envelope(),
            RuntimeEffectLocalExecutor::unavailable(),
        )
        .await
        .expect("durable fixture drift: completed runtime effect did not replay");
    let RuntimeEffectOutcome::ExecCode { result } = replayed_effect else {
        panic!("durable fixture semantic drift: runtime-effect replay outcome kind changed");
    };
    let response = result.expect("durable fixture semantic drift: exec effect became an error");
    assert_eq!(response.observations, vec!["durable read effect"]);
    assert_eq!(response.duration_ms, 887);
    assert_eq!(
        response.terminal_finish,
        Some(serde_json::json!({"fixture": 887})),
        "durable fixture semantic drift: runtime-effect replay payload changed"
    );
}

/// Requires the committed expectations to equal what this build writes today.
///
/// [`assert_semantics`] is a read-back law: it decodes the previous artifact and
/// asserts the meaning recovered from it. A write-path payload-shape change is
/// invisible to it, because the committed bytes keep round-tripping through the
/// new types — an added field that is defaulted on read and skipped when absent
/// decodes, re-encodes, and re-hashes exactly as the old writer wrote it. The
/// schema-declaration gate cannot see it either: that gate only fires once a
/// fixture artifact is already in the diff.
///
/// This is the converse law (FIG-1433). The caller re-seeds a throwaway store
/// with the current code and hands the serialized expectations here, so a shape
/// change fails in the diff that introduces it instead of being absorbed by the
/// next unrelated regeneration.
///
/// Its reach is exactly [`ExpectedFixture`]: payloads that struct does not carry
/// — trigger subscription/occurrence/delivery rows and process registrations —
/// can still gain a field unflagged. The fixture README records that bound.
pub fn assert_committed_expectations_match_current_writes(committed: &[u8], written_now: &[u8]) {
    if committed == written_now {
        return;
    }
    panic!(
        "durable fixture write-shape drift: this build writes durable payloads the committed \
         expectations do not carry.{}\nDecide first whether the new write shape is intended. If \
         it is not, revert the shape change: regenerating here would absorb the drift into the \
         committed surface, which is the failure FIG-1433 closed. Drift that appears or \
         disappears between runs (without a code change) means nondeterminism in the fixture \
         inputs — e.g. a non-empty HashMap reaching serialization, or a tie in the `ORDER BY \
         generation` read — and must be fixed at the source, NOT by regenerating the fixture. \
         Only once the change is intended, bump DURABLE_READ_FIXTURE_SCHEMA_VERSION and \
         regenerate both backends:\n  {REGENERATION_COMMANDS}",
        rendered_expectation_drift(committed, written_now)
    );
}

const REGENERATION_COMMANDS: &str = "LASH_REGENERATE_DURABLE_READ_FIXTURES=1 cargo test -p \
     lash-sqlite-store --test durable_read_fixture regenerate_sqlite_durable_fixture -- \
     --ignored --exact\n  LASH_POSTGRES_DATABASE_URL=<throwaway> \
     LASH_REGENERATE_DURABLE_READ_FIXTURES=1 cargo test -p lash-postgres-store --test \
     durable_read_fixture regenerate_postgres_durable_fixture -- --ignored --exact";

fn rendered_expectation_drift(committed: &[u8], written_now: &[u8]) -> String {
    let (Ok(committed), Ok(written_now)) = (
        serde_json::from_slice::<serde_json::Value>(committed),
        serde_json::from_slice::<serde_json::Value>(written_now),
    ) else {
        return String::new();
    };
    let mut drift = Vec::new();
    collect_expectation_drift("", &committed, &written_now, &mut drift);
    if drift.is_empty() {
        return String::new();
    }
    drift.truncate(20);
    format!("\n  - {}", drift.join("\n  - "))
}

fn collect_expectation_drift(
    path: &str,
    committed: &serde_json::Value,
    written_now: &serde_json::Value,
    drift: &mut Vec<String>,
) {
    match (committed, written_now) {
        (serde_json::Value::Object(committed), serde_json::Value::Object(written_now)) => {
            let keys = committed
                .keys()
                .chain(written_now.keys())
                .collect::<BTreeSet<_>>();
            for key in keys {
                let child = format!("{path}/{key}");
                match (committed.get(key), written_now.get(key)) {
                    (Some(committed), Some(written_now)) => {
                        collect_expectation_drift(&child, committed, written_now, drift)
                    }
                    (Some(committed), None) => drift.push(format!(
                        "{child}: committed only ({})",
                        rendered_drift_value(committed)
                    )),
                    (None, written_now) => drift.push(format!(
                        "{child}: written by this build only ({})",
                        written_now.map_or_else(String::new, rendered_drift_value)
                    )),
                }
            }
        }
        (serde_json::Value::Array(committed), serde_json::Value::Array(written_now))
            if committed.len() == written_now.len() =>
        {
            for (index, (committed, written_now)) in
                committed.iter().zip(written_now.iter()).enumerate()
            {
                collect_expectation_drift(
                    &format!("{path}/{index}"),
                    committed,
                    written_now,
                    drift,
                );
            }
        }
        (committed, written_now) if committed != written_now => drift.push(format!(
            "{path}: committed {} but this build writes {}",
            rendered_drift_value(committed),
            rendered_drift_value(written_now)
        )),
        _ => {}
    }
}

fn rendered_drift_value(value: &serde_json::Value) -> String {
    let mut rendered = value.to_string();
    if rendered.chars().count() > 80 {
        rendered = rendered.chars().take(77).collect::<String>() + "...";
    }
    rendered
}

fn assert_graph_payloads(nodes: &[lash_core::SessionNodeRecord]) {
    assert_eq!(
        nodes.len(),
        3,
        "durable fixture semantic drift: graph node count changed"
    );
    match &nodes[0].payload {
        SessionNodePayload::FrameOpen {
            frame_key,
            reason,
            assignment,
            protocol_turn_options,
        } => {
            assert_eq!(frame_key, "initial-frame");
            assert_eq!(reason.as_str(), "initial");
            assert_eq!(assignment.policy.model.id, "");
            assert_eq!(assignment.policy.recorded_provider_id(), "");
            assert_eq!(assignment.policy.context_window_tokens(), 1);
            assert_eq!(assignment.policy.session_id, None);
            assert!(!assignment.policy.autonomous);
            assert_eq!(
                assignment.policy.turn_budget,
                lash_core::TurnBudget::Unbounded
            );
            assert_eq!(assignment.usage_source, None);
            assert_eq!(
                serde_json::to_value(&assignment.plugin_options)
                    .expect("encode frame plugin options"),
                serde_json::json!({})
            );
            assert_eq!(
                serde_json::to_value(protocol_turn_options)
                    .expect("encode frame protocol-turn options"),
                serde_json::to_value(ProtocolTurnOptions::default())
                    .expect("encode expected protocol-turn options")
            );
        }
        other => {
            panic!("durable fixture semantic drift: first graph node is not FrameOpen: {other:?}")
        }
    }
    match &nodes[1].payload {
        SessionNodePayload::Event {
            event: lash_core::SessionHistoryRecord::Conversation(message),
        } => {
            assert_eq!(message.role, MessageRole::User);
            assert_eq!(message.parts.len(), 1);
            assert_eq!(message.parts[0].kind, PartKind::Text);
            assert_eq!(message.parts[0].content, "durable read user message");
            assert!(matches!(
                message.origin.as_ref(),
                Some(MessageOrigin::Plugin { plugin_id, transient: false })
                    if plugin_id == "plugin"
            ));
        }
        other => panic!(
            "durable fixture semantic drift: second graph node is not the fixture message: {other:?}"
        ),
    }
    match &nodes[2].payload {
        SessionNodePayload::Plugin { plugin_type, body } => {
            assert_eq!(plugin_type, "durable-read-plugin");
            assert_eq!(
                body.as_ref(),
                &serde_json::json!({"fixture": true, "order": 2}),
                "durable fixture semantic drift: plugin node body changed"
            );
        }
        other => panic!(
            "durable fixture semantic drift: third graph node is not the fixture plugin: {other:?}"
        ),
    }
}

async fn assert_process_change_feed(processes: &dyn ProcessRegistry) {
    let (first, first_cursor) = processes
        .processes_changed_since(ProcessChangeCursor::initial(), 2)
        .await
        .expect("durable fixture drift: first process-change page failed");
    assert_eq!(first.len(), 2);
    assert!(first_cursor.store_sequence() > 0);
    let (second, final_cursor) = processes
        .processes_changed_since(first_cursor, 10)
        .await
        .expect("durable fixture drift: second process-change page failed");
    assert_eq!(second.len(), 1);
    assert!(final_cursor.store_sequence() > first_cursor.store_sequence());
    let (empty, stable_cursor) = processes
        .processes_changed_since(final_cursor, 10)
        .await
        .expect("durable fixture drift: terminal process-change page failed");
    assert!(empty.is_empty());
    assert_eq!(stable_cursor, final_cursor);

    let mut observed = BTreeMap::new();
    for change in first.into_iter().chain(second) {
        match change {
            ProcessChange::Upsert { record } => {
                observed.insert(record.id.clone(), "upsert".to_string());
            }
            ProcessChange::Deleted { tombstone } => {
                assert_eq!(tombstone.terminal_label, "completed");
                assert_eq!(tombstone.pruned_at_ms, FIXTURE_WRITE_MS);
                observed.insert(tombstone.process_id, "deleted".to_string());
            }
        }
    }
    assert_eq!(
        observed,
        BTreeMap::from([
            (PROCESS_ID.to_string(), "upsert".to_string()),
            (TOMBSTONE_PROCESS_ID.to_string(), "deleted".to_string()),
            (WAKE_PROCESS_ID.to_string(), "upsert".to_string()),
        ]),
        "durable fixture semantic drift: ADR-0020 change-feed rows changed"
    );
}

fn fixture_session_request(session_id: &str) -> SessionStoreCreateRequest {
    SessionStoreCreateRequest {
        session_id: session_id.to_string(),
        relation: SessionRelation::Root,
        policy: SessionPolicy::new(lash_core::TurnBudget::Unbounded),
    }
}

fn fixture_plugin_snapshot() -> PluginSessionSnapshot {
    PluginSessionSnapshot {
        plugins: BTreeMap::from([(
            "durable-read-snapshot-plugin".to_string(),
            PluginSnapshotEntry {
                meta: PluginSnapshotMeta {
                    plugin_id: "durable-read-snapshot-plugin".to_string(),
                    plugin_version: "8.8.7".to_string(),
                    revision: 887,
                    state: Some(serde_json::json!({"fixture": "plugin-state", "value": 887})),
                },
                artifacts: vec![PluginSnapshotArtifact {
                    name: "durable-read-artifact.bin".to_string(),
                    data: vec![8, 8, 7],
                }],
            },
        )]),
    }
}

fn fixture_effect_envelope() -> RuntimeEffectEnvelope {
    RuntimeEffectEnvelope::new(
        RuntimeInvocation::effect(
            RuntimeScope::for_turn(SESSION_ID, "durable-read-effect-turn", 7, 0),
            "durable-read-exec-effect",
            RuntimeEffectKind::ExecCode,
            "durable-read-exec-replay",
        ),
        RuntimeEffectCommand::ExecCode {
            language: "fixture".to_string(),
            code: "return 887".to_string(),
        },
    )
}

fn fixture_state() -> RuntimeSessionState {
    RuntimeSessionState {
        session_id: SESSION_ID.to_string(),
        ..RuntimeSessionState::new(lash_core::SessionPolicy::new(
            lash_core::TurnBudget::Unbounded,
        ))
    }
}

fn fixture_append_nodes() -> Vec<SessionAppendNode> {
    vec![
        SessionAppendNode::message(lash_core::PluginMessage::text(
            lash_core::MessageRole::User,
            "durable read user message",
        )),
        SessionAppendNode::plugin(
            "durable-read-plugin",
            serde_json::json!({"fixture": true, "order": 2}),
        ),
    ]
}

fn fixture_process_env() -> ProcessExecutionEnvSpec {
    ProcessExecutionEnvSpec::new(
        Default::default(),
        SessionPolicy::new(lash_core::TurnBudget::Unbounded),
    )
}

pub fn expected_process_lease() -> lash_core::ProcessLease {
    lash_core::facade_support::registry_transitions::acquired_process_lease(
        PROCESS_ID,
        &LeaseOwnerIdentity::opaque("durable-read-owner", "durable-read-incarnation"),
        1,
        FIXTURE_WRITE_MS,
        100,
    )
}

fn waiting_process_registration(env_ref: ProcessExecutionEnvRef) -> ProcessRegistration {
    ProcessRegistration::new(
        PROCESS_ID,
        ProcessInput::Engine {
            kind: "durable-read-engine".to_string(),
            payload: serde_json::json!({"fixture": "process"}),
        },
        RecoveryContract::Rerunnable,
        ProcessProvenance::host(),
    )
    .with_execution_env_ref(Some(env_ref))
    .with_identity(
        ProcessIdentity::new("durable-read-engine")
            .with_label(Some("Durable read fixture".to_string()))
            .with_definition(Some(serde_json::json!({"fixture": "process"}))),
    )
}

fn fixture_wait_state() -> WaitState {
    WaitState {
        kind: WaitKind::Signal {
            name: "fixture-ready".to_string(),
            event_type: "process.signal.fixture-ready".to_string(),
            key: "durable-read-wait-key".to_string(),
            ordinal: 1,
        },
        since_ms: 123,
    }
}

fn fixture_handover() -> PersistedSegmentHandover {
    PersistedSegmentHandover {
        segment_ordinal: 1,
        handover: SegmentHandover {
            reason: BoundaryReason::JournalBudget,
            program_hash: "durable-read-program-v1".to_string(),
            engine_state: vec![8, 8, 7],
        },
    }
}

fn fixture_register_command(env_ref: ProcessExecutionEnvRef) -> TriggerCommand {
    let mut input_template = BTreeMap::new();
    input_template.insert("event".to_string(), TriggerInputBinding::Event);
    TriggerCommand::Register {
        owner_scope: TriggerOwnerScope::session(SESSION_ID),
        actor: ProcessOriginator::session(SessionScope::new(SESSION_ID)),
        draft: TriggerSubscriptionDraft {
            subscription_key: TRIGGER_KEY.to_string(),
            env_ref,
            wake_target: Some(SessionScope::new(SESSION_ID)),
            name: Some("Durable read trigger".to_string()),
            source_type: "fixture.event".to_string(),
            source_key: "fixture-source".to_string(),
            source: serde_json::json!({"fixture": "source"}),
            payload_schema: LashSchema::new(serde_json::json!({
                "type": "object",
                "properties": {"value": {"type": "integer"}},
                "required": ["value"],
                "additionalProperties": false
            })),
            target: ProcessInput::Engine {
                kind: "durable-read-trigger-target".to_string(),
                payload: serde_json::json!({"fixture": "trigger"}),
            },
            target_identity: ProcessIdentity::new("durable-read-trigger-target")
                .with_label(Some("Durable read trigger target".to_string()))
                .with_definition(Some(serde_json::json!({"fixture": "trigger"}))),
            event_types: Vec::new(),
            input_template,
            target_label: Some("Durable read trigger target".to_string()),
        },
    }
}

async fn trigger_receipt(
    store: &dyn TriggerStore,
    operation_id: &str,
    command: TriggerCommand,
) -> lash_core::TriggerMutationReceipt {
    let outcome = store
        .execute_command(operation_id, command)
        .await
        .expect("execute fixture trigger command")
        .expect("fixture trigger command domain outcome");
    match outcome {
        TriggerCommandOutcome::Mutation { receipt } => *receipt,
        TriggerCommandOutcome::List { .. } | TriggerCommandOutcome::Prune { .. } => {
            panic!("fixture trigger command must return a mutation receipt")
        }
    }
}
