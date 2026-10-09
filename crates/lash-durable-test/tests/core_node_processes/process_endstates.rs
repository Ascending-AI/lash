//! Typed process input and artifact end states.

use super::*;

/// A start whose input names a document no store holds is refused with
/// the store's typed cause, and no process is recorded.
async fn a_kernel_process_whose_document_is_not_stored_is_refused_at_its_start(tier: Tier) {
    let deployment = deploy_with(tier, Vec::new(), rlm_core).await;
    let mut payload = worker_payload(&deployment.backend).await;
    let other = lash::workflow::document::parse_document(
        "kernel 1\nnumbers float\n\nmain {\n  finish \"unpublished\"\n}\n",
    )
    .expect("the document parses")
    .identity()
    .expect("the document has an identity");
    payload["document"] = serde_json::to_value(other).expect("the identity encodes");
    let env_ref = deployment
        .core
        .host_artifacts()
        .publish_process_env(&lash_core::HostArtifactPin::mint(), &environment())
        .await
        .expect("the environment is published");
    let refused = deployment
        .core
        .processes()
        .start(
            lash_core::ProcessStartRequest::new(
                lash_core::ProcessInput::Engine {
                    kind: lash_vm_runtime::LASH_VM_ENGINE_KIND.to_owned(),
                    payload,
                },
                lash_core::ProcessOriginator::host(),
                lash_core::LifetimeDecision::Detached,
            )
            .with_env_ref(env_ref),
            deployment.core.effect_host(),
        )
        .await
        .expect_err("a start over a document no store holds is refused");
    let spelled = format!("{refused:?}");
    assert!(
        spelled.contains("ArtifactMissing") && spelled.contains(&other.to_string()),
        "the refusal names the missing document: {spelled}"
    );
}

on_every_tier!(a_kernel_process_whose_document_is_not_stored_is_refused_at_its_start);

/// A session tombstone leaves its detached process alive. Host cancellation
/// remains usable after deletion and determines the process's terminal origin.
async fn a_detached_process_outlives_its_deleted_session_and_accepts_host_cancellation(tier: Tier) {
    const SESSION: &str = "outlived-session";
    let deployment = deploy(
        tier,
        vec![Arc::new(ScriptEngine {
            kind: HOLD_ENGINE,
            advance: holding_engine_advance,
        })],
        // The session runs no turn; its creation needs a served profile.
        |builder| {
            builder
                .serve_test_llm_profile(scripted(|request, _| text(request, "unused")), metadata())
        },
    )
    .await;
    let session = SessionId::from(SESSION);
    deployment
        .core
        .session(session.clone())
        .create(lash::SessionCreation::root(
            lash::plugins::SessionToolAccess::ambient(),
            spec(),
        ))
        .await
        .expect("the session is created");
    let process = start_as(
        &deployment.core,
        HOLD_ENGINE,
        serde_json::Value::Null,
        |request| request.with_observers([session.clone()]),
    )
    .await;
    let administration = deployment.core.session_administration().await;
    lash::LashCore::delete_session(administration.delete_context(&session).unwrap())
        .await
        .expect("the delete is requested");
    let catalog = deployment.backend.stores().session_store_factory();
    tokio::time::timeout(Duration::from_secs(60), async {
        while !matches!(
            catalog.lookup_session(&session).await.unwrap(),
            lash_core::store::SessionLookup::Deleted
        ) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the session's deletion completes within a minute");
    let registry = deployment.backend.process_registry();
    let retained = registry
        .get_process(&process)
        .await
        .expect("the process is read")
        .expect("the detached process remains");
    assert!(
        !retained.is_terminal(),
        "session deletion does not end a detached process"
    );
    registry
        .request_process_cancel(
            &process,
            lash_core::CancelOrigin::OperatorRequested,
            "outlived-host".to_owned(),
            None,
        )
        .await
        .expect("host cancellation is accepted after deletion");
    let output = ended(&deployment.core, &process).await;
    assert!(
        matches!(&output.outcome, lash_core::ToolCallOutcome::Cancelled(cancellation) if cancellation.origin == Some(lash_core::CancelOrigin::OperatorRequested)),
        "the host can still end the detached process: {output:?}"
    );
}

on_every_tier!(a_detached_process_outlives_its_deleted_session_and_accepts_host_cancellation);

/// How long the owner of a sleeping process has had to release it before
/// the host cancels it.
#[derive(Clone, Copy, Debug)]
enum SleepState {
    /// Cancelled as soon as its sleep is its committed wait, inside the
    /// owner's `idle_evict`.
    Hot,
    /// Cancelled once `idle_evict` has passed over its sleep.
    Suspended,
}

/// A kernel process cancelled while its machine is parked on a sleep ends
/// cancelled with the host's origin: the cancel is answered from the
/// committed snapshot, never by a VM run that fails to resume.
async fn a_sleeping_kernel_process_ends_cancelled(tier: Tier, state: SleepState) {
    const IDLE_EVICT: Duration = Duration::from_millis(50);
    let settings = match state {
        SleepState::Hot => lash_core_execution::DurableSettings::default(),
        SleepState::Suspended => lash_core_execution::DurableSettings {
            idle_evict: IDLE_EVICT,
            ..lash_core_execution::DurableSettings::default()
        },
    };
    let deployment = deploy_configured(tier, settings, Vec::new(), rlm_core).await;
    let payload = kernel_process::payload(
        &deployment.backend,
        "kernel 1\nnumbers float\nentry sleeper() -> Any\n\nfn sleeper() {\n  do sleep 3600000\n  return \"woke\"\n}\n\nmain {\n  finish null\n}\n",
        "sleeper",
    )
    .await;
    let process = start(
        &deployment.core,
        lash_vm_runtime::LASH_VM_ENGINE_KIND,
        payload,
    )
    .await;
    let registry = deployment.backend.process_registry();
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let record = registry
                .get_process(&process)
                .await
                .expect("the process is read")
                .expect("the process exists");
            assert!(
                !record.is_terminal(),
                "the process ended before its sleep: {record:?}"
            );
            if record
                .waits()
                .iter()
                .any(|wait| matches!(wait.kind, lash_core::WaitKind::Sleep { .. }))
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the process parks on its sleep within a minute");
    if matches!(state, SleepState::Suspended) {
        tokio::time::sleep(IDLE_EVICT * 10).await;
    }
    registry
        .request_process_cancel(
            &process,
            lash_core::CancelOrigin::OperatorRequested,
            "sleeper-host".to_owned(),
            None,
        )
        .await
        .expect("the cancel is accepted");
    let output = ended(&deployment.core, &process).await;
    assert!(
        matches!(&output.outcome, lash_core::ToolCallOutcome::Cancelled(cancellation)
            if cancellation.origin == Some(lash_core::CancelOrigin::OperatorRequested)),
        "the sleeping process ends cancelled by the host: {output:?}"
    );
    let record = registry
        .get_process(&process)
        .await
        .expect("the process is read")
        .expect("the process exists");
    assert!(
        matches!(
            &record.lifecycle,
            lash_core::ProcessLifecycleState::Terminal { .. }
        ) && record.waits().is_empty(),
        "the cancelled process holds its terminal and no wait: {record:?}"
    );
}

async fn a_lash_vm_process_cancelled_while_its_sleep_is_hot_ends_cancelled(tier: Tier) {
    a_sleeping_kernel_process_ends_cancelled(tier, SleepState::Hot).await;
}

async fn a_lash_vm_process_cancelled_after_idle_eviction_of_its_sleep_ends_cancelled(tier: Tier) {
    a_sleeping_kernel_process_ends_cancelled(tier, SleepState::Suspended).await;
}

on_every_tier!(a_lash_vm_process_cancelled_while_its_sleep_is_hot_ends_cancelled);
on_every_tier!(a_lash_vm_process_cancelled_after_idle_eviction_of_its_sleep_ends_cancelled);
