//! Cross-layer attachment owner / cold effect-replay conformance.

use super::*;
use lash_sansio::sync::MutexExt;
use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};

pub type ReopenEffectControllerFuture =
    std::pin::Pin<Box<dyn Future<Output = Arc<dyn crate::RuntimeEffectController>> + Send>>;
pub type ReopenEffectController = Arc<dyn Fn() -> ReopenEffectControllerFuture + Send + Sync>;

/// Durable handles needed by [`attachment_owner_cold_replay`]. The two effect
/// controllers must be independently opened over the same journal; the vector
/// deliberately never calls `start_replay` on the first controller.
pub struct AttachmentOwnerColdReplayBackend {
    pub session_store_factory: Arc<dyn crate::SessionStoreFactory>,
    pub process_registry: Arc<dyn crate::ProcessRegistry>,
    pub attachment_store: Arc<dyn crate::AttachmentStore>,
    pub first_effect_controller: Option<Arc<dyn crate::RuntimeEffectController>>,
    pub reopen_effect_controller: ReopenEffectController,
    pub clock: Arc<dyn crate::Clock>,
    pub advance_clock: Arc<dyn Fn(u64) + Send + Sync>,
}

/// FIG-546 owner-binding vector: ordinary JSON and typed tool outputs survive a
/// cold replay, turn finalization stamps by owner, superseding turns release
/// dead intents, and process intents remain roots exactly until process prune.
#[cfg(any(test, feature = "testing"))]
pub async fn attachment_owner_cold_replay(mut backend: AttachmentOwnerColdReplayBackend) {
    const SESSION_ID: &str = "attachment-owner-cold-replay";
    const TURN_ID: &str = "attachment-owner-turn";
    const PLAIN_BYTES: &[u8] = b"plain-json-owner-bytes";
    const TYPED_BYTES: &[u8] = b"typed-output-owner-bytes";

    let request = session_request(SESSION_ID);
    let store_a = backend
        .session_store_factory
        .create_store(&request)
        .await
        .expect("create attachment owner session store");
    let facade_a = Arc::new(crate::SessionAttachmentStore::new_with_clock(
        Arc::clone(&backend.attachment_store),
        Arc::new(crate::attachments::PersistenceManifestAdapter(Arc::clone(
            &store_a,
        ))),
        SESSION_ID,
        Arc::clone(&backend.clock),
    ));
    let owner_binding_a = facade_a.bind_turn_scoped(TURN_ID);

    let plain_id = Arc::new(Mutex::new(None));
    let typed_id = Arc::new(Mutex::new(None));
    let first_local_calls = Arc::new(AtomicUsize::new(0));
    let plain_envelope = tool_attempt_envelope("plain-json-effect", "plain-json-call", TURN_ID);
    let typed_envelope = tool_attempt_envelope("typed-effect", "typed-call", TURN_ID);

    let first_effect_controller = backend
        .first_effect_controller
        .take()
        .expect("first effect controller");
    let plain_outcome = first_effect_controller
        .execute_effect(
            plain_envelope.clone(),
            attachment_put_executor(
                Arc::clone(&facade_a),
                PLAIN_BYTES,
                false,
                Arc::clone(&plain_id),
                Arc::clone(&first_local_calls),
                "plain-json-call",
            ),
        )
        .await
        .expect("journal plain JSON attachment outcome");
    let typed_outcome = first_effect_controller
        .execute_effect(
            typed_envelope.clone(),
            attachment_put_executor(
                Arc::clone(&facade_a),
                TYPED_BYTES,
                true,
                Arc::clone(&typed_id),
                Arc::clone(&first_local_calls),
                "typed-call",
            ),
        )
        .await
        .expect("journal typed attachment outcome");
    assert_eq!(first_local_calls.load(Ordering::SeqCst), 2);
    let plain_id = plain_id.lock_recover().clone().expect("plain put");
    let typed_id = typed_id.lock_recover().clone().expect("typed put");
    assert_plain_json_outcome(&plain_outcome, &plain_id);
    assert_typed_outcome(&typed_outcome, &typed_id);

    // Cold-object boundary: no facade, session store, or first controller is
    // retained for recovery. The factory/backend are deployment resources, not
    // live per-turn correctness objects.
    drop(owner_binding_a);
    drop(facade_a);
    drop(store_a);
    drop(plain_outcome);
    drop(typed_outcome);
    drop(first_effect_controller);

    (backend.advance_clock)(10_000);
    let pre_recovery = crate::reclaim_unreferenced_attachments(
        &*backend.session_store_factory,
        &*backend.attachment_store,
        0,
    )
    .await
    .expect("pre-recovery GC");
    assert_eq!(
        pre_recovery.reclaimed_count, 0,
        "aged intents with a still-committable turn owner must survive"
    );
    assert_blob(&*backend.attachment_store, &plain_id, PLAIN_BYTES).await;
    assert_blob(&*backend.attachment_store, &typed_id, TYPED_BYTES).await;

    let store_b = backend
        .session_store_factory
        .open_existing_store(&request)
        .await
        .expect("reopen owner store")
        .expect("owner store exists");
    let replay_effect_controller = (backend.reopen_effect_controller)().await;
    let replay_local_calls = Arc::new(AtomicUsize::new(0));
    let replay_plain = replay_effect_controller
        .execute_effect(
            plain_envelope,
            failing_executor(Arc::clone(&replay_local_calls)),
        )
        .await
        .expect("cold replay plain outcome");
    let replay_typed = replay_effect_controller
        .execute_effect(
            typed_envelope,
            failing_executor(Arc::clone(&replay_local_calls)),
        )
        .await
        .expect("cold replay typed outcome");
    assert_eq!(
        replay_local_calls.load(Ordering::SeqCst),
        0,
        "journal replay must not invoke either local tool executor"
    );
    assert_plain_json_outcome(&replay_plain, &plain_id);
    assert_typed_outcome(&replay_typed, &typed_id);

    let stamped_commit =
        final_turn_commit(&store_b, SESSION_ID, TURN_ID, vec![typed_id.clone()]).await;
    let first_result = commit_with_lease(&store_b, stamped_commit.clone(), "first-commit").await;
    let duplicate = store_b
        .commit_runtime_state(stamped_commit)
        .await
        .expect("duplicate final commit is idempotent");
    assert_eq!(duplicate.head_revision, first_result.head_revision);
    assert!(
        store_b
            .list_uncommitted(u64::MAX)
            .expect("list committed owner rows")
            .into_iter()
            .all(|entry| entry.attachment_id != plain_id && entry.attachment_id != typed_id),
        "owner stamping commits JSON-only and typed rows alike"
    );

    let post_commit = crate::reclaim_unreferenced_attachments(
        &*backend.session_store_factory,
        &*backend.attachment_store,
        0,
    )
    .await
    .expect("post-commit GC");
    assert_eq!(post_commit.reclaimed_count, 0);
    let reader = crate::SessionAttachmentStore::new_with_clock(
        Arc::clone(&backend.attachment_store),
        Arc::new(crate::attachments::PersistenceManifestAdapter(Arc::clone(
            &store_b,
        ))),
        SESSION_ID,
        Arc::clone(&backend.clock),
    );
    assert_eq!(
        reader.get(&plain_id).await.expect("resolve plain").bytes,
        PLAIN_BYTES
    );
    assert_eq!(
        reader.get(&typed_id).await.expect("resolve typed").bytes,
        TYPED_BYTES
    );

    superseded_turn_leg(&backend).await;
    process_owner_leg(&backend).await;
}

#[cfg(any(test, feature = "testing"))]
fn attachment_put_executor(
    facade: Arc<crate::SessionAttachmentStore>,
    bytes: &'static [u8],
    typed: bool,
    captured_id: Arc<Mutex<Option<crate::AttachmentId>>>,
    calls: Arc<AtomicUsize>,
    call_id: &'static str,
) -> crate::RuntimeEffectLocalExecutor<'static> {
    crate::RuntimeEffectLocalExecutor::testing(move |_| async move {
        calls.fetch_add(1, Ordering::SeqCst);
        let reference = facade
            .put(bytes.to_vec(), attachment_meta(call_id))
            .await
            .map_err(|err| {
                crate::RuntimeEffectControllerError::foreign("attachment_put", err.to_string())
            })?;
        *captured_id.lock_recover() = Some(reference.id.clone());
        let output = if typed {
            crate::ToolCallOutput::success(crate::ToolValue::Attachment(
                crate::AttachmentSource::stored(reference),
            ))
        } else {
            crate::ToolCallOutput::success(serde_json::json!({
                "attachment_id": reference.id.as_str()
            }))
        };
        Ok(tool_attempt_outcome(call_id, output))
    })
}

#[cfg(any(test, feature = "testing"))]
fn failing_executor(calls: Arc<AtomicUsize>) -> crate::RuntimeEffectLocalExecutor<'static> {
    crate::RuntimeEffectLocalExecutor::testing(move |_| async move {
        calls.fetch_add(1, Ordering::SeqCst);
        Err(crate::RuntimeEffectControllerError::foreign(
            "cold_replay_local_executor_called",
            "cold replay invoked the local attachment tool",
        ))
    })
}

fn tool_attempt_envelope(
    effect_id: &str,
    call_id: &str,
    turn_id: &str,
) -> crate::RuntimeEffectEnvelope {
    crate::RuntimeEffectEnvelope::new(
        crate::RuntimeInvocation::effect(
            crate::RuntimeScope::for_turn("attachment-owner-cold-replay", turn_id, 1, 0),
            effect_id,
            crate::RuntimeEffectKind::ToolAttempt,
            format!("attachment-owner:{turn_id}:{effect_id}"),
        ),
        crate::RuntimeEffectCommand::ToolAttempt {
            call: crate::PreparedToolCall::from_parts(
                call_id,
                crate::ToolId::from(format!("tool:{call_id}")),
                call_id,
                serde_json::json!({}),
                None,
                serde_json::Value::Null,
            ),
            execution_grant: None,
            attempt: 1,
            max_attempts: 1,
        },
    )
}

fn tool_attempt_outcome(
    call_id: &str,
    output: crate::ToolCallOutput,
) -> crate::RuntimeEffectOutcome {
    crate::RuntimeEffectOutcome::ToolAttempt {
        launch: Box::new(crate::ToolAttemptLaunch::Done {
            record: Box::new(crate::ToolCallRecord {
                call_id: Some(call_id.to_string()),
                tool: call_id.to_string(),
                args: serde_json::json!({}),
                output,
                duration_ms: 0,
            }),
        }),
        triggers: Vec::new(),
    }
}

fn assert_plain_json_outcome(outcome: &crate::RuntimeEffectOutcome, id: &crate::AttachmentId) {
    let crate::RuntimeEffectOutcome::ToolAttempt { launch, .. } = outcome else {
        panic!("expected completed tool attempt")
    };
    let crate::ToolAttemptLaunch::Done { record } = launch.as_ref() else {
        panic!("expected completed tool attempt")
    };
    assert!(record.output.attachments().is_empty());
    assert_eq!(
        record
            .output
            .value_for_projection()
            .get("attachment_id")
            .cloned(),
        Some(serde_json::Value::String(id.to_string()))
    );
}

fn assert_typed_outcome(outcome: &crate::RuntimeEffectOutcome, id: &crate::AttachmentId) {
    let crate::RuntimeEffectOutcome::ToolAttempt { launch, .. } = outcome else {
        panic!("expected completed tool attempt")
    };
    let crate::ToolAttemptLaunch::Done { record } = launch.as_ref() else {
        panic!("expected completed tool attempt")
    };
    assert_eq!(
        record
            .output
            .attachments()
            .into_iter()
            .filter_map(|source| source.stored_ref().map(|reference| reference.id.clone()))
            .collect::<Vec<_>>(),
        vec![id.clone()]
    );
}

async fn superseded_turn_leg(backend: &AttachmentOwnerColdReplayBackend) {
    const SESSION_ID: &str = "attachment-owner-superseded";
    let request = session_request(SESSION_ID);
    let store = backend
        .session_store_factory
        .create_store(&request)
        .await
        .expect("create superseded session");
    let facade = Arc::new(crate::SessionAttachmentStore::new_with_clock(
        Arc::clone(&backend.attachment_store),
        Arc::new(crate::attachments::PersistenceManifestAdapter(Arc::clone(
            &store,
        ))),
        SESSION_ID,
        Arc::clone(&backend.clock),
    ));
    let _owner_binding = facade.bind_turn_scoped("crashed-turn");
    let old = facade
        .put(
            b"superseded-owner-bytes".to_vec(),
            attachment_meta("superseded"),
        )
        .await
        .expect("put superseded attachment");
    (backend.advance_clock)(1_000);
    commit_with_lease(
        &store,
        final_turn_commit(&store, SESSION_ID, "later-turn", Vec::new()).await,
        "later-turn-owner",
    )
    .await;
    let report = crate::reclaim_unreferenced_attachments(
        &*backend.session_store_factory,
        &*backend.attachment_store,
        0,
    )
    .await
    .expect("superseded turn GC");
    assert!(report.reclaimed_count >= 1);
    assert!(matches!(
        backend.attachment_store.get(&old.id).await,
        Err(crate::AttachmentStoreError::NotFound(_))
    ));
}

async fn process_owner_leg(backend: &AttachmentOwnerColdReplayBackend) {
    const PROCESS_ID: &str = "attachment-owner-process";
    const SESSION_ID: &str = "process-env:attachment-owner-process";
    backend
        .process_registry
        .register_process(crate::ProcessRegistration::new(
            PROCESS_ID,
            crate::ProcessInput::External {
                metadata: serde_json::Value::Null,
            },
            crate::RecoveryDisposition::ExternallyOwned,
            crate::ProcessProvenance::host(),
        ))
        .await
        .expect("register process attachment owner");
    let store = backend
        .session_store_factory
        .create_store(&session_request(SESSION_ID))
        .await
        .expect("create process attachment store");
    let facade = Arc::new(crate::SessionAttachmentStore::new_with_clock(
        Arc::clone(&backend.attachment_store),
        Arc::new(crate::attachments::PersistenceManifestAdapter(Arc::clone(
            &store,
        ))),
        SESSION_ID,
        Arc::clone(&backend.clock),
    ));
    let _owner_binding = facade.bind_process_scoped(PROCESS_ID);
    let reference = facade
        .put(b"process-owner-bytes".to_vec(), attachment_meta("process"))
        .await
        .expect("put process attachment");
    (backend.advance_clock)(1_000);
    let live_report = crate::reclaim_unreferenced_attachments(
        &*backend.session_store_factory,
        &*backend.attachment_store,
        0,
    )
    .await
    .expect("live process GC");
    assert_eq!(live_report.reclaimed_count, 0);
    assert_blob(
        &*backend.attachment_store,
        &reference.id,
        b"process-owner-bytes",
    )
    .await;

    let terminal = backend
        .process_registry
        .complete_process(
            PROCESS_ID,
            crate::ProcessAwaitOutput::Success {
                value: serde_json::Value::Null,
                control: None,
            },
            crate::ProcessCompletionAuthority::external_owner(),
        )
        .await
        .expect("complete process owner");
    backend
        .process_registry
        .prune_terminal_processes(
            terminal.updated_at_ms.saturating_add(1),
            None,
            crate::ProjectionWatermark::NoProjector,
        )
        .await
        .expect("prune process owner");
    let pruned_report = crate::reclaim_unreferenced_attachments(
        &*backend.session_store_factory,
        &*backend.attachment_store,
        0,
    )
    .await
    .expect("pruned process GC");
    assert!(pruned_report.reclaimed_count >= 1);
    assert!(matches!(
        backend.attachment_store.get(&reference.id).await,
        Err(crate::AttachmentStoreError::NotFound(_))
    ));
}

async fn final_turn_commit(
    store: &Arc<dyn crate::RuntimePersistence>,
    session_id: &str,
    turn_id: &str,
    adopted_attachment_ids: Vec<crate::AttachmentId>,
) -> crate::RuntimeCommit {
    store
        .load_session_meta()
        .await
        .expect("load commit session metadata")
        .expect("commit session metadata exists");
    let state = crate::RuntimeSessionState {
        session_id: session_id.to_string(),
        ..crate::RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    let mut commit = crate::RuntimeCommit::persisted_state_for_test(&state, &[])
        .with_committed_attachments(adopted_attachment_ids);
    commit.turn_commit =
        crate::RuntimeTurnCommitStamp::new(crate::OperationId::turn(session_id, turn_id, "final"));
    commit
}

async fn commit_with_lease(
    store: &Arc<dyn crate::RuntimePersistence>,
    commit: crate::RuntimeCommit,
    owner_id: &str,
) -> crate::store::RuntimeCommitResult {
    let owner = crate::LeaseOwnerIdentity::opaque(owner_id, format!("{owner_id}:incarnation"));
    let lease = store
        .try_claim_session_execution_lease(&commit.session_id, &owner, 60_000)
        .await
        .expect("claim commit lease")
        .acquired()
        .expect("commit lease acquired");
    store
        .commit_runtime_state(commit.releasing_session_execution_lease(lease.completion()))
        .await
        .expect("commit runtime state")
}

fn session_request(session_id: &str) -> crate::SessionStoreCreateRequest {
    crate::SessionStoreCreateRequest {
        session_id: session_id.to_string(),
        relation: crate::SessionRelation::Root,
        policy: crate::SessionPolicy {
            model: crate::ModelSpec::builder("attachment-owner-conformance-model")
                .context_window_tokens(8_192)
                .build()
                .expect("valid model"),
            provider_id: "attachment-owner-conformance".to_string(),
            session_id: Some(session_id.to_string()),
            ..crate::SessionPolicy::new(crate::TurnBudget::Unbounded)
        },
    }
}

fn attachment_meta(label: &str) -> crate::AttachmentCreateMeta {
    crate::AttachmentCreateMeta::new(
        crate::MediaType::parse("image/png").unwrap(),
        Some(crate::AttachmentTypeMetadata::image(Some(1), Some(1))),
        Some(format!("{label}.png")),
    )
}

async fn assert_blob(
    store: &dyn crate::AttachmentStore,
    id: &crate::AttachmentId,
    expected: &[u8],
) {
    assert_eq!(
        store.get(id).await.expect("attachment blob exists").bytes,
        expected
    );
}
