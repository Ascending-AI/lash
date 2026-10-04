//! Test witness of a wait's registration, fired by the index handler.

use super::*;

type WaitRegistrationWitness = tokio::sync::oneshot::Sender<RestateDurableWaitRegistration>;

static WAIT_REGISTRATION_WITNESSES: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<String, WaitRegistrationWitness>>,
> = std::sync::LazyLock::new(Default::default);

/// The receiver fires from the index handler, so an unfinished ingress task is
/// never mistaken for durable registration. The workflow key keeps concurrent
/// live tests independent.
pub(crate) fn arm_wait_registration_witness(
    key: &AwaitEventKey,
) -> tokio::sync::oneshot::Receiver<RestateDurableWaitRegistration> {
    let address = RestateDurableWaitAddress::for_key(key);
    let (send, receive) = tokio::sync::oneshot::channel();
    WAIT_REGISTRATION_WITNESSES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(address.workflow_key, send);
    receive
}

pub(super) fn observe_wait_registration(
    key: &AwaitEventKey,
    registration: &RestateDurableWaitRegistration,
) {
    let address = RestateDurableWaitAddress::for_key(key);
    if let Some(witness) = WAIT_REGISTRATION_WITNESSES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&address.workflow_key)
    {
        let _ = witness.send(registration.clone());
    }
}
