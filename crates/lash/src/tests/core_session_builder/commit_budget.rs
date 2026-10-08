//! A turn's commit is refused typed before it lands when its rows, the
//! attachment-intent adoption rows included, pass the core's node budget
//! (FIG-5307, on the durable substrate).

use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn adopted_attachment_intent_rows_fail_the_node_budget_before_commit() {
    const CONFIGURED_ROW_LIMIT: usize = 3;
    let provider = crate::testing::TestProvider::builder()
        .kind("adoption-row-budget")
        .complete(|_request| async move { Ok(text_response("assistant response")) })
        .build()
        .into_handle();
    let core = explicit_ephemeral_facets_with_budget(
        LashCore::standard_builder(sqlite_memory_store_backend().await),
        crate::CommitBudget::new(
            crate::CommitBudgetLimit::Unbounded,
            crate::CommitBudgetLimit::bounded(CONFIGURED_ROW_LIMIT),
        ),
    )
    .serve_test_llm_profile(provider, mock_llm_profile_spec())
    .build(crate::testing::runtime_lease_owner())
    .expect("core");

    // Precondition: a turn with no attachment fits the budget.
    core.session(crate::SessionId::parse("commit-graph-only-budget-surface").expect("id"))
        .create(crate::SessionCreation::root(
            crate::plugins::SessionToolAccess::ambient(),
            mock_session_spec(),
        ))
        .await
        .expect("created")
        .send(crate::TurnInput::text("graph rows only"))
        .output()
        .await
        .expect("a graph-only turn fits the node budget");

    core.session(crate::SessionId::parse("commit-adoption-row-budget-surface").expect("id"))
        .create(crate::SessionCreation::root(
            crate::plugins::SessionToolAccess::ambient(),
            mock_session_spec(),
        ))
        .await
        .expect("created");
    let session = core
        .session(crate::SessionId::from("commit-adoption-row-budget-surface"))
        .open()
        .await
        .expect("open");
    let png = || lash_core::MediaType::parse("image/png").expect("image media type");
    let first = session
        .put_attachment(
            vec![1, 2, 3],
            lash_core::AttachmentCreateMeta::new(png(), None, None),
        )
        .await
        .expect("put first");
    let second = session
        .put_attachment(
            vec![4, 5, 6],
            lash_core::AttachmentCreateMeta::new(png(), None, None),
        )
        .await
        .expect("put second");
    let error = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        session
            .send(
                crate::TurnInput::text("adopt two attachments")
                    .with_attachment(first)
                    .with_attachment(second),
            )
            .output(),
    )
    .await
    .expect("the over-budget turn settles")
    .expect_err("the adoption rows push the commit past its row limit");
    let EmbedError::Runtime(runtime_error) = &error else {
        panic!("expected a host-visible runtime error, got {error}");
    };
    assert_eq!(
        runtime_error.code,
        lash_core::RuntimeErrorCode::StoreCommitNodeBudgetExceeded,
        "{runtime_error:?}"
    );
    assert!(
        runtime_error.message.contains(&format!(
            "exceeding the configured {CONFIGURED_ROW_LIMIT}-row node budget"
        )),
        "{}",
        runtime_error.message
    );
    assert!(
        runtime_error
            .message
            .contains("including attachment-intent adoption"),
        "{}",
        runtime_error.message
    );
    assert!(error.is_terminal(), "{error}");
    assert!(!error.is_retryable(), "{error}");
    core.shutdown().await.expect("shutdown");
}
