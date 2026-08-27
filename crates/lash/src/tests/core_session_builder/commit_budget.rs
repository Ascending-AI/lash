#[tokio::test]
async fn adopted_attachment_intent_rows_fail_the_node_budget_before_commit() -> Result<()> {
    const CONFIGURED_ROW_LIMIT: usize = 3;
    let provider = crate::testing::TestProvider::builder()
        .kind("adoption-row-budget")
        .complete(|_request| async move { Ok(text_response("assistant response")) })
        .build()
        .into_handle();
    let core = explicit_ephemeral_facets_with_budget(
        LashCore::standard_builder(crate::TurnBudget::Unbounded),
        crate::CommitBudget::new(
            crate::CommitBudgetLimit::Unbounded,
            crate::CommitBudgetLimit::bounded(CONFIGURED_ROW_LIMIT),
        ),
    )
    .provider(provider)
    .model(mock_model_spec())
    .store_factory(Arc::new(
        lash_core::facade_support::InMemorySessionStoreFactory::new(),
    ))
    .with_native_queued_work()
    .build(crate::testing::runtime_lease_owner())?;

    core.session("commit-graph-only-budget-surface")
        .open()
        .await?
        .turn(TurnInput::text("graph rows only"))
        .turn_id("commit-graph-only-budget-turn")
        .run()
        .await?;

    let session = core.session("commit-adoption-row-budget-surface").open().await?;
    let error = session
        .turn(TurnInput::text("adopt one attachment").with_attachment(
            lash_core::AttachmentSource::inline(
                lash_core::MediaType::parse("image/png").expect("image media type"),
                vec![1, 2, 3],
            ),
        ))
        .turn_id("commit-adoption-row-budget-turn")
        .run()
        .await
        .expect_err("the adoption row must push the commit past its row limit");

    let EmbedError::Runtime(runtime_error) = &error else {
        panic!("expected a host-visible runtime error, got {error}");
    };
    assert_eq!(
        runtime_error.code,
        lash_core::RuntimeErrorCode::StoreCommitNodeBudgetExceeded
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
    Ok(())
}
