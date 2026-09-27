use super::*;

// --- Caller departure at the facade seam (ADR 0110) --------------------------

/// Caller departure end to end through the host API (FIG-1383). A detached
/// launch registers its externally-owned audit row, the caller's scope then
/// departs before any outcome exists, and lash refuses to invent one. The row
/// stays non-terminal and is observable as `CallerDeparted`; a host await on it
/// is typed-refused instead of parking forever; and retention policy may name
/// the state directly, reclaiming those rows while live work survives.
#[tokio::test]
async fn caller_departed_rows_are_selectable_retention_policy() -> Result<()> {
    let backend = memory_backend().await;
    let registry: Arc<dyn lash_core::ProcessRegistry> = backend.process_registry();
    let core = process_test_core(backend.clone().into())?;

    let mut registered = Vec::new();
    for _ in 0..2 {
        let id = registry
            .register_process(
                lash_core::ProcessRegistration::new(
                    lash_core::ProcessInput::External {
                        metadata: serde_json::json!({}),
                    },
                    lash_core::ProcessProvenance::host(),
                    lash_core::Lifetime::Detached,
                )
                .with_admitted_identity(
                    lash_core::AdmittedProcessIdentity::for_testing(
                        lash_core::ProcessIdentity::new("test"),
                    ),
                ),
            )
            .await?
            .id;
        registered.push(id);
    }
    let [departed, live] =
        <[ProcessId; 2]>::try_from(registered).expect("two registered processes");
    registry.record_caller_departure(&departed).await?;

    let observed = core
        .processes()
        .get(&departed)
        .await?
        .expect("the caller-departed row is observable");
    assert_eq!(
        observed.lifecycle,
        lash_core::ProcessStatus::CallerDeparted,
        "the host sees the departure as a lifecycle state, not an invented terminal"
    );

    // Awaiting it is refused with a typed error rather than parking forever.
    let refusal = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        core.processes().await_output(&departed),
    )
    .await
    .expect("an await on a caller-departed row must be bounded, not parked")
    .expect_err("an await on a caller-departed row must be refused");
    assert!(
        refusal.to_string().contains("recorded a caller departure"),
        "unexpected await refusal: {refusal}"
    );

    // Retention policy names the state directly; live work is untouched.
    let report = core
        .processes()
        .prune(
            u64::MAX,
            Some(&lash_core::ProcessListFilter {
                status: lash_core::ProcessStatusFilter::any_of([
                    lash_core::ProcessStatus::CallerDeparted,
                ]),
                ..lash_core::ProcessListFilter::default()
            }),
            lash_core::ProjectionWatermark::NoProjector,
        )
        .await?;
    assert_eq!(
        report.pruned_processes, 1,
        "retention reclaims exactly the caller-departed row"
    );
    assert_eq!(
        registry
            .get_process(&live)
            .await?
            .expect("the running sibling survives retention")
            .status,
        lash_core::ProcessStatus::Running,
    );

    Ok(())
}
