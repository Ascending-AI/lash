//! Test witnesses of a wait's registration: a receiver the index handler
//! fires, and a hold that parks the registering workflow before its call.

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

type RegistrationHold = (
    tokio::sync::oneshot::Sender<()>,
    tokio::sync::oneshot::Receiver<()>,
);
static REGISTRATION_HOLDS: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<String, RegistrationHold>>,
> = std::sync::LazyLock::new(Default::default);

pub(crate) fn hold_wait_registration(
    key: &AwaitEventKey,
) -> (
    tokio::sync::oneshot::Receiver<()>,
    tokio::sync::oneshot::Sender<()>,
) {
    let (entered, waiting) = tokio::sync::oneshot::channel();
    let (release, held) = tokio::sync::oneshot::channel();
    REGISTRATION_HOLDS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(
            RestateDurableWaitAddress::for_key(key).workflow_key,
            (entered, held),
        );
    (waiting, release)
}

pub(super) async fn await_registration_release(key: &AwaitEventKey) {
    let hold = REGISTRATION_HOLDS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&RestateDurableWaitAddress::for_key(key).workflow_key);
    if let Some((entered, held)) = hold {
        let _ = entered.send(());
        let _ = held.await;
    }
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
