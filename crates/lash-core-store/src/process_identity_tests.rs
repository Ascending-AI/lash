use super::*;

#[test]
fn a_process_execution_policy_rejects_the_retired_session_id() {
    let policy = crate::SessionPolicy::new(
        crate::TurnBudget::Unbounded,
        crate::MaxToolCalls::new(1024),
        crate::NoProgressBudget::bounded(12),
    );
    let encoded = serde_json::to_value(&policy).expect("encode policy");
    assert!(encoded.get("session_id").is_none());
    for retired in [serde_json::Value::Null, serde_json::json!("session")] {
        let mut fields = encoded.clone();
        fields["session_id"] = retired;
        let error = serde_json::from_value::<crate::SessionPolicy>(fields)
            .expect_err("the retired session id is refused");
        assert!(error.to_string().contains("unknown field `session_id`"));
    }
}

#[test]
fn sequential_process_id_mint_preserves_every_ordinal_and_parses() {
    let ordinals = [0, 1, (1 << 62) - 1, 1 << 62, 1 << 63, u64::MAX];
    let ids = ordinals.map(ProcessIdMint::sequential_id_for_testing);
    for (index, id) in ids.iter().enumerate() {
        assert_eq!(ProcessId::parse(id.as_str()).unwrap(), *id);
        assert!(
            !ids[..index].contains(id),
            "different ordinals mint different ids"
        );
    }
}
