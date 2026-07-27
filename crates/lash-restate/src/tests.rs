//! Tests for the Restate adapter (extracted from lib.rs).

use super::*;
use http_body_util::{BodyExt, Empty};
use lash_core::TestProcessRegistryWriteExt;
use lash_core::{ProcessInput, ProcessRegistration, RuntimeScope};
use lash_http_transport::{HttpResponse, HttpResponseBody, HttpTransport, HttpTransportError};
use lash_lashlang_runtime::{LashlangToolBinding, ToolDefinitionLashlangExt};
use restate_sdk::prelude::Endpoint;
use restate_sdk::service::Discoverable;
use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Mutex, RwLock};
use std::task::{Context, Poll, Waker};

mod endpoint_protocol;
mod process_tool_replay;
mod tool_context_conformance;
use endpoint_protocol::invoke_process_workflow_endpoint;

struct PanicsWhenPolledAfterReady {
    completed: bool,
}

impl Future for PanicsWhenPolledAfterReady {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
        assert!(!self.completed, "non-fused future was polled after ready");
        self.completed = true;
        Poll::Ready(())
    }
}

struct PanicsWhenPolledAfterErrorWake {
    polled: bool,
}

impl Future for PanicsWhenPolledAfterErrorWake {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        assert!(!self.polled, "error-signalling future was re-polled");
        self.polled = true;
        cx.waker().wake_by_ref();
        Poll::Pending
    }
}

#[test]
fn restate_context_futures_are_structurally_safe_after_ready() {
    let mut future = Box::pin(guard_restate_context_future(PanicsWhenPolledAfterReady {
        completed: false,
    }));
    let waker = Waker::noop();
    let mut context = Context::from_waker(waker);

    assert_eq!(future.as_mut().poll(&mut context), Poll::Ready(()));
    assert_eq!(future.as_mut().poll(&mut context), Poll::Pending);
}

#[tokio::test]
async fn restate_context_future_sdk_error_wake_fails_attempt_instead_of_parking() {
    let guarded = guard_restate_context_future(PanicsWhenPolledAfterErrorWake { polled: false });
    let mut attempt = tokio::spawn(async move {
        tokio::select! {
            _ = guarded => {}
            _ = std::future::pending::<()>() => {}
        }
    });

    let outcome = tokio::time::timeout(Duration::from_millis(100), &mut attempt).await;
    if outcome.is_err() {
        attempt.abort();
    }
    assert!(
        matches!(outcome, Ok(Err(ref error)) if error.is_panic()),
        "an SDK error wake must fail the handler attempt instead of parking it: {outcome:?}"
    );
}

#[test]
fn restate_session_cancel_sweep_excludes_turn_control_addresses() {
    let durable_wait = RestateDurableWaitAddress {
        workflow_key: "tool-wait".to_string(),
        session_id: Some("session".to_string()),
        classification: RestateDurableWaitClassification::DurableWait,
    };
    let cancel_gate = RestateDurableWaitAddress {
        workflow_key: "turn-cancel".to_string(),
        session_id: Some("session".to_string()),
        classification: RestateDurableWaitClassification::TurnControl,
    };
    let terminal = RestateDurableWaitAddress {
        workflow_key: "turn-terminal".to_string(),
        session_id: Some("session".to_string()),
        classification: RestateDurableWaitClassification::TurnControl,
    };

    let (cancelled, retained) = split_cancellable_waits(vec![
        durable_wait.clone(),
        cancel_gate.clone(),
        terminal.clone(),
    ]);
    assert_eq!(cancelled, vec![durable_wait]);
    assert_eq!(retained, vec![cancel_gate, terminal]);
}

#[test]
fn durable_wait_index_upgrade_migrates_legacy_aggregate_state() {
    let session_id = "upgrade-session";
    let durable_wait = RestateDurableWaitAddress {
        workflow_key: "durable-workflow".to_string(),
        session_id: Some(session_id.to_string()),
        classification: RestateDurableWaitClassification::DurableWait,
    };
    let turn_control = RestateDurableWaitAddress {
        workflow_key: "control-workflow".to_string(),
        session_id: Some(session_id.to_string()),
        classification: RestateDurableWaitClassification::TurnControl,
    };
    let awakeable = RestateDurableWaitAwakeableRequest {
        address: durable_wait.clone(),
        awakeable_id: "awakeable-1".to_string(),
    };

    // This byte payload is the value persisted under the old `waits` key.
    let old_layout_bytes = serde_json::to_vec(&RestateDurableWaitIndexState {
        revoked: true,
        waits: vec![durable_wait.clone(), turn_control.clone()],
        awakeables: vec![awakeable.clone()],
    })
    .expect("serialize old wait-index layout");
    let old_layout: RestateDurableWaitIndexState = serde_json::from_slice(&old_layout_bytes)
        .expect("read old wait-index layout after upgrade");

    let (metadata, migrated_waits) = migrate_legacy_durable_wait_index(old_layout);

    assert!(metadata.revoked);
    assert_eq!(metadata.awakeables.len(), 1);
    assert_eq!(metadata.awakeables[0].awakeable_id, awakeable.awakeable_id);
    assert_eq!(migrated_waits.len(), 2);
    for (state_key, expected) in migrated_waits {
        assert!(state_key.starts_with(DURABLE_WAIT_INDEX_WAIT_PREFIX));
        assert_eq!(
            durable_wait_address_from_state_key(session_id, &state_key),
            Some(expected)
        );
    }
    assert!(DURABLE_WAIT_INDEX_METADATA_KEY.starts_with("wait-index/v1/"));
}

fn wait_index_measurement_address(ordinal: usize) -> RestateDurableWaitAddress {
    RestateDurableWaitAddress {
        workflow_key: format!("{ordinal:064x}"),
        session_id: Some("restate-postgres-workers-e2e".to_string()),
        classification: RestateDurableWaitClassification::DurableWait,
    }
}

fn aggregate_wait_index_serialized_bytes(k: usize) -> usize {
    let waits = (0..k)
        .map(wait_index_measurement_address)
        .collect::<Vec<_>>();
    let mut bytes = 0;
    for registered in 1..=k {
        bytes += serde_json::to_vec(&RestateDurableWaitIndexState {
            revoked: false,
            waits: waits[..registered].to_vec(),
            awakeables: Vec::new(),
        })
        .expect("serialize aggregate wait-index register state")
        .len();
    }
    for remaining in (0..k).rev() {
        bytes += serde_json::to_vec(&RestateDurableWaitIndexState {
            revoked: false,
            waits: waits[..remaining].to_vec(),
            awakeables: Vec::new(),
        })
        .expect("serialize aggregate wait-index settle state")
        .len();
    }
    bytes
}

fn keyed_wait_index_serialized_bytes(k: usize) -> usize {
    let metadata_bytes = serde_json::to_vec(&RestateDurableWaitIndexMetadata::default())
        .expect("serialize keyed wait-index metadata")
        .len();
    metadata_bytes
        + (0..k)
            .map(wait_index_measurement_address)
            .map(|address| {
                serde_json::to_vec(&address)
                    .expect("serialize keyed wait-index entry")
                    .len()
            })
            .sum::<usize>()
}

#[test]
fn durable_wait_index_k_effect_measurements_are_linear() {
    for k in [4, 16] {
        // Each concurrent effect produces one register and one settle index
        // invocation in the workers-harness turn shape.
        let index_object_calls = 2 * k;
        let before_bytes = aggregate_wait_index_serialized_bytes(k);
        let after_bytes = keyed_wait_index_serialized_bytes(k);
        println!(
            "wait-index measurement K={k}: index_object_calls={index_object_calls}, before_serialized_state_bytes={before_bytes}, after_serialized_state_bytes={after_bytes}"
        );
        assert_eq!(index_object_calls, 2 * k);
        assert!(after_bytes < before_bytes);
    }
    assert_eq!(
        keyed_wait_index_serialized_bytes(16)
            - serde_json::to_vec(&RestateDurableWaitIndexMetadata::default())
                .expect("serialize metadata")
                .len(),
        4 * (keyed_wait_index_serialized_bytes(4)
            - serde_json::to_vec(&RestateDurableWaitIndexMetadata::default())
                .expect("serialize metadata")
                .len())
    );
}

#[test]
fn restate_effect_name_uses_lash_replay_key() {
    let invocation = RuntimeInvocation::effect(
        lash_core::runtime::RuntimeScope::for_turn("session", "turn", 1, 2),
        "effect",
        RuntimeEffectKind::ToolAttempt,
        "session:turn:1:2:tool_attempt:effect",
    );

    assert_eq!(
        restate_effect_name(&invocation),
        "lash:session:turn:1:2:tool_attempt:effect"
    );
}

#[tokio::test]
async fn restate_effect_host_satisfies_scope_factory_conformance() {
    lash_core::testing::conformance::effect_host(|| Arc::new(RestateEffectHost::new())).await;
}

#[tokio::test]
async fn restate_turn_work_driver_satisfies_shared_conformance() {
    let context = Arc::new(RecordingContext::default());
    let host: Arc<dyn EffectHost> = Arc::new(RestateRuntimeEffectController::new(context));
    lash_core::testing::conformance::turn_work_driver(host).await;
}

#[tokio::test]
async fn restate_handler_controller_satisfies_concurrent_replay_conformance() {
    let context = Arc::new(ReplayableRecordingContext::default());
    let controller = RestateRuntimeEffectController::new(Arc::clone(&context));
    let replay_context = Arc::clone(&context);

    lash_core::testing::conformance::effect_controller_concurrent_replay_deterministic(
        &controller,
        move || replay_context.start_replay(),
    )
    .await;

    let durable_context = Arc::new(ReplayableRecordingContext::default());
    let durable_controller = RestateRuntimeEffectController::new(Arc::clone(&durable_context));
    let durable_replay_context = Arc::clone(&durable_context);
    lash_core::testing::conformance::effect_controller_journaled_effect_replay(
        &durable_controller,
        move || durable_replay_context.start_replay(),
    )
    .await;

    let tool_context = Arc::new(ReplayableRecordingContext::default());
    let tool_controller = RestateRuntimeEffectController::new(Arc::clone(&tool_context));
    let tool_replay_context = Arc::clone(&tool_context);
    lash_core::testing::conformance::effect_controller_tool_attempt_fanout_replay_deterministic(
        &tool_controller,
        move || tool_replay_context.start_replay(),
    )
    .await;

    let runs = context.runs();
    assert_eq!(runs.len(), 4);
    assert!(runs.iter().any(|name| name.ends_with(":effect-slow")));
    assert!(runs.iter().any(|name| name.ends_with(":effect-fast")));

    let tool_runs = tool_context.runs();
    assert_eq!(tool_runs.len(), 4);
    assert!(
        tool_runs
            .iter()
            .any(|name| name.ends_with(":tool-attempt-slow"))
    );
    assert!(
        tool_runs
            .iter()
            .any(|name| name.ends_with(":tool-attempt-fast"))
    );
}

#[tokio::test]
async fn restate_handler_controller_journals_typed_trigger_execution() {
    let context = Arc::new(RecordingContext::default());
    let controller = RestateRuntimeEffectController::new(Arc::clone(&context));
    let envelope = RuntimeEffectEnvelope::new(
        RuntimeInvocation::effect(
            RuntimeScope::new("restate-trigger-session"),
            "restate-trigger-list",
            RuntimeEffectKind::Trigger,
            "restate-trigger-list",
        ),
        RuntimeEffectCommand::Trigger {
            command: Box::new(lash_core::TriggerCommand::List {
                owner_scope: lash_core::TriggerOwnerScope::session("restate-trigger-session"),
                filter: lash_core::TriggerSubscriptionFilter::default(),
            }),
        },
    );
    let store =
        Arc::new(lash_core::InMemoryTriggerStore::new()) as Arc<dyn lash_core::TriggerStore>;

    let outcome = controller
        .execute_effect(envelope, RuntimeEffectLocalExecutor::triggers(store))
        .await
        .expect("handler-scoped Restate controller must execute typed trigger effects")
        .into_trigger()
        .expect("typed trigger outcome");

    assert!(matches!(
        outcome,
        Ok(lash_core::TriggerCommandOutcome::List { records }) if records.is_empty()
    ));
    assert_eq!(
        context.runs.lock().expect("runs lock").as_slice(),
        ["lash:restate-trigger-list"]
    );
}

#[tokio::test]
async fn journaled_cancel_peeks_replay_while_live_watcher_observes_later_cancel() {
    let context = Arc::new(ReplayableRecordingContext::default());
    let controller = RestateRuntimeEffectController::new(Arc::clone(&context));
    let scope = ExecutionScope::turn("journaled-peek-session", "journaled-peek-turn");
    let key = controller
        .await_event_key(&scope, AwaitEventWaitIdentity::TurnCancelGate)
        .await
        .expect("cancel gate key");
    let envelope = |identity: &str| {
        RuntimeEffectEnvelope::new(
            RuntimeInvocation::effect(
                RuntimeScope {
                    session_id: "journaled-peek-session".to_string(),
                    turn_id: Some("journaled-peek-turn".to_string()),
                    turn_index: None,
                    protocol_iteration: None,
                },
                identity,
                RuntimeEffectKind::PeekAwaitEvent,
                identity,
            ),
            RuntimeEffectCommand::PeekAwaitEvent { key: key.clone() },
        )
    };
    let start = envelope("turn_cancel.start_gate");
    let post_abort = envelope("turn_cancel.post_abort_gate");
    assert_ne!(
        start.stable_hash().expect("start hash"),
        post_abort.stable_hash().expect("post-abort hash"),
        "later owner reads require a distinct causal identity"
    );

    let first = controller
        .execute_effect(start.clone(), RuntimeEffectLocalExecutor::unavailable())
        .await
        .expect("fresh start-gate peek")
        .into_peek_await_event()
        .expect("start-gate outcome");
    assert_eq!(first, None);

    let cancellation = Resolution::Ok(serde_json::json!({
        "status": "cancel_requested",
        "request_id": "after-start"
    }));
    assert_eq!(
        controller
            .resolve_await_event(&key, cancellation.clone())
            .await
            .expect("resolve cancel gate"),
        ResolveOutcome::Accepted
    );
    let later = controller
        .execute_effect(
            post_abort.clone(),
            RuntimeEffectLocalExecutor::unavailable(),
        )
        .await
        .expect("fresh post-abort peek")
        .into_peek_await_event()
        .expect("post-abort outcome");
    assert_eq!(later, Some(cancellation.clone()));

    context.start_replay();
    let replayed_start = controller
        .execute_effect(start, RuntimeEffectLocalExecutor::unavailable())
        .await
        .expect("replayed start-gate peek")
        .into_peek_await_event()
        .expect("replayed start-gate outcome");
    let replayed_later = controller
        .execute_effect(post_abort, RuntimeEffectLocalExecutor::unavailable())
        .await
        .expect("replayed post-abort peek")
        .into_peek_await_event()
        .expect("replayed post-abort outcome");
    assert_eq!(replayed_start, None);
    assert_eq!(replayed_later, Some(cancellation.clone()));

    let live = controller
        .await_await_event(&key, tokio_util::sync::CancellationToken::new(), None)
        .await
        .expect("live watcher observes durable cancellation");
    assert_eq!(live, cancellation);
}

#[test]
fn restate_handler_controller_disallows_concurrent_effect_calls() {
    let controller = RestateRuntimeEffectController::new(Arc::new(RecordingContext::default()));

    assert!(
        !controller.supports_concurrent_effects(),
        "Restate handler context calls such as ctx.run must be awaited before the next effect call"
    );
}

#[test]
fn recorded_runtime_effect_hash_mismatch_fails_explicitly() {
    let recorded_envelope = test_sleep_envelope(1)
        .canonical_form()
        .expect("recorded envelope");
    let reconstructed = test_sleep_envelope(2)
        .canonical_form()
        .expect("reconstructed envelope");
    let recorded = RecordedRuntimeEffect {
        envelope: Arc::new(recorded_envelope),
        outcome: Ok(RuntimeEffectOutcome::Sleep),
    };

    let err = validate_recorded_effect_envelope(recorded, &reconstructed, None)
        .expect_err("hash mismatch");

    assert_eq!(err.code, "restate_effect_hash_mismatch");
    assert_eq!(
        err.summary.expect("mismatch summary"),
        lash_core::RuntimeEffectReplayMismatchSummary {
            divergent_path_count: 1,
            first_divergent_paths: vec!["command.duration_ms".to_string()],
        }
    );
}

#[test]
fn recorded_runtime_effect_hash_match_returns_replayed_outcome() {
    let envelope = test_sleep_envelope(1)
        .canonical_form()
        .expect("canonical envelope");
    let recorded = RecordedRuntimeEffect {
        envelope: Arc::new(envelope.clone()),
        outcome: Ok(RuntimeEffectOutcome::Sleep),
    };

    let outcome = validate_recorded_effect_envelope(recorded, &envelope, None)
        .expect("hash match")
        .expect("replayed outcome");

    assert!(matches!(outcome, RuntimeEffectOutcome::Sleep));
}

fn test_sleep_envelope(duration_ms: u64) -> RuntimeEffectEnvelope {
    RuntimeEffectEnvelope::new(
        RuntimeInvocation::effect(
            lash_core::runtime::RuntimeScope::for_turn("session", "turn", 0, 0),
            "sleep:test",
            RuntimeEffectKind::Sleep,
            "sleep:test",
        ),
        RuntimeEffectCommand::Sleep { duration_ms },
    )
}

fn llm_spec() -> lash_core::LlmRequestSpec {
    lash_core::LlmRequestSpec {
        model: "model".to_string(),
        messages: Vec::new(),
        attachments: Vec::new(),
        tools: Arc::new(Vec::new()),
        tool_choice: Default::default(),
        model_variant: Default::default(),
        model_capability: lash_core::ModelCapability::default(),
        generation: lash_core::GenerationOptions::default(),
        scope: lash_core::LlmRequestScope::new(
            "session".to_string(),
            "session:frame:test".to_string(),
            "session:request:test".to_string(),
        ),
        output_spec: None,
    }
}

fn prepared_tool_call() -> lash_core::PreparedToolCall {
    lash_core::PreparedToolCall::from_parts(
        "call-1",
        "tool:tool",
        "tool",
        serde_json::json!({}),
        None,
        serde_json::Value::Null,
    )
}

fn legacy_durable_step_command() -> RuntimeEffectCommand {
    serde_json::from_value(serde_json::json!({
        "type": "durable_step",
        "step_id": "step",
        "input": { "x": 1 }
    }))
    .expect("legacy DurableStep command remains readable for one release")
}

fn prepared_tool_call_with(call_id: &str, tool_name: &str) -> lash_core::PreparedToolCall {
    lash_core::PreparedToolCall::from_parts(
        call_id,
        format!("tool:{tool_name}"),
        tool_name,
        serde_json::json!({ "call": call_id }),
        None,
        serde_json::Value::Null,
    )
}

fn completed_tool_record(call_id: &str, tool_name: &str) -> lash_core::ToolCallRecord {
    lash_core::ToolCallRecord {
        call_id: Some(call_id.to_string()),
        tool: tool_name.to_string(),
        args: serde_json::json!({ "call": call_id }),
        output: lash_core::ToolCallOutput::success(serde_json::json!({ "call": call_id })),
        duration_ms: 1,
    }
}

fn external_registration(id: &str) -> ProcessRegistration {
    ProcessRegistration::new(
        id,
        ProcessInput::External {
            metadata: serde_json::Value::Null,
        },
        lash_core::RecoveryDisposition::ExternallyOwned,
        lash_core::ProcessProvenance::host(),
    )
}

fn rerunnable_registration(id: &str) -> ProcessRegistration {
    ProcessRegistration::new(
        id,
        ProcessInput::External {
            metadata: serde_json::Value::Null,
        },
        lash_core::RecoveryDisposition::Rerunnable,
        lash_core::ProcessProvenance::host(),
    )
}

fn owner_bound_registration(id: &str) -> ProcessRegistration {
    ProcessRegistration::new(
        id,
        ProcessInput::External {
            metadata: serde_json::Value::Null,
        },
        lash_core::RecoveryDisposition::OwnerBound,
        lash_core::ProcessProvenance::host(),
    )
}

fn sync_await<T, F>(future: F) -> T
where
    T: Send + 'static,
    F: std::future::Future<Output = T> + Send + 'static,
{
    std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(future)
    })
    .join()
    .expect("runtime thread")
}

fn process_registry() -> Arc<dyn ProcessRegistry> {
    Arc::new(sync_await(async {
        lash_sqlite_store::SqliteProcessRegistry::memory()
            .await
            .expect("sqlite registry")
    }))
}

fn lashlang_process_input(input: lash_lashlang_runtime::LashlangProcessInput) -> ProcessInput {
    input
        .into_process_input()
        .expect("serialize lashlang process input")
}

#[derive(Default)]
struct DurableMemoryAttachmentStore {
    inner: lash_core::InMemoryAttachmentStore,
}

#[async_trait::async_trait]
impl lash_core::AttachmentStore for DurableMemoryAttachmentStore {
    fn persistence(&self) -> lash_core::AttachmentStorePersistence {
        lash_core::AttachmentStorePersistence::Durable
    }

    async fn put(
        &self,
        bytes: Vec<u8>,
        meta: lash_core::AttachmentCreateMeta,
    ) -> Result<lash_core::AttachmentRef, lash_core::AttachmentStoreError> {
        self.inner.put(bytes, meta).await
    }

    async fn get(
        &self,
        id: &lash_core::AttachmentId,
    ) -> Result<lash_core::StoredAttachment, lash_core::AttachmentStoreError> {
        self.inner.get(id).await
    }

    async fn delete(
        &self,
        id: &lash_core::AttachmentId,
    ) -> Result<(), lash_core::AttachmentStoreError> {
        self.inner.delete(id).await
    }

    async fn list(&self) -> Result<Vec<lash_core::StoredBlobRef>, lash_core::AttachmentStoreError> {
        self.inner.list().await
    }
}

#[derive(Default)]
struct DurableMemoryProcessEnvStore {
    inner: lash_core::InMemoryProcessExecutionEnvStore,
}

#[async_trait::async_trait]
impl lash_core::ProcessExecutionEnvStore for DurableMemoryProcessEnvStore {
    fn durability_tier(&self) -> lash_core::DurabilityTier {
        lash_core::DurabilityTier::Durable
    }

    async fn put_process_execution_env(
        &self,
        env_ref: &lash_core::ProcessExecutionEnvRef,
        bytes: &[u8],
    ) -> Result<(), lash_core::PluginError> {
        self.inner.put_process_execution_env(env_ref, bytes).await
    }

    async fn get_process_execution_env(
        &self,
        env_ref: &lash_core::ProcessExecutionEnvRef,
    ) -> Result<Option<Vec<u8>>, lash_core::PluginError> {
        self.inner.get_process_execution_env(env_ref).await
    }
}

static RECOVERY_PROCESS_ENV_STORE: LazyLock<Arc<DurableMemoryProcessEnvStore>> =
    LazyLock::new(|| Arc::new(DurableMemoryProcessEnvStore::default()));

struct CommitRetryStore {
    inner: Arc<dyn lash_core::RuntimePersistence>,
    lease_claim_count: Arc<AtomicUsize>,
}

impl CommitRetryStore {
    fn new(inner: Arc<dyn lash_core::RuntimePersistence>) -> Self {
        Self {
            inner,
            lease_claim_count: Arc::new(AtomicUsize::new(0)),
        }
    }
}

lash_core::impl_noop_attachment_manifest!(CommitRetryStore);

// Pass-through wrapper over the shared in-memory recovery store; every
// segment delegates to `inner`.
#[async_trait::async_trait]
impl lash_core::SessionCommitStore for CommitRetryStore {
    fn durability_tier(&self) -> lash_core::DurabilityTier {
        lash_core::DurabilityTier::Durable
    }

    async fn load_session(
        &self,
        _scope: lash_core::SessionReadScope,
    ) -> Result<Option<lash_core::store::PersistedSessionRead>, lash_core::StoreError> {
        Ok(None)
    }

    async fn load_node(
        &self,
        node_id: &str,
    ) -> Result<Option<lash_core::SessionNodeRecord>, lash_core::StoreError> {
        self.inner.load_node(node_id).await
    }

    async fn commit_runtime_state(
        &self,
        commit: lash_core::store::RuntimeCommit,
    ) -> Result<lash_core::store::RuntimeCommitResult, lash_core::StoreError> {
        self.inner.commit_runtime_state(commit).await
    }

    async fn save_session_meta(
        &self,
        meta: lash_core::SessionMeta,
    ) -> Result<(), lash_core::StoreError> {
        self.inner.save_session_meta(meta).await
    }

    async fn load_session_meta(
        &self,
    ) -> Result<Option<lash_core::SessionMeta>, lash_core::StoreError> {
        self.inner.load_session_meta().await
    }
}

#[async_trait::async_trait]
impl lash_core::SessionExecutionLeaseStore for CommitRetryStore {
    async fn try_claim_session_execution_lease(
        &self,
        session_id: &str,
        owner: &lash_core::LeaseOwnerIdentity,
        lease_ttl_ms: u64,
    ) -> Result<lash_core::SessionExecutionLeaseClaimOutcome, lash_core::StoreError> {
        self.lease_claim_count.fetch_add(1, Ordering::SeqCst);
        self.inner
            .try_claim_session_execution_lease(session_id, owner, lease_ttl_ms)
            .await
    }

    async fn reclaim_session_execution_lease(
        &self,
        session_id: &str,
        owner: &lash_core::LeaseOwnerIdentity,
        observed_holder: &lash_core::SessionExecutionLeaseFence,
        lease_ttl_ms: u64,
    ) -> Result<lash_core::SessionExecutionLeaseClaimOutcome, lash_core::StoreError> {
        self.inner
            .reclaim_session_execution_lease(session_id, owner, observed_holder, lease_ttl_ms)
            .await
    }

    async fn renew_session_execution_lease(
        &self,
        fence: &lash_core::SessionExecutionLeaseFence,
        lease_ttl_ms: u64,
    ) -> Result<lash_core::SessionExecutionLease, lash_core::StoreError> {
        self.inner
            .renew_session_execution_lease(fence, lease_ttl_ms)
            .await
    }

    async fn release_session_execution_lease(
        &self,
        completion: &lash_core::SessionExecutionLeaseCompletion,
    ) -> Result<(), lash_core::StoreError> {
        self.inner.release_session_execution_lease(completion).await
    }
}

#[async_trait::async_trait]
impl lash_core::QueuedWorkStore for CommitRetryStore {
    async fn enqueue_queued_work(
        &self,
        batch: lash_core::runtime::QueuedWorkBatchDraft,
    ) -> Result<lash_core::runtime::QueuedWorkBatch, lash_core::StoreError> {
        self.inner.enqueue_queued_work(batch).await
    }

    async fn claim_leading_ready_session_command(
        &self,
        session_id: &str,
        session_execution_lease: &lash_core::SessionExecutionLeaseFence,
        owner: &lash_core::LeaseOwnerIdentity,
    ) -> Result<Option<lash_core::runtime::QueuedWorkClaim>, lash_core::StoreError> {
        self.inner
            .claim_leading_ready_session_command(session_id, session_execution_lease, owner)
            .await
    }

    async fn claim_ready_queued_work(
        &self,
        session_id: &str,
        session_execution_lease: &lash_core::SessionExecutionLeaseFence,
        owner: &lash_core::LeaseOwnerIdentity,
        boundary: lash_core::runtime::QueuedWorkClaimBoundary,
        max_batches: usize,
    ) -> Result<Option<lash_core::runtime::QueuedWorkClaim>, lash_core::StoreError> {
        self.inner
            .claim_ready_queued_work(
                session_id,
                session_execution_lease,
                owner,
                boundary,
                max_batches,
            )
            .await
    }

    async fn claim_checkpoint_work(
        &self,
        session_id: &str,
        session_execution_lease: &lash_core::SessionExecutionLeaseFence,
        owner: &lash_core::LeaseOwnerIdentity,
        turn_id: &str,
        checkpoint: lash_core::CheckpointKind,
        max_inputs: usize,
        max_batches: usize,
    ) -> Result<
        (
            Option<lash_core::runtime::TurnInputClaim>,
            Option<lash_core::runtime::QueuedWorkClaim>,
        ),
        lash_core::StoreError,
    > {
        self.inner
            .claim_checkpoint_work(
                session_id,
                session_execution_lease,
                owner,
                turn_id,
                checkpoint,
                max_inputs,
                max_batches,
            )
            .await
    }

    async fn claim_ready_queued_work_by_batch_ids(
        &self,
        session_id: &str,
        session_execution_lease: &lash_core::SessionExecutionLeaseFence,
        owner: &lash_core::LeaseOwnerIdentity,
        boundary: lash_core::runtime::QueuedWorkClaimBoundary,
        batch_ids: &[String],
    ) -> Result<Option<lash_core::runtime::QueuedWorkClaim>, lash_core::StoreError> {
        self.inner
            .claim_ready_queued_work_by_batch_ids(
                session_id,
                session_execution_lease,
                owner,
                boundary,
                batch_ids,
            )
            .await
    }

    async fn abandon_queued_work_claim(
        &self,
        claim: &lash_core::runtime::QueuedWorkClaim,
    ) -> Result<(), lash_core::StoreError> {
        self.inner.abandon_queued_work_claim(claim).await
    }

    async fn cancel_queued_work_batch(
        &self,
        session_id: &str,
        batch_id: &str,
    ) -> Result<Option<lash_core::runtime::QueuedWorkBatch>, lash_core::StoreError> {
        self.inner
            .cancel_queued_work_batch(session_id, batch_id)
            .await
    }

    async fn list_queued_work(
        &self,
        session_id: &str,
    ) -> Result<Vec<lash_core::runtime::QueuedWorkBatch>, lash_core::StoreError> {
        self.inner.list_queued_work(session_id).await
    }

    async fn list_pending_queued_work(
        &self,
        session_id: &str,
    ) -> Result<Vec<lash_core::runtime::QueuedWorkBatch>, lash_core::StoreError> {
        self.inner.list_pending_queued_work(session_id).await
    }
}

#[async_trait::async_trait]
impl lash_core::TurnInputStore for CommitRetryStore {
    async fn enqueue_pending_turn_input(
        &self,
        input: lash_core::PendingTurnInputDraft,
    ) -> Result<lash_core::PendingTurnInput, lash_core::StoreError> {
        self.inner.enqueue_pending_turn_input(input).await
    }

    async fn list_pending_turn_inputs(
        &self,
        session_id: &str,
    ) -> Result<Vec<lash_core::PendingTurnInput>, lash_core::StoreError> {
        self.inner.list_pending_turn_inputs(session_id).await
    }

    async fn cancel_pending_turn_inputs(
        &self,
        session_id: &str,
        targets: &[lash_core::PendingTurnInputCancelTarget],
    ) -> Result<Vec<lash_core::PendingTurnInputCancelResult>, lash_core::StoreError> {
        self.inner
            .cancel_pending_turn_inputs(session_id, targets)
            .await
    }

    async fn cancel_pending_turn_input_suffix(
        &self,
        session_id: &str,
        anchor: &lash_core::PendingTurnInputCancelTarget,
    ) -> Result<lash_core::PendingTurnInputSuffixCancelOutcome, lash_core::StoreError> {
        self.inner
            .cancel_pending_turn_input_suffix(session_id, anchor)
            .await
    }

    async fn claim_active_turn_inputs(
        &self,
        session_id: &str,
        session_execution_lease: &lash_core::SessionExecutionLeaseFence,
        owner: &lash_core::LeaseOwnerIdentity,
        turn_id: &str,
        checkpoint: lash_core::CheckpointKind,
        max_inputs: usize,
    ) -> Result<Option<lash_core::runtime::TurnInputClaim>, lash_core::StoreError> {
        self.inner
            .claim_active_turn_inputs(
                session_id,
                session_execution_lease,
                owner,
                turn_id,
                checkpoint,
                max_inputs,
            )
            .await
    }

    async fn claim_next_turn_inputs(
        &self,
        session_id: &str,
        session_execution_lease: &lash_core::SessionExecutionLeaseFence,
        owner: &lash_core::LeaseOwnerIdentity,
        max_inputs: usize,
    ) -> Result<Option<lash_core::runtime::TurnInputClaim>, lash_core::StoreError> {
        self.inner
            .claim_next_turn_inputs(session_id, session_execution_lease, owner, max_inputs)
            .await
    }

    async fn abandon_turn_input_claim(
        &self,
        claim: &lash_core::runtime::TurnInputClaim,
    ) -> Result<(), lash_core::StoreError> {
        self.inner.abandon_turn_input_claim(claim).await
    }
}

#[async_trait::async_trait]
impl lash_core::StoreMaintenance for CommitRetryStore {
    async fn tombstone_nodes(&self, ids: &[String]) -> Result<(), lash_core::StoreError> {
        self.inner.tombstone_nodes(ids).await
    }

    async fn vacuum(&self) -> Result<lash_core::VacuumReport, lash_core::StoreError> {
        self.inner.vacuum().await
    }

    async fn gc_unreachable(&self) -> Result<lash_core::GcReport, lash_core::StoreError> {
        self.inner.gc_unreachable().await
    }
}

#[test]
fn restate_command_execution_plan_is_explicit_for_every_command() {
    let cases = vec![
        (
            RuntimeEffectCommand::Sleep { duration_ms: 1 },
            RestateEffectExecution::Timer,
        ),
        (
            RuntimeEffectCommand::process(ProcessCommand::List {
                session_scope: lash_core::SessionScope::new("session"),
                mode: lash_core::ProcessListMode::Live,
            }),
            RestateEffectExecution::DirectProcess,
        ),
        (
            RuntimeEffectCommand::AwaitEvent {
                key: restate_await_event_key(
                    &ExecutionScope::turn("session", "turn"),
                    AwaitEventWaitIdentity::Custom {
                        key: "event".to_string(),
                    },
                )
                .expect("await-event key"),
            },
            RestateEffectExecution::AwaitEvent,
        ),
        (
            RuntimeEffectCommand::LlmCall {
                request: Box::new(llm_spec()),
            },
            RestateEffectExecution::JournaledRun,
        ),
        (
            RuntimeEffectCommand::Direct {
                request: Box::new(llm_spec()),
                usage_source: "test".to_string(),
            },
            RestateEffectExecution::JournaledRun,
        ),
        (
            RuntimeEffectCommand::ToolAttempt {
                call: prepared_tool_call(),
                execution_grant: None,
                attempt: 1,
                max_attempts: 1,
            },
            RestateEffectExecution::JournaledRun,
        ),
        (
            RuntimeEffectCommand::ToolBatch {
                batch: lash_core::PreparedToolBatch::new("batch", vec![prepared_tool_call()]),
            },
            RestateEffectExecution::DirectLocal,
        ),
        (
            RuntimeEffectCommand::ExecCode {
                language: "code".to_string(),
                code: "1 + 1".to_string(),
            },
            // The interpreter is composite: it can issue nested timers,
            // waits, tools, and model calls. Rebuild it on handler replay and
            // let those child effects use their own stable journal keys.
            RestateEffectExecution::DirectLocal,
        ),
        (
            RuntimeEffectCommand::Checkpoint {
                checkpoint: lash_core::CheckpointKind::AfterWork,
            },
            RestateEffectExecution::JournaledRun,
        ),
        (
            RuntimeEffectCommand::SyncExecutionEnvironment {
                update_machine_config: true,
            },
            RestateEffectExecution::JournaledRun,
        ),
        (
            legacy_durable_step_command(),
            RestateEffectExecution::JournaledRun,
        ),
    ];

    for (command, expected) in cases {
        assert_eq!(restate_effect_execution(&command), expected);
    }
}

#[derive(Default)]
struct RecordingContext {
    endpoint: Option<Endpoint>,
    block_sleeps: AtomicBool,
    sleeps: Mutex<Vec<u64>>,
    runs: Mutex<Vec<String>>,
    started: Mutex<Vec<ProcessRegistration>>,
    started_execution_contexts: Mutex<Vec<ProcessExecutionContext>>,
    process_command_log: Mutex<Vec<String>>,
    cancelled: Mutex<Vec<(String, Option<String>)>>,
    resolved_events: Mutex<Vec<RestateDurableWaitResolveRequest>>,
    awaited_events: Mutex<HashMap<String, Resolution>>,
    durable_events: Mutex<HashMap<String, Resolution>>,
    durable_event_notifies: Mutex<HashMap<String, Arc<tokio::sync::Notify>>>,
    session_waits: Mutex<HashMap<String, Vec<RestateDurableWaitAddress>>>,
    revoked_sessions: Mutex<HashSet<String>>,
}

impl RecordingContext {
    fn with_endpoint(endpoint: Endpoint) -> Self {
        Self {
            endpoint: Some(endpoint),
            ..Default::default()
        }
    }

    fn resolve_process_terminal(&self, process_id: &str, output: &ProcessAwaitOutput) {
        let key = restate_process_terminal_await_key(process_id).expect("terminal await key");
        let resolution =
            restate_process_terminal_resolution(output).expect("terminal await resolution");
        self.awaited_events
            .lock()
            .expect("awaited events lock")
            .insert(key.promise_key(), resolution);
    }

    fn durable_event_notify(&self, workflow_key: &str) -> Arc<tokio::sync::Notify> {
        self.durable_event_notifies
            .lock()
            .expect("durable event notifies lock")
            .entry(workflow_key.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Notify::new()))
            .clone()
    }

    fn reset_invocation_state_for_replay_preserving_durable_event(&self, workflow_key: &str) {
        // Restate replays invocation-local awakeables in journal order. This
        // in-memory context must discard the prior pass's turn-control and
        // process-terminal resolutions while retaining external input.
        let preserved = self
            .durable_events
            .lock()
            .expect("durable events lock")
            .get(workflow_key)
            .cloned()
            .expect("durable event to preserve during replay");
        let mut events = self.durable_events.lock().expect("durable events lock");
        events.clear();
        events.insert(workflow_key.to_string(), preserved);
        drop(events);
        self.awaited_events
            .lock()
            .expect("awaited events lock")
            .clear();
    }

    fn resolve_durable_event(&self, request: RestateDurableWaitResolveRequest) -> ResolveOutcome {
        if request
            .address
            .session_id
            .as_deref()
            .is_some_and(|session_id| {
                self.revoked_sessions
                    .lock()
                    .expect("revoked sessions lock")
                    .contains(session_id)
            })
        {
            return ResolveOutcome::UnknownOrRevoked;
        }
        self.terminalize_durable_event(request)
    }

    fn terminalize_durable_event(
        &self,
        request: RestateDurableWaitResolveRequest,
    ) -> ResolveOutcome {
        self.resolved_events
            .lock()
            .expect("resolved events lock")
            .push(request.clone());
        let mut events = self.durable_events.lock().expect("durable events lock");
        if let Some(terminal) = events.get(&request.address.workflow_key) {
            return ResolveOutcome::AlreadyResolved {
                terminal: terminal.clone(),
            };
        }
        events.insert(request.address.workflow_key.clone(), request.resolution);
        drop(events);
        self.durable_event_notify(&request.address.workflow_key)
            .notify_waiters();
        ResolveOutcome::Accepted
    }

    fn settle_session_wait(&self, address: &RestateDurableWaitAddress) {
        if address.classification == RestateDurableWaitClassification::TurnControl {
            return;
        }
        let Some(session_id) = address.session_id.as_deref() else {
            return;
        };
        if let Some(waits) = self
            .session_waits
            .lock()
            .expect("session waits lock")
            .get_mut(session_id)
        {
            waits.retain(|wait| wait != address);
        }
    }
}

impl<'ctx> RestateControllerContext<'ctx> for Arc<RecordingContext> {
    fn sleep_send<'run>(
        &'run self,
        duration: Duration,
    ) -> Pin<Box<dyn Future<Output = Result<(), TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
    {
        self.sleeps
            .lock()
            .expect("sleeps lock")
            .push(duration.as_millis() as u64);
        let block = self.block_sleeps.load(Ordering::SeqCst);
        Box::pin(async move {
            if block {
                std::future::pending::<()>().await;
            }
            Ok(())
        })
    }

    fn run_json_send<'run, T, Fut>(
        &'run self,
        _effect_name: String,
        _retry_policy: Option<RunRetryPolicy>,
        future: Fut,
    ) -> Pin<Box<dyn Future<Output = Result<Json<T>, TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
        T: Serialize + DeserializeOwned + Send + 'static,
        Fut: Future<Output = T> + Send + 'run,
    {
        self.runs.lock().expect("runs lock").push(_effect_name);
        Box::pin(async move { Ok(Json(future.await)) })
    }

    fn start_process_workflow<'run>(
        &'run self,
        registration: ProcessRegistration,
        execution_context: ProcessExecutionContext,
    ) -> Pin<Box<dyn Future<Output = Result<String, TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
    {
        let process_id = registration.id.clone();
        let endpoint = self.endpoint.clone();
        self.process_command_log
            .lock()
            .expect("process command log lock")
            .push(format!("send:{process_id}"));
        self.started
            .lock()
            .expect("started lock")
            .push(registration.clone());
        self.started_execution_contexts
            .lock()
            .expect("started execution contexts lock")
            .push(execution_context.clone());
        Box::pin(async move {
            if let Some(endpoint) = endpoint {
                let complete_runs =
                    matches!(registration.input.as_ref(), ProcessInput::ToolCall { .. });
                invoke_process_workflow_endpoint(
                    &endpoint,
                    "run",
                    &process_id,
                    &RestateProcessWorkflowInput {
                        registration,
                        execution_context,
                        segment_ordinal: 0,
                        execution_id: None,
                    },
                    complete_runs,
                )
                .await?;
            }
            Ok(format!("invocation-{process_id}"))
        })
    }

    fn request_process_workflow_cancel<'run>(
        &'run self,
        request: RestateProcessCancelRequest,
    ) -> Pin<Box<dyn Future<Output = Result<(), TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
    {
        let endpoint = self.endpoint.clone();
        let process_id = request.process_id.clone();
        self.cancelled
            .lock()
            .expect("cancelled lock")
            .push((request.process_id.clone(), request.reason.clone()));
        Box::pin(async move {
            if let Some(endpoint) = endpoint {
                invoke_process_workflow_endpoint(&endpoint, "cancel", &process_id, &request, false)
                    .await?;
            }
            Ok(())
        })
    }

    fn await_event<'run>(
        &'run self,
        request: RestateDurableWaitAwaitRequest,
        cancellation: tokio_util::sync::CancellationToken,
    ) -> Pin<Box<dyn Future<Output = Result<Resolution, TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
    {
        let context = Arc::clone(self);
        Box::pin(async move {
            if let Some(session_id) = request.address.session_id.as_deref() {
                if context
                    .revoked_sessions
                    .lock()
                    .expect("revoked sessions lock")
                    .contains(session_id)
                {
                    context.terminalize_durable_event(RestateDurableWaitResolveRequest {
                        address: request.address,
                        resolution: Resolution::Cancelled,
                    });
                    return Ok(Resolution::Cancelled);
                }
                context
                    .session_waits
                    .lock()
                    .expect("session waits lock")
                    .entry(session_id.to_string())
                    .or_default()
                    .push(request.address.clone());
            }
            let notify = context.durable_event_notify(&request.address.workflow_key);
            loop {
                if let Some(resolution) = context
                    .durable_events
                    .lock()
                    .expect("durable events lock")
                    .get(&request.address.workflow_key)
                    .cloned()
                {
                    context.settle_session_wait(&request.address);
                    return Ok(resolution);
                }
                if let Some(timeout_ms) = request.timeout_ms {
                    tokio::select! {
                        _ = notify.notified() => {}
                        _ = cancellation.cancelled() => {
                            context.resolve_durable_event(RestateDurableWaitResolveRequest {
                                address: request.address.clone(),
                                resolution: Resolution::Cancelled,
                            });
                        }
                        _ = tokio::time::sleep(Duration::from_millis(timeout_ms)) => {
                            context.resolve_durable_event(RestateDurableWaitResolveRequest {
                                address: request.address.clone(),
                                resolution: Resolution::Timeout,
                            });
                        }
                    }
                } else {
                    tokio::select! {
                        _ = notify.notified() => {}
                        _ = cancellation.cancelled() => {
                            context.resolve_durable_event(RestateDurableWaitResolveRequest {
                                address: request.address.clone(),
                                resolution: Resolution::Cancelled,
                            });
                        }
                    }
                }
            }
        })
    }

    fn peek_event<'run>(
        &'run self,
        _address: RestateDurableWaitAddress,
    ) -> Pin<Box<dyn Future<Output = Result<Option<Resolution>, TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
    {
        Box::pin(async { Ok(None) })
    }

    fn await_process_terminal<'run>(
        &'run self,
        process_id: String,
    ) -> Pin<Box<dyn Future<Output = Result<ProcessAwaitOutput, TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
    {
        self.process_command_log
            .lock()
            .expect("process command log lock")
            .push(format!("call:{process_id}"));
        let result = restate_process_terminal_await_key(&process_id)
            .map_err(TerminalError::from_error)
            .and_then(|key| {
                self.awaited_events
                    .lock()
                    .expect("awaited events lock")
                    .get(&key.promise_key())
                    .cloned()
                    .ok_or_else(|| {
                        TerminalError::new(format!(
                            "process terminal await is unresolved: {process_id}"
                        ))
                    })
            })
            .and_then(|resolution| {
                restate_process_terminal_output(&process_id, resolution)
                    .map_err(TerminalError::from_error)
            });
        Box::pin(async move { result })
    }

    fn resolve_event<'run>(
        &'run self,
        request: RestateDurableWaitResolveRequest,
    ) -> Pin<Box<dyn Future<Output = Result<ResolveOutcome, TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
    {
        let outcome = self.resolve_durable_event(request);
        Box::pin(async move { Ok(outcome) })
    }

    fn update_session_waits<'run>(
        &'run self,
        session_id: String,
        revoke: bool,
    ) -> Pin<Box<dyn Future<Output = Result<(), TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
    {
        if revoke {
            self.revoked_sessions
                .lock()
                .expect("revoked sessions lock")
                .insert(session_id.clone());
        }
        let waits = self
            .session_waits
            .lock()
            .expect("session waits lock")
            .remove(&session_id)
            .unwrap_or_default();
        let (resolve, retain): (Vec<_>, Vec<_>) = if revoke {
            (waits, Vec::new())
        } else {
            split_cancellable_waits(waits)
        };
        if !retain.is_empty() {
            self.session_waits
                .lock()
                .expect("session waits lock")
                .insert(session_id, retain);
        }
        for address in resolve {
            self.terminalize_durable_event(RestateDurableWaitResolveRequest {
                address,
                resolution: Resolution::Cancelled,
            });
        }
        Box::pin(async { Ok(()) })
    }

    fn session_is_revoked<'run>(
        &'run self,
        session_id: String,
    ) -> Pin<Box<dyn Future<Output = Result<bool, TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
    {
        let revoked = self
            .revoked_sessions
            .lock()
            .expect("revoked sessions lock")
            .contains(&session_id);
        Box::pin(async move { Ok(revoked) })
    }
}

#[derive(Default)]
struct ReplayableRecordingContext {
    sleeps: Mutex<Vec<u64>>,
    runs: Mutex<Vec<String>>,
    records: Mutex<HashMap<String, Vec<u8>>>,
    replaying: AtomicBool,
    peek_records: Mutex<Vec<Option<Resolution>>>,
    peek_cursor: AtomicUsize,
    events: Arc<RecordingContext>,
    process_worker: Mutex<Option<DurableProcessWorker>>,
}

impl ReplayableRecordingContext {
    fn start_replay(&self) {
        self.replaying.store(true, Ordering::SeqCst);
        self.peek_cursor.store(0, Ordering::SeqCst);
    }

    fn runs(&self) -> Vec<String> {
        self.runs.lock().expect("runs lock").clone()
    }

    fn recorded_runtime_effect_envelopes(&self) -> Vec<(String, RuntimeEffectEnvelope)> {
        let mut envelopes = self
            .records
            .lock()
            .expect("records lock")
            .iter()
            .map(|(effect_name, bytes)| {
                let recorded: RecordedRuntimeEffect =
                    serde_json::from_slice(bytes).expect("recorded runtime effect");
                let canonical =
                    serde_json::to_value(recorded.envelope).expect("canonical envelope value");
                let json = canonical
                    .get("json")
                    .and_then(serde_json::Value::as_str)
                    .expect("canonical envelope json");
                let envelope =
                    serde_json::from_str(json).expect("canonical runtime effect envelope");
                (effect_name.clone(), envelope)
            })
            .collect::<Vec<_>>();
        envelopes.sort_by(|left, right| left.0.cmp(&right.0));
        envelopes
    }

    fn install_process_worker(&self, worker: DurableProcessWorker) {
        *self.process_worker.lock().expect("process worker lock") = Some(worker);
    }
}

#[derive(Default)]
struct PositionalReplayContext {
    sleeps: Mutex<Vec<u64>>,
    runs: Mutex<Vec<String>>,
    records: Mutex<Vec<(String, Vec<u8>)>>,
    replaying: AtomicBool,
    replay_cursor: AtomicUsize,
}

impl PositionalReplayContext {
    fn start_replay(&self) {
        self.replaying.store(true, Ordering::SeqCst);
        self.replay_cursor.store(0, Ordering::SeqCst);
    }

    fn runs(&self) -> Vec<String> {
        self.runs.lock().expect("runs lock").clone()
    }

    fn record_count(&self) -> usize {
        self.records.lock().expect("records lock").len()
    }
}

impl<'ctx> RestateControllerContext<'ctx> for Arc<PositionalReplayContext> {
    fn sleep_send<'run>(
        &'run self,
        duration: Duration,
    ) -> Pin<Box<dyn Future<Output = Result<(), TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
    {
        self.sleeps
            .lock()
            .expect("sleeps lock")
            .push(duration.as_millis() as u64);
        Box::pin(async { Ok(()) })
    }

    fn run_json_send<'run, T, Fut>(
        &'run self,
        effect_name: String,
        _retry_policy: Option<RunRetryPolicy>,
        future: Fut,
    ) -> Pin<Box<dyn Future<Output = Result<Json<T>, TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
        T: Serialize + DeserializeOwned + Send + 'static,
        Fut: Future<Output = T> + Send + 'run,
    {
        self.runs
            .lock()
            .expect("runs lock")
            .push(effect_name.clone());
        if self.replaying.load(Ordering::SeqCst) {
            let position = self.replay_cursor.fetch_add(1, Ordering::SeqCst);
            let recorded = self
                .records
                .lock()
                .expect("records lock")
                .get(position)
                .cloned();
            return Box::pin(async move {
                let (recorded_effect_name, bytes) = recorded.ok_or_else(|| {
                    TerminalError::new(format!("missing recorded effect at position {position}"))
                })?;
                if recorded_effect_name != effect_name {
                    return Err(TerminalError::new(format!(
                        "recorded effect at position {position} was `{recorded_effect_name}`, got `{effect_name}`"
                    )));
                }
                serde_json::from_slice(&bytes)
                    .map(Json)
                    .map_err(TerminalError::from_error)
            });
        }

        let context = Arc::clone(self);
        Box::pin(async move {
            let value = future.await;
            let bytes = serde_json::to_vec(&value).map_err(TerminalError::from_error)?;
            context
                .records
                .lock()
                .expect("records lock")
                .push((effect_name, bytes));
            Ok(Json(value))
        })
    }

    fn start_process_workflow<'run>(
        &'run self,
        _registration: ProcessRegistration,
        _execution_context: ProcessExecutionContext,
    ) -> Pin<Box<dyn Future<Output = Result<String, TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
    {
        Box::pin(async { Err(TerminalError::new("process workflow start is unsupported")) })
    }

    fn request_process_workflow_cancel<'run>(
        &'run self,
        _request: RestateProcessCancelRequest,
    ) -> Pin<Box<dyn Future<Output = Result<(), TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
    {
        Box::pin(async { Err(TerminalError::new("process workflow cancel is unsupported")) })
    }

    fn await_event<'run>(
        &'run self,
        _request: RestateDurableWaitAwaitRequest,
        _cancellation: tokio_util::sync::CancellationToken,
    ) -> Pin<Box<dyn Future<Output = Result<Resolution, TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
    {
        Box::pin(async { Err(TerminalError::new("event await is unsupported")) })
    }

    fn peek_event<'run>(
        &'run self,
        _address: RestateDurableWaitAddress,
    ) -> Pin<Box<dyn Future<Output = Result<Option<Resolution>, TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
    {
        Box::pin(async { Ok(None) })
    }

    fn await_process_terminal<'run>(
        &'run self,
        _process_id: String,
    ) -> Pin<Box<dyn Future<Output = Result<ProcessAwaitOutput, TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
    {
        Box::pin(async { Err(TerminalError::new("process terminal await is unsupported")) })
    }

    fn resolve_event<'run>(
        &'run self,
        _request: RestateDurableWaitResolveRequest,
    ) -> Pin<Box<dyn Future<Output = Result<ResolveOutcome, TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
    {
        Box::pin(async { Err(TerminalError::new("event resolve is unsupported")) })
    }

    fn update_session_waits<'run>(
        &'run self,
        _session_id: String,
        _revoke: bool,
    ) -> Pin<Box<dyn Future<Output = Result<(), TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
    {
        Box::pin(async { Err(TerminalError::new("session wait update is unsupported")) })
    }
}

impl<'ctx> RestateControllerContext<'ctx> for Arc<ReplayableRecordingContext> {
    fn sleep_send<'run>(
        &'run self,
        duration: Duration,
    ) -> Pin<Box<dyn Future<Output = Result<(), TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
    {
        self.sleeps
            .lock()
            .expect("sleeps lock")
            .push(duration.as_millis() as u64);
        Box::pin(async { Ok(()) })
    }

    fn run_json_send<'run, T, Fut>(
        &'run self,
        effect_name: String,
        _retry_policy: Option<RunRetryPolicy>,
        future: Fut,
    ) -> Pin<Box<dyn Future<Output = Result<Json<T>, TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
        T: Serialize + DeserializeOwned + Send + 'static,
        Fut: Future<Output = T> + Send + 'run,
    {
        self.runs
            .lock()
            .expect("runs lock")
            .push(effect_name.clone());
        let replaying = self.replaying.load(Ordering::SeqCst);
        if replaying {
            let recorded = self
                .records
                .lock()
                .expect("records lock")
                .get(&effect_name)
                .cloned();
            return Box::pin(async move {
                let bytes = recorded.ok_or_else(|| {
                    TerminalError::new(format!("missing recorded effect `{effect_name}`"))
                })?;
                serde_json::from_slice(&bytes)
                    .map(Json)
                    .map_err(TerminalError::from_error)
            });
        }

        let context = Arc::clone(self);
        Box::pin(async move {
            let value = future.await;
            let bytes = serde_json::to_vec(&value).map_err(TerminalError::from_error)?;
            context
                .records
                .lock()
                .expect("records lock")
                .insert(effect_name, bytes);
            Ok(Json(value))
        })
    }

    fn start_process_workflow<'run>(
        &'run self,
        registration: ProcessRegistration,
        execution_context: ProcessExecutionContext,
    ) -> Pin<Box<dyn Future<Output = Result<String, TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
    {
        let worker = self
            .process_worker
            .lock()
            .expect("process worker lock")
            .clone();
        let context = Arc::clone(self);
        Box::pin(async move {
            let Some(worker) = worker else {
                return Err(TerminalError::new("process workflow start is unsupported"));
            };
            let process_id = registration.id.clone();
            let controller = RestateRuntimeEffectController::new(Arc::clone(&context));
            let scoped_effect_controller = controller
                .scoped_effect_controller(ExecutionScope::process(&process_id))
                .map_err(TerminalError::from_error)?;
            let cancellation = tokio_util::sync::CancellationToken::new();
            let mut handover = None;
            let execution_write_authority =
                lash_core::ProcessExecutionWriteAuthority::testing(&process_id);
            let output = loop {
                match worker
                    .run_process_segment_with_scoped_effect_controller(
                        registration.clone(),
                        execution_context.clone(),
                        execution_write_authority.clone(),
                        scoped_effect_controller.clone(),
                        cancellation.clone(),
                        handover,
                    )
                    .await
                    .map_err(TerminalError::from_error)?
                {
                    lash_core::ProcessRunOutcome::Terminal(output) => break *output,
                    lash_core::ProcessRunOutcome::SegmentBoundary(next) => handover = Some(next),
                }
            };
            context
                .events
                .resolve_process_terminal(&process_id, &output);
            Ok(format!("invocation-{process_id}"))
        })
    }

    fn request_process_workflow_cancel<'run>(
        &'run self,
        _request: RestateProcessCancelRequest,
    ) -> Pin<Box<dyn Future<Output = Result<(), TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
    {
        Box::pin(async { Err(TerminalError::new("process workflow cancel is unsupported")) })
    }

    fn await_event<'run>(
        &'run self,
        request: RestateDurableWaitAwaitRequest,
        cancellation: tokio_util::sync::CancellationToken,
    ) -> Pin<Box<dyn Future<Output = Result<Resolution, TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
    {
        self.events.await_event(request, cancellation)
    }

    fn peek_event<'run>(
        &'run self,
        address: RestateDurableWaitAddress,
    ) -> Pin<Box<dyn Future<Output = Result<Option<Resolution>, TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
    {
        let resolution = if self.replaying.load(Ordering::SeqCst) {
            let position = self.peek_cursor.fetch_add(1, Ordering::SeqCst);
            self.peek_records
                .lock()
                .expect("peek records lock")
                .get(position)
                .cloned()
                .ok_or_else(|| {
                    TerminalError::new(format!(
                        "missing recorded await-event peek at position {position}"
                    ))
                })
        } else {
            let resolution = self
                .events
                .durable_events
                .lock()
                .expect("durable events lock")
                .get(&address.workflow_key)
                .cloned();
            self.peek_records
                .lock()
                .expect("peek records lock")
                .push(resolution.clone());
            Ok(resolution)
        };
        Box::pin(async move { resolution })
    }

    fn await_process_terminal<'run>(
        &'run self,
        process_id: String,
    ) -> Pin<Box<dyn Future<Output = Result<ProcessAwaitOutput, TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
    {
        self.events.await_process_terminal(process_id)
    }

    fn resolve_event<'run>(
        &'run self,
        request: RestateDurableWaitResolveRequest,
    ) -> Pin<Box<dyn Future<Output = Result<ResolveOutcome, TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
    {
        self.events.resolve_event(request)
    }

    fn update_session_waits<'run>(
        &'run self,
        session_id: String,
        revoke: bool,
    ) -> Pin<Box<dyn Future<Output = Result<(), TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
    {
        self.events.update_session_waits(session_id, revoke)
    }
}

fn runtime_invocation(kind: RuntimeEffectKind, effect_id: &str) -> RuntimeInvocation {
    RuntimeInvocation::effect(
        lash_core::runtime::RuntimeScope::for_turn("session", "turn", 1, 0),
        effect_id,
        kind,
        format!("session:turn:1:0:{}:{effect_id}", kind.as_str()),
    )
}

#[test]
fn restate_turn_cancel_race_excludes_process_owned_waits() {
    let process_scoped_sleep = RuntimeInvocation::effect(
        lash_core::runtime::RuntimeScope::for_turn("session", "turn", 1, 0),
        "parent:process:worker:sleep:1",
        RuntimeEffectKind::Sleep,
        "session:turn:1:0:process:worker:sleep:1",
    );
    assert!(
        restate_timer_turn_cancel_wait_request(&process_scoped_sleep, false)
            .expect("process sleep classification")
            .is_none(),
        "background process sleep must outlive its originating turn"
    );

    assert!(
        restate_await_event_turn_cancel_wait_request(
            &runtime_invocation(RuntimeEffectKind::AwaitEvent, "process-wait"),
            false,
        )
        .expect("process await-event classification")
        .is_none(),
        "background process await-event must outlive its originating turn"
    );

    assert!(
        restate_timer_turn_cancel_wait_request(
            &runtime_invocation(RuntimeEffectKind::Sleep, "turn-sleep"),
            true,
        )
        .expect("turn sleep classification")
        .is_some(),
        "foreground turn sleep must observe the durable cancellation gate"
    );

    assert!(
        restate_await_event_turn_cancel_wait_request(
            &runtime_invocation(RuntimeEffectKind::AwaitEvent, "turn-process-wait"),
            true,
        )
        .expect("foreground process await-event classification")
        .is_some(),
        "a Lashlang wait inside a foreground turn must observe the turn gate"
    );
}

#[tokio::test]
async fn restate_controller_executes_atomic_effect_inside_run() {
    let context = Arc::new(RecordingContext::default());
    let host = RestateRuntimeEffectController::new(context.clone());
    let err = host
        .execute_effect(
            RuntimeEffectEnvelope::new(
                runtime_invocation(RuntimeEffectKind::ToolAttempt, "step"),
                RuntimeEffectCommand::ToolAttempt {
                    call: prepared_tool_call(),
                    execution_grant: None,
                    attempt: 1,
                    max_attempts: 1,
                },
            ),
            RuntimeEffectLocalExecutor::unavailable(),
        )
        .await
        .expect_err("unavailable local executor should be returned from ctx.run");

    assert_eq!(err.code, "runtime_effect_local_executor_unavailable");
    assert_eq!(
        context.runs.lock().expect("runs lock").as_slice(),
        &["lash:session:turn:1:0:tool_attempt:step".to_string()]
    );
    assert!(context.sleeps.lock().expect("sleeps lock").is_empty());
}

#[tokio::test]
async fn restate_positional_replay_records_tool_attempt_as_one_command() {
    let context = Arc::new(PositionalReplayContext::default());
    let host = RestateRuntimeEffectController::new(context.clone());
    let call = prepared_tool_call_with("call-fast", "fast_tool");
    let envelope = RuntimeEffectEnvelope::new(
        runtime_invocation(RuntimeEffectKind::ToolAttempt, "tool-attempt"),
        RuntimeEffectCommand::ToolAttempt {
            call,
            execution_grant: None,
            attempt: 1,
            max_attempts: 1,
        },
    );
    let local_runs = Arc::new(AtomicUsize::new(0));

    let first = host
        .execute_effect(
            envelope.clone(),
            RuntimeEffectLocalExecutor::testing({
                let local_runs = Arc::clone(&local_runs);
                |_envelope| async move {
                    local_runs.fetch_add(1, Ordering::SeqCst);
                    Ok(RuntimeEffectOutcome::ToolAttempt {
                        launch: Box::new(lash_core::ToolAttemptLaunch::Done {
                            record: Box::new(completed_tool_record("call-fast", "fast_tool")),
                        }),
                        triggers: Vec::new(),
                    })
                }
            }),
        )
        .await
        .expect("first attempt run");

    let RuntimeEffectOutcome::ToolAttempt { launch, .. } = first else {
        panic!("expected tool attempt outcome");
    };
    assert!(matches!(
        &*launch,
        lash_core::ToolAttemptLaunch::Done { record } if record.call_id.as_deref() == Some("call-fast")
    ));
    assert_eq!(context.record_count(), 1);
    assert_eq!(context.runs().len(), 1);
    assert_eq!(local_runs.load(Ordering::SeqCst), 1);

    context.start_replay();
    let replayed = host
        .execute_effect(
            envelope,
            RuntimeEffectLocalExecutor::testing(|_| async {
                panic!("positional replay should not rerun the ToolAttempt executor")
            }),
        )
        .await
        .expect("replayed attempt run");

    let RuntimeEffectOutcome::ToolAttempt { launch, .. } = replayed else {
        panic!("expected replayed tool attempt outcome");
    };
    assert!(matches!(
        &*launch,
        lash_core::ToolAttemptLaunch::Done { record } if record.call_id.as_deref() == Some("call-fast")
    ));
    assert_eq!(context.record_count(), 1);
    assert_eq!(context.runs().len(), 2);
    assert_eq!(local_runs.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn restate_controller_routes_sleep_only_through_timer() {
    let context = Arc::new(RecordingContext::default());
    let host = RestateRuntimeEffectController::new(context.clone());
    let outcome = host
        .execute_effect(
            RuntimeEffectEnvelope::new(
                runtime_invocation(RuntimeEffectKind::Sleep, "sleep"),
                RuntimeEffectCommand::Sleep { duration_ms: 42 },
            ),
            RuntimeEffectLocalExecutor::unavailable(),
        )
        .await
        .expect("sleep");

    assert!(matches!(outcome, RuntimeEffectOutcome::Sleep));
    assert_eq!(
        context.sleeps.lock().expect("sleeps lock").as_slice(),
        &[42]
    );
    assert!(context.runs.lock().expect("runs lock").is_empty());
}

#[tokio::test]
async fn restate_timer_stops_when_its_fresh_attempt_is_cancelled() {
    let context = Arc::new(RecordingContext::default());
    context.block_sleeps.store(true, Ordering::SeqCst);
    let host = RestateRuntimeEffectController::new(context.clone());
    let cancellation = tokio_util::sync::CancellationToken::new();
    cancellation.cancel();

    let error = host
        .execute_effect(
            RuntimeEffectEnvelope::new(
                runtime_invocation(RuntimeEffectKind::Sleep, "cancelled-sleep"),
                RuntimeEffectCommand::Sleep {
                    duration_ms: 60_000,
                },
            ),
            RuntimeEffectLocalExecutor::sleep(cancellation),
        )
        .await
        .expect_err("cancelled Restate timer must stop the interpreter attempt");

    assert_eq!(error.code, "runtime_effect_sleep_cancelled");
    assert_eq!(
        context.sleeps.lock().expect("sleeps lock").as_slice(),
        &[60_000]
    );
}

#[tokio::test]
async fn restate_suspended_timer_is_woken_by_the_durable_turn_cancel_gate() {
    let context = Arc::new(RecordingContext::default());
    context.block_sleeps.store(true, Ordering::SeqCst);
    let cancellation = tokio_util::sync::CancellationToken::new();
    let task_context = Arc::clone(&context);
    let task_cancellation = cancellation.clone();
    let sleep = tokio::spawn(async move {
        RestateRuntimeEffectController::new(task_context)
            .execute_effect(
                RuntimeEffectEnvelope::new(
                    runtime_invocation(RuntimeEffectKind::Sleep, "suspended-sleep"),
                    RuntimeEffectCommand::Sleep {
                        duration_ms: 300_000,
                    },
                ),
                RuntimeEffectLocalExecutor::sleep(task_cancellation),
            )
            .await
    });
    tokio::task::yield_now().await;
    assert!(!sleep.is_finished(), "timer must genuinely remain pending");

    let cancel_key = restate_await_event_key(
        &ExecutionScope::turn("session", "turn"),
        AwaitEventWaitIdentity::TurnCancelGate,
    )
    .expect("cancel gate key");
    assert_eq!(
        context.resolve_durable_event(RestateDurableWaitResolveRequest {
            address: RestateDurableWaitAddress::for_key(&cancel_key),
            resolution: Resolution::Ok(serde_json::json!({
                "state": "cancel_requested",
                "cancellation": {
                    "request_id": "cancel-suspended-sleep",
                    "origin": "test",
                },
            })),
        }),
        ResolveOutcome::Accepted
    );

    let error = tokio::time::timeout(Duration::from_secs(1), sleep)
        .await
        .expect("durable cancel gate must wake a suspended timer promptly")
        .expect("join suspended timer")
        .expect_err("durable turn cancellation must abort the timer effect");
    assert_eq!(error.code, "runtime_effect_sleep_cancelled");
    assert!(cancellation.is_cancelled());
}

#[tokio::test]
async fn restate_suspended_await_event_is_woken_by_the_durable_turn_cancel_gate() {
    let context = Arc::new(RecordingContext::default());
    let awaited_key = restate_await_event_key(
        &ExecutionScope::turn("session", "turn"),
        AwaitEventWaitIdentity::Custom {
            key: "wait-for-signal".to_string(),
        },
    )
    .expect("await-event key");
    let cancellation = tokio_util::sync::CancellationToken::new();
    let task_context = Arc::clone(&context);
    let task_cancellation = cancellation.clone();
    let wait = tokio::spawn(async move {
        RestateRuntimeEffectController::new(task_context)
            .execute_effect(
                RuntimeEffectEnvelope::new(
                    runtime_invocation(RuntimeEffectKind::AwaitEvent, "suspended-await-event"),
                    RuntimeEffectCommand::AwaitEvent { key: awaited_key },
                ),
                RuntimeEffectLocalExecutor::await_event(task_cancellation, None),
            )
            .await
    });
    tokio::task::yield_now().await;
    assert!(
        !wait.is_finished(),
        "await-event must genuinely remain pending"
    );

    let cancel_key = restate_await_event_key(
        &ExecutionScope::turn("session", "turn"),
        AwaitEventWaitIdentity::TurnCancelGate,
    )
    .expect("cancel gate key");
    assert_eq!(
        context.resolve_durable_event(RestateDurableWaitResolveRequest {
            address: RestateDurableWaitAddress::for_key(&cancel_key),
            resolution: Resolution::Ok(serde_json::json!({
                "state": "cancel_requested",
                "cancellation": {
                    "request_id": "cancel-suspended-await-event",
                    "origin": "test",
                },
            })),
        }),
        ResolveOutcome::Accepted
    );

    let outcome = tokio::time::timeout(Duration::from_secs(1), wait)
        .await
        .expect("durable cancel gate must wake a suspended await-event promptly")
        .expect("join suspended await-event")
        .expect("turn cancellation should terminalize the await-event");
    assert!(matches!(
        outcome,
        RuntimeEffectOutcome::AwaitEvent {
            resolution: Resolution::Cancelled,
        }
    ));
    assert!(cancellation.is_cancelled());
}

#[tokio::test]
async fn restate_routes_every_execution_scope_to_an_exact_durable_wait_address() {
    let context = Arc::new(RecordingContext::default());
    let host = RestateRuntimeEffectController::new(context.clone());
    let scopes = [
        ExecutionScope::turn("session", "turn"),
        ExecutionScope::process("process"),
        ExecutionScope::queue_drain("session", "drain"),
        ExecutionScope::session_delete("session"),
        ExecutionScope::runtime_operation("operation"),
    ];
    let mut addresses = HashSet::new();

    for (index, scope) in scopes.into_iter().enumerate() {
        let key = restate_await_event_key(
            &scope,
            AwaitEventWaitIdentity::Custom {
                key: format!("scope-{index}"),
            },
        )
        .expect("scope wait key");
        let address = RestateDurableWaitAddress::for_key(&key);
        assert!(addresses.insert(address.workflow_key.clone()));
        assert!(
            !address.index_key().contains('/'),
            "wait-index object keys must remain one ingress path segment"
        );
        let resolution = Resolution::Ok(serde_json::json!({ "scope": index }));
        assert_eq!(
            host.resolve_await_event(&key, resolution.clone())
                .await
                .expect("resolve scope wait"),
            ResolveOutcome::Accepted
        );
        assert_eq!(
            host.await_await_event(&key, tokio_util::sync::CancellationToken::new(), None,)
                .await
                .expect("await scope wait"),
            resolution
        );
    }
}

#[tokio::test]
async fn restate_execute_effect_honors_cancellation_and_terminalizes_late_resolution() {
    let context = Arc::new(RecordingContext::default());
    let key = restate_await_event_key(
        &ExecutionScope::turn("cancel-session", "cancel-turn"),
        AwaitEventWaitIdentity::tool_completion("cancel-tool"),
    )
    .expect("cancel wait key");
    let cancellation = tokio_util::sync::CancellationToken::new();
    let task_context = context.clone();
    let task_key = key.clone();
    let task_cancellation = cancellation.clone();
    let wait = tokio::spawn(async move {
        RestateRuntimeEffectController::new(task_context)
            .execute_effect(
                RuntimeEffectEnvelope::new(
                    runtime_invocation(RuntimeEffectKind::AwaitEvent, "cancel-wait"),
                    RuntimeEffectCommand::AwaitEvent { key: task_key },
                ),
                RuntimeEffectLocalExecutor::await_event(task_cancellation, None),
            )
            .await
    });
    tokio::task::yield_now().await;
    assert!(
        !wait.is_finished(),
        "mock wait must genuinely remain pending"
    );
    cancellation.cancel();
    let outcome = wait
        .await
        .expect("join cancellation wait")
        .expect("cancel wait");
    assert!(matches!(
        outcome,
        RuntimeEffectOutcome::AwaitEvent {
            resolution: Resolution::Cancelled,
        }
    ));

    let host = RestateRuntimeEffectController::new(context);
    assert_eq!(
        host.resolve_await_event(&key, Resolution::Ok(serde_json::json!("late")))
            .await
            .expect("late resolve"),
        ResolveOutcome::AlreadyResolved {
            terminal: Resolution::Cancelled,
        }
    );
}

#[tokio::test]
async fn restate_deadline_durably_terminalizes_timeout() {
    let context = Arc::new(RecordingContext::default());
    let host = RestateRuntimeEffectController::new(context.clone());
    let key = restate_await_event_key(
        &ExecutionScope::runtime_operation("deadline-operation"),
        AwaitEventWaitIdentity::Custom {
            key: "deadline".to_string(),
        },
    )
    .expect("deadline key");
    let resolution = host
        .await_await_event(
            &key,
            tokio_util::sync::CancellationToken::new(),
            Some(std::time::Instant::now() + Duration::from_millis(10)),
        )
        .await
        .expect("deadline wait");
    assert_eq!(resolution, Resolution::Timeout);
    assert_eq!(
        host.resolve_await_event(&key, Resolution::Ok(serde_json::json!("late")))
            .await
            .expect("late deadline resolve"),
        ResolveOutcome::AlreadyResolved {
            terminal: Resolution::Timeout,
        }
    );
}

#[tokio::test]
async fn restate_session_cancel_cancels_current_waits_but_allows_new_waits() {
    let context = Arc::new(RecordingContext::default());
    let first_key = restate_await_event_key(
        &ExecutionScope::queue_drain("cancel-session", "drain-one"),
        AwaitEventWaitIdentity::Custom {
            key: "first".to_string(),
        },
    )
    .expect("first session wait");
    let task_context = context.clone();
    let task_key = first_key.clone();
    let wait = tokio::spawn(async move {
        RestateRuntimeEffectController::new(task_context)
            .await_await_event(&task_key, tokio_util::sync::CancellationToken::new(), None)
            .await
    });
    tokio::task::yield_now().await;
    assert!(!wait.is_finished());
    let host = RestateRuntimeEffectController::new(context.clone());
    host.cancel_await_events_for_session("cancel-session")
        .await
        .expect("cancel session waits");
    assert_eq!(
        wait.await
            .expect("join cancelled session wait")
            .expect("cancelled session wait"),
        Resolution::Cancelled
    );

    let next_key = restate_await_event_key(
        &ExecutionScope::turn("cancel-session", "turn-two"),
        AwaitEventWaitIdentity::Custom {
            key: "next".to_string(),
        },
    )
    .expect("next session wait");
    let expected = Resolution::Ok(serde_json::json!("resumed"));
    host.resolve_await_event(&next_key, expected.clone())
        .await
        .expect("resolve new session wait");
    assert_eq!(
        host.await_await_event(&next_key, tokio_util::sync::CancellationToken::new(), None,)
            .await
            .expect("new wait after cancel"),
        expected
    );
}

#[tokio::test]
async fn restate_session_delete_revokes_current_and_future_waits() {
    let context = Arc::new(RecordingContext::default());
    let host = RestateRuntimeEffectController::new(context.clone());
    let key = restate_await_event_key(
        &ExecutionScope::session_delete("deleted-session"),
        AwaitEventWaitIdentity::Custom {
            key: "delete".to_string(),
        },
    )
    .expect("delete wait");
    let task_context = context.clone();
    let task_key = key.clone();
    let wait = tokio::spawn(async move {
        RestateRuntimeEffectController::new(task_context)
            .await_await_event(&task_key, tokio_util::sync::CancellationToken::new(), None)
            .await
    });
    tokio::task::yield_now().await;
    assert!(!wait.is_finished());
    host.revoke_await_events_for_session("deleted-session")
        .await
        .expect("revoke deleted session waits");
    assert_eq!(
        wait.await
            .expect("join deleted wait")
            .expect("deleted wait"),
        Resolution::Cancelled
    );

    let future_key = restate_await_event_key(
        &ExecutionScope::turn("deleted-session", "future-turn"),
        AwaitEventWaitIdentity::Custom {
            key: "future".to_string(),
        },
    )
    .expect("future revoked wait");
    let future_error = host
        .await_await_event(
            &future_key,
            tokio_util::sync::CancellationToken::new(),
            None,
        )
        .await
        .expect_err("future revoked wait is not observable");
    assert_eq!(future_error.code.as_str(), "await_event_unknown_or_revoked");
    assert_eq!(
        host.resolve_await_event(&future_key, Resolution::Ok(serde_json::json!("late")))
            .await
            .expect("late resolve after deletion"),
        ResolveOutcome::UnknownOrRevoked
    );
}

#[tokio::test]
async fn restate_effect_host_without_ingress_refuses_session_mutation() {
    let host = RestateEffectHost::new();
    let err = host
        .revoke_await_events_for_session("restate-session")
        .await
        .expect_err("deployment host without ingress cannot revoke Restate state");
    assert_eq!(err.code.as_str(), "restate_await_event_ingress_required");
}

#[tokio::test]
async fn restate_effect_host_checks_revocation_then_awaits_resolution() {
    let expected = Resolution::Ok(serde_json::json!({ "answer": "approved" }));
    let scripted = Arc::new(ScriptedHttpTransport::new([
        HttpResponse {
            status: 200,
            headers: vec![("content-type".to_string(), "application/json".to_string())],
            body: HttpResponseBody::buffered("false"),
        },
        HttpResponse {
            status: 200,
            headers: vec![("content-type".to_string(), "application/json".to_string())],
            body: HttpResponseBody::buffered(
                serde_json::to_string(&expected).expect("encode resolution"),
            ),
        },
    ]));
    let host = RestateEffectHost::with_ingress_url(RestateConnection::with_transport(
        "https://restate.example",
        scripted.clone(),
    ));
    let key = restate_await_event_key(
        &ExecutionScope::turn("single-call-session", "single-call-turn"),
        AwaitEventWaitIdentity::Custom {
            key: "single-call-wait".to_string(),
        },
    )
    .expect("single-call wait key");

    let resolution = host
        .await_await_event(&key, tokio_util::sync::CancellationToken::new(), None)
        .await
        .expect("await resolution through ingress");

    assert_eq!(resolution, expected);
    let requests = scripted.requests();
    assert_eq!(requests.len(), 2, "durable wait must check its tombstone");
    assert!(
        requests[0]
            .url
            .contains("/LashDurableWaitIndex/single-call-session/")
            && requests[0].url.ends_with("/is_revoked"),
        "durable wait must check the session tombstone first: {}",
        requests[0].url
    );
    assert!(
        requests[1].url.contains("/LashDurableWaitWorkflow/")
            && requests[1].url.ends_with("/await_resolution"),
        "durable wait must call await_resolution directly: {}",
        requests[1].url
    );
}

struct PostCommitFailingQueuedWorkRunHandle {
    attempts: AtomicUsize,
    recovered: tokio::sync::Notify,
}

#[async_trait::async_trait]
impl lash_core::QueuedWorkRunHandle for PostCommitFailingQueuedWorkRunHandle {
    async fn run_queued_work(
        &self,
        _request: lash_core::QueuedWorkRunRequest,
    ) -> Result<(), lash_core::QueuedWorkRunError> {
        if self.attempts.fetch_add(1, Ordering::SeqCst) == 0 {
            return Err(lash_core::QueuedWorkRunError::transient(
                PluginError::Session(
                    "FIG-430 deterministic post-commit dispatch failure".to_string(),
                ),
            ));
        }
        self.recovered.notify_one();
        Ok(())
    }
}

/// FIG-430: durable acceptance is final once the pending-input row commits.
/// Dispatch failure is operational telemetry, and the wake retries itself
/// without waiting for another enqueue or unrelated host event.
#[tokio::test]
async fn restate_enqueue_never_errors_after_commit() {
    let dir = tempfile::tempdir().expect("tempdir");
    let session_id = "restate-enqueue-post-commit-error";
    let provider = lash_core::testing::TestProvider::builder()
        .kind("fig-430-stub")
        .complete(|_| async { Ok(lash_core::LlmResponse::default()) })
        .build()
        .into_handle();
    let queued_work = Arc::new(PostCommitFailingQueuedWorkRunHandle {
        attempts: AtomicUsize::new(0),
        recovered: tokio::sync::Notify::new(),
    });
    let recovered = queued_work.recovered.notified();
    let core = lash::LashCore::standard_builder()
        .provider(provider)
        .model(lash_core::ModelSpec::new(
            "fig-430-model",
            std::num::NonZeroUsize::new(1024).expect("non-zero context window"),
        ))
        .store_factory(Arc::new(lash_sqlite_store::SqliteSessionStoreFactory::new(
            dir.path().join("sessions"),
        )))
        .effect_host(Arc::new(lash::durability::InlineEffectHost::default()))
        .attachment_store(Arc::new(DurableMemoryAttachmentStore::default()))
        .process_env_store(Arc::new(DurableMemoryProcessEnvStore::default()))
        .queued_work_driver(lash_core::QueuedWorkDriver::new(queued_work.clone()))
        .build()
        .expect("build FIG-430 core");
    let session = core
        .session(session_id)
        .open()
        .await
        .expect("open FIG-430 session");

    let outcome = session
        .enqueue(lash_core::TurnInput::text("commit before dispatch"))
        .id("fig-430-retry")
        .send()
        .await;
    tokio::time::timeout(std::time::Duration::from_secs(1), recovered)
        .await
        .expect("the failed post-commit wake must retry on its own");
    let persisted = session
        .pending_turn_inputs()
        .await
        .expect("inspect committed pending input");

    match (&outcome, persisted.as_slice()) {
        (Err(_), []) => {}
        (Ok(receipt), [stored]) => {
            assert_eq!(stored.input_id, receipt.input_id);
            assert_eq!(stored.session_id, receipt.session_id);
            assert_eq!(stored.source_key, receipt.source_key);
            assert_eq!(stored.ingress, receipt.ingress);
            assert_eq!(receipt.source_key.as_deref(), Some("host:fig-430-retry"));
        }
        (Err(error), stored) => panic!(
            "enqueue returned an undifferentiated error after durable commit: \
             caller_outcome={error:?}, persisted_row_count={}",
            stored.len()
        ),
        (Ok(receipt), stored) => panic!(
            "successful enqueue must identify exactly one durable row: \
             caller_outcome={receipt:?}, persisted_rows={stored:?}"
        ),
    }
    assert_eq!(
        queued_work.attempts.load(Ordering::SeqCst),
        2,
        "the wake path retries exactly once after the injected failure"
    );

    let retry_receipt = session
        .enqueue(lash_core::TurnInput::text("commit before dispatch"))
        .id("fig-430-retry")
        .send()
        .await
        .expect("retry the same durable source identity");
    assert_eq!(
        Some(&retry_receipt.input_id),
        outcome.as_ref().ok().map(|receipt| &receipt.input_id),
        "the source key is the idempotent retry identity"
    );
    assert_eq!(
        session
            .pending_turn_inputs()
            .await
            .expect("inspect idempotent retry")
            .len(),
        1,
        "an exact source retry must not create another durable input"
    );
}

fn replay_test_policy(session_id: &str) -> lash_core::SessionPolicy {
    let mut policy = lash_core::testing::mock_session_policy();
    policy.session_id = Some(session_id.to_string());
    policy
}

fn replay_test_state(
    session_id: &str,
    policy: &lash_core::SessionPolicy,
) -> lash_core::RuntimeSessionState {
    lash_core::RuntimeSessionState {
        session_id: session_id.to_string(),
        policy: policy.clone(),
        ..lash_core::RuntimeSessionState::default()
    }
}

fn replay_test_input(turn_id: &str) -> lash_core::TurnInput {
    let mut input = lash_core::TurnInput::text("finish once");
    input.trace_turn_id = Some(turn_id.to_string());
    input
}

async fn replay_test_runtime(
    session_id: &str,
    policy: lash_core::SessionPolicy,
    initial_state: lash_core::RuntimeSessionState,
    host: lash_core::RuntimeHostConfig,
    store: Arc<dyn lash_core::RuntimePersistence>,
) -> lash_core::LashRuntime {
    Box::pin(replay_test_runtime_with_plugins(
        session_id,
        policy,
        initial_state,
        host,
        store,
        lash_core::testing::test_standard_protocol_factories(),
    ))
    .await
}

async fn replay_test_runtime_with_plugins(
    session_id: &str,
    policy: lash_core::SessionPolicy,
    initial_state: lash_core::RuntimeSessionState,
    host: lash_core::RuntimeHostConfig,
    store: Arc<dyn lash_core::RuntimePersistence>,
    plugin_factories: Vec<Arc<dyn lash_core::PluginFactory>>,
) -> lash_core::LashRuntime {
    Box::pin(replay_test_runtime_with_plugins_and_registry(
        session_id,
        policy,
        initial_state,
        host,
        store,
        plugin_factories,
        None,
    ))
    .await
}

#[allow(clippy::too_many_arguments)]
async fn replay_test_runtime_with_plugins_and_registry(
    session_id: &str,
    policy: lash_core::SessionPolicy,
    initial_state: lash_core::RuntimeSessionState,
    host: lash_core::RuntimeHostConfig,
    store: Arc<dyn lash_core::RuntimePersistence>,
    plugin_factories: Vec<Arc<dyn lash_core::PluginFactory>>,
    process_registry: Option<Arc<dyn ProcessRegistry>>,
) -> lash_core::LashRuntime {
    let mut builder = lash_core::LashRuntime::builder()
        .with_session_id(session_id)
        .with_policy(policy)
        .with_initial_state(initial_state)
        .with_runtime_host(host)
        .with_plugin_factories(plugin_factories)
        .with_store(store);
    if let Some(process_registry) = process_registry {
        builder = builder.with_process_registry(process_registry);
    }
    builder.build().await.expect("build replay test runtime")
}

async fn run_restate_replay_turn(
    runtime: &mut lash_core::LashRuntime,
    context: Arc<ReplayableRecordingContext>,
    session_id: &str,
    turn_id: &str,
) -> lash_core::AssembledTurn {
    let controller = RestateRuntimeEffectController::new(context);
    let scoped_effect_controller = controller
        .scoped_effect_controller(ExecutionScope::turn(session_id, turn_id))
        .expect("scoped restate controller");
    runtime
        .stream_turn(
            replay_test_input(turn_id),
            lash_core::TurnOptions::new(
                tokio_util::sync::CancellationToken::new(),
                scoped_effect_controller,
            ),
        )
        .await
        .expect("run replay test turn")
}

#[tokio::test]
async fn restate_handler_replay_retries_final_lash_commit_idempotently() {
    let dir = tempfile::tempdir().expect("tempdir");
    let session_id = "restate-final-commit-replay";
    let turn_id = "restate-turn-1";
    let provider_calls = Arc::new(AtomicUsize::new(0));
    let provider = lash_core::testing::TestProvider::builder()
        .kind("stub")
        .complete({
            let provider_calls = Arc::clone(&provider_calls);
            move |_request| {
                let provider_calls = Arc::clone(&provider_calls);
                async move {
                    let call_index = provider_calls.fetch_add(1, Ordering::SeqCst);
                    assert_eq!(
                        call_index, 0,
                        "Restate replay should return the recorded LLM effect"
                    );
                    Ok(lash_core::LlmResponse {
                        full_text: "committed once".to_string(),
                        parts: vec![lash_core::LlmOutputPart::Text {
                            text: "committed once".to_string(),
                            response_meta: None,
                        }],
                        response_metadata: Default::default(),
                        ..lash_core::LlmResponse::default()
                    })
                }
            }
        })
        .build()
        .into_handle();
    let mut host = lash_core::RuntimeHostConfig::in_memory();
    host.providers.provider_resolver = Arc::new(lash_core::SingleProviderResolver::new(provider));
    host.durability.attachment_store = Arc::new(lash_core::SessionAttachmentStore::ephemeral(
        Arc::new(DurableMemoryAttachmentStore::default()),
    ));
    host.durability.process_env_store = Arc::new(DurableMemoryProcessEnvStore::default());
    let store = Arc::new(
        lash_sqlite_store::Store::open(&dir.path().join("session.db"))
            .await
            .expect("open session store"),
    );
    let runtime_store: Arc<dyn lash_core::RuntimePersistence> = store.clone();
    let policy = replay_test_policy(session_id);
    let initial_state = replay_test_state(session_id, &policy);
    let context = Arc::new(ReplayableRecordingContext::default());

    let mut first = replay_test_runtime(
        session_id,
        policy.clone(),
        initial_state.clone(),
        host.clone(),
        Arc::clone(&runtime_store),
    )
    .await;
    let first_turn =
        run_restate_replay_turn(&mut first, Arc::clone(&context), session_id, turn_id).await;
    assert!(matches!(
        first_turn.outcome,
        lash_core::TurnOutcome::Finished(_)
    ));
    let first_runs = context.runs();
    assert!(!first_runs.is_empty());

    let blocking_owner =
        lash_core::LeaseOwnerIdentity::opaque("replay-blocker", "replay-blocker:001");
    let blocking_lease = lash_core::SessionExecutionLeaseStore::try_claim_session_execution_lease(
        &*store,
        session_id,
        &blocking_owner,
        60_000,
    )
    .await
    .expect("claim replay-blocking advisory lease")
    .acquired()
    .expect("replay-blocking advisory lease");
    context.start_replay();
    let retry_store: Arc<dyn lash_core::RuntimePersistence> =
        Arc::new(CommitRetryStore::new(Arc::clone(&runtime_store)));
    let mut replay =
        replay_test_runtime(session_id, policy, initial_state, host, retry_store).await;
    let replay_turn =
        run_restate_replay_turn(&mut replay, Arc::clone(&context), session_id, turn_id).await;
    assert!(matches!(
        replay_turn.outcome,
        lash_core::TurnOutcome::Finished(_)
    ));
    assert_eq!(first_turn.llm_calls.len(), 1);
    assert_eq!(replay_turn.llm_calls, first_turn.llm_calls);
    assert_eq!(provider_calls.load(Ordering::SeqCst), 1);
    lash_core::SessionExecutionLeaseStore::release_session_execution_lease(
        &*store,
        &blocking_lease.completion(),
    )
    .await
    .expect("release replay-blocking advisory lease");

    let conn = rusqlite::Connection::open(dir.path().join("session.db"))
        .expect("open raw session sqlite store");
    let rows: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM runtime_turn_commits WHERE session_id = ?1 AND turn_id = ?2",
            rusqlite::params![session_id, turn_id],
            |row| row.get(0),
        )
        .expect("count turn commit stamps");
    assert_eq!(rows, 1);
}

/// FIG-460: a dropped suspended handler leaves its advisory lease live, but a
/// fresh durable worker re-enters before TTL and still makes progress under the
/// authoritative final-commit CAS fence.
#[tokio::test]
async fn restate_replay_lease_acquisition_takes_recorded_branch() {
    let dir = tempfile::tempdir().expect("tempdir");
    let session_id = "restate-replay-lease-branch";
    let turn_id = "restate-replay-lease-turn-1";
    let provider_calls = Arc::new(AtomicUsize::new(0));
    let first_provider_started = Arc::new(tokio::sync::Notify::new());
    let provider = lash_core::testing::TestProvider::builder()
        .kind("stub")
        .complete({
            let provider_calls = Arc::clone(&provider_calls);
            let first_provider_started = Arc::clone(&first_provider_started);
            move |_| {
                let provider_calls = Arc::clone(&provider_calls);
                let first_provider_started = Arc::clone(&first_provider_started);
                async move {
                    let call_index = provider_calls.fetch_add(1, Ordering::SeqCst);
                    if call_index == 0 {
                        first_provider_started.notify_one();
                        std::future::pending::<()>().await;
                    }
                    Ok(lash_core::LlmResponse {
                        full_text: "fresh worker progressed".to_string(),
                        parts: vec![lash_core::LlmOutputPart::Text {
                            text: "fresh worker progressed".to_string(),
                            response_meta: None,
                        }],
                        response_metadata: Default::default(),
                        ..lash_core::LlmResponse::default()
                    })
                }
            }
        })
        .build()
        .into_handle();
    let mut host = lash_core::RuntimeHostConfig::in_memory();
    host.providers.provider_resolver = Arc::new(lash_core::SingleProviderResolver::new(provider));
    host.durability.attachment_store = Arc::new(lash_core::SessionAttachmentStore::ephemeral(
        Arc::new(DurableMemoryAttachmentStore::default()),
    ));
    host.durability.process_env_store = Arc::new(DurableMemoryProcessEnvStore::default());

    let store = Arc::new(
        lash_sqlite_store::Store::open(&dir.path().join("session.db"))
            .await
            .expect("open session store"),
    );
    let underlying_store: Arc<dyn lash_core::RuntimePersistence> = store.clone();
    let lease_claim_count = Arc::new(AtomicUsize::new(0));
    let probed_store = Arc::new(CommitRetryStore {
        inner: Arc::clone(&underlying_store),
        lease_claim_count: Arc::clone(&lease_claim_count),
    });
    let runtime_store: Arc<dyn lash_core::RuntimePersistence> = probed_store;
    let policy = replay_test_policy(session_id);
    let initial_state = replay_test_state(session_id, &policy);
    let context = Arc::new(ReplayableRecordingContext::default());

    let mut suspended = replay_test_runtime(
        session_id,
        policy.clone(),
        initial_state.clone(),
        host.clone(),
        Arc::clone(&runtime_store),
    )
    .await;
    let suspended_context = Arc::clone(&context);
    let suspended_turn = tokio::spawn(async move {
        run_restate_replay_turn(&mut suspended, suspended_context, session_id, turn_id).await
    });
    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        first_provider_started.notified(),
    )
    .await
    .expect("first durable worker reaches provider suspension");
    assert_eq!(lease_claim_count.load(Ordering::SeqCst), 1);
    assert!(
        !context.runs().is_empty(),
        "the suspended handler reached the real Restate run boundary"
    );
    suspended_turn.abort();
    assert!(
        suspended_turn
            .await
            .expect_err("dropped suspended handler future")
            .is_cancelled()
    );

    let mut fresh_worker =
        replay_test_runtime(session_id, policy, initial_state, host, runtime_store).await;
    let controller = RestateRuntimeEffectController::new(Arc::clone(&context));
    let scoped_effect_controller = controller
        .scoped_effect_controller(ExecutionScope::turn(session_id, turn_id))
        .expect("scoped replay controller");
    let replay_turn = fresh_worker
        .stream_turn(
            replay_test_input(turn_id),
            lash_core::TurnOptions::new(
                tokio_util::sync::CancellationToken::new(),
                scoped_effect_controller,
            ),
        )
        .await;

    let replay_turn = replay_turn.unwrap_or_else(|error| {
        panic!(
            "fresh durable worker must treat pre-TTL lease busy as advisory and \
             progress under CAS: {error:?}; total_lease_store_acquisitions={}",
            lease_claim_count.load(Ordering::SeqCst)
        )
    });
    assert!(matches!(
        replay_turn.outcome,
        lash_core::TurnOutcome::Finished(_)
    ));
    assert_eq!(
        replay_turn.assistant_output.safe_text,
        "fresh worker progressed"
    );
    assert_eq!(
        lease_claim_count.load(Ordering::SeqCst),
        2,
        "the fresh worker re-enters before TTL and observes the advisory busy lease"
    );
    assert_eq!(
        provider_calls.load(Ordering::SeqCst),
        2,
        "the dropped provider attempt is retried by the fresh worker"
    );

    let conn = rusqlite::Connection::open(dir.path().join("session.db"))
        .expect("open raw session sqlite store");
    let rows: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM runtime_turn_commits WHERE session_id = ?1 AND turn_id = ?2",
            rusqlite::params![session_id, turn_id],
            |row| row.get(0),
        )
        .expect("count liveness turn commit stamps");
    assert_eq!(rows, 1, "the fresh worker commits exactly once");
}

struct ReplayScalarPendingTools {
    scalar_invocations: Arc<AtomicUsize>,
    completion_key_tx:
        Mutex<Option<tokio::sync::oneshot::Sender<Result<lash_core::AwaitEventKey, String>>>>,
}

impl ReplayScalarPendingTools {
    fn scalar_definition() -> lash_core::ToolDefinition {
        lash_core::ToolDefinition::raw(
            "tool:replay_scalar_counter",
            "replay_scalar_counter",
            "Increment a non-idempotent replay probe counter.",
            serde_json::json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
            serde_json::json!({
                "type": "object",
                "properties": { "value": {} },
                "required": ["value"],
                "additionalProperties": false
            }),
        )
        .with_lashlang_binding(LashlangToolBinding::new(["tools"], "replay_scalar_counter"))
    }

    fn pending_definition() -> lash_core::ToolDefinition {
        lash_core::ToolDefinition::raw(
            "tool:replay_pending_input",
            "replay_pending_input",
            "Wait for an externally supplied replay-test value.",
            serde_json::json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
            serde_json::json!({
                "type": "object",
                "properties": { "answer": {} },
                "required": ["answer"],
                "additionalProperties": true
            }),
        )
        .with_lashlang_binding(LashlangToolBinding::new(["tools"], "replay_pending_input"))
    }
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for ReplayScalarPendingTools {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![
            Self::scalar_definition().manifest(),
            Self::pending_definition().manifest(),
        ]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        match name {
            "replay_scalar_counter" => Some(Arc::new(Self::scalar_definition().contract())),
            "replay_pending_input" => Some(Arc::new(Self::pending_definition().contract())),
            _ => None,
        }
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolResult {
        match call.name {
            "replay_scalar_counter" => {
                self.scalar_invocations.fetch_add(1, Ordering::SeqCst);
                lash_core::ToolResult::ok(serde_json::json!({ "value": "counted" }))
            }
            "replay_pending_input" => {
                let key = match call.context.completion_key().await {
                    Ok(key) => key,
                    Err(err) => return lash_core::ToolResult::err_fmt(err),
                };
                if let Some(tx) = self
                    .completion_key_tx
                    .lock()
                    .expect("completion key sender")
                    .take()
                {
                    let _ = tx.send(Ok(key));
                }
                lash_core::ToolResult::pending(lash_core::PendingCompletion::new())
            }
            other => lash_core::ToolResult::err_fmt(format!("unknown replay tool `{other}`")),
        }
    }
}

#[tokio::test]
async fn restate_replay_does_not_reexecute_scalar_lashlang_tool_before_pending_wait() {
    let dir = tempfile::tempdir().expect("tempdir");
    let session_id = "restate-scalar-lashlang-replay";
    let turn_id = "restate-scalar-lashlang-turn-1";
    let scalar_invocations = Arc::new(AtomicUsize::new(0));
    let (completion_key_tx, completion_key_rx) = tokio::sync::oneshot::channel();
    let tools: Arc<dyn lash_core::ToolProvider> = Arc::new(ReplayScalarPendingTools {
        scalar_invocations: Arc::clone(&scalar_invocations),
        completion_key_tx: Mutex::new(Some(completion_key_tx)),
    });
    let tool_plugin: Arc<dyn lash_core::PluginFactory> =
        Arc::new(lash_core::plugin::StaticPluginFactory::new(
            "restate-scalar-replay-tools",
            lash_core::PluginSpec::new().with_tool_provider(tools),
        ));
    let artifact_store: Arc<dyn lashlang::LashlangArtifactStore> =
        Arc::new(lashlang::InMemoryLashlangArtifactStore::new());
    let rlm_plugin: Arc<dyn lash_core::PluginFactory> = Arc::new(
        lash_protocol_rlm::RlmProtocolPluginFactory::new(
            lash_protocol_rlm::RlmProtocolPluginConfig::default(),
            Arc::clone(&artifact_store),
        )
        .with_process_lifecycle(true),
    );
    let plugin_factories = vec![rlm_plugin, tool_plugin];
    let llm_provider_calls = Arc::new(AtomicUsize::new(0));
    let provider = lash_core::testing::TestProvider::builder()
        .kind("stub")
        .complete({
            let llm_provider_calls = Arc::clone(&llm_provider_calls);
            move |_request| {
                let llm_provider_calls = Arc::clone(&llm_provider_calls);
                async move {
                    llm_provider_calls.fetch_add(1, Ordering::SeqCst);
                    let source = r#"<lashlang>
process replay_probe(tools: Tools) {
  counted = await tools.replay_scalar_counter({})?
  resumed = await tools.replay_pending_input({})?
  finish { counted: counted.value, answer: resumed.answer }
}
handle = start replay_probe(tools: tools)
finish (await handle)?
</lashlang>"#;
                    Ok(lash_core::LlmResponse {
                        full_text: source.to_string(),
                        parts: vec![lash_core::LlmOutputPart::Text {
                            text: source.to_string(),
                            response_meta: None,
                        }],
                        response_metadata: Default::default(),
                        ..lash_core::LlmResponse::default()
                    })
                }
            }
        })
        .build()
        .into_handle();
    let mut host = lash_core::RuntimeHostConfig::in_memory();
    host.providers.provider_resolver = Arc::new(lash_core::SingleProviderResolver::new(provider));
    host.durability.attachment_store = Arc::new(lash_core::SessionAttachmentStore::ephemeral(
        Arc::new(DurableMemoryAttachmentStore::default()),
    ));
    let process_env_store: Arc<dyn lash_core::ProcessExecutionEnvStore> =
        Arc::new(DurableMemoryProcessEnvStore::default());
    host.durability.process_env_store = Arc::clone(&process_env_store);
    host = host.with_process_engine(Arc::new(lash_lashlang_runtime::LashlangProcessEngine::new(
        Arc::clone(&artifact_store),
        lash_lashlang_runtime::LashlangSurface::default(),
    )));
    let store = Arc::new(
        lash_sqlite_store::Store::open(&dir.path().join("session.db"))
            .await
            .expect("open session store"),
    );
    let runtime_store: Arc<dyn lash_core::RuntimePersistence> = store;
    let policy = replay_test_policy(session_id);
    let initial_state = replay_test_state(session_id, &policy);
    let context = Arc::new(ReplayableRecordingContext::default());
    let process_registry = process_registry();
    let process_worker = DurableProcessWorker::new(lash_core::DurableProcessWorkerConfig::new(
        Arc::new(lash_core::PluginHost::new(plugin_factories.clone())),
        host.clone(),
        Arc::new(lash_core::InMemorySessionStoreFactory::new()),
        Arc::clone(&process_registry),
    ));
    context.install_process_worker(process_worker);

    let mut first = Box::pin(replay_test_runtime_with_plugins_and_registry(
        session_id,
        policy.clone(),
        initial_state.clone(),
        host.clone(),
        Arc::clone(&runtime_store),
        plugin_factories.clone(),
        Some(Arc::clone(&process_registry)),
    ))
    .await;
    let first_context = Arc::clone(&context);
    let first_turn = tokio::spawn(async move {
        run_restate_replay_turn(&mut first, first_context, session_id, turn_id).await
    });
    let completion_key = completion_key_rx
        .await
        .expect("pending tool must publish its completion key")
        .expect("pending tool must obtain its completion key");
    let resolver = RestateRuntimeEffectController::new(Arc::clone(&context));
    assert_eq!(
        resolver
            .resolve_await_event(
                &completion_key,
                Resolution::Ok(serde_json::json!({ "answer": "resumed" })),
            )
            .await
            .expect("resolve pending replay-test tool"),
        ResolveOutcome::Accepted
    );
    let first_turn = first_turn.await.expect("first turn task");
    assert!(matches!(
        first_turn.outcome,
        lash_core::TurnOutcome::Finished(_)
    ));
    assert_eq!(scalar_invocations.load(Ordering::SeqCst), 1);
    let first_recorded_envelopes = context.recorded_runtime_effect_envelopes();
    let scalar_tool_attempts = first_recorded_envelopes
        .iter()
        .filter(|(_, envelope)| {
            matches!(
                &envelope.command,
                RuntimeEffectCommand::ToolAttempt { call, .. }
                    if call.tool_name == "replay_scalar_counter"
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        scalar_tool_attempts.len(),
        1,
        "the production Lashlang tool caller must emit one journaled ToolAttempt envelope"
    );
    let (scalar_effect_name, scalar_envelope) = scalar_tool_attempts[0];
    assert_eq!(
        scalar_envelope.invocation.effect_kind(),
        Some(RuntimeEffectKind::ToolAttempt)
    );
    let RuntimeEffectCommand::ToolAttempt {
        call,
        attempt,
        max_attempts,
        ..
    } = &scalar_envelope.command
    else {
        unreachable!("filtered to the scalar ToolAttempt");
    };
    assert_eq!(call.tool_name, "replay_scalar_counter");
    assert_eq!((*attempt, *max_attempts), (1, 1));
    assert_eq!(
        scalar_effect_name,
        &restate_effect_name(&scalar_envelope.invocation),
        "the real journaling host must derive its run identity from the caller-emitted envelope"
    );
    let scalar_envelope_hash = scalar_envelope.stable_hash().expect("scalar envelope hash");
    let recorded_effect_count = first_recorded_envelopes.len();

    context
        .events
        .reset_invocation_state_for_replay_preserving_durable_event(
            &RestateDurableWaitAddress::for_key(&completion_key).workflow_key,
        );
    context.start_replay();
    let retry_store: Arc<dyn lash_core::RuntimePersistence> =
        Arc::new(CommitRetryStore::new(Arc::clone(&runtime_store)));
    let mut replay = Box::pin(replay_test_runtime_with_plugins_and_registry(
        session_id,
        policy,
        initial_state,
        host,
        retry_store,
        plugin_factories,
        Some(process_registry),
    ))
    .await;
    let replay_turn =
        run_restate_replay_turn(&mut replay, Arc::clone(&context), session_id, turn_id).await;
    assert!(matches!(
        replay_turn.outcome,
        lash_core::TurnOutcome::Finished(_)
    ));
    assert_eq!(llm_provider_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        scalar_invocations.load(Ordering::SeqCst),
        1,
        "Restate replay must return the journaled scalar ToolAttempt instead of re-executing the provider"
    );
    let replayed_envelopes = context.recorded_runtime_effect_envelopes();
    assert_eq!(
        replayed_envelopes.len(),
        recorded_effect_count,
        "replay must consume the journal rather than append another ToolAttempt record"
    );
    let replayed_scalar = replayed_envelopes
        .iter()
        .find(|(_, envelope)| {
            matches!(
                &envelope.command,
                RuntimeEffectCommand::ToolAttempt { call, .. }
                    if call.tool_name == "replay_scalar_counter"
            )
        })
        .expect("replayed scalar ToolAttempt envelope");
    assert_eq!(
        replayed_scalar
            .1
            .stable_hash()
            .expect("replayed scalar envelope hash"),
        scalar_envelope_hash,
        "the caller must reconstruct the same ToolAttempt envelope on replay"
    );
    assert_eq!(
        context
            .runs()
            .iter()
            .filter(|effect_name| *effect_name == scalar_effect_name)
            .count(),
        2,
        "the production caller must cross the journaling host once live and once on replay"
    );
}

#[tokio::test]
async fn restate_controller_schedules_process_workflow_without_running_executor() {
    let context = Arc::new(RecordingContext::default());
    let host = RestateRuntimeEffectController::new(context.clone());
    let registry = process_registry();
    let registration = external_registration("task-1");
    let outcome = host
        .execute_effect(
            RuntimeEffectEnvelope::new(
                runtime_invocation(RuntimeEffectKind::Process, "background-start"),
                RuntimeEffectCommand::process(ProcessCommand::Start {
                    registration,
                    grant: Some(lash_core::ProcessStartGrant {
                        session_scope: lash_core::SessionScope::new("session"),
                        descriptor: lash_core::ProcessHandleDescriptor::new(
                            Some("tool"),
                            Some("task"),
                        ),
                    }),
                    execution_context: Box::new(ProcessExecutionContext::default()),
                }),
            ),
            RuntimeEffectLocalExecutor::processes(registry.clone(), None),
        )
        .await
        .expect("start");
    let RuntimeEffectOutcome::Process {
        result: ProcessEffectOutcome::Start { record },
    } = outcome
    else {
        panic!("wrong outcome");
    };

    assert_eq!(
        record
            .external_ref
            .as_ref()
            .map(|external| external.id.as_str()),
        Some("LashProcessWorkflow/task-1")
    );
    assert_eq!(
        registry
            .get_process("task-1")
            .await
            .expect("get")
            .external_ref
            .as_ref()
            .map(|external| external.id.as_str()),
        Some("LashProcessWorkflow/task-1")
    );
    let session_scope = lash_core::SessionScope::new("session");
    assert_eq!(
        registry
            .list_handle_grants(&session_scope)
            .await
            .expect("grants")
            .into_iter()
            .next()
            .and_then(|(_, record)| record.external_ref)
            .map(|external| (
                external.backend,
                external
                    .metadata
                    .and_then(|metadata| metadata.get("invocation_id").cloned())
            )),
        Some((
            "restate".to_string(),
            Some(serde_json::json!("invocation-task-1"))
        ))
    );
    assert_eq!(
        context
            .started
            .lock()
            .expect("started lock")
            .iter()
            .map(|registration| registration.id.as_str())
            .collect::<Vec<_>>(),
        vec!["task-1"]
    );
    assert!(
        context.runs.lock().expect("runs lock").is_empty(),
        "process workflow scheduling must not call Restate context from inside ctx.run"
    );
}

#[tokio::test]
async fn restate_controller_replays_process_start_await_command_sequence() {
    let context = Arc::new(RecordingContext::default());
    let host = RestateRuntimeEffectController::new(context.clone());
    let registry = process_registry();
    let process_id = "task-start-await-replay";

    let start = || {
        RuntimeEffectEnvelope::new(
            runtime_invocation(RuntimeEffectKind::Process, "process-start-replay"),
            RuntimeEffectCommand::process(ProcessCommand::Start {
                registration: external_registration(process_id),
                grant: None,
                execution_context: Box::new(ProcessExecutionContext::default()),
            }),
        )
    };
    let await_terminal = || {
        RuntimeEffectEnvelope::new(
            runtime_invocation(RuntimeEffectKind::Process, "process-await-replay"),
            RuntimeEffectCommand::process(ProcessCommand::Await {
                process_id: process_id.to_string(),
            }),
        )
    };
    let terminal = ProcessAwaitOutput::Success {
        value: serde_json::json!({ "done": true }),
        control: None,
    };

    host.execute_effect(
        start(),
        RuntimeEffectLocalExecutor::processes(registry.clone(), None),
    )
    .await
    .expect("first start");
    registry
        .complete_process(
            process_id,
            terminal.clone(),
            lash_core::ProcessCompletionAuthority::external_owner(),
        )
        .await
        .expect("complete child process");
    context.resolve_process_terminal(process_id, &terminal);
    host.execute_effect(
        await_terminal(),
        RuntimeEffectLocalExecutor::processes(registry.clone(), None),
    )
    .await
    .expect("first await");

    // Simulates Restate replay of the same parent handler after a later
    // suspension resumes. The already persisted registry record has an
    // external_ref at this point, but the handler must still issue the same
    // Restate send before the await call so the journal command sequence stays
    // send -> call -> ... on every replay.
    host.execute_effect(
        start(),
        RuntimeEffectLocalExecutor::processes(registry.clone(), None),
    )
    .await
    .expect("replay start");
    host.execute_effect(
        await_terminal(),
        RuntimeEffectLocalExecutor::processes(registry.clone(), None),
    )
    .await
    .expect("replay await");

    assert_eq!(
        context
            .process_command_log
            .lock()
            .expect("process command log lock")
            .as_slice(),
        &[
            format!("send:{process_id}"),
            format!("call:{process_id}"),
            format!("send:{process_id}"),
            format!("call:{process_id}"),
        ],
        "child process start/await must replay the same Restate command sequence"
    );
}

#[tokio::test]
async fn restate_controller_start_emits_send_when_external_ref_already_exists() {
    let context = Arc::new(RecordingContext::default());
    let host = RestateRuntimeEffectController::new(context.clone());
    let registry = process_registry();
    let process_id = "task-start-existing-ref";
    let registration = external_registration(process_id);
    registry
        .register_process(registration.clone())
        .await
        .expect("register process");
    registry
        .set_external_ref(
            process_id,
            ProcessExternalRef {
                backend: "restate".to_string(),
                id: format!("LashProcessWorkflow/{process_id}"),
                metadata: Some(serde_json::json!({
                    "invocation_id": format!("invocation-{process_id}")
                })),
            },
        )
        .await
        .expect("pre-set external ref");

    host.execute_effect(
        RuntimeEffectEnvelope::new(
            runtime_invocation(RuntimeEffectKind::Process, "process-start-existing-ref"),
            RuntimeEffectCommand::process(ProcessCommand::Start {
                registration,
                grant: None,
                execution_context: Box::new(ProcessExecutionContext::default()),
            }),
        ),
        RuntimeEffectLocalExecutor::processes(registry, None),
    )
    .await
    .expect("start with existing external ref");

    assert_eq!(
        context
            .process_command_log
            .lock()
            .expect("process command log lock")
            .as_slice(),
        &[format!("send:{process_id}")],
        "pre-existing external_ref must not suppress the journaled Restate send"
    );
}

async fn run_parent_shaped_start_await_suspend_flow(
    host: &RestateRuntimeEffectController<'_, Arc<RecordingContext>>,
    registry: Arc<dyn ProcessRegistry>,
    process_id: &str,
    suspend_key: AwaitEventKey,
) {
    host.execute_effect(
        RuntimeEffectEnvelope::new(
            runtime_invocation(RuntimeEffectKind::Process, "parent-flow-start-child"),
            RuntimeEffectCommand::process(ProcessCommand::Start {
                registration: external_registration(process_id),
                grant: None,
                execution_context: Box::new(ProcessExecutionContext::default()),
            }),
        ),
        RuntimeEffectLocalExecutor::processes(registry.clone(), None),
    )
    .await
    .expect("parent flow start child");

    host.execute_effect(
        RuntimeEffectEnvelope::new(
            runtime_invocation(RuntimeEffectKind::Process, "parent-flow-await-child"),
            RuntimeEffectCommand::process(ProcessCommand::Await {
                process_id: process_id.to_string(),
            }),
        ),
        RuntimeEffectLocalExecutor::processes(registry, None),
    )
    .await
    .expect("parent flow await child");

    host.execute_effect(
        RuntimeEffectEnvelope::new(
            runtime_invocation(RuntimeEffectKind::AwaitEvent, "parent-flow-suspend"),
            RuntimeEffectCommand::AwaitEvent { key: suspend_key },
        ),
        RuntimeEffectLocalExecutor::await_event(tokio_util::sync::CancellationToken::new(), None),
    )
    .await
    .expect("parent flow await resume event");
}

#[tokio::test]
async fn restate_controller_replays_parent_shaped_start_await_suspend_flow() {
    let context = Arc::new(RecordingContext::default());
    let host = RestateRuntimeEffectController::new(context.clone());
    let registry = process_registry();
    let process_id = "task-parent-flow-replay";
    let terminal = ProcessAwaitOutput::Success {
        value: serde_json::json!({ "done": true }),
        control: None,
    };
    let suspend_key = restate_await_event_key(
        &ExecutionScope::process(process_id),
        AwaitEventWaitIdentity::Custom {
            key: "parent-resume-input".to_string(),
        },
    )
    .expect("parent suspend key");
    context.resolve_process_terminal(process_id, &terminal);
    context.resolve_durable_event(RestateDurableWaitResolveRequest {
        address: RestateDurableWaitAddress::for_key(&suspend_key),
        resolution: Resolution::Ok(serde_json::json!({ "answer": "resume" })),
    });

    run_parent_shaped_start_await_suspend_flow(
        &host,
        registry.clone(),
        process_id,
        suspend_key.clone(),
    )
    .await;
    run_parent_shaped_start_await_suspend_flow(&host, registry, process_id, suspend_key).await;

    assert_eq!(
        context
            .process_command_log
            .lock()
            .expect("process command log lock")
            .as_slice(),
        &[
            format!("send:{process_id}"),
            format!("call:{process_id}"),
            format!("send:{process_id}"),
            format!("call:{process_id}"),
        ],
        "a parent-shaped replay after suspension must preserve child start/await command order"
    );
}

#[tokio::test]
async fn restate_controller_schedules_lashlang_process_with_serializable_input() {
    let context = Arc::new(RecordingContext::default());
    let host = RestateRuntimeEffectController::new(context.clone());
    let registry = process_registry();
    let module = lashlang::parse("process scan(root: str) { finish root }")
        .expect("lashlang process module");
    let catalog = lashlang::LashlangHostCatalog::new();
    let linked_module = lashlang::LinkedModule::link(
        module.clone(),
        lashlang::LashlangHostEnvironment::new(catalog, lashlang::LashlangAbilities::all()),
    )
    .expect("link lashlang module");
    let process_ref = linked_module
        .artifact
        .process_ref("scan")
        .expect("scan process ref")
        .clone();
    let mut args = serde_json::Map::new();
    args.insert("root".to_string(), serde_json::json!("."));
    let registration = ProcessRegistration::new(
        "process-1",
        lashlang_process_input(lash_lashlang_runtime::LashlangProcessInput {
            module_ref: linked_module.module_ref.clone(),
            process_ref: process_ref.clone(),
            host_requirements_ref: linked_module.host_requirements_ref.clone(),
            process_name: "scan".to_string(),
            args: args.clone(),
        }),
        lash_core::RecoveryDisposition::Rerunnable,
        lash_core::ProcessProvenance::session(lash_core::SessionScope::new("session")),
    )
    .with_extra_event_types(lash_lashlang_runtime::lashlang_process_event_types())
    .with_execution_env_ref(Some(lash_core::ProcessExecutionEnvRef::new(
        "process-env:test:process-1",
    )))
    .with_wake_target(Some(lash_core::SessionScope::new("session")));

    let outcome = host
        .execute_effect(
            RuntimeEffectEnvelope::new(
                runtime_invocation(RuntimeEffectKind::Process, "lashlang-process-start"),
                RuntimeEffectCommand::process(ProcessCommand::Start {
                    registration,
                    grant: None,
                    execution_context: Box::new(ProcessExecutionContext::default()),
                }),
            ),
            RuntimeEffectLocalExecutor::processes(registry.clone(), None),
        )
        .await
        .expect("start");
    let RuntimeEffectOutcome::Process {
        result: ProcessEffectOutcome::Start { record },
    } = outcome
    else {
        panic!("wrong outcome");
    };

    assert_eq!(
        record
            .external_ref
            .as_ref()
            .map(|external| external.backend.as_str()),
        Some("restate")
    );
    assert_eq!(
        registry
            .get_process("process-1")
            .await
            .expect("registered process")
            .external_ref
            .as_ref()
            .map(|external| external.backend.as_str()),
        Some("restate")
    );
    let started = context.started.lock().expect("started lock").clone();
    assert_eq!(started.len(), 1);
    let ProcessInput::Engine { kind, payload } = started[0].input.as_ref() else {
        panic!("expected engine process input");
    };
    assert_eq!(kind, lash_lashlang_runtime::LASHLANG_ENGINE_KIND);
    let sent = lash_lashlang_runtime::LashlangProcessInput::from_payload(payload.clone())
        .expect("typed lashlang payload");
    assert_eq!(sent.module_ref, linked_module.module_ref);
    assert_eq!(sent.process_ref, process_ref);
    assert_eq!(
        sent.host_requirements_ref,
        linked_module.host_requirements_ref
    );
    assert_eq!(sent.process_name, "scan");
    assert_eq!(sent.args, args);
    assert_eq!(
        context
            .started
            .lock()
            .expect("started lock")
            .iter()
            .map(|registration| {
                registration
                    .wake_target
                    .as_ref()
                    .map(|scope| scope.session_id.as_str())
            })
            .collect::<Vec<_>>(),
        vec![Some("session")]
    );
}

#[tokio::test]
async fn restate_controller_lists_and_transfers_grants_through_process_effects() {
    let context = Arc::new(RecordingContext::default());
    let host = RestateRuntimeEffectController::new(context.clone());
    let registry = process_registry();
    let s1 = lash_core::SessionScope::new("s1");
    let s2 = lash_core::SessionScope::new("s2");
    registry
        .register_process(external_registration("task-list"))
        .await
        .expect("register");
    registry
        .grant_handle(
            &s1,
            "task-list",
            lash_core::ProcessHandleDescriptor::new(Some("tool"), Some("task")),
        )
        .await
        .expect("grant");

    let outcome = host
        .execute_effect(
            RuntimeEffectEnvelope::new(
                runtime_invocation(RuntimeEffectKind::Process, "process-list-s1"),
                RuntimeEffectCommand::process(ProcessCommand::List {
                    session_scope: s1.clone(),
                    mode: lash_core::ProcessListMode::Live,
                }),
            ),
            RuntimeEffectLocalExecutor::processes(registry.clone(), None),
        )
        .await
        .expect("list");
    let RuntimeEffectOutcome::Process {
        result: ProcessEffectOutcome::List { entries },
    } = outcome
    else {
        panic!("wrong list outcome");
    };
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].0.process_id, "task-list");

    let outcome = host
        .execute_effect(
            RuntimeEffectEnvelope::new(
                runtime_invocation(RuntimeEffectKind::Process, "process-transfer"),
                RuntimeEffectCommand::process(ProcessCommand::Transfer {
                    from_scope: s1.clone(),
                    to_scope: s2.clone(),
                    process_ids: vec!["task-list".to_string()],
                }),
            ),
            RuntimeEffectLocalExecutor::processes(registry.clone(), None),
        )
        .await
        .expect("transfer");
    assert!(matches!(
        outcome,
        RuntimeEffectOutcome::Process {
            result: ProcessEffectOutcome::Transfer
        }
    ));

    let entries = registry.list_handle_grants(&s2).await.expect("s2 grants");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].0.process_id, "task-list");
    assert!(
        registry
            .list_handle_grants(&s1)
            .await
            .expect("s1")
            .is_empty()
    );
    assert!(context.started.lock().expect("started lock").is_empty());
}

#[tokio::test]
async fn restate_controller_awaits_and_signals_through_process_effects() {
    let context = Arc::new(RecordingContext::default());
    let host = RestateRuntimeEffectController::new(context.clone());
    let registry = process_registry();
    registry
        .register_process(external_registration("task-await-signal"))
        .await
        .expect("register");
    registry
        .register_process(
            external_registration("task-signal")
                .with_extra_event_types(lash_lashlang_runtime::lashlang_process_event_types())
                .with_extra_event_types([lash_core::ProcessEventType {
                    name: "signal.notify".to_string(),
                    payload_schema: lash_core::LashSchema::any(),
                    semantics: lash_core::ProcessEventSemanticsSpec::default(),
                }]),
        )
        .await
        .expect("register signal target");
    let awaited_output = ProcessAwaitOutput::Success {
        value: serde_json::json!({ "done": true }),
        control: None,
    };
    registry
        .complete_process(
            "task-await-signal",
            awaited_output.clone(),
            lash_core::ProcessCompletionAuthority::external_owner(),
        )
        .await
        .expect("complete");
    context.resolve_process_terminal("task-await-signal", &awaited_output);

    let outcome = host
        .execute_effect(
            RuntimeEffectEnvelope::new(
                runtime_invocation(RuntimeEffectKind::Process, "process-await"),
                RuntimeEffectCommand::process(ProcessCommand::Await {
                    process_id: "task-await-signal".to_string(),
                }),
            ),
            RuntimeEffectLocalExecutor::processes(registry.clone(), None),
        )
        .await
        .expect("await");
    let RuntimeEffectOutcome::Process {
        result: ProcessEffectOutcome::Await { output },
    } = outcome
    else {
        panic!("wrong await outcome");
    };
    assert_eq!(
        *output,
        ProcessAwaitOutput::Success {
            value: serde_json::json!({ "done": true }),
            control: None,
        }
    );

    let outcome = host
        .execute_effect(
            RuntimeEffectEnvelope::new(
                runtime_invocation(RuntimeEffectKind::Process, "process-signal"),
                RuntimeEffectCommand::process(ProcessCommand::Signal {
                    process_id: "task-signal".to_string(),
                    signal_name: "notify".to_string(),
                    signal_id: "notify".to_string(),
                    request: lash_core::ProcessEventAppendRequest::new(
                        "signal.notify",
                        serde_json::json!({ "signal": "notify" }),
                    )
                    .with_replay_key("signal:notify"),
                }),
            ),
            RuntimeEffectLocalExecutor::processes(registry.clone(), None),
        )
        .await
        .expect("signal");
    let RuntimeEffectOutcome::Process {
        result: ProcessEffectOutcome::Signal { event },
    } = outcome
    else {
        panic!("wrong signal outcome");
    };
    assert_eq!(event.event_type, "signal.notify");
    assert!(context.started.lock().expect("started lock").is_empty());

    // Append-before-resolve discipline: the durable event is the record, the
    // promise resolution is only the wake-up, keyed by the Nth occurrence of
    // this signal name so repeated signals map onto one-shot engine promises.
    {
        let resolved = context.resolved_events.lock().expect("resolved lock");
        assert_eq!(resolved.len(), 1);
        let expected_key = restate_await_event_key(
            &ExecutionScope::process("task-signal"),
            AwaitEventWaitIdentity::process_signal("task-signal", "notify", 1),
        )
        .expect("first signal wait key");
        assert_eq!(
            resolved[0].address,
            RestateDurableWaitAddress::for_key(&expected_key)
        );
        assert_eq!(
            resolved[0].resolution,
            Resolution::Ok(serde_json::json!({ "signal": "notify" }))
        );
    }

    let outcome = host
        .execute_effect(
            RuntimeEffectEnvelope::new(
                runtime_invocation(RuntimeEffectKind::Process, "process-signal-2"),
                RuntimeEffectCommand::process(ProcessCommand::Signal {
                    process_id: "task-signal".to_string(),
                    signal_name: "notify".to_string(),
                    signal_id: "notify-2".to_string(),
                    request: lash_core::ProcessEventAppendRequest::new(
                        "signal.notify",
                        serde_json::json!({ "signal": "notify-2" }),
                    )
                    .with_replay_key("signal:notify-2"),
                }),
            ),
            RuntimeEffectLocalExecutor::processes(registry.clone(), None),
        )
        .await
        .expect("second signal");
    let RuntimeEffectOutcome::Process {
        result: ProcessEffectOutcome::Signal { .. },
    } = outcome
    else {
        panic!("wrong second signal outcome");
    };
    let resolved = context.resolved_events.lock().expect("resolved lock");
    assert_eq!(resolved.len(), 2);
    let expected_key = restate_await_event_key(
        &ExecutionScope::process("task-signal"),
        AwaitEventWaitIdentity::process_signal("task-signal", "notify", 2),
    )
    .expect("second signal wait key");
    assert_eq!(
        resolved[1].address,
        RestateDurableWaitAddress::for_key(&expected_key),
        "second signal must resolve the ordinal-2 wait key"
    );
}

#[tokio::test]
async fn restate_controller_cancel_requests_call_workflow_cancel() {
    let context = Arc::new(RecordingContext::default());
    let host = RestateRuntimeEffectController::new(context.clone());
    let registry = process_registry();
    let registration = external_registration("task-cancel");
    registry
        .register_process(registration)
        .await
        .expect("register");

    let outcome = host
        .execute_effect(
            RuntimeEffectEnvelope::new(
                runtime_invocation(RuntimeEffectKind::Process, "background-cancel"),
                RuntimeEffectCommand::process(ProcessCommand::Cancel {
                    process_id: "task-cancel".to_string(),
                    reason: Some("user requested".to_string()),
                }),
            ),
            RuntimeEffectLocalExecutor::processes(registry, None),
        )
        .await
        .expect("cancel");
    let RuntimeEffectOutcome::Process {
        result: ProcessEffectOutcome::Cancel { record },
    } = outcome
    else {
        panic!("wrong outcome");
    };

    assert!(!record.is_terminal());
    assert_eq!(
        context.cancelled.lock().expect("cancelled lock").as_slice(),
        &[(
            "task-cancel".to_string(),
            Some("user requested".to_string())
        )]
    );
}

#[derive(Debug, PartialEq, Eq)]
struct RecordedProcessRun {
    process_id: String,
    wake_target_session_id: Option<String>,
    tool_effect_id: Option<String>,
    execution_scope_id: String,
    controller_tier: lash_core::DurabilityTier,
}

#[derive(Default)]
struct RecordingRunner {
    ran: Mutex<Vec<RecordedProcessRun>>,
    cancelled: Mutex<Vec<RestateProcessCancelRequest>>,
}

#[async_trait::async_trait]
impl RestateProcessRunner for RecordingRunner {
    async fn run_process_segment(
        &self,
        registration: ProcessRegistration,
        execution_context: ProcessExecutionContext,
        scoped_effect_controller: lash_core::ScopedEffectController<'_>,
        _handover: Option<lash_core::SegmentHandover>,
        _cancellation: tokio_util::sync::CancellationToken,
    ) -> Result<lash_core::ProcessRunOutcome, PluginError> {
        self.ran
            .lock()
            .expect("runner ran lock")
            .push(RecordedProcessRun {
                process_id: registration.id.clone(),
                wake_target_session_id: registration
                    .wake_target
                    .as_ref()
                    .map(|scope| scope.session_id.clone()),
                tool_effect_id: execution_context
                    .causal_invocation
                    .and_then(|invocation| invocation.effect_id().map(str::to_string)),
                execution_scope_id: scoped_effect_controller.scope_id().to_string(),
                controller_tier: scoped_effect_controller.controller().durability_tier(),
            });
        Ok(ProcessAwaitOutput::Success {
            value: serde_json::json!({"ok": true}),
            control: None,
        }
        .into())
    }

    async fn request_process_cancel(
        &self,
        request: RestateProcessCancelRequest,
    ) -> Result<(), PluginError> {
        self.cancelled
            .lock()
            .expect("runner cancelled lock")
            .push(request);
        Ok(())
    }
}

struct AlreadyStartedRunner {
    calls: Mutex<usize>,
    winner: lash_core::LeaseOwnerIdentity,
}

#[async_trait::async_trait]
impl RestateProcessRunner for AlreadyStartedRunner {
    async fn run_process_segment(
        &self,
        registration: ProcessRegistration,
        _execution_context: ProcessExecutionContext,
        _scoped_effect_controller: lash_core::ScopedEffectController<'_>,
        _handover: Option<lash_core::SegmentHandover>,
        _cancellation: tokio_util::sync::CancellationToken,
    ) -> Result<lash_core::ProcessRunOutcome, PluginError> {
        *self.calls.lock().expect("runner calls lock") += 1;
        Err(PluginError::ProcessAlreadyStarted {
            process_id: registration.id,
            by: Box::new(self.winner.clone()),
        })
    }

    async fn request_process_cancel(
        &self,
        _request: RestateProcessCancelRequest,
    ) -> Result<(), PluginError> {
        Ok(())
    }
}

struct SegmentedRecordingRunner {
    outcomes: Mutex<VecDeque<lash_core::ProcessRunOutcome>>,
    handovers: Mutex<Vec<Option<lash_core::SegmentHandover>>>,
    runs: AtomicUsize,
}

#[derive(Default)]
struct CancellationAwareRunner {
    started: tokio::sync::Notify,
    finish_successfully: tokio::sync::Notify,
}

#[derive(Debug, Default)]
struct BlockingCancelSignalTransport {
    requests: Mutex<Vec<HttpRequest>>,
    started: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

#[async_trait::async_trait]
impl HttpTransport for BlockingCancelSignalTransport {
    async fn send(
        &self,
        request: HttpRequest,
        _timeout: Option<Duration>,
    ) -> Result<HttpResponse, HttpTransportError> {
        self.requests.lock().expect("requests lock").push(request);
        self.started.notify_one();
        self.release.notified().await;
        Ok(HttpResponse {
            status: 200,
            headers: vec![("content-type".to_string(), "application/json".to_string())],
            body: HttpResponseBody::buffered(r#""cancel_requested""#),
        })
    }
}

#[async_trait::async_trait]
impl RestateProcessRunner for CancellationAwareRunner {
    async fn run_process_segment(
        &self,
        _registration: ProcessRegistration,
        _execution_context: ProcessExecutionContext,
        _scoped_effect_controller: lash_core::ScopedEffectController<'_>,
        _handover: Option<lash_core::SegmentHandover>,
        cancellation: tokio_util::sync::CancellationToken,
    ) -> Result<lash_core::ProcessRunOutcome, PluginError> {
        self.started.notify_one();
        tokio::select! {
            _ = cancellation.cancelled() => Ok(ProcessAwaitOutput::Cancelled {
                message: "cancel signal observed".to_string(),
                raw: None,
                control: None,
            }
            .into()),
            _ = self.finish_successfully.notified() => Ok(ProcessAwaitOutput::Success {
                value: serde_json::json!("runner completed"),
                control: None,
            }
            .into()),
        }
    }

    async fn request_process_cancel(
        &self,
        _request: RestateProcessCancelRequest,
    ) -> Result<(), PluginError> {
        Ok(())
    }
}

#[async_trait::async_trait]
impl RestateProcessRunner for SegmentedRecordingRunner {
    async fn run_process_segment(
        &self,
        _registration: ProcessRegistration,
        _execution_context: ProcessExecutionContext,
        _scoped_effect_controller: lash_core::ScopedEffectController<'_>,
        handover: Option<lash_core::SegmentHandover>,
        _cancellation: tokio_util::sync::CancellationToken,
    ) -> Result<lash_core::ProcessRunOutcome, PluginError> {
        self.runs.fetch_add(1, Ordering::SeqCst);
        self.handovers
            .lock()
            .expect("segment handovers lock")
            .push(handover);
        self.outcomes
            .lock()
            .expect("segment outcomes lock")
            .pop_front()
            .ok_or_else(|| PluginError::Session("unexpected duplicate segment run".to_string()))
    }

    async fn request_process_cancel(
        &self,
        _request: RestateProcessCancelRequest,
    ) -> Result<(), PluginError> {
        Ok(())
    }
}

fn inline_process_scope(process_id: &str) -> lash_core::ScopedEffectController<'static> {
    lash_core::ScopedEffectController::shared(
        Arc::new(lash_core::InlineRuntimeEffectController::default()),
        lash_core::ExecutionScope::process(process_id.to_string()),
    )
    .expect("inline process scope")
}

async fn pending_process_cancel_signal() -> Result<(), HandlerError> {
    std::future::pending().await
}

#[tokio::test]
async fn running_process_cancel_uses_native_signal_without_poll_delay() {
    let runner = Arc::new(CancellationAwareRunner::default());
    let registry = process_registry();
    let signal_transport = Arc::new(BlockingCancelSignalTransport::default());
    let cancel_ingress = RestateIngressClient::new(RestateConnection::with_transport(
        "https://restate.invalid",
        signal_transport.clone(),
    ));
    let workflow = Arc::new(LashProcessWorkflowImpl::new(
        Arc::clone(&runner),
        Arc::clone(&registry),
        cancel_ingress,
    ));
    let registration = rerunnable_registration("prompt-cancel");
    registry
        .register_process(registration.clone())
        .await
        .expect("register process");

    let run = {
        let workflow = Arc::clone(&workflow);
        tokio::spawn(async move {
            let cancellation_signal = workflow.cancellation_signal("prompt-cancel", 0);
            workflow
                .run_registration(
                    registration,
                    ProcessExecutionContext::default(),
                    inline_process_scope("prompt-cancel"),
                    0,
                    None,
                    cancellation_signal,
                )
                .await
        })
    };
    runner.started.notified().await;
    signal_transport.started.notified().await;
    registry
        .append_event(
            "prompt-cancel",
            lash_core::ProcessEventAppendRequest::cancel_requested(
                "prompt-cancel",
                Some("stop promptly".to_string()),
            ),
        )
        .await
        .expect("append cancel request");
    signal_transport.release.notify_one();

    let outcome = tokio::time::timeout(Duration::from_millis(20), run)
        .await
        .expect("accepted cancel must not wait for the 25ms polling floor")
        .expect("join running process")
        .expect("run process");
    assert!(matches!(
        outcome,
        lash_core::ProcessRunOutcome::Terminal(output)
            if matches!(*output, ProcessAwaitOutput::Cancelled { .. })
    ));
    let requests = signal_transport.requests.lock().expect("requests lock");
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].url,
        "https://restate.invalid/LashProcessWorkflow/prompt-cancel/await_cancel"
    );
}

#[tokio::test]
async fn transient_cancel_registry_read_error_cannot_fall_through_to_success() {
    let runner = Arc::new(CancellationAwareRunner::default());
    let registry = process_registry();
    let workflow = Arc::new(LashProcessWorkflowImpl::new_for_test(
        Arc::clone(&runner),
        Arc::clone(&registry),
    ));
    let registration = rerunnable_registration("transient-cancel-read");
    registry
        .register_process(registration.clone())
        .await
        .expect("register process");

    let (signal, cancellation_signal) = tokio::sync::oneshot::channel();
    let run = {
        let workflow = Arc::clone(&workflow);
        tokio::spawn(async move {
            workflow
                .run_registration(
                    registration,
                    ProcessExecutionContext::default(),
                    inline_process_scope("transient-cancel-read"),
                    0,
                    None,
                    async move {
                        cancellation_signal.await.map_err(|_| {
                            HandlerError::from(TerminalError::new(
                                "test cancellation signal sender dropped",
                            ))
                        })
                    },
                )
                .await
        })
    };
    runner.started.notified().await;
    registry
        .append_event(
            "transient-cancel-read",
            lash_core::ProcessEventAppendRequest::cancel_requested(
                "transient-cancel-read",
                Some("retry the durable read".to_string()),
            ),
        )
        .await
        .expect("append cancel request");
    workflow.fail_next_cancel_reads(1);
    signal
        .send(())
        .expect("resolve process cancellation signal");
    runner.finish_successfully.notify_one();

    let outcome = tokio::time::timeout(Duration::from_millis(100), run)
        .await
        .expect("transient registry error must be retried")
        .expect("join running process")
        .expect("run process");
    assert!(matches!(
        outcome,
        lash_core::ProcessRunOutcome::Terminal(output)
            if matches!(*output, ProcessAwaitOutput::Cancelled { .. })
    ));
}

#[tokio::test]
async fn durable_segment_handover_resumes_once_and_terminalizes_once() {
    let continuation = lash_core::SegmentHandover {
        reason: lash_core::BoundaryReason::JournalBudget,
        program_hash: Some("program-v1".to_string()),
        engine_state: vec![1, 2, 3],
    };
    let terminal = ProcessAwaitOutput::Success {
        value: serde_json::json!({"result": 42}),
        control: None,
    };
    let runner = Arc::new(SegmentedRecordingRunner {
        outcomes: Mutex::new(VecDeque::from([
            lash_core::ProcessRunOutcome::SegmentBoundary(continuation.clone()),
            terminal.clone().into(),
        ])),
        handovers: Mutex::new(Vec::new()),
        runs: AtomicUsize::new(0),
    });
    let registry = process_registry();
    let workflow = LashProcessWorkflowImpl::new_for_test(runner.clone(), registry.clone());
    let registration = rerunnable_registration("segmented-durable");
    let _record = registry
        .register_process(registration.clone())
        .await
        .expect("register segmented process");
    let first_context = Arc::new(ReplayableRecordingContext::default());
    let first_controller = RestateRuntimeEffectController::new(first_context.clone());

    let first = workflow
        .run_registration(
            registration.clone(),
            ProcessExecutionContext::default(),
            first_controller
                .scoped_effect_controller(ExecutionScope::process("segmented-durable"))
                .expect("durable first-segment scope"),
            0,
            None,
            pending_process_cancel_signal(),
        )
        .await
        .expect("run first segment");
    let lash_core::ProcessRunOutcome::SegmentBoundary(first_handover) = first else {
        panic!("first incarnation must end at a segment boundary");
    };
    let persisted = lash_core::PersistedSegmentHandover {
        segment_ordinal: 1,
        program_hash: "program-v1".to_string(),
        handover: first_handover,
    };
    registry
        .put_segment_handover("segmented-durable", persisted.clone())
        .await
        .expect("persist before successor schedule");
    registry
        .put_segment_handover("segmented-durable", persisted.clone())
        .await
        .expect("crash recovery repeats the persist idempotently");
    assert_eq!(
        process_segment_workflow_key("segmented-durable", 1),
        "segmented-durable#1"
    );

    let loaded = registry
        .get_segment_handover("segmented-durable", 1)
        .await
        .expect("load successor handover")
        .expect("persisted successor handover");
    let resumed = validate_segment_program_hash("segmented-durable", loaded)
        .expect("matching program identity");
    first_context.start_replay();
    let successor_context = Arc::new(ReplayableRecordingContext::default());
    let successor_controller = RestateRuntimeEffectController::new(successor_context);
    let second = workflow
        .run_registration(
            registration,
            ProcessExecutionContext::default(),
            successor_controller
                .scoped_effect_controller(ExecutionScope::process("segmented-durable"))
                .expect("durable successor scope"),
            1,
            Some(resumed),
            pending_process_cancel_signal(),
        )
        .await
        .expect("run successor segment");
    assert!(matches!(second, lash_core::ProcessRunOutcome::Terminal(_)));
    assert_eq!(runner.runs.load(Ordering::SeqCst), 2);
    assert_eq!(
        runner
            .handovers
            .lock()
            .expect("segment handovers lock")
            .as_slice(),
        &[None, Some(continuation)]
    );
    let events = registry
        .events_after("segmented-durable", 0)
        .await
        .expect("process events");
    assert_eq!(
        events
            .iter()
            .filter(|event| event.semantics.terminal.is_some())
            .count(),
        1,
        "only the true terminal is process-visible"
    );
    let awaited = lash_core::ProcessAwaiter::polling(registry)
        .await_terminal("segmented-durable")
        .await
        .expect("await true terminal");
    assert_eq!(awaited, terminal);
}

#[derive(Clone, Copy, Debug)]
enum RestateSegmentReplayPoint {
    GetHandover,
    PutHandover,
    CancelCheck,
}

fn restate_segment_tool_attempt_outcome(ordinal: u64) -> RuntimeEffectOutcome {
    RuntimeEffectOutcome::ToolAttempt {
        launch: Box::new(lash_core::ToolAttemptLaunch::Done {
            record: Box::new(completed_tool_record(
                &format!("matrix-call-{ordinal}"),
                "matrix_tool",
            )),
        }),
        triggers: Vec::new(),
    }
}

#[tokio::test]
async fn restate_segment_transition_replay_matrix_preserves_lineage_invariants() {
    for replay_point in [
        RestateSegmentReplayPoint::GetHandover,
        RestateSegmentReplayPoint::PutHandover,
        RestateSegmentReplayPoint::CancelCheck,
    ] {
        let process_id = format!("matrix-{replay_point:?}").to_ascii_lowercase();
        let registry = process_registry();
        let registration = rerunnable_registration(&process_id);
        registry
            .register_process(registration.clone())
            .await
            .expect("register matrix process");
        let terminal = ProcessAwaitOutput::Success {
            value: serde_json::json!({"result": 99, "effects": [0, 1, 2]}),
            control: None,
        };
        let runner = Arc::new(SegmentedRecordingRunner {
            outcomes: Mutex::new(VecDeque::from([
                lash_core::ProcessRunOutcome::SegmentBoundary(lash_core::SegmentHandover {
                    reason: lash_core::BoundaryReason::JournalBudget,
                    program_hash: Some("matrix-program-v1".to_string()),
                    engine_state: vec![1],
                }),
                lash_core::ProcessRunOutcome::SegmentBoundary(lash_core::SegmentHandover {
                    reason: lash_core::BoundaryReason::JournalBudget,
                    program_hash: Some("matrix-program-v1".to_string()),
                    engine_state: vec![2],
                }),
                terminal.clone().into(),
            ])),
            handovers: Mutex::new(Vec::new()),
            runs: AtomicUsize::new(0),
        });
        let workflow =
            LashProcessWorkflowImpl::new_for_test(Arc::clone(&runner), Arc::clone(&registry));
        let mut input_handover = None;
        let mut successor_keys = HashSet::new();

        for ordinal in 0_u64..3 {
            let context = Arc::new(ReplayableRecordingContext::default());
            let controller = RestateRuntimeEffectController::with_options(
                Arc::clone(&context),
                RestateEffectControllerOptions::default().segment_effect_budget(1),
            );
            let local_calls = Arc::new(AtomicUsize::new(0));
            let envelope = RuntimeEffectEnvelope::new(
                RuntimeInvocation::effect(
                    lash_core::runtime::RuntimeScope::new(process_id.clone()),
                    format!("matrix-effect-{ordinal}"),
                    RuntimeEffectKind::ToolAttempt,
                    format!("matrix:{process_id}:{ordinal}"),
                ),
                RuntimeEffectCommand::ToolAttempt {
                    call: prepared_tool_call_with(&format!("matrix-call-{ordinal}"), "matrix_tool"),
                    execution_grant: None,
                    attempt: 1,
                    max_attempts: 1,
                },
            );
            let first_calls = Arc::clone(&local_calls);
            let first_effect = controller
                .execute_effect(
                    envelope.clone(),
                    RuntimeEffectLocalExecutor::testing(move |_| async move {
                        first_calls.fetch_add(1, Ordering::SeqCst);
                        Ok(restate_segment_tool_attempt_outcome(ordinal))
                    }),
                )
                .await
                .expect("first matrix effect");
            let progress = lash_core::SegmentProgress {
                effects_executed: 1,
                journaled_bytes_estimate: None,
            };
            assert_eq!(
                RuntimeEffectController::wants_segment_boundary(&controller, &progress),
                Some(lash_core::BoundaryReason::JournalBudget)
            );
            context.start_replay();
            let replay_calls = Arc::clone(&local_calls);
            let replay_effect = controller
                .execute_effect(
                    envelope,
                    RuntimeEffectLocalExecutor::testing(move |_| async move {
                        replay_calls.fetch_add(1, Ordering::SeqCst);
                        Ok(restate_segment_tool_attempt_outcome(ordinal))
                    }),
                )
                .await
                .expect("replayed matrix effect");
            assert_eq!(
                serde_json::to_value(&replay_effect).expect("serialize replay effect"),
                serde_json::to_value(&first_effect).expect("serialize first effect"),
                "replay effect identity"
            );
            assert_eq!(
                local_calls.load(Ordering::SeqCst),
                1,
                "handler replay must not double-execute an effect"
            );
            assert_eq!(
                RuntimeEffectController::wants_segment_boundary(&controller, &progress),
                Some(lash_core::BoundaryReason::JournalBudget),
                "replay must cut at the identical completed-effect budget"
            );

            if ordinal > 0 {
                let loaded = registry
                    .get_segment_handover(&process_id, ordinal)
                    .await
                    .expect("get input handover")
                    .expect("running segment input survives");
                if matches!(replay_point, RestateSegmentReplayPoint::GetHandover) {
                    assert_eq!(
                        registry
                            .get_segment_handover(&process_id, ordinal)
                            .await
                            .expect("replayed get"),
                        Some(loaded.clone())
                    );
                }
                input_handover = Some(
                    validate_segment_program_hash(&process_id, loaded)
                        .expect("valid matrix handover"),
                );
            }

            let run_once = workflow
                .run_registration(
                    registration.clone(),
                    ProcessExecutionContext::default(),
                    inline_process_scope(&process_id),
                    ordinal,
                    input_handover.take(),
                    pending_process_cancel_signal(),
                )
                .await
                .expect("matrix segment run");
            if ordinal == 2 {
                assert_eq!(run_once, terminal.clone().into());
                break;
            }
            let lash_core::ProcessRunOutcome::SegmentBoundary(boundary) = run_once else {
                panic!("matrix segment {ordinal} must request a boundary");
            };
            let next = ordinal + 1;
            let persisted = lash_core::PersistedSegmentHandover {
                segment_ordinal: next,
                program_hash: "matrix-program-v1".to_string(),
                handover: boundary,
            };
            registry
                .put_segment_handover(&process_id, persisted.clone())
                .await
                .expect("put matrix handover");
            if matches!(replay_point, RestateSegmentReplayPoint::PutHandover) {
                registry
                    .put_segment_handover(&process_id, persisted)
                    .await
                    .expect("replayed put is idempotent");
            }
            let cancel_checks = if matches!(replay_point, RestateSegmentReplayPoint::CancelCheck) {
                2
            } else {
                1
            };
            for _ in 0..cancel_checks {
                assert!(
                    !workflow
                        .process_cancel_requested(&process_id)
                        .await
                        .expect("matrix cancel check")
                );
            }
            let key = process_segment_workflow_key(&process_id, next);
            assert!(
                successor_keys.insert(key.clone()),
                "one successor per ordinal"
            );
        }

        assert_eq!(
            successor_keys.len(),
            2,
            "exactly one successor for each boundary"
        );
        assert_eq!(
            runner.runs.load(Ordering::SeqCst),
            3,
            "no duplicate incarnation"
        );
        registry
            .complete_process(
                &process_id,
                terminal.clone(),
                workflow_key_authority(&process_id),
            )
            .await
            .expect("write root terminal");
        let attach_after_retention = lash_core::ProcessAwaiter::polling(Arc::clone(&registry));
        assert_eq!(
            attach_after_retention
                .await_terminal(&process_id)
                .await
                .expect("post-retention durable attach"),
            terminal
        );
        assert_eq!(
            registry
                .events_after(&process_id, 0)
                .await
                .expect("matrix events")
                .iter()
                .filter(|event| event.semantics.terminal.is_some())
                .count(),
            1,
            "root terminal is durable and exactly once"
        );
    }
}

#[test]
fn missing_segment_handover_distinguishes_superseded_orphan_from_current_input() {
    let latest = lash_core::PersistedSegmentHandover {
        segment_ordinal: 4,
        program_hash: "program-v1".to_string(),
        handover: lash_core::SegmentHandover {
            reason: lash_core::BoundaryReason::JournalBudget,
            program_hash: Some("program-v1".to_string()),
            engine_state: vec![4],
        },
    };
    assert!(missing_segment_is_superseded(2, Some(&latest)));
    assert!(!missing_segment_is_superseded(4, Some(&latest)));
    assert!(!missing_segment_is_superseded(5, Some(&latest)));
    assert!(!missing_segment_is_superseded(1, None));
}

#[tokio::test]
async fn persisted_handover_is_change_feed_and_event_invariant() {
    let registry = process_registry();
    let _record = registry
        .register_process(rerunnable_registration("segment-invariant"))
        .await
        .expect("register process");
    let (_, cursor) = registry
        .processes_changed_since(lash_core::ProcessChangeCursor::default(), 10)
        .await
        .expect("initial change feed");
    registry
        .put_segment_handover(
            "segment-invariant",
            lash_core::PersistedSegmentHandover {
                segment_ordinal: 1,
                program_hash: "program-v1".to_string(),
                handover: lash_core::SegmentHandover {
                    reason: lash_core::BoundaryReason::DurationCap,
                    program_hash: Some("program-v1".to_string()),
                    engine_state: vec![9],
                },
            },
        )
        .await
        .expect("persist handover");
    let (changes, next_cursor) = registry
        .processes_changed_since(cursor, 10)
        .await
        .expect("change feed after handover");
    assert!(changes.is_empty());
    assert_eq!(next_cursor, cursor);
    assert!(
        registry
            .events_after("segment-invariant", 0)
            .await
            .expect("events")
            .is_empty()
    );
}

#[tokio::test]
async fn segment_program_hash_mismatch_is_typed_and_cancel_skips_successor_engine() {
    let mismatch = validate_segment_program_hash(
        "hash-bound",
        lash_core::PersistedSegmentHandover {
            segment_ordinal: 1,
            program_hash: "old-program".to_string(),
            handover: lash_core::SegmentHandover {
                reason: lash_core::BoundaryReason::JournalBudget,
                program_hash: Some("new-program".to_string()),
                engine_state: vec![],
            },
        },
    )
    .expect_err("changed program must fail closed");
    assert_eq!(
        mismatch.code,
        lash_core::RuntimeErrorCode::from("restate_segment_program_hash_mismatch")
    );

    let runner = Arc::new(SegmentedRecordingRunner {
        outcomes: Mutex::new(VecDeque::from([ProcessAwaitOutput::Success {
            value: serde_json::Value::Null,
            control: None,
        }
        .into()])),
        handovers: Mutex::new(Vec::new()),
        runs: AtomicUsize::new(0),
    });
    let registry = process_registry();
    let workflow = LashProcessWorkflowImpl::new_for_test(runner.clone(), registry.clone());
    let registration = rerunnable_registration("cancel-between-segments");
    registry
        .register_process(registration.clone())
        .await
        .expect("register process");
    registry
        .append_event(
            "cancel-between-segments",
            lash_core::ProcessEventAppendRequest::cancel_requested(
                "cancel-between-segments",
                Some("stop".to_string()),
            ),
        )
        .await
        .expect("cancel between segments");
    let outcome = workflow
        .run_registration(
            registration,
            ProcessExecutionContext::default(),
            inline_process_scope("cancel-between-segments"),
            1,
            Some(lash_core::SegmentHandover {
                reason: lash_core::BoundaryReason::JournalBudget,
                program_hash: Some("program-v1".to_string()),
                engine_state: vec![1],
            }),
            pending_process_cancel_signal(),
        )
        .await
        .expect("cancelled successor");
    assert!(matches!(
        outcome,
        lash_core::ProcessRunOutcome::Terminal(output)
            if matches!(*output, ProcessAwaitOutput::Cancelled { .. })
    ));
    assert_eq!(runner.runs.load(Ordering::SeqCst), 0);
}

#[test]
fn cancel_terminal_from_successor_routes_to_root_await_workflow_key() {
    assert_eq!(terminal_completion_workflow_key("process-1", 0), None);
    assert_eq!(
        terminal_completion_workflow_key("process-1", 1),
        Some("process-1".to_string())
    );
    assert_eq!(
        process_segment_workflow_key("process-1", 1),
        "process-1#1",
        "the running successor key must differ from the terminal await key"
    );
}

#[test]
fn boundary_registry_io_errors_are_retryable_handler_failures() {
    let error = retryable_registry_error(lash_core::PluginError::Session(
        "transient registry outage".to_string(),
    ));
    let debug = format!("{error:?}");
    assert!(
        debug.contains("Retryable"),
        "registry I/O must ask Restate to retry: {debug}"
    );
}

#[test]
fn boundary_with_armed_wait_is_declined_instead_of_terminalized() {
    let mut record = lash_core::ProcessRecord::from_registration(rerunnable_registration("wait"));
    record.wait = Some(lash_core::WaitState {
        since_ms: 1,
        kind: lash_core::WaitKind::Signal {
            name: "ready".to_string(),
            event_type: "signal.ready".to_string(),
            key: "process:wait:signal.ready:1".to_string(),
            ordinal: 1,
        },
    });
    assert!(boundary_must_be_declined(Some(&record)));
    record.wait = None;
    assert!(!boundary_must_be_declined(Some(&record)));
}

#[tokio::test]
async fn process_workflow_endpoint_smoke_schedules_runs_and_cancels_process() {
    let runner = Arc::new(RecordingRunner::default());
    let registry = process_registry();
    let endpoint = Endpoint::builder()
        .bind(LashProcessWorkflowImpl::new_for_test(runner.clone(), registry.clone()).serve())
        .build();
    let context = Arc::new(RecordingContext::with_endpoint(endpoint));
    let host = RestateRuntimeEffectController::new(context.clone());
    let registration = external_registration("task-smoke")
        .with_wake_target(Some(lash_core::SessionScope::new("wake-smoke")));
    let execution_context = ProcessExecutionContext::default().with_causal_invocation(Some(
        runtime_invocation(RuntimeEffectKind::ToolAttempt, "tool-smoke"),
    ));

    let outcome = host
        .execute_effect(
            RuntimeEffectEnvelope::new(
                runtime_invocation(RuntimeEffectKind::Process, "background-smoke-start"),
                RuntimeEffectCommand::process(ProcessCommand::Start {
                    registration,
                    grant: Some(lash_core::ProcessStartGrant {
                        session_scope: lash_core::SessionScope::new("session"),
                        descriptor: lash_core::ProcessHandleDescriptor::new(
                            Some("tool"),
                            Some("task-smoke"),
                        ),
                    }),
                    execution_context: Box::new(execution_context),
                }),
            ),
            RuntimeEffectLocalExecutor::processes(registry.clone(), None),
        )
        .await
        .expect("start through endpoint smoke");
    let RuntimeEffectOutcome::Process {
        result: ProcessEffectOutcome::Start { record },
    } = outcome
    else {
        panic!("wrong start outcome");
    };

    let external_ref = record.external_ref.as_ref().expect("external ref");
    assert_eq!(external_ref.backend, "restate");
    assert_eq!(external_ref.id, "LashProcessWorkflow/task-smoke");
    assert_eq!(
        external_ref
            .metadata
            .as_ref()
            .and_then(|metadata| metadata.get("invocation_id")),
        Some(&serde_json::json!("invocation-task-smoke"))
    );

    let session_scope = lash_core::SessionScope::new("session");
    let grants = registry
        .list_handle_grants(&session_scope)
        .await
        .expect("session grants");
    assert_eq!(grants.len(), 1);
    assert_eq!(grants[0].0.process_id, "task-smoke");
    let granted_external_ref = grants[0].1.external_ref.as_ref().expect("grant ref");
    assert_eq!(granted_external_ref.backend, "restate");
    assert_eq!(granted_external_ref.id, "LashProcessWorkflow/task-smoke");

    assert_eq!(
        context
            .started
            .lock()
            .expect("started lock")
            .iter()
            .map(|registration| registration.id.as_str())
            .collect::<Vec<_>>(),
        vec!["task-smoke"]
    );
    assert_eq!(
        runner.ran.lock().expect("runner ran lock").as_slice(),
        &[RecordedProcessRun {
            process_id: "task-smoke".to_string(),
            wake_target_session_id: Some("wake-smoke".to_string()),
            tool_effect_id: Some("tool-smoke".to_string()),
            execution_scope_id: "task-smoke".to_string(),
            controller_tier: lash_core::DurabilityTier::Durable,
        }]
    );

    let outcome = host
        .execute_effect(
            RuntimeEffectEnvelope::new(
                runtime_invocation(RuntimeEffectKind::Process, "background-smoke-cancel"),
                RuntimeEffectCommand::process(ProcessCommand::Cancel {
                    process_id: "task-smoke".to_string(),
                    reason: Some("stop-smoke".to_string()),
                }),
            ),
            RuntimeEffectLocalExecutor::processes(registry, None),
        )
        .await
        .expect("cancel through endpoint smoke");
    assert!(matches!(
        outcome,
        RuntimeEffectOutcome::Process {
            result: ProcessEffectOutcome::Cancel { .. }
        }
    ));
    assert_eq!(
        context.cancelled.lock().expect("cancelled lock").as_slice(),
        &[("task-smoke".to_string(), Some("stop-smoke".to_string()))]
    );
    assert_eq!(
        runner
            .cancelled
            .lock()
            .expect("runner cancelled lock")
            .as_slice(),
        &[RestateProcessCancelRequest {
            process_id: "task-smoke".to_string(),
            reason: Some("stop-smoke".to_string()),
        }]
    );
}

struct RecoveryProcessTool;

impl RecoveryProcessTool {
    fn definition() -> lash_core::ToolDefinition {
        lash_core::ToolDefinition::raw(
            "tool:recovery_echo",
            "recovery_echo",
            "Echo a line and emit a durable process wake.",
            serde_json::json!({
                "type": "object",
                "properties": { "line": { "type": "string" } },
                "required": ["line"],
                "additionalProperties": false
            }),
            serde_json::json!({ "type": "object" }),
        )
        .with_lashlang_binding(LashlangToolBinding::new(["tools"], "recovery_echo"))
    }
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for RecoveryProcessTool {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![Self::definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == "recovery_echo").then(|| Arc::new(Self::definition().contract()))
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolResult {
        let line = call
            .args
            .get("line")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string();
        let event = lash_core::ProcessEventAppendRequest::new(
            "process.wake",
            serde_json::json!({ "message": line, "wake_input": line }),
        )
        .with_replay_key(format!("process.wake:{line}"));
        if let Err(err) = call.context.process_events().emit_request(event).await {
            return lash_core::ToolResult::err_fmt(err);
        }
        lash_core::ToolResult::ok(serde_json::json!({ "echo": line }))
    }
}

struct SnapshotRecoveryTool;

impl SnapshotRecoveryTool {
    fn definition() -> lash_core::ToolDefinition {
        lash_core::ToolDefinition::raw(
            "tool:snapshot_echo",
            "snapshot_echo",
            "Echo a line from a snapshot-backed process tool.",
            serde_json::json!({
                "type": "object",
                "properties": { "line": { "type": "string" } },
                "required": ["line"],
                "additionalProperties": false
            }),
            serde_json::json!({ "type": "object" }),
        )
        .with_lashlang_binding(LashlangToolBinding::new(["tools"], "snapshot_echo"))
    }
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for SnapshotRecoveryTool {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![Self::definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == "snapshot_echo").then(|| Arc::new(Self::definition().contract()))
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolResult {
        let line = call
            .args
            .get("line")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        lash_core::ToolResult::ok(serde_json::json!({ "echo": format!("snapshot:{line}") }))
    }
}

#[derive(serde::Deserialize)]
struct SnapshotRecoveryToolOptions {
    snapshot_ref: String,
}

fn snapshot_recovery_tool_options(snapshot_ref: &str) -> lash_core::PluginOptions {
    lash_core::PluginOptions::typed(
        "snapshot-recovery-tool",
        serde_json::json!({ "snapshot_ref": snapshot_ref }),
    )
    .expect("snapshot recovery plugin options")
}

fn snapshot_recovery_tool_factory() -> Arc<dyn lash_core::PluginFactory> {
    Arc::new(lash_core::plugin::PluginSpecFactory::new(
        "snapshot-recovery-tool",
        Arc::new(|ctx| {
            let snapshot_available = ctx
                .plugin_options
                .decode::<SnapshotRecoveryToolOptions>("snapshot-recovery-tool")
                .map_err(|err| {
                    lash_core::PluginError::Registration(format!(
                        "invalid snapshot recovery tool options: {err}"
                    ))
                })?
                .is_some_and(|options| options.snapshot_ref == "tool-authority:sha256:ok");
            let spec = if snapshot_available {
                lash_core::PluginSpec::new().with_tool_provider(Arc::new(SnapshotRecoveryTool))
            } else {
                lash_core::PluginSpec::new()
            };
            Ok(spec)
        }),
    ))
}

fn recovery_worker(
    registry: Arc<dyn ProcessRegistry>,
    store_factory: Arc<dyn lash_core::SessionStoreFactory>,
) -> DurableProcessWorker {
    recovery_worker_with_plugins(registry, store_factory, Vec::new())
}

fn recovery_worker_with_plugins(
    registry: Arc<dyn ProcessRegistry>,
    store_factory: Arc<dyn lash_core::SessionStoreFactory>,
    extra_plugins: Vec<Arc<dyn lash_core::PluginFactory>>,
) -> DurableProcessWorker {
    let tools: Arc<dyn lash_core::ToolProvider> = Arc::new(RecoveryProcessTool);
    let mut plugins = vec![
        Arc::new(lash_protocol_standard::StandardProtocolPluginFactory::new())
            as Arc<dyn lash_core::PluginFactory>,
        Arc::new(lash_core::plugin::StaticPluginFactory::new(
            "recovery-tool",
            lash_core::PluginSpec::new().with_tool_provider(tools),
        )),
    ];
    plugins.extend(extra_plugins);
    let plugin_host = lash_core::PluginHost::new(plugins);
    let process_env_store: Arc<dyn lash_core::ProcessExecutionEnvStore> =
        RECOVERY_PROCESS_ENV_STORE.clone();
    let runtime_host = lash_core::RuntimeHostConfig::in_memory()
        .with_process_env_store(process_env_store)
        .with_process_engine(Arc::new(
            lash_lashlang_runtime::LashlangProcessEngine::in_memory(
                lash_lashlang_runtime::LashlangSurface::default(),
            ),
        ));
    DurableProcessWorker::new(
        lash_core::DurableProcessWorkerConfig::new(
            Arc::new(plugin_host),
            runtime_host,
            store_factory,
            registry,
        )
        .with_session_policy(recovery_session_policy()),
    )
}

fn recovery_session_policy() -> lash_core::SessionPolicy {
    lash_core::SessionPolicy {
        model: lash_core::ModelSpec::from_token_limits(
            "mock-model",
            Default::default(),
            200_000,
            None,
        )
        .expect("model spec"),
        ..lash_core::SessionPolicy::default()
    }
}

async fn persist_recovery_env_ref() -> lash_core::ProcessExecutionEnvRef {
    let spec = lash_core::ProcessExecutionEnvSpec::new(
        lash_core::PluginOptions::empty(),
        recovery_session_policy(),
    );
    lash_core::runtime::persist_process_execution_env(RECOVERY_PROCESS_ENV_STORE.as_ref(), &spec)
        .await
        .expect("persist recovery process execution env")
}

async fn persist_snapshot_recovery_env_ref(
    snapshot_ref: &str,
) -> lash_core::ProcessExecutionEnvRef {
    let spec = lash_core::ProcessExecutionEnvSpec::new(
        snapshot_recovery_tool_options(snapshot_ref),
        recovery_session_policy(),
    );
    lash_core::runtime::persist_process_execution_env(RECOVERY_PROCESS_ENV_STORE.as_ref(), &spec)
        .await
        .expect("persist snapshot recovery process execution env")
}

fn process_wake_event_type() -> lash_core::ProcessEventType {
    lash_core::ProcessEventType {
        name: "process.wake".to_string(),
        payload_schema: lash_core::LashSchema::any(),
        semantics: lash_core::ProcessEventSemanticsSpec {
            wake: Some(lash_core::ProcessWakeSpec {
                when: Some(lash_core::ProcessValueSelector::Present(
                    "/wake_input".to_string(),
                )),
                input: lash_core::ProcessValueSelector::Pointer("/wake_input".to_string()),
                dedupe_key: lash_core::ProcessWakeDedupeKey::Selector(
                    lash_core::ProcessValueSelector::Pointer("/message".to_string()),
                ),
            }),
            ..lash_core::ProcessEventSemanticsSpec::default()
        },
    }
}

async fn snapshot_lashlang_registration(
    process_id: &str,
    env_ref: lash_core::ProcessExecutionEnvRef,
) -> ProcessRegistration {
    let module = lashlang::parse(
        r#"
        process main() {
          called = await tools.snapshot_echo({ line: "restored" })?
          finish called.echo
        }
        "#,
    )
    .expect("snapshot lashlang module");
    let contract = SnapshotRecoveryTool::definition().contract();
    let mut resources = lashlang::LashlangHostCatalog::new();
    resources.add_module_operation(
        ["tools"],
        "Tools",
        "snapshot_echo",
        "tool:snapshot_echo",
        lashlang::json_schema_to_type_expr(contract.input_schema.canonical()),
        lashlang::json_schema_to_type_expr(contract.output_schema.canonical()),
    );
    let linked_module = lashlang::LinkedModule::link(
        module,
        lashlang::LashlangHostEnvironment::new(
            resources,
            lashlang::LashlangAbilities::default()
                .with_processes()
                .with_sleep()
                .with_process_signals(),
        ),
    )
    .expect("link snapshot lashlang module");
    lashlang::LashlangArtifactStore::put_module_artifact(
        lashlang::global_in_memory_lashlang_artifact_store().as_ref(),
        &linked_module.artifact,
    )
    .await
    .expect("store snapshot lashlang module artifact");
    let process_ref = linked_module
        .artifact
        .process_ref("main")
        .expect("main process ref")
        .clone();
    ProcessRegistration::new(
        process_id,
        lashlang_process_input(lash_lashlang_runtime::LashlangProcessInput {
            module_ref: linked_module.module_ref,
            process_ref,
            host_requirements_ref: linked_module.host_requirements_ref,
            process_name: "main".to_string(),
            args: serde_json::Map::new(),
        }),
        lash_core::RecoveryDisposition::Rerunnable,
        lash_core::ProcessProvenance::host(),
    )
    .with_extra_event_types(lash_lashlang_runtime::lashlang_process_event_types())
    .with_execution_env_ref(Some(env_ref))
}

#[tokio::test]
async fn sqlite_process_recovery_reopens_registry_worker_grants_wakes_and_cancel() {
    let temp = tempfile::tempdir().expect("tempdir");
    let process_db = temp.path().join("processes.db");
    let store_factory = Arc::new(lash_sqlite_store::SqliteSessionStoreFactory::new(
        temp.path().join("sessions"),
    )) as Arc<dyn lash_core::SessionStoreFactory>;
    let registry_a = Arc::new(
        lash_sqlite_store::SqliteProcessRegistry::open(
            &process_db,
            process_db.with_extension("sessions"),
        )
        .await
        .expect("open registry"),
    ) as Arc<dyn ProcessRegistry>;
    let worker_a = recovery_worker(Arc::clone(&registry_a), Arc::clone(&store_factory));
    let _root_store = store_factory
        .create_store(&lash_core::SessionStoreCreateRequest {
            session_id: "root".to_string(),
            relation: lash_core::SessionRelation::default(),
            policy: recovery_session_policy(),
        })
        .await
        .expect("create root session store before wake delivery");
    let endpoint_a = Endpoint::builder()
        .bind(
            LashProcessWorkflowImpl::new_for_test(
                Arc::new(RestateCoreProcessRunner::new(worker_a)),
                Arc::clone(&registry_a),
            )
            .serve(),
        )
        .build();
    let context_a = Arc::new(RecordingContext::with_endpoint(endpoint_a));
    let host_a = RestateRuntimeEffectController::new(context_a);
    let creator_scope = lash_core::SessionScope::new("root");
    let scope_id = creator_scope.id();
    let env_ref = persist_recovery_env_ref().await;
    let registration = ProcessRegistration::new(
        "recover-tool",
        ProcessInput::ToolCall {
            call: lash_core::PreparedToolCall::from_parts(
                "recover-call",
                "tool:recovery_echo",
                "recovery_echo",
                serde_json::json!({ "line": "wake-after-rebuild" }),
                None,
                serde_json::Value::Null,
            ),
        },
        lash_core::RecoveryDisposition::Rerunnable,
        lash_core::ProcessProvenance::session(creator_scope.clone()),
    )
    .with_extra_event_types([process_wake_event_type()])
    .with_execution_env_ref(Some(env_ref))
    .with_wake_target(Some(creator_scope.clone()));

    host_a
        .execute_effect(
            RuntimeEffectEnvelope::new(
                runtime_invocation(RuntimeEffectKind::Process, "recovery-start"),
                RuntimeEffectCommand::process(ProcessCommand::Start {
                    registration,
                    grant: Some(lash_core::ProcessStartGrant {
                        session_scope: creator_scope.clone(),
                        descriptor: lash_core::ProcessHandleDescriptor::new(
                            Some("tool"),
                            Some("recover-tool"),
                        ),
                    }),
                    execution_context: Box::new(ProcessExecutionContext::default()),
                }),
            ),
            RuntimeEffectLocalExecutor::processes(Arc::clone(&registry_a), None),
        )
        .await
        .expect("schedule and run process through Restate endpoint");
    drop(host_a);
    drop(registry_a);

    let registry_b = Arc::new(
        lash_sqlite_store::SqliteProcessRegistry::open(
            &process_db,
            process_db.with_extension("sessions"),
        )
        .await
        .expect("reopen registry"),
    ) as Arc<dyn ProcessRegistry>;
    let grants = registry_b
        .list_handle_grants(&creator_scope)
        .await
        .expect("list reopened grants");
    assert_eq!(grants.len(), 1);
    assert_eq!(grants[0].0.process_id, "recover-tool");
    assert_eq!(
        lash_core::ProcessAwaiter::polling(Arc::clone(&registry_b))
            .await_terminal("recover-tool")
            .await
            .expect("await recovered terminal process"),
        ProcessAwaitOutput::Success {
            value: serde_json::json!({ "echo": "wake-after-rebuild" }),
            control: None,
        }
    );
    let queue_store = store_factory
        .create_store(&lash_core::SessionStoreCreateRequest {
            session_id: "root".to_string(),
            relation: lash_core::SessionRelation::default(),
            policy: lash_core::SessionPolicy {
                model: lash_core::ModelSpec::from_token_limits(
                    "mock-model",
                    Default::default(),
                    200_000,
                    None,
                )
                .expect("model spec"),
                ..lash_core::SessionPolicy::default()
            },
        })
        .await
        .expect("open root session store");
    let queued = queue_store
        .list_queued_work("root")
        .await
        .expect("list queued wakes");
    assert_eq!(queued.len(), 1);
    assert_eq!(queued[0].items.len(), 1);
    let lash_core::runtime::QueuedWorkPayload::ProcessWake { wake } = &queued[0].items[0].payload
    else {
        panic!("expected process wake queue payload");
    };
    assert_eq!(wake.input, "wake-after-rebuild");
    assert_eq!(wake.target_scope_id, scope_id);

    let worker_b = recovery_worker(Arc::clone(&registry_b), store_factory);
    let endpoint_b = Endpoint::builder()
        .bind(
            LashProcessWorkflowImpl::new_for_test(
                Arc::new(RestateCoreProcessRunner::new(worker_b)),
                Arc::clone(&registry_b),
            )
            .serve(),
        )
        .build();
    let context_b = Arc::new(RecordingContext::with_endpoint(endpoint_b));
    let host_b = RestateRuntimeEffectController::new(context_b);
    host_b
        .execute_effect(
            RuntimeEffectEnvelope::new(
                runtime_invocation(RuntimeEffectKind::Process, "recovery-cancel"),
                RuntimeEffectCommand::process(ProcessCommand::Cancel {
                    process_id: "recover-tool".to_string(),
                    reason: Some("post-rebuild cancel probe".to_string()),
                }),
            ),
            RuntimeEffectLocalExecutor::processes(Arc::clone(&registry_b), None),
        )
        .await
        .expect("cancel through reopened process workflow");
    assert!(
        registry_b
            .events_after("recover-tool", 0)
            .await
            .expect("events after cancel")
            .iter()
            .any(|event| event.event_type == "process.cancel_requested")
    );
}

#[tokio::test]
async fn sqlite_process_recovery_rebuilds_snapshot_plugin_options_after_worker_reopen() {
    let temp = tempfile::tempdir().expect("tempdir");
    let process_db = temp.path().join("processes.db");
    let store_factory = Arc::new(lash_sqlite_store::SqliteSessionStoreFactory::new(
        temp.path().join("sessions"),
    )) as Arc<dyn lash_core::SessionStoreFactory>;
    let registry_a = Arc::new(
        lash_sqlite_store::SqliteProcessRegistry::open(
            &process_db,
            process_db.with_extension("sessions"),
        )
        .await
        .expect("open registry"),
    ) as Arc<dyn ProcessRegistry>;
    let env_ref = persist_snapshot_recovery_env_ref("tool-authority:sha256:ok").await;
    registry_a
        .register_process(snapshot_lashlang_registration("snapshot-ok", env_ref).await)
        .await
        .expect("register snapshot-backed process");
    drop(registry_a);

    let registry_b = Arc::new(
        lash_sqlite_store::SqliteProcessRegistry::open(
            &process_db,
            process_db.with_extension("sessions"),
        )
        .await
        .expect("reopen registry"),
    ) as Arc<dyn ProcessRegistry>;
    let worker_b = recovery_worker_with_plugins(
        Arc::clone(&registry_b),
        store_factory,
        vec![snapshot_recovery_tool_factory()],
    );
    worker_b
        .drive_pending_processes()
        .await
        .expect("recover snapshot-backed process");

    assert_eq!(
        lash_core::ProcessAwaiter::polling(Arc::clone(&registry_b))
            .await_terminal("snapshot-ok")
            .await
            .expect("await recovered snapshot-backed process"),
        ProcessAwaitOutput::Success {
            value: serde_json::json!("snapshot:restored"),
            control: None,
        }
    );
}

#[tokio::test]
async fn sqlite_process_recovery_terminalizes_revoked_snapshot_plugin_options() {
    let temp = tempfile::tempdir().expect("tempdir");
    let process_db = temp.path().join("processes.db");
    let store_factory = Arc::new(lash_sqlite_store::SqliteSessionStoreFactory::new(
        temp.path().join("sessions"),
    )) as Arc<dyn lash_core::SessionStoreFactory>;
    let registry_a = Arc::new(
        lash_sqlite_store::SqliteProcessRegistry::open(
            &process_db,
            process_db.with_extension("sessions"),
        )
        .await
        .expect("open registry"),
    ) as Arc<dyn ProcessRegistry>;
    let env_ref = persist_snapshot_recovery_env_ref("tool-authority:sha256:revoked").await;
    registry_a
        .register_process(snapshot_lashlang_registration("snapshot-revoked", env_ref).await)
        .await
        .expect("register revoked snapshot-backed process");
    drop(registry_a);

    let registry_b = Arc::new(
        lash_sqlite_store::SqliteProcessRegistry::open(
            &process_db,
            process_db.with_extension("sessions"),
        )
        .await
        .expect("reopen registry"),
    ) as Arc<dyn ProcessRegistry>;
    let worker_b = recovery_worker_with_plugins(
        Arc::clone(&registry_b),
        store_factory,
        vec![snapshot_recovery_tool_factory()],
    );
    worker_b
        .drive_pending_processes()
        .await
        .expect("recover revoked snapshot-backed process");

    let await_output = lash_core::ProcessAwaiter::polling(Arc::clone(&registry_b))
        .await_terminal("snapshot-revoked")
        .await
        .expect("await terminal revoked snapshot-backed process");
    let ProcessAwaitOutput::Failure { code, message, .. } = await_output else {
        panic!("expected revoked snapshot process failure, got {await_output:#?}");
    };
    assert_eq!(code, "process_host_environment_incompatible");
    assert!(
        message.contains("module `tools` does not expose operation `snapshot_echo`"),
        "{message}"
    );
}

/// Build a durable registration for a trigger-started Lashlang engine process.
///
/// A trigger-started process carries the trigger route's engine payload and
/// provenance whose `caused_by` is the
/// trigger occurrence that fired it — distinct from a turn-started process, whose
/// provenance traces to a live turn/tool call. The module artifact is stored
/// in the process-global in-memory artifact store, mirroring how a trigger
/// route's linked module is published before the process runs; that store
/// survives the registry/worker reopen within a single test process.
async fn trigger_lashlang_registration(process_id: &str, resource: &str) -> ProcessRegistration {
    let module =
        lashlang::parse("process notify(resource: str) { finish { triggered: resource } }")
            .expect("lashlang trigger module");
    let linked_module = lashlang::LinkedModule::link(
        module,
        lashlang::LashlangHostEnvironment::new(
            lashlang::LashlangHostCatalog::new(),
            lashlang::LashlangAbilities::all(),
        ),
    )
    .expect("link lashlang trigger module");
    lashlang::LashlangArtifactStore::put_module_artifact(
        lashlang::global_in_memory_lashlang_artifact_store().as_ref(),
        &linked_module.artifact,
    )
    .await
    .expect("store lashlang trigger module artifact");
    let process_ref = linked_module
        .artifact
        .process_ref("notify")
        .expect("notify process ref")
        .clone();
    let mut args = serde_json::Map::new();
    args.insert("resource".to_string(), serde_json::json!(resource));
    let env_ref = persist_recovery_env_ref().await;
    ProcessRegistration::new(
        process_id,
        lashlang_process_input(lash_lashlang_runtime::LashlangProcessInput {
            module_ref: linked_module.module_ref,
            process_ref,
            host_requirements_ref: linked_module.host_requirements_ref,
            process_name: "notify".to_string(),
            args,
        }),
        lash_core::RecoveryDisposition::Rerunnable,
        lash_core::ProcessProvenance::session(lash_core::SessionScope::new("root")).with_caused_by(
            Some(lash_core::CausalRef::SessionNode {
                session_id: "root".to_string(),
                node_id: "trigger:resource.updated".to_string(),
            }),
        ),
    )
    .with_extra_event_types(lash_lashlang_runtime::lashlang_process_event_types())
    .with_execution_env_ref(Some(env_ref))
}

fn assert_lashlang_engine_record(
    record: &lash_core::ProcessRecord,
    expected_process_name: &str,
    expected_args: serde_json::Map<String, serde_json::Value>,
) {
    let ProcessInput::Engine { kind, payload } = record.input.as_ref() else {
        panic!(
            "persisted Lashlang process must use generic engine input, got {:?}",
            record.input
        );
    };
    assert_eq!(
        kind,
        lash_lashlang_runtime::LASHLANG_ENGINE_KIND,
        "persisted row must dispatch through the registered Lashlang process engine"
    );
    let decoded = lash_lashlang_runtime::LashlangProcessInput::from_payload(payload.clone())
        .expect("persisted Lashlang engine payload must decode after registry reopen");
    assert_eq!(decoded.process_name, expected_process_name);
    assert_eq!(decoded.args, expected_args);
}

/// Phase-B recovery: a TRIGGER-started process whose worker died mid-flight is
/// left non-terminal in the durable registry; a subsequent worker reopening
/// that registry must drive it to completion via the recovery sweep — the same
/// durable re-execution guarantee a turn-started process has (invariant 3).
///
/// Mirrors `sqlite_process_recovery_reopens_registry_worker_grants_wakes_and_cancel`
/// but the process is started by a trigger occurrence (a `lashlang` engine row
/// with trigger provenance), not by a live turn's tool call.
#[tokio::test]
async fn sqlite_trigger_started_process_recovered_after_worker_registry_reopen() {
    let temp = tempfile::tempdir().expect("tempdir");
    let process_db = temp.path().join("processes.db");
    let store_factory = Arc::new(lash_sqlite_store::SqliteSessionStoreFactory::new(
        temp.path().join("sessions"),
    )) as Arc<dyn lash_core::SessionStoreFactory>;

    // A worker started the trigger process and crashed before it could run:
    // the durable row exists and is non-terminal. We register it directly to
    // model exactly that mid-flight crash state.
    let registry_a = Arc::new(
        lash_sqlite_store::SqliteProcessRegistry::open(
            &process_db,
            process_db.with_extension("sessions"),
        )
        .await
        .expect("open registry"),
    ) as Arc<dyn ProcessRegistry>;
    registry_a
        .register_process(trigger_lashlang_registration("trigger-notify", "issue-42").await)
        .await
        .expect("register trigger-started process");
    let persisted_before_rebuild = registry_a
        .get_process("trigger-notify")
        .await
        .expect("persisted trigger-started process before recovery");
    assert!(
        !persisted_before_rebuild.is_terminal(),
        "freshly trigger-started process must be non-terminal before recovery"
    );
    drop(registry_a);

    // Reopen the registry and stand up a fresh worker over it: the crash
    // recovery counterpart. The recovery sweep submits the non-terminal process
    // by workflow key; Restate coalesces duplicates and the workflow writes the
    // terminal outcome.
    let registry_b = Arc::new(
        lash_sqlite_store::SqliteProcessRegistry::open(
            &process_db,
            process_db.with_extension("sessions"),
        )
        .await
        .expect("reopen registry"),
    ) as Arc<dyn ProcessRegistry>;
    let reopened_record = registry_b
        .get_process("trigger-notify")
        .await
        .expect("trigger-started process survives registry reopen");
    assert_lashlang_engine_record(
        &reopened_record,
        "notify",
        serde_json::Map::from_iter([("resource".to_string(), serde_json::json!("issue-42"))]),
    );
    assert_eq!(
        registry_b
            .list_non_terminal()
            .await
            .expect("list non-terminal after reopen")
            .iter()
            .map(|record| record.id.as_str())
            .collect::<Vec<_>>(),
        vec!["trigger-notify"],
        "the trigger-started process must be on the recovery worklist after reopen"
    );

    let worker_b = recovery_worker(Arc::clone(&registry_b), Arc::clone(&store_factory));
    worker_b
        .drive_pending_processes()
        .await
        .expect("recover non-terminal trigger-started process");

    assert_eq!(
        lash_core::ProcessAwaiter::polling(Arc::clone(&registry_b))
            .await_terminal("trigger-notify")
            .await
            .expect("await recovered trigger-started process"),
        ProcessAwaitOutput::Success {
            value: serde_json::json!({ "triggered": "issue-42" }),
            control: None,
        },
        "the trigger-started process must run to its terminal value on recovery"
    );
    assert!(
        registry_b
            .list_non_terminal()
            .await
            .expect("list non-terminal after recovery")
            .is_empty(),
        "recovery must drive the trigger-started process to terminal"
    );

    // Idempotent by process_id: re-running the sweep over an already-terminal
    // process is a no-op and never double-executes it.
    worker_b
        .drive_pending_processes()
        .await
        .expect("second recovery sweep is idempotent");
    assert_eq!(
        lash_core::ProcessAwaiter::polling(Arc::clone(&registry_b))
            .await_terminal("trigger-notify")
            .await
            .expect("await after idempotent re-sweep"),
        ProcessAwaitOutput::Success {
            value: serde_json::json!({ "triggered": "issue-42" }),
            control: None,
        }
    );
}

/// A process tool that counts executions in a shared atomic. Its execution is
/// the observable side effect the crash-recovery e2e keys off: a started
/// OwnerBound row that is (correctly) NOT re-run leaves the counter untouched,
/// while a Rerunnable sibling that IS re-run bumps it exactly once.
struct CountingProcessTool {
    executions: Arc<AtomicUsize>,
}

impl CountingProcessTool {
    fn definition() -> lash_core::ToolDefinition {
        lash_core::ToolDefinition::raw(
            "tool:recovery_count",
            "recovery_count",
            "Increment a shared execution counter (a stand-in non-idempotent side effect).",
            serde_json::json!({
                "type": "object",
                "properties": { "line": { "type": "string" } },
                "required": ["line"],
                "additionalProperties": false
            }),
            serde_json::json!({ "type": "object" }),
        )
        .with_lashlang_binding(LashlangToolBinding::new(["tools"], "recovery_count"))
    }
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for CountingProcessTool {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![Self::definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == "recovery_count").then(|| Arc::new(Self::definition().contract()))
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolResult {
        let executed = self.executions.fetch_add(1, Ordering::SeqCst) + 1;
        let line = call
            .args
            .get("line")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        lash_core::ToolResult::ok(serde_json::json!({ "executed": executed, "line": line }))
    }
}

fn counting_tool_plugin(executions: Arc<AtomicUsize>) -> Arc<dyn lash_core::PluginFactory> {
    Arc::new(lash_core::plugin::StaticPluginFactory::new(
        "counting-process-tool",
        lash_core::PluginSpec::new()
            .with_tool_provider(Arc::new(CountingProcessTool { executions })),
    ))
}

/// A recovery worker whose lease owner is a real same-host/same-boot local
/// process (not the default opaque owner) so `is_definitely_dead_for_claimant`
/// can render a provably-dead verdict against a fabricated dead holder.
fn recovery_worker_local_owner(
    registry: Arc<dyn ProcessRegistry>,
    store_factory: Arc<dyn lash_core::SessionStoreFactory>,
    owner: lash_core::LeaseOwnerIdentity,
    extra_plugins: Vec<Arc<dyn lash_core::PluginFactory>>,
) -> DurableProcessWorker {
    let base = recovery_worker_with_plugins(registry, store_factory, extra_plugins);
    DurableProcessWorker::from_shared_config(Arc::new(
        base.config().clone().with_lease_owner(owner),
    ))
}

/// A same-host/same-boot local-process lease owner. When `process_start` does
/// not match this live process's real start time, the holder is provably dead
/// for a claimant fabricated the same way — exactly the recovery-sweep death
/// evidence path (`process_worker::recovery_tests`).
fn local_process_owner(owner_id: &str, process_start: &str) -> lash_core::LeaseOwnerIdentity {
    lash_core::LeaseOwnerIdentity {
        owner_id: owner_id.to_string(),
        incarnation_id: format!("{owner_id}:incarnation"),
        liveness: lash_core::LeaseOwnerLiveness::local_process_for_test(
            "restate-recovery-host",
            "restate-recovery-boot",
            std::process::id(),
            process_start,
        ),
    }
}

fn counting_tool_registration(
    id: &str,
    disposition: lash_core::RecoveryDisposition,
    env_ref: lash_core::ProcessExecutionEnvRef,
) -> ProcessRegistration {
    ProcessRegistration::new(
        id,
        ProcessInput::ToolCall {
            call: lash_core::PreparedToolCall::from_parts(
                format!("{id}-call"),
                "tool:recovery_count",
                "recovery_count",
                serde_json::json!({ "line": id }),
                None,
                serde_json::Value::Null,
            ),
        },
        disposition,
        lash_core::ProcessProvenance::host(),
    )
    .with_execution_env_ref(Some(env_ref))
}

/// The Harbor reproducer at the process layer, over a REOPENED sqlite registry
/// (ADR 0019): a host crashed mid-flight leaving a started OwnerBound row whose
/// holder is provably dead. A fresh worker's recovery sweep must terminalize it
/// `Abandoned{Sweep}` — never re-execute it (double side effects) — while a
/// Rerunnable sibling in the same sweep IS re-run. The shared execution counter
/// proves exactly one execution happened: the Rerunnable one.
#[tokio::test]
async fn sqlite_sweep_abandons_started_owner_bound_without_rerunning_but_reruns_sibling() {
    let temp = tempfile::tempdir().expect("tempdir");
    let process_db = temp.path().join("processes.db");
    let store_factory = Arc::new(lash_sqlite_store::SqliteSessionStoreFactory::new(
        temp.path().join("sessions"),
    )) as Arc<dyn lash_core::SessionStoreFactory>;
    let env_ref = persist_recovery_env_ref().await;

    // The crashed host: it started an OwnerBound row and a Rerunnable row, then
    // died. We register that mid-flight state directly on the durable registry.
    let registry_a = Arc::new(
        lash_sqlite_store::SqliteProcessRegistry::open(
            &process_db,
            process_db.with_extension("sessions"),
        )
        .await
        .expect("open registry"),
    ) as Arc<dyn ProcessRegistry>;
    let dead_holder = local_process_owner("crashed-host", "before-the-crash");
    registry_a
        .register_process(counting_tool_registration(
            "ob-crashed",
            lash_core::RecoveryDisposition::OwnerBound,
            env_ref.clone(),
        ))
        .await
        .expect("register OwnerBound crashed row");
    registry_a
        .record_first_started(
            "ob-crashed",
            lash_core::ProcessStarted {
                owner: dead_holder.clone(),
                fencing_token: 0,
                attempt: 1,
                started_at_ms: 1,
            },
        )
        .await
        .expect("record OwnerBound first_started");
    registry_a
        .claim_process_lease("ob-crashed", &dead_holder, 60_000)
        .await
        .expect("dead holder claims OwnerBound lease")
        .acquired()
        .expect("dead holder lease acquired");
    // A Rerunnable sibling the same crashed host had started (no live lease held).
    registry_a
        .register_process(counting_tool_registration(
            "rerun-crashed",
            lash_core::RecoveryDisposition::Rerunnable,
            env_ref.clone(),
        ))
        .await
        .expect("register Rerunnable crashed row");
    registry_a
        .record_first_started(
            "rerun-crashed",
            lash_core::ProcessStarted {
                owner: dead_holder.clone(),
                fencing_token: 0,
                attempt: 1,
                started_at_ms: 1,
            },
        )
        .await
        .expect("record Rerunnable first_started");
    drop(registry_a);

    // The recovery counterpart: reopen the registry, stand up a fresh worker
    // whose lease owner can render the provably-dead verdict, and sweep.
    let registry_b = Arc::new(
        lash_sqlite_store::SqliteProcessRegistry::open(
            &process_db,
            process_db.with_extension("sessions"),
        )
        .await
        .expect("reopen registry"),
    ) as Arc<dyn ProcessRegistry>;
    let executions = Arc::new(AtomicUsize::new(0));
    let worker = recovery_worker_local_owner(
        Arc::clone(&registry_b),
        Arc::clone(&store_factory),
        local_process_owner("recovery-host", "recovery-claimant"),
        vec![counting_tool_plugin(Arc::clone(&executions))],
    );
    worker
        .drive_pending_processes()
        .await
        .expect("recovery sweep dispatches");

    // The OwnerBound crashed row reaches Abandoned{Sweep}, naming the dead holder
    // — never a run terminal.
    let ob_output = lash_core::ProcessAwaiter::polling(Arc::clone(&registry_b))
        .await_terminal("ob-crashed")
        .await
        .expect("OwnerBound crashed row reaches terminal");
    let ProcessAwaitOutput::Abandoned { evidence, .. } = &ob_output else {
        panic!("expected Abandoned terminal for the started OwnerBound row, got {ob_output:?}");
    };
    assert_eq!(evidence.writer, AbandonWriter::Sweep);
    assert_eq!(
        evidence.owner.as_ref().map(|owner| owner.owner_id.as_str()),
        Some("crashed-host"),
        "the sweep names the provably-dead holder as the abandoned owner"
    );

    // The Rerunnable sibling IS re-run to a run terminal in the same sweep.
    let rerun_output = lash_core::ProcessAwaiter::polling(Arc::clone(&registry_b))
        .await_terminal("rerun-crashed")
        .await
        .expect("Rerunnable crashed row reaches terminal");
    assert!(
        matches!(rerun_output, ProcessAwaitOutput::Success { .. }),
        "the Rerunnable sibling is re-run to a run terminal, got {rerun_output:?}"
    );

    // The counter is the execution journal: exactly one execution happened — the
    // Rerunnable one. The started OwnerBound row's non-idempotent side effect was
    // NEVER repeated.
    assert_eq!(
        executions.load(Ordering::SeqCst),
        1,
        "exactly one execution: the Rerunnable sibling; the abandoned OwnerBound row was never re-run"
    );
}

fn discover_service<S: Discoverable>(_: &S) -> restate_sdk::discovery::Service {
    S::discover()
}

#[tokio::test]
async fn restate_workflows_and_wait_index_bind_with_required_handlers() {
    let runner = Arc::new(RecordingRunner::default());
    let registry = process_registry();
    let service = LashProcessWorkflowImpl::new_for_test(runner, registry).serve();
    let discovery = discover_service(&service);
    let wait_workflow = LashDurableWaitWorkflowImpl.serve();
    let wait_workflow_discovery = discover_service(&wait_workflow);
    let wait_index = LashDurableWaitIndexImpl.serve();
    let wait_index_discovery = discover_service(&wait_index);
    let endpoint = Endpoint::builder()
        .bind(service)
        .bind(wait_workflow)
        .bind(wait_index)
        .build();

    assert_eq!(discovery.name.to_string(), "LashProcessWorkflow");
    assert_eq!(
        discovery.ty.to_string(),
        restate_sdk::discovery::ServiceType::Workflow.to_string()
    );
    assert_eq!(discovery.handlers.len(), 6);

    let run = discovery
        .handlers
        .iter()
        .find(|handler| handler.name.to_string() == "run")
        .expect("run handler discovery");
    let cancel = discovery
        .handlers
        .iter()
        .find(|handler| handler.name.to_string() == "cancel")
        .expect("cancel handler discovery");
    let await_terminal = discovery
        .handlers
        .iter()
        .find(|handler| handler.name.to_string() == "await_terminal")
        .expect("await_terminal handler discovery");
    let complete_terminal = discovery
        .handlers
        .iter()
        .find(|handler| handler.name.to_string() == "complete_terminal")
        .expect("complete_terminal handler discovery");
    let deliver_cancel = discovery
        .handlers
        .iter()
        .find(|handler| handler.name.to_string() == "deliver_cancel")
        .expect("deliver_cancel handler discovery");
    let await_cancel = discovery
        .handlers
        .iter()
        .find(|handler| handler.name.to_string() == "await_cancel")
        .expect("await_cancel handler discovery");

    assert_eq!(
        run.ty.as_ref().map(ToString::to_string).as_deref(),
        Some("WORKFLOW")
    );
    assert_eq!(
        cancel.ty.as_ref().map(ToString::to_string).as_deref(),
        Some("SHARED")
    );
    assert_eq!(
        await_terminal
            .ty
            .as_ref()
            .map(ToString::to_string)
            .as_deref(),
        Some("SHARED")
    );
    assert_eq!(
        complete_terminal
            .ty
            .as_ref()
            .map(ToString::to_string)
            .as_deref(),
        Some("SHARED")
    );
    assert_eq!(
        deliver_cancel
            .ty
            .as_ref()
            .map(ToString::to_string)
            .as_deref(),
        Some("SHARED")
    );
    assert_eq!(
        await_cancel.ty.as_ref().map(ToString::to_string).as_deref(),
        Some("SHARED")
    );

    let response = endpoint.handle(
        http::Request::builder()
            .uri("/discover")
            .header("accept", "application/vnd.restate.endpointmanifest.v3+json")
            .body(Empty::<bytes::Bytes>::new())
            .expect("discover request"),
    );
    assert_eq!(response.status(), http::StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get(http::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok()),
        Some("application/vnd.restate.endpointmanifest.v3+json")
    );
    let body = response
        .into_body()
        .collect()
        .await
        .expect("discover response body")
        .to_bytes();
    let manifest: serde_json::Value =
        serde_json::from_slice(&body).expect("discover response json");
    let workflow = manifest["services"]
        .as_array()
        .expect("services array")
        .iter()
        .find(|service| service["name"] == "LashProcessWorkflow")
        .expect("workflow service");
    let handlers = workflow["handlers"].as_array().expect("handlers array");
    assert!(
        handlers
            .iter()
            .any(|handler| handler["name"] == "run" && handler["ty"] == "WORKFLOW")
    );
    assert!(
        handlers
            .iter()
            .any(|handler| handler["name"] == "cancel" && handler["ty"] == "SHARED")
    );
    assert!(
        handlers
            .iter()
            .any(|handler| handler["name"] == "await_terminal" && handler["ty"] == "SHARED")
    );
    assert!(
        handlers
            .iter()
            .any(|handler| { handler["name"] == "complete_terminal" && handler["ty"] == "SHARED" })
    );
    assert!(
        handlers
            .iter()
            .any(|handler| { handler["name"] == "deliver_cancel" && handler["ty"] == "SHARED" })
    );
    assert!(
        handlers
            .iter()
            .any(|handler| { handler["name"] == "await_cancel" && handler["ty"] == "SHARED" })
    );
    assert_eq!(
        wait_workflow_discovery.name.to_string(),
        "LashDurableWaitWorkflow"
    );
    assert!(wait_workflow_discovery.handlers.iter().any(|handler| {
        handler.name.to_string() == "await_resolution"
            && handler.ty.as_ref().map(ToString::to_string).as_deref() == Some("SHARED")
    }));
    assert_eq!(wait_workflow_discovery.handlers.len(), 5);
    assert!(
        wait_workflow_discovery
            .handlers
            .iter()
            .all(|handler| handler.name.to_string() != "observe"),
        "the superseded observe handoff must not remain registered"
    );
    assert!(wait_workflow_discovery.handlers.iter().any(|handler| {
        handler.name.to_string() == "resolve"
            && handler.ty.as_ref().map(ToString::to_string).as_deref() == Some("SHARED")
    }));
    assert_eq!(
        wait_index_discovery.name.to_string(),
        "LashDurableWaitIndex"
    );
    for required in ["register", "settle", "resolve", "cancel_all", "revoke_all"] {
        assert!(
            wait_index_discovery
                .handlers
                .iter()
                .any(|handler| handler.name.to_string() == required),
            "missing wait-index handler {required}"
        );
    }
}

#[tokio::test]
async fn process_deployment_driver_and_workflow_share_registry() {
    let registry = process_registry();
    let deployment = RestateProcessDeployment::new("http://127.0.0.1:8080", Arc::clone(&registry));
    let driver = deployment.process_work_driver();
    let driver_registry = driver.process_registry();

    assert!(Arc::ptr_eq(&driver.process_registry(), &driver_registry));

    let worker = DurableProcessWorker::new(
        lash_core::DurableProcessWorkerConfig::new(
            Arc::new(lash_core::PluginHost::empty()),
            lash_core::RuntimeHostConfig::in_memory(),
            Arc::new(lash_core::InMemorySessionStoreFactory::new()),
            Arc::clone(&driver_registry),
        )
        .with_change_hub(driver.change_hub()),
    );
    let service = deployment.workflow(worker).serve();
    let discovery = discover_service(&service);
    let endpoint = Endpoint::builder().bind(service).build();

    assert_eq!(discovery.name.to_string(), "LashProcessWorkflow");
    assert!(discovery.handlers.iter().any(|handler| {
        handler.name.to_string() == "run"
            && handler.ty.as_ref().map(ToString::to_string).as_deref() == Some("WORKFLOW")
    }));
    assert!(discovery.handlers.iter().any(|handler| {
        handler.name.to_string() == "cancel"
            && handler.ty.as_ref().map(ToString::to_string).as_deref() == Some("SHARED")
    }));
    assert!(discovery.handlers.iter().any(|handler| {
        handler.name.to_string() == "await_terminal"
            && handler.ty.as_ref().map(ToString::to_string).as_deref() == Some("SHARED")
    }));

    let response = endpoint.handle(
        http::Request::builder()
            .uri("/discover")
            .header("accept", "application/vnd.restate.endpointmanifest.v3+json")
            .body(Empty::<bytes::Bytes>::new())
            .expect("discover request"),
    );
    assert_eq!(response.status(), http::StatusCode::OK);
}

#[tokio::test]
async fn process_workflow_impl_runs_and_cancels_through_runner() {
    let runner = Arc::new(RecordingRunner::default());
    let registry = process_registry();
    let workflow = LashProcessWorkflowImpl::new_for_test(runner.clone(), registry.clone());
    // The workflow only ever runs lash-executed rows: `submit_record` refuses to
    // POST an ExternallyOwned row, and the registry rejects a workflow-key
    // completion of one (ADR 0027) — so the fixture is Rerunnable.
    let registration = rerunnable_registration("task-workflow")
        .with_wake_target(Some(lash_core::SessionScope::new("wake-session")));
    registry
        .register_process(registration.clone())
        .await
        .expect("register workflow process");
    let execution_context = ProcessExecutionContext::default().with_causal_invocation(Some(
        runtime_invocation(RuntimeEffectKind::ToolAttempt, "tool-effect"),
    ));

    let output = workflow
        .run_registration(
            registration,
            execution_context,
            lash_core::ScopedEffectController::shared(
                Arc::new(lash_core::InlineRuntimeEffectController::default()),
                lash_core::ExecutionScope::process("task-workflow"),
            )
            .expect("inline process scope"),
            0,
            None,
            pending_process_cancel_signal(),
        )
        .await
        .expect("workflow run");
    workflow
        .cancel_registration(RestateProcessCancelRequest {
            process_id: "task-workflow".to_string(),
            reason: Some("stop".to_string()),
        })
        .await
        .expect("workflow cancel");

    assert!(matches!(
        output,
        lash_core::ProcessRunOutcome::Terminal(output)
            if matches!(*output, ProcessAwaitOutput::Success { .. })
    ));
    assert_eq!(
        runner.ran.lock().expect("runner ran lock").as_slice(),
        &[RecordedProcessRun {
            process_id: "task-workflow".to_string(),
            wake_target_session_id: Some("wake-session".to_string()),
            tool_effect_id: Some("tool-effect".to_string()),
            execution_scope_id: "task-workflow".to_string(),
            controller_tier: lash_core::DurabilityTier::Inline,
        }]
    );
    assert_eq!(
        runner
            .cancelled
            .lock()
            .expect("runner cancelled lock")
            .as_slice(),
        &[RestateProcessCancelRequest {
            process_id: "task-workflow".to_string(),
            reason: Some("stop".to_string()),
        }]
    );
}

#[tokio::test]
async fn terminal_retry_returns_the_stored_outcome() {
    let runner = Arc::new(RecordingRunner::default());
    let registry = process_registry();
    let workflow = LashProcessWorkflowImpl::new_for_test(runner, registry.clone());
    registry
        .register_process(rerunnable_registration("terminal-retry"))
        .await
        .expect("register process");
    let stored = ProcessAwaitOutput::Success {
        value: serde_json::json!({"winner": "stored"}),
        control: None,
    };
    registry
        .complete_process(
            "terminal-retry",
            stored.clone(),
            lash_core::ProcessCompletionAuthority::workflow_key("terminal-retry"),
        )
        .await
        .expect("commit terminal");

    let replayed = workflow
        .complete_with_stored_outcome(
            "terminal-retry",
            ProcessAwaitOutput::Failure {
                class: lash_core::ToolFailureClass::Execution,
                code: "divergent".to_string(),
                message: "must not replace the stored outcome".to_string(),
                raw: None,
                control: None,
            },
        )
        .await
        .expect("terminal retry");

    assert_eq!(replayed, stored);
    assert_eq!(
        registry
            .events_after("terminal-retry", 0)
            .await
            .expect("terminal events")
            .into_iter()
            .filter(|event| event.semantics.terminal.is_some())
            .count(),
        1
    );
}

fn invocation_started(
    process_id: &str,
    execution_id: &str,
    attempt: u32,
) -> (
    lash_core::ProcessExecutionWriteAuthority,
    lash_core::ProcessStarted,
) {
    let authority = lash_core::ProcessExecutionWriteAuthority::invocation(process_id, execution_id)
        .bind_attempt(attempt);
    let mut started = authority
        .invocation_started()
        .expect("bound invocation authority");
    started.started_at_ms = u64::from(attempt);
    (authority, started)
}

#[tokio::test]
async fn restate_invocation_identity_distinguishes_replay_from_fresh_execution() {
    let registry = process_registry();
    registry
        .register_process(rerunnable_registration("invocation-rerun").with_max_attempts(Some(2)))
        .await
        .expect("register rerunnable");

    let (first_authority, first_started) =
        invocation_started("invocation-rerun", "invocation-1", 1);
    assert!(matches!(
        registry
            .record_first_started_with_authority(
                "invocation-rerun",
                first_started.clone(),
                &first_authority,
            )
            .await
            .expect("first invocation"),
        lash_core::ProcessStartOutcome::Started(_)
    ));
    assert!(matches!(
        registry
            .record_first_started_with_authority(
                "invocation-rerun",
                first_started,
                &first_authority,
            )
            .await
            .expect("cross-replica journal replay"),
        lash_core::ProcessStartOutcome::AlreadyApplied(_)
    ));

    let (second_authority, second_started) =
        invocation_started("invocation-rerun", "invocation-2", 2);
    assert!(matches!(
        registry
            .record_first_started_with_authority(
                "invocation-rerun",
                second_started,
                &second_authority,
            )
            .await
            .expect("fresh invocation"),
        lash_core::ProcessStartOutcome::Started(_)
    ));
    let (third_authority, third_started) =
        invocation_started("invocation-rerun", "invocation-3", 3);
    assert!(matches!(
        registry
            .record_first_started_with_authority(
                "invocation-rerun",
                third_started,
                &third_authority,
            )
            .await
            .expect("attempt budget verdict"),
        lash_core::ProcessStartOutcome::AttemptsExhausted {
            attempts: 2,
            max_attempts: 2,
            ..
        }
    ));
}

#[tokio::test]
async fn owner_bound_segment_continuation_reuses_root_invocation_identity() {
    let registry = process_registry();
    registry
        .register_process(owner_bound_registration("owner-bound-segment"))
        .await
        .expect("register owner-bound");
    let (root_authority, root_started) =
        invocation_started("owner-bound-segment", "root-invocation", 1);
    registry
        .record_first_started_with_authority(
            "owner-bound-segment",
            root_started.clone(),
            &root_authority,
        )
        .await
        .expect("start root segment");

    assert!(matches!(
        registry
            .record_first_started_with_authority(
                "owner-bound-segment",
                root_started,
                &root_authority,
            )
            .await
            .expect("mid-chain continuation"),
        lash_core::ProcessStartOutcome::AlreadyApplied(_)
    ));
    let (fresh_authority, fresh_started) =
        invocation_started("owner-bound-segment", "fresh-invocation", 2);
    assert!(matches!(
        registry
            .record_first_started_with_authority(
                "owner-bound-segment",
                fresh_started,
                &fresh_authority,
            )
            .await
            .expect("fresh owner-bound invocation verdict"),
        lash_core::ProcessStartOutcome::AlreadyStarted { .. }
    ));
}

#[tokio::test]
async fn run_registration_abandons_restarted_owner_bound_without_running() {
    // ADR 0019: when the engine re-invokes the workflow for an OwnerBound row
    // whose prior incarnation already recorded `first_started` but left no
    // outcome, the run handler must NOT re-execute it. It completes the row as
    // Abandoned{Sweep} — the durable tier's crash-recovery verdict — and returns
    // that output so the durable promise still resolves for awaiters.
    let started_owner = lash_core::LeaseOwnerIdentity::opaque("owner-a", "incarnation-1");
    let runner = Arc::new(AlreadyStartedRunner {
        calls: Mutex::new(0),
        winner: started_owner.clone(),
    });
    let registry = process_registry();
    let workflow = LashProcessWorkflowImpl::new_for_test(runner.clone(), registry.clone());
    let registration = owner_bound_registration("ob-restart");
    registry
        .register_process(registration.clone())
        .await
        .expect("register owner-bound process");
    // Simulate the prior incarnation that began executing but never completed.
    registry
        .record_first_started(
            "ob-restart",
            lash_core::ProcessStarted {
                owner: started_owner.clone(),
                fencing_token: 0,
                attempt: 1,
                started_at_ms: 42,
            },
        )
        .await
        .expect("record prior incarnation start");

    let output = workflow
        .run_registration(
            registration,
            ProcessExecutionContext::default(),
            lash_core::ScopedEffectController::shared(
                Arc::new(lash_core::InlineRuntimeEffectController::default()),
                lash_core::ExecutionScope::process("ob-restart"),
            )
            .expect("inline process scope"),
            0,
            None,
            pending_process_cancel_signal(),
        )
        .await
        .expect("run_registration");

    // The real runner rejects this before user-code execution when its atomic
    // start write observes the prior OwnerBound attempt. This fake returns the
    // same typed verdict so the durable tier's terminal decision is exercised.
    assert_eq!(*runner.calls.lock().expect("runner calls lock"), 1);
    // Both the returned output and the persisted terminal are Abandoned{Sweep},
    // naming the incarnation that began the work as the evidence owner.
    let lash_core::ProcessRunOutcome::Terminal(output) = &output else {
        panic!("expected terminal output, got {output:?}");
    };
    let ProcessAwaitOutput::Abandoned { evidence, .. } = output.as_ref() else {
        panic!("expected Abandoned output, got {output:?}");
    };
    assert_eq!(evidence.writer, AbandonWriter::Sweep);
    assert_eq!(evidence.owner.as_ref(), Some(&started_owner));
    let record = registry
        .get_process("ob-restart")
        .await
        .expect("get abandoned row");
    assert!(record.is_terminal(), "the row is completed as terminal");
    assert!(matches!(
        record.status.await_output(),
        Some(ProcessAwaitOutput::Abandoned { .. })
    ));
}

#[tokio::test]
async fn run_registration_runs_fresh_owner_bound() {
    // A fresh OwnerBound row has no `first_started` (the runner records it inside
    // run_process, during execution), so the re-invocation guard must NOT fire:
    // the runner executes normally on the first invocation.
    let runner = Arc::new(RecordingRunner::default());
    let registry = process_registry();
    let workflow = LashProcessWorkflowImpl::new_for_test(runner.clone(), registry.clone());
    let registration = owner_bound_registration("ob-fresh");
    registry
        .register_process(registration.clone())
        .await
        .expect("register fresh owner-bound process");

    let output = workflow
        .run_registration(
            registration,
            ProcessExecutionContext::default(),
            lash_core::ScopedEffectController::shared(
                Arc::new(lash_core::InlineRuntimeEffectController::default()),
                lash_core::ExecutionScope::process("ob-fresh"),
            )
            .expect("inline process scope"),
            0,
            None,
            pending_process_cancel_signal(),
        )
        .await
        .expect("run_registration");

    assert!(matches!(
        output,
        lash_core::ProcessRunOutcome::Terminal(output)
            if matches!(*output, ProcessAwaitOutput::Success { .. })
    ));
    assert_eq!(
        runner
            .ran
            .lock()
            .expect("runner ran lock")
            .iter()
            .map(|run| run.process_id.clone())
            .collect::<Vec<_>>(),
        vec!["ob-fresh".to_string()],
        "a fresh OwnerBound row runs through the runner on first invocation"
    );
}

#[tokio::test]
async fn ingress_runner_submits_non_terminal_process_by_workflow_key() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    // A non-terminal, Lash-executed (Rerunnable) process is the durable
    // worklist row the ingress runner must submit. ExternallyOwned rows are
    // never submitted (ADR 0019), so the submittable case uses a Rerunnable row.
    let registry = process_registry();
    registry
        .register_process(rerunnable_registration("task-1"))
        .await
        .expect("register");

    // Minimal mock ingress: capture two submissions, then reply 202 Accepted
    // so the reqwest submit succeeds. The second submit exercises the
    // registry's exact-repeat external_ref path for a still-running process.
    let captured: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let captured_server = captured.clone();
    let server = tokio::spawn(async move {
        for _ in 0..2 {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let mut buf = vec![0u8; 8192];
            let n = socket.read(&mut buf).await.expect("read request");
            captured_server
                .lock()
                .expect("captured lock")
                .push(String::from_utf8_lossy(&buf[..n]).into_owned());
            socket
                .write_all(
                    b"HTTP/1.1 202 Accepted\r\ncontent-type: application/json\r\ncontent-length: 49\r\n\r\n{\"invocationId\":\"inv_task_1\",\"status\":\"Accepted\"}",
                )
                .await
                .expect("write response");
            socket.flush().await.expect("flush");
        }
    });

    let runner = RestateProcessIngressRunner::new(format!("http://{addr}"), registry.clone());
    runner.claim_and_run_pending().await.expect("drive pending");
    runner
        .claim_and_run_pending()
        .await
        .expect("drive pending again");
    server.await.expect("mock ingress server task");

    let requests = captured.lock().expect("captured lock").clone();
    assert_eq!(
        requests.len(),
        2,
        "the non-terminal process must be submitted on both scans"
    );
    let request = &requests[0];
    assert!(
        request.starts_with("POST /LashProcessWorkflow/task-1/run/send "),
        "submits the keyed workflow run: {request}"
    );
    assert!(
        !request.contains("idempotency-key:"),
        "workflow sends must not carry an idempotency header; Restate coalesces by workflow key: {request}"
    );
    assert!(
        requests[1].starts_with("POST /LashProcessWorkflow/task-1/run/send "),
        "repeat scan submits the same keyed workflow run: {}",
        requests[1]
    );

    // The durable backend reference is recorded so the process is observably
    // owned by Restate.
    let record = registry.get_process("task-1").await.expect("get process");
    assert_eq!(
        record.external_ref.as_ref().map(|e| e.backend.as_str()),
        Some("restate"),
        "the durable external_ref must be recorded after a successful submit"
    );
    assert_eq!(
        record
            .external_ref
            .as_ref()
            .and_then(|external| external.metadata.as_ref())
            .and_then(|metadata| metadata.get("invocation_id")),
        Some(&serde_json::json!("inv_task_1"))
    );
}

#[tokio::test]
async fn ingress_sweep_resumes_latest_segment_without_duplicate_segment_zero() {
    let registry = process_registry();
    registry
        .register_process(rerunnable_registration("mid-chain"))
        .await
        .expect("register");
    registry
        .put_segment_handover(
            "mid-chain",
            lash_core::PersistedSegmentHandover {
                segment_ordinal: 3,
                program_hash: "program-v1".to_string(),
                handover: lash_core::SegmentHandover {
                    reason: lash_core::BoundaryReason::JournalBudget,
                    program_hash: Some("program-v1".to_string()),
                    engine_state: vec![3],
                },
            },
        )
        .await
        .expect("persist live segment");
    let (base_url, captured, server) = spawn_restate_http_capture(vec![MockHttpResponse {
        status: "202 Accepted",
        body: r#"{"invocationId":"inv_mid_chain_3","status":"Accepted"}"#,
    }])
    .await;
    let runner = RestateProcessIngressRunner::new(base_url, registry);
    runner.claim_and_run_pending().await.expect("drive pending");
    server.await.expect("capture server");

    let requests = captured.lock().expect("captured lock");
    assert_eq!(requests.len(), 1);
    assert!(
        requests[0].starts_with("POST /LashProcessWorkflow/mid-chain%233/run/send "),
        "recovery must address the latest segment workflow key: {}",
        requests[0]
    );
    assert!(
        requests[0].contains("\"segment_ordinal\":3"),
        "recovery input must preserve the latest ordinal: {}",
        requests[0]
    );
    assert!(!requests[0].starts_with("POST /LashProcessWorkflow/mid-chain/run/send "));
}

#[tokio::test]
async fn ingress_sweep_skips_externally_owned_and_reconciles_abandon_request() {
    // ADR 0019 at the Restate tier: the ingress sweep never POSTs a run for an
    // ExternallyOwned row (Lash does not execute it), but it does reconcile such
    // a row's pending Abandon Request into an `Abandoned{ReconciledRequest}`
    // terminal — mirroring the core sweep's `reconcile_externally_owned_abandon`.
    // A Rerunnable row alongside them still submits, so exactly one ingress call
    // fires and it is for the Lash-executed row.
    let registry = process_registry();
    registry
        .register_process(external_registration("ext-abandon"))
        .await
        .expect("register externally-owned row with pending abandon");
    registry
        .request_process_abandon(
            "ext-abandon",
            lash_core::AbandonRequest {
                requested_by: "operator".to_string(),
                requested_at_ms: 111,
                reason: Some("host retired".to_string()),
            },
        )
        .await
        .expect("record abandon request");
    registry
        .register_process(external_registration("ext-idle"))
        .await
        .expect("register externally-owned row without abandon");
    registry
        .register_process(rerunnable_registration("rerun-1"))
        .await
        .expect("register rerunnable row");

    // The capture server accepts exactly one connection: if any ExternallyOwned
    // row were submitted, a second connect would be attempted and the extra
    // submit would fail, so the single-response server also proves they are not.
    let (base_url, captured, server) = spawn_restate_http_capture(vec![MockHttpResponse {
        status: "202 Accepted",
        body: r#"{"invocationId":"inv_rerun_1","status":"Accepted"}"#,
    }])
    .await;
    let runner = RestateProcessIngressRunner::new(base_url, Arc::clone(&registry));
    runner
        .claim_and_run_pending()
        .await
        .expect("sweep skips externally-owned rows and submits the rerunnable one");
    server.await.expect("mock ingress server task");

    let requests = captured.lock().expect("captured lock").clone();
    assert_eq!(
        requests.len(),
        1,
        "only the Rerunnable row is submitted; ExternallyOwned rows are never POSTed"
    );
    assert!(
        requests[0].starts_with("POST /LashProcessWorkflow/rerun-1/run/send "),
        "the single submit is the Lash-executed row: {}",
        requests[0]
    );

    // The abandon-request externally-owned row is now terminal Abandoned, written
    // by the reconciled-request path with no Lash execution owner to name.
    let abandoned = registry
        .get_process("ext-abandon")
        .await
        .expect("get reconciled row");
    assert!(
        abandoned.is_terminal(),
        "an externally-owned row with a pending abandon request is reconciled to terminal"
    );
    let Some(ProcessAwaitOutput::Abandoned { evidence, .. }) = abandoned.status.await_output()
    else {
        panic!("expected Abandoned terminal, got {:?}", abandoned.status);
    };
    assert_eq!(evidence.writer, AbandonWriter::ReconciledRequest);
    assert!(
        evidence.owner.is_none(),
        "externally-owned work has no Lash execution owner to name"
    );

    // The externally-owned row without an abandon request is left untouched for
    // its external owner to complete.
    let idle = registry
        .get_process("ext-idle")
        .await
        .expect("get idle externally-owned row");
    assert!(
        !idle.is_terminal(),
        "an externally-owned row with no abandon request is left non-terminal"
    );
}

struct MockHttpResponse {
    status: &'static str,
    body: &'static str,
}

async fn read_http_request(socket: &mut tokio::net::TcpStream) -> String {
    use tokio::io::AsyncReadExt;

    let mut buf = Vec::new();
    let mut scratch = [0u8; 1024];
    loop {
        let n = socket.read(&mut scratch).await.expect("read request");
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&scratch[..n]);
        let Some(header_end) = buf.windows(4).position(|window| window == b"\r\n\r\n") else {
            continue;
        };
        let headers = String::from_utf8_lossy(&buf[..header_end]);
        let content_length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())
                    .flatten()
            })
            .unwrap_or(0);
        if buf.len() >= header_end + 4 + content_length {
            break;
        }
    }
    String::from_utf8_lossy(&buf).into_owned()
}

async fn spawn_restate_http_capture(
    responses: Vec<MockHttpResponse>,
) -> (String, Arc<Mutex<Vec<String>>>, tokio::task::JoinHandle<()>) {
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpListener;

    let captured = Arc::new(Mutex::new(Vec::new()));
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let captured_server = Arc::clone(&captured);
    let server = tokio::spawn(async move {
        for response in responses {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let request = read_http_request(&mut socket).await;
            captured_server.lock().expect("captured lock").push(request);
            let body = response.body.as_bytes();
            let header = format!(
                "HTTP/1.1 {}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                response.status,
                body.len()
            );
            socket
                .write_all(header.as_bytes())
                .await
                .expect("write response headers");
            socket.write_all(body).await.expect("write response body");
            socket.flush().await.expect("flush");
        }
    });
    (format!("http://{addr}"), captured, server)
}

#[tokio::test]
async fn restate_ingress_client_parses_send_invocation_id() {
    let (base_url, captured, server) = spawn_restate_http_capture(vec![MockHttpResponse {
        status: "202 Accepted",
        body: r#"{"invocationId":"inv_123","status":"Accepted"}"#,
    }])
    .await;
    let client = RestateIngressClient::new(base_url);

    let invocation_id = client
        .send_workflow_json(
            "WorkbenchTurnWorkflow",
            "turn-1",
            "run",
            &serde_json::json!({ "turn_id": "turn-1" }),
        )
        .await
        .expect("send workflow");
    server.await.expect("capture server");

    assert_eq!(invocation_id.as_str(), "inv_123");
    let requests = captured.lock().expect("captured lock");
    assert!(
        requests[0].starts_with("POST /WorkbenchTurnWorkflow/turn-1/run/send "),
        "unexpected request: {}",
        requests[0]
    );
    assert!(requests[0].contains(r#""turn_id":"turn-1""#));
}

#[derive(Debug)]
struct ScriptedHttpTransport {
    requests: Mutex<Vec<HttpRequest>>,
    responses: Mutex<VecDeque<HttpResponse>>,
}

impl ScriptedHttpTransport {
    fn new(responses: impl IntoIterator<Item = HttpResponse>) -> Self {
        Self {
            requests: Mutex::new(Vec::new()),
            responses: Mutex::new(responses.into_iter().collect()),
        }
    }

    fn requests(&self) -> Vec<HttpRequest> {
        self.requests.lock().expect("requests lock").clone()
    }
}

#[async_trait::async_trait]
impl HttpTransport for ScriptedHttpTransport {
    async fn send(
        &self,
        request: HttpRequest,
        _timeout: Option<Duration>,
    ) -> Result<HttpResponse, HttpTransportError> {
        self.requests.lock().expect("requests lock").push(request);
        self.responses
            .lock()
            .expect("responses lock")
            .pop_front()
            .ok_or_else(|| HttpTransportError::new("scripted transport exhausted"))
    }
}

#[derive(Debug)]
struct AuthorizationTransport {
    inner: Arc<dyn HttpTransport>,
    token: Arc<RwLock<String>>,
}

#[async_trait::async_trait]
impl HttpTransport for AuthorizationTransport {
    async fn send(
        &self,
        mut request: HttpRequest,
        timeout: Option<Duration>,
    ) -> Result<HttpResponse, HttpTransportError> {
        let token = self.token.read().expect("token lock").clone();
        request
            .headers
            .push(("authorization".to_string(), format!("Bearer {token}")));
        self.inner.send(request, timeout).await
    }
}

fn accepted_response(invocation_id: &str) -> HttpResponse {
    HttpResponse {
        status: 202,
        headers: vec![("content-type".to_string(), "application/json".to_string())],
        body: HttpResponseBody::buffered(format!(
            r#"{{"invocationId":"{invocation_id}","status":"Accepted"}}"#
        )),
    }
}

#[tokio::test]
async fn host_transport_injects_authorization_on_ingress_submit() {
    let scripted = Arc::new(ScriptedHttpTransport::new([accepted_response("inv_auth")]));
    let token = Arc::new(RwLock::new("cloud-token".to_string()));
    let decorated: Arc<dyn HttpTransport> = Arc::new(AuthorizationTransport {
        inner: scripted.clone(),
        token,
    });
    let connection = RestateConnection::with_transport("https://cloud.example", decorated);
    let client = RestateIngressClient::new(connection);

    client
        .send_service_json("LashService", "run", &serde_json::json!({"input": "hello"}))
        .await
        .expect("authenticated ingress submit");

    let requests = scripted.requests();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].headers.iter().any(|(name, value)| {
        name.eq_ignore_ascii_case("authorization") && value == "Bearer cloud-token"
    }));
}

#[tokio::test]
async fn ingress_unauthorized_error_mentions_status_401() {
    let scripted: Arc<dyn HttpTransport> = Arc::new(ScriptedHttpTransport::new([HttpResponse {
        status: 401,
        headers: Vec::new(),
        body: HttpResponseBody::buffered(r#"{"message":"missing bearer token"}"#),
    }]));
    let client = RestateIngressClient::new(RestateConnection::with_transport(
        "https://cloud.example",
        scripted,
    ));

    let error = client
        .send_service_json("LashService", "run", &serde_json::json!({}))
        .await
        .expect_err("unauthorized submit must fail");

    assert!(error.to_string().contains("status 401"), "{error}");
    assert!(
        error.to_string().contains("missing bearer token"),
        "{error}"
    );
}

#[tokio::test]
async fn authorization_decorator_reads_rotated_credentials_per_request() {
    let scripted = Arc::new(ScriptedHttpTransport::new([
        accepted_response("inv_first"),
        accepted_response("inv_second"),
    ]));
    let token = Arc::new(RwLock::new("first-token".to_string()));
    let decorated: Arc<dyn HttpTransport> = Arc::new(AuthorizationTransport {
        inner: scripted.clone(),
        token: Arc::clone(&token),
    });
    let client = RestateIngressClient::new(RestateConnection::with_transport(
        "https://cloud.example",
        decorated,
    ));

    client
        .send_service_json("LashService", "run", &serde_json::json!({"attempt": 1}))
        .await
        .expect("first submit");
    *token.write().expect("token lock") = "second-token".to_string();
    client
        .send_service_json("LashService", "run", &serde_json::json!({"attempt": 2}))
        .await
        .expect("second submit");

    let authorization = scripted
        .requests()
        .into_iter()
        .map(|request| {
            request
                .headers
                .into_iter()
                .find(|(name, _)| name.eq_ignore_ascii_case("authorization"))
                .expect("authorization header")
                .1
        })
        .collect::<Vec<_>>();
    assert_eq!(authorization, ["Bearer first-token", "Bearer second-token"]);
}

#[tokio::test]
async fn restate_ingress_client_accepts_previously_accepted_send() {
    let (base_url, _captured, server) = spawn_restate_http_capture(vec![MockHttpResponse {
        status: "202 Accepted",
        body: r#"{"invocationId":"inv_duplicate","status":"PreviouslyAccepted"}"#,
    }])
    .await;
    let client = RestateIngressClient::new(base_url);

    let invocation_id = client
        .send_workflow_json(
            "LashProcessWorkflow",
            "process-1",
            "run",
            &serde_json::json!({ "process_id": "process-1" }),
        )
        .await
        .expect("idempotent duplicate send");
    server.await.expect("capture server");

    assert_eq!(invocation_id.as_str(), "inv_duplicate");
}

#[tokio::test]
async fn restate_ingress_client_calls_workflow_and_decodes_output() {
    let (base_url, captured, server) = spawn_restate_http_capture(vec![MockHttpResponse {
        status: "200 OK",
        body: r#"{"type":"success","value":{"ok":true}}"#,
    }])
    .await;
    let client = RestateIngressClient::new(base_url);

    let output: ProcessAwaitOutput = client
        .call_workflow_json(
            "LashProcessWorkflow",
            "process-1",
            "await_terminal",
            &RestateProcessAwaitRequest {
                process_id: "process-1".to_string(),
            },
        )
        .await
        .expect("call workflow");
    server.await.expect("capture server");

    assert_eq!(
        output,
        ProcessAwaitOutput::Success {
            value: serde_json::json!({ "ok": true }),
            control: None,
        }
    );
    let requests = captured.lock().expect("captured lock");
    assert!(
        requests[0].starts_with("POST /LashProcessWorkflow/process-1/await_terminal "),
        "unexpected request: {}",
        requests[0]
    );
    assert!(!requests[0].contains("/send "));
}

#[tokio::test]
async fn restate_ingress_client_pins_effect_replay_with_idempotency_key() {
    let (base_url, captured, server) = spawn_restate_http_capture(vec![MockHttpResponse {
        status: "200 OK",
        body: r#"{"status":"cancelled"}"#,
    }])
    .await;
    let client = RestateIngressClient::new(base_url);

    let output: Resolution = client
        .call_workflow_json_idempotent(
            "LashDurableWaitWorkflow",
            "promise-key",
            "await_resolution",
            &serde_json::json!({}),
            "stable-envelope-hash",
        )
        .await
        .expect("call idempotent workflow");
    server.await.expect("capture server");

    assert_eq!(output, Resolution::Cancelled);
    let requests = captured.lock().expect("captured lock");
    assert!(
        requests[0].contains("idempotency-key: stable-envelope-hash"),
        "explicit effect replay identity must reach Restate: {}",
        requests[0]
    );
}

#[tokio::test]
async fn restate_process_attach_calls_await_terminal_ingress() {
    let (base_url, _captured, server) = spawn_restate_http_capture(vec![MockHttpResponse {
        status: "200 OK",
        body: r#"{"type":"success","value":"attached"}"#,
    }])
    .await;
    let runner = RestateProcessIngressRunner::new(base_url, process_registry());

    let output = runner
        .await_terminal("process-1")
        .await
        .expect("attach await");
    server.await.expect("capture server");

    assert_eq!(
        output,
        ProcessAwaitOutput::Success {
            value: serde_json::json!("attached"),
            control: None,
        }
    );
}

#[tokio::test]
async fn cancel_during_successor_boundary_routes_root_and_await_terminal_resolves() {
    assert_eq!(
        terminal_completion_workflow_key("retained-terminal", 2),
        Some("retained-terminal".to_string())
    );
    let registry = process_registry();
    registry
        .register_process(external_registration("retained-terminal"))
        .await
        .expect("register");
    let expected = ProcessAwaitOutput::Cancelled {
        message: "cancelled after a long chain".to_string(),
        raw: None,
        control: None,
    };
    registry
        .complete_process(
            "retained-terminal",
            expected.clone(),
            lash_core::ProcessCompletionAuthority::external_owner(),
        )
        .await
        .expect("complete");
    let runner = RestateProcessIngressRunner::new("http://127.0.0.1:1", registry);

    assert_eq!(
        runner
            .await_terminal("retained-terminal")
            .await
            .expect("registry terminal bypasses expired workflow key"),
        expected
    );
}

#[tokio::test]
async fn restate_process_attach_maps_ingress_error_to_plugin_error() {
    let (base_url, _captured, server) = spawn_restate_http_capture(vec![MockHttpResponse {
        status: "500 Internal Server Error",
        body: r#"{"message":"boom"}"#,
    }])
    .await;
    let runner = RestateProcessIngressRunner::new(base_url, process_registry());

    let err = runner
        .await_terminal("process-1")
        .await
        .expect_err("attach error");
    server.await.expect("capture server");

    assert!(
        err.to_string()
            .contains("ingress await for process `process-1` failed")
    );
    assert!(err.to_string().contains("status 500"));
    assert!(err.to_string().contains("boom"));
}

/// Like [`spawn_restate_http_capture`], but holds each accepted connection open
/// for `delay` before responding, modeling a durable promise that resolves only
/// once the workflow's `run` completes.
async fn spawn_restate_http_capture_delayed(
    responses: Vec<MockHttpResponse>,
    delay: std::time::Duration,
) -> (String, Arc<Mutex<Vec<String>>>, tokio::task::JoinHandle<()>) {
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpListener;

    let captured = Arc::new(Mutex::new(Vec::new()));
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let captured_server = Arc::clone(&captured);
    let server = tokio::spawn(async move {
        for response in responses {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let request = read_http_request(&mut socket).await;
            captured_server.lock().expect("captured lock").push(request);
            tokio::time::sleep(delay).await;
            let body = response.body.as_bytes();
            let header = format!(
                "HTTP/1.1 {}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                response.status,
                body.len()
            );
            socket
                .write_all(header.as_bytes())
                .await
                .expect("write response headers");
            socket.write_all(body).await.expect("write response body");
            socket.flush().await.expect("flush");
        }
    });
    (format!("http://{addr}"), captured, server)
}

#[tokio::test]
async fn restate_attach_before_run_resolves_with_delayed_workflow_output() {
    // The ingress attach is a synchronous long-hold call issued while the
    // workflow's `run` is still in flight; it resolves only when the durable
    // promise does. A delayed mock stands in for that hold, and the eventual
    // output flows back through the driver's attach.
    let delay = std::time::Duration::from_millis(300);
    let (base_url, captured, server) = spawn_restate_http_capture_delayed(
        vec![MockHttpResponse {
            status: "200 OK",
            body: r#"{"type":"success","value":{"eventual":true}}"#,
        }],
        delay,
    )
    .await;
    let registry = process_registry();
    let deployment = RestateProcessDeployment::new(base_url, Arc::clone(&registry));
    let driver = deployment.process_work_driver();
    // A non-terminal process routes await_terminal through the ingress attach
    // rather than the registry short-circuit.
    driver
        .process_registry()
        .register_process(external_registration("process-1"))
        .await
        .expect("register non-terminal process");

    let started = std::time::Instant::now();
    let output = driver
        .await_terminal("process-1")
        .await
        .expect("attach await resolves with the eventual output");
    let elapsed = started.elapsed();
    server.await.expect("capture server");

    assert_eq!(
        output,
        ProcessAwaitOutput::Success {
            value: serde_json::json!({ "eventual": true }),
            control: None,
        }
    );
    assert!(
        elapsed >= delay,
        "the attach must block on the durable promise until run resolves (waited {elapsed:?})"
    );
    let requests = captured.lock().expect("captured lock");
    assert_eq!(
        requests.len(),
        1,
        "await_terminal issues exactly one ingress call"
    );
    assert!(
        requests[0].starts_with("POST /LashProcessWorkflow/process-1/await_terminal "),
        "unexpected request: {}",
        requests[0]
    );
}

#[tokio::test]
async fn restate_driver_short_circuits_terminal_without_ingress_call() {
    // Empty response set: the capture server accepts nothing, so any ingress
    // call would fail. The registry terminal short-circuit must fire first, so
    // the attach is never consulted for an already-terminal process.
    let (base_url, captured, server) = spawn_restate_http_capture(vec![]).await;
    let registry = process_registry();
    let deployment = RestateProcessDeployment::new(base_url, Arc::clone(&registry));
    let driver = deployment.process_work_driver();
    let output = ProcessAwaitOutput::Success {
        value: serde_json::json!("already-terminal"),
        control: None,
    };
    driver
        .process_registry()
        .register_process(external_registration("process-1"))
        .await
        .expect("register");
    driver
        .process_registry()
        .complete_process(
            "process-1",
            output.clone(),
            lash_core::ProcessCompletionAuthority::external_owner(),
        )
        .await
        .expect("complete");

    let resolved = driver
        .await_terminal("process-1")
        .await
        .expect("terminal short-circuit resolves without ingress");
    server.await.expect("capture server");

    assert_eq!(resolved, output);
    assert!(
        captured.lock().expect("captured lock").is_empty(),
        "a terminal short-circuit must not issue any ingress call"
    );
}

#[tokio::test]
async fn restate_process_attach_maps_malformed_ingress_body_to_plugin_error() {
    // A 2xx response whose body does not decode into ProcessAwaitOutput must
    // surface as a PluginError, not a panic — complementing the non-2xx case.
    let (base_url, _captured, server) = spawn_restate_http_capture(vec![MockHttpResponse {
        status: "200 OK",
        body: "this-is-not-valid-json",
    }])
    .await;
    let runner = RestateProcessIngressRunner::new(base_url, process_registry());

    let err = runner
        .await_terminal("process-1")
        .await
        .expect_err("a malformed ingress body must surface as an error");
    server.await.expect("capture server");

    assert!(
        err.to_string()
            .contains("ingress await for process `process-1` failed"),
        "unexpected error: {err}"
    );
}

/// Records each pushed event's `(event_type, sequence)` in emit order.
#[derive(Clone, Default)]
struct RecordingProcessEventSink {
    events: Arc<Mutex<Vec<(String, u64)>>>,
}

#[async_trait::async_trait]
impl lash_core::ProcessEventSink for RecordingProcessEventSink {
    async fn emit(&self, event: &lash_core::ProcessEvent) {
        self.events
            .lock()
            .expect("sink lock")
            .push((event.event_type.clone(), event.sequence));
    }
}

#[tokio::test]
async fn restate_deployment_sink_funnel_feeds_appended_events() {
    // ADR 0017 names `RestateProcessDeployment::new_with_sink` as the durable
    // hosts' wrap funnel: a sink installed there observes every append made
    // through the deployment's shared registry, including terminal events.
    let sink = RecordingProcessEventSink::default();
    let deployment = RestateProcessDeployment::new_with_sink(
        "http://127.0.0.1:8080",
        process_registry(),
        Some(Arc::new(sink.clone())),
    );
    let registry = deployment.process_work_driver().process_registry();
    registry
        .register_process(
            external_registration("sink-funnel").with_extra_event_types([
                lash_core::ProcessEventType {
                    name: "producer.tick".to_string(),
                    payload_schema: lash_core::LashSchema::any(),
                    semantics: lash_core::ProcessEventSemanticsSpec::default(),
                },
            ]),
        )
        .await
        .expect("register");
    registry
        .append_event(
            "sink-funnel",
            lash_core::ProcessEventAppendRequest::new("producer.tick", serde_json::json!({})),
        )
        .await
        .expect("append");
    registry
        .complete_process(
            "sink-funnel",
            ProcessAwaitOutput::Success {
                value: serde_json::Value::Null,
                control: None,
            },
            lash_core::ProcessCompletionAuthority::external_owner(),
        )
        .await
        .expect("complete");

    assert_eq!(
        sink.events.lock().expect("sink lock").clone(),
        vec![
            ("producer.tick".to_string(), 1),
            ("process.completed".to_string(), 2),
        ],
        "the deployment-wrapped registry feeds every append to the sink"
    );
}

#[tokio::test]
async fn restate_process_attach_is_reentrant_across_sequential_awaits() {
    // The shared await_terminal handler is re-entrant: two sequential attaches
    // each issue an independent ingress call and both succeed.
    let (base_url, captured, server) = spawn_restate_http_capture(vec![
        MockHttpResponse {
            status: "200 OK",
            body: r#"{"type":"success","value":"first"}"#,
        },
        MockHttpResponse {
            status: "200 OK",
            body: r#"{"type":"success","value":"second"}"#,
        },
    ])
    .await;
    let runner = RestateProcessIngressRunner::new(base_url, process_registry());

    let first = runner
        .await_terminal("process-1")
        .await
        .expect("first attach await");
    let second = runner
        .await_terminal("process-1")
        .await
        .expect("second attach await");
    server.await.expect("capture server");

    assert_eq!(
        first,
        ProcessAwaitOutput::Success {
            value: serde_json::json!("first"),
            control: None,
        }
    );
    assert_eq!(
        second,
        ProcessAwaitOutput::Success {
            value: serde_json::json!("second"),
            control: None,
        }
    );
    assert_eq!(
        captured.lock().expect("captured lock").len(),
        2,
        "each await issues an independent ingress call"
    );
}

#[tokio::test]
async fn restate_admin_client_cancels_kills_and_queries_invocation_status() {
    let (base_url, captured, server) = spawn_restate_http_capture(vec![
        MockHttpResponse {
            status: "202 Accepted",
            body: "",
        },
        MockHttpResponse {
            status: "200 OK",
            body: "",
        },
        MockHttpResponse {
            status: "200 OK",
            body: r#"{"rows":[{"id":"inv_123","target":"WorkbenchTurnWorkflow/turn-1/run","target_service_name":"WorkbenchTurnWorkflow","target_service_key":"turn-1","target_handler_name":"run","status":"completed","completion_result":"success","completion_failure":null}]}"#,
        },
        MockHttpResponse {
            status: "200 OK",
            body: r#"{"rows":[{"id":"inv_456","target":"WorkbenchTurnWorkflow/turn-2/run","target_service_name":"WorkbenchTurnWorkflow","target_service_key":"turn-2","target_handler_name":"run","status":"suspended"}]}"#,
        },
    ])
    .await;
    let client = RestateAdminClient::new(base_url);
    let invocation_id = RestateInvocationId::new("inv_123");

    client
        .cancel_invocation(&invocation_id)
        .await
        .expect("cancel");
    client
        .kill_invocation_for_test_cleanup(&invocation_id)
        .await
        .expect("kill");
    let status = client
        .invocation_status(&invocation_id)
        .await
        .expect("status")
        .expect("status row");
    let workflow_status = client
        .workflow_invocation_status("WorkbenchTurnWorkflow", "turn-2", "run")
        .await
        .expect("workflow status")
        .expect("workflow status row");
    server.await.expect("capture server");

    assert!(status.completed_successfully());
    assert_eq!(status.target_service_name, "WorkbenchTurnWorkflow");
    assert!(workflow_status.is_still_active());
    let requests = captured.lock().expect("captured lock");
    assert!(
        requests[0].starts_with("PATCH /invocations/inv_123/cancel "),
        "unexpected cancel request: {}",
        requests[0]
    );
    assert!(
        requests[1].starts_with("PATCH /invocations/inv_123/kill "),
        "unexpected kill request: {}",
        requests[1]
    );
    assert!(
        requests[2].starts_with("POST /query "),
        "unexpected query request: {}",
        requests[2]
    );
    assert!(requests[2].contains("FROM sys_invocation WHERE id = 'inv_123'"));
    assert!(requests[3].contains(
        "target_service_name = 'WorkbenchTurnWorkflow' AND target_service_key = 'turn-2' AND target_handler_name = 'run'"
    ));
}
