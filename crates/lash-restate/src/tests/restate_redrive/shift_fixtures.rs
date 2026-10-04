use super::*;

/// Registers `registration` and answers the id its registrar minted.
pub(super) async fn registered(
    registry: &dyn ProcessRegistry,
    registration: &ProcessRegistration,
) -> ProcessId {
    registry
        .register_process(registration.clone())
        .await
        .expect("register the process")
        .id
}
