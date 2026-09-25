//! FIG-3575 on the Restate engine host: a deterministic failure before the
//! model call is recorded as a failed turn, exactly as on the store hosts, and
//! a handler replay over the recorded journal reproduces that same failed turn
//! and commits it once. Before FIG-3575 the Restate tier aborted the
//! invocation instead, so no failure was ever recorded.

use super::*;

#[derive(Default)]
struct RefusingBeforeLlmCall {
    calls: AtomicUsize,
}

#[async_trait::async_trait]
impl lash_core::plugin::ProtocolSessionPlugin for RefusingBeforeLlmCall {
    async fn before_llm_call(
        &self,
        _ctx: lash_core::plugin::ProtocolBeforeLlmCallContext,
        _request: &lash_core::LlmRequest,
    ) -> Result<Option<lash_core::ProtocolLlmCallAction>, lash_core::PluginError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Err(lash_core::PluginError::Invoke(
            "the protocol refuses this request".to_string(),
        ))
    }
}

fn assert_recorded_before_llm_failure(turn: &lash_core::facade_support::AssembledTurn) {
    assert!(
        matches!(
            turn.outcome,
            lash_core::facade_support::TurnOutcome::Stopped(
                lash_core::facade_support::TurnStop::RuntimeError
            )
        ),
        "a deterministic before-LLM failure is recorded as a failed turn: {:?}",
        turn.outcome
    );
    assert!(
        turn.errors
            .iter()
            .any(|issue| issue.kind == lash_core::TurnFailureKind::ProtocolBeforeLlmCall),
        "the recorded turn names the protocol failure: {:?}",
        turn.errors
    );
}

#[tokio::test]
pub(super) async fn restate_before_llm_refusal_is_a_recorded_failed_turn_that_replays_once() {
    let dir = tempfile::tempdir().expect("tempdir");
    let session_id = SessionId::from("restate-before-llm-refusal");
    let turn_id = TurnId::from("restate-before-llm-turn");
    let provider_calls = Arc::new(AtomicUsize::new(0));
    let provider = lash_core::testing::TestProvider::builder()
        .kind("stub")
        .complete({
            let provider_calls = Arc::clone(&provider_calls);
            move |_request| {
                let provider_calls = Arc::clone(&provider_calls);
                async move {
                    provider_calls.fetch_add(1, Ordering::SeqCst);
                    Ok(lash_core::LlmResponse::default())
                }
            }
        })
        .build()
        .into_handle();
    let mut host = memory_host_config().await;
    host.providers.provider_resolver = Arc::new(
        lash_core::facade_support::SingleProviderResolver::new(provider),
    );
    host.durability.attachment_store = Arc::new(
        lash_core::facade_support::SessionAttachmentStore::ephemeral(Arc::new(
            lash_core::facade_support::FileAttachmentStore::new(dir.path().join("attachments")),
        )),
    );
    let store: Arc<dyn lash_core::RuntimePersistence> = Arc::new(
        lash_sqlite_store::Store::open(&dir.path().join("session.db"))
            .await
            .expect("open session store"),
    );
    let policy = replay_test_policy(&session_id);
    let initial_state = replay_test_state(&session_id, &policy);
    let context = Arc::new(ReplayableRecordingContext::default());
    bind_restate_test_effect_host(&mut host, &context);
    let protocol = Arc::new(RefusingBeforeLlmCall::default());
    let plugins = || {
        vec![
            lash_core::testing::test_standard_protocol_factory_with_runtime_state(
                protocol.clone(),
                None,
            ),
        ]
    };

    let mut first = replay_test_runtime_with_plugins(
        &session_id,
        policy.clone(),
        initial_state.clone(),
        host.clone(),
        Arc::clone(&store),
        plugins(),
    )
    .await;
    let first_turn =
        run_restate_replay_turn(&mut first, Arc::clone(&context), &session_id, &turn_id).await;
    assert_recorded_before_llm_failure(&first_turn);
    assert!(!context.runs().is_empty());

    // The handler is redriven over the same journal: the refusal is a
    // function of the journaled inputs, so the replay reaches the same failed
    // turn and the store recognises the one commit it already holds.
    context.start_replay();
    let retry_store: Arc<dyn lash_core::RuntimePersistence> =
        Arc::new(CommitRetryStore::new(Arc::clone(&store)));
    let mut replay = replay_test_runtime_with_plugins(
        &session_id,
        policy,
        initial_state,
        host,
        retry_store,
        plugins(),
    )
    .await;
    let replay_turn =
        run_restate_replay_turn(&mut replay, Arc::clone(&context), &session_id, &turn_id).await;
    assert_recorded_before_llm_failure(&replay_turn);
    assert_eq!(replay_turn.outcome, first_turn.outcome);
    assert_eq!(protocol.calls.load(Ordering::SeqCst), 2);
    assert_eq!(provider_calls.load(Ordering::SeqCst), 0);
    let conn = rusqlite::Connection::open(dir.path().join("session.db"))
        .expect("open raw session sqlite store");
    let rows: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM runtime_turn_commits WHERE session_id = ?1",
            rusqlite::params![session_id.as_str()],
            |row| row.get(0),
        )
        .expect("count turn commit stamps");
    assert_eq!(rows, 1, "the recorded failure commits exactly once");
}
