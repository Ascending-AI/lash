use super::*;
use crate::RuntimeEffectInvocation;
use std::sync::atomic::{AtomicBool, Ordering};

/// FIG-3719: a served-only executor never runs its effect live. An
/// engine that reaches it without asking gets the refusal, and the
/// command's guard trips so the run stops on it; the refusal survives the
/// handoff to a proxied engine.
#[tokio::test]
async fn a_served_only_executor_refuses_and_trips_its_guard() {
    let refusal = RuntimeEffectControllerError::new(
        crate::RuntimeErrorCode::LashlangCellBindingDrift,
        "binding drifted",
    );
    let ran = Arc::new(AtomicBool::new(false));
    let guard = Arc::new(CommandJournalGuard::open());
    let executor = RuntimeEffectLocalExecutor::testing({
        let ran = Arc::clone(&ran);
        move |_| async move {
            ran.store(true, Ordering::SeqCst);
            Ok(RuntimeEffectOutcome::Sleep)
        }
    })
    .serving_only_from_journal(refusal.clone(), Arc::clone(&guard));
    let (engine_side, _) = executor.into_remote_execution();
    assert!(engine_side.served_only().is_some(), "the handoff keeps it");
    let error = engine_side
        .execute(RuntimeEffectEnvelope::new(
            RuntimeEffectInvocation::new(
                crate::EffectAddress::new(
                    crate::ExecutionScope::runtime_operation("served-only"),
                    "served-only:attempt",
                )
                .expect("valid served-only address"),
                crate::RuntimeAttribution::none(),
                "attempt",
            ),
            RuntimeEffectCommand::Sleep {
                spec: crate::SleepSpec::For { duration_ms: 1 },
            },
        ))
        .await
        .expect_err("a served-only effect never runs live");
    assert_eq!(error.code, refusal.code);
    assert!(!ran.load(Ordering::SeqCst), "the effect did not run");
    assert_eq!(
        guard.tripped().map(|tripped| tripped.code),
        Some(crate::RuntimeErrorCode::LashlangCellBindingDrift)
    );
}
