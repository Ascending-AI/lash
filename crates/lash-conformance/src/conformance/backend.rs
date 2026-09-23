//! Laws every [`Backend`](crate::Backend) implementation answers
//! (ADR 0102): SQLite file and memory, and the Postgres and Restate
//! backends that follow.

use super::*;

/// A backend's binding identity is the one its effect host binds turn
/// control to: the identity is the single source, so no host mints one of its
/// own.
pub async fn a_backend_binds_its_effect_host_to_its_identity(backend: Arc<dyn crate::Backend>) {
    assert_eq!(
        backend.effect_host().turn_control_binding_id(),
        backend.binding_identity(),
        "a backend's effect host must bind turn control to the backend's identity"
    );
}
