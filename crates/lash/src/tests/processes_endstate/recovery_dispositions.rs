use super::*;

// --- ADR 0019 disposition-driven recovery, at the facade seam ---------------
//
// The disposition/evidence verdicts themselves are pinned by unit and
// conformance coverage (`process_worker::recovery_tests`, the conformance
// registry cases). These tests are the END-TO-END evidence the wave scoped: the
// facade await/observe/prune seams (`core.processes()`) resolving and reflecting
// those verdicts, and a foreign worker's later sweep never resurrecting a
// terminalized row.

fn recovery_local_owner(
    owner_id: &str,
    _host_id: &str,
    _process_start: &str,
) -> lash_core::LeaseOwnerIdentity {
    lash_core::LeaseOwnerIdentity::opaque(owner_id, format!("{owner_id}:incarnation"))
}

/// A bare recovery worker over `registry` with a known lease owner. It runs no
/// engine (empty `PluginHost`); it drains and sweeps the registry only, so it
/// stands in for a host that started OwnerBound work under `owner`.
fn recovery_process_worker(
    registry: Arc<dyn lash_core::ProcessRegistry>,
    owner: lash_core::LeaseOwnerIdentity,
) -> lash_core::facade_support::DurableProcessWorker {
    let watched = lash_core::facade_support::watch_process_registry(registry);
    lash_core::facade_support::DurableProcessWorker::new(
        lash_core::facade_support::DurableProcessWorkerConfig::new(
            Arc::new(lash_core::facade_support::PluginHost::new(Vec::new())),
            lash_core::facade_support::RuntimeHostConfig::in_memory(
                lash_core::CommitBudget::bounded(1024 * 1024, 512),
                lash_core::QueuedWorkBatchingConfig::new(1),
            ),
            Arc::new(lash_core::facade_support::InMemorySessionStoreFactory::new()),
            lash_core::WorkerProcessWork::SelfNative(watched),
            Arc::new(lash_core::NoQueuedWork::new()),
            owner,
        ),
    )
    .expect("valid test native substrate config")
}

fn owner_bound_external_registration(id: &str) -> lash_core::ProcessRegistration {
    lash_core::ProcessRegistration::new(
        id,
        lash_core::ProcessInput::External {
            metadata: serde_json::json!({}),
        },
        lash_core::RecoveryContract::OwnerBound,
        lash_core::ProcessProvenance::host(),
    )
}

/// Graceful-drain end to end (ADR 0019): a host holds `await_output` on its own
/// started OwnerBound work, then drains at close. The held awaiter resolves with
/// the `Abandoned{OwnerDrain}` terminal naming this owner; a foreign worker's
/// later sweep never resurrects or re-runs it; the facade prune reclaims it.
#[tokio::test]
async fn owner_bound_graceful_drain_resolves_awaiter_and_prunes_end_to_end() -> Result<()> {
    let artifact_store: Arc<dyn lash_lashlang_runtime::LashlangArtifactStore> =
        Arc::new(lash_lashlang_runtime::InMemoryLashlangArtifactStore::new());
    let trigger_store: Arc<dyn lash_core::TriggerStore> =
        Arc::new(lash_core::facade_support::InMemoryTriggerStore::default());
    let registry: Arc<dyn lash_core::ProcessRegistry> =
        Arc::new(TestLocalProcessRegistry::default());
    let core = process_test_core(
        Arc::clone(&artifact_store),
        Arc::clone(&trigger_store),
        Arc::clone(&registry),
        in_memory_process_env_store(),
    )?;

    let drain_owner = recovery_local_owner("drain-host", "host-a", "drain-start");
    let worker = recovery_process_worker(Arc::clone(&registry), drain_owner.clone());

    // A started OwnerBound row this host owns (first_started under `drain_owner`).
    let process_id = "owner-bound-drain";
    registry
        .register_process(owner_bound_external_registration(process_id))
        .await?;
    registry
        .record_first_started(
            &ProcessId::from(process_id),
            lash_core::ProcessStarted {
                owner: drain_owner.clone(),
                fencing_token: 0,
                attempt: 1,
                started_at_ms: 1,
            },
        )
        .await?;

    // Hold the terminal await on the still-running row through the facade await
    // seam — it must resolve only once drain terminalizes the work.
    let await_core = core.clone();
    let await_id = process_id.to_string();
    let waiter = tokio::spawn(async move {
        await_core
            .processes()
            .await_output(&ProcessId::from(await_id))
            .await
    });

    // The host drains its own started OwnerBound work natively at close.
    let report = worker.drain_owner_bound_work().await?;
    assert_eq!(
        report.abandoned,
        vec![process_id.to_string()],
        "drain terminalizes exactly this host's started OwnerBound work"
    );

    let output = tokio::time::timeout(std::time::Duration::from_secs(5), waiter)
        .await
        .expect("held await resolves within bound")
        .expect("join await task")?;
    let lash_core::ProcessAwaitOutput::Abandoned { evidence, .. } = output else {
        panic!("held await did not resolve to an Abandoned terminal: {output:#?}");
    };
    assert_eq!(evidence.writer, lash_core::AbandonWriter::OwnerDrain);
    assert_eq!(
        evidence.owner.as_ref(),
        Some(&drain_owner),
        "the owner-drain evidence names this host as the abandoned owner"
    );

    // The Abandoned terminal is model-visible read-side (a fourth terminal peer).
    let observed = core
        .processes()
        .get(&ProcessId::from(process_id))
        .await?
        .expect("abandoned row observed through the facade");
    assert_eq!(observed.lifecycle, lash_core::ProcessStatus::Abandoned);
    assert!(observed.terminal);

    // A foreign worker's sweep never resurrects or re-runs an abandoned row: the
    // row is terminal, so it is off the worklist and untouched.
    let foreign = recovery_process_worker(
        Arc::clone(&registry),
        recovery_local_owner("foreign-host", "host-b", "foreign-start"),
    );
    let _ = foreign.drive_pending_processes().await?;
    assert!(
        registry
            .list_non_terminal_page(
                std::num::NonZeroUsize::new(16).expect("non-zero test page size"),
                None,
            )
            .await?
            .records
            .is_empty(),
        "a foreign sweep must not resurrect the abandoned row onto the worklist"
    );
    let re_awaited = core
        .processes()
        .await_output(&ProcessId::from(process_id))
        .await?;
    let lash_core::ProcessAwaitOutput::Abandoned { evidence, .. } = re_awaited else {
        panic!("the abandoned terminal was mutated by a foreign sweep: {re_awaited:#?}");
    };
    assert_eq!(evidence.writer, lash_core::AbandonWriter::OwnerDrain);
    assert_eq!(
        evidence.owner.as_ref(),
        Some(&drain_owner),
        "the immutable owner-drain evidence survives a foreign sweep — the row is never re-run"
    );

    // Retention: the facade prune reclaims the terminal row.
    let prune = core
        .processes()
        .prune(
            i64::MAX as u64,
            None,
            lash_core::ProjectionWatermark::NoProjector,
        )
        .await?;
    assert_eq!(prune.pruned_processes, 1);
    assert!(
        matches!(
            core.processes().get(&ProcessId::from(process_id)).await,
            Err(crate::EmbedError::Plugin(
                lash_core::PluginError::ProcessNoLongerRetained { .. }
            ))
        ),
        "the pruned abandoned row is a typed retained-history miss"
    );

    Ok(())
}

/// Silent-owner classification + abandon-request reconciliation end to end (ADR
/// 0019): a started OwnerBound row whose holder lease expired
/// (a different host) survives a sweep non-terminal; the facade read exposes the
/// lease facts so a host can classify staleness; an operator's `request_abandon`
/// is visible read-side while pending; and once the stale lease lapses the next
/// sweep reconciles it into `Abandoned{ReconciledRequest}` naming the lapsed
/// owner. Elapsed time alone never terminalized it — only the recorded
/// authorization did.
#[tokio::test]
async fn silent_owner_stays_running_then_abandon_request_reconciles_end_to_end() -> Result<()> {
    let artifact_store: Arc<dyn lash_lashlang_runtime::LashlangArtifactStore> =
        Arc::new(lash_lashlang_runtime::InMemoryLashlangArtifactStore::new());
    let trigger_store: Arc<dyn lash_core::TriggerStore> =
        Arc::new(lash_core::facade_support::InMemoryTriggerStore::default());
    let registry: Arc<dyn lash_core::ProcessRegistry> =
        Arc::new(TestLocalProcessRegistry::default());
    let core = process_test_core(
        Arc::clone(&artifact_store),
        Arc::clone(&trigger_store),
        Arc::clone(&registry),
        in_memory_process_env_store(),
    )?;

    // The sweep runs on host-a; the started owner is on host-b, so it is never
    // available for a claimant — a silent, foreign, expired holder.
    let sweep_owner = recovery_local_owner("recovery-host", "host-a", "claimant-start");
    let worker = recovery_process_worker(Arc::clone(&registry), sweep_owner);
    let silent_owner = recovery_local_owner("silent-owner", "host-b", "silent-start");

    let process_id = "silent-owner-bound";
    registry
        .register_process(owner_bound_external_registration(process_id))
        .await?;
    registry
        .record_first_started(
            &ProcessId::from(process_id),
            lash_core::ProcessStarted {
                owner: silent_owner.clone(),
                fencing_token: 0,
                attempt: 1,
                started_at_ms: 1,
            },
        )
        .await?;
    let silent_lease = registry
        .claim_process_lease(&ProcessId::from(process_id), &silent_owner, 60_000)
        .await?
        .acquired()
        .expect("silent holder claims its lease");

    // A sweep leaves the silent holder's row non-terminal:
    // elapsed time / silence alone never terminalizes.
    let _ = worker.drive_pending_processes().await?;
    let observed = core
        .processes()
        .get(&ProcessId::from(process_id))
        .await?
        .expect("silent row observed");
    assert_eq!(
        observed.lifecycle,
        lash_core::ProcessStatus::Running,
        "a silent holder is left non-terminal by the sweep"
    );
    // The read exposes the lease facts a host classifies staleness from.
    assert_eq!(
        observed.disposition,
        lash_core::RecoveryContract::OwnerBound
    );
    assert_eq!(
        observed.lease_holder.as_ref(),
        Some(&silent_owner),
        "ObservedProcess exposes the lease holder identity read-side"
    );
    assert!(
        observed.lease_expires_at_ms.is_some(),
        "ObservedProcess exposes the lease expiry read-side"
    );
    assert!(
        observed.abandon_request.is_none(),
        "no abandon request has been recorded yet"
    );

    // A third party records an Abandon Request through the facade: authorization
    // to accept uncertainty. It terminalizes nothing itself and is visible to
    // observers while pending.
    let after_request = core
        .processes()
        .request_abandon(
            &ProcessId::from(process_id),
            "operator",
            Some("host retired".to_string()),
        )
        .await?;
    let request = after_request
        .abandon_request
        .as_ref()
        .expect("abandon request visible on the returned observation");
    assert_eq!(request.requested_by, "operator");
    assert_eq!(
        after_request.lifecycle,
        lash_core::ProcessStatus::Running,
        "the abandon request does not terminalize the row"
    );
    assert!(
        core.processes()
            .get(&ProcessId::from(process_id))
            .await?
            .and_then(|process| process.abandon_request)
            .is_some(),
        "the pending abandon request is visible read-side before reconciliation"
    );

    // The stale lease lapses (the foreign host finally drops it). Only now may
    // the sweep reconcile the request.
    registry
        .complete_process_lease(&lash_core::ProcessLeaseCompletion::from_lease(
            &silent_lease,
        ))
        .await?;
    let _ = worker.drive_pending_processes().await?;

    let output = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        core.processes().await_output(&ProcessId::from(process_id)),
    )
    .await
    .expect("reconciled terminal resolves within bound")?;
    let lash_core::ProcessAwaitOutput::Abandoned { evidence, .. } = output else {
        panic!("reconciliation did not produce an Abandoned terminal: {output:#?}");
    };
    assert_eq!(evidence.writer, lash_core::AbandonWriter::ReconciledRequest);
    assert_eq!(
        evidence.owner.as_ref().map(|owner| owner.owner_id.as_str()),
        Some("silent-owner"),
        "the reconciled abandonment names the started owner as the lapsed owner"
    );

    Ok(())
}

/// Caller departure end to end through the host API (FIG-1383). A detached
/// launch registers its externally-owned audit row, the caller's scope then
/// departs before any outcome exists, and lash refuses to invent one. The row
/// stays non-terminal and is observable as `CallerDeparted`; a host await on it
/// is typed-refused instead of parking forever; and retention policy may name
/// the state directly, reclaiming those rows while live work survives.
#[tokio::test]
async fn caller_departed_rows_are_selectable_retention_policy() -> Result<()> {
    let artifact_store: Arc<dyn lash_lashlang_runtime::LashlangArtifactStore> =
        Arc::new(lash_lashlang_runtime::InMemoryLashlangArtifactStore::new());
    let trigger_store: Arc<dyn lash_core::TriggerStore> =
        Arc::new(lash_core::facade_support::InMemoryTriggerStore::default());
    let registry: Arc<dyn lash_core::ProcessRegistry> =
        Arc::new(TestLocalProcessRegistry::default());
    let core = process_test_core(
        Arc::clone(&artifact_store),
        Arc::clone(&trigger_store),
        Arc::clone(&registry),
        in_memory_process_env_store(),
    )?;

    let departed = "facade-caller-departed";
    let live = "facade-caller-live";
    for id in [departed, live] {
        registry
            .register_process(
                lash_core::ProcessRegistration::new(
                    id,
                    lash_core::ProcessInput::External {
                        metadata: serde_json::json!({}),
                    },
                    lash_core::RecoveryContract::ExternallyOwned,
                    lash_core::ProcessProvenance::host(),
                )
                .with_identity(lash_core::ProcessIdentity::new("test")),
            )
            .await?;
    }
    registry
        .record_caller_departure(&ProcessId::from(departed))
        .await?;

    let observed = core
        .processes()
        .get(&ProcessId::from(departed))
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
        core.processes().await_output(&ProcessId::from(departed)),
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
            .get_process(&ProcessId::from(live))
            .await?
            .expect("the running sibling survives retention")
            .status,
        lash_core::ProcessStatus::Running,
    );

    Ok(())
}
