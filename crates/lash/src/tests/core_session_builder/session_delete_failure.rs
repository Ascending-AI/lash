#[tokio::test]
async fn facade_session_delete_failure_preserves_witnessed_partial_report() -> Result<()> {
    let factory = Arc::new(DeletingStoreFactory::default());
    let expected_partial = lash_core::SessionBlobReclaimReport {
        enumerated_blob_count: 4,
        retained_blob_count: 1,
        deleted_blob_count: 2,
    };
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(mock_provider())
        .model(mock_model_spec())
        .store_factory(factory.clone())
        .build(crate::testing::runtime_lease_owner())?;
    drop(core.session("delete-partial-report").open().await?);
    factory.fail_next_delete(lash_core::MaintenanceFailure::failed(
        lash_core::StoreError::Backend("injected facade delete failure".to_string()),
        expected_partial.clone(),
    ));

    let error = core
        .delete_session(
            "delete-partial-report",
            session_delete_scope(&core, "delete-partial-report").await,
        )
        .await
        .expect_err("injected storage failure must reach the facade caller");

    match error {
        EmbedError::SessionDeleteStorage {
            session_id,
            failure,
        } => {
            assert_eq!(session_id, "delete-partial-report");
            match *failure {
                lash_core::MaintenanceFailure {
                    stop: lash_core::MaintenanceStop::Failed(lash_core::StoreError::Backend(message)),
                    partial,
                } => {
                    assert_eq!(message, "injected facade delete failure");
                    assert_eq!(partial, expected_partial);
                }
                other => panic!("facade must preserve the typed maintenance stop, got {other:?}"),
            }
        }
        other => panic!("facade must preserve the typed partial report, got {other:?}"),
    }
    Ok(())
}
