use super::*;
use crate::tests::host::ingress::*;

/// FIG-3070: a start committed before its submission outcome is retained
/// reports the store's coalescing verdict when ingress realizes it again.
#[tokio::test]
async fn a_store_coalesced_start_reports_replayed_through_ingress() -> Result<()> {
    let backend = sqlite_memory_store_backend().await;
    let (core, registry) = ingress_engine_core(backend.clone()).await?;
    let scope = lash_core::ExecutionScope::turn(SESSION, SCOPE);
    let ingress = core.tool_intents(SessionId::from(SESSION), scope.clone())?;
    let key = ingress.key("store-coalesced-start", 0).expect("key");
    let intent = engine_start_intent(INGRESS_ENGINE_KIND, serde_json::json!({"program": "known"}));
    let lash_core::ToolIntent::StartProcess(start) = &intent else {
        panic!("start intent");
    };
    // Model the crash interval: the start exists, but ingress has no recorded
    // submission outcome to answer without invoking its local executor.
    let request = start.into_request(key.identity());
    let env_store = backend.process_env_store();
    let starter = scope.journal_identity().expect("valid turn scope");
    let started = lash_core::runtime::register_process_start(
        &lash_core::runtime::ProcessStartStores {
            tracing: Some(&core.env.core.tracing),
            registry: registry.as_ref(),
            env_store: Some(&env_store),
            engines: &core.host_process_engines,
            session_catalog: Some(core.store_factory.as_ref()),
            session_turn_admission: None,
            executor: "coalesced ingress law",
            starter: &starter,
        },
        request.into_registration(),
        &[],
    )
    .await
    .map_err(lash_core::PluginError::from)?;
    assert_eq!(started.realization(), lash_core::StoreRealization::Realized);
    let coalesced = ingress.submit(key, intent).await;
    assert!(
        matches!(
            &coalesced,
            crate::tools::ToolIntentIngressOutcome::Admitted { replayed: true, .. }
        ),
        "the local observer must report the store's verdict: {coalesced:?}"
    );
    assert_eq!(started_process_id(&coalesced), started.record.id);
    assert_eq!(registered_process_count(&registry).await?, 1);
    let fresh = ingress
        .submit(
            ingress.key("fresh-start", 0).expect("fresh key"),
            engine_start_intent(INGRESS_ENGINE_KIND, serde_json::json!({"program": "known"})),
        )
        .await;
    assert!(
        matches!(
            fresh,
            crate::tools::ToolIntentIngressOutcome::Admitted {
                replayed: false,
                ..
            }
        ),
        "a fresh start must realize: {fresh:?}"
    );
    core.shutdown().await?;
    Ok(())
}

/// FIG-1838: host ingress and session-owned recorded-intent execution admit an
/// equivalent start to one identity: it never changes with the route used to
/// start it.
#[tokio::test]
async fn equivalent_recorded_start_has_the_same_identity_across_routes() -> Result<()> {
    let (core, registry) = ingress_engine_core(sqlite_memory_store_backend().await).await?;
    let payload = serde_json::json!({"program": "environment-sensitive"});

    let ingress = core.tool_intents(
        crate::SessionId::parse(SESSION).expect("nonblank host identity"),
        lash_core::ExecutionScope::turn(SESSION, "host-ingress-route"),
    )?;
    let ingress_key = ingress
        .key("environment-sensitive-host", 0)
        .expect("a host submission handle");
    let host_outcome = ingress
        .submit(
            ingress_key,
            engine_start_intent(INGRESS_ENGINE_KIND, payload.clone()),
        )
        .await;
    assert!(
        matches!(
            host_outcome,
            crate::tools::ToolIntentIngressOutcome::Admitted {
                outcome: lash_core::ToolIntentExecutionOutcome::Executed { .. },
                ..
            }
        ),
        "host ingress must admit the environment-sensitive engine"
    );
    let ingress_identity = registry
        .get_process(&started_process_id(&host_outcome))
        .await?
        .expect("host ingress registers a process")
        .identity;

    let session = created_session(
        &core,
        crate::SessionId::parse(SESSION).expect("nonblank host identity"),
    )
    .await
    .open()
    .await?;
    let effect_host = session.effect_host();
    let scoped = effect_host.scoped(lash_core::AdmittedScope::turn(
        SESSION,
        "session-recorded-intent-route",
    ))?;
    let processes = {
        let writer = session.runtime.writer();
        let runtime = writer.lock().await;
        runtime.process_service()?
    };
    let intents =
        lash_core::ToolIntents::v3(vec![engine_start_intent(INGRESS_ENGINE_KIND, payload)]);
    let realization = lash_core::testing::execute_tool_intents_with_services(
        scoped,
        processes,
        &SessionId::from(SESSION),
        &lash_core::ToolCallId::fixture("environment-sensitive-session"),
        &intents,
    )
    .await
    .map_err(lash_core::PluginError::from)?;
    let outcomes = &realization.receipt.outcomes;
    let [
        lash_core::ToolIntentExecutionOutcome::Executed {
            realized: lash_core::ToolIntentRealized::StartProcess(result),
            ..
        },
    ] = outcomes.as_slice()
    else {
        panic!("session recorded-intent route must execute: {outcomes:?}")
    };
    // The session route stages its start for the outcome commit of the call
    // that declares it: the identity is the one that commit registers.
    let [staged] = realization.store_local.as_slice() else {
        panic!(
            "the session route stages one start: {:?}",
            realization.store_local
        )
    };
    let staged = lash_core::testing::staged_start_record(staged)?
        .expect("the session route stages a process start");
    assert_eq!(staged.id, result.process_id);
    let session_identity = staged.identity;

    assert_eq!(ingress_identity, session_identity);
    let expected = lash_core::ProcessDefinitionDraft::new(
        INGRESS_ENGINE_KIND,
        serde_json::json!({
            "payload": {"program": "environment-sensitive"},
        }),
        [],
    )
    .expect("the payload produces a canonical definition");
    assert_eq!(
        ingress_identity.definition_id,
        Some(expected.id()),
        "the shared identity is the payload's admission"
    );
    Ok(())
}
