//! Typed process input and artifact end states.

use super::*;

/// A lash_vm process whose recorded input disagrees with its published
/// artifact, in its host requirements or in the process it names, ends at
/// its first `vm_run` with that refusal's typed code, before the VM runs.
async fn a_lash_vm_process_whose_input_disagrees_with_its_artifact_ends_with_its_typed_refusal(
    tier: Tier,
) {
    let deployment = deploy_with(tier, Vec::new(), rlm_core).await;
    let payload = worker_payload(&deployment.backend).await;
    let mismatched = |mutate: fn(&mut lash_vm_runtime::LashVmProcessInput)| {
        let mut input = lash_vm_runtime::LashVmProcessInput::from_payload(payload.clone())
            .expect("the worker's input decodes");
        mutate(&mut input);
        serde_json::to_value(input).expect("the input encodes")
    };
    for (payload, code) in [
        (
            mismatched(|input| {
                input.host_requirements_ref =
                    lash_vm::HostRequirementsRef::new(&lash_vm::ContentHash::new("mismatch"));
            }),
            "process_host_requirements_mismatch",
        ),
        (
            mismatched(|input| {
                input.process_ref =
                    lash_vm::ProcessRef::new(lash_vm::ContentHash::new("mismatch"), 0);
            }),
            "process_ref_mismatch",
        ),
    ] {
        let process = start(
            &deployment.core,
            lash_vm_runtime::LASH_VM_ENGINE_KIND,
            payload,
        )
        .await;
        let output = ended(&deployment.core, &process).await;
        assert!(
            matches!(&output.outcome, lash_core::ToolCallOutcome::Failure(failure)
                if failure.code == code),
            "the process ends with {code}: {output:?}"
        );
    }
}

on_every_tier!(
    a_lash_vm_process_whose_input_disagrees_with_its_artifact_ends_with_its_typed_refusal
);

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

/// A lash_vm process cancelled while its VM is parked on a sleep ends
/// cancelled with the host's origin: the cancel is answered from the
/// committed snapshot, never by a VM run that fails to resume.
async fn a_sleeping_lash_vm_process_ends_cancelled(tier: Tier, state: SleepState) {
    use lash_vm::testing::ast_builders as b;
    const IDLE_EVICT: Duration = Duration::from_millis(50);
    let settings = match state {
        SleepState::Hot => lash_core_execution::DurableSettings::default(),
        SleepState::Suspended => lash_core_execution::DurableSettings {
            idle_evict: IDLE_EVICT,
            ..lash_core_execution::DurableSettings::default()
        },
    };
    let deployment = deploy_configured(tier, settings, Vec::new(), rlm_core).await;
    let payload = lash_vm_payload(
        &deployment.backend,
        "process sleeper() -> str { sleep(3600000); finish \"woke\" }",
        b::process_returning(
            "sleeper",
            Vec::new(),
            lash_vm::TypeExpr::Str,
            b::block(vec![
                b::sleep_for(b::num(3_600_000.0)),
                b::finish(b::string("woke")),
            ]),
        ),
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
    a_sleeping_lash_vm_process_ends_cancelled(tier, SleepState::Hot).await;
}

async fn a_lash_vm_process_cancelled_after_idle_eviction_of_its_sleep_ends_cancelled(tier: Tier) {
    a_sleeping_lash_vm_process_ends_cancelled(tier, SleepState::Suspended).await;
}

on_every_tier!(a_lash_vm_process_cancelled_while_its_sleep_is_hot_ends_cancelled);
on_every_tier!(a_lash_vm_process_cancelled_after_idle_eviction_of_its_sleep_ends_cancelled);
