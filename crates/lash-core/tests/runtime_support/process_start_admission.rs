fn admit_payload_gated_engine(
    _kind: &'static str,
    payload: &serde_json::Value,
    _env_spec: Option<&lash_core::ProcessExecutionEnvSpec>,
) -> Result<lash_core::ProcessIdentity, lash_core::PluginError> {
    if payload.get("program").and_then(serde_json::Value::as_str) == Some("known") {
        return Ok(lash_core::ProcessIdentity::for_definition(
            lash_core::ProcessDefinitionRef::unclaimed(PAYLOAD_GATED_ENGINE_KIND, payload.clone()),
            payload.get("program").and_then(serde_json::Value::as_str),
        ));
    }
    Err(lash_core::PluginError::Session(format!(
        "unknown {PAYLOAD_GATED_ENGINE_KIND} program"
    )))
}

#[test]
fn process_engine_registration_rejects_a_kind_mismatch() {
    assert!(matches!(
        lash_core::ProcessEngineRegistration::new(
            Arc::new(PayloadGatedEngine),
            lash_core::ProcessEngineAdmission::accepting("different-kind"),
        ),
        Err(lash_core::PluginError::Registration(_))
    ));
}

/// Shared fixture: a runtime whose only process engine is
/// [`PayloadGatedEngine`], plus the registry the started rows land in.
async fn payload_gated_engine_runtime(
    backend: &lash_core::Backend,
    session_id: &SessionId,
) -> (Arc<dyn lash_core::ProcessRegistry>, LashRuntime) {
    let registry = backend.process_registry();
    lash_core::publish_process_execution_env(
        backend.process_env_store().as_ref(),
        &lash_core::testing::host_pin_claim_for_testing(),
        &lash_core::ProcessExecutionEnvSpec::new(
            lash_core::PluginOptions::default(),
            standard_test_policy(),
        ),
    )
    .await
    .expect("publish engine fixture environment");
    let core = test_host_config(backend)
        .core
        .with_process_engine_registration(
            lash_core::ProcessEngineRegistration::new(
                Arc::new(PayloadGatedEngine),
                lash_core::ProcessEngineAdmission::new(
                    PAYLOAD_GATED_ENGINE_KIND,
                    admit_payload_gated_engine,
                ),
            )
            .expect("payload-gated engine and admission share a fixed kind"),
        );
    let env = lash_core::facade_support::RuntimeEnvironment::builder(core)
        .with_plugin_host(dynamic_plugin_host(Arc::new(DynamicToolSurface::default())))
        .with_process_work(lash_core::testing::process_work_wiring_for_registry(
            registry.clone(),
        ))
        .with_queued_work(Arc::new(lash_core::NoSessionWork::new()))
        .build();
    let runtime = LashRuntime::from_environment(
        &env,
        standard_test_policy(),
        root_state(session_id),
        None,
        lash_core::testing::runtime_lease_owner(),
    )
    .await
    .expect("runtime with a payload-gated process engine");
    (registry, runtime)
}

fn payload_gated_scope(handler: &lash_restate_test::OpenHandler) -> lash_core::ProcessOpScope<'_> {
    lash_core::ProcessOpScope::new(handler.scoped())
}

fn payload_gated_request(
    session_id: &SessionId,
    label: &str,
    kind: &str,
    payload: serde_json::Value,
) -> lash_core::ProcessStartRequest {
    lash_core::ProcessStartRequest::new(
        lash_core::ProcessInput::Engine {
            kind: kind.to_string(),
            payload,
        },
        lash_core::ProcessOriginator::session(lash_core::SessionScope::new(session_id)),
        lash_core::Lifetime::Detached,
    )
    .with_env_ref(
        (lash_core::ProcessExecutionEnvSpec::new(
            lash_core::PluginOptions::default(),
            standard_test_policy(),
        ))
        .stable_ref()
        .expect("captured environment digest"),
    )
    .with_observers([session_id])
    .with_host_start_key(label)
}

async fn started_row_identity(
    registry: &Arc<dyn lash_core::ProcessRegistry>,
    process_id: &ProcessId,
) -> lash_core::ProcessIdentity {
    lash_core::ProcessQuery::get_process(registry.as_ref(), process_id)
        .await
        .expect("read started row")
        .expect("started row exists")
        .identity
}

async fn no_rows_registered(registry: &Arc<dyn lash_core::ProcessRegistry>, labels: &[&str]) {
    let rows = lash_core::ProcessQuery::list_processes(
        registry.as_ref(),
        &lash_core::ProcessListFilter {
            status: lash_core::ProcessStatusFilter::Any,
            ..lash_core::ProcessListFilter::default()
        },
    )
    .await
    .expect("read registered rows");
    for label in labels {
        let key = lash_core::StartKey::for_host(label);
        assert!(
            rows.iter().all(|row| row.start_key.as_ref() != Some(&key)),
            "a refused start must journal and register nothing: {label}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn recorded_intent_engine_start_crosses_the_same_validation_and_identity_gate() {
    let double = kernel_double(SEED + 16, lash_restate_test::ServerConfig::default()).await;
    let backend = double.lash_backend();
    let session_id = "recorded-intent-engine-session";
    let (registry, runtime) = Box::pin(payload_gated_engine_runtime(
        &backend,
        &SessionId::from(session_id),
    ))
    .await;
    let service = runtime
        .runtime_session_services()
        .expect("runtime session services")
        .model_tool_process_service();
    let request = |label: &str, payload: serde_json::Value| {
        payload_gated_request(
            &SessionId::from(session_id),
            label,
            PAYLOAD_GATED_ENGINE_KIND,
            payload,
        )
    };
    let invalid_payload = json!({"program": "smuggled"});
    let handler = double
        .open_handler(AdmittedScope::turn(
            SessionId::from(session_id).clone(),
            TurnId::from(uuid::Uuid::new_v4().to_string()),
        ))
        .await
        .expect("open the scope's handler");
    // A leaf tool's recorded StartProcess declaration must be refused exactly
    // as the direct request-shaped start is, before anything is journaled.
    let direct_refusal = service
        .start_from_request(
            &SessionId::from(session_id),
            request("direct-invalid", invalid_payload.clone()),
            payload_gated_scope(&handler),
        )
        .await
        .expect_err("a direct start must not admit an unvalidated engine payload");
    handler.close().await.expect("close the scope's handler");
    let handler = double
        .open_handler(AdmittedScope::turn(
            SessionId::from(session_id).clone(),
            TurnId::from(uuid::Uuid::new_v4().to_string()),
        ))
        .await
        .expect("open the scope's handler");
    let recorded_refusal = service
        .start_from_recorded_intent(
            &lash_core::RuntimeOwner::Session(SessionId::from(session_id)),
            request("recorded-invalid", invalid_payload.clone()),
            payload_gated_scope(&handler),
        )
        .await
        .expect_err("a recorded-intent start must not admit an unvalidated engine payload");
    handler.close().await.expect("close the scope's handler");
    for refusal in [&direct_refusal, &recorded_refusal] {
        assert!(
            matches!(refusal, lash_core::PluginError::Session(message)
                if message == &format!("unknown {PAYLOAD_GATED_ENGINE_KIND} program")),
            "both start paths owe the engine's own typed refusal: {refusal}"
        );
    }
    assert_eq!(
        std::mem::discriminant(&direct_refusal),
        std::mem::discriminant(&recorded_refusal),
        "the refusal shape must be identical across both start paths"
    );
    no_rows_registered(&registry, &["direct-invalid", "recorded-invalid"]).await;

    // A valid recorded-intent start carries the engine identity stamp a direct
    // start would.
    let valid_payload = json!({"program": "known"});
    let handler = double
        .open_handler(AdmittedScope::turn(
            SessionId::from(session_id).clone(),
            TurnId::from(uuid::Uuid::new_v4().to_string()),
        ))
        .await
        .expect("open the scope's handler");
    let direct = service
        .start_from_request(
            &SessionId::from(session_id),
            request("direct-valid", valid_payload.clone()),
            payload_gated_scope(&handler),
        )
        .await
        .expect("valid direct engine start");
    handler.close().await.expect("close the scope's handler");
    let handler = double
        .open_handler(AdmittedScope::turn(
            SessionId::from(session_id).clone(),
            TurnId::from(uuid::Uuid::new_v4().to_string()),
        ))
        .await
        .expect("open the scope's handler");
    let recorded = service
        .start_from_recorded_intent(
            &lash_core::RuntimeOwner::Session(SessionId::from(session_id)),
            request("recorded-valid", valid_payload.clone()),
            payload_gated_scope(&handler),
        )
        .await
        .expect("valid recorded-intent engine start");
    handler.close().await.expect("close the scope's handler");
    let expected = admit_payload_gated_engine(PAYLOAD_GATED_ENGINE_KIND, &valid_payload, None)
        .expect("known payload");
    assert_eq!(
        started_row_identity(&registry, &direct.process_id).await,
        expected
    );
    assert_eq!(
        started_row_identity(&registry, &recorded.process_id).await,
        expected,
        "the recorded-intent start must carry the same engine identity stamp"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn recorded_intent_start_refuses_an_unregistered_engine_kind_like_a_direct_start() {
    let double = kernel_double(SEED + 17, lash_restate_test::ServerConfig::default()).await;
    let backend = double.lash_backend();
    let session_id = "recorded-intent-unregistered-kind-session";
    let (registry, runtime) = Box::pin(payload_gated_engine_runtime(
        &backend,
        &SessionId::from(session_id),
    ))
    .await;
    let service = runtime
        .runtime_session_services()
        .expect("runtime session services")
        .model_tool_process_service();
    let request = |label: &str| {
        payload_gated_request(
            &SessionId::from(session_id),
            label,
            "fig1488-never-registered",
            json!({"program": "known"}),
        )
    };
    let handler = double
        .open_handler(AdmittedScope::turn(
            SessionId::from(session_id),
            TurnId::from(uuid::Uuid::new_v4().to_string()),
        ))
        .await
        .expect("open the scope's handler");
    let direct_error = service
        .start_from_request(
            &SessionId::from(session_id),
            request("direct-unregistered"),
            payload_gated_scope(&handler),
        )
        .await
        .expect_err("a direct start must not admit an unregistered engine kind");
    handler.close().await.expect("close the scope's handler");
    let handler = double
        .open_handler(AdmittedScope::turn(
            SessionId::from(session_id),
            TurnId::from(uuid::Uuid::new_v4().to_string()),
        ))
        .await
        .expect("open the scope's handler");
    let recorded_error = service
        .start_from_recorded_intent(
            &lash_core::RuntimeOwner::Session(SessionId::from(session_id)),
            request("recorded-unregistered"),
            payload_gated_scope(&handler),
        )
        .await
        .expect_err("a recorded-intent start must not admit an unregistered engine kind");
    handler.close().await.expect("close the scope's handler");
    for (route, error) in [("direct", direct_error), ("recorded", recorded_error)] {
        assert!(
            matches!(&error, lash_core::PluginError::Session(message)
                if message == "process engine `fig1488-never-registered` is not configured"),
            "{route} start owes the engine registry's typed miss: {error}"
        );
    }
    no_rows_registered(&registry, &["direct-unregistered", "recorded-unregistered"]).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn engine_start_without_an_env_spec_keeps_its_per_route_semantics() {
    let double = kernel_double(SEED + 18, lash_restate_test::ServerConfig::default()).await;
    let backend = double.lash_backend();
    let session_id = "recorded-intent-no-env-session";
    let (registry, runtime) = Box::pin(payload_gated_engine_runtime(
        &backend,
        &SessionId::from(session_id),
    ))
    .await;
    let service = runtime
        .runtime_session_services()
        .expect("runtime session services")
        .model_tool_process_service();
    let valid_payload = json!({"program": "known"});
    let no_env = |label: &str| {
        let mut request = payload_gated_request(
            &SessionId::from(session_id),
            label,
            PAYLOAD_GATED_ENGINE_KIND,
            valid_payload.clone(),
        );
        request.env_ref = None;
        request
    };
    let handler = double
        .open_handler(AdmittedScope::turn(
            SessionId::from(session_id).clone(),
            TurnId::from(uuid::Uuid::new_v4().to_string()),
        ))
        .await
        .expect("open the scope's handler");
    // The routes deliberately differ, because a recorded start may only be
    // validated against the env its own record carries. The direct route
    // captures the live session env before the gate, so dropping the request's
    // env spec changes nothing; the recorded route has nothing to validate
    // against and refuses with the pre-existing typed error. That refusal is not
    // new: before the gate moved ahead of the journal, the same message came out
    // of `validate_process_registration` downstream — only one wrapping layer
    // deeper, because it surfaced from registration validation rather than from
    // the engine gate.
    let direct_no_env = service
        .start_from_request(
            &SessionId::from(session_id),
            no_env("direct-no-env"),
            payload_gated_scope(&handler),
        )
        .await
        .expect("a direct start captures the live session env for itself");
    handler.close().await.expect("close the scope's handler");
    assert_eq!(
        started_row_identity(&registry, &direct_no_env.process_id).await,
        admit_payload_gated_engine(PAYLOAD_GATED_ENGINE_KIND, &valid_payload, None)
            .expect("known payload")
    );
    let handler = double
        .open_handler(AdmittedScope::turn(
            SessionId::from(session_id).clone(),
            TurnId::from(uuid::Uuid::new_v4().to_string()),
        ))
        .await
        .expect("open the scope's handler");
    let recorded_no_env = service
        .start_from_recorded_intent(
            &lash_core::RuntimeOwner::Session(SessionId::from(session_id)),
            no_env("recorded-no-env"),
            payload_gated_scope(&handler),
        )
        .await
        .expect_err("a recorded start carries its own env or none at all");
    handler.close().await.expect("close the scope's handler");
    assert!(
        matches!(&recorded_no_env, lash_core::PluginError::Session(message)
        if *message == format!(
            "process `start {}` requires a captured execution env",
            lash_core::StartKey::for_host("recorded-no-env")
        )),
        "the no-env recorded refusal keeps the pre-existing typed shape: {recorded_no_env}"
    );
    no_rows_registered(&registry, &["recorded-no-env"]).await;
}
