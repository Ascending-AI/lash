use super::*;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::sync::{Arc, Mutex};

use lash_core::testing::conformance::{
    StoreContractHandles, StoreContractOp, StoreContractScenario, sample_store_contract_operations,
};
use lash_core::{
    AttachmentCreateMeta, AttachmentStore, AwaitEventWaitIdentity, EffectHost, ExecutionScope,
    MediaType, ProcessExecutionEnvRef, ProcessIdentity, ProcessInput, ProcessOriginator,
    Resolution, RuntimeEffectCommand, RuntimeEffectEnvelope, RuntimeEffectKind,
    RuntimeEffectLocalExecutor, RuntimeEffectOutcome, RuntimeInvocation, RuntimeScope,
    SessionScope, TestLocalProcessRegistry, TriggerCommand, TriggerInputBinding,
    TriggerOccurrenceRequest, TriggerOwnerScope, TriggerStore, TriggerSubscriptionDraft,
    WakeDeliveryDisposition, facade_support::InMemoryAttachmentStore,
    facade_support::InMemoryTriggerStore,
};
use lash_s3_store::{S3AttachmentStore, S3AttachmentStoreConfig};
use lash_sqlite_store::{
    SqliteEffectHost, SqliteProcessRegistry, SqliteTriggerStore, Store as SqliteStore,
};

const DEFAULT_CASES: usize = 4;
const DEFAULT_SEED: u64 = 852;
const OPS_PER_CASE: usize = 32;
const SURFACE_SESSION: &str = "surface-session";
const SURFACE_TURN: &str = "surface-turn";
const ALL_SURFACE_OPERATION_KINDS: &[&str] = &[
    "register",
    "first_start",
    "enter_wait",
    "clear_wait",
    "set_external_ref",
    "signal",
    "cancel_request",
    "terminal",
    "add_observer",
    "remove_observer",
    "retarget",
    "claim_lease",
    "release_lease",
    "claim_wake",
    "mark_wake",
    "discard_wake",
    "defer_wake",
    "enqueue_wake",
    "consume_wake",
    "prune",
    "compact_tombstones",
    "trigger_register",
    "trigger_disable",
    "trigger_occurrence",
    "trigger_occurrence_null_source",
    "process_signal_zero",
    "effect_record",
    "tool_intent_batch",
    "await_resolve",
    "await_revoke_session",
];

#[derive(Clone, Debug, serde::Serialize)]
#[serde(tag = "surface", content = "operation", rename_all = "snake_case")]
enum SurfaceOperation {
    StoreContract(StoreContractOp),
    TriggerRegister { key: u8 },
    TriggerDisable { key: u8 },
    TriggerOccurrence { key: u8 },
    TriggerOccurrenceNullSource { key: u8 },
    ProcessSignalZero { negative: bool },
    EffectRecord { key: u8, duration_ms: u8 },
    ToolIntentBatch,
    AwaitResolve { key: u8 },
    AwaitRevokeSession,
}

impl SurfaceOperation {
    fn kind(&self) -> &'static str {
        match self {
            Self::StoreContract(operation) => match operation {
                StoreContractOp::Register { .. } => "register",
                StoreContractOp::FirstStart { .. } => "first_start",
                StoreContractOp::EnterWait { .. } => "enter_wait",
                StoreContractOp::ClearWait { .. } => "clear_wait",
                StoreContractOp::SetExternalRef { .. } => "set_external_ref",
                StoreContractOp::Signal { .. } => "signal",
                StoreContractOp::CancelRequest { .. } => "cancel_request",
                StoreContractOp::Terminal { .. } => "terminal",
                StoreContractOp::AddObserver { .. } => "add_observer",
                StoreContractOp::RemoveObserver { .. } => "remove_observer",
                StoreContractOp::Retarget { .. } => "retarget",
                StoreContractOp::ClaimLease { .. } => "claim_lease",
                StoreContractOp::ReleaseLease { .. } => "release_lease",
                StoreContractOp::ClaimWake => "claim_wake",
                StoreContractOp::MarkWake { .. } => "mark_wake",
                StoreContractOp::DiscardWake { .. } => "discard_wake",
                StoreContractOp::DeferWake { .. } => "defer_wake",
                StoreContractOp::EnqueueWake { .. } => "enqueue_wake",
                StoreContractOp::ConsumeWake { .. } => "consume_wake",
                StoreContractOp::Prune { .. } => "prune",
                StoreContractOp::CompactTombstones { .. } => "compact_tombstones",
            },
            Self::TriggerRegister { .. } => "trigger_register",
            Self::TriggerDisable { .. } => "trigger_disable",
            Self::TriggerOccurrence { .. } => "trigger_occurrence",
            Self::TriggerOccurrenceNullSource { .. } => "trigger_occurrence_null_source",
            Self::ProcessSignalZero { .. } => "process_signal_zero",
            Self::EffectRecord { .. } => "effect_record",
            Self::ToolIntentBatch => "tool_intent_batch",
            Self::AwaitResolve { .. } => "await_resolve",
            Self::AwaitRevokeSession => "await_revoke_session",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
struct ProcessLeaseObservation {
    process_id: String,
    owner: serde_json::Value,
    lease_token_present: bool,
    fencing_token: u64,
    claimed: bool,
    ttl_ms: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize)]
struct ProcessRows {
    records: Vec<serde_json::Value>,
    events: Vec<serde_json::Value>,
    observers: Vec<(String, String)>,
    leases: Vec<ProcessLeaseObservation>,
    wake_deliveries: Vec<serde_json::Value>,
    wake_allocation_floors: Vec<(String, String, u64)>,
    tombstones: Vec<serde_json::Value>,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize)]
struct TriggerRows {
    subscriptions: Vec<serde_json::Value>,
    mutation_receipts: Vec<serde_json::Value>,
    occurrences: Vec<serde_json::Value>,
    deliveries: Vec<serde_json::Value>,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize)]
struct SurfaceState {
    processes: ProcessRows,
    wake_redelivery_fences: Vec<(String, String, u64)>,
    triggers: TriggerRows,
    effect_journal: Option<Vec<serde_json::Value>>,
    await_journal: Option<Vec<serde_json::Value>>,
}

enum SurfaceReader {
    InMemory {
        runtime: Arc<InMemorySessionStore>,
        registry: Arc<TestLocalProcessRegistry>,
        triggers: Arc<InMemoryTriggerStore>,
    },
    Sqlite {
        runtime_path: PathBuf,
        process_path: PathBuf,
        trigger_path: PathBuf,
        effect_path: PathBuf,
    },
    Postgres {
        pool: PgPool,
    },
}

struct SurfaceRunner {
    name: &'static str,
    scenario: StoreContractScenario,
    process_registry: Arc<dyn lash_core::ProcessRegistry>,
    trigger_store: Arc<dyn TriggerStore>,
    effect_host: Arc<dyn EffectHost>,
    reader: SurfaceReader,
}

struct SurfaceIntentProvider;

impl SurfaceIntentProvider {
    fn definition() -> lash_core::ToolDefinition {
        lash_core::ToolDefinition::raw(
            "tool:surface_intent_provider",
            "surface_intent_provider",
            "Return a literal result and two durable process intents.",
            lash_core::ToolDefinition::default_input_schema(),
            serde_json::json!({"type": "object"}),
        )
    }
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for SurfaceIntentProvider {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![Self::definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == "surface_intent_provider").then(|| Arc::new(Self::definition().contract()))
    }

    async fn execute(&self, _call: lash_core::ToolCall<'_>) -> lash_core::ToolOutcome {
        panic!("the cross-backend intent provider must use AttemptContext")
    }

    async fn execute_attempt(
        &self,
        call: lash_core::ToolCall<'_>,
    ) -> lash_core::ToolAttemptOutcome {
        assert_eq!(call.context.session_id(), SURFACE_SESSION);
        assert_eq!(call.context.execution_scope_id(), SURFACE_TURN);
        assert_eq!(call.context.tool_call_id(), Some("surface-intent-call"));
        assert_eq!(call.context.attempt_number(), 1);
        assert_eq!(call.context.max_attempts(), 1);
        lash_core::ToolAttemptOutcome::done(
            lash_core::ToolOutcomeDone::ok(serde_json::json!({"ok": true})),
            lash_core::ToolIntents::v1(
                (0..2)
                    .map(|index| {
                        lash_core::ToolIntent::StartProcess(Box::new(
                            lash_core::StartProcessIntent {
                                session_id: SURFACE_SESSION.to_string(),
                                request: lash_core::ProcessStartRequest::external(
                                    format!("ignored-derived-intent-id-{index}"),
                                    ProcessOriginator::host_scoped("surface-differential"),
                                    serde_json::json!({
                                        "source": "literal-intent-row",
                                        "index": index,
                                    }),
                                ),
                                on_parent_end: lash_core::ProcessParentEndPolicy::Cancel,
                            },
                        ))
                    })
                    .collect(),
            ),
        )
    }
}

struct LiteralFrameController {
    inner: lash_core::ScopedEffectController<'static>,
    frames: Arc<Mutex<Vec<RuntimeEffectEnvelope>>>,
}

#[async_trait::async_trait]
impl lash_core::AwaitEventResolver for LiteralFrameController {
    async fn prepare_completion_key(
        &self,
        scope: &lash_core::ExecutionScope,
        wait: lash_core::AwaitEventWaitIdentity,
        may_defer: bool,
    ) -> Result<lash_core::CompletionKeyPreparation, lash_core::RuntimeError> {
        self.inner
            .controller()
            .prepare_completion_key(scope, wait, may_defer)
            .await
    }
}

#[async_trait::async_trait]
impl lash_core::RuntimeEffectController for LiteralFrameController {
    async fn runtime_effect_failure_disposition(
        &self,
        code: lash_core::RuntimeErrorCode,
    ) -> Result<lash_core::RuntimeEffectFailureDisposition, lash_core::RuntimeError> {
        self.inner
            .controller()
            .runtime_effect_failure_disposition(code)
            .await
    }

    async fn turn_control_participation(
        &self,
    ) -> Result<lash_core::TurnControlParticipation, lash_core::RuntimeError> {
        self.inner.controller().turn_control_participation().await
    }

    fn wants_segment_boundary(
        &self,
        progress: &lash_core::SegmentProgress,
    ) -> Option<lash_core::BoundaryReason> {
        self.inner.controller().wants_segment_boundary(progress)
    }

    fn supports_concurrent_effects(&self) -> bool {
        self.inner.controller().supports_concurrent_effects()
    }

    async fn execute_effect(
        &self,
        envelope: RuntimeEffectEnvelope,
        local_executor: RuntimeEffectLocalExecutor<'_>,
    ) -> Result<RuntimeEffectOutcome, lash_core::RuntimeEffectControllerError> {
        self.frames
            .lock()
            .expect("literal frame recorder lock")
            .push(envelope.clone());
        self.inner
            .controller()
            .execute_effect(envelope, local_executor)
            .await
    }
}

fn generated_surface_operations(seed: u64) -> Vec<SurfaceOperation> {
    let contract = sample_store_contract_operations(seed, OPS_PER_CASE - 8);
    let mut operations = vec![
        SurfaceOperation::TriggerRegister { key: 0 },
        SurfaceOperation::TriggerOccurrence { key: 0 },
        SurfaceOperation::EffectRecord {
            key: 0,
            duration_ms: 1,
        },
        SurfaceOperation::ToolIntentBatch,
        SurfaceOperation::AwaitResolve { key: 0 },
    ];
    for (index, operation) in contract.into_iter().enumerate() {
        operations.push(SurfaceOperation::StoreContract(operation));
        if index == 5 {
            operations.push(SurfaceOperation::TriggerDisable { key: 0 });
        }
        if index == 11 {
            operations.push(SurfaceOperation::AwaitRevokeSession);
        }
        if index == 17 {
            operations.push(SurfaceOperation::EffectRecord {
                key: 0,
                duration_ms: 1,
            });
        }
    }
    operations
}

impl SurfaceRunner {
    async fn apply(&mut self, operation: &SurfaceOperation) -> Result<(), String> {
        match operation {
            SurfaceOperation::StoreContract(operation) => self.scenario.apply(operation).await,
            SurfaceOperation::TriggerRegister { key } => {
                let subscription_key = format!("surface-{key}");
                let mut inputs = BTreeMap::new();
                inputs.insert("event".to_string(), TriggerInputBinding::Event);
                let command = TriggerCommand::Register {
                    owner_scope: TriggerOwnerScope::session(SURFACE_SESSION),
                    actor: ProcessOriginator::session(SessionScope::new(SURFACE_SESSION)),
                    draft: TriggerSubscriptionDraft {
                        subscription_key,
                        env_ref: ProcessExecutionEnvRef::new("surface-env"),
                        wake_target: Some(SessionScope::new(SURFACE_SESSION)),
                        name: Some("surface-worker".to_string()),
                        source_type: "surface.event".to_string(),
                        source_key: format!("source-{key}"),
                        source: serde_json::json!({"source": key}),
                        payload_schema: lash_core::LashSchema::any(),
                        target: ProcessInput::Engine {
                            kind: "surface".to_string(),
                            payload: serde_json::json!({"key": key}),
                        },
                        target_identity: ProcessIdentity::new("surface")
                            .with_label(Some("surface-worker".to_string())),
                        event_types: Vec::new(),
                        input_template: inputs,
                        target_label: Some("surface-worker".to_string()),
                    },
                };
                self.trigger_store
                    .execute_command(&format!("surface-register-{key}"), command)
                    .await
                    .map_err(|error| error.to_string())?
                    .map_err(|error| error.to_string())?;
                Ok(())
            }
            SurfaceOperation::TriggerDisable { key } => {
                let command = TriggerCommand::Disable {
                    owner_scope: TriggerOwnerScope::session(SURFACE_SESSION),
                    actor: ProcessOriginator::session(SessionScope::new(SURFACE_SESSION)),
                    subscription_key: format!("surface-{key}"),
                    expected_revision: 1,
                };
                self.trigger_store
                    .execute_command(&format!("surface-disable-{key}"), command)
                    .await
                    .map_err(|error| error.to_string())?
                    .map_err(|error| error.to_string())?;
                Ok(())
            }
            SurfaceOperation::TriggerOccurrence { key } => {
                self.trigger_store
                    .ingest_occurrence(
                        TriggerOccurrenceRequest::new(
                            "surface.event",
                            format!("source-{key}"),
                            serde_json::json!({"event": key}),
                            format!("surface-occurrence-{key}"),
                        )
                        .with_source(serde_json::json!({"source": key}))
                        .for_session(SURFACE_SESSION),
                    )
                    .await
                    .map_err(|error| error.to_string())?;
                Ok(())
            }
            SurfaceOperation::TriggerOccurrenceNullSource { key } => {
                self.trigger_store
                    .ingest_occurrence(
                        TriggerOccurrenceRequest::new(
                            "surface.event",
                            format!("null-source-{key}"),
                            serde_json::json!({"event": key}),
                            format!("surface-null-source-occurrence-{key}"),
                        )
                        .with_source(serde_json::Value::Null)
                        .for_session(SURFACE_SESSION),
                    )
                    .await
                    .map_err(|error| error.to_string())?;
                Ok(())
            }
            SurfaceOperation::ProcessSignalZero { negative } => {
                let payload = if *negative {
                    serde_json::json!({"value": -0.0})
                } else {
                    serde_json::json!({"value": 0.0})
                };
                self.process_registry
                    .append_event(
                        "prop-process-0",
                        lash_core::ProcessEventAppendRequest::new("property.signal", payload)
                            .with_replay_key("surface-zero-replay"),
                    )
                    .await
                    .map_err(|error| error.to_string())?;
                Ok(())
            }
            SurfaceOperation::EffectRecord { key, duration_ms } => {
                let replay_key = format!("surface-effect-{key}");
                let envelope = RuntimeEffectEnvelope::new(
                    RuntimeInvocation::effect(
                        RuntimeScope::for_turn(SURFACE_SESSION, SURFACE_TURN, 1, 0),
                        &replay_key,
                        RuntimeEffectKind::Sleep,
                        &replay_key,
                    ),
                    RuntimeEffectCommand::Sleep {
                        duration_ms: u64::from(*duration_ms),
                    },
                );
                let controller = self
                    .effect_host
                    .scoped(ExecutionScope::turn(SURFACE_SESSION, SURFACE_TURN))
                    .map_err(|error| error.to_string())?;
                let result = controller
                    .controller()
                    .execute_effect(
                        envelope,
                        RuntimeEffectLocalExecutor::testing(|_| async {
                            Ok(RuntimeEffectOutcome::Sleep)
                        }),
                    )
                    .await;
                result.map_err(|error| error.to_string())?;
                Ok(())
            }
            SurfaceOperation::ToolIntentBatch => {
                let scope = ExecutionScope::turn(SURFACE_SESSION, SURFACE_TURN);
                let inner = self
                    .effect_host
                    .scoped_static(scope.clone())
                    .map_err(|error| error.to_string())?;
                let inner = inner.ok_or_else(|| {
                    format!("{} omitted a static differential controller", self.name)
                })?;
                let frames = Arc::new(Mutex::new(Vec::new()));
                let controller = lash_core::ScopedEffectController::shared(
                    Arc::new(LiteralFrameController {
                        inner,
                        frames: Arc::clone(&frames),
                    }),
                    scope,
                )
                .map_err(|error| error.to_string())?;
                let processes = lash_core::testing::effect_backed_process_service(Arc::clone(
                    &self.process_registry,
                ));
                let completed =
                    lash_core::testing::conformance::coordinate_tool_provider_with_services(
                        controller.clone(),
                        Arc::clone(&processes),
                        SURFACE_SESSION,
                        SurfaceIntentProvider::definition(),
                        Arc::new(SurfaceIntentProvider),
                        lash_core::PreparedToolCall::from_parts(
                            "surface-intent-call",
                            lash_core::ToolId::from("tool:surface_intent_provider"),
                            "surface_intent_provider",
                            serde_json::json!({}),
                            None,
                            serde_json::Value::Null,
                        ),
                    )
                    .await
                    .map_err(|error| {
                        format!("{} provider/coordinator row failed: {error}", self.name)
                    })?;
                let intent_outcomes = completed.intent_outcomes.clone();
                let literal_intent_outcomes = vec![
                    lash_core::ToolIntentExecutionOutcome::Executed {
                        identity: lash_core::ToolIntentIdentity {
                            session_id: "surface-session".to_string(),
                            execution_scope_id: "surface-turn".to_string(),
                            tool_call_id: "surface-intent-call".to_string(),
                            intent_index: 0,
                            replay_key: "tool-intent:v1:blake3:757f8d9d430e24620947f3ea6a96ef989d19f3c46c4d01e26485622a850d65e1".to_string(),
                        },
                        kind: lash_core::ToolIntentKind::StartProcess,
                        result: serde_json::json!({
                            "__handle__": "process",
                            "id": "tool-intent:v1:blake3:757f8d9d430e24620947f3ea6a96ef989d19f3c46c4d01e26485622a850d65e1",
                            "process_id": "tool-intent:v1:blake3:757f8d9d430e24620947f3ea6a96ef989d19f3c46c4d01e26485622a850d65e1",
                            "kind": "external",
                            "status": "running"
                        }),
                        parent_end: Some(lash_core::ToolIntentParentEnd {
                            process_id: "tool-intent:v1:blake3:757f8d9d430e24620947f3ea6a96ef989d19f3c46c4d01e26485622a850d65e1".to_string(),
                            policy: lash_core::ProcessParentEndPolicy::Cancel,
                        }),
                    },
                    lash_core::ToolIntentExecutionOutcome::Executed {
                        identity: lash_core::ToolIntentIdentity {
                            session_id: "surface-session".to_string(),
                            execution_scope_id: "surface-turn".to_string(),
                            tool_call_id: "surface-intent-call".to_string(),
                            intent_index: 1,
                            replay_key: "tool-intent:v1:blake3:2c76670706b64fe2677ffa9b0af30a7c1f94b8508f85fcf6d4eb5e38d6ffc569".to_string(),
                        },
                        kind: lash_core::ToolIntentKind::StartProcess,
                        result: serde_json::json!({
                            "__handle__": "process",
                            "id": "tool-intent:v1:blake3:2c76670706b64fe2677ffa9b0af30a7c1f94b8508f85fcf6d4eb5e38d6ffc569",
                            "process_id": "tool-intent:v1:blake3:2c76670706b64fe2677ffa9b0af30a7c1f94b8508f85fcf6d4eb5e38d6ffc569",
                            "kind": "external",
                            "status": "running"
                        }),
                        parent_end: Some(lash_core::ToolIntentParentEnd {
                            process_id: "tool-intent:v1:blake3:2c76670706b64fe2677ffa9b0af30a7c1f94b8508f85fcf6d4eb5e38d6ffc569".to_string(),
                            policy: lash_core::ProcessParentEndPolicy::Cancel,
                        }),
                    },
                ];
                if intent_outcomes != literal_intent_outcomes {
                    return Err(format!(
                        "{} intent row differed from its independent literal oracle: {intent_outcomes:?}",
                        self.name
                    ));
                }
                let actions = vec![
                    lash_core::ToolIntentParentEndAction {
                        identity: lash_core::ToolIntentIdentity {
                            session_id: "surface-session".to_string(),
                            execution_scope_id: "surface-turn".to_string(),
                            tool_call_id: "surface-intent-call".to_string(),
                            intent_index: 0,
                            replay_key: "tool-intent:v1:blake3:757f8d9d430e24620947f3ea6a96ef989d19f3c46c4d01e26485622a850d65e1".to_string(),
                        },
                        parent_end: lash_core::ToolIntentParentEnd {
                            process_id: "tool-intent:v1:blake3:757f8d9d430e24620947f3ea6a96ef989d19f3c46c4d01e26485622a850d65e1".to_string(),
                            policy: lash_core::ProcessParentEndPolicy::Cancel,
                        },
                    },
                    lash_core::ToolIntentParentEndAction {
                        identity: lash_core::ToolIntentIdentity {
                            session_id: "surface-session".to_string(),
                            execution_scope_id: "surface-turn".to_string(),
                            tool_call_id: "surface-intent-call".to_string(),
                            intent_index: 1,
                            replay_key: "tool-intent:v1:blake3:2c76670706b64fe2677ffa9b0af30a7c1f94b8508f85fcf6d4eb5e38d6ffc569".to_string(),
                        },
                        parent_end: lash_core::ToolIntentParentEnd {
                            process_id: "tool-intent:v1:blake3:2c76670706b64fe2677ffa9b0af30a7c1f94b8508f85fcf6d4eb5e38d6ffc569".to_string(),
                            policy: lash_core::ProcessParentEndPolicy::Cancel,
                        },
                    },
                ];

                self.process_registry
                    .register_process(lash_core::ProcessRegistration::new(
                        "surface-intent-parent",
                        ProcessInput::External {
                            metadata: serde_json::json!({"role": "durable-parent"}),
                        },
                        lash_core::RecoveryContract::ExternallyOwned,
                        lash_core::ProcessProvenance::new(ProcessOriginator::host_scoped(
                            "surface-differential",
                        )),
                    ))
                    .await
                    .map_err(|error| error.to_string())?;
                self.process_registry
                    .complete_process_with_parent_end(
                        "surface-intent-parent",
                        lash_core::ProcessAwaitOutput::from_tool_output(
                            lash_core::ToolCallOutput::success(serde_json::json!({"ended": true})),
                        ),
                        lash_core::ProcessCompletionAuthority::external_owner(),
                        actions.clone(),
                    )
                    .await
                    .map_err(|error| error.to_string())?;
                let retained = self
                    .process_registry
                    .get_pending_parent_end_plan("surface-intent-parent")
                    .await
                    .map_err(|error| error.to_string())?
                    .ok_or_else(|| format!("{} lost its post-terminal plan", self.name))?;
                if retained.actions != actions {
                    return Err(format!(
                        "{} changed the retained parent-end plan",
                        self.name
                    ));
                }
                for action in &actions {
                    if self
                        .process_registry
                        .events_after(&action.parent_end.process_id, 0)
                        .await
                        .map_err(|error| error.to_string())?
                        .iter()
                        .any(|event| event.event_type == "process.cancel_requested")
                    {
                        return Err(format!(
                            "{} cancelled a child before the parent terminal was durable",
                            self.name
                        ));
                    }
                }

                let mut observed_parent_end_outcomes = Vec::new();
                for action in actions.iter().take(1).chain(actions.iter()) {
                    let replay_key = format!("{}:parent-end", action.identity.replay_key);
                    let mut parent = RuntimeInvocation::effect(
                        RuntimeScope::new(SURFACE_SESSION),
                        format!("tool-intent-parent-end:{}", action.identity.intent_index),
                        RuntimeEffectKind::ToolParentEnd,
                        replay_key.clone(),
                    );
                    parent.replay = Some(lash_core::RuntimeReplay {
                        key: replay_key,
                        attribution: Some(lash_core::RuntimeReplayAttribution::ToolIntent(
                            action.identity.clone(),
                        )),
                    });
                    let outcome = processes
                        .finish_recorded_intent_parent(
                            SURFACE_SESSION,
                            action.identity.clone(),
                            action.parent_end.process_id.clone(),
                            action.parent_end.policy,
                            "differential parent ended".to_string(),
                            lash_core::ProcessOpScope::new(controller.clone())
                                .with_parent_invocation(Some(parent)),
                        )
                        .await
                        .map_err(|error| error.to_string())?;
                    observed_parent_end_outcomes.push(outcome);
                }
                if observed_parent_end_outcomes
                    != vec![
                        lash_core::ToolIntentParentEndOutcome::Cancelled {
                            identity: lash_core::ToolIntentIdentity {
                                session_id: "surface-session".to_string(),
                                execution_scope_id: "surface-turn".to_string(),
                                tool_call_id: "surface-intent-call".to_string(),
                                intent_index: 0,
                                replay_key: "tool-intent:v1:blake3:757f8d9d430e24620947f3ea6a96ef989d19f3c46c4d01e26485622a850d65e1".to_string(),
                            },
                            process_id: "tool-intent:v1:blake3:757f8d9d430e24620947f3ea6a96ef989d19f3c46c4d01e26485622a850d65e1".to_string(),
                        },
                        lash_core::ToolIntentParentEndOutcome::Cancelled {
                            identity: lash_core::ToolIntentIdentity {
                                session_id: "surface-session".to_string(),
                                execution_scope_id: "surface-turn".to_string(),
                                tool_call_id: "surface-intent-call".to_string(),
                                intent_index: 0,
                                replay_key: "tool-intent:v1:blake3:757f8d9d430e24620947f3ea6a96ef989d19f3c46c4d01e26485622a850d65e1".to_string(),
                            },
                            process_id: "tool-intent:v1:blake3:757f8d9d430e24620947f3ea6a96ef989d19f3c46c4d01e26485622a850d65e1".to_string(),
                        },
                        lash_core::ToolIntentParentEndOutcome::Cancelled {
                            identity: lash_core::ToolIntentIdentity {
                                session_id: "surface-session".to_string(),
                                execution_scope_id: "surface-turn".to_string(),
                                tool_call_id: "surface-intent-call".to_string(),
                                intent_index: 1,
                                replay_key: "tool-intent:v1:blake3:2c76670706b64fe2677ffa9b0af30a7c1f94b8508f85fcf6d4eb5e38d6ffc569".to_string(),
                            },
                            process_id: "tool-intent:v1:blake3:2c76670706b64fe2677ffa9b0af30a7c1f94b8508f85fcf6d4eb5e38d6ffc569".to_string(),
                        },
                    ]
                {
                    return Err(format!(
                        "{} returned non-literal parent-end outcomes: {observed_parent_end_outcomes:?}",
                        self.name
                    ));
                }
                let observed_parent_end_frames = frames
                    .lock()
                    .expect("literal frame recorder lock")
                    .iter()
                    .filter_map(|envelope| match &envelope.command {
                        RuntimeEffectCommand::Process { command }
                            if matches!(
                                command.as_ref(),
                                lash_core::ProcessCommand::ParentEnd { .. }
                            ) =>
                        {
                            Some(serde_json::json!({
                                "replay_key": envelope.invocation.replay_key(),
                                "command": command,
                            }))
                        }
                        _ => None,
                    })
                    .collect::<Vec<_>>();
                let expected_parent_end_frames = vec![
                    serde_json::json!({
                        "replay_key": "tool-intent:v1:blake3:757f8d9d430e24620947f3ea6a96ef989d19f3c46c4d01e26485622a850d65e1:parent-end:process:parent-end:tool-intent:v1:blake3:757f8d9d430e24620947f3ea6a96ef989d19f3c46c4d01e26485622a850d65e1",
                        "command": {
                            "op": "parent_end",
                            "identity": {
                                "session_id": "surface-session",
                                "execution_scope_id": "surface-turn",
                                "tool_call_id": "surface-intent-call",
                                "intent_index": 0,
                                "replay_key": "tool-intent:v1:blake3:757f8d9d430e24620947f3ea6a96ef989d19f3c46c4d01e26485622a850d65e1"
                            },
                            "process_id": "tool-intent:v1:blake3:757f8d9d430e24620947f3ea6a96ef989d19f3c46c4d01e26485622a850d65e1",
                            "policy": "cancel",
                            "reason": "differential parent ended"
                        }
                    }),
                    serde_json::json!({
                        "replay_key": "tool-intent:v1:blake3:757f8d9d430e24620947f3ea6a96ef989d19f3c46c4d01e26485622a850d65e1:parent-end:process:parent-end:tool-intent:v1:blake3:757f8d9d430e24620947f3ea6a96ef989d19f3c46c4d01e26485622a850d65e1",
                        "command": {
                            "op": "parent_end",
                            "identity": {
                                "session_id": "surface-session",
                                "execution_scope_id": "surface-turn",
                                "tool_call_id": "surface-intent-call",
                                "intent_index": 0,
                                "replay_key": "tool-intent:v1:blake3:757f8d9d430e24620947f3ea6a96ef989d19f3c46c4d01e26485622a850d65e1"
                            },
                            "process_id": "tool-intent:v1:blake3:757f8d9d430e24620947f3ea6a96ef989d19f3c46c4d01e26485622a850d65e1",
                            "policy": "cancel",
                            "reason": "differential parent ended"
                        }
                    }),
                    serde_json::json!({
                        "replay_key": "tool-intent:v1:blake3:2c76670706b64fe2677ffa9b0af30a7c1f94b8508f85fcf6d4eb5e38d6ffc569:parent-end:process:parent-end:tool-intent:v1:blake3:2c76670706b64fe2677ffa9b0af30a7c1f94b8508f85fcf6d4eb5e38d6ffc569",
                        "command": {
                            "op": "parent_end",
                            "identity": {
                                "session_id": "surface-session",
                                "execution_scope_id": "surface-turn",
                                "tool_call_id": "surface-intent-call",
                                "intent_index": 1,
                                "replay_key": "tool-intent:v1:blake3:2c76670706b64fe2677ffa9b0af30a7c1f94b8508f85fcf6d4eb5e38d6ffc569"
                            },
                            "process_id": "tool-intent:v1:blake3:2c76670706b64fe2677ffa9b0af30a7c1f94b8508f85fcf6d4eb5e38d6ffc569",
                            "policy": "cancel",
                            "reason": "differential parent ended"
                        }
                    }),
                ];
                if observed_parent_end_frames != expected_parent_end_frames {
                    return Err(format!(
                        "{} parent-end command frames differed from the literal oracle: {observed_parent_end_frames:#?}",
                        self.name
                    ));
                }
                self.process_registry
                    .complete_parent_end_plan("surface-intent-parent")
                    .await
                    .map_err(|error| error.to_string())?;
                let mut durable_children = Vec::new();
                let mut durable_cancel_events = Vec::new();
                for action in &actions {
                    let record = self
                        .process_registry
                        .get_process(&action.parent_end.process_id)
                        .await
                        .map_err(|error| error.to_string())?
                        .ok_or_else(|| format!("{} lost a literal intent child", self.name))?;
                    durable_children.push(serde_json::json!({
                        "id": record.id,
                        "input": record.input,
                        "status": record.status,
                    }));
                    let events = self
                        .process_registry
                        .events_after(&action.parent_end.process_id, 0)
                        .await
                        .map_err(|error| error.to_string())?;
                    let count = events
                        .iter()
                        .filter(|event| event.event_type == "process.cancel_requested")
                        .count();
                    if count != 1 {
                        return Err(format!(
                            "{} applied child cancellation {count} times after redrive",
                            self.name
                        ));
                    }
                    durable_cancel_events.extend(events.into_iter().filter_map(|event| {
                        (event.event_type == "process.cancel_requested").then(|| {
                            serde_json::json!({
                                "process_id": event.process_id,
                                "event_type": event.event_type,
                                "payload": event.payload,
                            })
                        })
                    }));
                }
                if durable_children
                    != vec![
                        serde_json::json!({
                            "id": "tool-intent:v1:blake3:757f8d9d430e24620947f3ea6a96ef989d19f3c46c4d01e26485622a850d65e1",
                            "input": {"type": "external", "metadata": {"source": "literal-intent-row", "index": 0}},
                            "status": "running"
                        }),
                        serde_json::json!({
                            "id": "tool-intent:v1:blake3:2c76670706b64fe2677ffa9b0af30a7c1f94b8508f85fcf6d4eb5e38d6ffc569",
                            "input": {"type": "external", "metadata": {"source": "literal-intent-row", "index": 1}},
                            "status": "running"
                        }),
                    ]
                    || durable_cancel_events
                        != vec![
                            serde_json::json!({
                                "process_id": "tool-intent:v1:blake3:757f8d9d430e24620947f3ea6a96ef989d19f3c46c4d01e26485622a850d65e1",
                                "event_type": "process.cancel_requested",
                                "payload": {"reason": "differential parent ended"}
                            }),
                            serde_json::json!({
                                "process_id": "tool-intent:v1:blake3:2c76670706b64fe2677ffa9b0af30a7c1f94b8508f85fcf6d4eb5e38d6ffc569",
                                "event_type": "process.cancel_requested",
                                "payload": {"reason": "differential parent ended"}
                            }),
                        ]
                {
                    return Err(format!(
                        "{} durable intent rows/events differed from literal oracles: children={durable_children:#?} events={durable_cancel_events:#?}",
                        self.name
                    ));
                }
                let observed = serde_json::to_value(RuntimeEffectOutcome::ToolBatch {
                    launches: vec![lash_core::runtime::ToolCallLaunch::Done {
                        result: Box::new(completed),
                    }],
                    triggers: Vec::new(),
                    settlement_order: vec![0],
                })
                .expect("serialize observed non-empty intent batch");
                let literal_model_return = serde_json::json!({
                    "type": "tool_batch",
                    "launches": [{
                        "status": "done",
                        "result": {
                            "call_id": "surface-intent-call",
                            "tool_name": "surface_intent_provider",
                            "args": {},
                            "output": {"outcome": {
                                "status": "success",
                                "payload": {
                                    "$lash_tool_value": "untrusted_json",
                                    "value": {"ok": true}
                                }
                            }},
                            "model_return": {
                                "call_id": "surface-intent-call",
                                "tool_name": "surface_intent_provider",
                                "parts": [
                                    {"type": "text", "text": "{\"ok\":true}"},
                                    {
                                        "type": "text",
                                        "text": "[tool intent start_process #0 executed: {\"__handle__\":\"process\",\"id\":\"tool-intent:v1:blake3:757f8d9d430e24620947f3ea6a96ef989d19f3c46c4d01e26485622a850d65e1\",\"kind\":\"external\",\"process_id\":\"tool-intent:v1:blake3:757f8d9d430e24620947f3ea6a96ef989d19f3c46c4d01e26485622a850d65e1\",\"status\":\"running\"}]"
                                    },
                                    {
                                        "type": "text",
                                        "text": "[tool intent start_process #1 executed: {\"__handle__\":\"process\",\"id\":\"tool-intent:v1:blake3:2c76670706b64fe2677ffa9b0af30a7c1f94b8508f85fcf6d4eb5e38d6ffc569\",\"kind\":\"external\",\"process_id\":\"tool-intent:v1:blake3:2c76670706b64fe2677ffa9b0af30a7c1f94b8508f85fcf6d4eb5e38d6ffc569\",\"status\":\"running\"}]"
                                    }
                                ]
                            },
                            "duration_ms": 0,
                            "intent_outcomes": [
                                {
                                    "status": "executed",
                                    "identity": {
                                        "session_id": "surface-session",
                                        "execution_scope_id": "surface-turn",
                                        "tool_call_id": "surface-intent-call",
                                        "intent_index": 0,
                                        "replay_key": "tool-intent:v1:blake3:757f8d9d430e24620947f3ea6a96ef989d19f3c46c4d01e26485622a850d65e1"
                                    },
                                    "kind": "start_process",
                                    "result": {
                                        "__handle__": "process",
                                        "id": "tool-intent:v1:blake3:757f8d9d430e24620947f3ea6a96ef989d19f3c46c4d01e26485622a850d65e1",
                                        "process_id": "tool-intent:v1:blake3:757f8d9d430e24620947f3ea6a96ef989d19f3c46c4d01e26485622a850d65e1",
                                        "kind": "external",
                                        "status": "running"
                                    },
                                    "parent_end": {
                                        "process_id": "tool-intent:v1:blake3:757f8d9d430e24620947f3ea6a96ef989d19f3c46c4d01e26485622a850d65e1",
                                        "policy": "cancel"
                                    }
                                },
                                {
                                    "status": "executed",
                                    "identity": {
                                        "session_id": "surface-session",
                                        "execution_scope_id": "surface-turn",
                                        "tool_call_id": "surface-intent-call",
                                        "intent_index": 1,
                                        "replay_key": "tool-intent:v1:blake3:2c76670706b64fe2677ffa9b0af30a7c1f94b8508f85fcf6d4eb5e38d6ffc569"
                                    },
                                    "kind": "start_process",
                                    "result": {
                                        "__handle__": "process",
                                        "id": "tool-intent:v1:blake3:2c76670706b64fe2677ffa9b0af30a7c1f94b8508f85fcf6d4eb5e38d6ffc569",
                                        "process_id": "tool-intent:v1:blake3:2c76670706b64fe2677ffa9b0af30a7c1f94b8508f85fcf6d4eb5e38d6ffc569",
                                        "kind": "external",
                                        "status": "running"
                                    },
                                    "parent_end": {
                                        "process_id": "tool-intent:v1:blake3:2c76670706b64fe2677ffa9b0af30a7c1f94b8508f85fcf6d4eb5e38d6ffc569",
                                        "policy": "cancel"
                                    }
                                }
                            ],
                            "replay": null
                        }
                    }],
                    "settlement_order": [0]
                });
                if observed != literal_model_return {
                    return Err(format!(
                        "{} non-empty intent batch differed from its literal per-tier oracle: {observed:#?}",
                        self.name
                    ));
                }
                Ok(())
            }
            SurfaceOperation::AwaitResolve { key } => {
                let scope = ExecutionScope::turn(SURFACE_SESSION, SURFACE_TURN);
                let await_key = self
                    .effect_host
                    .await_event_key(
                        &scope,
                        AwaitEventWaitIdentity::tool_completion(format!("surface-call-{key}")),
                    )
                    .await
                    .map_err(|error| error.to_string())?;
                self.effect_host
                    .resolve_await_event(
                        &await_key,
                        Resolution::Ok(serde_json::json!({"resolved": key})),
                    )
                    .await
                    .map_err(|error| error.to_string())?;
                Ok(())
            }
            SurfaceOperation::AwaitRevokeSession => self
                .effect_host
                .revoke_await_events_for_session(SURFACE_SESSION)
                .await
                .map_err(|error| error.to_string()),
        }
    }

    async fn observe(&self) -> SurfaceState {
        self.reader.observe().await
    }
}

impl SurfaceReader {
    async fn observe(&self) -> SurfaceState {
        match self {
            Self::InMemory {
                runtime,
                registry,
                triggers,
            } => SurfaceState {
                processes: process_rows_from_memory(registry).await,
                wake_redelivery_fences: runtime.raw_wake_redelivery_fences_for_testing(),
                triggers: trigger_rows_from_memory(triggers),
                effect_journal: None,
                await_journal: None,
            },
            Self::Sqlite {
                runtime_path,
                process_path,
                trigger_path,
                effect_path,
            } => read_sqlite_surface(runtime_path, process_path, trigger_path, effect_path),
            Self::Postgres { pool } => read_postgres_surface(pool).await,
        }
    }
}

async fn process_rows_from_memory(registry: &TestLocalProcessRegistry) -> ProcessRows {
    let raw = registry.raw_state_for_testing().await;
    ProcessRows {
        records: raw
            .records
            .into_iter()
            .map(|(record, change_seq)| {
                normalized_json(serde_json::json!({"change_seq": change_seq, "record": record}))
            })
            .collect(),
        events: raw
            .events
            .into_iter()
            .map(|(process_id, event)| {
                normalized_json(serde_json::json!({"process_id": process_id, "event": event}))
            })
            .collect(),
        observers: raw.observers,
        leases: raw
            .leases
            .into_iter()
            .map(|lease| ProcessLeaseObservation {
                process_id: lease.process_id,
                lease_token_present: !lease.lease_token.is_empty(),
                owner: if lease.lease_token.is_empty() {
                    serde_json::Value::Null
                } else {
                    serde_json::to_value(lease.owner).expect("encode process lease owner")
                },
                fencing_token: lease.fencing_token,
                claimed: lease.claimed_at_epoch_ms != 0,
                ttl_ms: (lease.claimed_at_epoch_ms != 0).then_some(
                    lease
                        .expires_at_epoch_ms
                        .saturating_sub(lease.claimed_at_epoch_ms),
                ),
            })
            .collect(),
        wake_deliveries: raw
            .wake_deliveries
            .into_iter()
            .map(normalized_memory_wake_delivery)
            .collect(),
        wake_allocation_floors: raw.wake_allocation_floors,
        tombstones: raw
            .tombstones
            .into_iter()
            .map(|row| normalized_json(serde_json::to_value(row).expect("encode tombstone")))
            .collect(),
    }
}

fn normalized_memory_wake_delivery(delivery: lash_core::WakeDelivery) -> serde_json::Value {
    let state = delivery.state();
    let claim_token = match &delivery.disposition {
        WakeDeliveryDisposition::Enqueuing { claim_token } => Some(claim_token.clone()),
        _ => None,
    };
    let discard_reason = delivery.disposition.discard_reason();
    let mut value = serde_json::json!({
        "delivery_id": delivery.delivery_id,
        "wake": delivery.wake,
        "state": state,
        "attempts": delivery.attempts,
        "first_attempt_ms": delivery.first_attempt_ms,
        "next_attempt_at_ms": delivery.next_attempt_at_ms,
        "expires_at_ms": delivery.expires_at_ms,
        "discard_reason": discard_reason,
    });
    if let Some(claim_token) = claim_token {
        value
            .as_object_mut()
            .expect("wake delivery projection is an object")
            .insert("claim_token".to_string(), serde_json::json!(claim_token));
    }
    normalized_json(value)
}

fn trigger_rows_from_memory(store: &InMemoryTriggerStore) -> TriggerRows {
    let raw = store.raw_state_for_testing();
    let mut incarnations = BTreeMap::new();
    TriggerRows {
        subscriptions: raw
            .subscriptions
            .into_iter()
            .map(|row| {
                normalized_trigger_json(serde_json::to_value(row).unwrap(), &mut incarnations)
            })
            .collect(),
        mutation_receipts: raw
            .mutation_receipts
            .into_iter()
            .map(
                |(operation_id, request_fingerprint, result, _created_at_ms)| {
                    normalized_trigger_receipt_json(
                        serde_json::json!({
                            "operation_id": operation_id,
                            "request_fingerprint": request_fingerprint,
                            "result": result,
                        }),
                        &mut incarnations,
                    )
                },
            )
            .collect(),
        occurrences: raw
            .occurrences
            .into_iter()
            .map(|record| {
                normalized_trigger_json(serde_json::json!({"record": record}), &mut incarnations)
            })
            .collect(),
        deliveries: raw
            .deliveries
            .into_iter()
            .map(
                |(occurrence_id, subscription_id, process_id, _created_at_ms, snapshot)| {
                    normalized_trigger_delivery_json(
                        serde_json::json!({
                            "occurrence_id": occurrence_id,
                            "subscription_id": subscription_id,
                            "process_id": process_id,
                            "subscription_snapshot": snapshot,
                        }),
                        &mut incarnations,
                    )
                },
            )
            .collect(),
    }
}

fn normalized_json(mut value: serde_json::Value) -> serde_json::Value {
    normalize_json_fields(&mut value, None);
    value
}

fn normalized_trigger_json(
    mut value: serde_json::Value,
    incarnations: &mut BTreeMap<String, String>,
) -> serde_json::Value {
    normalize_json_fields(&mut value, Some(incarnations));
    value
}

fn normalized_trigger_receipt_json(
    mut value: serde_json::Value,
    incarnations: &mut BTreeMap<String, String>,
) -> serde_json::Value {
    if let Some(result) = value
        .get_mut("result")
        .and_then(serde_json::Value::as_object_mut)
    {
        for variant in ["Ok", "Err"] {
            if let Some(payload) = result
                .get_mut(variant)
                .and_then(serde_json::Value::as_object_mut)
            {
                // SQL stores use this private field to classify retention. The
                // typed receipt returned to callers, and the in-memory row,
                // deliberately keep that metadata outside the logical result.
                payload.remove("_owner_scope_namespace");
            }
        }
    }
    normalized_trigger_json(value, incarnations)
}

fn normalized_trigger_delivery_json(
    mut value: serde_json::Value,
    incarnations: &mut BTreeMap<String, String>,
) -> serde_json::Value {
    let fields = value
        .as_object_mut()
        .expect("trigger delivery observation must be an object");
    let occurrence_id = fields
        .get("occurrence_id")
        .and_then(serde_json::Value::as_str)
        .expect("trigger delivery occurrence id");
    let subscription_id = fields
        .get("subscription_id")
        .and_then(serde_json::Value::as_str)
        .expect("trigger delivery subscription id");
    let process_id = fields
        .get("process_id")
        .and_then(serde_json::Value::as_str)
        .expect("trigger delivery process id");
    let snapshot = fields
        .get("subscription_snapshot")
        .and_then(serde_json::Value::as_object)
        .expect("trigger delivery subscription snapshot");
    let incarnation = snapshot
        .get("incarnation")
        .and_then(serde_json::Value::as_str)
        .expect("trigger delivery subscription incarnation");
    let revision = snapshot
        .get("revision")
        .and_then(serde_json::Value::as_u64)
        .expect("trigger delivery subscription revision");
    let expected_process_id = lash_core::facade_support::deterministic_delivery_process_id(
        occurrence_id,
        subscription_id,
        incarnation,
        revision,
    )
    .expect("derive trigger delivery process id");
    let process_id_matches_derivation = process_id == expected_process_id;
    fields.remove("process_id");
    fields.insert(
        "process_id_matches_derivation".to_string(),
        serde_json::Value::Bool(process_id_matches_derivation),
    );
    normalized_trigger_json(value, incarnations)
}

fn normalize_json_fields(
    value: &mut serde_json::Value,
    mut incarnations: Option<&mut BTreeMap<String, String>>,
) {
    match value {
        serde_json::Value::Array(values) => {
            for value in values {
                normalize_json_fields(value, incarnations.as_deref_mut());
            }
        }
        serde_json::Value::Object(fields) => {
            for (name, value) in fields {
                match name.as_str() {
                    "created_at_ms"
                    | "updated_at_ms"
                    | "occurred_at_ms"
                    | "deleted_at_ms"
                    | "pruned_at_ms"
                    | "first_attempt_ms"
                    | "next_attempt_at_ms"
                    | "expires_at_ms"
                    | "created_at_epoch_ms"
                    | "updated_at_epoch_ms"
                    | "claimed_at_epoch_ms"
                    | "expires_at_epoch_ms"
                    | "resolved_at_ms"
                    | "lease_expires_at_ms"
                    | "due_at_ms"
                    | "occurred_at" => {
                        if !value.is_null() {
                            *value = serde_json::json!("normalized_timestamp");
                        }
                    }
                    "claim_token" | "lease_token" | "lease_owner_id" => {
                        *value = serde_json::Value::Bool(!value.is_null());
                    }
                    "incarnation" | "subscription_incarnation" => {
                        if let (Some(raw), Some(map)) =
                            (value.as_str(), incarnations.as_deref_mut())
                        {
                            let next = map.len();
                            let alias = map
                                .entry(raw.to_string())
                                .or_insert_with(|| format!("incarnation-{next}"))
                                .clone();
                            *value = serde_json::Value::String(alias);
                        }
                    }
                    _ => normalize_json_fields(value, incarnations.as_deref_mut()),
                }
            }
        }
        _ => {}
    }
}

fn read_sqlite_surface(
    runtime_path: &Path,
    process_path: &Path,
    trigger_path: &Path,
    effect_path: &Path,
) -> SurfaceState {
    let runtime = rusqlite::Connection::open(runtime_path).expect("open SQLite runtime reader");
    let process = rusqlite::Connection::open(process_path).expect("open SQLite process reader");
    let trigger = rusqlite::Connection::open(trigger_path).expect("open SQLite trigger reader");
    let effect = rusqlite::Connection::open(effect_path).expect("open SQLite effect reader");
    let records = sqlite_simple_json_rows(
        &process,
        "SELECT record_json, change_seq FROM processes ORDER BY process_id",
        |row| {
            let record: String = row.get(0)?;
            Ok(normalized_json(serde_json::json!({
                "change_seq": row.get::<_, i64>(1)?,
                "record": serde_json::from_str::<serde_json::Value>(&record).unwrap(),
            })))
        },
    );
    let events = sqlite_simple_json_rows(
        &process,
        "SELECT process_id, event_json FROM process_events ORDER BY process_id, sequence",
        |row| {
            let event: String = row.get(1)?;
            Ok(normalized_json(serde_json::json!({
                "process_id": row.get::<_, String>(0)?,
                "event": serde_json::from_str::<serde_json::Value>(&event).unwrap(),
            })))
        },
    );
    let observers = {
        let mut stmt = process
            .prepare("SELECT session_id, process_id FROM process_observers ORDER BY session_id, process_id")
            .unwrap();
        stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    };
    let leases = {
        let mut stmt = process
            .prepare(
                "SELECT process_id, lease_owner_id, lease_owner_incarnation_id,
                    lease_owner_liveness_json, lease_token, lease_fencing_token,
                    lease_claimed_at_ms, lease_expires_at_ms
             FROM process_leases ORDER BY process_id",
            )
            .unwrap();
        stmt.query_map([], |row| {
            let owner_id: Option<String> = row.get(1)?;
            let incarnation_id: Option<String> = row.get(2)?;
            let liveness: Option<String> = row.get(3)?;
            let claimed: i64 = row.get(6)?;
            let expires: i64 = row.get(7)?;
            Ok(ProcessLeaseObservation {
                process_id: row.get(0)?,
                lease_token_present: row.get::<_, Option<String>>(4)?.is_some(),
                owner: if row.get::<_, Option<String>>(4)?.is_some() {
                    serde_json::to_value(decode_lease_owner(owner_id, incarnation_id, liveness))
                        .unwrap()
                } else {
                    serde_json::Value::Null
                },
                fencing_token: row.get::<_, i64>(5)? as u64,
                claimed: claimed != 0,
                ttl_ms: (claimed != 0).then_some((expires - claimed) as u64),
            })
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
    };
    let wake_deliveries = sqlite_simple_json_rows(
        &process,
        "SELECT delivery_id, delivery_json, state, claim_token, attempts, first_attempt_ms,
                next_attempt_at_ms, expires_at_ms, discard_reason
         FROM process_wake_deliveries ORDER BY delivery_id",
        |row| {
            let json: String = row.get(1)?;
            let wake: serde_json::Value = serde_json::from_str(&json).unwrap();
            let mut value = serde_json::json!({
                "delivery_id": row.get::<_, String>(0)?,
                "wake": wake,
            });
            let fields = value.as_object_mut().unwrap();
            fields.insert(
                "state".to_string(),
                serde_json::json!(row.get::<_, String>(2)?),
            );
            if let Some(token) = row.get::<_, Option<String>>(3)? {
                fields.insert("claim_token".to_string(), serde_json::json!(token));
            } else {
                fields.remove("claim_token");
            }
            fields.insert(
                "attempts".to_string(),
                serde_json::json!(row.get::<_, i64>(4)?),
            );
            fields.insert(
                "first_attempt_ms".to_string(),
                serde_json::json!(row.get::<_, Option<i64>>(5)?),
            );
            fields.insert(
                "next_attempt_at_ms".to_string(),
                serde_json::json!(row.get::<_, i64>(6)?),
            );
            fields.insert(
                "expires_at_ms".to_string(),
                serde_json::json!(row.get::<_, i64>(7)?),
            );
            fields.insert(
                "discard_reason".to_string(),
                serde_json::json!(row.get::<_, Option<String>>(8)?),
            );
            Ok(normalized_json(value))
        },
    );
    let wake_allocation_floors = {
        let mut stmt = process
            .prepare(
                "SELECT target_session_id, process_id, allocation_floor
                 FROM wake_allocation_floors ORDER BY target_session_id, process_id",
            )
            .unwrap();
        stmt.query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get::<_, i64>(2)? as u64))
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
    };
    let tombstones = sqlite_simple_json_rows(
        &process,
        "SELECT process_id, terminal_label, pruned_at_ms, pruned_change_seq
         FROM process_tombstones ORDER BY process_id",
        |row| {
            Ok(normalized_json(serde_json::json!({
                "process_id": row.get::<_, String>(0)?,
                "terminal_label": row.get::<_, String>(1)?,
                "pruned_at_ms": row.get::<_, i64>(2)?,
                "pruned_change_seq": row.get::<_, i64>(3)?,
            })))
        },
    );
    let wake_redelivery_fences = {
        let mut stmt = runtime
            .prepare(
                "SELECT session_id, process_id, allocation_floor
             FROM wake_redelivery_fences ORDER BY session_id, process_id",
            )
            .unwrap();
        stmt.query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get::<_, i64>(2)? as u64))
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
    };
    SurfaceState {
        processes: ProcessRows {
            records,
            events,
            observers,
            leases,
            wake_deliveries,
            wake_allocation_floors,
            tombstones,
        },
        wake_redelivery_fences,
        triggers: read_sqlite_triggers(&trigger),
        effect_journal: Some(sqlite_simple_json_rows(
            &effect,
            "SELECT scope_id, session_id, replay_key, envelope_hash, envelope_json, status,
                    outcome_json, error_json, lease_owner_id, lease_token,
                    lease_expires_at_ms, due_at_ms
             FROM runtime_effect_replay ORDER BY scope_id, replay_key",
            |row| {
                let envelope: String = row.get(4)?;
                Ok(normalized_json(serde_json::json!({
                    "scope_id": row.get::<_, String>(0)?,
                    "session_id": row.get::<_, Option<String>>(1)?,
                    "replay_key": row.get::<_, String>(2)?,
                    "envelope_hash": row.get::<_, String>(3)?,
                    "envelope": serde_json::from_str::<serde_json::Value>(&envelope).unwrap(),
                    "status": row.get::<_, String>(5)?,
                    "outcome": row.get::<_, Option<String>>(6)?.map(|v| serde_json::from_str::<serde_json::Value>(&v).unwrap()),
                    "error": row.get::<_, Option<String>>(7)?.map(|v| serde_json::from_str::<serde_json::Value>(&v).unwrap()),
                    "lease_owner_id": row.get::<_, Option<String>>(8)?,
                    "lease_token": row.get::<_, Option<String>>(9)?,
                    "lease_expires_at_ms": row.get::<_, i64>(10)?,
                    "due_at_ms": row.get::<_, Option<i64>>(11)?,
                })))
            },
        )),
        await_journal: Some(read_sqlite_await(&effect)),
    }
}

fn sqlite_simple_json_rows<F>(
    connection: &rusqlite::Connection,
    query: &str,
    decode: F,
) -> Vec<serde_json::Value>
where
    F: FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<serde_json::Value>,
{
    let mut stmt = connection.prepare(query).unwrap();
    stmt.query_map([], decode)
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
}

fn read_sqlite_triggers(connection: &rusqlite::Connection) -> TriggerRows {
    let mut incarnations = BTreeMap::new();
    let subscriptions = sqlite_simple_json_rows(
        connection,
        "SELECT record_json FROM trigger_subscriptions ORDER BY subscription_id",
        |row| {
            let json: String = row.get(0)?;
            Ok(serde_json::from_str(&json).unwrap())
        },
    )
    .into_iter()
    .map(|row| normalized_trigger_json(row, &mut incarnations))
    .collect();
    let mutation_receipts = sqlite_simple_json_rows(connection, "SELECT operation_id, request_fingerprint, result_json FROM trigger_mutation_receipts ORDER BY operation_id", |row| {
        let result: String = row.get(2)?;
        Ok(serde_json::json!({"operation_id": row.get::<_, String>(0)?, "request_fingerprint": row.get::<_, String>(1)?, "result": serde_json::from_str::<serde_json::Value>(&result).unwrap()}))
    }).into_iter().map(|row| normalized_trigger_receipt_json(row, &mut incarnations)).collect();
    let occurrences = sqlite_simple_json_rows(connection, "SELECT record_json FROM trigger_occurrences ORDER BY occurrence_id", |row| {
        let record: String = row.get(0)?;
        Ok(serde_json::json!({"record": serde_json::from_str::<serde_json::Value>(&record).unwrap()}))
    }).into_iter().map(|row| normalized_trigger_json(row, &mut incarnations)).collect();
    let deliveries = sqlite_simple_json_rows(connection, "SELECT occurrence_id, subscription_id, process_id, subscription_snapshot_json FROM trigger_deliveries ORDER BY occurrence_id, subscription_id", |row| {
        let snapshot: String = row.get(3)?;
        Ok(serde_json::json!({"occurrence_id": row.get::<_, String>(0)?, "subscription_id": row.get::<_, String>(1)?, "process_id": row.get::<_, String>(2)?, "subscription_snapshot": serde_json::from_str::<serde_json::Value>(&snapshot).unwrap()}))
    }).into_iter().map(|row| normalized_trigger_delivery_json(row, &mut incarnations)).collect();
    TriggerRows {
        subscriptions,
        mutation_receipts,
        occurrences,
        deliveries,
    }
}

fn read_sqlite_await(connection: &rusqlite::Connection) -> Vec<serde_json::Value> {
    let mut rows = sqlite_simple_json_rows(
        connection,
        "SELECT key_id, scope_json, wait_json, session_id, turn_control, terminal_json, resolved_at_ms FROM await_event_waits ORDER BY key_id",
        |row| {
            let scope: String = row.get(1)?;
            let wait: String = row.get(2)?;
            let terminal: Option<String> = row.get(5)?;
            Ok(normalized_json(serde_json::json!({
                "kind": "wait", "key_id": row.get::<_, String>(0)?,
                "scope": serde_json::from_str::<serde_json::Value>(&scope).unwrap(),
                "wait": serde_json::from_str::<serde_json::Value>(&wait).unwrap(),
                "session_id": row.get::<_, Option<String>>(3)?,
                "turn_control": row.get::<_, i64>(4)? != 0,
                "terminal": terminal.map(|v| serde_json::from_str::<serde_json::Value>(&v).unwrap()),
                "resolved_at_ms": row.get::<_, Option<i64>>(6)?,
            })))
        },
    );
    rows.extend(sqlite_simple_json_rows(connection, "SELECT session_id FROM await_event_revoked_sessions ORDER BY session_id", |row| {
        Ok(serde_json::json!({"kind": "revoked_session", "session_id": row.get::<_, String>(0)?}))
    }));
    rows
}

async fn read_postgres_surface(pool: &PgPool) -> SurfaceState {
    let record_rows: Vec<(String, i64)> =
        sqlx::query_as("SELECT record_json, change_seq FROM lash_processes ORDER BY process_id")
            .fetch_all(pool)
            .await
            .unwrap();
    let records = record_rows.into_iter().map(|(record, change_seq)| normalized_json(serde_json::json!({"change_seq": change_seq, "record": serde_json::from_str::<serde_json::Value>(&record).unwrap()}))).collect();
    let event_rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT process_id, event_json FROM lash_process_events ORDER BY process_id, sequence",
    )
    .fetch_all(pool)
    .await
    .unwrap();
    let events = event_rows.into_iter().map(|(process_id, event)| normalized_json(serde_json::json!({"process_id": process_id, "event": serde_json::from_str::<serde_json::Value>(&event).unwrap()}))).collect();
    let observers = sqlx::query_as(
        "SELECT session_id, process_id FROM lash_process_observers ORDER BY session_id, process_id",
    )
    .fetch_all(pool)
    .await
    .unwrap();
    type PgLeaseRow = (
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        i64,
        i64,
        i64,
    );
    let lease_rows: Vec<PgLeaseRow> = sqlx::query_as("SELECT process_id, lease_owner_id, lease_owner_incarnation_id, lease_owner_liveness_json, lease_token, lease_fencing_token, lease_claimed_at_ms, lease_expires_at_ms FROM lash_process_leases ORDER BY process_id").fetch_all(pool).await.unwrap();
    let leases = lease_rows
        .into_iter()
        .map(
            |(process_id, owner_id, incarnation, liveness, token, fencing, claimed, expires)| {
                ProcessLeaseObservation {
                    process_id,
                    owner: if token.is_some() {
                        serde_json::to_value(decode_lease_owner(owner_id, incarnation, liveness))
                            .unwrap()
                    } else {
                        serde_json::Value::Null
                    },
                    lease_token_present: token.is_some(),
                    fencing_token: fencing as u64,
                    claimed: claimed != 0,
                    ttl_ms: (claimed != 0).then_some((expires - claimed) as u64),
                }
            },
        )
        .collect();
    type PgWakeRow = (
        String,
        String,
        String,
        Option<String>,
        i64,
        Option<i64>,
        i64,
        i64,
        Option<String>,
    );
    let wake_rows: Vec<PgWakeRow> = sqlx::query_as("SELECT delivery_id, delivery_json, state, claim_token, attempts, first_attempt_ms, next_attempt_at_ms, expires_at_ms, discard_reason FROM lash_process_wake_deliveries ORDER BY delivery_id").fetch_all(pool).await.unwrap();
    let wake_deliveries = wake_rows
        .into_iter()
        .map(
            |(
                delivery_id,
                json,
                state,
                token,
                attempts,
                first_attempt,
                next_attempt,
                expires,
                discard,
            )| {
                let wake: serde_json::Value = serde_json::from_str(&json).unwrap();
                let mut value = serde_json::json!({
                    "delivery_id": delivery_id,
                    "wake": wake,
                });
                let fields = value.as_object_mut().unwrap();
                fields.insert("state".to_string(), serde_json::json!(state));
                if let Some(token) = token {
                    fields.insert("claim_token".to_string(), serde_json::json!(token));
                } else {
                    fields.remove("claim_token");
                }
                fields.insert("attempts".to_string(), serde_json::json!(attempts));
                fields.insert(
                    "first_attempt_ms".to_string(),
                    serde_json::json!(first_attempt),
                );
                fields.insert(
                    "next_attempt_at_ms".to_string(),
                    serde_json::json!(next_attempt),
                );
                fields.insert("expires_at_ms".to_string(), serde_json::json!(expires));
                fields.insert("discard_reason".to_string(), serde_json::json!(discard));
                normalized_json(value)
            },
        )
        .collect();
    let allocation_rows: Vec<(String, String, i64)> = sqlx::query_as(
        "SELECT target_session_id, process_id, allocation_floor
         FROM lash_wake_allocation_floors ORDER BY target_session_id, process_id",
    )
    .fetch_all(pool)
    .await
    .unwrap();
    let wake_allocation_floors = allocation_rows
        .into_iter()
        .map(|(session, process, sequence)| (session, process, sequence as u64))
        .collect();
    let tombstone_rows: Vec<(String, String, i64, i64)> = sqlx::query_as("SELECT process_id, terminal_label, pruned_at_ms, pruned_change_seq FROM lash_process_tombstones ORDER BY process_id").fetch_all(pool).await.unwrap();
    let tombstones = tombstone_rows.into_iter().map(|(process_id, terminal_label, pruned_at_ms, pruned_change_seq)| normalized_json(serde_json::json!({"process_id": process_id, "terminal_label": terminal_label, "pruned_at_ms": pruned_at_ms, "pruned_change_seq": pruned_change_seq}))).collect();
    let fence_rows: Vec<(String, String, i64)> = sqlx::query_as("SELECT session_id, process_id, allocation_floor FROM lash_wake_redelivery_fences ORDER BY session_id, process_id").fetch_all(pool).await.unwrap();
    let wake_redelivery_fences = fence_rows
        .into_iter()
        .map(|(session, process, sequence)| (session, process, sequence as u64))
        .collect();
    SurfaceState {
        processes: ProcessRows {
            records,
            events,
            observers,
            leases,
            wake_deliveries,
            wake_allocation_floors,
            tombstones,
        },
        wake_redelivery_fences,
        triggers: read_postgres_triggers(pool).await,
        effect_journal: Some(read_postgres_effects(pool).await),
        await_journal: Some(read_postgres_await(pool).await),
    }
}

async fn read_postgres_triggers(pool: &PgPool) -> TriggerRows {
    let mut incarnations = BTreeMap::new();
    let subscriptions: Vec<String> = sqlx::query_scalar(
        "SELECT record_json FROM lash_trigger_subscriptions ORDER BY subscription_id",
    )
    .fetch_all(pool)
    .await
    .unwrap();
    let subscriptions = subscriptions
        .into_iter()
        .map(|row| normalized_trigger_json(serde_json::from_str(&row).unwrap(), &mut incarnations))
        .collect();
    let receipts: Vec<(String, String, String)> = sqlx::query_as("SELECT operation_id, request_fingerprint, result_json FROM lash_trigger_mutation_receipts ORDER BY operation_id").fetch_all(pool).await.unwrap();
    let mutation_receipts = receipts.into_iter().map(|(operation_id, request_fingerprint, result)| normalized_trigger_receipt_json(serde_json::json!({"operation_id": operation_id, "request_fingerprint": request_fingerprint, "result": serde_json::from_str::<serde_json::Value>(&result).unwrap()}), &mut incarnations)).collect();
    let occurrence_rows: Vec<String> = sqlx::query_scalar(
        "SELECT record_json FROM lash_trigger_occurrences ORDER BY occurrence_id",
    )
    .fetch_all(pool)
    .await
    .unwrap();
    let occurrences = occurrence_rows.into_iter().map(|record| normalized_trigger_json(serde_json::json!({"record": serde_json::from_str::<serde_json::Value>(&record).unwrap()}), &mut incarnations)).collect();
    let delivery_rows: Vec<(String, String, String, String)> = sqlx::query_as("SELECT occurrence_id, subscription_id, process_id, subscription_snapshot_json FROM lash_trigger_deliveries ORDER BY occurrence_id, subscription_id").fetch_all(pool).await.unwrap();
    let deliveries = delivery_rows.into_iter().map(|(occurrence_id, subscription_id, process_id, snapshot)| normalized_trigger_delivery_json(serde_json::json!({"occurrence_id": occurrence_id, "subscription_id": subscription_id, "process_id": process_id, "subscription_snapshot": serde_json::from_str::<serde_json::Value>(&snapshot).unwrap()}), &mut incarnations)).collect();
    TriggerRows {
        subscriptions,
        mutation_receipts,
        occurrences,
        deliveries,
    }
}

async fn read_postgres_effects(pool: &PgPool) -> Vec<serde_json::Value> {
    type Row = (
        String,
        Option<String>,
        String,
        String,
        String,
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        i64,
        Option<i64>,
    );
    let rows: Vec<Row> = sqlx::query_as("SELECT scope_id, session_id, replay_key, envelope_hash, envelope_json, status, outcome_json, error_json, lease_owner_id, lease_token, lease_expires_at_ms, due_at_ms FROM lash_runtime_effect_replay ORDER BY scope_id, replay_key").fetch_all(pool).await.unwrap();
    rows.into_iter().map(|(scope_id, session_id, replay_key, envelope_hash, envelope, status, outcome, error, owner, token, lease_expires, due)| normalized_json(serde_json::json!({"scope_id": scope_id, "session_id": session_id, "replay_key": replay_key, "envelope_hash": envelope_hash, "envelope": serde_json::from_str::<serde_json::Value>(&envelope).unwrap(), "status": status, "outcome": outcome.map(|v| serde_json::from_str::<serde_json::Value>(&v).unwrap()), "error": error.map(|v| serde_json::from_str::<serde_json::Value>(&v).unwrap()), "lease_owner_id": owner, "lease_token": token, "lease_expires_at_ms": lease_expires, "due_at_ms": due}))).collect()
}

async fn read_postgres_await(pool: &PgPool) -> Vec<serde_json::Value> {
    type Row = (
        String,
        String,
        String,
        Option<String>,
        bool,
        Option<String>,
        Option<i64>,
    );
    let waits: Vec<Row> = sqlx::query_as("SELECT key_id, scope_json, wait_json, session_id, turn_control, terminal_json, resolved_at_ms FROM lash_await_event_waits ORDER BY key_id").fetch_all(pool).await.unwrap();
    let mut rows = waits.into_iter().map(|(key_id, scope, wait, session_id, turn_control, terminal, resolved)| normalized_json(serde_json::json!({"kind": "wait", "key_id": key_id, "scope": serde_json::from_str::<serde_json::Value>(&scope).unwrap(), "wait": serde_json::from_str::<serde_json::Value>(&wait).unwrap(), "session_id": session_id, "turn_control": turn_control, "terminal": terminal.map(|v| serde_json::from_str::<serde_json::Value>(&v).unwrap()), "resolved_at_ms": resolved}))).collect::<Vec<_>>();
    let revoked: Vec<String> = sqlx::query_scalar(
        "SELECT session_id FROM lash_await_event_revoked_sessions ORDER BY session_id",
    )
    .fetch_all(pool)
    .await
    .unwrap();
    rows.extend(revoked.into_iter().map(
        |session_id| serde_json::json!({"kind": "revoked_session", "session_id": session_id}),
    ));
    rows
}

async fn reset_postgres_surface(storage: &PostgresStorage) {
    let tables: Vec<String> = sqlx::query_scalar("SELECT tablename FROM pg_tables WHERE schemaname = 'public' AND tablename LIKE 'lash\\_%' AND tablename NOT IN ('lash_schema_versions', 'lash_await_event_meta') ORDER BY tablename").fetch_all(storage.pool()).await.unwrap();
    sqlx::query(&format!(
        "TRUNCATE {} RESTART IDENTITY CASCADE",
        tables.join(", ")
    ))
    .execute(storage.pool())
    .await
    .unwrap();
    sqlx::query("INSERT INTO lash_process_change_clock (singleton, current_seq) VALUES (TRUE, 0) ON CONFLICT (singleton) DO UPDATE SET current_seq = 0").execute(storage.pool()).await.unwrap();
}

async fn surface_runners(
    root: &Path,
    storage: &PostgresStorage,
    clock: Arc<dyn Clock>,
) -> Vec<SurfaceRunner> {
    let memory_runtime = Arc::new(InMemorySessionStore::with_clock(Arc::clone(&clock)));
    let memory_registry =
        Arc::new(TestLocalProcessRegistry::default().with_clock(Arc::clone(&clock)));
    let memory_triggers = Arc::new(InMemoryTriggerStore::with_clock(Arc::clone(&clock)));
    let memory_effect = Arc::new(lash_core::facade_support::NativeEffectHost::default());

    let sqlite_runtime_path = root.join("runtime.db");
    let sqlite_process_path = root.join("process.db");
    let sqlite_trigger_path = root.join("trigger.db");
    let sqlite_effect_path = root.join("effect.db");
    let sqlite_runtime = Arc::new(SqliteStore::open(&sqlite_runtime_path).await.unwrap());
    let sqlite_registry = Arc::new(
        SqliteProcessRegistry::open_with_clock(
            &sqlite_process_path,
            Arc::clone(&clock),
            root.join("sessions"),
        )
        .await
        .unwrap(),
    );
    let sqlite_triggers = Arc::new(
        SqliteTriggerStore::open_with_clock(&sqlite_trigger_path, Arc::clone(&clock))
            .await
            .unwrap(),
    );
    let sqlite_effect = Arc::new(
        SqliteEffectHost::open_with_clock(&sqlite_effect_path, Arc::clone(&clock))
            .await
            .unwrap(),
    );

    let postgres_runtime = Arc::new(
        storage
            .session_store("prop-runtime-session")
            .with_clock(Arc::clone(&clock)),
    );
    let postgres_registry = Arc::new(storage.process_registry().with_clock(Arc::clone(&clock)));
    let postgres_triggers = Arc::new(storage.trigger_store());
    let postgres_effect = Arc::new(storage.effect_host());

    vec![
        SurfaceRunner {
            name: "in-memory",
            scenario: StoreContractScenario::new(StoreContractHandles {
                registry: memory_registry.clone(),
                runtime: memory_runtime.clone(),
            }),
            process_registry: memory_registry.clone(),
            trigger_store: memory_triggers.clone(),
            effect_host: memory_effect,
            reader: SurfaceReader::InMemory {
                runtime: memory_runtime,
                registry: memory_registry,
                triggers: memory_triggers,
            },
        },
        SurfaceRunner {
            name: "sqlite",
            scenario: StoreContractScenario::new(StoreContractHandles {
                registry: sqlite_registry.clone(),
                runtime: sqlite_runtime,
            }),
            process_registry: sqlite_registry,
            trigger_store: sqlite_triggers,
            effect_host: sqlite_effect,
            reader: SurfaceReader::Sqlite {
                runtime_path: sqlite_runtime_path,
                process_path: sqlite_process_path,
                trigger_path: sqlite_trigger_path,
                effect_path: sqlite_effect_path,
            },
        },
        SurfaceRunner {
            name: "postgres",
            scenario: StoreContractScenario::new(StoreContractHandles {
                registry: postgres_registry.clone(),
                runtime: postgres_runtime,
            }),
            process_registry: postgres_registry,
            trigger_store: postgres_triggers,
            effect_host: postgres_effect,
            reader: SurfaceReader::Postgres {
                pool: storage.pool().clone(),
            },
        },
    ]
}

fn states_agree(observations: &[(&str, SurfaceState)]) -> bool {
    let common = observations.windows(2).all(|pair| {
        pair[0].1.processes == pair[1].1.processes
            && pair[0].1.wake_redelivery_fences == pair[1].1.wake_redelivery_fences
            && pair[0].1.triggers == pair[1].1.triggers
    });
    let sqlite = observations
        .iter()
        .find(|(name, _)| *name == "sqlite")
        .unwrap()
        .1
        .clone();
    let postgres = observations
        .iter()
        .find(|(name, _)| *name == "postgres")
        .unwrap()
        .1
        .clone();
    common
        && sqlite.effect_journal == postgres.effect_journal
        && sqlite.await_journal == postgres.await_journal
}

fn operation_results_agree(results: &[(&str, Option<String>)]) -> bool {
    results.windows(2).all(|pair| pair[0].1 == pair[1].1)
}

#[derive(Debug)]
struct SurfaceDivergence {
    step: usize,
    operation: SurfaceOperation,
    operation_results: Vec<(&'static str, Option<String>)>,
    observations: Vec<(&'static str, SurfaceState)>,
}

async fn apply_and_observe(
    runners: &mut [SurfaceRunner],
    operation: &SurfaceOperation,
) -> (
    Vec<(&'static str, Option<String>)>,
    Vec<(&'static str, SurfaceState)>,
) {
    let mut operation_results = Vec::with_capacity(runners.len());
    for runner in runners.iter_mut() {
        operation_results.push((runner.name, runner.apply(operation).await.err()));
    }
    let mut observations = Vec::with_capacity(runners.len());
    for runner in runners {
        observations.push((runner.name, runner.observe().await));
    }
    (operation_results, observations)
}

fn counterexample_path(seed: u64) -> PathBuf {
    std::env::var_os("LASH_CROSS_BACKEND_COUNTEREXAMPLE_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("CARGO_TARGET_DIR").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("target"))
        .join("cross-backend-counterexamples")
        .join(format!("seed-{seed}.json"))
}

fn persist_counterexample(
    seed: u64,
    operations: &[SurfaceOperation],
    divergence: &SurfaceDivergence,
) -> PathBuf {
    let path = counterexample_path(seed);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(
        &path,
        serde_json::to_vec_pretty(&serde_json::json!({
            "seed": seed,
            "minimal_operations": operations,
            "first_diverging_step": divergence.step,
            "operation": divergence.operation,
            "operation_results": divergence.operation_results,
            "rows": divergence.observations,
        }))
        .unwrap(),
    )
    .unwrap();
    path
}

async fn first_divergence(
    storage: &PostgresStorage,
    operations: &[SurfaceOperation],
) -> Option<SurfaceDivergence> {
    reset_postgres_surface(storage).await;
    let root = tempfile::tempdir().unwrap();
    let clock = Arc::new(DifferentialClock) as Arc<dyn Clock>;
    let mut runners = surface_runners(root.path(), storage, clock).await;
    for (step, operation) in operations.iter().enumerate() {
        let (operation_results, observations) = apply_and_observe(&mut runners, operation).await;
        if !operation_results_agree(&operation_results) || !states_agree(&observations) {
            return Some(SurfaceDivergence {
                step: step + 1,
                operation: operation.clone(),
                operation_results,
                observations,
            });
        }
    }
    None
}

async fn minimize_diverging_prefix(
    storage: &PostgresStorage,
    operations: &[SurfaceOperation],
) -> Vec<SurfaceOperation> {
    let mut minimal = operations.to_vec();
    let mut index = 0;
    while index + 1 < minimal.len() {
        let mut candidate = minimal.clone();
        candidate.remove(index);
        if first_divergence(storage, &candidate).await.is_some() {
            minimal = candidate;
        } else {
            index += 1;
        }
    }
    minimal
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "compares three durable backends; requires Postgres (`just cross-backend-store-soak`, or LASH_POSTGRES_DATABASE_URL with --include-ignored)"]
async fn generated_cross_backend_surface_differential_agrees() {
    let database_url = match std::env::var("LASH_POSTGRES_DATABASE_URL") {
        Ok(value) if !value.is_empty() => value,
        _ if std::env::var("LASH_REQUIRE_POSTGRES").as_deref() == Ok("1") => {
            panic!("LASH_POSTGRES_DATABASE_URL must be set when LASH_REQUIRE_POSTGRES=1")
        }
        _ => {
            eprintln!(
                "SKIPPED generated cross-backend surface differential: PostgreSQL is not configured"
            );
            return;
        }
    };
    let mut database_lock = PgConnection::connect(&database_url).await.unwrap();
    sqlx::query("SELECT pg_advisory_lock($1)")
        .bind(SHARED_DATABASE_LOCK_KEY)
        .execute(&mut database_lock)
        .await
        .unwrap();
    let storage = PostgresStorage::connect(&database_url).await.unwrap();
    // CI seed 852 minimized to occurrence ingestion with no subscription state.
    if let Some(divergence) =
        first_divergence(&storage, &[SurfaceOperation::TriggerOccurrence { key: 0 }]).await
    {
        panic!("seed-852 minimized trigger-occurrence regression diverged: {divergence:#?}");
    }
    // PR #570 seed 852 at 9eef49f32 minimized to one session-owned registration.
    if let Some(divergence) =
        first_divergence(&storage, &[SurfaceOperation::TriggerRegister { key: 0 }]).await
    {
        panic!("seed-852 minimized trigger-register regression diverged: {divergence:#?}");
    }
    let canonical_conflict_material = [
        SurfaceOperation::TriggerOccurrenceNullSource { key: 0 },
        SurfaceOperation::TriggerOccurrenceNullSource { key: 0 },
        SurfaceOperation::StoreContract(StoreContractOp::Register {
            process: 0,
            disposition: 0,
            max_attempts: 1,
            wake_target: None,
        }),
        SurfaceOperation::ProcessSignalZero { negative: true },
        SurfaceOperation::ProcessSignalZero { negative: false },
    ];
    if let Some(divergence) = first_divergence(&storage, &canonical_conflict_material).await {
        panic!("canonical conflict-material differential diverged: {divergence:#?}");
    }
    let cases = std::env::var("LASH_CROSS_BACKEND_CASES")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(DEFAULT_CASES);
    let runner_seed = std::env::var("LASH_CROSS_BACKEND_SEED")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(DEFAULT_SEED);
    assert!(
        cases > 0,
        "LASH_CROSS_BACKEND_CASES must be greater than zero"
    );
    eprintln!(
        "cross-backend generated coverage is bounded: cases={cases} first_seed={runner_seed} \
         operations_per_case={OPS_PER_CASE}; omitted_seeds=all seeds outside the configured \
         contiguous range; effect_await_backends=sqlite,postgres (the in-memory effect host has \
         no durable journal)"
    );
    for case_index in 0..cases {
        reset_postgres_surface(&storage).await;
        let root = tempfile::tempdir().unwrap();
        let seed = runner_seed.wrapping_add(case_index as u64);
        let operations = generated_surface_operations(seed);
        let covered = operations
            .iter()
            .map(SurfaceOperation::kind)
            .collect::<BTreeSet<_>>();
        let omitted = ALL_SURFACE_OPERATION_KINDS
            .iter()
            .copied()
            .filter(|kind| !covered.contains(kind))
            .collect::<Vec<_>>();
        eprintln!(
            "cross-backend generated case seed={seed}: covered_operation_kinds={covered:?}; \
             omitted_operation_kinds={omitted:?}"
        );
        let clock = Arc::new(DifferentialClock) as Arc<dyn Clock>;
        let mut runners = surface_runners(root.path(), &storage, clock).await;
        for (step, operation) in operations.iter().enumerate() {
            let (operation_results, observations) =
                apply_and_observe(&mut runners, operation).await;
            if !operation_results_agree(&operation_results) || !states_agree(&observations) {
                let minimal = minimize_diverging_prefix(&storage, &operations[..=step]).await;
                let minimal_divergence = first_divergence(&storage, &minimal)
                    .await
                    .expect("minimized sequence must retain a divergence");
                let divergence = format!(
                    "seed={seed} step={} operation={:?} operation_results={:#?} rows={:#?}",
                    minimal_divergence.step,
                    minimal_divergence.operation,
                    minimal_divergence.operation_results,
                    minimal_divergence.observations,
                );
                let path = persist_counterexample(seed, &minimal, &minimal_divergence);
                panic!(
                    "cross-backend surface state diverged; prefix-minimized reproduction persisted to {}\n{divergence}",
                    path.display()
                );
            }
        }
    }
}

#[derive(Clone, Debug)]
enum BlobOperation {
    Put(Vec<u8>),
    DeleteFirst,
    DeleteAbsent,
}

#[tokio::test]
#[ignore = "compares file and S3 blob stores; requires MinIO (`just push-gate`, or LASH_MINIO_ENDPOINT with --include-ignored)"]
async fn attachment_blob_store_differential_agrees() {
    let endpoint = match std::env::var("LASH_MINIO_ENDPOINT") {
        Ok(value) if !value.is_empty() => value,
        _ if std::env::var("LASH_REQUIRE_MINIO").as_deref() == Ok("1") => {
            panic!("LASH_MINIO_ENDPOINT must be set when LASH_REQUIRE_MINIO=1")
        }
        _ => {
            eprintln!("SKIPPED attachment blob-store differential: MinIO is not configured");
            return;
        }
    };
    let memory = InMemoryAttachmentStore::new();
    let root = tempfile::tempdir().unwrap();
    let file = lash_core::facade_support::FileAttachmentStore::new(root.path());
    let s3 = S3AttachmentStore::from_config(S3AttachmentStoreConfig {
        endpoint_url: Some(endpoint),
        region: "us-east-1".to_string(),
        bucket: std::env::var("LASH_MINIO_BUCKET")
            .unwrap_or_else(|_| "lash-attachments".to_string()),
        prefix: Some(format!("cross-backend/{}", run_nonce())),
        access_key_id: Some("minioadmin".to_string()),
        secret_access_key: Some("minioadmin".to_string()),
        path_style: true,
    })
    .unwrap();
    let operations = [
        BlobOperation::Put(vec![1, 2, 3]),
        BlobOperation::Put(vec![9, 8]),
        BlobOperation::Put(vec![1, 2, 3]),
        BlobOperation::DeleteFirst,
        BlobOperation::DeleteAbsent,
    ];
    eprintln!(
        "attachment blob differential coverage is bounded: operations={operations:?}; \
         backends=in-memory,file,s3; omitted_operations=all other byte sequences and operation \
         sequences"
    );
    let mut first_id = None;
    for operation in &operations {
        match operation {
            BlobOperation::Put(bytes) => {
                let meta = || {
                    AttachmentCreateMeta::new(
                        MediaType::parse("application/octet-stream").unwrap(),
                        None,
                        Some("surface".to_string()),
                    )
                };
                let memory_ref = memory.put(bytes.clone(), meta()).await.unwrap();
                let file_ref = file.put(bytes.clone(), meta()).await.unwrap();
                let s3_ref = s3.put(bytes.clone(), meta()).await.unwrap();
                assert_eq!(memory_ref.id, file_ref.id);
                assert_eq!(file_ref.id, s3_ref.id);
                first_id.get_or_insert(memory_ref.id);
            }
            BlobOperation::DeleteFirst => {
                let id = first_id.as_ref().unwrap();
                memory.delete(id).await.unwrap();
                file.delete(id).await.unwrap();
                s3.delete(id).await.unwrap();
            }
            BlobOperation::DeleteAbsent => {
                let id = lash_core::AttachmentId::parse("absent").expect("valid attachment id");
                memory.delete(&id).await.unwrap();
                file.delete(&id).await.unwrap();
                s3.delete(&id).await.unwrap();
            }
        }
        let memory_rows = InMemoryAttachmentStore::raw_blobs_for_testing(&memory);
        let file_rows = raw_file_blobs(root.path());
        let s3_rows = s3.raw_blobs_for_testing().await.unwrap();
        assert_eq!(
            memory_rows, file_rows,
            "in-memory/file attachment blobs diverged after {operation:?}"
        );
        assert_eq!(
            file_rows, s3_rows,
            "file/S3 attachment blobs diverged after {operation:?}"
        );
    }
}

fn raw_file_blobs(root: &Path) -> Vec<(lash_core::AttachmentId, Vec<u8>)> {
    let mut rows = Vec::new();
    let content_root = root.join("blake3");
    if !content_root.exists() {
        return rows;
    }
    for prefix in fs::read_dir(content_root).unwrap() {
        for entry in fs::read_dir(prefix.unwrap().path()).unwrap() {
            let entry = entry.unwrap();
            let name = entry.file_name().to_string_lossy().to_string();
            if !name.contains(".staging.") {
                rows.push((
                    lash_core::AttachmentId::parse(name).expect("valid attachment id"),
                    fs::read(entry.path()).unwrap(),
                ));
            }
        }
    }
    rows.sort_by(|left, right| left.0.cmp(&right.0));
    rows
}
