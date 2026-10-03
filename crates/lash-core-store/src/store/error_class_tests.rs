use super::StoreError;
use crate::runtime_error::{RuntimeEffectControllerError, RuntimeErrorClass};

/// FIG-4649: a store error is retried exactly when the identical operation
/// may succeed again, on every boundary that carries it, and every boundary
/// carries it under the same code and cause.
#[test]
fn every_store_error_is_retryable_exactly_when_transient_on_every_boundary() {
    let mut disagreements = Vec::new();
    for index in 0..StoreError::samples_for_testing().len() {
        let sample = || StoreError::samples_for_testing().swap_remove(index);
        let name = sample().variant_name();
        // The carried runtime error classifies itself.
        if matches!(
            sample(),
            StoreError::TurnOutcomeMaterializationRefused { .. }
        ) {
            continue;
        }
        let transient = sample().is_transient();
        let code = sample().runtime_code();
        let cause = sample().runtime_cause();
        let boundaries = [
            RuntimeEffectControllerError::from(sample()).into_runtime_error(),
            crate::runtime_error::runtime_error_from_store_commit(sample()),
            crate::runtime_error::runtime_error_from_turn_input_admission(sample()),
            sample().runtime_error(),
        ];
        for error in &boundaries {
            if error.code != code || error.cause != cause {
                disagreements.push(format!(
                    "{name}: carried as {} with {:?}, classified {code} with {cause:?}",
                    error.code, error.cause
                ));
            }
            if error.is_retryable() != transient {
                disagreements.push(format!(
                    "{name}: transient={transient}, carried as {} ({:?})",
                    error.code,
                    error.code.classification()
                ));
            }
        }
        if code.classification() == RuntimeErrorClass::Parked {
            disagreements.push(format!("{name}: parked under {code}"));
        }
    }
    assert!(
        disagreements.is_empty(),
        "{} disagreements:\n{}",
        disagreements.len(),
        disagreements.join("\n")
    );
}

/// A refused turn outcome hands back the runtime refusal it carries.
#[test]
fn a_refused_turn_outcome_is_carried_as_the_refusal_itself() {
    let refusal = crate::RuntimeError::new(
        crate::RuntimeErrorCode::HistoricalAgentFrameSwitchUnsupported,
        "frame `frame-a` is a persisted historical frame",
    );
    let error = StoreError::TurnOutcomeMaterializationRefused {
        error: Box::new(refusal.clone()),
    };
    assert_eq!(error.runtime_code(), refusal.code);
    let carried = error.runtime_error();
    assert_eq!(carried.code, refusal.code);
    assert_eq!(carried.message, refusal.message);
}
