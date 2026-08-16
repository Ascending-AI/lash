use axum::extract::FromRequest;

#[test]
fn wrong_field_mail_payload_is_rejected_as_unprocessable_entity() {
    run_async_test_on_stack_budget("workbench-mail-payload-rejection-test", || async {
        let request = axum::http::Request::builder()
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(r#"{"subject":"ignored","body":"ignored"}"#))
            .expect("build malformed mail request");
        let rejection = Json::<InjectMessageRequest>::from_request(request, &())
            .await
            .expect_err("wrong mail field names must be rejected");

        assert_eq!(
            rejection.into_response().status(),
            StatusCode::UNPROCESSABLE_ENTITY,
            "malformed mail JSON should return HTTP 422"
        );
    });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn inject_message_scopes_emission_to_requested_session() {
    let data_dir = tempfile::tempdir().expect("tempdir");
    let trigger_store = Arc::new(
        lash_sqlite_store::SqliteTriggerStore::open(&data_dir.path().join("triggers.db"))
            .await
            .expect("open trigger store"),
    );
    let mut state = recoverable_chat_test_state_with_trigger_store(
        data_dir.path(),
        Arc::clone(&trigger_store) as Arc<dyn lash::triggers::TriggerStore>,
    )
    .await;

    let (restate_ingress_url, mut restate_requests) = spawn_restate_ingress_capture().await;
    state.restate_ingress_url = restate_ingress_url;

    let scoped_session_id = "scoped-session-test";
    state.sessions.ensure(scoped_session_id, state.rlm_dialect);

    let account_summary = state
        .mail_world
        .add_account("Test Inbox")
        .expect("add mock account");
    let slug = account_summary.slug;

    let outcome = lash::triggers::TriggerStore::execute_command(
        trigger_store.as_ref(),
        "mail-register-operation",
        lash::triggers::TriggerCommand::Register {
            owner_scope: lash::triggers::TriggerOwnerScope::session(scoped_session_id),
            actor: lash::process::ProcessOriginator::session(lash::process::SessionScope::new(
                scoped_session_id,
            )),
            draft: lash::triggers::TriggerSubscriptionDraft::for_process(
                "mail-listener".to_string(),
                lash::process::ProcessExecutionEnvRef::new("process-env:mail-listener"),
                MAIL_RECEIVED_SOURCE_TYPE,
                lash::triggers::empty_trigger_source_key(MAIL_RECEIVED_SOURCE_TYPE)
                    .expect("source key"),
                lash::process::ProcessInput::Engine {
                    kind: "mail-listener-engine".to_string(),
                    payload: serde_json::json!({}),
                },
                lash::process::ProcessIdentity::new("mail-listener-engine"),
            )
            .with_payload_schema(mail_received_payload_schema()),
        },
    )
    .await
    .expect("execute command reaches store")
    .expect("register mail trigger succeeds");
    let subscription_id = match outcome {
        lash::triggers::TriggerCommandOutcome::Mutation { receipt } => receipt.subscription_id,
        _ => panic!("expected mutation outcome"),
    };

    let Json(accepted) = inject_message(
        AxumPath(slug.clone()),
        State(state.clone()),
        Query(SessionQuery {
            session_id: Some(scoped_session_id.to_string()),
        }),
        Json(InjectMessageRequest {
            title: "Important Update".to_string(),
            text: "Hello from test".to_string(),
            model: None,
            model_variant: None,
        }),
    )
    .await
    .expect("inject message accepted");
    assert!(accepted.accepted);

    let request = tokio::time::timeout(Duration::from_secs(2), restate_requests.recv())
        .await
        .expect("Restate request received")
        .expect("Restate request payload");

    let req_session_id = request
        .pointer("/body/session_id")
        .and_then(Value::as_str)
        .expect("session_id in body");
    let delivery: mail::MailDelivery = serde_json::from_value(
        request
            .pointer("/body/delivery")
            .expect("delivery in body")
            .clone(),
    )
    .expect("deserialize delivery");

    let operation_id = "workbench-test-mail-delivery";
    let scoped_effect_controller = lash::runtime::ScopedEffectController::shared(
        Arc::new(lash::runtime::InlineRuntimeEffectController::default()),
        lash::runtime::ExecutionScope::runtime_operation(format!("trigger:{operation_id}")),
    )
    .expect("scoped effect controller");

    let report = enqueue_mail_received_trigger_command(
        &state,
        req_session_id,
        &delivery,
        operation_id,
        scoped_effect_controller,
    )
    .await
    .expect("emit mail received trigger command");

    let deliveries = trigger_store
        .list_deliveries_by_subscription_id(&subscription_id)
        .await
        .expect("list deliveries for subscription");

    assert_eq!(
        req_session_id, scoped_session_id,
        "emission should be scoped to requested session, but was sent under {req_session_id}"
    );
    assert_eq!(
        report.started_process_ids().len(),
        1,
        "trigger occurrence should match the scoped session's subscription"
    );
    assert_eq!(
        deliveries.len(),
        1,
        "inject_message on a scoped session must deliver to that session's subscription (got {})",
        deliveries.len()
    );
}
