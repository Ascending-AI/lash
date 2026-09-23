//! Laws every [`Deployment`](crate::Deployment) implementation answers
//! (ADR 0102): SQLite file and memory, and the Postgres and Restate
//! deployments that follow.

use super::*;

/// A deployment's binding identity is the one its effect host binds turn
/// control to: the identity is the single source, so no host mints one of its
/// own.
pub async fn a_deployment_binds_its_effect_host_to_its_identity(
    deployment: Arc<dyn crate::Deployment>,
) {
    assert_eq!(
        deployment.effect_host().turn_control_binding_id(),
        deployment.binding_identity(),
        "a deployment's effect host must bind turn control to the deployment's identity"
    );
}
