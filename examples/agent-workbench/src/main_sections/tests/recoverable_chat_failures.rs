use super::*;
use lash::SessionId;

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
            session_id: SessionId::from("auth-session"),
        })
        .expect_err("host policy must be able to deny observation");
    assert_eq!(denied.status, StatusCode::FORBIDDEN);
    authorization
        .authorize(WorkbenchAuthorizationAction::EnqueueTurn {
            session_id: SessionId::from("auth-session"),
        })
        .expect("independent enqueue policy remains pluggable");
}

/// A provider failure reaches the page only as the fixed public copy: the
/// provider's free text never reaches the session's product rows, and an
/// internal route error answers only `internal server error`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workbench_provider_failure_emits_only_fixed_public_product_copy() {
    const INTERNAL_PROVIDER_FAILURE: &str = "provider rejected credentials for secret account";
    let provider = lash::testing::TestProvider::builder()
        .kind("workbench-harness")
        .complete_error(INTERNAL_PROVIDER_FAILURE)
        .build()
        .into_handle();
    let workbench = Workbench::builder(provider).build().await;
    let state = &workbench.state;
    let session_id = state.current_session_id();
    run_turn(state, "fail through the provider").await;

    let serialized = serde_json::to_string(&state.event_tx.snapshot(&session_id))
        .expect("serialize the provider failure projection");
    assert!(
        serialized.contains(PUBLIC_TURN_FAILURE_MESSAGE),
        "the page shows the fixed failure copy: {serialized}"
    );
    assert!(!serialized.contains(INTERNAL_PROVIDER_FAILURE));
    let rendered = serde_json::to_string(&read_state(state, None).await.expect("the snapshot"))
        .expect("serialize the snapshot");
    assert!(
        !rendered.contains(INTERNAL_PROVIDER_FAILURE),
        "the provider's text reaches no rendered row"
    );

    let response = AppError::internal(INTERNAL_PROVIDER_FAILURE).into_response();
    let bytes = axum::body::to_bytes(response.into_body(), 1024)
        .await
        .expect("read the internal error response");
    assert_eq!(
        serde_json::from_slice::<Value>(&bytes).expect("decode the internal error response"),
        json!({ "error": "internal server error" })
    );
    workbench.shutdown().await;
}
