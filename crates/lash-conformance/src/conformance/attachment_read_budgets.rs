use super::{LawBackend, law_session_store};
use crate::*;
use lash_core::llm::types::{LlmContentBlock, LlmMessage, LlmRequest, LlmRole, LlmToolChoice};
use lash_core::testing::TestTurnExecution as _;
use std::sync::atomic::{AtomicUsize, Ordering};

fn request(sources: Vec<AttachmentSource>) -> LlmRequest {
    LlmRequest {
        instructions: None,
        model: lash_sansio::llm_profile::LlmProfileConfig::new(
            lash_sansio::llm_profile::RecordedLlmProfile::mint(
                lash_sansio::llm_profile::LlmProfileKey::new("request-fixture"),
                lash_sansio::llm_profile::LlmProfileMetadata::new(
                    "fixture",
                    std::num::NonZeroUsize::MIN.saturating_add(127_999),
                )
                .with_capability(Default::default())
                .with_extra_body(Default::default())
                .with_request_defaults(Default::default()),
            ),
        )
        .with_reasoning(Default::default()),
        messages: vec![LlmMessage::new(
            LlmRole::User,
            sources
                .into_iter()
                .map(|source| LlmContentBlock::Attachment {
                    source: Box::new(source),
                })
                .collect(),
        )],
        resolved_stored: Default::default(),
        tools: Arc::new(vec![]),
        tool_choice: LlmToolChoice::None,
        attachment_acceptance: Default::default(),
        scope: lash_core::llm::types::LlmRequestScope::new("session", "frame", "call"),
        output_spec: None,
        stream_events: None,
        generation: Default::default(),
        provider_trace: None,
    }
}
#[expect(clippy::unwrap_used, reason = "fixture MIME is valid")]
fn meta() -> AttachmentCreateMeta {
    AttachmentCreateMeta::new(MediaType::parse("image/png").unwrap(), None, None)
}
/// Every attachment backend bounds actual bytes before returning a retained blob.
#[expect(clippy::expect_used, reason = "conformance fixture setup must succeed")]
pub async fn attachment_materialization_read_budgets(backend: Arc<dyn AttachmentStore>) {
    let first = backend
        .put(vec![1; 4], meta())
        .await
        .expect("put legal blob");
    let second = backend
        .put(vec![2; 4], meta())
        .await
        .expect("put second legal blob");
    let mut large = backend
        .put(vec![3; 5], meta())
        .await
        .expect("put foreign-runtime blob");
    large.byte_len = 0;
    let store = RuntimeAttachmentStore::ephemeral(Arc::clone(&backend)).with_read_policy(
        AttachmentReadPolicy {
            max_blob_bytes: 4,
            max_request_bytes: 8192,
        },
    );
    assert!(matches!(
        backend.get(&large.id, 4).await,
        Err(AttachmentStoreError::ReadLimitExceeded { .. })
    ));
    assert_eq!(
        backend
            .get(&first.id, 4)
            .await
            .expect("exact limit")
            .bytes
            .len(),
        4
    );
    assert!(
        resolve_llm_request_attachments(request(vec![AttachmentSource::stored(large)]), &store)
            .await
            .is_err()
    );
    let bounded = store.reconfigured_read_policy(AttachmentReadPolicy {
        max_blob_bytes: 4,
        max_request_bytes: 2548,
    });
    assert!(
        resolve_llm_request_attachments(
            request(vec![
                AttachmentSource::stored(first.clone()),
                AttachmentSource::stored(second)
            ]),
            &bounded
        )
        .await
        .is_err()
    );
    let per_occurrence = 4 * 8 + 1024 + 24 * 9;
    let exact = store.reconfigured_read_policy(AttachmentReadPolicy {
        max_blob_bytes: 4,
        max_request_bytes: 4 + 2 * per_occurrence,
    });
    let resolved = resolve_llm_request_attachments(
        request(vec![
            AttachmentSource::stored(first.clone()),
            AttachmentSource::stored(first),
        ]),
        &exact,
    )
    .await
    .expect("deduplicated retained bytes fit exactly");
    assert_eq!(resolved.resolved_stored.len(), 1);
}

/// Failed reads settle a durable turn and never reach the provider. A repeated
/// ID fits when its bytes are charged once, while both encodings are charged.
#[expect(clippy::expect_used, reason = "conformance fixture setup must succeed")]
pub async fn attachment_materialization_turn_witnesses(
    prefix: &str,
    _effect_host: ActorContext,
    stores: Arc<dyn StoreSet>,
    runner: Arc<dyn ConformanceTurnRunner>,
) {
    for (case, max_request_bytes, expected_calls) in
        [(0, 8192, 0), (1, 2548, 0), (2, 2548, 1), (3, 2500, 0)]
    {
        let session_id = SessionId::fixture(format!("{prefix}-attachment-budget-{case}"));
        let turn_id = TurnId::fixture(format!("{prefix}-attachment-budget-turn-{case}"));
        let backend = RuntimeAttachmentStore::new(
            stores.attachment_store(),
            stores.attachment_referrers(),
            crate::RuntimeOwner::Session(session_id.clone()),
        );
        let mut first = backend
            .put(vec![case + 1; if case == 0 { 5 } else { 4 }], meta())
            .await
            .expect("seed attachment");
        first.byte_len = 0;
        let second = if case == 1 {
            backend.put(vec![9; 4], meta()).await.expect("second blob")
        } else {
            first.clone()
        };
        let calls = Arc::new(AtomicUsize::new(0));
        let provider = crate::testing::TestProvider::builder()
            .kind("stub")
            .complete({
                let calls = Arc::clone(&calls);
                move |request| {
                    calls.fetch_add(1, Ordering::SeqCst);
                    assert_eq!(request.resolved_stored.len(), 1);
                    async { Ok(crate::LlmResponse::default()) }
                }
            })
            .build()
            .into_handle();
        let mut host = LawBackend::over_stores(Arc::clone(&stores))
            .host_config(
                CommitBudget::bounded(1024 * 1024, 512),
                QueuedWorkBatchingConfig::new(1),
            )
            .with_attachment_read_policy(AttachmentReadPolicy {
                max_blob_bytes: 4,
                max_request_bytes,
            });
        host.providers.models = crate::testing::standard_test_llm_profiles(provider);
        let store = law_session_store(stores.as_ref(), &session_id).await;
        let mut policy = crate::testing::mock_session_policy();
        policy.attachment_acceptance = attachment_test_acceptance();
        let state = RuntimeSessionState {
            session_id: session_id.clone(),
            policy: policy.clone(),
            ..RuntimeSessionState::new(policy.clone())
        };
        let mut input = TurnInput::text("read these attachments")
            .with_attachment(AttachmentSource::stored(first));
        if case != 0 {
            input = input.with_attachment(AttachmentSource::stored(second));
        }
        input.trace_turn_id = Some(turn_id.clone());
        let durable_store = Arc::clone(&store);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        runner
            .run_turn(
                crate::admit(ExecutionScope::turn(&session_id, &turn_id)),
                Arc::new(move |scope| {
                    let host = host.clone();
                    let policy = policy.clone();
                    let state = state.clone();
                    let store = Arc::clone(&store);
                    let input = input.clone();
                    let tx = tx.clone();
                    Box::pin(async move {
                        let session_store =
                            super::helpers::session_view(&store, state.session_id.clone());
                        let mut runtime = Box::pin(
                            LashRuntime::builder(host, crate::testing::runtime_lease_owner())
                                .with_policy(policy)
                                .with_initial_state(state)
                                .with_plugin_factories(
                                    crate::testing::test_standard_protocol_factories(),
                                )
                                .with_store(session_store)
                                .build(),
                        )
                        .await
                        .expect("runtime");
                        let result = runtime
                            .execute_turn(
                                input,
                                TurnOptions::new(tokio_util::sync::CancellationToken::new(), scope),
                            )
                            .await;
                        let end = ConformanceTurnEnd::of(&result);
                        tx.send(result).expect("turn witness receiver");
                        end
                    })
                }),
            )
            .await;
        let turn = rx
            .recv()
            .await
            .expect("turn completed")
            .expect("refusal settles the turn");
        assert!(
            durable_store
                .load_session_head_meta(&session_id)
                .await
                .expect("durable turn head")
                .is_some_and(|head| head.head_revision > 0),
            "case {case} must commit its settled turn"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            expected_calls,
            "case {case}, errors {:?}",
            turn.errors
        );
        if expected_calls == 0 {
            assert!(
                turn.errors.iter().any(|issue| issue.code
                    == Some(crate::FailureCode::lash(
                        crate::TurnFailureCode::AttachmentResolutionFailed
                    ))),
                "case {case}: {:?}",
                turn.errors
            );
        }
    }
}
