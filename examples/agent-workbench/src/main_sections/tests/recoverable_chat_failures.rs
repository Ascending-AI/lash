#[tokio::test]
async fn workbench_provider_failure_emits_only_fixed_public_product_copy() {
    const INTERNAL_PROVIDER_FAILURE: &str = "provider rejected credentials for secret account";
    let data_dir = tempfile::tempdir().expect("provider failure tempdir");
    let provider = lash::testing::TestProvider::builder()
        .kind("recoverable-chat-provider-failure")
        .complete_error(INTERNAL_PROVIDER_FAILURE)
        .build()
        .into_handle();
    let state =
        recoverable_chat_test_state_with_provider(data_dir.path(), 16, provider).await;
    let session_id = state.current_session_id();
    let session = state
        .core
        .session(session_id.clone())
        .open()
        .await
        .expect("open provider failure session");
    let turn_state = Arc::new(Mutex::new(TurnStreamState::default()));
    let output = session
        .turn(lash::TurnInput::text("fail through the provider"))
        .turn_id("provider-failure-turn")
        .require_finish()
        .expect("require finish")
        .stream_to(&ChannelTurnEvents {
            turn_state: Arc::clone(&turn_state),
        })
        .await
        .expect("provider failure is represented as a stopped turn");
    assert!(
        output
            .errors
            .iter()
            .any(|error| error.message.contains(INTERNAL_PROVIDER_FAILURE)),
        "the real provider diagnostic must reach the internal turn result"
    );
    crate::restate::record_turn_output(
        &state,
        &session,
        "provider-failure-turn",
        output,
        turn_state,
        "test.provider.failed",
    )
    .await
    .expect("project provider failure through the production recorder");

    let serialized = serde_json::to_string(&state.event_tx.snapshot(&session_id))
        .expect("serialize provider failure projection");
    assert!(serialized.contains(PUBLIC_TURN_FAILURE_MESSAGE));
    assert!(!serialized.contains(INTERNAL_PROVIDER_FAILURE));

    let response = AppError::internal(INTERNAL_PROVIDER_FAILURE).into_response();
    let bytes = axum::body::to_bytes(response.into_body(), 1024)
        .await
        .expect("read internal error response");
    assert_eq!(
        serde_json::from_slice::<Value>(&bytes).expect("decode internal error response"),
        json!({ "error": "internal server error" })
    );
}

#[test]
fn authorization_seam_can_deny_observation_without_product_specific_auth() {
    struct DenyObservation;

    impl WorkbenchAuthorizer for DenyObservation {
        fn authorize(&self, action: &WorkbenchAuthorizationAction) -> Result<(), AppError> {
            match action {
                WorkbenchAuthorizationAction::Observe { .. } => {
                    Err(AppError::forbidden("observation denied by host policy"))
                }
                _ => Ok(()),
            }
        }
    }

    let authorization = WorkbenchAuthorization::with_authorizer(Arc::new(DenyObservation));
    let denied = authorization
        .authorize(WorkbenchAuthorizationAction::Observe {
            session_id: "auth-session".to_string(),
        })
        .expect_err("host policy must be able to deny observation");
    assert_eq!(denied.status, StatusCode::FORBIDDEN);
    authorization
        .authorize(WorkbenchAuthorizationAction::EnqueueTurn {
            session_id: "auth-session".to_string(),
        })
        .expect("independent enqueue policy remains pluggable");
}
