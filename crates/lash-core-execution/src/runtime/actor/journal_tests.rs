//! The scope admission and owner-step laws the deleted scoped controller
//! carried, over the context that carries them now.

use crate::{ActorContext, AdmittedScope, RuntimeEffectControllerError};

fn process_id(name: &str) -> lash_vocabulary::identity::ProcessId {
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

/// R3: P cannot register through a clone while the owner awaits D, and
/// ownership at the claim: the owner step is released however its await
/// ends, so an await dropped at any point of its step, or completed, leaves
/// registration admitted.
#[tokio::test]
async fn preparation_registration_refuses_at_the_owner_await() {
    let scoped = ActorContext::unavailable()
        .scoped(AdmittedScope::turn("s", "t"))
        .expect("scope");
    lash_core_ids::cancel_sweep::sweep_drops(
        async || {
            let (owner, actor) = (scoped.clone(), scoped.clone());
            let step = async move {
                for _ in 0..2 {
                    let refused = actor
                        .admit_journal_write_at(Some("start:prepare"))
                        .expect_err("P registration must refuse at its live site");
                    assert_eq!(
                        refused.code,
                        crate::RuntimeErrorCode::JournalWriteDuringOwnerStep
                    );
                    assert!(refused.message.contains("D"));
                    tokio::task::yield_now().await;
                }
                Ok::<(), RuntimeEffectControllerError>(())
            };
            ((), async move {
                owner.await_owner_step("D".to_owned(), step).await
            })
        },
        async |(), cut| {
            scoped
                .admit_journal_write_at(Some("start:prepare"))
                .unwrap_or_else(|error| {
                    panic!("registration is admitted once the owner await is {cut}: {error:?}")
                });
        },
    )
    .await;
}
