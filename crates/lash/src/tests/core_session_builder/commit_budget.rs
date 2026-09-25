use super::*;

const SEED: u64 = 0x5c_f101;

#[tokio::test]
async fn adopted_attachment_intent_rows_fail_the_node_budget_before_commit() -> Result<()> {
    const CONFIGURED_ROW_LIMIT: usize = 3;
    let provider = crate::testing::TestProvider::builder()
        .kind("adoption-row-budget")
        .complete(|_request| async move { Ok(text_response("assistant response")) })
        .build()
        .into_handle();
    let double = restate_double(SEED).await;
    let core = backend_work_facets_with_budget(
        LashCore::standard_builder(double.lash_backend(), crate::TurnBudget::Unbounded),
        crate::CommitBudget::new(
            crate::CommitBudgetLimit::Unbounded,
            crate::CommitBudgetLimit::bounded(CONFIGURED_ROW_LIMIT),
        ),
    )
    .provider(provider)
    .model(mock_model_spec())
    .build(crate::testing::runtime_lease_owner())?;

    core.session("commit-graph-only-budget-surface")
        .open()
        .await?
        .send(TurnInput::text("graph rows only"))
        .id("commit-graph-only-budget-turn")
        .output()
        .await?;

    let session = core
        .session("commit-adoption-row-budget-surface")
        .open()
        .await?;
    let error = session
        .send(TurnInput::text("adopt one attachment").with_attachment(
            lash_core::AttachmentSource::inline(
                lash_core::MediaType::parse("image/png").expect("image media type"),
                vec![1, 2, 3],
            ),
        ))
        .id("commit-adoption-row-budget-turn")
        .output()
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
