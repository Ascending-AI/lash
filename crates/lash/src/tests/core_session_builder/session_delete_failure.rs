use super::*;

/// A failed storage delete is one attempt of the session's `SessionDelete`
/// obligation (ADR 0109 §4): the call answers the session closing with the
/// typed stop and the partial report the storage witnessed (ADR 0067), and
/// the relay's next attempt, after the backoff, deletes it.
#[tokio::test]
async fn facade_session_delete_failure_preserves_witnessed_partial_report() -> Result<()> {
    let catalog = Arc::new(std::sync::OnceLock::<
        Arc<lash_core::testing::runtime_helpers::RecordingDeploymentStore>,
    >::new());
    let armed = Arc::clone(&catalog);
    let backend = DecoratedBackend::over(double_backend_explicit_reconcile().await)
        .session_store_factory(move |inner| {
            let recording = Arc::new(
                lash_core::testing::runtime_helpers::RecordingDeploymentStore::over(inner),
            );
            let _ = armed.set(Arc::clone(&recording));
            recording
        });
    let expected_partial = lash_core::SessionBlobReclaimReport {
        enumerated_blob_count: 4,
        retained_blob_count: 1,
        deleted_blob_count: 2,
    };
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        backend.into(),
        crate::TurnBudget::Unbounded,
    ))
    .provider(mock_provider())
    .model(mock_model_spec())
    .build(crate::testing::runtime_lease_owner())?;
    let session = core
        .session("delete-partial-report")
        .created()
        .await
        .open()
        .await?;
    session
        .send(TurnInput::text("materialize the session"))
        .output()
        .await?;
    drop(session);
    catalog
        .get()
        .expect("the core built its catalog")
        .fail_next_delete(lash_core::MaintenanceFailure::failed(
            lash_core::StoreError::Backend("injected facade delete failure".to_string()),
            expected_partial.clone(),
        ));

    let deletion = delete_bound_session_outcome(&core, "delete-partial-report").await?;
    let crate::SessionDeletion::Closing(closing) = deletion else {
        panic!("a failed storage delete leaves the session closing, got {deletion:?}");
    };
    assert_eq!(closing.session_id, "delete-partial-report");
    let obligation = closing
        .obligation
        .clone()
        .expect("the close's acknowledgement armed the delete");
    match closing.waiting {
        crate::SessionDeleteWait::Failed(crate::SessionDeleteFailure::Storage(failure)) => {
            match *failure {
                lash_core::MaintenanceFailure {
                    stop:
                        lash_core::MaintenanceStop::Failed(lash_core::StoreError::Backend(message)),
                    partial,
                } => {
                    assert_eq!(message, "injected facade delete failure");
                    assert_eq!(partial, expected_partial);
                }
                other => panic!("the typed maintenance stop is preserved, got {other:?}"),
            }
        }
        other => panic!("the typed partial report is preserved, got {other:?}"),
    }
    assert!(
        !core
            .session("delete-partial-report")
            .durable()
            .await?
            .was_deleted()
            .await?,
        "nothing was deleted: the storage delete is the last step"
    );

    // The relay retries the obligation after its backoff, never before.
    let administration = core.session_administration().await;
    let relay = lash_core::session_delete::SessionDeleteRelay::new(administration);
    let now = core_now_ms(&core);
    let page = std::num::NonZeroUsize::new(8).expect("non-zero page");
    let early = lash_core::runtime::drive::relay::relay_due(
        &relay,
        &lash_core::testing::TestClock::new(now),
        page,
    )
    .await?;
    assert_eq!(early.claimed, 0, "the retry waits out its backoff");
    let pass = lash_core::runtime::drive::relay::relay_due(
        &relay,
        &lash_core::testing::TestClock::new(now + 2_000),
        page,
    )
    .await?;
    assert_eq!(pass.claimed, 1, "the retry claims the delete: {pass:?}");
    assert_eq!(
        pass.claim_lost, 1,
        "the physical delete removed the row its obligation lived on: {pass:?}"
    );
    assert!(
        core.session("delete-partial-report")
            .durable()
            .await?
            .was_deleted()
            .await?
    );
    assert_eq!(
        core.backend
            .obligation_ledger(lash_core::store::ObligationKind::SessionDelete)
            .state(&obligation)
            .await?,
        None,
        "the delivered obligation went with its row"
    );
    Ok(())
}
