use super::*;

async fn append_wake(
    registry: &Arc<dyn crate::ProcessRegistry>,
    process_id: &str,
    target_session_id: &str,
) -> crate::WakeDelivery {
    registry
        .register_process(
            process_registry::registration(process_id)
                .with_extra_event_types([process_registry::wake_event_type("producer.wake")]),
        )
        .await
        .expect("register wake producer");
    let append = registry
        .append_event(
            process_id,
            crate::ProcessEventAppendRequest::new(
                "producer.wake",
                serde_json::json!({"wake_input": format!("wake from {process_id}")}),
            )
            .with_wake_target_scope(crate::SessionScope::new(target_session_id)),
        )
        .await
        .expect("append wake event");
    let wake = append.wake_delivery.expect("wake delivery");
    registry
        .list_wake_deliveries(Some(crate::WakeDeliveryState::Pending))
        .await
        .expect("list pending wake deliveries")
        .into_iter()
        .find(|delivery| {
            delivery.wake.process_id == process_id && delivery.wake.sequence == wake.sequence
        })
        .expect("appended outbox row")
}

async fn drive_once(
    registry: Arc<dyn crate::ProcessRegistry>,
    factory: Arc<dyn crate::SessionStoreFactory>,
) -> crate::WakeDeliveryDriveReport {
    crate::WakeDeliveryDriver::drive_pending_once(
        registry,
        factory,
        None,
        Arc::new(crate::SystemClock),
        32,
    )
    .await
    .expect("drive pending wakes")
}

/// Cross-backend process-wake crash matrix.
///
/// The supplied process registry and session factory must share their normal
/// backend durability domain, but need not share a database transaction: the
/// sender outbox and receiver evidence deliberately bridge that crash boundary.
pub async fn wake_delivery_crash_matrix(
    factory: Arc<dyn crate::SessionStoreFactory>,
    registry: Arc<dyn crate::ProcessRegistry>,
) {
    let target_request = crate::SessionStoreCreateRequest {
        session_id: "wake-crash-target".to_string(),
        relation: crate::SessionRelation::Root,
        policy: crate::SessionPolicy {
            model: crate::ModelSpec::from_token_limits(
                "wake-crash-model",
                Default::default(),
                200_000,
                None,
            )
            .expect("valid crash-matrix model"),
            provider_id: "conformance-provider".to_string(),
            session_id: Some("wake-crash-target".to_string()),
            autonomous: false,
            max_turns: None,
            prompt: crate::PromptLayer::new(),
            generation: crate::GenerationOptions::default(),
        },
    };
    let target = factory
        .create_store(&target_request)
        .await
        .expect("create wake target");

    // Crash after process append, before enqueue: startup scan finds the row.
    let append_only = append_wake(&registry, "wake-append-only", &target_request.session_id).await;
    assert!(
        target
            .list_queued_work(&target_request.session_id)
            .await
            .expect("queue before startup scan")
            .is_empty()
    );
    let report = drive_once(Arc::clone(&registry), Arc::clone(&factory)).await;
    assert_eq!(report.enqueued, 1);
    assert_eq!(
        registry
            .list_wake_deliveries(Some(crate::WakeDeliveryState::Enqueued))
            .await
            .expect("enqueued outbox rows")
            .iter()
            .filter(|delivery| delivery.delivery_id == append_only.delivery_id)
            .count(),
        1
    );

    // Crash after enqueue, before sender mark: replay returns the live queue row.
    let before_mark = append_wake(&registry, "wake-before-mark", &target_request.session_id).await;
    let before_mark_batch = target
        .enqueue_queued_work(crate::process_wake_batch_draft(before_mark.wake.clone()))
        .await
        .expect("simulate enqueue before crash");
    drive_once(Arc::clone(&registry), Arc::clone(&factory)).await;
    let queued = target
        .list_queued_work(&target_request.session_id)
        .await
        .expect("list live queued wakes");
    assert_eq!(
        queued
            .iter()
            .filter(|batch| batch.source_key.as_deref() == Some(&before_mark.source_key()))
            .count(),
        1
    );

    // Settle the queued wake and then redrive the sender row. Receiver evidence
    // must absorb the late redelivery without recreating work.
    let owner = crate::LeaseOwnerIdentity::opaque("wake-crash-owner", "wake-crash-incarnation");
    let lease = match target
        .try_claim_session_execution_lease(&target_request.session_id, &owner, 60_000)
        .await
        .expect("claim target session")
    {
        crate::SessionExecutionLeaseClaimOutcome::Acquired(lease) => lease,
        crate::SessionExecutionLeaseClaimOutcome::Busy { .. } => {
            panic!("fresh crash-matrix target lease must be available")
        }
    };
    let claim = target
        .claim_ready_queued_work_by_batch_ids(
            &target_request.session_id,
            &lease.fence(),
            &owner,
            crate::QueuedWorkClaimBoundary::Idle,
            std::slice::from_ref(&before_mark_batch.batch_id),
        )
        .await
        .expect("claim queued wakes")
        .expect("queued wake claim");
    let state = crate::RuntimeSessionState {
        session_id: target_request.session_id.clone(),
        ..crate::RuntimeSessionState::default()
    };
    target
        .commit_runtime_state(
            crate::RuntimeCommit::persisted_state_for_test(&state, &[])
                .completing_queue_claim(claim.completion())
                .releasing_session_execution_lease(lease.completion()),
        )
        .await
        .expect("settle queued wakes and evidence atomically");
    registry
        .redrive_wake_delivery(&before_mark.delivery_id)
        .await
        .expect("redrive consumed sender row");
    drive_once(Arc::clone(&registry), Arc::clone(&factory)).await;
    assert!(
        target
            .list_queued_work(&target_request.session_id)
            .await
            .expect("queue after consumed redelivery")
            .iter()
            .all(|batch| batch.source_key.as_deref() != Some(&before_mark.source_key())),
        "late sender redelivery must resolve against receiver evidence"
    );

    // Competing drivers may both inspect a pending row, but queue and sender
    // transitions remain idempotent.
    let concurrent = append_wake(
        &registry,
        "wake-concurrent-drivers",
        &target_request.session_id,
    )
    .await;
    let (left, right) = tokio::join!(
        drive_once(Arc::clone(&registry), Arc::clone(&factory)),
        drive_once(Arc::clone(&registry), Arc::clone(&factory))
    );
    assert!(
        left.enqueued + right.enqueued >= 1,
        "at least one competing driver must enqueue the pending wake"
    );
    assert_eq!(
        target
            .list_queued_work(&target_request.session_id)
            .await
            .expect("queue after concurrent drivers")
            .iter()
            .filter(|batch| batch.source_key.as_deref() == Some(&concurrent.source_key()))
            .count(),
        1
    );

    // A missing/deleted receiver is a typed terminal outcome; explicit redrive
    // is the only operation that returns it to the pending lane.
    let gone = append_wake(&registry, "wake-target-gone", "missing-wake-target").await;
    let report = drive_once(Arc::clone(&registry), Arc::clone(&factory)).await;
    assert_eq!(report.discarded_target_gone, 1);
    let discarded = registry
        .list_wake_deliveries(Some(crate::WakeDeliveryState::Discarded))
        .await
        .expect("discarded deliveries")
        .into_iter()
        .find(|delivery| delivery.delivery_id == gone.delivery_id)
        .expect("target-gone delivery");
    assert_eq!(
        discarded.discard_reason,
        Some(crate::WakeDiscardReason::TargetGone)
    );
    registry
        .redrive_wake_delivery(&gone.delivery_id)
        .await
        .expect("explicitly redrive target-gone row");
    assert!(
        registry
            .list_wake_deliveries(Some(crate::WakeDeliveryState::Pending))
            .await
            .expect("pending after redrive")
            .iter()
            .any(|delivery| delivery.delivery_id == gone.delivery_id)
    );
    registry
        .discard_wake_delivery(&gone.delivery_id, crate::WakeDiscardReason::TargetGone)
        .await
        .expect("return redriven missing target to its typed discard");

    // Expiry is a typed terminal outcome. Redrive is explicit, refreshes the
    // expiry horizon, and lets the same row deliver normally.
    let expiry = append_wake(&registry, "wake-expiry", &target_request.session_id).await;
    let wait_ms = expiry
        .expires_at_ms
        .saturating_sub(crate::current_epoch_ms())
        .saturating_add(5);
    assert!(
        wait_ms <= 5_000,
        "crash-matrix registries must use a short test expiry, got {wait_ms}ms"
    );
    tokio::time::sleep(std::time::Duration::from_millis(wait_ms)).await;
    let report = drive_once(Arc::clone(&registry), Arc::clone(&factory)).await;
    assert_eq!(report.discarded_expired, 1);
    registry
        .redrive_wake_delivery(&expiry.delivery_id)
        .await
        .expect("redrive expired delivery");
    let report = drive_once(Arc::clone(&registry), Arc::clone(&factory)).await;
    assert_eq!(report.enqueued, 1);
    assert_eq!(
        target
            .list_queued_work(&target_request.session_id)
            .await
            .expect("queue after expired redrive")
            .iter()
            .filter(|batch| batch.source_key.as_deref() == Some(&expiry.source_key()))
            .count(),
        1
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn in_memory_wake_delivery_crash_matrix() {
        let config =
            crate::WakeDeliveryConfig::new(250, 10_000).expect("valid short test retention");
        let registry =
            Arc::new(crate::TestLocalProcessRegistry::default().with_wake_delivery_config(config))
                as Arc<dyn crate::ProcessRegistry>;
        let factory = Arc::new(crate::InMemorySessionStoreFactory::new())
            as Arc<dyn crate::SessionStoreFactory>;
        wake_delivery_crash_matrix(factory, registry).await;
    }
}
