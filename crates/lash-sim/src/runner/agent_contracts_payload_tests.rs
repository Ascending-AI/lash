//! The process effect-outcome contract's normalization of replay identity.

use super::*;

#[test]
fn process_effect_outcome_contract_normalizes_only_opaque_replay_identity() {
    let first = json!({"replay_key": "first", "node_id": "node:a", "outcome_class": "success"});
    let second = json!({"replay_key": "second", "node_id": "node:a", "outcome_class": "success"});
    let changed_outcome =
        json!({"replay_key": "second", "node_id": "node:a", "outcome_class": "failure"});

    assert_eq!(
        normalize_contract_process_event_payload("process.effect_outcome", first.clone()),
        normalize_contract_process_event_payload("process.effect_outcome", second)
    );
    assert_ne!(
        normalize_contract_process_event_payload("process.effect_outcome", first.clone()),
        normalize_contract_process_event_payload("process.effect_outcome", changed_outcome)
    );
    assert_eq!(
        normalize_contract_process_event_payload("process.completed", first.clone()),
        first
    );
}

/// A process lifted out of a cell carries no author-chosen name: the runtime's
/// derived label is a `__process_` digest, but a host-declared `label` replaces
/// it as display metadata without touching the process's identity. The lifted
/// evidence is structural — `ProcessOrigin::Lifted` on the module IR's process
/// declaration — so a renamed display label must not change the oracle's
/// verdict.
#[tokio::test]
async fn lifted_process_display_name_does_not_change_the_oracle_verdict() {
    let expected = json!({ "ok": true });
    let result = facade_agent_process_execution(
        "lash_runtime agent renamed lifted process",
        &SessionId::from("sim-agent-renamed-lifted-process-contract"),
        "Start a process under a declared display label and return its value.",
        vec![
            r#"<typescript>
const lookup = async () => {
  return { ok: true };
};
const handle = await processes.start({ definition: lookup, label: "renamed display label" });
const result = await handle;
finish(result);
</typescript>"#,
        ],
        &expected,
        None,
    )
    .await
    .expect("renamed lifted process contract world");

    let entries = result
        .pointer("/process_facts/completed_entries")
        .and_then(Value::as_array)
        .expect("process_facts records completed entries");
    assert_eq!(
        entries.iter().filter_map(Value::as_str).collect::<Vec<_>>(),
        vec!["renamed display label"],
        "the display label, not the lift digest, is what observation records",
    );

    crate::oracles::require_agent_lifted_process_entries(&result, 1, "test")
        .expect("a renamed display label must not change the lifted-process verdict");
}
