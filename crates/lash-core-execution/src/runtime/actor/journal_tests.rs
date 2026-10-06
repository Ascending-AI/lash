//! The scope admission and owner-step laws the deleted scoped controller
//! carried, over the context that carries them now.

use crate::{ActorContext, AdmittedScope, RuntimeEffectControllerError};

fn process_id(name: &str) -> crate::ProcessId {
    crate::process_id_for_test(name)
}

/// A non-process context has no pin a process target could match, so it can
/// never rescope into a process context: process admission only exists at
/// construction.
#[test]
fn a_non_process_context_cannot_rescope_into_a_process() {
    let scoped = ActorContext::unavailable()
        .scoped(AdmittedScope::turn("session-1", "turn-1"))
        .expect("turn scope");

    let error = scoped
        .rescope(AdmittedScope::process(process_id("worker")))
        .expect_err("a turn context cannot become a process context");
    assert_eq!(
        error.code,
        crate::RuntimeErrorCode::ExecutionScopeAdmissionRefused
    );
}

/// R3: P cannot register through a clone while the owner awaits D.
#[tokio::test]
async fn preparation_registration_refuses_at_the_owner_await() {
    let scoped = ActorContext::unavailable()
        .scoped(AdmittedScope::turn("s", "t"))
        .expect("scope");
    let actor = scoped.clone();
    let mut wait = Box::pin(scoped.await_owner_step(
        "D".to_owned(),
        std::future::pending::<Result<(), RuntimeEffectControllerError>>(),
    ));
    assert!(futures_util::poll!(&mut wait).is_pending());
    let refused = actor
        .admit_journal_write_at(Some("start:prepare"))
        .expect_err("P registration must refuse at its live site");
    assert_eq!(
        refused.code,
        crate::RuntimeErrorCode::JournalWriteDuringOwnerStep
    );
    assert!(refused.message.contains("D"));
    drop(wait);
    actor
        .admit_journal_write_at(Some("start:prepare"))
        .expect("dropping the await releases admission");
}
