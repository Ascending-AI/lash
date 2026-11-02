use super::StoreError;
use crate::runtime_error::RuntimeEffectControllerError;

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

/// RLM-HISTORY-PIN: a released run window refuses with its exact base revision
/// through the runtime boundaries used by turn recovery, including serialization.
#[test]
fn a_released_turn_base_keeps_its_typed_refusal() {
    let store = || StoreError::TurnBaseNotRetained { revision: 17 };
    for error in [
        store().runtime_error(),
        RuntimeEffectControllerError::from(store()).into_runtime_error(),
        crate::runtime_error::runtime_error_from_store_commit(store()),
        crate::runtime_error::runtime_error_from_turn_input_admission(store()),
    ] {
        assert_eq!(error.code, crate::RuntimeErrorCode::StoreRefused);
        let encoded = serde_json::to_value(&error).expect("runtime error serializes");
        let decoded: crate::RuntimeError =
            serde_json::from_value(encoded).expect("runtime error round-trips");
        let Some(crate::RuntimeErrorCause::StoreRefusal { refusal }) = decoded.cause else {
            panic!("the released run base lost its typed refusal: {decoded:?}");
        };
        assert!(matches!(
            refusal.into_store_error(),
            StoreError::TurnBaseNotRetained { revision: 17 }
        ));
    }
}
