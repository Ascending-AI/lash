//! Process end states a session or its artifact decides: a detached
//! process outlives its session's deletion, and a lashlang process whose
//! input disagrees with its artifact ends with its typed refusal.

use super::*;

/// A detached process its session observes outlives the session's
/// deletion: deleted while the process is parked on a signal, the session
/// ends at its tombstone, and a host's signal still wakes the process,
/// which ends with the signal's payload.
async fn a_process_outlives_its_deleted_session_and_resumes_on_a_host_signal(tier: Tier) {
    const SESSION: &str = "outlived-session";
    let deployment = deploy(
        tier,
        vec![Arc::new(ScriptEngine {
            kind: SIGNAL_ENGINE,
            advance: signal_engine_advance,
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
        .create(lash::SessionCreation::root(spec()))
        .await
        .expect("the session is created");
    let process = start_as(
        &deployment.core,
        SIGNAL_ENGINE,
        serde_json::Value::Null,
        |request| {
            request
                .with_event_types([signal_type("go")])
                .with_observers([session.clone()])
        },
    )
    .await;
    parked(&deployment.backend, &process).await;
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
    let identity = lash_core::ProcessSignalIdentity::new(process.clone(), "go", "outlived-signal")
        .expect("a signal identity");
    deployment
        .core
        .processes()
        .signal(
            lash_core::ProcessSignal::new(identity, serde_json::json!({ "go": true })),
            deployment.core.effect_host(),
        )
        .await
        .expect("the signal is delivered after the session's deletion");
    let answer = success(&ended(&deployment.core, &process).await);
    assert_eq!(answer, serde_json::json!({ "signal": { "go": true } }));
}

on_every_tier!(a_process_outlives_its_deleted_session_and_resumes_on_a_host_signal);

/// A lashlang process whose recorded input disagrees with its published
/// artifact, in its host requirements or in the process it names, ends at
/// its first `vm_run` with that refusal's typed code, before the VM runs.
async fn a_lashlang_process_whose_input_disagrees_with_its_artifact_ends_with_its_typed_refusal(
    tier: Tier,
) {
    let deployment = deploy_with(tier, Vec::new(), rlm_core).await;
    let payload = worker_payload(&deployment.backend).await;
    let mismatched = |mutate: fn(&mut lash_lashlang_runtime::LashlangProcessInput)| {
        let mut input = lash_lashlang_runtime::LashlangProcessInput::from_payload(payload.clone())
            .expect("the worker's input decodes");
        mutate(&mut input);
        serde_json::to_value(input).expect("the input encodes")
    };
    for (payload, code) in [
        (
            mismatched(|input| {
                input.host_requirements_ref =
                    lashlang::HostRequirementsRef::new(&lashlang::ContentHash::new("mismatch"));
            }),
            "process_host_requirements_mismatch",
        ),
        (
            mismatched(|input| {
                input.process_ref =
                    lashlang::ProcessRef::new(lashlang::ContentHash::new("mismatch"), 0);
            }),
            "process_ref_mismatch",
        ),
    ] {
        let process = start(
            &deployment.core,
            lash_lashlang_runtime::LASHLANG_ENGINE_KIND,
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
    a_lashlang_process_whose_input_disagrees_with_its_artifact_ends_with_its_typed_refusal
);
