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
