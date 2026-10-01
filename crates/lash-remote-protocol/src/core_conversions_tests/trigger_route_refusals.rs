use super::*;

#[test]
fn trigger_route_refusal_codes_survive_report_boundaries() {
    for (code, expected_core, expected_remote) in [
        (
            "trigger_route_unavailable",
            lash_core::RuntimeErrorCode::TriggerRouteUnavailable,
            RemoteTriggerDeliveryFailureCode::TriggerRouteUnavailable,
        ),
        (
            "trigger_route_revoked",
            lash_core::RuntimeErrorCode::TriggerRouteRevoked,
            RemoteTriggerDeliveryFailureCode::TriggerRouteRevoked,
        ),
    ] {
        let wire = serde_json::json!({"failed": {"code": code, "reason": "opaque detail"}});
        let core: lash_core::facade_support::TriggerDeliveryEmitOutcome =
            serde_json::from_value(wire.clone()).expect("decode core refusal");
        assert_eq!(
            serde_json::to_value(&core).expect("encode core refusal"),
            wire
        );
        assert!(matches!(&core,
            lash_core::facade_support::TriggerDeliveryEmitOutcome::Failed { code, .. }
                if *code == expected_core
        ));
        let remote = RemoteTriggerDeliveryEmitOutcome::from(core.clone());
        assert!(matches!(&remote,
            RemoteTriggerDeliveryEmitOutcome::Failed { code, .. } if *code == expected_remote
        ));
        assert_eq!(
            serde_json::to_value(&remote).expect("encode remote refusal"),
            wire
        );
        let decoded: RemoteTriggerDeliveryEmitOutcome =
            serde_json::from_value(wire).expect("decode remote refusal");
        let returned = lash_core::facade_support::TriggerDeliveryEmitOutcome::from(decoded);
        assert_eq!(returned, core);
    }
}
