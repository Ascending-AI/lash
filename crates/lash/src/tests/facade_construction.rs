/// FIG-5020: every durable facade feed has a bounded page and an explicit
/// continuation, including an empty page; time cutoffs are validated instants.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn facade_change_feeds_share_bounded_pages_and_typed_cutoffs() -> crate::Result<()> {
    let core = super::explicit_ephemeral_facets(crate::LashCore::standard_builder(
        super::sqlite_memory_store_backend().await,
    ))
    .serve_test_llm_profile(super::mock_provider(), super::mock_llm_profile_spec())
    .build(crate::tests::host::runtime_lease_owner())?;
    let limit = std::num::NonZeroUsize::new(1).unwrap();
    let turns: crate::ChangePage<
        crate::persistence::TurnChange,
        crate::persistence::TurnChangeCursor,
    > = core
        .turns_changed_since(crate::persistence::TurnChangeCursor::initial(), limit)
        .await?;
    assert!(turns.changes.is_empty());
    assert_eq!(turns.next, crate::persistence::TurnChangeCursor::initial());
    assert_eq!(turns.retained_after, Some(turns.next));
    // Standing-fault pages retain the last item as their continuation; an
    // empty page retains the caller's cursor instead of restarting the feed.
    for label in ["fault-a", "fault-b"] {
        core.session(
            lash_vocabulary::identity::SessionId::parse(label).expect("nonblank host identity"),
        )
        .create(crate::SessionCreation::root(super::mock_session_spec()))
        .await?;
        let record = crate::SessionFaultRecord::new(
            crate::SessionFaultOrigin::DriveAdmission,
            &crate::runtime::RuntimeError::new(
                crate::runtime::RuntimeErrorCode::RuntimeStoreCorrupt,
                "corrupt",
            ),
        );
        core.store_factory
            .record_session_fault(
                &lash_vocabulary::identity::SessionId::parse(label).unwrap(),
                &record,
                1,
            )
            .await?;
    }
    let first = core.session_faults(None, limit).await?;
    assert_eq!(first.changes.len(), 1);
    assert_eq!(first.next.as_ref().unwrap().as_str(), "fault-a");
    let second = core.session_faults(first.next.as_ref(), limit).await?;
    assert_eq!(second.changes.len(), 1);
    assert_eq!(second.next.as_ref().unwrap().as_str(), "fault-b");
    let after = lash_vocabulary::identity::SessionId::parse("last-fault").unwrap();
    let faults: crate::ChangePage<
        crate::SessionFault,
        Option<lash_vocabulary::identity::SessionId>,
    > = core.session_faults(Some(&after), limit).await?;
    assert!(faults.changes.is_empty());
    assert_eq!(faults.next, Some(after));
    assert_eq!(faults.retained_after, None);
    Ok(())
}
